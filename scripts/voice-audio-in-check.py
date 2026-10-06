#!/usr/bin/env python3
# Usage: scripts/voice-audio-in-check.py BASE TOKEN_FILE MODEL OUT_JSON [--runs N] [--long-runs N]
#                                        [--corpus DIR]
"""The live check of voice turns the model hears (voice-audio-input design §7),
on a DEV COPY (scripts/dev-copy.sh), never the installed app: BASE must be a
127.0.0.1 port other than 8001.

Each run is a fresh thread on MODEL (temperature 0, a short-answer prompt,
language de) bound to a voice session with server VAD, one TTS-generated
German clip streamed into it in real time (100 ms appends) followed by
silence, until the response's `lmgw.response.timing`. The same clips run
under `audio_input: on` and then `off` (the thread's override), after one
warm-up run each. Per path it reports commit → first token and commit →
first audio as the client saw them, and the server's own `first_token_ms`,
`asr_ms`, `transcript_wait_ms` and prompt tokens: median, min and max, for
2–5 s turns and an unbroken ~30 s turn (the corpus's local_only clips joined
with 200 ms gaps). Three more sessions per path then say whether an
earlier turn replays as text (a second turn's prompt against the same clip's
single run), whether an utterance the VAD commits in two parts (a 300 ms
silence window) gets one reply, and whether noise and silence committed by
push-to-talk end quietly (no row, no speech, no error). Every stored user row is compared with Parakeet's transcript.

Only TTS-generated speech (the probe's corpus, piper voices on synthetic
text) and synthetic noise and silence: never a recording of a person. The
reply audio is counted, never played or kept. The threads are deleted at the
end.

It calls no cloud model (design §7). Since 2026-10-06 a configured fallback
hears a turn wherever it runs, so the check refuses to start unless MODEL is
a local chat row with no fallback at all: its hold_fallback_mode `none`, or
`inherit` with no global hold fallback. Nothing can then hand a turn on —
not the GPU hold, a benchmark run or a busy card, whenever they come. As a
backstop it still checks the verdict at start and stops at the first
response a fallback answered.
"""
import argparse
import asyncio
import base64
import json
import statistics
import time
import urllib.error
import urllib.parse
import urllib.request
import wave
from pathlib import Path

import websockets

SYSTEM = "Du bist ein hilfreicher Assistent. Antworte auf Deutsch in einem kurzen Satz."
SHORT = [
    "de_complete_short",
    "local_only/de00_de_DE-thorsten-medium__complete",
    "local_only/de05_de_DE-karlsson-low__complete",
    "local_only/de07_de_DE-ramona-low__complete",
    "local_only/de14_de_DE-mls-medium_s100__complete",
    "local_only/de04_de_DE-kerstin-low__complete",
]
CHUNK = 4800  # 100 ms of 24 kHz PCM16


