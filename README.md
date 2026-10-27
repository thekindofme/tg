# tg

Let your [Claude Code](https://claude.com/claude-code) and [Codex](https://github.com/openai/codex) sessions message you on Telegram — and get your replies back.

- A session runs `tg send "deploy done, ship it?"`. You get it on your phone, tagged with the session (`[claude·myrepo·48213]`).
- You hit **Reply**. The reply goes back to **that** session only — not to any other Claude or Codex session on the machine.
- Single Rust binary (~1.7 MB, two deps), plain files on disk, no database.

## How it works

```
 agent session ──tg send──▶ Telegram ──▶ you
                    │                     │ Reply
       map/<msg id> = session             ▼
                                   tg daemon (long-poll)
 agent session ◀──tg wait── inbox/<session>/ ◀── routed by reply-to id
```

- **One daemon** long-polls Telegram (only one poller per bot is allowed). It saves your reply to the right session's inbox *before* acknowledging it to Telegram, so a crash never loses a reply; Telegram redelivers for 24h.
- **Session identity** — Claude: the nearest `claude` ancestor process (pid + start time). Codex: `$CODEX_THREAD_ID` (Codex's sandbox hides the process tree). The agent never has to remember an id.
- **Receipts** — 👌 on your reply when it's queued, 👀 when the session has actually read it. Mistakes are never silent: a message that isn't a Reply, or a reply to an ended Claude session, gets a ⚠️ answer. (Codex sessions can't be checked from outside their sandbox, so a reply to an ended one gets 👌 but never 👀.)
- **Self-healing** — any `tg` command starts the daemon (via the systemd unit, or directly as a fallback) if it isn't running; systemd restarts it if it dies.
- **Hooks** (optional backup) — on each prompt / turn end, a pending reply is injected into the session, so replies that arrive after `tg wait` gave up still get picked up.

## Commands

| | |
|---|---|
| `tg send "msg"` / `tg send - < file` | Send. Silent on success, non-zero exit = not sent. |
| `tg wait [secs]` | Block until a reply arrives; prints it. Exit 2 = timed out. |
| `tg inbox` | Print unread replies now. |
| `tg status` | Daemon, current session, unread count. |
| `tg chats` / `tg setchat <id>` | One-time setup: find and save your chat id. |

## Install

Requires Linux, Rust (`rustup`), Python 3 (installer only), systemd user services.

1. Create a bot: message [@BotFather](https://t.me/BotFather), `/newbot`, copy the token.
2. Store the token (don't paste it into an agent chat):
   ```sh
   mkdir -p ~/.secrets && read -rsp 'token: ' t && printf 'TELEGRAM_BOT_TOKEN=%s\n' "$t" > ~/.secrets/telegram.env && chmod 600 ~/.secrets/telegram.env
   ```
3. Install: `./install/install.sh` — builds `tg` into `~/.local/bin`, adds the Claude skill + hooks, the Codex `AGENTS.md` section + hooks (if `~/.codex` exists), and the systemd user service.
4. Send your bot any message, then lock `tg` to your private chat:
   ```sh
   systemctl --user stop tg && tg chats     # prints: <id> private <you>
   tg setchat <id> && systemctl --user start tg
   ```
5. Optional: `sudo loginctl enable-linger $USER` so the service runs without a login session.

### Codex sandbox

Codex's default `workspace-write` sandbox blocks network access and writes outside the project, so `tg` can't run inside it. Either approve `tg` calls when Codex asks, or allow it in `~/.codex/config.toml`:

```toml
[sandbox_workspace_write]
network_access = true                              # note: applies to all sandboxed commands
writable_roots = ["/home/YOU/.local/state/tg"]
```

## Security model

- **Whoever controls your Telegram account can instruct your agents.** Replies are injected into sessions as user instructions, and agents may be running with broad permissions. Protect the account (2FA, locked phone).
- Only messages from **your private chat** are accepted: chat id, chat type `private`, and sender id must all match. Groups, other users, edits and channel posts are ignored. `tg` only ever sends to that chat.
- The bot token is read from `~/.secrets/telegram.env` (or `~/.config/tg/token`, or `$TELEGRAM_BOT_TOKEN`) and is scrubbed from all error output. `tg status` warns if the file is readable by others.
- State (`~/.local/state/tg`) and config are created owner-only (dirs 0700, files 0600).
- Release builds ignore the `TG_*` env overrides used by the test suite, so a hostile environment can't redirect the token or state. Session ids are restricted to `[A-Za-z0-9_-]` before being used as paths.
- Messages are sent as plain text (no Markdown/HTML parsing).

## Tests

```sh
tests/run.sh
```

Runs against a local fake Telegram server (`tests/mock_tg.py`): concurrent sessions with out-of-order replies, daemon SIGKILL mid-flight, racing readers (exactly-once delivery), warnings for every mistake, foreign chats/groups/senders ignored, restart without duplicates, hooks, UTF-16 chunking, permissions, path-safe session ids.

## License

MIT
