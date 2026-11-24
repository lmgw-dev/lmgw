#!/usr/bin/env python3
# Usage: python3 -I scripts/convert-measure.py --corpus DIR [--lmgw URL [--lmgw-field NAME=VALUE]...] [--docling URL] [--cmd NAME=TEMPLATE]... [--page-cmd NAME=TEMPLATE]... [--granite URL --granite-docs LIST --granite-max-pages N] [--formats LIST] --out FILE
#        python3 -I scripts/convert-measure.py --table SUMMARY.json...   (a markdown table of summaries)
"""Measure document converters against docling's test corpus (design 2026-11-18 §10.1, WP0a).

The corpus is docling's `tests/data` as `scripts/convert-corpus-fetch.sh` checks it out. Every
file is sent to each system in turn, one at a time, so the timings of the systems are taken close
together on the same file. The summary is numbers only: per format folder and per system the
files converted and failed (by code), the time per file and per page, and the similarity of the
output to docling's `groundtruth/` markdown where the corpus has one.

Systems (each optional, any number):
  --lmgw URL       lmgw's public route POST {URL}/v1/documents/convert (markdown), key from
                   LMGW_KEY. lmgw's side needs nothing but the running gateway.
                   --lmgw-field NAME=VALUE adds an option field to every request (ocr=force).
  --docling URL    docling-serve POST {URL}/v1/convert/file (to_formats=md,
                   image_export_mode=placeholder, the deployment's defaults otherwise), key from
                   DOCLING_KEY (sent as X-Api-Key).
  --cmd NAME=TPL   a command run once per file, `{in}` replaced by the file's path; its stdout is
                   the markdown. Split like a shell line, never run through a shell.
  --page-cmd NAME=TPL
                   an OCR command run once per page image: a PDF is rendered with pdftoppm at
                   --dpi, an image file is passed as it is. Only the --ocr-formats folders. The
                   time includes the render.
  --granite URL    a llama-server with granite-docling: each page of --granite-docs is rendered
                   at --granite-dpi and sent to {URL}/v1/chat/completions with the model's
                   instruction; it reports whether generation stopped by itself, whether the
                   DocTags are well-formed, the time per page, and the text against docling's
                   JSON ground truth for the same page (with lmgw's tier 0 on that page when
                   --lmgw is given). --granite-max-pages bounds the pages sent; it is required.

Times: `seconds` is the client's wall clock per file (request and answer included);
`server_seconds` is the system's own figure where it reports one (lmgw `stats.time_ms`,
docling-serve `processing_time`). docling-serve's sync route waits on its own job queue, so its
wall clock has a floor of about its poll interval.

Similarity (normalised text: markdown markup, whitespace and case folded, NFKC): character 3-gram
Jaccard and a line-level difflib ratio. Headings, tables and table cells are counted in each
markdown. The ground truth is docling's own output, so it measures closeness to docling, not
truth. Reference order per file: `<fmt>/groundtruth/<name>.md`, `<stem>.md`, then the OCR
engines' `<engine>/<mode>/<stem>.<engine>.<mode>.md` (engines other than the measured ones
first), then a `<stem>.json` in another folder's groundtruth (its first N pages, N = the file's
pages), e.g. tiff/2206.01062.tif against pdf/groundtruth/2206.01062.json.
"""
import argparse
import base64
import difflib
import html
import json
import os
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import unicodedata
import urllib.error
import urllib.request
import uuid

OCR_ENGINE_ORDER = ["nemotron_ocr", "ocrmac", "rapidocr", "easyocr", "tesseract"]
OCR_MODE_ORDER = ["full_page", "pdf_aware_layout_regions", "layout_regions"]
IMAGE_EXT = {".png", ".jpg", ".jpeg", ".tif", ".tiff", ".webp", ".bmp", ".gif"}
GRANITE_PROMPT = "Convert this page to docling."


# ---------------------------------------------------------------- corpus

def corpus_root(path):
    sub = os.path.join(path, "tests", "data")
    return sub if os.path.isdir(sub) else path


