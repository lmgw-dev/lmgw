#!/usr/bin/env python3
# Usage: img2pdf.py OUT.pdf DPI IMG...   one image per page
import sys
from PIL import Image
ims = [Image.open(p) for p in sys.argv[3:]]
ims[0].save(sys.argv[1], save_all=True, append_images=ims[1:], resolution=float(sys.argv[2]))