class Gateway:
    def __init__(self, base, token):
        self.base, self.token = base.rstrip("/"), token

    def call(self, method, path, body=None):
        req = urllib.request.Request(
            self.base + path, method=method,
            headers={"Authorization": f"Bearer {self.token}", "content-type": "application/json"},
            data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(req, timeout=600) as r:
            raw = r.read()
        return json.loads(raw) if raw else None

    def thread(self, model, audio_input):
        t = self.call("POST", "/chat/api/threads", {"model_alias": model, "kind": "chat"})
        self.call("POST", f"/chat/api/threads/{t['id']}/settings", {
            "temperature": 0, "system_prompt": SYSTEM,
            "voice": {"language": "de", "audio_input": audio_input}})
        return t["id"]

    def rows(self, tid):
        v = self.call("GET", f"/chat/api/threads/{tid}")
        return v.get("messages") or []

    def verdict(self, tid):
        v = self.call("GET", f"/chat/api/threads/{tid}")
        return ((v.get("thread") or {}).get("voice_resolved") or {}).get("audio_input") or {}


def no_fallback(gw, model):
    """Refuse to start unless MODEL has no fallback configured at all (module
    doc): the check calls no cloud model, and a configured fallback is always
    used."""
    q = urllib.parse.urlencode({"model_id": model, "target": "chat"})
    try:
        row = gw.call("GET", f"/api/local-model?{q}")
    except urllib.error.HTTPError as e:
        raise SystemExit(f"refusing: {model} is no local chat row of this dev copy ({e}); "
                         "the live check runs on one with no fallback")
    mode = row.get("hold_fallback_mode")
    glob = ((gw.call("GET", "/api/settings-full") or {}).get("hold") or {}).get("fallback_alias")
    named = {"alias": row.get("hold_fallback"), "inherit": glob}.get(mode)
    if mode not in ("none", "inherit", "alias") or (named or "").strip():
        raise SystemExit(
            f"refusing: {model} has a fallback ({mode}: {named!r}); a configured fallback is "
            "always used, and the live check calls no other model. Set its hold fallback to "
            "none on the dev copy first.")
    tid = gw.thread(model, "on")
    try:
        v = gw.verdict(tid)
    finally:
        gw.call("POST", f"/chat/api/threads/{tid}/delete")
    if v.get("model") != model or v.get("lead"):
        raise SystemExit(
            f"refusing: a turn to {model} would go to {v.get('model')!r} "
            f"({v.get('lead') or v.get('why') or 'a fallback'}); the live check calls no "
            "other model. Release the GPU hold or wait for the benchmark run.")


def answered_by(turn):
    """The model a fallback answered with in `turn`'s chat frames, if any."""
    for e in turn["events"]:
        if e.get("type") == "lmgw.chat.frame" and e.get("event") == "done":
            who = (e.get("data") or {}).get("answered_by")
            if who:
                return who
    return None


def first_party(turns):
    """Stop the check at the first response a fallback answered (module doc)."""
    for t in turns:
        who = answered_by(t)
        if who:
            raise SystemExit(f"stopped: {t['clip']} was answered by the fallback {who}, not "
                             "the model under test; the live check calls no other model")
    return turns


def pcm(corpus, name):
    w = wave.open(str(Path(corpus) / f"{name}.wav"))
    assert (w.getframerate(), w.getnchannels(), w.getsampwidth()) == (24000, 1, 2), name
    return w.readframes(w.getnframes())


def long_turn(corpus):
    """~30 s of speech as one turn: the local_only clips joined by 200 ms gaps."""
    gap = b"\x00" * (CHUNK * 2)
    out = b""
    for f in sorted(Path(corpus, "local_only").glob("de*__complete.wav")):
        out += pcm(corpus, f"local_only/{f.stem}") + gap
        if len(out) >= 30 * 24000 * 2:
            break
    return out


async def session(gw, tid, clips, silence_ms):
    """Stream `clips` (name, pcm) into a session bound to `tid`, one turn each;
    the events each turn brought, stamped with when they arrived. Server VAD
    at `silence_ms`; `None`: push-to-talk, each clip committed by the client
    and answered by its `response.create`."""
    url = gw.base.replace("http", "ws", 1) + f"/v1/realtime?chat_thread={tid}"
    turns = []
    async with websockets.connect(
            url, additional_headers={"Authorization": f"Bearer {gw.token}"}, max_size=None) as ws:
        async def nxt(timeout):
            ev = json.loads(await asyncio.wait_for(ws.recv(), timeout))
            if ev.get("type") == "response.output_audio.delta":
                ev = {"type": ev["type"], "bytes": len(base64.b64decode(ev["delta"]))}
            ev["_t"] = time.monotonic()
            return ev

        while (await nxt(120))["type"] != "session.created":
            pass
        detection = None if silence_ms is None else {
            "type": "server_vad", "silence_duration_ms": silence_ms}
        await ws.send(json.dumps({"type": "session.update", "session": {
            "type": "realtime", "output_modalities": ["audio"],
            "audio": {"input": {"turn_detection": detection}}}}))
        while (await nxt(60))["type"] != "session.updated":
            pass
        for name, audio in clips:
            audio = audio + (b"\x00" * (CHUNK * 15) if silence_ms is not None else b"")
            events = []

            async def stream():
                for i in range(0, len(audio), CHUNK):
                    await ws.send(json.dumps({
                        "type": "input_audio_buffer.append",
                        "audio": base64.b64encode(audio[i:i + CHUNK]).decode()}))
                    await asyncio.sleep(0.1)
                if silence_ms is None:
                    await ws.send(json.dumps({"type": "input_audio_buffer.commit"}))
                    await ws.send(json.dumps({"type": "response.create"}))

            streamer = asyncio.create_task(stream())
            started = time.monotonic()
            # Until the response's timing — or, with no response running
            # (noise, silence, a veto), until the stream is over and nothing
            # came for 6 s. A turn stuck for 10 min keeps what came, and says so.
            running, quiet_since = False, None
            while True:
                try:
                    ev = await nxt(0.5)
                except asyncio.TimeoutError:
                    now = time.monotonic()
                    if streamer.done() and not running:
                        quiet_since = quiet_since or now
                        if now - quiet_since >= 6:
                            break
                    if now - started > 600:
                        print(f"  {name}: no end after 600 s; keeping what came", flush=True)
                        break
                    continue
                quiet_since = None
                events.append(ev)
                if ev["type"] == "response.created":
                    running = True
                elif ev["type"] == "response.done" and ev["response"].get("status") != "completed":
                    running = False
                elif ev["type"] == "lmgw.response.timing":
                    break
            await streamer
            try:
                while True:
                    events.append(await nxt(2))
            except asyncio.TimeoutError:
                pass
            turns.append({"clip": name, "seconds": len(audio) / 48000, "started": started,
                          "events": events})
    return turns


def measure(turn):
    ev = turn["events"]
    commits = [e["_t"] for e in ev if e["type"] == "input_audio_buffer.committed"]
    first = lambda pred: next((e["_t"] for e in ev if pred(e)), None)  # noqa: E731
    delta = first(lambda e: e["type"] == "lmgw.chat.frame" and e.get("event") == "delta")
    audio = first(lambda e: e["type"] == "response.output_audio.delta")
    timing = next((e for e in ev if e["type"] == "lmgw.response.timing"), {})
    usage = [e["data"] for e in ev if e["type"] == "lmgw.chat.frame" and e.get("event") == "usage"]
    done = [e["response"] for e in ev if e["type"] == "response.done"]
    commit = commits[-1] if commits else None
    ms = lambda t: None if t is None or commit is None else round((t - commit) * 1000)  # noqa: E731
    return {
        "clip": turn["clip"], "commits": len(commits),
        "commit_to_first_token_ms": ms(delta), "commit_to_first_audio_ms": ms(audio),
        "first_token_ms": timing.get("first_token_ms"), "asr_ms": timing.get("asr_ms"),
        "transcript_wait_ms": timing.get("transcript_wait_ms"),
        "to_first_audio_ms": timing.get("to_first_audio_ms"),
        "input": timing.get("input"), "input_why": timing.get("input_why"),
        "prompt_tokens": usage[-1].get("prompt_tokens") if usage else None,
        "completion_tokens": usage[-1].get("completion_tokens") if usage else None,
        "transcript": " ".join(
            e.get("transcript", "") for e in ev
            if e["type"] == "conversation.item.input_audio_transcription.completed"),
        "reply": "".join(e["data"].get("text", "") for e in ev
                         if e["type"] == "lmgw.chat.frame" and e.get("event") == "delta"),
        "done": [(d.get("status"), (d.get("status_details") or {}).get("reason")) for d in done],
        "audio_bytes": sum(e.get("bytes", 0) for e in ev
                           if e["type"] == "response.output_audio.delta"),
        "errors": [e.get("error") for e in ev if e["type"] == "error"],
        "users": [e.get("content") for e in ev if e["type"] == "lmgw.chat.user"],
        "inputs": [(e.get("input"), e.get("why")) for e in ev if e["type"] == "lmgw.chat.input"],
    }


def stats(values):
    v = [x for x in values if x is not None]
    if not v:
        return None
    return {"median": statistics.median(v), "min": min(v), "max": max(v), "n": len(v)}


async def runs(gw, model, path, clips, silence_ms, tids):
    out = []
    for name, audio in clips:
        tid = gw.thread(model, path)
        tids.append(tid)
        turn = first_party(await session(gw, tid, [(name, audio)], silence_ms))[0]
        m = measure(turn)
        rows = gw.rows(tid)
        m["rows"] = [(r["role"], r["content"], (r.get("voice") or {}).get("input"),
                      (r.get("voice") or {}).get("asr")) for r in rows]
        out.append(m)
        print(f"  {path:10} {name[-40:]:40} commit→token {m['commit_to_first_token_ms']} ms, "
              f"→audio {m['commit_to_first_audio_ms']} ms, wait {m['transcript_wait_ms']}, "
              f"input {m['input']}, prompt {m['prompt_tokens']}", flush=True)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base")
    ap.add_argument("token_file")
    ap.add_argument("model")
    ap.add_argument("out")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--long-runs", type=int, default=5)
    ap.add_argument("--corpus", default="target/realtime-spike/corpus")
    ap.add_argument("--checks-only", action="store_true",
                    help="skip the timed runs: only the replay, pause and noise sessions")
    a = ap.parse_args()
    url = urllib.parse.urlsplit(a.base)
    if url.hostname != "127.0.0.1" or url.port in (None, 8001):
        raise SystemExit(f"refusing: {a.base} is not a dev copy on 127.0.0.1 (8001 is the app's)")
    gw = Gateway(a.base, Path(a.token_file).read_text().strip())
    no_fallback(gw, a.model)
    corpus = a.corpus
    short = [(n, pcm(corpus, n)) for n in SHORT[: a.runs]]
    long = [(f"long_{i}", long_turn(corpus)) for i in range(a.long_runs)]
    print(f"long turn: {len(long[0][1]) / 48000:.1f} s", flush=True)
    tids = []
    result = {"model": a.model, "paths": {}}

    async def checked(path, clips, silence_ms):
        tid = gw.thread(a.model, path)
        tids.append(tid)
        turns = [measure(t) for t in first_party(await session(gw, tid, clips, silence_ms))]
        rows = [(r["role"], r["content"], (r.get("voice") or {}).get("input"))
                for r in gw.rows(tid)]
        return {"turns": turns, "rows": rows}

    async def go():
        for path in ("on", "off"):
            print(f"{path}: warm-up", flush=True)
            await runs(gw, a.model, path, short[:1], 700, tids)
            r = {}
            if not a.checks_only:
                r["short"] = await runs(gw, a.model, path, short, 700, tids)
                r["long"] = await runs(gw, a.model, path, long, 900, tids)
            print(f"{path}: replay, pause and noise sessions", flush=True)
            # Two turns: the second's prompt says how the first replays.
            r["replay"] = await checked(path, [
                (SHORT[0], pcm(corpus, SHORT[0])), (SHORT[1], pcm(corpus, SHORT[1]))], 700)
            # A short silence window: the pause in each clip commits it twice.
            r["pause"] = await checked(path, [
                ("de_midsentence_pause", pcm(corpus, "de_midsentence_pause")),
                ("de_two_sentences_pause", pcm(corpus, "de_two_sentences_pause"))], 300)
            # Push-to-talk, so the VAD cannot keep them out: noise and silence
            # committed as turns and asked for a reply.
            r["noise"] = await checked(path, [
                ("noise_only", pcm(corpus, "noise_only")),
                ("silence_2s", pcm(corpus, "silence_2s"))], None)
            result["paths"][path] = r

    asyncio.run(go())
    summary = {}
    for path, r in result["paths"].items():
        s = {}
        for kind in ("short", "long"):
            if kind in r:
                s[kind] = {k: stats([m[k] for m in r[kind]]) for k in (
                    "commit_to_first_token_ms", "commit_to_first_audio_ms", "first_token_ms",
                    "asr_ms", "transcript_wait_ms", "to_first_audio_ms", "prompt_tokens")}
        summary[path] = s
    # Prompt tokens per second of audio: the same clip, heard against read.
    per_s = []
    heard, off = result["paths"]["on"], result["paths"]["off"]
    for lm, om in zip(heard.get("short", []) + heard.get("long", []),
                      off.get("short", []) + off.get("long", [])):
        if lm["prompt_tokens"] and om["prompt_tokens"]:
            secs = next(len(au) / 48000 for n, au in short + long if n == lm["clip"])
            per_s.append(round((lm["prompt_tokens"] - om["prompt_tokens"]) / secs, 1))
    summary["extra_prompt_tokens_per_audio_second"] = per_s
    # Replay: the second turn's prompt against the same clip's single run is
    # the first turn's history (its row and its reply) — heard and read alike
    # when the first turn replays as text.
    replay = {}
    for path, r in result["paths"].items():
        t2 = r["replay"]["turns"][1]
        single = next((m for m in r.get("short", []) if m["clip"] == t2["clip"]), None)
        if single and t2["prompt_tokens"] and single["prompt_tokens"]:
            replay[path] = t2["prompt_tokens"] - single["prompt_tokens"]
    summary["second_turn_history_tokens"] = replay
    result["summary"] = summary
    Path(a.out).write_text(json.dumps(result, indent=1, ensure_ascii=False))
    print(json.dumps(summary, indent=1))
    for tid in tids:
        try:
            gw.call("POST", f"/chat/api/threads/{tid}/delete")
        except Exception as e:  # noqa: BLE001
            print("could not delete thread", tid, e)


if __name__ == "__main__":
    main()
