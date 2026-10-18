#!/usr/bin/env python3
"""The host side of the WebRTC interop rig (scripts/webrtc-interop.sh, realtime
calls design §13.2).

Usage: webrtc_interop.py setup BASE WORKDIR MOCK_URL OWNER_TOKEN_FILE
       webrtc_interop.py check WORKDIR DEV_LOG

setup configures a fresh dev instance through its own API, with the owner bearer
from OWNER_TOKEN_FILE: the mock upstream (scripts/mock-openai.py) as one
upstream with a chat, a speech-to-text and a text-to-speech alias, the realtime
cascade and the Chat's voice on them, two owner keys (WORKDIR/key for the pages, WORKDIR/holder.key for the bound
page's holder; 0600, the script removes them at the end) and an ongoing Chat folder for the bound
page. It writes WORKDIR/setup.json.

check reads the pages' WORKDIR/result.json and the dev instance's log (DEV_LOG,
at DEBUG) and judges what only the log knows: the playout
allowance from the bound page's reports against the formula for the same stretch
and against the page's own jitterBufferDelay; the client's capture-to-arrival
offset (by its sender reports) against realtime.webrtc_commit_window_ms; frames
that missed a push-to-talk cut; the RTP that arrived before oai-events opened;
and the heard a stop's playout position cut against what the page's tap played.
It prints every check and the measurements, writes WORKDIR/report.json, and
exits 0 when every check passed, 2 when one failed. A check tagged with a
question of the calls design (`known`, e.g. Q5) is a known finding: it passes
while the fact it recorded holds, and fails, saying so, the day it changes.
"""
import json
import os
import re
import statistics
import sys
import urllib.error
import urllib.request
import wave

# The bound page offers this max-message-size, so lmgw's larger events arrive
# as lmgw.event.part (calls design §6.3).
MAX_MESSAGE = 1200
# The page's level threshold for "audio plays" (RMS of a 10 ms block, 0 to 1).
ONSET_RMS = 0.01
# How far apart two measures of the same thing may be before a check fails.
PLAYOUT_TOLERANCE_MS = 40
HEARD_TOLERANCE_MS = 60


def request(base, path, token, body=None):
    req = urllib.request.Request(
        f"{base}{path}",
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        method="GET" if body is None else "POST",
    )
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")