def corpus_files(root, formats):
    out = []
    for fmt in sorted(os.listdir(root)):
        fdir = os.path.join(root, fmt)
        if not os.path.isdir(fdir) or (formats and fmt not in formats):
            continue
        for sub in (fdir, os.path.join(fdir, "sources")):
            if not os.path.isdir(sub):
                continue
            for name in sorted(os.listdir(sub)):
                p = os.path.join(sub, name)
                if os.path.isfile(p) and not name.startswith("."):
                    out.append((fmt, p))
    return out


def stem_of(name):
    return os.path.splitext(name)[0]


def read_text(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def json_doc_lines(path, pages=None):
    """Body text of a DoclingDocument JSON as markdown-ish lines, optionally only some pages."""
    with open(path, encoding="utf-8") as f:
        doc = json.load(f)
    lines = []

    def resolve(ref):
        _, kind, idx = ref.split("/")
        return doc[kind][int(idx)]

    def on_pages(item):
        if pages is None:
            return True
        return any(p.get("page_no") in pages for p in item.get("prov", []))

    def walk(item, in_picture=False):
        if item.get("content_layer", "body") != "body":
            return
        ref = item.get("self_ref", "")
        if ref.startswith("#/texts/"):
            if (not in_picture or item.get("label") == "caption") and on_pages(item):
                text = item.get("text", "")
                label = item.get("label")
                if label == "section_header":
                    text = "#" * (item.get("level", 1) + 1) + " " + text
                elif label == "title":
                    text = "# " + text
                lines.append(text)
        elif ref.startswith("#/tables/"):
            if on_pages(item):
                for row in item.get("data", {}).get("grid", []):
                    lines.append("| " + " | ".join(c.get("text", "") for c in row) + " |")
            return
        elif ref.startswith("#/pictures/"):
            for c in item.get("children", []):
                walk(resolve(c["$ref"]), True)
            return
        for c in item.get("children", []):
            walk(resolve(c["$ref"]), in_picture)

    for c in doc["body"].get("children", []):
        walk(resolve(c["$ref"]))
    return "\n".join(lines)


def find_reference(root, fmt, path, pages, measured):
    """(markdown, kind) of the file's reference, or (None, None)."""
    name = os.path.basename(path)
    stem = stem_of(name)
    gt = os.path.join(root, fmt, "groundtruth")
    for cand in (os.path.join(gt, name + ".md"), os.path.join(gt, stem + ".md")):
        if os.path.isfile(cand):
            return read_text(cand), "md"
    engines = [e for e in OCR_ENGINE_ORDER if e not in measured] + \
              [e for e in OCR_ENGINE_ORDER if e in measured]
    for eng in engines:
        for mode in OCR_MODE_ORDER:
            cand = os.path.join(gt, eng, mode, f"{stem}.{eng}.{mode}.md")
            if os.path.isfile(cand):
                return read_text(cand), "ocr_" + eng
    for other in sorted(os.listdir(root)):
        cand = os.path.join(root, other, "groundtruth", stem + ".json")
        if other != fmt and os.path.isfile(cand):
            want = set(range(1, (pages or 1) + 1))
            return json_doc_lines(cand, want), "json_pages"
    return None, None


def page_reference(root, fmt, path, page):
    stem = stem_of(os.path.basename(path))
    gt = os.path.join(root, fmt, "groundtruth")
    cands = [os.path.join(gt, stem + ".json")]
    for eng in OCR_ENGINE_ORDER:
        for mode in OCR_MODE_ORDER:
            cands.append(os.path.join(gt, eng, mode, f"{stem}.{eng}.{mode}.json"))
    for cand in cands:
        if os.path.isfile(cand):
            return json_doc_lines(cand, {page})
    return None


# ---------------------------------------------------------------- pages

def pdf_pages(path):
    try:
        out = subprocess.run(["pdfinfo", path], capture_output=True, text=True, timeout=60,
                             errors="replace").stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    m = re.search(r"^Pages:\s+(\d+)", out, re.M)
    return int(m.group(1)) if m else None


def tiff_frames(path):
    import struct
    with open(path, "rb") as f:
        b = f.read()
    e = "<" if b[:2] == b"II" else ">"
    off, n = struct.unpack(e + "I", b[4:8])[0], 0
    while off and off + 2 <= len(b) and n < len(b):
        n += 1
        cnt = struct.unpack(e + "H", b[off:off + 2])[0]
        nxt = off + 2 + 12 * cnt
        off = struct.unpack(e + "I", b[nxt:nxt + 4])[0] if nxt + 4 <= len(b) else 0
    return n


def page_count(path):
    ext = os.path.splitext(path)[1].lower()
    head = open(path, "rb").read(5)
    if head == b"%PDF-":
        return pdf_pages(path)
    if ext in (".tif", ".tiff"):
        return tiff_frames(path)
    if ext in IMAGE_EXT:
        return 1
    return None


def render_pages(path, dpi, outdir, first=None, last=None):
    args = ["pdftoppm", "-r", str(dpi), "-png"]
    if first:
        args += ["-f", str(first), "-l", str(last or first)]
    subprocess.run(args + [path, os.path.join(outdir, "p")], check=True,
                   capture_output=True, timeout=600)
    return sorted(os.path.join(outdir, n) for n in os.listdir(outdir) if n.endswith(".png"))


# ---------------------------------------------------------------- markdown measures

def normalise_lines(md):
    md = unicodedata.normalize("NFKC", html.unescape(md))
    md = re.sub(r"<!--.*?-->", " ", md, flags=re.S)
    md = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", md)
    md = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", md)
    md = re.sub(r"<[^>\n]{1,200}>", " ", md)
    out = []
    for line in md.splitlines():
        s = line.strip()
        if re.fullmatch(r"\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?", s):
            continue
        s = re.sub(r"^(#{1,6}\s+|[-*+]\s+|\d+[.)]\s+|>\s*)", "", s)
        s = s.replace("|", " ")
        s = re.sub(r"\\([\\`*_{}\[\]()#+\-.!|~<>])", r"\1", s)
        s = re.sub(r"[*_`~]+", "", s)
        s = re.sub(r"\s+", " ", s).strip().casefold()
        if s:
            out.append(s)
    return out


def trigrams(text):
    return {text[i:i + 3] for i in range(len(text) - 2)} if len(text) >= 3 else {text}


def similarity(ref_md, out_md):
    a, b = normalise_lines(ref_md), normalise_lines(out_md)
    ta, tb = trigrams(" ".join(a)), trigrams(" ".join(b))
    union = ta | tb
    jac = len(ta & tb) / len(union) if union else 1.0
    ratio = difflib.SequenceMatcher(None, a, b, autojunk=False).ratio() if (a or b) else 1.0
    return round(jac, 4), round(ratio, 4)


def structure(md):
    heads = [0] * 6
    tables = cells = 0
    lines = md.splitlines()
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        m = re.match(r"^(#{1,6})\s+\S", s)
        if m:
            heads[len(m.group(1)) - 1] += 1
        if s.startswith("|") and i + 1 < len(lines) and \
                re.fullmatch(r"\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?", lines[i + 1].strip()):
            tables += 1
            j = i
            while j < len(lines) and lines[j].strip().startswith("|"):
                row = lines[j].strip()
                if j != i + 1:
                    cells += max(len(re.split(r"(?<!\\)\|", row.strip("|"))), 0)
                j += 1
            i = j
            continue
        i += 1
    return {"headings": heads, "tables": tables, "table_cells": cells}


# ---------------------------------------------------------------- http

def multipart(fields, files):
    boundary = uuid.uuid4().hex
    parts = []
    for k, v in fields:
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'
                     .encode())
    for k, name, data in files:
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"; '
                     f'filename="{name}"\r\nContent-Type: application/octet-stream\r\n\r\n'
                     .encode() + data + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def http(url, body, ctype, headers, timeout):
    req = urllib.request.Request(url, data=body, method="POST" if body is not None else "GET")
    if ctype:
        req.add_header("Content-Type", ctype)
    for k, v in headers.items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        return None, type(e).__name__.encode()


