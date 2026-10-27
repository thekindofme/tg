#!/usr/bin/env bash
# Installs tg: binary, Claude Code skill + hooks, Codex AGENTS.md + hooks (if Codex is present),
# and a systemd user service. Idempotent. Edited config files are backed up once to *.bak-tg.
set -euo pipefail
D="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$D")"

command -v cargo >/dev/null || . "$HOME/.cargo/env"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
install -D -m755 "$ROOT/target/release/tg" ~/.local/bin/tg

install -D -m644 "$D/SKILL.md" ~/.claude/skills/tg/SKILL.md

HOOK_FILES=(~/.claude/settings.json)
if [ -d ~/.codex ]; then
  # Managed block, replaced on every run so upgrades refresh it
  python3 - "$D/AGENTS.md" ~/.codex/AGENTS.md <<'PY2'
import os, re, sys
block = "<!-- tg:begin -->\n" + open(sys.argv[1]).read().strip() + "\n<!-- tg:end -->\n"
path = sys.argv[2]
old = open(path).read() if os.path.exists(path) else ""
old = re.sub(r"\n*<!-- tg:begin -->.*?<!-- tg:end -->\n?", "\n", old, flags=re.S)
old = re.sub(r"\n*^## Telegram \(tg\)\n.*?(?=^## |\Z)", "\n", old, flags=re.S | re.M)  # pre-marker installs
open(path, "w").write(old.rstrip("\n") + ("\n\n" if old.strip() else "") + block)
PY2
  HOOK_FILES+=(~/.codex/hooks.json)
fi

# Hooks: UserPromptSubmit -> tg hook prompt, Stop -> tg hook stop (both silent unless a reply is waiting)
python3 - "${HOOK_FILES[@]}" <<'PY'
import json, os, shutil, sys
tg = os.path.expanduser("~/.local/bin/tg")
for path in sys.argv[1:]:
    d = json.load(open(path)) if os.path.exists(path) else {}
    if os.path.exists(path) and not os.path.exists(path + ".bak-tg"):
        shutil.copy(path, path + ".bak-tg")
    hooks = d.setdefault("hooks", {})
    for ev, arg in [("UserPromptSubmit", "prompt"), ("Stop", "stop")]:
        # Drop any previous tg hook (old path/timeout), then add the current one
        groups = [g for g in hooks.get(ev, []) if not any("tg hook" in h.get("command", "") for h in g.get("hooks", []))]
        groups.append({"hooks": [{"type": "command", "command": f"{tg} hook {arg}", "timeout": 10}]})
        hooks[ev] = groups
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(d, f, indent=2)
    os.replace(tmp, path)
    print("hooks ok:", path)
PY

# Background service: restarts automatically (sandboxed Codex sessions can't start it themselves)
install -D -m644 "$D/tg.service" ~/.config/systemd/user/tg.service
systemctl --user daemon-reload
systemctl --user enable tg.service
systemctl --user restart tg.service

# Tighten state left by older versions
for d in ~/.local/state/tg ~/.config/tg; do if [ -d "$d" ]; then chmod -R go-rwx "$d"; fi; done

echo "installed. Next: put TELEGRAM_BOT_TOKEN=... in ~/.secrets/telegram.env (chmod 600),"
echo "message your bot, then: tg chats && tg setchat <your id>"
