#!/usr/bin/env python3
# Usage: scripts/window-frame.py frame IN.png OUT.png | scripts/window-frame.py measure PNG...
"""Put a window shot into the app window's own frame, as a desktop screenshot of the window shows it.

`frame` takes a viewport shot of the dashboard in its framed-window state (html.csd: the page has
rounded its own corners, so the shot's corners are transparent) and draws around it what the shell's
frame draws around the webview (src-tauri/src/window_chrome/linux.rs): the hairline outline and the
drop shadow of a focused window, on a fully transparent canvas as large as the window's surface.
Spectacle's "active window" capture of the app on KDE is that surface: the content, and the shadow's
extents around it (85 px at the sides, 67 above, 103 below at the default 125 % shadow).

The shadow is GTK 3's, computed the way GTK draws it (gtkcssshadowvalue.c, gtkcairoblur.c):
- each layer of the `box-shadow` list is the rounded box (the content's, its radius grown by the
  spread), moved by its offset and blurred with GTK's box blur: three box passes per axis of
  size int(blur x 3 sqrt(2 pi) / 4) (for an even size two of that size, one to each side, and one
  one larger, centred, as the SVG spec has it), then filled with the layer's colour;
- every layer is clipped to outside the rounded box (an outset shadow never draws under the
  content), and the surface reaches the largest layer's extent per side (the same formula as the
  linux.rs test `extents`).
The layers and the radius are the shell's, for the dark theme: SHADOW below must equal the active
dark list that linux.rs's `shadows()` returns (read from its source, continuation lines joined),
RADIUS the shell's DEFAULT_RADIUS (window_chrome.rs), which is the page's own radius (frame.rs
RADIUS) at the default interface scale (ui_scale.rs DEFAULT), and scripts/drive/window-frame.js
must hand the page that radius and zoom. `frame` refuses to run when any of them differs (change
them together); scripts/window_frame_test.py checks the same in the gate. The shot must be the
window at that scale: WINDOW pixels, its corners rounded by RADIUS.

`measure` prints what a capture of a framed window shows, for comparing ours with a real one:
the opaque content's box, the margin per side, the corner radius (where the content's top row and
left column turn fully opaque), and the alpha along the lines through the middle of each edge and
along the top-left corner's diagonal, outwards from the content.
"""
import math
import re
import sys
from pathlib import Path

# numpy and Pillow draw and read images only: the shell's frame constants, the shadow parsing and
# the extents need neither, so scripts/window_frame_test.py runs those without them.
try:
    import numpy as np
    from PIL import Image
except ImportError as e:
    np = Image = None
    IMAGE_LIBS_MISSING = f"{e} (the image commands need numpy and Pillow: dnf install python3-numpy python3-pillow)"
else:
    IMAGE_LIBS_MISSING = None

ROOT = Path(__file__).resolve().parent.parent
LINUX_RS = ROOT / "src-tauri/src/window_chrome/linux.rs"
WINDOW_CHROME_RS = ROOT / "src-tauri/src/window_chrome.rs"
FRAME_RS = ROOT / "crates/lmgw-ui/src/frame.rs"
UI_SCALE_RS = ROOT / "crates/lmgw-ui/src/ui_scale.rs"
FRAME_JS = ROOT / "scripts/drive/window-frame.js"

# linux.rs `shadows(true).0`, the focused window's shadow in the dark theme, and DEFAULT_RADIUS
# (window_chrome.rs, the page's 14 px at its 125 %): in window pixels, which a shot at device scale
# 1.25 of the 125 % page has as its pixels.
SHADOW = "0 0 0 1px rgba(0,0,0,0.75), 0 18px 30px rgba(0,0,0,0.55), 0 4px 7px rgba(0,0,0,0.35)"
RADIUS = 17.5
# The window the shot is of: scripts/readme-shots.sh's 1536x864 CSS px at device scale 1.25.
WINDOW = (1920, 1080)
SCALE = 3.0 * (2.0 * math.pi) ** 0.5 / 4.0


