#!/usr/bin/env python3
# Usage: gen_md.py OUT.md PAGES_APPROX [--tables N] [--pics DIR N]
import random, sys
random.seed(7)
W = ("lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna "
     "aliqua enim ad minim veniam quis nostrud exercitation ullamco laboris nisi aliquip ex ea commodo consequat duis aute "
     "irure in reprehenderit voluptate velit esse cillum fugiat nulla pariatur excepteur sint occaecat cupidatat non proident "
     "sunt culpa qui officia deserunt mollit anim id est laborum gateway document page table model layout").split()
def sent(n): 
    s = " ".join(random.choice(W) for _ in range(n)); return s[0].upper()+s[1:]+"."
def para(): return " ".join(sent(random.randint(8,22)) for _ in range(random.randint(3,7)))
out, pages = sys.argv[1], int(sys.argv[2])
tables = int(sys.argv[sys.argv.index("--tables")+1]) if "--tables" in sys.argv else pages//10
pics = None
if "--pics" in sys.argv:
    i = sys.argv.index("--pics"); pics = (sys.argv[i+1], int(sys.argv[i+2]))
with open(out, "w") as f:
    # ~ 450 words per page
    words_target = pages*450; words = 0; sec = 0; t = 0; p = 0
    while words < words_target:
        sec += 1
        f.write(f"# Chapter {sec}: {sent(4)}\n\n")
        for sub in range(random.randint(2,4)):
            f.write(f"## Section {sec}.{sub+1} {sent(3)}\n\n")
            for _ in range(random.randint(3,6)):
                x = para(); words += len(x.split()); f.write(x+"\n\n")
            if t < tables and random.random() < 0.4:
                t += 1
                cols = random.randint(3,6)
                f.write("| "+" | ".join(f"Col {c}" for c in range(cols))+" |\n")
                f.write("|"+"---|"*cols+"\n")
                for r in range(random.randint(4,12)):
                    f.write("| "+" | ".join(str(random.randint(1,99999)) if c else random.choice(W) for c in range(cols))+" |\n")
                f.write("\n")
            if pics and p < pics[1] and random.random() < 0.5:
                f.write(f"![Figure {p+1}: {sent(5)}]({pics[0]}/pic{p:03d}.jpg){{width=10cm}}\n\n"); p += 1
            f.write("- "+sent(6)+"\n- "+sent(7)+"\n- "+sent(5)+"\n\n")
        while pics and p < pics[1] and words >= words_target:
            f.write(f"![Figure {p+1}]({pics[0]}/pic{p:03d}.jpg){{width=10cm}}\n\n"+para()+"\n\n"); p += 1
