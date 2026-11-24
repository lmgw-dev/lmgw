#!/usr/bin/env python3
# Usage: bench.py LABEL [--field k=v]... [--json] [--callers N] [--repeat N] [--save OUT] [--path /v1/documents/convert] FILE...   (scripts/convert-perf: seq.sh runs it)
#   env: GW (url), TOKEN, GWPID, CG (cgroup dir, optional)
# One request with all FILEs (or N callers each sending that request at once); prints wall, the
# gateway's own CPU, its children's CPU (waited-for), gateway peak RSS (VmHWM, reset before), the
# cgroup's peak memory (gateway + children, reset before), status and answer size.
import os, sys, time, json, threading, http.client, urllib.parse, uuid, base64
GW = os.environ.get("GW", "http://127.0.0.1:8888"); TOKEN = os.environ["TOKEN"]; PID = int(os.environ["GWPID"]); CG = os.environ.get("CG")
args = sys.argv[1:]; label = args.pop(0); fields = []; files = []; callers = 1; repeat = 1; as_json = False; save = None; path = "/v1/documents/convert"
while args:
    a = args.pop(0)
    if a == "--field": k, v = args.pop(0).split("=", 1); fields.append((k, v))
    elif a == "--callers": callers = int(args.pop(0))
    elif a == "--repeat": repeat = int(args.pop(0))
    elif a == "--json": as_json = True
    elif a == "--save": save = args.pop(0)
    elif a == "--path": path = args.pop(0)
    else: files.append(a)
TCK = os.sysconf("SC_CLK_TCK")
def cpu():
    s = open(f"/proc/{PID}/stat").read().rsplit(")", 1)[1].split()
    return (int(s[11]) + int(s[12])) / TCK, (int(s[13]) + int(s[14])) / TCK   # own, children
def status(key):
    for l in open(f"/proc/{PID}/status"):
        if l.startswith(key + ":"): return int(l.split()[1]) / 1024
def io():
    d = dict(l.split(": ") for l in open(f"/proc/{PID}/io").read().splitlines())
    return int(d["rchar"]) / (1 << 20), int(d["wchar"]) / (1 << 20)
def reset():
    try: open(f"/proc/{PID}/clear_refs", "w").write("5")
    except OSError as e: print("clear_refs:", e, file=sys.stderr)
    global PEAKFD
    if CG:
        try:
            if PEAKFD is None: PEAKFD = os.open(CG + "/memory.peak", os.O_RDWR)
            os.pwrite(PEAKFD, b"0", 0)   # resets the peak seen through this descriptor
        except OSError as e: print("memory.peak reset:", e, file=sys.stderr)
PEAKFD = None
def cgpeak():
    return int(os.pread(PEAKFD, 64, 0)) / (1 << 20) if PEAKFD is not None else float("nan")
def cgnow():
    return int(open(CG + "/memory.current").read()) / (1 << 20) if CG else float("nan")
def body():
    if as_json:
        d = {k: v for k, v in fields}
        d["files"] = [{"name": os.path.basename(f), "data_base64": base64.b64encode(open(f, "rb").read()).decode()} for f in files]
        return json.dumps(d).encode(), "application/json"
    b = uuid.uuid4().hex; out = []
    for k, v in fields:
        out.append(f'--{b}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode())
    fname = "files" if path.startswith("/v1/convert") else "file"
    for f in files:
        out.append(f'--{b}\r\nContent-Disposition: form-data; name="{fname}"; filename="{os.path.basename(f)}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode())
        out.append(open(f, "rb").read()); out.append(b"\r\n")
    out.append(f"--{b}--\r\n".encode())
    return b"".join(out), f"multipart/form-data; boundary={b}"
u = urllib.parse.urlparse(GW)
payload, ctype = body()
results = [None] * callers
def one(i):
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=3600)
    t = time.perf_counter()
    c.request("POST", path, body=payload, headers={"Content-Type": ctype, "Authorization": "Bearer " + TOKEN})
    r = c.getresponse(); data = r.read()
    results[i] = (r.status, data, time.perf_counter() - t)
for rep in range(repeat):
    reset(); cg0 = cgnow(); rss0 = status("VmRSS"); own0, kid0 = cpu(); r0, w0 = io(); t0 = time.perf_counter()
    th = [threading.Thread(target=one, args=(i,)) for i in range(callers)]
    [t.start() for t in th]; [t.join() for t in th]
    wall = time.perf_counter() - t0; own1, kid1 = cpu(); r1, w1 = io()
    st, data, _ = results[0]
    info = ""
    try:
        j = json.loads(data)
        if "documents" in j:
            docs = j["documents"]; codes = {}
            for d in docs: codes[d["status"]] = codes.get(d["status"], 0) + 1
            tm = sum(d.get("stats", {}).get("time_ms", 0) for d in docs)
            parts = sum(d.get("parts", 0) for d in docs)
            warns = sorted({w["code"] for d in docs for w in d.get("warnings", [])})
            errs = sorted({d["error"]["code"] for d in docs if d.get("error")})
            info = f"docs {codes} parts {parts} sum_time_ms {tm} warn {warns} err {errs}"
        else: info = json.dumps(j)[:300]
    except Exception: info = f"non-json {data[:80]!r}"
    lat = sorted(r[2] for r in results)
    print(f"{label:34s} wall {wall:7.2f}s gw_cpu {own1-own0:6.2f}s kids_cpu {kid1-kid0:7.2f}s gw_rss {rss0:6.0f}->peak {status('VmHWM'):6.0f}M cg {cg0:5.0f}->peak {cgpeak():6.0f}M written(gw+kids) {w1-w0:8.1f}M in {len(payload)/(1<<20):7.1f}M out {len(data)/(1<<20):7.2f}M http {st} lat_max {lat[-1]:.2f} {info}", flush=True)
    if save: open(save, "wb").write(data)
