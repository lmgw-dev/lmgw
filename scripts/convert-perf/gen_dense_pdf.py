#!/usr/bin/env python3
# Usage: gen_dense_pdf.py OUT.pdf PAGES WORDS_PER_PAGE   hand-written PDF, Helvetica, tiny font
import sys, random
random.seed(11)
out, pages, wpp = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
W = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt labore dolore magna aliqua enim minim veniam quis nostrud".split()
fs = 2.5 if wpp > 3000 else 9.0
lead = fs*1.2; width = 595-72; cpl = int(width/(fs*0.5))
objs = []
def add(b): objs.append(b); return len(objs)
font = add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
pages_id = len(objs)+1; objs.append(None)
kids = []
for p in range(pages):
    lines=[]; n=0; cur=""
    while n < wpp:
        w = random.choice(W); n+=1
        if len(cur)+len(w)+1 > cpl: lines.append(cur); cur=w
        else: cur = (cur+" "+w).strip()
    lines.append(cur)
    maxl = int((842-72)/lead); lines = lines[:maxl]
    s = f"BT /F1 {fs} Tf {lead} TL 36 806 Td\n" + "".join(f"({l}) Tj T*\n" for l in lines) + "ET\n"
    sb = s.encode()
    c = add(b"<< /Length %d >>\nstream\n" % len(sb) + sb + b"endstream")
    pg = add(b"<< /Type /Page /Parent %d 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 %d 0 R >> >> /Contents %d 0 R >>" % (pages_id, font, c))
    kids.append(pg)
objs[pages_id-1] = b"<< /Type /Pages /Count %d /Kids [%s] >>" % (len(kids), b" ".join(b"%d 0 R" % k for k in kids))
cat = add(b"<< /Type /Catalog /Pages %d 0 R >>" % pages_id)
with open(out,"wb") as f:
    f.write(b"%PDF-1.4\n"); offs=[]
    for i,o in enumerate(objs):
        offs.append(f.tell()); f.write(b"%d 0 obj\n" % (i+1) + o + b"\nendobj\n")
    x = f.tell(); f.write(b"xref\n0 %d\n" % (len(objs)+1)); f.write(b"0000000000 65535 f \n")
    for o in offs: f.write(b"%010d 00000 n \n" % o)
    f.write(b"trailer\n<< /Size %d /Root %d 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs)+1, cat, x))
