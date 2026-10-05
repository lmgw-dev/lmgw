"""scripts/shell-check.py's checks of chat-voice WP6, WP7 and WP9 in the real
shell: the Chat composer's devices popover with the first tone recorded off the
private graph's null sinks, a dictation round, and voice mode (see shell-check's
docstring for what each proves).
"""
import array
import math
import signal
import time
import wave

from private_session import stop, wait_for
from shell_check_lib import (REPO, STREAM_ID, WORK, base_env, linked_to, navigate, playback,
                             props, pw_dump, sinks, spawn, target_of, wp_state)


# The page's devices popover, driven from the inspector (window.__wp6).
WP6_JS = r"""(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const until = async (fn, ms) => {
    const end = performance.now() + ms;
    while (performance.now() < end) {
      try { const v = fn(); if (v) return v; } catch (_) {}
      await sleep(50);
    }
    return null;
  };
  const q = (s) => document.querySelector(s);
  const root = () => q('[data-voice-devices]');
  window.__wp6 = {
    async open() {
      for (let i = 0; i < 300 && !q('[data-voice-devices-btn]'); i++) {
        const b = [...document.querySelectorAll('button')]
          .find((x) => (x.title || '').startsWith('New temporary chat'));
        if (b && i % 10 === 0) b.click();
        await sleep(100);
      }
      const btn = q('[data-voice-devices-btn]');
      if (!btn) return { error: 'no devices button' };
      if (!root()) btn.click();
      await until(() => root() && root().dataset.outputKind
        && root().querySelectorAll('[data-vd=output] .vd-opt').length > 1, 10000);
      return this.state();
    },
    state() {
      const r = root();
      const btn = q('[data-voice-devices-btn]');
      if (!r) return { error: 'no popover' };
      return { kind: r.dataset.outputKind, mic: r.dataset.mic, tone: r.dataset.tone,
        pushed: +r.dataset.tonePushed, played: +r.dataset.tonePlayed, player: r.dataset.player,
        route: r.dataset.route, latency: +r.dataset.latency,
        outputs: [...r.querySelectorAll('[data-vd=output] .vd-opt')].map((b) => b.textContent.trim()),
        notes: [...r.querySelectorAll('.vd-note')].map((n) => n.textContent),
        alert: btn ? (btn.dataset.voiceAlert || '') : null,
        stored: localStorage.getItem('lmgw.voice.output') };
    },
    pick(text) {
      const b = [...root().querySelectorAll('[data-vd=output] .vd-opt')]
        .find((x) => x.textContent.trim().startsWith(text));
      if (!b) return { error: 'no row ' + text };
      b.click();
      return { ok: true };
    },
    async mic() {
      q('[data-vd-mic=start]').click();
      await until(() => ['open', 'error'].includes(root().dataset.mic), 10000);
      return this.state();
    },
    async stopMic() {
      const b = q('[data-vd-mic=stop]');
      if (b) b.click();
      await sleep(300);
      return this.state();
    },
    async tone() {
      q('[data-vd-tone]').click();
      await sleep(150);
      await until(() => ['done', 'error'].includes(root().dataset.tone), 20000);
      return this.state();
    },
    async until(what, value, ms) {
      await until(() => root().dataset[what] === value, ms);
      return this.state();
    },
  };
  return JSON.stringify('ready');
})()"""

# The test tone's notes (lmgw-ui pages/chat_voice/audio/pcm.rs test_tone).
CHIME = (523.25, 659.25, 783.99)


def call_wp6(insp, expr, timeout=40):
    return insp.eval(f"(async () => JSON.stringify(await window.__wp6.{expr}))()", timeout=timeout)


def recorder(client, sink, path):
    """pw-record off a null sink's monitor, into a 24 kHz mono WAV."""
    env = base_env()
    env.update(client)
    return spawn(["pw-record", "--target", sink, "-P", "{ stream.capture.sink = true }",
                  "--rate", "24000", "--channels", "1", "--format", "s16", str(path)],
                 env, path.with_suffix(".log"))


def chime(path):
    """What a recording holds: its peak, and the runs of 10 ms frames in which
    one of the chime's notes dominates (above -40 dBFS), as
    [(note index, start s, length s)], runs shorter than 30 ms dropped."""
    with wave.open(str(path)) as w:
        rate = w.getframerate()
        a = array.array("h", w.readframes(w.getnframes()))
    peak = max((abs(x) for x in a), default=0)
    hop = rate // 100
    win = 2 * hop
    coeffs = [2 * math.cos(2 * math.pi * f / rate) for f in CHIME]
    frames = []
    for start in range(0, max(0, len(a) - win), hop):
        seg = a[start:start + win]
        if sum(x * x for x in seg) / win < (32768 * 0.01) ** 2:
            frames.append(None)
            continue
        powers = []
        for c in coeffs:
            s1 = s2 = 0.0
            for x in seg:
                s1, s2 = x + c * s1 - s2, s1
            powers.append(s1 * s1 + s2 * s2 - c * s1 * s2)
        frames.append(max(range(len(CHIME)), key=lambda i: powers[i]))
    runs = []
    for i, k in enumerate(frames):
        if k is None:
            continue
        if runs and runs[-1][0] == k and runs[-1][2] == i - 1:
            runs[-1][2] = i
        else:
            runs.append([k, i, i])
    out = [(k, round(s * hop / rate, 3), round((e - s + 1) * hop / rate, 3)) for k, s, e in runs]
    return peak, [r for r in out if r[2] >= 0.03]