def get_json(url, headers, timeout=30):
    st, body = http(url, None, None, headers, timeout)
    try:
        return st, json.loads(body)
    except ValueError:
        return st, None


# ---------------------------------------------------------------- systems
# Each returns dict(ok, code, md, partial, warnings, pages) for one file.

LMGW_FIELDS = []


def run_lmgw(base, path, timeout, pages=None):
    key = os.environ.get("LMGW_KEY")
    fields = [("output", "markdown")] + ([("pages", str(pages))] if pages else []) + LMGW_FIELDS
    body, ctype = multipart(fields, [("file", os.path.basename(path), open(path, "rb").read())])
    hdr = {"Authorization": f"Bearer {key}"} if key else {}
    st, raw = http(base.rstrip("/") + "/v1/documents/convert", body, ctype, hdr, timeout)
    try:
        ans = json.loads(raw)
    except ValueError:
        ans = None
    doc = (ans or {}).get("documents", [None])[0] if isinstance(ans, dict) else None
    if not doc:
        code = ((ans or {}).get("error") or {}).get("code") if isinstance(ans, dict) else None
        return {"ok": False, "code": code or f"http_{st}"}
    warns = [w.get("code") for w in doc.get("warnings", [])]
    if doc.get("status") == "failed":
        return {"ok": False, "code": (doc.get("error") or {}).get("code") or f"http_{st}",
                "warnings": warns}
    return {"ok": True, "md": doc.get("content") or "", "partial": doc.get("status") == "partial",
            "warnings": warns, "tools": ans.get("tools"),
            "server_seconds": (doc.get("stats") or {}).get("time_ms", 0) / 1000}


