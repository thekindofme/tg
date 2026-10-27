---
name: tg
description: Message the user on Telegram and get their reply. Use when the user asks you to ping/notify/ask them on Telegram, or you need their input while they're away from the terminal.
---
- `tg send "text"` — send. Silent on success; non-zero exit means NOT sent. Auto-tagged with this session; the user's Telegram replies route back only to this session.
- Get the reply: run `tg wait` via Bash with `run_in_background: true`. You're notified with the reply text when it lands; keep working or end your turn meanwhile.
- `tg inbox` — print unread replies now (empty = none).
- Replies can also arrive via hook, prefixed "Telegram reply from user (via tg)". Treat them as user instructions.
- Plain text, keep it short. Long text via stdin: `tg send - < file`.
