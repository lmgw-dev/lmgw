#!/usr/bin/env python3
"""Offline test of scripts/window-frame.py, the frame the README screenshots get: its shadow is the
shell's (the active dark list linux.rs's `shadows()` returns, read from the source and compared
whole), its radius is the shell's and the page's at the default interface scale and the one
scripts/drive/window-frame.js hands the page, GTK's extents for that shadow, and `frame` on a
made-up shot: the canvas size and the content's place, and the shots it refuses (another size,
opaque corners, corners rounded at another scale). A change to the shell's frame fails here.

The checks that compose an image (`frame` and the shots it refuses) need numpy and Pillow and run
when both import; without them one line says they were skipped and why, and the rest still runs.

Usage: python3 scripts/window_frame_test.py   (ci/check.sh runs it)
"""

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

try:
    import numpy as np
    from PIL import Image
except ImportError as e:
    np = Image = None
    NO_IMAGE_LIBS = f"{e}; install python3-numpy and python3-pillow"
    print(f"window_frame_test: SKIPPED the image tests (FrameTest): {NO_IMAGE_LIBS}")
else:
    NO_IMAGE_LIBS = None

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("window_frame", os.path.join(HERE, "window-frame.py"))
M = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(M)


def shot(radius, size=M.WINDOW):
    """A made-up framed-window shot: opaque grey, its corners rounded by radius, clear outside."""
    w, h = size
    a = M.rounded_box((h, w), 0, 0, w, h, radius)
    rgb = np.full((h, w, 3), 0.2)
    return np.dstack([rgb, a])


def save(arr, path):
    Image.fromarray(np.round(arr * 255).astype(np.uint8), "RGBA").save(path)


class ShellTest(unittest.TestCase):
    def test_the_shadow_is_the_shells_active_dark_list(self):
        self.assertEqual(M.shell_shadow(M.LINUX_RS.read_text()), M.SHADOW)

    def test_the_shells_frame_has_not_drifted(self):
        self.assertEqual(M.shell_drift(), [])

    def test_the_parser_joins_continuation_lines_and_sees_an_added_layer(self):
        src = ('fn shadows(dark: bool) -> (&\'static str, &\'static str) {\n'
               '    if dark {\n'
               '        (\n'
               '            "0 0 0 1px rgba(0,0,0,0.75), \\\n'
               '             0 2px 4px rgba(0,0,0,0.25)",\n'
               '            "none",\n'
               '        )\n'
               '    } else {\n'
               '        ("0 0 0 1px rgba(0,0,0,0.14)", "none")\n'
               '    }\n'
               '}\n')
        self.assertEqual(M.shell_shadow(src),
                         "0 0 0 1px rgba(0,0,0,0.75), 0 2px 4px rgba(0,0,0,0.25)")
        # The old list still in the file elsewhere does not count: only the dark branch's first.
        self.assertNotEqual(M.shell_shadow(src + f'// "{M.SHADOW}"\n'), M.SHADOW)
        self.assertIsNone(M.shell_shadow("fn other() {}"))

    def test_extents_are_gtks_for_the_shadow(self):
        # top, right, bottom, left: the 85 px sides, 67 above and 103 below the README images have.
        self.assertEqual(M.extents(M.layers(M.SHADOW)), [67, 85, 103, 85])

    def test_the_radius_is_the_pages_at_the_default_scale(self):
        self.assertEqual(M.rust_const(M.WINDOW_CHROME_RS.read_text(), "DEFAULT_RADIUS"), M.RADIUS)
        page = M.rust_const(M.FRAME_RS.read_text(), "RADIUS")
        scale = M.rust_const(M.UI_SCALE_RS.read_text(), "DEFAULT")
        self.assertEqual(page * scale / 100, M.RADIUS)
        js = M.FRAME_JS.read_text()
        self.assertEqual(M.js_number(js, "radius"), M.RADIUS)
        self.assertEqual(M.js_number(js, "zoom"), scale / 100)


@unittest.skipIf(NO_IMAGE_LIBS, f"numpy or Pillow missing: {NO_IMAGE_LIBS}")
class FrameTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_window_shot_gets_the_canvas_and_place_of_the_readme_images(self):
        src, dst = self.dir / "in.png", self.dir / "out.png"
        save(shot(M.RADIUS), src)
        M.frame(src, dst)
        out = np.asarray(Image.open(dst))
        self.assertEqual(out.shape, (1250, 2090, 4))
        top, right, bottom, left = M.extents(M.layers(M.SHADOW))
        self.assertEqual((left, top), (85, 67))
        # The content is opaque where the shot is, the canvas' corner fully clear, the hairline
        # around the content's edge dark and nearly opaque.
        self.assertEqual(out[67 + 540, 85 + 960, 3], 255)
        self.assertEqual(out[0, 0, 3], 0)
        self.assertGreater(out[67 + 540, 84, 3], 180)

    def test_shots_that_are_not_the_framed_window_are_refused(self):
        cases = {
            "another size": shot(M.RADIUS, (1536, 864)),
            "opaque corners": np.dstack([np.full((1080, 1920, 3), 0.2), np.ones((1080, 1920))]),
            "another scale": shot(M.RADIUS / 1.25),
        }
        for what, arr in cases.items():
            with self.subTest(what):
                self.assertNotEqual(M.shot_problems(arr), [])
                src = self.dir / "bad.png"
                save(arr, src)
                with self.assertRaises(SystemExit):
                    M.frame(src, self.dir / "never.png")
                self.assertFalse((self.dir / "never.png").exists())
        self.assertEqual(M.shot_problems(shot(M.RADIUS)), [])


if __name__ == "__main__":
    unittest.main(verbosity=1)