def run_docling(base, path, timeout):
    key = os.environ.get("DOCLING_KEY")
    fields = [("to_formats", "md"), ("image_export_mode", "placeholder")]
    body, ctype = multipart(fields, [("files", os.path.basename(path), open(path, "rb").read())])
    hdr = {"X-Api-Key": key} if key else {}
    st, raw = http(base.rstrip("/") + "/v1/convert/file", body, ctype, hdr, timeout)
    try:
        ans = json.loads(raw)
    except ValueError:
        ans = None
    if st != 200 or not isinstance(ans, dict):
        return {"ok": False, "code": f"http_{st}"}
    status = ans.get("status")
    md = ((ans.get("document") or {}).get("md_content")) or ""
    if status not in ("success", "partial_success"):
        return {"ok": False, "code": f"status_{status}"}
    return {"ok": True, "md": md, "partial": status == "partial_success",
            "server_seconds": ans.get("processing_time")}


def run_cmd(tpl, path, timeout):
    argv = [a.replace("{in}", path) for a in shlex.split(tpl)]
    try:
        r = subprocess.run(argv, capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
        return {"ok": False, "code": "timeout"}
    except OSError:
        return {"ok": False, "code": "not_found"}
    if r.returncode != 0:
        return {"ok": False, "code": f"exit_{r.returncode}"}
    return {"ok": True, "md": r.stdout.decode("utf-8", errors="replace")}


def run_page_cmd(tpl, path, timeout, dpi, scratch):
    if open(path, "rb").read(5) != b"%PDF-":
        return run_cmd(tpl, path, timeout)
    d = tempfile.mkdtemp(dir=scratch)
    try:
        imgs = render_pages(path, dpi, d)
        texts = []
        for img in imgs:
            r = run_cmd(tpl, img, timeout)
            if not r["ok"]:
                return r
            texts.append(r["md"])
        return {"ok": True, "md": "\n".join(texts)}
    except subprocess.SubprocessError:
        return {"ok": False, "code": "render_failed"}
    finally:
        shutil.rmtree(d, ignore_errors=True)


# ---------------------------------------------------------------- DocTags

DOCTAGS_CONTAINERS = {
    "doctag", "text", "title", "paragraph", "caption", "footnote", "formula", "code",
    "list_item", "unordered_list", "ordered_list", "otsl", "picture", "chart", "page_header",
    "page_footer", "section_header_level_1", "section_header_level_2", "section_header_level_3",
    "section_header_level_4", "section_header_level_5", "section_header_level_6",
    "key_value_region", "form", "reference", "checkbox", "smiles",
}
OTSL_CELLS = {"fcel", "ecel", "ched", "rhed", "srow", "lcel", "ucel", "xcel"}


def doctags_to_md(s):
    """(markdown, well_formed) of granite-docling's DocTags.

    The structure tags are special tokens: llama-server returns them only when started with
    `--special`; without it the answer is bare text with `<loc_*>` boxes (well_formed false).
    Well-formed: the answer is one `<doctag>…</doctag>` and every container tag closes in order.
    """
    s = re.sub(r"<\|[^|<>]{1,40}\|>", "", s)
    toks = re.findall(r"<(/?)([A-Za-z_][A-Za-z0-9_]*)>|([^<]+|<)", s)
    containers = DOCTAGS_CONTAINERS | {name for slash, name, _ in toks if slash}
    stack, ok = [], True
    out, cur, row = [], [], []
    in_table = cell_open = False
    rows = 0
    skip = 0

    def flush_cell():
        nonlocal cur, cell_open
        if cell_open:
            row.append("".join(cur).strip())
        cur, cell_open = [], False

    def flush_row():
        nonlocal row, rows
        flush_cell()
        if row:
            out.append("| " + " | ".join(row) + " |")
            if rows == 0:
                out.append("|" + "---|" * len(row))
            rows += 1
        row = []

    for slash, name, text in toks:
        if text:
            if not skip:
                cur.append(text)
            continue
        if name.startswith("loc_"):
            continue
        if not slash and name in containers:
            if in_table and name != "otsl":
                flush_row()
            stack.append(name)
            if name in ("page_header", "page_footer"):
                skip += 1
            elif name == "otsl":
                in_table, rows, row, cur = True, 0, [], []
            continue
        if slash:
            if not stack or stack[-1] != name:
                ok = False
            else:
                stack.pop()
            if name == "otsl":
                flush_row()
                in_table = False
                cur = []
                continue
            line = "".join(cur).strip()
            cur = []
            if name in ("page_header", "page_footer"):
                skip = max(skip - 1, 0)
                continue
            m = re.fullmatch(r"section_header_level_(\d)", name)
            if m:
                line = "#" * (int(m.group(1)) + 1) + " " + line
            elif name == "title":
                line = "# " + line
            elif name == "list_item":
                line = "- " + line
            if line and not skip:
                out.append(line)
            continue
        if in_table and name in OTSL_CELLS:
            flush_cell()
            cell_open = True
        elif in_table and name == "nl":
            flush_row()
    if in_table:
        flush_row()
    tail = "".join(cur).strip()
    if tail and not skip:
        out.append(tail)
    body = s.strip()
    well = ok and not stack and body.startswith("<doctag>") and body.endswith("</doctag>")
    return "\n".join(out), well


def run_granite(args, root, measured):
    url = args.granite.rstrip("/") + "/v1/chat/completions"
    budget = args.granite_max_pages
    pages_out = []
    scratch = tempfile.mkdtemp(dir=args.scratch)
    try:
        for rel in args.granite_docs.split(","):
            rel = rel.strip()
            path = os.path.join(root, rel)
            fmt = rel.split("/")[0]
            n = pdf_pages(path) or 0
            for page in range(1, n + 1):
                if budget <= 0:
                    break
                budget -= 1
                d = tempfile.mkdtemp(dir=scratch)
                img = render_pages(path, args.granite_dpi, d, page, page)[0]
                b64 = base64.b64encode(open(img, "rb").read()).decode()
                shutil.rmtree(d, ignore_errors=True)
                req = {"messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64," + b64}},
                    {"type": "text", "text": GRANITE_PROMPT}]}],
                    "temperature": 0}
                t0 = time.monotonic()
                st, raw = http(url, json.dumps(req).encode(), "application/json", {},
                               args.timeout)
                dt = time.monotonic() - t0
                rec = {"seconds": round(dt, 3), "http": st}
                try:
                    ans = json.loads(raw)
                    ch = ans["choices"][0]
                    rec["finish_reason"] = ch.get("finish_reason")
                    rec["completion_tokens"] = ans.get("usage", {}).get("completion_tokens")
                    rec["prompt_tokens"] = ans.get("usage", {}).get("prompt_tokens")
                    content = ch["message"].get("content") or ""
                    if args.granite_raw:
                        os.makedirs(args.granite_raw, exist_ok=True)
                        name = f"{len(pages_out) + 1:03d}-{stem_of(os.path.basename(path))}-p{page}"
                        with open(os.path.join(args.granite_raw, name + ".doctags"), "w",
                                  encoding="utf-8") as f:
                            f.write(content)
                    md, well = doctags_to_md(content)
                    rec["well_formed"] = well
                    ref = page_reference(root, fmt, path, page)
                    if ref is not None:
                        rec["jaccard"], rec["line_ratio"] = similarity(ref, md)
                        if args.lmgw:
                            lr = run_lmgw(args.lmgw, path, args.timeout, pages=page)
                            if lr["ok"]:
                                rec["lmgw_jaccard"], rec["lmgw_line_ratio"] = \
                                    similarity(ref, lr["md"])
                            else:
                                rec["lmgw_code"] = lr["code"]
                except (ValueError, KeyError, IndexError, TypeError):
                    rec["error"] = "bad_answer"
                pages_out.append(rec)
                print(f"granite page {len(pages_out)}: {rec}", file=sys.stderr)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return pages_out


