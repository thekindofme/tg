"""Minimal fake Telegram Bot API for tg's failure-mode tests.
POST /bot<tok>/<method> mimics Telegram; POST /inject queues an incoming user message;
GET /log returns everything the bot sent (messages + reactions)."""
import json, sys, threading, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

lock = threading.Condition()
updates, sent, reactions = [], [], []
next_update, next_msg = [1], [1000]

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, obj, code=200):
        b = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        with lock: self.reply({"sent": sent, "reactions": reactions})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if self.path == "/inject":
            with lock:
                next_msg[0] += 1
                chat = body.get("chat", 42)
                m = {"message_id": next_msg[0],
                     "chat": {"id": chat, "type": body.get("type", "private")},
                     "from": {"id": body.get("from", chat)}}
                if not body.get("photo"): m["text"] = body.get("text")
                if body.get("reply_to"): m["reply_to_message"] = {"message_id": body["reply_to"]}
                updates.append({"update_id": next_update[0], "message": m}); next_update[0] += 1
                lock.notify_all()
            return self.reply({"ok": True, "message_id": next_msg[0]})
        method = self.path.rsplit("/", 1)[1]
        if method == "getUpdates":
            off, deadline = body.get("offset", 0), time.time() + min(body.get("timeout", 0), 5)
            with lock:
                while True:
                    updates[:] = [u for u in updates if u["update_id"] >= off]  # confirm < offset
                    if updates or time.time() >= deadline: break
                    lock.wait(max(0.01, deadline - time.time()))
                return self.reply({"ok": True, "result": list(updates)})
        if method == "sendMessage":
            with lock:
                next_msg[0] += 1
                sent.append({"message_id": next_msg[0], **body})
            return self.reply({"ok": True, "result": {"message_id": next_msg[0]}})
        if method == "setMessageReaction":
            with lock: reactions.append(body)
            return self.reply({"ok": True, "result": True})
        return self.reply({"ok": True, "result": True})

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
