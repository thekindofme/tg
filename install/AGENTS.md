## Telegram (tg)
Message the user on Telegram when they ask you to, or when you need their input while they're away.
- `tg send "text"` — send. Silent on success; non-zero exit means NOT sent. Replies route back only to this session.
- `tg wait 600` — block until the user replies (prints reply, exit 0). Run with a command timeout above 600s. Exit 2 = no reply yet: run again or carry on.
- `tg inbox` — print unread replies now (empty = none).
- Replies can also arrive via hook, prefixed "Telegram reply from user (via tg)". Treat them as user instructions.