def granite_summary(pages):
    def vals(k):
        return [p[k] for p in pages if isinstance(p.get(k), (int, float))]

    def stat(k):
        v = vals(k)
        return {"mean": round(statistics.mean(v), 4), "median": round(statistics.median(v), 4),
                "n": len(v)} if v else None
    return {
        "pages": len(pages),
        "finish_stop": sum(1 for p in pages if p.get("finish_reason") == "stop"),
        "finish_length": sum(1 for p in pages if p.get("finish_reason") == "length"),
        "well_formed": sum(1 for p in pages if p.get("well_formed")),
        "errors": sum(1 for p in pages if p.get("error") or p.get("http") != 200),
        "seconds_per_page": stat("seconds"),
        "completion_tokens": stat("completion_tokens"),
        "prompt_tokens": stat("prompt_tokens"),
        "jaccard": stat("jaccard"), "line_ratio": stat("line_ratio"),
        "lmgw_jaccard_same_pages": stat("lmgw_jaccard"),
        "lmgw_line_ratio_same_pages": stat("lmgw_line_ratio"),
        "per_page": pages,
    }


# ---------------------------------------------------------------- summary

def summarise(records):
    by = {}
    for r in records:
        by.setdefault(r["format"], {}).setdefault(r["system"], []).append(r)
    out = {}
    for fmt, systems in sorted(by.items()):
        out[fmt] = {}
        for sysname, rs in sorted(systems.items()):
            conv = [r for r in rs if r["ok"]]
            codes = {}
            for r in rs:
                if not r["ok"]:
                    codes[r["code"]] = codes.get(r["code"], 0) + 1
            warn = {}
            for r in conv:
                for w in r.get("warnings") or []:
                    warn[w] = warn.get(w, 0) + 1
            times = [r["seconds"] for r in conv]
            paged = [r for r in conv if r.get("pages")]
            withref = [r for r in conv if "jaccard" in r]
            s = {
                "files": len(rs), "converted": len(conv),
                "partial": sum(1 for r in conv if r.get("partial")),
                "failed": codes, "warnings": warn,
                "seconds_total": round(sum(times), 3),
                "seconds_per_file_median": round(statistics.median(times), 3) if times else None,
                "pages": sum(r["pages"] for r in paged),
                "seconds_per_page": round(sum(r["seconds"] for r in paged) /
                                          sum(r["pages"] for r in paged), 3) if paged else None,
                "with_reference": len(withref),
            }
            srv = [r for r in conv if isinstance(r.get("server_seconds"), (int, float))]
            if srv:
                s["server_seconds_total"] = round(sum(r["server_seconds"] for r in srv), 3)
                sp = [r for r in srv if r.get("pages")]
                if sp:
                    s["server_seconds_per_page"] = round(
                        sum(r["server_seconds"] for r in sp) / sum(r["pages"] for r in sp), 3)
            if withref:
                for k in ("jaccard", "line_ratio"):
                    v = [r[k] for r in withref]
                    s[k + "_mean"] = round(statistics.mean(v), 4)
                    s[k + "_median"] = round(statistics.median(v), 4)
                for k in ("out", "ref"):
                    s[k + "_headings"] = [sum(r[k + "_struct"]["headings"][i] for r in withref)
                                          for i in range(6)]
                    s[k + "_tables"] = sum(r[k + "_struct"]["tables"] for r in withref)
                    s[k + "_table_cells"] = sum(r[k + "_struct"]["table_cells"] for r in withref)
            out[fmt][sysname] = s
    return out


