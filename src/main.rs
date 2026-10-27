//! tg — lets Claude Code / Codex sessions message their owner on Telegram and get replies.
//! A reply is routed by Telegram's reply-to message id back to the session that sent the message.

use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: tg send <msg|-> | wait [secs] | inbox | status | hook <prompt|stop> | daemon | chats | setchat <id>";
const CHUNK_UTF16: usize = 4000; // Telegram's limit is 4096 UTF-16 code units
const KEEP: Duration = Duration::from_secs(30 * 24 * 3600); // prune map/read/inbox files older than this

// ---------- Paths, files, config ----------

/// Test-only overrides. Release builds ignore them, so a hostile environment
/// (e.g. an untrusted repo's .envrc) can't redirect the token, state, or session.
fn test_env(name: &str) -> Option<String> {
    if cfg!(feature = "testing") {
        std::env::var(name).ok()
    } else {
        None
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set"))
}
fn cfg_dir() -> PathBuf {
    test_env("TG_CONFIG").map(PathBuf::from).unwrap_or_else(|| home().join(".config/tg"))
}
fn state() -> PathBuf {
    test_env("TG_STATE").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state/tg"))
}
fn inbox_dir(sid: &str) -> PathBuf {
    state().join("inbox").join(sid)
}
fn read_dir(sid: &str) -> PathBuf {
    state().join("read").join(sid)
}
fn secrets_env() -> PathBuf {
    home().join(".secrets/telegram.env")
}
fn cfg(name: &str) -> Result<String, String> {
    fs::read_to_string(cfg_dir().join(name))
        .map(|s| s.trim().to_string())
        .map_err(|_| format!("missing {}", cfg_dir().join(name).display()))
}

/// All state is private to the owner: dirs 0700, files 0600.
fn mkdir(p: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(p)
}
fn open_private(p: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(p)
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect()
}

/// Write via temp file + fsync + rename so readers never see partial data and it survives a crash.
/// The temp file is created with O_EXCL, so a planted symlink is never followed.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap();
    mkdir(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), path.file_name().unwrap().to_string_lossy()));
    let _ = fs::remove_file(&tmp);
    {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// Token lookup: <config>/token, then $TELEGRAM_BOT_TOKEN, then ~/.secrets/telegram.env.
fn token() -> Result<String, String> {
    if let Ok(t) = cfg("token") {
        return Ok(t);
    }
    if let Ok(t) = std::env::var("TELEGRAM_BOT_TOKEN") {
        return Ok(t.trim().to_string());
    }
    let env = fs::read_to_string(secrets_env()).unwrap_or_default();
    env.lines()
        .filter_map(|l| l.trim().trim_start_matches("export ").split_once('='))
        .find(|(k, _)| k.trim() == "TELEGRAM_BOT_TOKEN")
        .map(|(_, v)| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "no token: put TELEGRAM_BOT_TOKEN=... in ~/.secrets/telegram.env".into())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn log(msg: &str) {
    let p = state().join("daemon.log");
    if fs::metadata(&p).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = fs::rename(&p, state().join("daemon.log.1"));
    }
    if let Ok(mut f) = open_private(&p) {
        let _ = writeln!(f, "{} {}", now(), msg);
    }
}

/// Run `tg <args>` fully detached (own process group, no stdio) so it outlives the caller.
fn spawn_detached<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(args: I) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
    }
}

// ---------- Telegram API ----------

enum ApiErr {
    /// Request certainly not processed (connect failure, 429, 409): safe to retry.
    Retry(Duration, String),
    /// Request may have been processed (timeout after sending, 5xx): retry only idempotent calls.
    Unsure(String),
    Fatal(String),
}

struct Api {
    agent: ureq::Agent,
    token: String,
    chat: String,
}

