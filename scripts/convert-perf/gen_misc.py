#!/usr/bin/env python3
# Usage: gen_misc.py DIR   big table html, big csv, 200 small html, eml with 40 attachments
import sys, os, random
from email.message import EmailMessage
d = sys.argv[1]; random.seed(5)
W = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor".split()
with open(f"{d}/bigtable.html","w") as f:
    f.write("<html><body><h1>Big table</h1><table border=1>\n<tr>"+"".join(f"<th>H{c}</th>" for c in range(10))+"</tr>\n")
    for r in range(5000):
        f.write("<tr>"+"".join(f"<td>{random.choice(W)} {random.randint(1,99999)}</td>" for c in range(10))+"</tr>\n")
    f.write("</table></body></html>")
with open(f"{d}/big.csv","w") as f:
    f.write(",".join(f"col{c}" for c in range(20))+"\n")
    for r in range(100000):
        f.write(",".join(str(random.randint(1,999999)) if c%2 else random.choice(W) for c in range(20))+"\n")
os.makedirs(f"{d}/html200", exist_ok=True)
for i in range(200):
    with open(f"{d}/html200/page{i:03d}.html","w") as f:
        f.write(f"<html><head><title>Page {i}</title></head><body><h1>Page {i}</h1>")
        for p in range(8):
            f.write("<p>"+" ".join(random.choice(W) for _ in range(80))+"</p>")
        f.write("<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table></body></html>")
m = EmailMessage(); m["From"]="sender@mail.example"; m["To"]="rcpt@mail.example"; m["Subject"]="Forty attachments"
m.set_content("Body text. "+" ".join(random.choice(W) for _ in range(200)))
for i in range(40):
    kind = i % 4
    if kind == 0:
        m.add_attachment(open(f"{d}/html200/page{i:03d}.html","rb").read(), maintype="text", subtype="html", filename=f"att{i:02d}.html")
    elif kind == 1:
        m.add_attachment(open(f"{d}/text5.docx","rb").read(), maintype="application", subtype="vnd.openxmlformats-officedocument.wordprocessingml.document", filename=f"att{i:02d}.docx")
    elif kind == 2:
        m.add_attachment(open(f"{d}/text5.pdf","rb").read(), maintype="application", subtype="pdf", filename=f"att{i:02d}.pdf")
    else:
        m.add_attachment((" ".join(random.choice(W) for _ in range(500))).encode(), maintype="text", subtype="plain", filename=f"att{i:02d}.txt")
open(f"{d}/att40.eml","wb").write(bytes(m))
m = EmailMessage(); m["From"]="sender@mail.example"; m["To"]="rcpt@mail.example"; m["Subject"]="Forty html attachments"
m.set_content("Body text.")
for i in range(40):
    m.add_attachment(open(f"{d}/html200/page{i:03d}.html","rb").read(), maintype="text", subtype="html", filename=f"att{i:02d}.html")
open(f"{d}/att40html.eml","wb").write(bytes(m))
