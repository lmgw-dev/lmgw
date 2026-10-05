#!/usr/bin/env python3
# Usage: scripts/chat-voice-seed.py BASE TOKEN DB MODEL
"""Seed a thread with spoken turns for scripts/drive/chat-voice-render.json
(chat-voice WP7): a dictated user turn, a reply spoken in voice mode that was
cut (its unheard rest, delivery cues, a timing), and a typed reply with a
bracket in its text.

Spoken turns are written by a thread-bound realtime session, which needs
real speech; this writes the `voice` column straight into a SCRATCH database
instead: DB must be a dev instance's or a dev copy's lmgw.sqlite (it refuses
the installed app's), and BASE a dev gateway on 127.0.0.1:8899 or :8898 (it
refuses any other, the installed app's 8001 above all: the sends would land
there while the writes went to the scratch file). The thread and its messages
are made through the API (two sends to MODEL), the thread is titled "voice
render seed", and its id is printed.
"""
import json
import os
import sqlite3
import sys
import urllib.parse
import urllib.request
from pathlib import Path

# The dev ports: scripts/dev-copy.sh's 8899, and 8898 (scripts/chat-drives.sh).
DEV_GATEWAYS = {("127.0.0.1", 8899), ("127.0.0.1", 8898)}

SPOKEN_REPLY = "[laughing] Na klar, das mache ich gern. Zuerst kommt der Teig."
UNHEARD = " Dann [sighs] heizt der Ofen auf zweihundert Grad vor. Dazu [a] Teig und [b] Form."
TYPED_REPLY = "Echo: typed [laughing] stays text"


def call(base, token, method, path, body=None):
    req = urllib.request.Request(
        base + path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        raw = r.read().decode()
    return raw if path.endswith("/send") else json.loads(raw)


def main() -> int:
    if len(sys.argv) != 5:
        print(__doc__, file=sys.stderr)
        return 2
    base, token, db, model = sys.argv[1:]
    url = urllib.parse.urlsplit(base)
    if (url.scheme != "http" or (url.hostname, url.port) not in DEV_GATEWAYS
            or url.path not in ("", "/")):
        print(f"refusing: {base} is not a dev gateway (http://127.0.0.1:8899 or :8898)",
              file=sys.stderr)
        return 1
    base = base.rstrip("/")
    prod = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "lmgw"
    if Path(db).resolve().parent in (prod.resolve(), (Path.home() / ".local/share/lmgw").resolve()):
        print(f"refusing: {db} is the installed app's database", file=sys.stderr)
        return 1
    t = call(base, token, "POST", "/chat/api/threads", {"model_alias": model, "kind": "chat"})
    tid = t["id"]
    call(base, token, "POST", f"/chat/api/threads/{tid}/send",
         {"content": "Wie backe ich einen Kuchen?"})
    call(base, token, "POST", f"/chat/api/threads/{tid}/send",
         {"content": "typed [laughing] stays text"})
    msgs = call(base, token, "GET", f"/chat/api/threads/{tid}")["messages"]
    users = [m["id"] for m in msgs if m["role"] == "user"]
    replies = [m["id"] for m in msgs if m["role"] == "assistant"]
    if len(users) != 2 or len(replies) != 2:
        print(f"expected two turns, got {[(m['role'], m['id']) for m in msgs]}", file=sys.stderr)
        return 1
    dictated = {"via": "dictation", "asr": "audio/parakeet-tdt-q8-0",
                "asr_answered_by": None, "asr_ms": 702, "audio_ms": 15823}
    spoken = {
        "via": "realtime", "tts": "audio/supertonic-3-q8-0", "voice": "M5",
        "unheard": UNHEARD,
        "timing": {"asr_ms": 31, "first_token_ms": 208, "first_clause_ms": 95,
                   "first_audio_ms": 36, "total_ms": 4120, "to_first_audio_ms": 782,
                   "end_of_turn_ms": 412, "cold": ["tts"],
                   "models": {"asr": {"alias": "audio/parakeet-tdt-q8-0", "answered_by": None},
                              "chat": {"alias": model, "answered_by": None},
                              "tts": {"alias": "audio/supertonic-3-q8-0", "answered_by": None,
                                      "voice": "M5"}}},
    }
    with sqlite3.connect(db, timeout=30) as c:
        c.execute("UPDATE chat_threads SET title = 'voice render seed' WHERE id = ?", (tid,))
        c.execute("UPDATE chat_messages SET voice = ? WHERE id = ?",
                  (json.dumps(dictated), users[0]))
        c.execute("UPDATE chat_messages SET content = ?, voice = ? WHERE id = ?",
                  (SPOKEN_REPLY, json.dumps(spoken), replies[0]))
        c.execute("UPDATE chat_messages SET content = ? WHERE id = ?", (TYPED_REPLY, replies[1]))
    print(tid)
    return 0


if __name__ == "__main__":
    sys.exit(main())