impl Api {
    fn new() -> Result<Api, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(70))
            .timeout_write(Duration::from_secs(20))
            .build();
        Ok(Api { agent, token: token()?, chat: cfg("chat_id")? })
    }

    /// Never let the token leak into error text (ureq errors include the URL).
    fn scrub(&self, s: String) -> String {
        s.replace(&self.token, "***")
    }

    fn call(&self, method: &str, body: Value) -> Result<Value, ApiErr> {
        let base = test_env("TG_API").unwrap_or_else(|| "https://api.telegram.org".into());
        let url = format!("{base}/bot{}/{}", self.token, method);
        match self.agent.post(&url).send_json(body) {
            Ok(r) => {
                let v: Value = r.into_json().map_err(|e| ApiErr::Unsure(self.scrub(format!("{method}: {e}"))))?;
                Ok(v["result"].clone())
            }
            Err(ureq::Error::Status(code, r)) => {
                let v: Value = r.into_json().unwrap_or(Value::Null);
                let desc = format!("{method}: {code} {}", v["description"].as_str().unwrap_or(""));
                match code {
                    429 => Err(ApiErr::Retry(Duration::from_secs(v["parameters"]["retry_after"].as_u64().unwrap_or(5)), desc)),
                    409 => Err(ApiErr::Retry(Duration::from_secs(5), desc)), // another poller (e.g. `tg chats`)
                    500.. => Err(ApiErr::Unsure(desc)),
                    _ => Err(ApiErr::Fatal(desc)),
                }
            }
            Err(ureq::Error::Transport(t)) => {
                let m = self.scrub(format!("{method}: {t}"));
                match t.kind() {
                    ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => Err(ApiErr::Retry(Duration::from_secs(2), m)),
                    _ => Err(ApiErr::Unsure(m)),
                }
            }
        }
    }

    /// Retry with backoff. Non-idempotent calls (sendMessage) are never retried once the
    /// request may have reached Telegram, so the owner never gets duplicates.
    fn call_retry(&self, method: &str, body: Value, attempts: u32, idempotent: bool) -> Result<Value, String> {
        let mut last = String::new();
        for i in 0..attempts {
            let wait = match self.call(method, body.clone()) {
                Ok(v) => return Ok(v),
                Err(ApiErr::Fatal(m)) => return Err(m),
                Err(ApiErr::Unsure(m)) if !idempotent => return Err(format!("{m} (may or may not have been delivered)")),
                Err(ApiErr::Unsure(m)) => {
                    last = m;
                    Duration::from_secs(2)
                }
                Err(ApiErr::Retry(d, m)) => {
                    last = m;
                    d
                }
            };
            if i + 1 < attempts {
                sleep(wait.max(Duration::from_secs(1 << i.min(4))));
            }
        }
        Err(last)
    }

    fn send(&self, text: &str) -> Result<i64, String> {
        let r = self.call_retry("sendMessage", json!({"chat_id": self.chat, "text": text}), 5, false)?;
        r["message_id"].as_i64().ok_or_else(|| "sendMessage: no message_id".into())
    }

    fn react(&self, msg_id: i64, emoji: &str) {
        let body = json!({"chat_id": self.chat, "message_id": msg_id, "reaction": [{"type": "emoji", "emoji": emoji}]});
        let _ = self.call_retry("setMessageReaction", body, 2, true);
    }

    fn notice(&self, reply_to: i64, text: &str) {
        let body = json!({"chat_id": self.chat, "text": text, "reply_parameters": {"message_id": reply_to, "allow_sending_without_reply": true}});
        let _ = self.call_retry("sendMessage", body, 3, false);
    }
}

// ---------- Session identity ----------

/// Session ids become path components, so only allow a safe charset.
fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// (comm, ppid, starttime) from /proc/<pid>/stat
fn proc_stat(pid: u32) -> Option<(String, u32, String)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (l, r) = (s.find('(')?, s.rfind(')')?);
    let comm = s[l + 1..r].to_string();
    let f: Vec<&str> = s[r + 2..].split(' ').collect();
    Some((comm, f.get(1)?.parse().ok()?, f.get(19)?.to_string()))
}

fn base(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn agent_kind(pid: u32, comm: &str) -> Option<&'static str> {
    let known = |n: &str| match n {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    };
    // comm is usually enough; exe and argv0 are only read as fallbacks.
    known(comm)
        .or_else(|| known(&fs::read_link(format!("/proc/{pid}/exe")).map(|p| base(&p)).unwrap_or_default()))
        .or_else(|| {
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            let argv0 = cmdline.split(|c| *c == 0).next()?;
            known(&base(Path::new(&*String::from_utf8_lossy(argv0))))
        })
}

