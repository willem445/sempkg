import json
import subprocess

BIN = r"C:/Projects/sempkg/src/sempkg/target/debug/sempkg.exe"
WORKSPACE = r"C:/Projects/sempkg"
QUERY = "how does handle node dispatch work"
LIMIT = 4


proc = subprocess.Popen(
    [BIN, "mcp", "--workspace", WORKSPACE],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=None,
    text=True,
    bufsize=1,
)


def read_json_line():
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("mcp stdout closed")
        s = line.strip()
        if not s:
            continue
        if not s.startswith("{"):
            continue
        return json.loads(s)


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()
    return read_json_line()


send(
    {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "debug-probe", "version": "0"},
        },
    }
)

resp = send(
    {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "query",
            "arguments": {"query": QUERY, "limit": LIMIT},
        },
    }
)

text = ""
content = resp.get("result", {}).get("content", [])
if content:
    text = content[0].get("text", "")

print("=== MCP QUERY OUTPUT PREVIEW ===")
for ln in text.splitlines()[:20]:
    print(ln)

proc.stdin.close()
proc.terminate()
proc.wait(timeout=10)
