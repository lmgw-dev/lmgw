"""A headless Chrome's throwaway profile, and the /tmp directory Chrome makes beside it.

Used by scripts/ui-drive.py, scripts/ui-matrix.py and scripts/screenshot.py:
    before = chrome_profile.snapshot()   # just before Chrome starts
    ...
    chrome_profile.remove(profile, before)

Chrome keeps its singleton socket in a directory of its own, /tmp/com.google.Chrome.XXXXXX,
linked from the profile (SingletonSocket -> /tmp/com.google.Chrome.XXXXXX/SingletonSocket). A
Chrome that is terminated leaves that directory behind, and /tmp is RAM here. Only a directory
that appeared during this run and that this profile links to is removed: never another browser's.
"""

import glob
import os
import shutil
import time
from pathlib import Path

PATTERN = "/tmp/com.google.Chrome.*"


def snapshot() -> set:
    """The Chrome singleton directories in /tmp now."""
    return set(glob.glob(PATTERN))


def _linked(profile: str) -> set:
    """The /tmp singleton directories `profile` links to."""
    out = set()
    for name in ("SingletonSocket", "SingletonCookie", "SingletonLock"):
        try:
            target = Path(os.readlink(Path(profile) / name))
        except OSError:
            continue
        d = target.parent
        if d.parent == Path("/tmp") and d.name.startswith("com.google.Chrome."):
            out.add(str(d))
    return out


def remove(profile: str, before: set) -> None:
    """Remove `profile` and the singleton directory this run's Chrome made for it."""
    owned = _linked(profile) - before
    # Chrome's network service child flushes into the profile after the browser process exits,
    # so one rmtree can race it.
    for _ in range(5):
        shutil.rmtree(profile, ignore_errors=True)
        if not os.path.exists(profile):
            break
        time.sleep(0.2)
    for d in owned:
        shutil.rmtree(d, ignore_errors=True)
