#!/usr/bin/env python3
# Usage: jobs.py LABEL [--field k=v]... FILE...   one async job (async=true): accept time, time to the end (polling every 20 ms), poll cost, result fetch time and size; gateway CPU/RSS as bench.py
import os, sys, time, json, http.client, urllib.parse, uuid
GW = os.environ.get("GW", "http://127.0.0.1:8888"); TOKEN = os.environ["TOKEN"]; PID = int(os.environ["GWPID"])
args = sys.argv[1:]; label = args.pop(0); fields = [("async", "true")]; files = []
while args:
    a = args.pop(0)
    if a == "--field": k, v = args.pop(0).split("=", 1); fields.append((k, v))
    else: files.append(a)
TCK = os.sysconf("SC_CLK_TCK")
def cpu():
    s = open(f"/proc/{PID}/stat").read().rsplit(")", 1)[1].split(); return (int(s[11]) + int(s[12])) / TCK, (int(s[13]) + int(s[14])) / TCK
def st(key):
    for l in open(f"/proc/{PID}/status"):
        if l.startswith(key + ":"): return int(l.split()[1]) / 1024
b = uuid.uuid4().hex; out = []
for k, v in fields: out.append(f'--{b}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode())
for f in files:
    out.append(f'--{b}\r\nContent-Disposition: form-data; name="file"; filename="{os.path.basename(f)}"\r\n\r\n'.encode()); out.append(open(f, "rb").read()); out.append(b"\r\n")
out.append(f"--{b}--\r\n".encode()); payload = b"".join(out)
u = urllib.parse.urlparse(GW); c = http.client.HTTPConnection(u.hostname, u.port, timeout=3600); H = {"Authorization": "Bearer " + TOKEN}
open(f"/proc/{PID}/clear_refs", "w").write("5"); rss0 = st("VmRSS"); own0, kid0 = cpu(); t0 = time.perf_counter()
c.request("POST", "/v1/documents/convert", body=payload, headers={**H, "Content-Type": f"multipart/form-data; boundary={b}"}); r = c.getresponse(); d = json.loads(r.read()); t_acc = time.perf_counter() - t0
job = d.get("job"); polls = 0; poll_t = 0.0; state = None
while True:
    t = time.perf_counter(); c.request("GET", f"/v1/documents/jobs/{job}", headers=H); r = c.getresponse(); s = json.loads(r.read()); poll_t += time.perf_counter() - t; polls += 1
    state = s.get("state") or s.get("status")
    if state not in ("running", "queued", "pending", "started"): break
    time.sleep(0.02)
t_done = time.perf_counter() - t0; own1, kid1 = cpu()
t = time.perf_counter(); c.request("GET", f"/v1/documents/jobs/{job}/result", headers=H); r = c.getresponse(); res = r.read(); t_res = time.perf_counter() - t
own2, _ = cpu()
print(f"{label:30s} accept {t_acc:5.2f}s done {t_done:6.2f}s ({state}) polls {polls} avg_poll {poll_t/polls*1000:.2f}ms result {t_res:5.2f}s {len(res)/(1<<20):.2f}M http {r.status} | gw_cpu {own1-own0:.2f}s (+{own2-own1:.2f}s result) kids_cpu {kid1-kid0:.2f}s gw_rss {rss0:.0f}->peak {st('VmHWM'):.0f}M in {len(payload)/(1<<20):.1f}M")
c.request("DELETE", f"/v1/documents/jobs/{job}", headers=H); c.getresponse().read()
