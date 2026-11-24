#!/usr/bin/env python3
# Usage: gen_pics.py DIR N [W H]   photo-like JPEGs (smooth noise) 
import sys, os, random
from PIL import Image, ImageFilter, ImageDraw
d, n = sys.argv[1], int(sys.argv[2]); w = int(sys.argv[3]) if len(sys.argv)>3 else 1200; h = int(sys.argv[4]) if len(sys.argv)>4 else 800
os.makedirs(d, exist_ok=True); random.seed(3)
for i in range(n):
    im = Image.effect_noise((w//4,h//4), 80).convert("RGB").resize((w,h), Image.BICUBIC)
    r,g,b = im.split(); g = g.filter(ImageFilter.GaussianBlur(3)); b = b.rotate(180)
    im = Image.merge("RGB",(r,g,b)); dr = ImageDraw.Draw(im)
    for _ in range(12):
        x,y = random.randint(0,w), random.randint(0,h); dr.ellipse((x,y,x+random.randint(40,300),y+random.randint(40,300)), fill=tuple(random.randint(0,255) for _ in range(3)))
    im.save(f"{d}/pic{i:03d}.jpg", quality=85)
