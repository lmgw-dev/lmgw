#!/usr/bin/env python3
# Usage: scripts/drive/make-voice-fixtures.py BASE TOKEN_FILE OUT.js [TTS_ALIAS] [VOICE]
"""TTS-generated German utterances for the voice-mode drives (chat-voice WP9).

Each clip is a text-to-speech model's render of a synthetic sentence, asked
of the gateway at BASE (`/v1/audio/speech`, default alias
audio/supertonic-3-q8-0, voice F2) — never a recording of a person. OUT.js
holds them as `window.lmgwVoiceFixtures = {name: base64 WAV}` for a
{"script": OUT.js} step, which scripts/drive/fake-voice.js says into the
page's fake microphone. Keep OUT.js under target/: it is generated, not a
source file.
"""
import base64
import json
import sys
import urllib.request

CLIPS = {
    # a question answered in a couple of sentences (the state walk)
    "q_short": "Welche Tiere halten einen Winterschlaf?",
    # one answered at length (talked over for the barge-in)
    "q_long": "Erzähl mir bitte in fünf ganzen Sätzen, warum der Himmel blau ist.",
    # the second utterance, said over the reply
    "q_barge": "Stopp, warte mal kurz. Was ist mit dem Abendrot?",
}


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    base, token_file, out = sys.argv[1], sys.argv[2], sys.argv[3]
    alias = sys.argv[4] if len(sys.argv) > 4 else "audio/supertonic-3-q8-0"
    voice = sys.argv[5] if len(sys.argv) > 5 else "F2"
    token = open(token_file).read().strip()
    clips = {}
    for name, text in CLIPS.items():
        req = urllib.request.Request(
            base.rstrip("/") + "/v1/audio/speech", method="POST",
            data=json.dumps({"model": alias, "input": text, "voice": voice,
                             "response_format": "wav"}).encode(),
            headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=600) as r:
            wav = r.read()
        if wav[:4] != b"RIFF":
            raise SystemExit(f"{name}: the answer is no WAV ({wav[:40]!r})")
        clips[name] = base64.b64encode(wav).decode()
        print(f"{name}: {len(wav)} bytes — {text}")
    with open(out, "w") as f:
        f.write("// TTS renders of synthetic sentences (scripts/drive/make-voice-fixtures.py).\n")
        f.write("window.lmgwVoiceFixtures = " + json.dumps(clips) + ";\n'voice fixtures loaded';\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
