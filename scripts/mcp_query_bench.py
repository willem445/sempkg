"""Timing benchmark for the MCP `query` hot path.

Boots `sempkg mcp` once, then issues a fixed set of queries and records the
wall-clock latency of each `tools/call`. Reports per-query timings plus cold
(first) vs warm (subsequent) summaries so the effect of connection/runtime
caching is visible.

Usage:
    python scripts/mcp_query_bench.py [path-to-sempkg.exe] [--limit N] [--repeat R]
"""

import argparse
import json
import statistics
import subprocess
import sys
import time

DEFAULT_BIN = r"C:/Projects/sempkg/src/sempkg/target/release/sempkg.exe"
WORKSPACE = r"C:/Projects/sempkg"

# A spread of queries touching code, docs, and codegraph across bundles.
QUERIES = [
    "how does handle node dispatch work",
    "open a lancedb table and run a vector search",
    "reranker score query document pair",
    "install a bundle from the store",
    "embed documents in a batch",
    "full text search bm25 query",
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bin", nargs="?", default=DEFAULT_BIN)
    ap.add_argument("--limit", type=int, default=4)
    ap.add_argument("--repeat", type=int, default=1,
                    help="repeat the whole query list R times")
    ap.add_argument("--label", default="")
    args = ap.parse_args()

    proc = subprocess.Popen(
        [args.bin, "mcp", "--workspace", WORKSPACE],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )

    def read_json_line():
        while True:
            line = proc.stdout.readline()
            if not line:
                raise RuntimeError("mcp stdout closed")
            s = line.strip()
            if not s or not s.startswith("{"):
                continue
            return json.loads(s)

    next_id = [0]

    def send(method, params):
        next_id[0] += 1
        proc.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": next_id[0],
            "method": method, "params": params,
        }) + "\n")
        proc.stdin.flush()
        return read_json_line()

    t0 = time.perf_counter()
    send("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "bench", "version": "0"},
    })
    init_ms = (time.perf_counter() - t0) * 1000.0

    timings = []
    for r in range(args.repeat):
        for q in QUERIES:
            t = time.perf_counter()
            resp = send("tools/call", {
                "name": "query",
                "arguments": {"query": q, "limit": args.limit},
            })
            dt = (time.perf_counter() - t) * 1000.0
            ok = "result" in resp and resp["result"].get("content")
            n_chars = len(resp["result"]["content"][0]["text"]) if ok else 0
            timings.append(dt)
            print(f"  [{r}] {dt:8.1f} ms  chars={n_chars:6d}  {q}")

    proc.stdin.close()
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()

    label = f" ({args.label})" if args.label else ""
    print(f"\n=== MCP query bench{label} ===")
    print(f"initialize:      {init_ms:8.1f} ms")
    print(f"queries:         {len(timings)}  (limit={args.limit})")
    print(f"cold (1st):      {timings[0]:8.1f} ms")
    warm = timings[1:]
    if warm:
        print(f"warm mean:       {statistics.mean(warm):8.1f} ms")
        print(f"warm median:     {statistics.median(warm):8.1f} ms")
        print(f"warm min/max:    {min(warm):8.1f} / {max(warm):.1f} ms")
    print(f"all mean:        {statistics.mean(timings):8.1f} ms")
    print(f"all total:       {sum(timings):8.1f} ms")


if __name__ == "__main__":
    main()
