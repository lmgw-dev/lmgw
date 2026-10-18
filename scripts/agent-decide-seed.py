#!/usr/bin/env python3
# Usage: scripts/agent-decide-seed.py BASE TOKEN DB
"""Seed a dev instance for scripts/drive/agent-decide.json (labs-store
design §4): the question set "drive triage" (a choice of the mail categories
on decide/laya, its 0.7 threshold not measured) made through the API, and a
finished mail-labeler apply job whose rows that set classified, written
straight into the jobs table: two classified mails (18f1 Finance, 18f2 moved
by the reviewer to Newsletter), one whose fetch failed and that the model
never saw (18f3), and one whose call failed with its fallback left (18f4).
Each asked row carries the set's stamp (id, name, updated_at), as a classify
through the set writes it, so "Save as labelled set" is offered and saves
into that set. A set of that name from an earlier seed is deleted first.

There is no way to make an apply job of the mail labeler on a dev copy (no
gws server), so the job goes into a SCRATCH database: DB must be a dev
instance's lmgw.sqlite (it refuses the installed app's), and BASE a dev
gateway on 127.0.0.1:8899 or :8898 (it refuses any other, the installed
app's 8001 above all: the set would land there while the job went to the
scratch file). Fake data only: .example senders. Prints the job id.
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
SET_NAME = "drive triage"
OPTIONS = ["Newsletter", "Work", "Finance"]


def call(base, token, method, path, body=None):
    req = urllib.request.Request(
        base + path, method=method,
        data=None if method == "GET" else json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read().decode())


def mail(i, sender, subject, body):
    return (
        f"From: {sender}\nTo: owner@home.example\nDate: Fri, 16 Oct 2026 0{i}:00:00 +0000\n"
        f"Subject: {subject}\n\n{body}"
    )


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    base, token, db = sys.argv[1:]
    url = urllib.parse.urlsplit(base)
    if (url.scheme != "http" or (url.hostname, url.port) not in DEV_GATEWAYS
            or url.path not in ("", "/")):
        print(f"refusing: {base} is not a dev gateway (http://127.0.0.1:8899 or :8898)",
              file=sys.stderr)
        return 1
    base = base.rstrip("/")
    # Production's dir under every name it has, as scripts/dev-copy.sh refuses it.
    home_prod = Path.home() / ".local/share/lmgw"
    xdg = os.environ.get("XDG_DATA_HOME")
    prods = {home_prod.resolve()}
    if xdg:
        prods.add((Path(xdg) / "lmgw").resolve())
    if os.environ.get("LMGW_PROD_DIR"):
        prods.add(Path(os.environ["LMGW_PROD_DIR"]).resolve())
    if Path(db).resolve().parent in prods:
        print(f"refusing: {db} is the installed app's database", file=sys.stderr)
        return 1
    # Opened and checked before the set is made on the gateway: a wrong path
    # leaves nothing behind (read-only open creates no sqlite file).
    try:
        with sqlite3.connect(f"{Path(db).resolve().as_uri()}?mode=ro", uri=True, timeout=30) as c:
            c.execute("SELECT 1 FROM jobs LIMIT 1")
    except sqlite3.Error as e:
        print(f"refusing: {db} is no dev instance's database ({e})", file=sys.stderr)
        return 1

    for p in call(base, token, "GET", "/labs/api/decision/presets?kind=question_set")["presets"]:
        if p["name"] == SET_NAME:
            call(base, token, "POST", f"/labs/api/decision/presets/{p['id']}/delete", None)
    question = {"id": "category", "instructions": "Which category is this mail?",
                "kind": {"type": "choice",
                         "options": [{"name": n, "description": ""} for n in OPTIONS]}}
    made = call(base, token, "POST", "/labs/api/decision/presets", {
        "name": SET_NAME, "kind": "question_set",
        "body": {"questions": [question], "default_model": "decide/laya",
                 "thresholds": {"category": {"value": 0.7, "measured": None}}}})
    stamp = {"id": made["id"], "name": made["name"], "updated_at": made["updated_at"]}

    rows = [
        {
            "id": "18f1",
            "columns": {"date": "Fri, 16 Oct 2026 01:00:00 +0000",
                        "from": "billing@shop.example", "subject": "Your invoice 7"},
            "output": {"category": "Finance"},
            "prompt": mail(1, "billing@shop.example", "Your invoice 7",
                           "Your invoice for October is attached. Amount due: 12.00."),
            "raw": "{}",
            "attention": False,
            "set": stamp,
        },
        {
            "id": "18f2",
            "columns": {"date": "Fri, 16 Oct 2026 02:00:00 +0000",
                        "from": "news@letters.example", "subject": "This week in gardening"},
            # The reviewer moved it from Work to Newsletter.
            "output": {"category": "Newsletter"},
            "prompt": mail(2, "news@letters.example", "This week in gardening",
                           "Ten tips for autumn beds. Unsubscribe at the bottom."),
            "raw": "{}",
            "attention": True,
            "set": stamp,
        },
        {
            "id": "18f3",
            "columns": {"date": "", "from": "", "subject": ""},
            "output": {"category": "Other"},
            "error": "the tool 'gws__gmail_get' failed: message not found",
            "attention": True,
        },
        {
            "id": "18f4",
            "columns": {"date": "Fri, 16 Oct 2026 04:00:00 +0000",
                        "from": "long@digest.example", "subject": "The whole archive"},
            "output": {"category": "Other"},
            "prompt": mail(4, "long@digest.example", "The whole archive", "Every issue since 2019."),
            "error": "decide/laya answered 500: {\"error\": {\"code\": 500, \"message\": \"input "
                     "(9120 tokens) is too large to process. increase the physical batch size\", "
                     "\"type\": \"server_error\"}}",
            "attention": True,
            "set": stamp,
        },
    ]
    inp = {"agent_id": "mail-labeler", "phase": "apply", "rows": rows}
    result = {"phase": "apply", "agent_id": "mail-labeler", "model": "decide/laya", "rows": rows,
              "attention": 3, "errors": 2, "usage": {}, "cost_micro": None,
              "model_calls": 0, "tool_calls": 4,
              "applied": {"output": {"applied": 4, "labels": ["lmgw/Finance"]}, "text": "",
                          "tool_calls": []}}
    with sqlite3.connect(db, timeout=30) as c:
        cur = c.execute(
            "INSERT INTO jobs (kind, key, label, status, input, progress, result, started_at, "
            "finished_at) VALUES ('agent_run', 'agent:mail-labeler', 'mail-labeler · apply', "
            "'done', ?, '{}', ?, datetime('now'), datetime('now'))",
            (json.dumps(inp), json.dumps(result)),
        )
        job = cur.lastrowid
    print(job)
    return 0


if __name__ == "__main__":
    sys.exit(main())
