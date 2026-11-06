#!/usr/bin/env python3
"""Records the headers of a split GGUF set and its single file for the split-footprint tests.

Usage: scripts/split-set-record.py <repo> <out.zip> <file>...

Reads each file's header (its metadata and tensor descriptors, up to the first
byte of tensor data) from Hugging Face by HTTP Range, without a token, and
writes them into <out.zip> beside an index.json naming each file's real size.
A header keeps every key and every tensor descriptor as the hub serves it, but
for the strings of `tokenizer.ggml.tokens` and `tokenizer.ggml.merges`: they
are written empty (their count stays), and the bytes they held go into one
zero-filled `lmgw.fixture.padding` array at the end of the metadata, so the
header ends at the same byte and every tensor's offset and size read back
exactly. A test writes each header to a file of the real size (sparse) and
plans it from the disk (crates/lmgw-core/tests/it/split_footprint.rs).
"""
import io
import json
import os
import struct
import sys
import urllib.request
import zipfile

PADDING_KEY = "lmgw.fixture.padding"
EMPTIED = {"tokenizer.ggml.tokens", "tokenizer.ggml.merges"}
SCALAR = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}


class Short(Exception):
    pass


def rewrite(buf):
    """The header in `buf` rewritten (module doc), or Short when `buf` ends before its last descriptor."""
    p = [0]

    def take(n):
        if p[0] + n > len(buf):
            raise Short()
        b = buf[p[0]:p[0] + n]
        p[0] += n
        return b

    u32 = lambda: struct.unpack("<I", take(4))[0]
    u64 = lambda: struct.unpack("<Q", take(8))[0]

    def skip(t):
        if t == 8:
            take(u64())
        elif t == 9:
            et, n = u32(), u64()
            if et == 8:
                for _ in range(n):
                    take(u64())
            else:
                take(SCALAR[et] * n)
        else:
            take(SCALAR[t])

    if take(4) != b"GGUF":
        sys.exit("not a GGUF file")
    version, n_tensors, n_kv = u32(), u64(), u64()
    kvs = []
    removed = 0
    for _ in range(n_kv):
        start = p[0]
        key = take(u64()).decode()
        t = u32()
        if key in EMPTIED and t == 9:
            et, n = u32(), u64()
            if et != 8:
                sys.exit(f"{key} is not an array of strings")
            for _ in range(n):
                removed += len(take(u64()))
            kvs.append(struct.pack("<Q", len(key)) + key.encode() + struct.pack("<IIQ", 9, 8, n) + b"\0" * (8 * n))
        else:
            skip(t)
            kvs.append(buf[start:p[0]])
    tensors_start = p[0]
    for _ in range(n_tensors):
        take(u64())
        nd = u32()
        take(8 * nd)
        u32()
        u64()
    tensors = buf[tensors_start:p[0]]
    if removed == 0:
        # A later part of a split set: no tokenizer, nothing to rewrite.
        return buf[:p[0]]
    fixed = 8 + len(PADDING_KEY) + 4 + 4 + 8
    if removed < fixed:
        sys.exit("too little removed to pad")
    pad = struct.pack("<Q", len(PADDING_KEY)) + PADDING_KEY.encode() + struct.pack("<IIQ", 9, 0, removed - fixed)
    pad += b"\0" * (removed - fixed)
    out = b"GGUF" + struct.pack("<IQQ", version, n_tensors, n_kv + 1) + b"".join(kvs) + pad + tensors
    if len(out) != p[0]:
        sys.exit(f"rewritten header is {len(out)} bytes, the hub's {p[0]}")
    return out


def header(repo, path):
    url = f"https://huggingface.co/{repo}/resolve/main/{path}"
    buf, chunk = b"", 1 << 20
    while True:
        req = urllib.request.Request(
            url, headers={"Range": f"bytes={len(buf)}-{len(buf) + chunk - 1}", "User-Agent": "lmgw-split-set-record"}
        )
        with urllib.request.urlopen(req) as r:
            total = int(r.headers["Content-Range"].split("/")[-1])
            buf += r.read()
        try:
            return rewrite(buf), total
        except Short:
            chunk *= 2


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    for k in ("HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"):
        os.environ.pop(k, None)
    repo, out, files = sys.argv[1], sys.argv[2], sys.argv[3:]
    index = {"repo": repo, "about": __doc__.splitlines()[0], "files": {}}
    blob = io.BytesIO()
    with zipfile.ZipFile(blob, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for f in files:
            data, total = header(repo, f)
            index["files"][f] = total
            z.writestr(zipfile.ZipInfo(f, (1980, 1, 1, 0, 0, 0)), data, zipfile.ZIP_DEFLATED)
            print(f, total, len(data))
        z.writestr(zipfile.ZipInfo("index.json", (1980, 1, 1, 0, 0, 0)), json.dumps(index, indent=1), zipfile.ZIP_DEFLATED)
    with open(out, "wb") as w:
        w.write(blob.getvalue())
    print(out, len(blob.getvalue()), "bytes")


if __name__ == "__main__":
    main()