def rust_str(lit):
    """A Rust string literal's text: a backslash at a line's end drops the line break and the next
    line's leading whitespace; the escapes these files use (\\, \") are undone."""
    lit = re.sub(r"\\\n\s*", "", lit)
    return re.sub(r"\\(.)", r"\1", lit)


def shell_shadow(text):
    """The active (focused) dark decoration's box-shadow list in linux.rs: the first string of the
    tuple `shadows()` returns in its `if dark` branch."""
    m = re.search(r"fn shadows\(dark: bool\)[^{]*\{\s*if dark \{\s*\(\s*\"((?:[^\"\\]|\\.)*)\"", text, re.S)
    if not m:
        return None
    return rust_str(m.group(1))


def rust_const(text, name):
    """A numeric `const NAME: … = value;` of a Rust source, or None."""
    m = re.search(rf"\bconst {name}: \w+ = ([0-9.]+);", text)
    return float(m.group(1)) if m else None


def js_number(text, key):
    """The number after `key:` in scripts/drive/window-frame.js's state object, or None."""
    m = re.search(rf"\b{key}: ([0-9.]+),", text)
    return float(m.group(1)) if m else None


def shell_drift():
    """What differs between this script's SHADOW and RADIUS and the shell's: one line per
    difference, empty when they agree."""
    out = []
    shadow = shell_shadow(LINUX_RS.read_text())
    if shadow is None:
        out.append(f"{LINUX_RS}: no `fn shadows(dark: bool)` with an `if dark` tuple of strings")
    elif shadow != SHADOW:
        out.append(f"{LINUX_RS}: the active dark shadow is {shadow!r}, this script has {SHADOW!r}")
    default_radius = rust_const(WINDOW_CHROME_RS.read_text(), "DEFAULT_RADIUS")
    page_radius = rust_const(FRAME_RS.read_text(), "RADIUS")
    scale = rust_const(UI_SCALE_RS.read_text(), "DEFAULT")
    js = FRAME_JS.read_text()
    js_radius, js_zoom = js_number(js, "radius"), js_number(js, "zoom")
    for what, value in [(f"{WINDOW_CHROME_RS} DEFAULT_RADIUS", default_radius),
                        (f"{FRAME_RS} RADIUS x {UI_SCALE_RS} DEFAULT / 100",
                         page_radius * scale / 100 if page_radius and scale else None),
                        (f"{FRAME_JS} radius", js_radius)]:
        if value != RADIUS:
            out.append(f"{what} is {value}, this script's RADIUS is {RADIUS}")
    if scale is None or js_zoom != scale / 100:
        out.append(f"{FRAME_JS} zoom is {js_zoom}, {UI_SCALE_RS} DEFAULT is {scale} %")
    return out


def corner_reach(alpha_row):
    """How far along a shot's top row the corner stays at most half covered (the top-left
    corner's arc where it meets the top edge)."""
    return int(np.argmax(alpha_row > 0.5))


def shot_problems(shot):
    """Why a shot (RGBA, 0..1) is not the framed window at WINDOW and RADIUS; empty when it is."""
    h, w = shot.shape[:2]
    if (w, h) != WINDOW:
        return [f"is {w}x{h}, not the {WINDOW[0]}x{WINDOW[1]} window (1536x864 CSS px at device scale 1.25)"]
    corners = shot[[0, 0, -1, -1], [0, -1, 0, -1], 3]
    if corners.max() > 0.5:
        return ["has opaque corners: the page was not in the framed window's state (html.csd), or the "
                "shot was not taken on a transparent background"]
    want = corner_reach(rounded_box((1, 64), 0, 0, 512, 512, RADIUS)[0])
    got = corner_reach(shot[0, :64, 3])
    if abs(got - want) > 1:
        return [f"has its top-left corner rounded over {got} px where a {RADIUS} px radius covers "
                f"{want}: the page was shot at another scale or with another radius"]
    return []


