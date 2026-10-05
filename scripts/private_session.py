"""Child processes for the checks that run with nothing reaching the desktop.

Used by scripts/shell-check.py (through shell_check_lib.py) and scripts/webkit-check.py:

    private_session.unwind_on_signals()          # SIGTERM/SIGHUP unwind like Ctrl-C
    try:
        p = private_session.spawn([...], env, logfile)
        ...
    finally:
        private_session.stop_all()

Every child is the leader of a process group of its own and is killed when the script dies,
however it dies (PR_SET_PDEATHSIG, set in the child before exec). stop_all stops them newest
first, each on its own, so one stuck child does not keep the rest alive.
"""

import ctypes
import os
import signal
import socket
import subprocess
import time

started = []  # Popen objects, each the leader of its own process group

_libc = ctypes.CDLL(None, use_errno=True)
PR_SET_PDEATHSIG = 1


def die_with_parent():
    """In the child, before exec: SIGKILL when the script dies, however it dies."""
    _libc.prctl(PR_SET_PDEATHSIG, signal.SIGKILL)


def spawn(args, env, logfile, **kw):
    """Start args in a process group of its own, output to logfile, recorded for stop_all."""
    out = open(logfile, "wb")
    p = subprocess.Popen(args, env=env, stdout=out, stderr=subprocess.STDOUT,
                         stdin=subprocess.DEVNULL, start_new_session=True,
                         preexec_fn=die_with_parent, **kw)
    started.append(p)
    return p


def track(p):
    """Record a child started elsewhere (with start_new_session and die_with_parent)."""
    started.append(p)
    return p


def stop(p, sig=signal.SIGTERM, wait=10):
    """sig to p's process group, then SIGKILL after wait seconds."""
    if p is None or p.poll() is not None:
        return
    try:
        os.killpg(p.pid, sig)
    except ProcessLookupError:
        return
    try:
        p.wait(timeout=wait)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.wait(timeout=5)


def stop_all(log=print, wait=10):
    """Stop every recorded child: SIGTERM to each group, newest first, then one shared wait
    of `wait` seconds and SIGKILL for what is left. One that will not stop is reported, not
    raised, so the rest still go (and a harness's SIGTERM is answered in seconds, not in a
    sum of per-child waits)."""
    live = [p for p in reversed(started) if p.poll() is None]
    for p in live:
        try:
            os.killpg(p.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except Exception as e:  # noqa: BLE001
            log(f"stopping pid {p.pid}: {type(e).__name__}: {e}")
    end = time.time() + wait
    for p in live:
        try:
            p.wait(timeout=max(0.1, end - time.time()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(p.pid, signal.SIGKILL)
                p.wait(timeout=5)
            except ProcessLookupError:
                pass
            except Exception as e:  # one stuck child must not keep the rest alive
                log(f"killing pid {p.pid}: {type(e).__name__}: {e}")


def unwind_on_signals():
    """SIGTERM/SIGHUP (a harness timeout, a closed terminal) unwind through the caller's
    `finally` like Ctrl-C does."""
    def on_signal(signum, _frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGHUP, on_signal)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def port_free(port):
    """Nobody listens there (a TIME-WAIT left by an earlier run does not count)."""
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def wait_for(what, fn, timeout, step=0.2):
    end = time.time() + timeout
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(step)
    raise RuntimeError(f"timed out waiting for {what}")