/// Nearest claude/codex ancestor as (kind, pid, starttime), computed once per process.
/// None inside Codex's sandbox, whose pid namespace hides the agent process.
fn ancestor() -> Option<&'static (&'static str, u32, String)> {
    static A: OnceLock<Option<(&'static str, u32, String)>> = OnceLock::new();
    A.get_or_init(|| {
        let mut pid = proc_stat(std::process::id())?.1;
        for _ in 0..64 {
            if pid <= 1 {
                break;
            }
            let (comm, ppid, start) = proc_stat(pid)?;
            if let Some(k) = agent_kind(pid, &comm) {
                return Some((k, pid, start));
            }
            pid = ppid;
        }
        None
    })
    .as_ref()
}

/// Session id:
/// - Claude: "claude-<pid>-<starttime>". Claude's own session id changes on /clear; its process doesn't.
///   Start time guards against pid reuse.
/// - Codex: "codex-thread-<CODEX_THREAD_ID>". Works inside Codex's sandbox, where the process tree is hidden.
///   Codex hooks may not get that env var, so they pass the session_id from their stdin as `hook_sid`.
fn session_from(hook_sid: Option<&str>) -> Option<String> {
    let thread = || std::env::var("CODEX_THREAD_ID").ok().filter(|t| !t.is_empty());
    let sid = test_env("TG_SESSION").or_else(|| match ancestor() {
        Some(("claude", pid, start)) => Some(format!("claude-{pid}-{start}")),
        Some((_, pid, start)) => Some(match thread().or(hook_sid.map(String::from)) {
            Some(t) => format!("codex-thread-{t}"),
            None => format!("codex-{pid}-{start}"),
        }),
        None => thread().map(|t| format!("codex-thread-{t}")),
    });
    sid.filter(|s| valid_sid(s))
}

fn session() -> Option<String> {
    session_from(None)
}

/// Inside Codex's sandbox: processes spawned from here die with the command and see the wrong /proc.
fn sandboxed() -> bool {
    ancestor().is_none() && std::env::var("CODEX_THREAD_ID").is_ok()
}

/// (kind, pid, starttime) for process-based ids like "claude-123-456".
fn parse_pid_sid(sid: &str) -> Option<(&str, u32, &str)> {
    let mut it = sid.splitn(3, '-');
    let kind = it.next()?;
    let pid = it.next()?.parse().ok()?;
    Some((kind, pid, it.next()?))
}

/// Codex threads have no liveness signal visible from outside, so they always count as alive:
/// replies to an ended Codex session are queued but never get 👀.
fn alive(sid: &str) -> bool {
    match parse_pid_sid(sid) {
        Some((_, pid, start)) => proc_stat(pid).is_some_and(|s| s.2 == start),
        None => true,
    }
}

/// Short human label shown on Telegram, e.g. "claude·myrepo·48213". Computed by the sender.
fn tag(sid: &str) -> String {
    if let Some((kind, pid, _)) = parse_pid_sid(sid) {
        let cwd = fs::read_link(format!("/proc/{pid}/cwd")).map(|p| base(&p)).unwrap_or_default();
        return format!("{kind}·{cwd}·{pid}");
    }
    match sid.strip_prefix("codex-thread-") {
        Some(t) => {
            let cwd = std::env::current_dir().map(|p| base(&p)).unwrap_or_default();
            format!("codex·{cwd}·{}", &t[t.len().saturating_sub(4)..]) // sid is ASCII (valid_sid)
        }
        None => sid.to_string(),
    }
}

// ---------- Daemon ----------

fn lock_path() -> PathBuf {
    state().join("daemon.lock")
}

fn open_lock() -> io::Result<File> {
    mkdir(&state())?;
    OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(lock_path())
}

/// Start the daemon if nobody holds its lock. Prefers the systemd unit (so the daemon runs under
/// systemd, not inside an agent's cgroup with its env); falls back to a detached `--if-free` copy.
/// Safe to call any number of times: racing `--if-free` copies that lose just exit.
fn ensure_daemon() {
    if sandboxed() {
        return;
    }
    let Ok(f) = open_lock() else { return };
    if f.try_lock().is_err() {
        return; // running
    }
    drop(f);
    let via_systemd = !cfg!(feature = "testing")
        && Command::new("systemctl")
            .args(["--user", "start", "tg.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if !via_systemd {
        spawn_detached(["daemon", "--if-free"]);
    }
}

fn daemon_pid() -> Option<u32> {
    if open_lock().ok()?.try_lock().is_ok() {
        return None;
    }
    fs::read_to_string(lock_path()).ok()?.trim().parse().ok()
}

/// `if_free`: exit if another daemon holds the lock. Otherwise block until we own it, so a
/// second copy is a hot standby that takes over the moment the active one dies.
fn daemon(if_free: bool) -> Result<(), String> {
    // Check config before taking the lock, so an unconfigured daemon never blocks `tg chats`.
    let api = Api::new()?;
    let mut lock = open_lock().map_err(|e| e.to_string())?;
    if if_free {
        if lock.try_lock().is_err() {
            return Ok(());
        }
    } else {
        lock.lock().map_err(|e| e.to_string())?;
    }
    lock.set_len(0).ok();
    let _ = write!(lock, "{}", std::process::id());
    log(&format!("daemon start pid={}", std::process::id()));
    let _ = api.call("deleteWebhook", json!({}));
    let off_path = state().join("offset");
    let mut offset: i64 = fs::read_to_string(&off_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let mut backoff = 1u64;
    let mut last_prune: Option<Instant> = None;
    loop {
        if last_prune.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
            prune();
            last_prune = Some(Instant::now());
        }
        let r = api.call("getUpdates", json!({"offset": offset, "timeout": 50, "allowed_updates": ["message"]}));
        let updates = match r {
            Ok(v) => v,
            Err(e) => {
                let (wait, m) = match e {
                    ApiErr::Fatal(m) => (30, m), // e.g. bad token: don't spin
                    ApiErr::Retry(d, m) => (d.as_secs().max(backoff), m),
                    ApiErr::Unsure(m) => (backoff, m),
                };
                log(&format!("getUpdates error: {m}"));
                sleep(Duration::from_secs(wait));
                backoff = (backoff * 2).min(30);
                continue;
            }
        };
        backoff = 1;
        for u in updates.as_array().into_iter().flatten() {
            let id = u["update_id"].as_i64().unwrap_or(0);
            if let Err(e) = handle(&api, id, &u["message"]) {
                // Disk failure: don't advance offset, Telegram will redeliver.
                log(&format!("handle error (will retry): {e}"));
                sleep(Duration::from_secs(2));
                break;
            }
            offset = id + 1;
            if let Err(e) = write_atomic(&off_path, offset.to_string().as_bytes()) {
                log(&format!("offset write error: {e}"));
            }
        }
    }
}

/// Only a private chat with the owner counts: same chat id AND sender id, so a group
/// (where other members could reply to the bot) can never be configured by accident.
fn from_owner(m: &Value, chat: &str) -> bool {
    m["chat"]["type"] == "private" && m["chat"]["id"].to_string() == chat && m["from"]["id"].to_string() == chat
}

/// map/<telegram msg id> holds "sid\ntag\nsnippet", written by `send`.
fn handle(api: &Api, update_id: i64, m: &Value) -> io::Result<()> {
    if m.is_null() || !from_owner(m, &api.chat) {
        return Ok(());
    }
    let mid = m["message_id"].as_i64().unwrap_or(0);
    let Some(text) = m["text"].as_str() else {
        api.notice(mid, "⚠️ not delivered: only text replies are supported");
        return Ok(());
    };
    let Some(rt) = m["reply_to_message"]["message_id"].as_i64() else {
        api.notice(mid, "⚠️ not delivered: use Reply on a session's message");
        return Ok(());
    };
    let entry = fs::read_to_string(state().join("map").join(rt.to_string())).unwrap_or_default();
    let mut parts = entry.splitn(3, '\n');
    let (sid, tag, snippet) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if !valid_sid(sid) {
        api.notice(mid, "⚠️ not delivered: that message isn't from a tracked session");
        return Ok(());
    }
    if !alive(sid) {
        api.notice(mid, &format!("⚠️ not delivered: session {tag} has ended"));
        return Ok(());
    }
    write_atomic(&inbox_dir(sid).join(format!("{update_id:020}")), format!("{mid}\n{snippet}\n{text}").as_bytes())?;
    log(&format!("queued update={update_id} msg={mid} -> {sid}"));
    api.react(mid, "👌");
    Ok(())
}

/// Delete files older than KEEP under map/, read/<sid>/ and inbox/<sid>/, and empty session dirs.
fn prune() {
    let rm_old = |dir: &Path| {
        for f in entries(dir) {
            if fs::metadata(&f).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > KEEP) {
                let _ = fs::remove_file(f);
            }
        }
    };
    rm_old(&state().join("map"));
    for top in ["read", "inbox"] {
        for dir in entries(&state().join(top)) {
            rm_old(&dir);
            let _ = fs::remove_dir(&dir); // only succeeds when empty
        }
    }
}

// ---------- Client commands ----------

fn need_session() -> String {
    session().unwrap_or_else(|| {
        eprintln!("tg: not inside a claude/codex session");
        exit(1)
    })
}

/// Split into chunks of at most CHUNK_UTF16 UTF-16 code units (how Telegram measures length).
fn chunks(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut n = 0;
    for c in s.chars() {
        if n + c.len_utf16() > CHUNK_UTF16 {
            out.push(String::new());
            n = 0;
        }
        n += c.len_utf16();
        out.last_mut().unwrap().push(c);
    }
    out
}

fn send(text: &str) -> Result<(), String> {
    let sid = need_session();
    ensure_daemon();
    let api = Api::new()?;
    let tag = tag(&sid);
    let snippet: String = text.chars().take(40).collect::<String>().replace('\n', " ");
    let parts = chunks(&format!("[{tag}] {text}"));
    for (i, part) in parts.iter().enumerate() {
        let mid = api.send(part).map_err(|e| if i == 0 { e } else { format!("{e} — only {i} of {} parts sent", parts.len()) })?;
        write_atomic(&state().join("map").join(mid.to_string()), format!("{sid}\n{tag}\n{snippet}").as_bytes())
            .map_err(|e| format!("map write: {e}"))?;
    }
    Ok(())
}

struct Delivery {
    claimed: Vec<String>,
    ids: Vec<i64>,
    text: String,
}

/// Claim queued replies by atomic rename into read/ (so two concurrent readers never
/// deliver the same reply). Call `emit` to hand them over.
fn deliver(sid: &str) -> Option<Delivery> {
    let (dir, done) = (inbox_dir(sid), read_dir(sid));
    let mut names: Vec<String> = entries(&dir).iter().map(|p| base(p)).filter(|n| !n.starts_with('.')).collect();
    if names.is_empty() {
        let _ = fs::remove_dir(&dir); // keep the hook's fast path a single failed read_dir
        return None;
    }
    names.sort();
    mkdir(&done).ok()?;
    let (mut claimed, mut ids, mut texts) = (vec![], vec![], vec![]);
    for n in names {
        if fs::rename(dir.join(&n), done.join(&n)).is_err() {
            continue; // another reader got it
        }
        let raw = fs::read(done.join(&n)).unwrap_or_default();
        let s = String::from_utf8_lossy(&raw);
        let mut p = s.splitn(3, '\n');
        ids.push(p.next().unwrap_or("0").parse().unwrap_or(0));
        let snip = p.next().unwrap_or("");
        texts.push(format!("re \"{snip}\": {}", p.next().unwrap_or("")));
        claimed.push(n);
    }
    (!texts.is_empty()).then(|| Delivery { claimed, ids, text: texts.join("\n\n") })
}

/// Print `out`. On failure requeue the claimed replies. On success ack them with 👀 from a
/// detached process, so the agent (and hooks, with their short timeout) never wait on Telegram.
/// Inside Codex's sandbox a detached process would die with the command, so ack inline there.
fn emit(sid: &str, d: &Delivery, out: &str) {
    let mut so = io::stdout().lock();
    let ok = writeln!(so, "{out}").and_then(|_| so.flush()).is_ok();
    drop(so);
    if !ok {
        for n in &d.claimed {
            let _ = fs::rename(read_dir(sid).join(n), inbox_dir(sid).join(n));
        }
        exit(1);
    }
    if sandboxed() {
        ack(&d.ids);
    } else {
        spawn_detached(std::iter::once("ack".to_string()).chain(d.ids.iter().map(i64::to_string)));
    }
}

fn ack(ids: &[i64]) {
    if let Ok(api) = Api::new() {
        for &id in ids {
            api.react(id, "👀");
        }
    }
}

fn wait(secs: Option<u64>) {
    let sid = need_session();
    ensure_daemon();
    let start = Instant::now();
    for tick in 1u32.. {
        if let Some(d) = deliver(&sid) {
            emit(&sid, &d, &d.text);
            return;
        }
        if tick % 10 == 0 {
            ensure_daemon(); // self-heal if the daemon died
            if !alive(&sid) {
                exit(3);
            }
        }
        if secs.is_some_and(|s| start.elapsed().as_secs() >= s) {
            eprintln!("no reply yet");
            exit(2);
        }
        sleep(Duration::from_millis(500));
    }
}

/// Runs on every prompt / turn end, so it must be cheap and silent when nothing is pending.
fn hook(kind: &str) {
    let input = io::read_to_string(io::stdin()).unwrap_or_default();
    let hook_sid = serde_json::from_str::<Value>(&input).ok().and_then(|v| v["session_id"].as_str().map(String::from));
    let Some(sid) = session_from(hook_sid.as_deref()) else { return };
    let Some(d) = deliver(&sid) else { return };
    let msg = format!("Telegram reply from user (via tg):\n{}", d.text);
    let out = if kind == "stop" { json!({"decision": "block", "reason": msg}).to_string() } else { msg };
    emit(&sid, &d, &out);
}

fn status() {
    println!("daemon: {}", daemon_pid().map(|p| format!("running pid {p}")).unwrap_or("not running".into()));
    match session() {
        Some(s) => {
            let n = entries(&inbox_dir(&s)).iter().filter(|p| !base(p).starts_with('.')).count();
            println!("session: {} ({s})\nunread: {n}", tag(&s));
        }
        None => println!("session: none"),
    }
    for p in [cfg_dir().join("token"), secrets_env()] {
        if fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
            println!("warning: {} is readable by other users (chmod 600 it)", p.display());
        }
    }
}