def whole_chime(runs):
    """The three notes in order, each 0.36-0.44 s (0.4 s with 15 ms fades),
    back to back: an onset swallowed by a late route shortens the first."""
    return (len(runs) == 3 and [r[0] for r in runs] == [0, 1, 2]
            and all(0.36 <= r[2] <= 0.44 for r in runs)
            and all(abs(runs[i + 1][1] - (runs[i][1] + runs[i][2])) <= 0.03 for i in range(2)))


def route_lines(shell_log):
    return [line for line in shell_log.read_text(errors="replace").splitlines() if "audio output: " in line]


def popover_checks(insp, client, shell_pid, port, shell_log, res):
    """chat-voice WP6 in the app: the devices popover, the first tone on the
    chosen sink, the resume, the way back (see the module docs)."""
    gateway = f"http://127.0.0.1:{port}/"
    navigate(insp, gateway + "chat", gateway + "chat")
    insp.eval(WP6_JS)
    st = call_wp6(insp, "open()")
    sk = sinks(pw_dump(client))
    hp, sp = sk["check_headphones"]["id"], sk["check_speakers"]["id"]
    hp_serial = str(props(sk["check_headphones"]).get("object.serial"))
    res.check("the popover lists the shell's outputs, the default marked",
              st.get("kind") == "shell"
              and any(o.startswith("Check speakers (null)") and "system default" in o for o in st.get("outputs", []))
              and any(o.startswith("Check headphones (null)") for o in st.get("outputs", [])),
              {"kind": st.get("kind"), "outputs": st.get("outputs")})
    # Chosen while no playback context exists, with nothing remembered.
    call_wp6(insp, "pick('Check headphones')")
    st = call_wp6(insp, "state()")
    res.check("choosing an output stores its node name for the window",
              '"check_headphones"' in (st.get("stored") or ""), st.get("stored"))
    st = call_wp6(insp, "mic()")
    res.check("Test microphone opens the (mock) capture in the app", st.get("mic") == "open",
              {"mic": st.get("mic"), "notes": st.get("notes")})

    def tone_recorded(name):
        lines0 = len(route_lines(shell_log))
        recs = {k: (WORK / f"{name}-{k}.wav") for k in ("headphones", "speakers")}
        procs = [recorder(client, f"check_{k}", p) for k, p in recs.items()]
        time.sleep(1.0)
        st = call_wp6(insp, "tone()", timeout=60)
        time.sleep(0.6)
        for p in procs:
            stop(p, signal.SIGINT, 5)
        heard = {k: chime(p) for k, p in recs.items()}
        return st, heard, route_lines(shell_log)[lines0:]

    st, heard, lines = tone_recorded("first")
    res.check("the first tone after choosing an output plays there from its first note (review M1)",
              st.get("tone") == "done" and whole_chime(heard["headphones"][1]) and heard["speakers"][0] < 33,
              {"tone": st.get("tone"), "headphones (note, start s, length s)": heard["headphones"][1],
               "speakers peak": heard["speakers"][0], "played/pushed": [st.get("played"), st.get("pushed")]})
    res.check("the popover says where lmgw plays",
              st.get("route") == "ok" and "lmgw plays on Check headphones (null)" in st.get("notes", []),
              {"route": st.get("route"), "notes": st.get("notes")})
    res.check("the composer's devices button carries the echo warning (device mode, not the default)",
              "lmgw plays on Check headphones (null)" in (st.get("alert") or ""), st.get("alert"))
    d = pw_dump(client)
    mine = playback(d, shell_pid)
    res.check("every lmgw stream, the test microphone's capture context's too, is on the chosen sink",
              len(mine) >= 2 and all(target_of(d, o["id"]) == hp_serial and linked_to(d, o["id"]) == {hp}
                                     for o in mine),
              {"streams": [o["id"] for o in mine], "links": [sorted(linked_to(d, o["id"])) for o in mine]})
    res.check("one route application for the context made", len(lines) == 1, lines)
    st = call_wp6(insp, "stopMic()")

    st = call_wp6(insp, "until('player', 'suspended', 15000)", timeout=30)
    res.check("the playback context suspends after 10 s with nothing to play", st.get("player") == "suspended",
              st.get("player"))
    st, heard, lines = tone_recorded("resumed")
    res.check("the next tone resumes it with one route application (review M2) and arrives whole",
              st.get("tone") == "done" and st.get("played") == st.get("pushed") and len(lines) == 1
              and whole_chime(heard["headphones"][1]) and heard["speakers"][0] < 33,
              {"tone": st.get("tone"), "route applications": lines,
               "headphones": heard["headphones"][1], "speakers peak": heard["speakers"][0]})
    res.check("the output latency WebKitGTK reports (taken off for truncation)", True,
              f"{st.get('latency')} samples at 24 kHz")

    call_wp6(insp, "pick('System default')")

    def back():
        d = pw_dump(client)
        mine = playback(d, shell_pid)
        return mine and all(target_of(d, o["id"]) is None and linked_to(d, o["id"]) == {sp} for o in mine)
    try:
        wait_for("lmgw's streams back on the default", back, 8)
        back_ok = True
    except RuntimeError:
        back_ok = False
    key = f"Output/Audio:application.id:{STREAM_ID}"
    time.sleep(1.0)
    st = call_wp6(insp, "state()")
    d = pw_dump(client)
    res.check("System default sends every lmgw stream back and clears the memory",
              back_ok and "check_headphones" not in wp_state().get(key, "") and st.get("route") == "ok"
              and not st.get("alert"),
              {"streams": {o["id"]: {"target": target_of(d, o["id"]), "links": sorted(linked_to(d, o["id"])),
                                     "props target": props(o).get("target.object")}
                           for o in playback(d, shell_pid)},
               "speakers": sp, "headphones": hp, "remembered": wp_state().get(key), "route": st.get("route"),
               "notes": st.get("notes"), "alert": st.get("alert"), "route applications": route_lines(shell_log)[-2:]})


