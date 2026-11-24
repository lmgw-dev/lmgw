#!/usr/bin/env python3
# Usage: chat.py LABEL ALIAS [--repeat N] [--no-stream]   one chat completion through the gateway (GW, TOKEN, GWPID as bench.py), read to its end; prints wall, the gateway's CPU, its peak RSS (VmHWM, reset before) and the bytes received (design §14 performance round review, the allocator rows; chat-mock.py is the upstream)
import os, sys, time, json, http.client, urllib.parse
GW = os.environ.get("GW", "http://127.0.0.1:8888"); TOKEN = os.environ["TOKEN"]; PID = int(os.environ["GWPID"])
args = sys.argv[1:]; label = args.pop(0); alias = args.pop(0); repeat = 1; stream = True
while args:
    a = args.pop(0)
    if a == "--repeat": repeat = int(args.pop(0))
    elif a == "--no-stream": stream = False
TCK = os.sysconf("SC_CLK_TCK")
def cpu():
    s = open(f"/proc/{PID}/stat").read().rsplit(")", 1)[1].split()
    return (int(s[11]) + int(s[12])) / TCK
def hwm():
    for l in open(f"/proc/{PID}/status"):
        if l.startswith("VmHWM:"): return int(l.split()[1]) / 1024
u = urllib.parse.urlparse(GW)
body = json.dumps({"model": alias, "stream": stream, "messages": [{"role": "user", "content": "Example."}]}).encode()
for _ in range(repeat):
    open(f"/proc/{PID}/clear_refs", "w").write("5")
    c0 = cpu(); t = time.perf_counter()
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=600)
    c.request("POST", "/v1/chat/completions", body=body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + TOKEN})
    r = c.getresponse(); n = 0
    while True:
        b = r.read(1 << 16)
        if not b: break
        n += len(b)
    wall = time.perf_counter() - t
    print(f"{label:30s} wall {wall:6.2f}s gw_cpu {cpu()-c0:5.2f}s gw_hwm {hwm():6.0f}M got {n/(1<<20):7.1f}M http {r.status}", flush=True)