def layers(css):
    """`x y blur spread alpha` per layer of a box-shadow list of black rgba() colours."""
    out = []
    for part in re.split(r",\s*(?![^()]*\))", css):
        m = re.fullmatch(r"\s*(\S+)\s+(\S+)(?:\s+(\S+))?(?:\s+(\S+))?\s+rgba\(0,0,0,([\d.]+)\)\s*", part)
        if not m:
            raise SystemExit(f"window-frame: cannot read shadow layer {part!r}")
        px = [float(v.removesuffix("px")) if v else 0.0 for v in m.groups()[:4]]
        out.append((*px, float(m.group(5))))
    return out


def extents(ls):
    """Top, right, bottom, left: how far GTK 3 grows the surface for the shadow."""
    acc = [0, 0, 0, 0]
    for x, y, blur, spread, _ in ls:
        reach = math.floor(blur * SCALE * 1.5 + 0.5) + spread
        side = [reach - y, reach + x, reach + y, reach - x]
        acc = [max(a, int(math.ceil(max(s, 0)))) for a, s in zip(acc, side)]
    return acc


def rounded_box(shape, x0, y0, w, h, r):
    """Coverage of a rounded rectangle on a pixel grid (antialiased by its signed distance)."""
    H, W = shape
    ys = np.arange(H, dtype=np.float64)[:, None] + 0.5
    xs = np.arange(W, dtype=np.float64)[None, :] + 0.5
    cx, cy = x0 + w / 2.0, y0 + h / 2.0
    qx = np.abs(xs - cx) - (w / 2.0 - r)
    qy = np.abs(ys - cy) - (h / 2.0 - r)
    outside = np.hypot(np.maximum(qx, 0), np.maximum(qy, 0))
    inside = np.minimum(np.maximum(qx, qy), 0)
    sdf = outside + inside - r
    return np.clip(0.5 - sdf, 0.0, 1.0)


def box_pass(a, size, left, axis):
    """One box blur pass: out[i] = mean(a[i - left .. i - left + size - 1]), zeros outside."""
    a = np.moveaxis(a, axis, -1)
    n = a.shape[-1]
    c = np.concatenate([np.zeros(a.shape[:-1] + (1,)), np.cumsum(a, axis=-1)], axis=-1)
    i = np.arange(n)
    lo = np.clip(i - left, 0, n)
    hi = np.clip(i - left + size, 0, n)
    out = (c[..., hi] - c[..., lo]) / size
    return np.moveaxis(out, -1, axis)