fn chats() -> Result<(), String> {
    if let Some(p) = daemon_pid() {
        return Err(format!("daemon (pid {p}) is consuming updates; stop it first: systemctl --user stop tg"));
    }
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
    let base = test_env("TG_API").unwrap_or_else(|| "https://api.telegram.org".into());
    let tok = token()?;
    let r: Value = agent
        .post(&format!("{base}/bot{tok}/getUpdates"))
        .send_json(json!({"timeout": 0}))
        .and_then(|r| Ok(r.into_json()?))
        .map_err(|e| e.to_string().replace(&tok, "***"))?;
    let mut seen = std::collections::BTreeMap::new();
    for u in r["result"].as_array().into_iter().flatten() {
        let c = &u["message"]["chat"];
        if !c.is_null() {
            let name = c["username"].as_str().or(c["first_name"].as_str()).or(c["title"].as_str()).unwrap_or("?");
            seen.insert(c["id"].to_string(), format!("{} {name}", c["type"].as_str().unwrap_or("?")));
        }
    }
    if seen.is_empty() {
        println!("no messages yet — send your bot a message first");
    }
    for (id, desc) in seen {
        println!("{id} {desc}");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = |i: usize| args.get(i).map(String::as_str);
    let r = match a(0) {
        Some("send") => {
            let mut text = args[1..].join(" ");
            if text.is_empty() || text == "-" {
                text = io::read_to_string(io::stdin()).unwrap_or_default();
            }
            if text.trim().is_empty() {
                Err("empty message".into())
            } else {
                send(text.trim_end())
            }
        }
        Some("wait") => {
            wait(a(1).and_then(|s| s.parse().ok()));
            Ok(())
        }
        Some("inbox") => {
            let sid = need_session();
            ensure_daemon();
            if let Some(d) = deliver(&sid) {
                emit(&sid, &d, &d.text);
            }
            Ok(())
        }
        Some("hook") => {
            hook(a(1).unwrap_or("prompt"));
            Ok(())
        }
        Some("ack") => {
            // internal: spawned detached by `emit`
            ack(&args[1..].iter().filter_map(|s| s.parse().ok()).collect::<Vec<_>>());
            Ok(())
        }
        Some("status") => {
            status();
            Ok(())
        }
        Some("daemon") => daemon(a(1) == Some("--if-free")),
        Some("chats") => chats(),
        Some("setchat") => match a(1) {
            Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => {
                write_atomic(&cfg_dir().join("chat_id"), id.as_bytes()).map_err(|e| e.to_string())
            }
            _ => Err("setchat needs your numeric private chat id (see `tg chats`)".into()),
        },
        _ => Err(USAGE.into()),
    };
    if let Err(e) = r {
        eprintln!("tg: {e}");
        exit(1);
    }
}
