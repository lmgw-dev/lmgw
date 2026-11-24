#!/usr/bin/env python3
# Usage: table.py out/seq-A.txt [out/seq-B.txt ...]   wall / gateway CPU / children CPU / gateway peak RSS over start, per row, side by side
import sys, re
rows = {}; order = []
for f in sys.argv[1:]:
    for line in open(f):
        m = re.match(r"(\S+) (.*?)\s+wall\s+([\d.]+)s gw_cpu\s+([\d.]+)s kids_cpu\s+([\d.]+)s gw_rss\s+(\d+)->peak\s+(\d+)M cg\s+(\d+)->peak\s+(\d+)M", line)
        if not m: continue
        tag, name = m.group(1), m.group(2).strip()
        key = name
        if key not in rows: rows[key] = {}; order.append(key)
        rows[key].setdefault(tag, []).append("%s/%s/%s/%d/%d" % (m.group(3), m.group(4), m.group(5), int(m.group(7)) - int(m.group(6)), int(m.group(9)) - int(m.group(8))))
tags = [open(f).readline().split()[0] for f in sys.argv[1:]]
print("| request | " + " | ".join(tags) + " |"); print("|---" * (len(tags) + 1) + "|")
for k in order:
    print("| %s | %s |" % (k, " | ".join(", ".join(rows[k].get(t, ["-"])) for t in tags)))
