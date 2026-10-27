#!/usr/bin/env bash
# Failure-mode tests for tg against a local fake Telegram. Usage: tests/run.sh
set -u
cd "$(dirname "$0")/.."
command -v cargo >/dev/null || . "$HOME/.cargo/env"
cargo build -q --release || exit 1
cargo build -q --release --features testing --target-dir target/testing || exit 1
BIN="$PWD/target/testing/release/tg"
T=$(mktemp -d)
trap 'kill $(cat $T/state/daemon.lock 2>/dev/null) $MOCK 2>/dev/null; pkill -f "$T/" 2>/dev/null; rm -rf $T' EXIT
export TG_STATE=$T/state TG_CONFIG=$T/cfg TG_API=http://127.0.0.1:18777
mkdir -p $T/cfg $T/bin && echo fake > $T/cfg/token && echo 42 > $T/cfg/chat_id
# Fake "claude"/"codex" processes: copies of bash, so tg sees them as ancestors.
cp /bin/bash $T/bin/claude; cp /bin/bash $T/bin/codex
mkdir -p $T/p && ln -s "$BIN" $T/p/tg && export PATH=$T/p:$PATH
# Permissive umask: the 0600/0700 checks must prove tg's explicit modes, not the user's umask.
umask 000
python3 tests/mock_tg.py 18777 & MOCK=$!
until curl -s localhost:18777/log >/dev/null; do sleep 0.05; done

pass=0; fail=0
# Usage: <check command>; ok "name"   — records the exit status of the previous command.
ok() { if [ $? = 0 ]; then echo "PASS $1"; pass=$((pass+1)); else echo "FAIL $1"; fail=$((fail+1)); fi; }
inject() { curl -s -XPOST localhost:18777/inject -d "$1" >/dev/null; }
log() { curl -s localhost:18777/log; }
# Texts of sent messages containing $1, one JSON string per line.
sent_texts() { log | python3 -c 'import json,sys; [print(json.dumps(m["text"])) for m in json.load(sys.stdin)["sent"] if sys.argv[1] in m["text"]]' "$1"; }
sent_id() { log | python3 -c 'import json,sys; print([m["message_id"] for m in json.load(sys.stdin)["sent"] if sys.argv[1] in m["text"]][0])' "$1"; }
# plant <sid> <n> <text>: queue a reply for a session directly (0600, like tg writes).
plant() { install -D -m600 /dev/stdin "$T/state/inbox/$1/$(printf %020d "$2")" <<< "$(printf '1\nsnip\n%s' "$3")"; }

echo "== 1. routing: 3 concurrent sessions, replies sent in reverse order"
$T/bin/claude -c 'tg send "question-A"; tg wait 30; true' > $T/A.out 2>&1 & P1=$!
$T/bin/codex  -c 'tg send "question-B"; tg wait 30; true' > $T/B.out 2>&1 & P2=$!
$T/bin/claude -c 'tg send "question-C"; tg wait 30; true' > $T/C.out 2>&1 & P3=$!
sleep 1.5
for x in C B A; do inject "{\"text\":\"answer-for-$x\",\"reply_to\":$(sent_id question-$x)}"; done
wait $P1 $P2 $P3
for x in A B C; do grep -q answer-for-$x $T/$x.out && [ $(grep -c answer-for $T/$x.out) = 1 ]; ok "session $x got only its own reply"; done
log | grep -q 'codex·' && log | grep -q 'claude·'; ok "messages tagged with agent kind"
sleep 0.5  # 👀 acks are sent by detached processes
[ $(log | grep -o '👌' | wc -l) = 3 ]; ok "👌 reaction on each queued reply"
[ $(log | grep -o '👀' | wc -l) = 3 ]; ok "👀 reaction on each delivered reply"
[ $(pgrep -fc "$T/p/tg daemon") -le 1 ]; ok "single daemon despite 3 concurrent starters"

echo "== 2. daemon SIGKILLed while a reply is in flight; wait self-heals"
$T/bin/claude -c 'tg send "question-D"; tg wait 40; true' > $T/D.out 2>&1 & PD=$!
sleep 1
kill -9 $(cat $T/state/daemon.lock); sleep 0.3
inject "{\"text\":\"answer-for-D\",\"reply_to\":$(sent_id question-D)}"
wait $PD
grep -q answer-for-D $T/D.out; ok "reply delivered after daemon crash"

echo "== 3. two readers race for the same replies (50 rounds)"
dups=0
for i in $(seq 50); do
  plant race $i "race-$i"
  TG_SESSION=race tg inbox > $T/r1 & R1=$!; TG_SESSION=race tg inbox > $T/r2 & R2=$!; wait $R1 $R2
  [ $(cat $T/r1 $T/r2 | grep -c "race-$i") = 1 ] || dups=$((dups+1))
