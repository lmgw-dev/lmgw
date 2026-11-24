#!/usr/bin/env python3
"""Usage: python3 -I scripts/convert-same.py OUT.json FILE... [--url URL] [--token TOKEN] [--against BEFORE.json]

Converts each FILE with lmgw's /v1/documents/convert in every output (text, markdown, json) and
every picture mode (placeholder, embedded, referenced), one file per request, and writes a
SHA-256 of each answer document (its `stats.time_ms` left out) to OUT.json. With --against, it
compares OUT with an earlier run's file and exits 1 naming every (file, output, pictures) whose
answer differs: a change that should only make conversion faster must keep every answer
byte-identical (document-conversion design §14, performance round).

URL defaults to $LMGW_URL or http://127.0.0.1:8888, TOKEN to $LMGW_TOKEN (the owner bearer a dev
instance prints). Nothing about the files leaves this machine; OUT holds names and hashes only.
"""
import hashlib
import http.client
import json
import os
import sys
import urllib.parse
import uuid

OUTPUTS = ["text", "markdown", "json"]
PICTURES = ["placeholder", "embedded", "referenced"]


def convert(url, token, path, fields):
    boundary = uuid.uuid4().hex
    parts = []
    for k, v in fields:
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'
                     .encode())
    name = os.path.basename(path)
    parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; '
                 f'filename="{name}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode())
    with open(path, "rb") as f:
        parts.append(f.read())
    parts.append(f"\r\n--{boundary}--\r\n".encode())
    u = urllib.parse.urlparse(url)
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=3600)
    c.request("POST", "/v1/documents/convert", body=b"".join(parts), headers={
        "Content-Type": f"multipart/form-data; boundary={boundary}",
        "Authorization": "Bearer " + token})
    r = c.getresponse()
    body = r.read()
    if r.status != 200:
        return f"http {r.status}"
    doc = json.loads(body)["documents"][0]
    doc.get("stats", {}).pop("time_ms", None)
    return hashlib.sha256(json.dumps(doc, sort_keys=True).encode()).hexdigest()


def main(argv):
    url = os.environ.get("LMGW_URL", "http://127.0.0.1:8888")
    token = os.environ.get("LMGW_TOKEN", "")
    against = None
    rest = []
    it = iter(argv)
    for a in it:
        if a == "--url":
            url = next(it)
        elif a == "--token":
            token = next(it)
        elif a == "--against":
            against = next(it)
        else:
            rest.append(a)
    if len(rest) < 2:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    out, files = rest[0], rest[1:]
    got = {}
    for path in files:
        for output in OUTPUTS:
            for pictures in PICTURES:
                key = f"{os.path.basename(path)} {output} {pictures}"
                got[key] = convert(url, token, path, [("output", output), ("pictures", pictures)])
    with open(out, "w") as f:
        json.dump(got, f, indent=1, sort_keys=True)
    if against is None:
        print(f"{len(got)} answers written to {out}")
        return 0
    with open(against) as f:
        before = json.load(f)
    differ = sorted(k for k in set(before) | set(got) if before.get(k) != got.get(k))
    for k in differ:
        print(f"differs: {k}")
    print(f"{len(got) - len(differ)} of {len(got)} answers identical")
    return 1 if differ else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