def gtk_blur(a, radius):
    d = int(SCALE * radius)
    if d <= 1:
        return a
    for axis in (1, 0):
        if d % 2:
            for _ in range(3):
                a = box_pass(a, d, d // 2, axis)
        else:
            a = box_pass(a, d, d // 2, axis)
            a = box_pass(a, d, d // 2 - 1, axis)
            a = box_pass(a, d + 1, d // 2, axis)
    return a


def frame(src, dst):
    drift = shell_drift()
    if drift:
        raise SystemExit("window-frame: the shell's frame changed, update SHADOW / RADIUS here "
                         "(and scripts/drive/window-frame.js) to it:\n  " + "\n  ".join(drift))
    shot = np.asarray(Image.open(src).convert("RGBA"), dtype=np.float64) / 255.0
    bad = shot_problems(shot)
    if bad:
        raise SystemExit(f"window-frame: {src} " + "; ".join(bad))
    h, w = shot.shape[:2]
    ls = layers(SHADOW)
    top, right, bottom, left = extents(ls)
    H, W = h + top + bottom, w + left + right
    # The blur reaches past the surface on the sides with less extent (the top): work on a padded
    # grid and crop, as GTK clips its larger shadow surface to the window's.
    pad = max(extents(ls)) * 2
    grid = (H + 2 * pad, W + 2 * pad)
    bx, by = left + pad, top + pad
    clear = 1.0
    for x, y, blur, spread, alpha in ls:
        r = RADIUS + spread if RADIUS > 0 else 0.0
        a = rounded_box(grid, bx + x - spread, by + y - spread, w + 2 * spread, h + 2 * spread, r)
        a = gtk_blur(a, blur)
        clear = clear * (1.0 - alpha * a)
    shadow = (1.0 - clear) * (1.0 - rounded_box(grid, bx, by, w, h, RADIUS))
    shadow = shadow[pad:pad + H, pad:pad + W]

    ca = np.zeros((H, W))
    crgb = np.zeros((H, W, 3))
    ca[top:top + h, left:left + w] = shot[..., 3]
    crgb[top:top + h, left:left + w] = shot[..., :3]
    out_a = ca + shadow * (1.0 - ca)
    with np.errstate(invalid="ignore", divide="ignore"):
        out_rgb = np.where(out_a[..., None] > 0, crgb * ca[..., None] / out_a[..., None], 0.0)
    out = np.dstack([out_rgb, out_a])
    Image.fromarray(np.round(out * 255.0).astype(np.uint8), "RGBA").save(dst)
    print(f"{dst}: {W}x{H}, content {w}x{h} at {left},{top}")


def measure(path):
    a = np.asarray(Image.open(path).convert("RGBA"))[..., 3].astype(int)
    H, W = a.shape
    ys, xs = np.where(a == 255)
    x0, x1, y0, y1 = xs.min(), xs.max(), ys.min(), ys.max()
    my, mx = (y0 + y1) // 2, (x0 + x1) // 2
    # Radius: how far along the top row and down the left column the content stays short of opaque.
    rx = int(np.argmax(a[y0, x0:] == 255))
    ry = int(np.argmax(a[y0:, x0] == 255))
    nz_y, nz_x = np.where(a > 0)
    print(f"{path}: {W}x{H}")
    print(f"  content (alpha 255) x {x0}..{x1}, y {y0}..{y1}: {x1 - x0 + 1}x{y1 - y0 + 1}")
    print(f"  margin left {x0}, right {W - 1 - x1}, top {y0}, bottom {H - 1 - y1}")
    print(f"  shadow reach (alpha > 0) left {x0 - nz_x.min()}, right {nz_x.max() - x1}, "
          f"top {y0 - nz_y.min()}, bottom {nz_y.max() - y1}")
    print(f"  corner: top row opaque from +{rx} px, left column from +{ry} px")
    steps = [1, 2, 4, 8, 16, 24, 32, 48, 64, 80]

    def line(name, vals):
        print(f"  {name:<8}" + " ".join(f"{v:3d}" for v in vals))

    print("  out (px) " + " ".join(f"{s:3d}" for s in steps) + "   (alpha 0..255; +1 is the hairline)")
    line("left", [a[my, x0 - s] if x0 - s >= 0 else -1 for s in steps])
    line("right", [a[my, x1 + s] if x1 + s < W else -1 for s in steps])
    line("top", [a[y0 - s, mx] if y0 - s >= 0 else -1 for s in steps])
    line("bottom", [a[y1 + s, mx] if y1 + s < H else -1 for s in steps])
    # Along the diagonal out of the top-left corner's arc (radius from the measured corner).
    r = (rx + ry) / 2.0
    cx, cy = x0 + r, y0 + r
    k = 1 / np.sqrt(2)
    diag = []
    for s in steps:
        px, py = int(round(cx - (r + s) * k)), int(round(cy - (r + s) * k))
        diag.append(a[py, px] if px >= 0 and py >= 0 else -1)
    line("corner", diag)


def main():
    if IMAGE_LIBS_MISSING and len(sys.argv) > 1 and sys.argv[1] in ("frame", "measure"):
        sys.exit(f"window-frame: {IMAGE_LIBS_MISSING}")
    if len(sys.argv) == 4 and sys.argv[1] == "frame":
        frame(sys.argv[2], sys.argv[3])
    elif len(sys.argv) >= 3 and sys.argv[1] == "measure":
        for p in sys.argv[2:]:
            measure(p)
    else:
        sys.exit("usage: scripts/window-frame.py frame IN.png OUT.png | scripts/window-frame.py measure PNG...")


if __name__ == "__main__":
    main()