def dictation_checks(insp, port, res):
    """chat-voice WP7 in the app: a dictation round through the composer's
    microphone — the shell's grant, WebKit's mock capture at 16 kHz, the WAV
    upload, the text at the caret, Esc, Right Ctrl — against a mock ASR the
    page answers itself (scripts/media-probe.js, phase "dictation"), so the
    scratch gateway needs no speech model."""
    gateway = f"http://127.0.0.1:{port}/"
    # Away first: the window may be on the Chat already, and a navigation to
    # the same URL would let the old document answer for the new one.
    navigate(insp, gateway + "settings", gateway + "settings")
    navigate(insp, gateway + "chat", gateway + "chat")

    def chat_ready():
        try:
            return insp.eval("JSON.stringify(!!document.querySelector('.chat-shell'))", timeout=5)
        except RuntimeError:
            return False
    wait_for("the Chat page", chat_ready, 30, 0.5)
    insp.eval((REPO / "scripts/media-probe.js").read_text() + "\nJSON.stringify('ready')")
    out = insp.eval("(async () => await window.lmgwMediaProbe({phase: 'dictation'}))()", timeout=180)
    checks = (out or {}).get("checks") or []
    if not checks:
        res.check("dictation: the probe ran", False, out)
    for c in checks:
        res.check(f"dictation: {c['name']}", c["ok"], c.get("detail"))


def realtime_checks(insp, port, res):
    """chat-voice WP9 in the app: voice mode through the composer's voice
    button — the shell's grant, WebKit's mock capture at 24 kHz streamed as
    appends, the state walk, the captions and bubbles, a barge-in's truncate
    at what was heard, stop talking, push-to-talk, M, a takeover with
    Re-enter, Esc — against a bound session the page answers itself
    (scripts/realtime-mock.js + media-probe.js, phase "realtime"), so the
    scratch gateway needs no speech model."""
    gateway = f"http://127.0.0.1:{port}/"
    navigate(insp, gateway + "settings", gateway + "settings")
    navigate(insp, gateway + "chat", gateway + "chat")

    def chat_ready():
        try:
            return insp.eval("JSON.stringify(!!document.querySelector('.chat-shell'))", timeout=5)
        except RuntimeError:
            return False
    wait_for("the Chat page", chat_ready, 30, 0.5)
    # One script per message: WebKit's inspector server resets a connection
    # whose message is over 128 KiB, and the two together passed that in
    # the WP11 UI fixes (132 KB before JSON escaping).
    for script in ("scripts/realtime-mock.js", "scripts/media-probe.js"):
        insp.eval((REPO / script).read_text() + "\nJSON.stringify('ready')")
    out = insp.eval("(async () => await window.lmgwMediaProbe({phase: 'realtime'}))()", timeout=240)
    checks = (out or {}).get("checks") or []
    if not checks:
        res.check("realtime: the probe ran", False, out)
    for c in checks:
        res.check(f"realtime: {c['name']}", c["ok"], c.get("detail"))