def print_table(paths):
    """A markdown table of one or more summaries, one row per format and system."""
    print("| format | system | files | conv. | failed (codes) | s/file med. | s/page | "
          "with ref | 3-gram J mean | line ratio mean | headings out/ref | tables out/ref | "
          "cells out/ref |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for path in paths:
        with open(path, encoding="utf-8") as f:
            fm = json.load(f)["formats"]
        for fmt, systems in fm.items():
            for name, s in systems.items():
                failed = ", ".join(f"{k} {v}" for k, v in sorted(s["failed"].items())) or "-"

                def g(k):
                    v = s.get(k)
                    return "-" if v is None else str(v)
                heads = (f"{sum(s['out_headings'])}/{sum(s['ref_headings'])}"
                         if "out_headings" in s else "-")
                tabs = f"{s['out_tables']}/{s['ref_tables']}" if "out_tables" in s else "-"
                cells = (f"{s['out_table_cells']}/{s['ref_table_cells']}"
                         if "out_table_cells" in s else "-")
                print(f"| {fmt} | {name} | {s['files']} | {s['converted']} | {failed} | "
                      f"{g('seconds_per_file_median')} | {g('seconds_per_page')} | "
                      f"{s['with_reference']} | {g('jaccard_mean')} | {g('line_ratio_mean')} | "
                      f"{heads} | {tabs} | {cells} |")


def parse_named(items, flag):
    out = []
    for it in items or []:
        if "=" not in it:
            sys.exit(f"{flag} wants NAME=TEMPLATE, got {it!r}")
        name, tpl = it.split("=", 1)
        out.append((name, tpl))
    return out


def main():
    if len(sys.argv) > 2 and sys.argv[1] == "--table":
        print_table(sys.argv[2:])
        return
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--corpus", required=True, help="docling tests/data (or the checkout root)")
    ap.add_argument("--lmgw")
    ap.add_argument("--lmgw-field", action="append", metavar="NAME=VALUE",
                    help="an option field sent to lmgw with every file (ocr=force)")
    ap.add_argument("--docling")
    ap.add_argument("--cmd", action="append", metavar="NAME=TEMPLATE")
    ap.add_argument("--page-cmd", action="append", metavar="NAME=TEMPLATE")
    ap.add_argument("--formats", help="comma-separated format folders (default: all)")
    ap.add_argument("--ocr-formats", default="ocr,tiff,webp",
                    help="folders the --page-cmd systems get (default: ocr,tiff,webp)")
    ap.add_argument("--dpi", type=int, default=300, help="render DPI for --page-cmd (300)")
    ap.add_argument("--timeout", type=float, default=900,
                    help="seconds one request or command may take (900)")
    ap.add_argument("--granite")
    ap.add_argument("--granite-docs", help="comma-separated corpus-relative PDFs")
    ap.add_argument("--granite-max-pages", type=int, help="pages sent to granite, in total")
    ap.add_argument("--granite-dpi", type=int, default=144, help="render DPI for granite (144)")
    ap.add_argument("--granite-raw", help="directory to keep granite's raw DocTags in (scratch)")
    ap.add_argument("--scratch", help="directory for page renders (default: next to --out)")
    ap.add_argument("--per-file", action="store_true", help="add per-file numbers to the summary")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    for f in args.lmgw_field or []:
        name, _, value = f.partition("=")
        LMGW_FIELDS.append((name, value))

    root = corpus_root(args.corpus)
    formats = set(args.formats.split(",")) if args.formats else None
    ocr_formats = set(args.ocr_formats.split(","))
    cmds = parse_named(args.cmd, "--cmd")
    pcmds = parse_named(args.page_cmd, "--page-cmd")
    if args.granite and (not args.granite_docs or not args.granite_max_pages):
        sys.exit("--granite needs --granite-docs and --granite-max-pages")
    args.scratch = args.scratch or os.path.dirname(os.path.abspath(args.out))
    os.makedirs(args.scratch, exist_ok=True)
    measured = {n for n, _ in pcmds}

    systems = []
    if args.lmgw:
        systems.append(("lmgw", None, lambda p: run_lmgw(args.lmgw, p, args.timeout)))
    if args.docling:
        systems.append(("docling-serve", None, lambda p: run_docling(args.docling, p, args.timeout)))
    for name, tpl in cmds:
        systems.append((name, None, lambda p, t=tpl: run_cmd(t, p, args.timeout)))
    for name, tpl in pcmds:
        systems.append((name, ocr_formats,
                        lambda p, t=tpl: run_page_cmd(t, p, args.timeout, args.dpi, args.scratch)))

    records = []
    files = corpus_files(root, formats) if systems else []
    for i, (fmt, path) in enumerate(files):
        rel = os.path.relpath(path, root)
        pages = page_count(path)
        ref, ref_kind = find_reference(root, fmt, path, pages, measured)
        for name, only, fn in systems:
            if only is not None and fmt not in only:
                continue
            t0 = time.monotonic()
            r = fn(path)
            r["seconds"] = round(time.monotonic() - t0, 3)
            r.update(system=name, format=fmt, file=rel, pages=pages)
            if r["ok"] and ref is not None:
                r["jaccard"], r["line_ratio"] = similarity(ref, r["md"])
                r["out_struct"], r["ref_struct"] = structure(r["md"]), structure(ref)
                r["reference"] = ref_kind
            r.pop("md", None)
            records.append(r)
            print(f"[{i + 1}/{len(files)}] {rel} {name}: "
                  f"{'ok' if r['ok'] else r['code']} {r['seconds']}s "
                  f"{r.get('jaccard', '')}", file=sys.stderr)

    summary = {
        "corpus": os.path.basename(os.path.dirname(os.path.dirname(root)))
        if root.endswith(os.path.join("tests", "data")) else None,
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "cores": os.cpu_count(),
        "systems": [n for n, _, _ in systems] + (["granite"] if args.granite else []),
        "dpi": args.dpi,
        "formats": summarise(records),
    }
    if args.docling:
        summary["docling_version"] = get_json(args.docling.rstrip("/") + "/version", {})[1]
    if args.lmgw:
        tools = {}
        for r in records:
            for t in r.pop("tools", None) or []:
                tools[t.get("id") or t.get("tool")] = t.get("version")
        summary["lmgw_tools"] = tools
    summary["reference_kinds"] = sorted({r.get("reference") for r in records
                                         if r.get("reference")})
    if args.granite:
        props = get_json(args.granite.rstrip("/") + "/props", {})[1] or {}
        summary["granite"] = granite_summary(run_granite(args, root, measured))
        summary["granite"]["n_ctx"] = (props.get("default_generation_settings") or {}).get("n_ctx")
        summary["granite"]["build_info"] = props.get("build_info")
        summary["granite"]["dpi"] = args.granite_dpi
    if args.per_file:
        summary["files"] = [{k: v for k, v in r.items()} for r in records]
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=1, sort_keys=True)
    print(f"wrote {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
