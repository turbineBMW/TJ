"""Turn a raw recording from record-claude-session.py into a fixture safe to commit.

Usage: python3 -I scripts/scrub-claude-fixture.py RAW.jsonl FIXTURE.jsonl

Keeps the messages and fields TJ parses, and removes what identifies the user or their
setup: account details, MCP servers, plugins, skills, hooks, memory paths, thinking
signatures, email addresses and local paths. Fails if anything that looks personal
survives, so check its output before committing.
"""
import json, os, re, sys

raw_path, out_path = sys.argv[1], sys.argv[2]
home = os.path.expanduser("~")
user = os.path.basename(home)

INIT_KEEP = {"type", "subtype", "session_id", "model", "permissionMode", "claude_code_version",
             "cwd", "uuid", "tools", "apiKeySource", "output_style"}
INITIALIZE_KEEP = {"claude_code_version", "current_permission_mode", "session_state",
                   "pending_permission_requests"}
RESULT_DROP = {"credential_expires_at"}

def scrub_str(s, workdirs):
    for w in workdirs:
        s = s.replace(w, "/work")
    # Streamed text can split a path across chunks, so whole-path replacement misses pieces.
    s = s.replace("/tmp/", "/work/")
    # Claude's per-user temp folder is named after the user id (claude-<uid>).
    s = re.sub(r"claude-\d+", "claude-uid", s)
    s = s.replace(home, "/home/user").replace(user, "user")
    s = re.sub(r"[\w.+-]+@[\w-]+\.[\w.]+", "user@example.com", s)
    return s

def scrub(value, workdirs):
    if isinstance(value, str):
        return scrub_str(value, workdirs)
    if isinstance(value, list):
        return [scrub(v, workdirs) for v in value]
    if isinstance(value, dict):
        return {k: scrub(v, workdirs) for k, v in value.items()}
    return value

records = [json.loads(line) for line in open(raw_path) if line.strip()]
workdirs = sorted({r["msg"]["cwd"] for r in records
                   if r.get("msg", {}).get("type") == "system" and "cwd" in r["msg"]}, key=len, reverse=True)

out = []
for record in records:
    if record.get("dir") not in ("in", "out"):
        continue
    m = record["msg"]
    t, sub = m.get("type"), m.get("subtype")
    if t == "system" and sub in ("hook_started", "hook_response"):
        continue
    if t == "system" and sub == "init":
        m = {k: v for k, v in m.items() if k in INIT_KEEP}
        m["tools"] = [tool for tool in m.get("tools", []) if not tool.startswith("mcp__")]
    if t == "control_response":
        response = m["response"]
        body = response.get("response") or {}
        if response.get("request_id", "").startswith("tj-initialize-"):
            response["response"] = {k: v for k, v in body.items() if k in INITIALIZE_KEEP}
        elif response.get("request_id", "").startswith("tj-get_usage-"):
            limits = {k: v for k, v in (body.get("rate_limits") or {}).items() if v is not None}
            response["response"] = {"subscription_type": body.get("subscription_type"),
                                    "rate_limits_available": body.get("rate_limits_available"),
                                    "rate_limits": limits}
    if t == "result":
        m = {k: v for k, v in m.items() if k not in RESULT_DROP}
    if t == "assistant":
        for block in m.get("message", {}).get("content", []):
            if "signature" in block:
                block["signature"] = "REDACTED"
    out.append({"dir": record["dir"], "msg": scrub(m, workdirs)})

text = "\n".join(json.dumps(r) for r in out) + "\n"
if re.search(r"claude-\d+", text):
    raise SystemExit("a per-user temp folder name survived scrubbing")
for needle in [home, user, "@", "mcp__", "/tmp/"]:
    if needle == "@":
        leaks = [e for e in re.findall(r"[\w.+-]+@[\w-]+\.[\w.]+", text) if e != "user@example.com"]
        assert not leaks, f"email-like text survived: {leaks[:3]}"
    else:
        assert needle not in text, f"{needle!r} survived scrubbing"
open(out_path, "w").write(text)
print(f"{len(out)} messages -> {out_path}")
