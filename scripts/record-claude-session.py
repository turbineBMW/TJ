"""Record one short claude session over the stdio protocol TJ speaks.

Usage: python3 -I scripts/record-claude-session.py RAW.jsonl [allow|deny] [MODE] [PROMPT]

Runs `claude` on Haiku in a scratch folder with permission MODE (default "default") and
PROMPT (default: `touch tj-probe.txt`, which needs approval). Permission requests are
answered with allow or deny; an AskUserQuestion is answered with each question's first
option. Then it asks for usage.
Every line in either direction is written as {"dir": "in"|"out", "msg": ...}.

The raw recording contains account details and local paths. Never commit it; run
scripts/scrub-claude-fixture.py on it to make a fixture for tests/fixtures/claude/.
It uses a little of the logged-in account's usage (about a cent on Haiku).
"""
import json, os, subprocess, sys, threading, time, queue

out_path = sys.argv[1]
decision = sys.argv[2] if len(sys.argv) > 2 else "allow"
mode = sys.argv[3] if len(sys.argv) > 3 else "default"
prompt = sys.argv[4] if len(sys.argv) > 4 else "Use the Bash tool to run exactly: touch tj-probe.txt. Then reply with one word: DONE."
import tempfile
workdir = tempfile.mkdtemp(prefix="tj-probe-")

cmd = [
    "claude", "-p",
    "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool=stdio",
    "--permission-mode", mode,
    "--model", "haiku",
]
proc = subprocess.Popen(cmd, cwd=workdir, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, text=True, bufsize=1)
log = open(out_path, "w")
lines = queue.Queue()

def reader():
    for line in proc.stdout:
        lines.put(line)
    lines.put(None)

threading.Thread(target=reader, daemon=True).start()

def send(msg):
    log.write(json.dumps({"dir": "in", "msg": msg}) + "\n")
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()

send({"type": "control_request", "request_id": "tj-initialize-1", "request": {"subtype": "initialize"}})
send({"type": "user", "parent_tool_use_id": None, "message": {"role": "user", "content": [
    {"type": "text", "text": prompt}]}})

deadline = time.time() + 150
usage_sent = False
while time.time() < deadline:
    try:
        line = lines.get(timeout=5)
    except queue.Empty:
        continue
    if line is None:
        break
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        log.write(json.dumps({"dir": "out", "raw": line}) + "\n")
        continue
    log.write(json.dumps({"dir": "out", "msg": msg}) + "\n")
    t = msg.get("type")
    if t == "control_request" and msg["request"].get("subtype") == "can_use_tool":
        req = msg["request"]
        if req.get("tool_name") == "AskUserQuestion":
            tool_input = dict(req.get("input", {}))
            tool_input["answers"] = {q["question"]: q["options"][0]["label"] for q in tool_input.get("questions", [])}
            result = {"behavior": "allow", "updatedInput": tool_input}
        elif decision == "allow":
            result = {"behavior": "allow", "updatedInput": req.get("input", {})}
        else:
            result = {"behavior": "deny", "message": "The user declined this tool call."}
        send({"type": "control_response", "response": {
            "subtype": "success", "request_id": msg["request_id"], "response": result}})
    elif t == "result" and not usage_sent:
        usage_sent = True
        send({"type": "control_request", "request_id": "tj-get_usage-2", "request": {"subtype": "get_usage"}})
    elif t == "control_response" and msg.get("response", {}).get("request_id") == "tj-get_usage-2":
        break

proc.stdin.close()
try:
    proc.wait(timeout=20)
except subprocess.TimeoutExpired:
    proc.kill()
log.write(json.dumps({"dir": "meta", "exit": proc.returncode, "stderr": proc.stderr.read()[-2000:]}) + "\n")
log.close()
print("exit", proc.returncode)