def setup(base, work, mock, owner_file):
    owner = open(owner_file).read().strip()

    def op(name, body):
        status, v = request(base, f"/api/op/{name}", owner, body)
        if status != 200:
            sys.exit(f"setup: {name} answered {status}: {v}")
        return v

    op("upstream_set", {"action": "create", "name": "interop-mock", "protocol": "openai",
                        "kind": "generic", "base_url": f"{mock}/v1"})
    for alias, task, endpoint in [("interop-chat", None, None),
                                  ("interop-asr", "asr", "/v1/audio/transcriptions"),
                                  ("interop-tts", "tts", "/v1/audio/speech")]:
        body = {"action": "create", "alias": alias, "upstream": "interop-mock",
                "upstream_model": alias}
        if task:
            body["capabilities_override"] = {"capabilities": {
                "task": task, "endpoints": [endpoint], "source": "owner"}}
        op("model_set", body)
    op("settings_set_full", {
        "realtime": {"default_model": "interop-chat", "asr_alias": "interop-asr",
                     "tts_alias": "interop-tts"},
        "chat_tts_alias": "interop-tts", "chat_stt_alias": "interop-asr"})
    keys = {}
    # The pages' key, and the bound page's holder's: a session bound with the
    # call's own key does not hold the thread against it.
    for name, file in [("webrtc-interop", "key"), ("webrtc-interop-holder", "holder.key")]:
        keys[file] = op("key_create", {"name": name, "kind": "owner"})["key"]
        fd = os.open(os.path.join(work, file), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(keys[file])
    key = keys["key"]
    status, folder = request(base, "/chat/api/folders", key, {
        "name": "Interop", "defaults": {"model_alias": "interop-chat"},
        "ongoing": {"idle_minutes": 30}})
    if status != 200:
        sys.exit(f"setup: the folder answered {status}: {folder}")
    with open(os.path.join(work, "setup.json"), "w") as f:
        json.dump({"folder": folder["id"], "max_message": MAX_MESSAGE}, f)
    print(f"setup: the mock cascade, an owner key and ongoing folder {folder['id']}")


ANSI = re.compile(r"\x1b\[[0-9;]*m")


def lines_of(log, call):
    return [ln for ln in log if f"WebRTC call {call}" in ln]


def median(v):
    return statistics.median(v) if v else None


def lead_silence_ms(path):
    """How long `path` (PCM16 mono) is quieter than ONSET_RMS before its first
    loud 10 ms block: what a level tap misses of an item's first audio."""
    with wave.open(path) as w:
        rate, n = w.getframerate(), w.getnframes()
        raw = w.readframes(n)
    samples = [int.from_bytes(raw[i:i + 2], "little", signed=True) / 32768 for i in range(0, len(raw), 2)]
    block = rate // 100
    for b in range(0, len(samples) - block, block):
        chunk = samples[b:b + block]
        if (sum(x * x for x in chunk) / len(chunk)) ** 0.5 > ONSET_RMS:
            return b * 1000 / rate
    return None


def check(work, dev_log):
    result = json.load(open(os.path.join(work, "result.json")))
    log = [ANSI.sub("", ln) for ln in open(dev_log, errors="replace")]
    checks = list(result["checks"])
    measures = {}

    def add(name, ok, detail=None):
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    pages = result.get("pages", {})
    calls = {name: p.get("info", {}).get("call_id") for name, p in pages.items()}

    # What arrived before oai-events opened, per call (§4.6, §16).
    kept_re = re.compile(r"its session starts (\d+) ms after ICE and DTLS connected, (\d+) ms after "
                         r"oai-events opened; (?:(\d+) frames \((\d+) ms of audio\)|(.*bind_pending))")
    measures["before_open"] = {}
    for name, call in calls.items():
        if not call:
            continue
        m = next((kept_re.search(ln) for ln in lines_of(log, call) if kept_re.search(ln)), None)
        marks = pages[name].get("info", {}).get("marks", {})
        measures["before_open"][name] = {
            "page_connected_ms": marks.get("connected"), "page_open_ms": marks.get("open"),
            "page_rtp_packets_before_open": marks.get("rtp_before_open"),
            "lmgw_session_after_connected_ms": int(m.group(1)) if m else None,
            "lmgw_session_after_open_ms": int(m.group(2)) if m else None,
            "lmgw_kept_frames": int(m.group(3)) if m and m.group(3) else None,
            "lmgw_kept_audio_ms": int(m.group(4)) if m and m.group(4) else None,
            "lmgw_dropped_under_bind_pending": bool(m and m.group(5)),
        }
    add("lmgw says how much audio it kept before each call's open",
        all(v["lmgw_session_after_open_ms"] is not None for v in measures["before_open"].values())
        and measures["before_open"], measures["before_open"])

    # The client's capture to arrival, against the commit window (§4.4).
    cap_re = re.compile(r"arrived (-?\d+)/(-?\d+)/(-?\d+) ms \(least/median/most of (\d+)\).*"
                        r"webrtc_commit_window_ms is (\d+) ms")
    caps, window = [], None
    for call in filter(None, calls.values()):
        for ln in lines_of(log, call):
            m = cap_re.search(ln)
            if m:
                caps.append([int(m.group(i)) for i in range(1, 5)])
                window = int(m.group(5))
    if caps:
        measures["capture_to_arrival_ms"] = {
            "windows": len(caps), "least": min(c[0] for c in caps),
            "median_of_medians": median([c[1] for c in caps]), "most": max(c[2] for c in caps),
            "commit_window_ms": window}
    add("the client's capture-to-arrival offset fits inside realtime.webrtc_commit_window_ms",
        caps and max(c[2] for c in caps) < window, measures.get("capture_to_arrival_ms"))

    bound = calls.get("bound")
    info = pages.get("bound", {}).get("info", {})
    if bound:
        blines = lines_of(log, bound)
        # Frames that missed the push-to-talk cut (§4.4's DEBUG line).
        late = [ln.strip() for ln in blines if "s cut arrived" in ln and "commit" in ln]
        measures["frames_after_a_commit_cut"] = len(late)
        add("no frame missed the push-to-talk commit's cut", not late, late[:3])

        # The allowance from the reports, the formula beside it, NetEQ's own.
        rep_re = re.compile(r"playout allowance (\d+) ms from the phone's playout report \(delay (\d+) ms")
        form_re = re.compile(r"playout allowance (\d+) ms from the formula")
        beside_re = re.compile(r"the formula would give (\d+) ms")
        jb_re = re.compile(r"the phone's jitter buffer (\d+) ms")
        rtt_re = re.compile(r"round trip (\d+) ms")
        reported, formula_beside, formula_alone, lmgw_jb, rtts = [], [], [], [], []
        for ln in blines:
            if (m := rep_re.search(ln)):
                reported.append(int(m.group(1)))
                if (b := beside_re.search(ln)):
                    formula_beside.append(int(b.group(1)))
                if (j := jb_re.search(ln)):
                    lmgw_jb.append(int(j.group(1)))
            elif (m := form_re.search(ln)):
                formula_alone.append(int(m.group(1)))
            if (r := rtt_re.search(ln)):
                rtts.append(int(r.group(1)))
        page_jb = [r["jitter_buffer_ms"] for r in info.get("reports", []) if r.get("jitter_buffer_ms") is not None]
        positions = [r for r in info.get("reports", []) if r.get("playout_ntp_ms") is not None]
        page_target = [r["target_ms"] for r in info.get("reports", []) if r.get("target_ms") is not None]
        measures["playout"] = {
            "allowance_from_reports_ms": {"n": len(reported), "median": median(reported),
                                          "min": min(reported, default=None), "max": max(reported, default=None)},
            "formula_beside_reports_ms": {"n": len(formula_beside), "median": median(formula_beside)},
            "formula_before_reports_ms": {"n": len(formula_alone), "median": median(formula_alone)},
            "page_jitter_buffer_delay_ms": {"n": len(page_jb), "median": median(page_jb),
                                            "min": min(page_jb, default=None), "max": max(page_jb, default=None)},
            "page_jitter_buffer_target_ms": {"n": len(page_target), "median": median(page_target)},
            "round_trip_ms": {"median": median(rtts)},
            "reports_sent": len(info.get("reports", [])),
        }
        measures["playout"]["reports_with_a_position"] = len(positions)
        # Without a position (estimatedPlayoutTimestamp absent, Q5: the
        # page's known-finding check) the reports cannot set the allowance,
        # and the comparison below is the formula's alone.
        if positions:
            if reported and page_jb:
                gap = median(reported) - median(page_jb)
                measures["playout"]["reports_minus_jitter_buffer_ms"] = gap
                add(f"the allowance from the playout reports agrees with jitterBufferDelay (within "
                    f"{PLAYOUT_TOLERANCE_MS} ms)", abs(gap) <= PLAYOUT_TOLERANCE_MS, measures["playout"])
            else:
                add("the playout reports set the allowance (a DEBUG line with source report)", False,
                    measures["playout"])
        formula = formula_beside or formula_alone
        if formula and page_jb:
            measures["playout"]["formula_minus_jitter_buffer_ms"] = median(formula) - median(page_jb)

        # A stop's heard against the tap (§5.4).
        stop = info.get("stop")
        if stop and stop.get("response_id"):
            rid = stop["response_id"]
            cut_re = re.compile(rf"realtime response {re.escape(rid)}: cut; heard is (.*?): (\d+) of (\d+) samples")
            m = next((cut_re.search(ln) for ln in log if cut_re.search(ln)), None)
            lead = lead_silence_ms(os.path.join(work, "answer.wav"))
            played = None
            if stop.get("onset_ms") is not None and lead is not None:
                played = stop["tap_ms"] - stop["onset_ms"] + lead
            heard = int(m.group(2)) / 24 if m else None
            measures["stop"] = {"heard_ms": heard, "sent_ms": int(m.group(3)) / 24 if m else None,
                                "how": m.group(1) if m else None, "tap_played_ms": played,
                                "answer_lead_silence_ms": lead, **stop}
            if stop.get("playout_ntp_ms") is not None:
                add("the stop's playout position cut heard at what the tap played (within "
                    f"{HEARD_TOLERANCE_MS} ms)",
                    m and played is not None and "playout position" in m.group(1)
                    and abs(heard - played) <= HEARD_TOLERANCE_MS, measures["stop"])
            elif heard is not None and played is not None:
                # No position to cut at: heard is what was sent (§5.4), and
                # how far that lies from the tap is the measurement.
                measures["stop"]["sent_minus_tap_played_ms"] = heard - played

    # Late ticks that skipped frames (§5.5), for the record.
    skip_re = re.compile(r"a tick (\d+) ms late skipped (\d+) frames due longer ago than the playout "
                         r"allowance \((\d+) ms\)")
    skips = [(int(m.group(1)), int(m.group(2)), int(m.group(3)))
             for call in filter(None, calls.values()) for ln in lines_of(log, call)
             if (m := skip_re.search(ln))]
    measures["late_ticks_that_skipped"] = {"ticks": len(skips), "frames": sum(s[1] for s in skips),
                                           "allowances_ms": sorted({s[2] for s in skips})}

    for p in pages.values():
        p.pop("events", None)
    report = {"checks": checks, "measures": measures, "pages": pages, "info": result.get("info")}
    with open(os.path.join(work, "report.json"), "w") as f:
        json.dump(report, f, indent=1)
    for c in checks:
        known = c.get("known")
        tag = f"[{known}, known finding] " if known else ""
        # A known finding says what it found either way.
        shown = c["detail"] is not None and (known or not c["ok"])
        print(f"{'PASS' if c['ok'] else 'FAIL'}  {tag}{c['name']}"
              + (f"\n      {json.dumps(c['detail'])[:600]}" if shown else ""))
    print("\nmeasurements:")
    print(json.dumps(measures, indent=1))
    failed = [c for c in checks if not c["ok"]]
    known = [c for c in checks if c.get("known")]
    print(f"\n{len(checks) - len(failed)} of {len(checks)} checks passed"
          + (f" (known findings checked: {', '.join(sorted({c['known'] for c in known}))})"
             if known else ""))
    return 0 if not failed else 2


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "setup":
        setup(*sys.argv[2:])
    elif len(sys.argv) == 4 and sys.argv[1] == "check":
        sys.exit(check(*sys.argv[2:]))
    else:
        sys.exit(__doc__.split("\n\n")[1])