done
[ $dups = 0 ]; ok "every reply delivered exactly once"

echo "== 4. mistakes get a visible warning instead of silence"
inject '{"text":"no reply-to"}'
inject '{"text":"x","reply_to":999999}'
inject '{"photo":true,"reply_to":1}'
$T/bin/claude -c 'tg send "question-dead"; true'   # session exits immediately
inject "{\"text\":\"too late\",\"reply_to\":$(sent_id question-dead)}"
inject '{"text":"stranger","reply_to":1,"chat":666}'
inject '{"text":"group member","reply_to":1,"type":"group"}'
inject '{"text":"impostor","reply_to":1,"from":777}'
sleep 1.5
log | grep -q 'use Reply'; ok "non-reply warned"
log | grep -q "isn't from a tracked session"; ok "unknown message warned"
log | grep -q 'only text'; ok "non-text warned"
log | grep -q 'session claude·tg·[0-9]* has ended'; ok "ended session warned (with its tag)"
! log | grep -qE 'stranger|group member|impostor' && [ $(log | grep -o '⚠️' | wc -l) = 4 ]; ok "other chats, groups and other senders ignored"

echo "== 5. daemon restart does not re-deliver handled updates"
before=$(find $T/state/read $T/state/inbox -type f | wc -l)
kill $(cat $T/state/daemon.lock); sleep 0.3; TG_SESSION=x tg inbox; sleep 1.5
[ $(find $T/state/read $T/state/inbox -type f | wc -l) = $before ]; ok "no duplicates after restart"

echo "== 6. hooks"
plant h 1 hook-reply
echo '{}' | TG_SESSION=h tg hook stop | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["decision"]=="block" and "hook-reply" in d["reason"]'
ok "stop hook blocks with reply as JSON"
[ -z "$(echo '{}' | TG_SESSION=h tg hook stop)" ] && [ -z "$(echo '{}' | TG_SESSION=none tg hook prompt)" ]; ok "hooks silent when nothing pending"
setsid -f sh -c "echo '{}' | env -u TG_SESSION $BIN hook stop > $T/detached.out 2>&1; echo done >> $T/detached.done"; sleep 1
[ -f $T/detached.done ] && [ ! -s $T/detached.out ]; ok "hook silent outside any session"

echo "== 7. long message split into <=4000-char chunks, each routable"
head -c 9000 /dev/zero | tr '\0' x > $T/long; $T/bin/claude -c "tg send - < $T/long; true"
[ $(sent_texts xxxx | wc -l) = 3 ]; ok "3 chunks sent"

echo "== 8. security"
! TG_SESSION=../../x tg inbox 2>/dev/null && ! TG_SESSION=/etc tg inbox 2>/dev/null && ! TG_SESSION= tg inbox 2>/dev/null
ok "path-like session ids rejected"
[ -z "$(find $T/state -type d ! -perm 700)" ]; ok "state dirs are 0700 (under umask 000)"
[ -z "$(find $T/state -type f ! -perm 600)" ]; ok "state files are 0600 (under umask 000)"
! TG_SESSION=x tg chats 2>/dev/null; ok "chats refuses while daemon runs"
! tg setchat ../x 2>/dev/null && ! tg setchat -100123 2>/dev/null; ok "setchat rejects non-numeric ids"
! TG_STATE=$T/state TG_SESSION=x $PWD/target/release/tg status 2>&1 | grep -q 'session: x'; ok "release build ignores TG_* overrides"

echo "== 9. emoji message chunked by UTF-16 length"
python3 -c "print('😀'*3000)" > $T/emoji; $T/bin/claude -c "tg send - < $T/emoji; true"
sent_texts 😀 | python3 -c 'import json,sys; ms=[json.loads(l) for l in sys.stdin]; assert len(ms)==2 and all(len(t.encode("utf-16-le"))//2 <= 4096 for t in ms)'
ok "every chunk <= 4096 UTF-16 units"

echo "== 10. acks never block (hook timeout is short)"
plant h2 1 slow
s=$(date +%s%N); echo '{}' | TG_SESSION=h2 TG_API=http://10.255.255.1:9 tg hook prompt >/dev/null; e=$(( ($(date +%s%N)-s)/1000000 ))
[ $e -lt 1000 ]; ok "hook returns fast even if Telegram unreachable (${e}ms)"

echo; echo "passed $pass, failed $fail"; [ $fail = 0 ]
