#!/usr/bin/env python3
# Usage: python3 -I convert-compare.py --docling URL --lmgw URL --dir FOLDER --out report.json [--also-adapter] [--limit N] [--resume]
#        python3 -I convert-compare.py --dir FOLDER --dry-run            (files per extension; nothing is sent, nothing written)
#        python3 -I convert-compare.py --show report.json                (read a report in full before sharing it)
#        python3 -I convert-compare.py --dir FOLDER --identify HASH...   (on the machine that holds the documents only: which file a report hash is; prints paths)
"""Compare docling-serve and lmgw on a folder of documents, on the machine that holds them, and produce a report of numbers only
(document-conversion design of 2026-11-18, section 10.1, WP0b; D25, D26).

One file, no imports from the repository: copy this file alone. It needs Python 3.6 or newer and
nothing but the standard library (no pip), and it is plain ASCII, so it survives any copy and
prints under any locale. Run it with `python3 -I`. Keys come from the environment only:
DOCLING_KEY (sent as X-Api-Key) and LMGW_KEY (sent as a bearer token); leave one unset if that
server needs none.

What it does. Every file under --dir (subfolders included; hidden files, `~$` lock files and
symlinks are skipped and counted, --follow-symlinks follows them) is sent, one file at a time, to
docling-serve's /v1/convert/file (to_formats=md, the deployment's defaults otherwise) and then to
lmgw's /v1/documents/convert (markdown), one system after the other so their timings do not
compete. --also-adapter sends it to lmgw's docling route too and records only whether the answer
has docling's shape. Files travel under neutral names (doc000012.pdf): the servers never see a
file name. Both markdown outputs of one file are held in memory, compared, and dropped before the
next file; nothing is written but the report, the salt and the state file. Memory: a file is
hashed and uploaded in pieces of CHUNK = 1 MiB and never held whole, so its size does not set the
memory; the two servers' answers for it do (each answer is held while it is read, with about two
more copies of its text while it is measured), plus the guard's word list (at most WORDS_MAX =
1,000,000 distinct words; the report says whether that was reached). A file whose measurement
fails inside this script (no memory left for an answer, an archive that does not unpack, any
other error) is recorded with a fixed word (`failed`), the error's type and line are printed, and
the run goes on with the next file.

Compared per file: text similarity of the normalised text (markdown markup, whitespace and case
folded, NFKC) as a character 3-gram Jaccard and a line-level difflib ratio (left out, as null with
`line_ratio_skipped: work_bound`, when difflib's work for the pair, the sum over the distinct
lines of their count on one side times their count on the other, times the shorter side's line
count, passes LINE_RATIO_MAX_WORK = 100,000,000, about two seconds; repeated lines are what make
difflib slow); headings per level, tables, table cells and pictures counted in each markdown;
lmgw's parts read by text layer, OCR, model and its own readers (`stats`); the time on each side;
and errors as codes (HTTP status, docling `status`, lmgw `error.code` and warning codes). A code
this script does not know is written as `other`. Pictures embedded as `data:` URIs (docling's
default) are cut down to `data:` before characters are counted and words collected, and each
counts as a picture. A file's `chars` and `lines` are rounded to two significant digits in its
entry (exact sums are in `totals` and `by_extension`, rounded too where only one file is summed).

The report (D25) holds numbers, booleans, fixed code words and 12-digit salted hashes, plus two
kinds of text taken from the servers: version strings (1.38.2) and the model alias names lmgw
has chosen for documents. It cannot hold a file name, a folder name, document text, an error
message, a URL or a key:
  1. every value is computed here or passed through a mapper to a number or a fixed word;
  2. before writing, the report is checked against a whitelist (every key known, every value of
     an allowed kind);
  3. the written text is then searched for every file and folder name under --dir and for every
     word of four letters or more of the converted texts; version strings are searched after
     every file as well, and a version string that holds a word of a file's text is withheld
     from then on (`withheld`; resumed runs keep it withheld) while the run goes on;
  4. progress lines show a file's number, its extension, codes and times, never its name;
  5. an unexpected error prints its type and this script's line number, nothing else.
A guard that fires stops the run without a report and names the rule, never the text (a file or
folder name inside a version string or an alias name stops it before any document is sent). The
state file survives, so `--resume --withhold-strings` writes the report at once, with the version
strings and alias names replaced by `withheld` (--withhold-aliases: the alias names only). Alias
names are searched for file and folder names and their words, not for document words: an alias
is made of ordinary words, so the closing summary lists them for you to read instead.

The salt (convert-compare.salt, mode 0600, next to the folder or in --state-dir) keys the file
hashes: HMAC-SHA-256 of a file's bytes, 12 hex digits. With it, and only on the machine that holds the documents, `--identify
HASH` tells which file a hash in the report is. It never enters the report; keep it next to the documents.
The state file (convert-compare.state, same place) holds each finished file's report entry (its
number, hash and measurements) and the first reading of the two systems with every version string
and alias name as a salted hash, so an interrupted run continues with --resume and still writes
one whole report. It holds no server text and no file names, and it stays next to the documents too. A version
string that differs when a run is resumed is written as `changed`.

How to run it:
  --dry-run first (counts only), then `--limit 3`, read the report with --show, then the full
  run with `--resume --overwrite`. Ctrl-C keeps the state; --resume continues. After three files
  in a row that got no answer (no connection, a timeout, a 502/503) the built-in test page is
  sent again: if both systems convert it, the run goes on; if not, it stops. Then, once the
  server is back, plain `--resume` continues and keeps those files' results; `--resume
  --retry-failed` measures them again. A test page that answers 200 but does not convert (a login
  page, say) stops the run before any document is sent, unless --accept-test-page.
  An address may end in /v1 or not. --fresh moves the earlier salt and state aside (`.old`).
  TLS: a private CA goes in --ca-file (added to the system's); --insecure switches certificate
  checks off and the report says so. Proxy variables in the environment are ignored unless
  --use-env-proxy is given; loopback addresses never use a proxy. Redirects are not followed.
  docling-serve's sync route answers 504 after its DOCLING_SERVE_MAX_SYNC_WAIT (120 s by
  default); that file is then sent again through the async route and polled (`route:
  async_fallback`; its time is the async task's, and docling-serve may still be busy with the
  first attempt meanwhile). --docling-route async uses the async route for every file.
  Timeouts: --timeout (1800 s) is the longest one conversion may stay silent, and the longest an
  async task may take; --quick-timeout (30 s) is for version, status and settings requests.
  lmgw's tools, pool and chosen models are read from its admin routes, which need the owner's
  bearer as LMGW_KEY; with a plain key those fields say `unavailable`.
"""
import argparse
import difflib
import hashlib
import hmac
import html
import http.client
import io
import ipaddress
import json
import math
import os
import re
import signal
import socket
import ssl
import statistics
import sys
import time
import traceback
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
import uuid
import warnings
import zipfile

MIN_PYTHON = (3, 6)
if sys.version_info < MIN_PYTHON:
    sys.stderr.write("convert-compare.py needs Python 3.6 or newer\n")
    sys.exit(2)

SCRIPT_REVISION = 2
REPORT_SCHEMA = 2
DEFAULT_TIMEOUT = 1800.0
DEFAULT_QUICK_TIMEOUT = 30.0
DEFAULT_POLL_WAIT = 5.0
POLL_PAUSE = 0.5
WORDS_MAX = 1000000
LINE_RATIO_MAX_WORK = 100000000
CHUNK = 1 << 20
STOP_AFTER_TRANSPORT_ERRORS = 3
SALT_NAME = "convert-compare.salt"
STATE_NAME = "convert-compare.state"
MAX_INT = 2 ** 53
MAX_NUM = 1e15

# ---------------------------------------------------------------- fixed vocabulary

EXTENSIONS = (
    "pdf", "docx", "doc", "docm", "dotx", "pptx", "ppt", "ppsx", "pptm", "xlsx", "xls", "xlsm",
    "xlsb", "odt", "ods", "odp", "rtf", "txt", "md", "csv", "tsv", "html", "htm", "xhtml",
    "mhtml", "xml", "json", "epub", "eml", "msg", "png", "jpg", "jpeg", "tif", "tiff", "bmp",
    "gif", "webp", "heic", "svg", "mp3", "wav", "m4a", "flac", "ogg", "mp4", "mkv", "mov", "vtt",
    "tex", "adoc", "zip", "none", "other")
TRANSPORT = ("ok", "timeout", "refused", "dns", "tls", "reset", "other")
ROUTES = ("sync", "async", "async_fallback")
ANSWERS = ("json_document", "zip", "json_other", "not_json", "none")
ZIP_LAYOUTS = ("flat", "numbered_directories", "other", "none")
DOCLING_STATUS = ("success", "partial_success", "failure", "skipped", "pending", "started",
                  "none", "other")
DOCLING_VERSION_KEYS = ("docling-serve", "docling-jobkit", "docling", "docling-core",
                        "docling-ibm-models", "docling-parse", "python")
PLATFORMS = ("linux", "windows", "macos", "none", "other")
TARGETS = ("inbody", "zip", "put", "s3", "presigned_url", "azure_blob", "google_cloud_storage",
           "google_drive", "none", "other")
LMGW_STATUS = ("converted", "partial", "failed", "skipped", "none", "other")
LMGW_ERRORS = (
    "none", "tool_missing", "no_reader", "unsupported", "unreadable", "invalid_base64",
    "pages_out_of_range", "output_too_large", "deadline_exceeded", "interrupted",
    "source_refused", "source_unreachable", "source_timeout", "source_too_large",
    "source_http_status", "body_limit",
    "no_files", "not_yet", "auth", "authentication_error", "permission_error", "key_scope",
    "key_expired", "key_rate", "key_budget", "rate_limit_exceeded", "request_too_large",
    "session_required", "invalid_request_error", "invalid_option", "not_found", "internal",
    "timeout", "gpu_hold", "model_not_chosen", "model_held", "model_not_allowed", "model_failed",
    "tool_not_started", "too_deep", "answer_memory", "job_storage_full", "too_many_jobs",
    "read_deadline", "memory_limit",
    "other")
LMGW_WARNINGS = (
    "tool_missing", "no_reader", "model_not_chosen", "model_held", "model_cannot_see",
    "model_not_allowed", "part_failed", "model_answer_malformed", "page_scaled",
    "header_removed", "equations_dropped", "pictures_partial", "picture_text",
    "pictures_left_out", "picture_context_cut", "formulas_partial", "ocr_language_missing",
    "ocr_low_confidence", "ocr_page_reduced", "ocr_page_turned", "not_yet", "pages_out_of_range",
    "pages_not_applicable", "option_not_applicable", "memory_limit_widened", "image_not_decoded",
    "other")
LMGW_FORMATS = (
    "text", "pdf", "pdf_page_without_text", "docx", "xlsx", "pptx", "odt", "ods", "odp", "image",
    "audio", "video", "epub", "email", "eml", "msg", "xls", "doc", "ppt", "html", "rtf", "csv",
    "markdown", "json", "xml", "docx_math", "latex", "rst", "org", "mediawiki", "docbook", "fb2",
    "none", "other")
# "reader" is lmgw's own reader executable (web pages, e-books, e-mail, old Excel files,
# pictures), listed among the tools like the others.
TOOL_IDS = ("reader", "poppler", "tesseract", "pandoc", "ffmpeg", "libreoffice", "pdfium",
            "other")
TOOL_STATES = ("found", "missing", "probe_failed", "other")
DEPLOYMENTS = ("desktop", "server", "none", "other")
ADAPTER_SHAPES = ("docling_document", "docling_document_incomplete", "zip", "detail_error",
                  "json_other", "not_json", "none")
GPU = ("yes", "no", "unknown")
# Why a file has no measurement although it was read: memory ran out for an answer, the privacy
# guard refused the file's entry, the file changed while it was sent, or another error here.
FAILURES = ("none", "memory", "guard", "file_changed", "error")
LINE_RATIO_SKIPS = ("none", "work_bound")
# What a version or alias field says when it holds no server text.
FREE_FIXED = ("unparseable", "withheld", "unavailable", "not_reported", "changed", "none")
# Letter runs a version string may carry without being taken for document words.
VERSION_WORDS = frozenset((
    "alpha", "beta", "post", "final", "release", "snapshot", "nightly", "local", "linux",
    "fedora", "ubuntu", "debian", "rhel", "centos", "alpine", "cuda", "cpython", "pypy", "glibc",
    "musl", "git", "dirty", "build", "master", "main", "stable", "preview"))

HASH_RE = re.compile(r"[0-9a-f]{12}")
FREE_RE = {"version": re.compile(r"[0-9A-Za-z.+_-]{1,40}"),
           "alias": re.compile(r"[0-9A-Za-z._:/+@-]{1,80}")}
TASK_ID_RE = re.compile(r"[0-9A-Za-z_-]{1,80}")
UPLOAD_EXT_RE = re.compile(r"[a-z0-9]{1,8}")
LETTERS_RE = re.compile(r"[^\W\d_]+")
WORD_RE = re.compile(r"[^\W\d_]{4,}")
DIGITS = "0123456789"
# A picture embedded in markdown as a data: URI (docling's default image mode): what follows the
# media type is base64, which would count as characters and spell random "words".
DATA_URI_RE = re.compile(r"data:[A-Za-z]+/[A-Za-z0-9.+-]+[;,][^\s)\"'>]*")


# ---------------------------------------------------------------- the whitelist schema (D25)

INT, NUM, BOOL, HASH = ("int",), ("num",), ("bool",), ("hash",)
VERSION, ALIAS = ("free", "version"), ("free", "alias")


def E(values):
    return ("enum", tuple(values))


def N(spec):
    return ("null", spec)


def L(spec, lo=0, hi=None):
    return ("list", spec, lo, hi)


def O(fields, optional=()):
    return ("obj", dict(fields), frozenset(optional))


def M(keys, spec):
    return ("map", tuple(keys), spec)


HEADINGS = L(INT, 6, 6)
COUNTED = O({"chars": INT, "lines": INT, "headings": HEADINGS, "tables": INT,
             "table_cells": INT, "pictures": INT})
# "asr": a speech-to-text model's transcript (WP7); a server before it sends none, read as 0.
BY_SOURCE = O({"text_layer": INT, "ocr": INT, "vision": INT, "native": INT, "asr": INT})
DOCLING_SIDE = O({
    "ok": BOOL, "http": N(INT), "transport": E(TRANSPORT), "route": E(ROUTES),
    "answer": E(ANSWERS), "status": E(DOCLING_STATUS), "errors": INT, "seconds": NUM,
    "sync_wait_seconds": N(NUM), "server_seconds": N(NUM), "counted": N(COUNTED)})
LMGW_SIDE = O({
    "ok": BOOL, "http": N(INT), "transport": E(TRANSPORT), "status": E(LMGW_STATUS),
    "error": E(LMGW_ERRORS), "format": E(LMGW_FORMATS), "warnings": M(LMGW_WARNINGS, INT),
    "tools": L(E(TOOL_IDS)), "seconds": NUM, "server_seconds": N(NUM), "parts": N(INT),
    "parts_by_source": N(BY_SOURCE),
    "stats": N(O({"headings": HEADINGS, "tables": INT, "table_cells": INT, "pictures": INT,
                  "model_calls": INT})),
    "counted": N(COUNTED)})
ADAPTER_SIDE = O({"http": N(INT), "transport": E(TRANSPORT), "shape": E(ADAPTER_SHAPES),
                  "status": E(DOCLING_STATUS), "seconds": NUM})
ENTRY = O({
    "index": INT, "hash": HASH, "ext": E(EXTENSIONS), "size_kib_bucket": INT,
    "failed": E(FAILURES), "docling": N(DOCLING_SIDE), "lmgw": N(LMGW_SIDE),
    "similarity": N(O({"jaccard": NUM, "line_ratio": N(NUM),
                       "line_ratio_skipped": E(LINE_RATIO_SKIPS)})),
    "adapter": ADAPTER_SIDE}, optional=("adapter",))
HTTP_COUNTS = L(O({"http": INT, "files": INT}))
AGGREGATE = O({
    "files": INT, "failed": M(FAILURES, INT), "docling_ok": INT, "lmgw_ok": INT, "both_ok": INT,
    "docling_partial": INT, "lmgw_partial": INT,
    "docling_seconds_total": NUM, "docling_seconds_median": N(NUM),
    "lmgw_seconds_total": NUM, "lmgw_seconds_median": N(NUM),
    "docling_server_seconds_total": NUM, "lmgw_server_seconds_total": NUM,
    "compared": INT, "jaccard_mean": N(NUM), "jaccard_median": N(NUM),
    "line_ratio_compared": INT, "line_ratio_skipped": INT, "line_ratio_mean": N(NUM),
    "line_ratio_median": N(NUM),
    "docling_counted": COUNTED, "lmgw_counted": COUNTED,
    "lmgw_parts": INT, "lmgw_parts_by_source": BY_SOURCE, "lmgw_model_calls": INT,
    "docling_status": M(DOCLING_STATUS, INT), "docling_route": M(ROUTES, INT),
    "docling_answer": M(ANSWERS, INT), "docling_transport": M(TRANSPORT, INT),
    "docling_http": HTTP_COUNTS,
    "lmgw_status": M(LMGW_STATUS, INT), "lmgw_error": M(LMGW_ERRORS, INT),
    "lmgw_warnings": M(LMGW_WARNINGS, INT), "lmgw_transport": M(TRANSPORT, INT),
    "lmgw_http": HTTP_COUNTS, "lmgw_tools": M(TOOL_IDS, INT),
    "adapter_shape": M(ADAPTER_SHAPES, INT)})
PROBE = O({"http": N(INT), "transport": E(TRANSPORT), "ok": BOOL, "seconds": NUM})
ENVIRONMENT = O({
    "script": O({"python": VERSION, "platform": E(PLATFORMS), "cores": N(INT)}),
    "docling": O({
        "health_http": N(INT), "version_http": N(INT),
        "versions": O(dict((k, VERSION) for k in DOCLING_VERSION_KEYS)),
        "other_version_keys": INT, "platform": E(PLATFORMS), "capabilities_http": N(INT),
        "target_default": E(TARGETS), "max_sources_per_request": N(INT),
        "max_num_pages": N(INT), "max_file_size": N(INT), "api_key_required": N(BOOL),
        "gpu": E(GPU), "probe": PROBE, "probe_status": E(DOCLING_STATUS)}),
    "lmgw": O({
        "version_http": N(INT), "version": VERSION, "docling_compat": VERSION,
        "models_http": N(INT), "settings_http": N(INT), "tools_http": N(INT),
        "hold_active": N(BOOL), "auth_enabled": N(BOOL), "max_body_mib": N(INT),
        "deployment": E(DEPLOYMENTS), "logical_cpus": N(INT), "physical_cores": N(INT),
        "tool_slots": N(INT), "pool_cores": N(INT), "pool_memory_gib": N(NUM),
        "tools": M(TOOL_IDS, O({"state": E(TOOL_STATES), "version": VERSION})),
        "models": O({"vision": ALIAS, "picture": ALIAS, "asr": ALIAS}),
        "gpu": E(GPU), "probe": PROBE,
        "probe_status": E(LMGW_STATUS), "probe_error": E(LMGW_ERRORS),
        "probe_tools": M(TOOL_IDS, VERSION)})})
DEFAULT_ANSWER = O({
    "run": BOOL, "files_sent": INT, "http": N(INT), "transport": E(TRANSPORT),
    "route": E(ROUTES), "answer": E(ANSWERS), "zip_layout": E(ZIP_LAYOUTS), "zip_entries": INT,
    "zip_md_entries": INT})
DEFAULTS = O({"one_file": DEFAULT_ANSWER, "several_files": DEFAULT_ANSWER})
RUN = O({
    "started_unix": INT, "finished_unix": INT, "invocations": INT, "files_in_folder": INT,
    "files_measured": INT, "files_unreadable": INT, "hidden_skipped": INT,
    "symlinks_skipped": INT, "symlinks_followed": BOOL, "folders_unreadable": INT,
    "special_skipped": INT, "limit": N(INT), "resumed": BOOL,
    "also_adapter": BOOL, "docling_route": E(("sync", "async")), "docling_target_forced": BOOL,
    "timeout_seconds": NUM, "quick_timeout_seconds": NUM, "poll_wait_seconds": NUM,
    "insecure_tls": BOOL, "ca_file": BOOL, "proxy_env_present": BOOL, "proxy_env_used": BOOL,
    "docling_key_sent": BOOL, "lmgw_key_sent": BOOL,
    "docling_scheme": E(("http", "https")), "lmgw_scheme": E(("http", "https")),
    "docling_loopback": BOOL, "lmgw_loopback": BOOL,
    "strings_withheld": BOOL, "environment_changed_during_run": BOOL,
    "test_page_accepted": BOOL, "test_page_resent": INT})
GUARD = O({"names_checked": INT, "words_checked": INT, "words_capped": BOOL,
           "free_strings": INT, "strings_withheld_for_words": INT})
REPORT = O({
    "report": E(("convert-compare",)), "schema": INT, "script_revision": INT, "complete": BOOL,
    "run": RUN, "environment": ENVIRONMENT, "defaults": DEFAULTS, "guard": GUARD,
    "totals": AGGREGATE, "by_extension": M(EXTENSIONS, AGGREGATE), "files": L(ENTRY)})
STATE_LINE = {
    "header": O({"kind": E(("header",)), "schema": INT, "folder_check": HASH,
                 "started_unix": INT, "environment": ENVIRONMENT}),
    # `replaces`: measured again by --retry-failed; on load it takes the place of the earliest
    # row of that hash that a retry would measure again.
    "file": O({"kind": E(("file",)), "entry": ENTRY, "replaces": BOOL}, optional=("replaces",)),
    # A version string that matched a word of a measured file, as its salted hash.
    "withheld": O({"kind": E(("withheld",)), "string": HASH}),
    "defaults": O({"kind": E(("defaults",)), "defaults": DEFAULTS}),
    "resume": O({"kind": E(("resume",)), "unix": INT}),
    "forced": O({"kind": E(("forced",))}),
}


def _vocabulary():
    words = set(FREE_FIXED) | {"true", "false", "null"}

    def walk(spec):
        kind = spec[0]
        if kind == "enum":
            words.update(spec[1])
        elif kind in ("null", "list"):
            walk(spec[1])
        elif kind == "obj":
            words.update(spec[1])
            for sub in spec[1].values():
                walk(sub)
        elif kind == "map":
            words.update(spec[1])
            walk(spec[2])
    walk(REPORT)
    for spec in STATE_LINE.values():
        walk(spec)
    return frozenset(words)


VOCAB = _vocabulary()


class GuardError(Exception):
    """A privacy rule fired. `rule` and `where` are this script's own words, never data."""

    def __init__(self, rule, where="report"):
        Exception.__init__(self)
        self.rule = rule
        self.where = where


def validate(value, spec, path="report"):
    """Whitelist check: every key known, every value of an allowed kind (D25)."""
    kind = spec[0]
    if kind == "null":
        if value is not None:
            validate(value, spec[1], path)
        return
    if kind == "int":
        ok = type(value) is int and 0 <= value <= MAX_INT
    elif kind == "num":
        ok = type(value) in (int, float) and math.isfinite(value) and 0 <= value <= MAX_NUM
    elif kind == "bool":
        ok = type(value) is bool
    elif kind == "hash":
        ok = type(value) is str and HASH_RE.fullmatch(value) is not None
    elif kind == "enum":
        ok = type(value) is str and value in spec[1]
    elif kind == "free":
        ok = type(value) is str and (value in FREE_FIXED
                                     or FREE_RE[spec[1]].fullmatch(value) is not None)
    elif kind == "list":
        ok = type(value) is list and len(value) >= spec[2] and \
            (spec[3] is None or len(value) <= spec[3])
        if ok:
            for item in value:
                validate(item, spec[1], path + "[]")
    elif kind == "obj":
        if type(value) is not dict:
            raise GuardError("schema_value_kind", path)
        fields, optional = spec[1], spec[2]
        for key in value:
            if type(key) is not str or key not in fields:
                raise GuardError("schema_unknown_key", path)
        for key, sub in fields.items():
            if key in value:
                validate(value[key], sub, path + "." + key)
            elif key not in optional:
                raise GuardError("schema_missing_key", path + "." + key)
        return
    elif kind == "map":
        if type(value) is not dict:
            raise GuardError("schema_value_kind", path)
        for key, item in value.items():
            if type(key) is not str or key not in spec[1]:
                raise GuardError("schema_unknown_key", path)
            validate(item, spec[2], path + "." + key)
        return
    else:
        ok = False
    if not ok:
        raise GuardError("schema_value_kind", path)


# ---------------------------------------------------------------- mappers: field -> fixed word

def enum(value, allowed):
    """The script's own constant for a server's code; `other` for a code it does not know."""
    if value is None and "none" in allowed:
        return "none"
    if isinstance(value, str) and value in allowed:
        return allowed[allowed.index(value)]
    return "other"


def count(value):
    return value if type(value) is int and 0 <= value <= MAX_INT else None


def num(value, digits=4):
    if type(value) not in (int, float) or not math.isfinite(value) or not 0 <= value <= MAX_NUM:
        return None
    return round(float(value), digits)


def flag(value):
    return value if type(value) is bool else None


def http_code(status):
    return status if type(status) is int and 100 <= status <= 599 else None


class FreeStrings(object):
    """The only server text a report may carry: what `version()` and `alias()` let through."""

    def __init__(self):
        self.versions = set()
        self.aliases = set()

    def version(self, value):
        """A version string as the server wrote it, or a fixed word. Twelve hex digits are
        what a file's hash looks like, so a version of that form is not taken."""
        if isinstance(value, str) and FREE_RE["version"].fullmatch(value) \
                and not HASH_RE.fullmatch(value):
            if value not in VOCAB:
                self.versions.add(value)
            return value
        return "none" if value is None else "unparseable"

    def alias(self, value):
        if value is None or value == "":
            return "none"
        if isinstance(value, str) and FREE_RE["alias"].fullmatch(value) \
                and not HASH_RE.fullmatch(value):
            if value not in VOCAB:
                self.aliases.add(value)
            return value
        return "unparseable"

    def all(self):
        return self.versions | self.aliases


def as_dict(value):
    return value if isinstance(value, dict) else {}


def as_list(value):
    return value if isinstance(value, list) else []


def parse_json(body):
    try:
        return json.loads(body.decode("utf-8"))
    except (ValueError, UnicodeDecodeError, AttributeError, RecursionError):
        return None


# ---------------------------------------------------------------- console: nothing of the documents

class Shown(object):
    """Text that may be printed as it is: the owner's own argument, or a checked free string."""

    def __init__(self, text):
        self.text = text


def say(template, *args, **kw):
    """The one way this script prints during a run. `template` is a literal of this script;
    an argument is a number, one of the fixed words, a hash, or a `Shown`; anything else
    prints as `?`."""
    safe = []
    for a in args:
        if isinstance(a, Shown):
            safe.append(a.text)
        elif type(a) in (int, float, bool):
            safe.append(a)
        elif type(a) is str and (a in VOCAB or HASH_RE.fullmatch(a)):
            safe.append(a)
        else:
            safe.append("?")
    emit(kw.get("stream") or sys.stderr, template.format(*safe))


def emit(stream, text):
    """Write a line whatever the terminal's encoding is (an old Python under LANG=C has an
    ASCII one): what it cannot show is written as an escape, not raised as an error."""
    encoding = getattr(stream, "encoding", None) or "ascii"
    EMITTING[0] = True
    try:
        stream.write(text.encode(encoding, "backslashreplace").decode(encoding) + "\n")
        stream.flush()
    except OSError:
        # The reader went away (`| head`, a closed terminal): a measurement never stops for
        # that. Point the stream at nothing, so the interpreter's own flush at exit is quiet.
        try:
            os.dup2(os.open(os.devnull, os.O_WRONLY), stream.fileno())
        except (OSError, ValueError, AttributeError):
            pass
    finally:
        EMITTING[0] = False


class Stop(Exception):
    """End the run with a message built from literals; `code` is the exit status."""

    def __init__(self, code, template, *args):
        Exception.__init__(self)
        self.code, self.template, self.args_ = code, template, args


# ---------------------------------------------------------------- the folder

class Folder(object):
    def __init__(self):
        self.files = []          # (relative path, absolute path)
        self.names = set()       # every file, stem and folder name under the folder
        self.symlinks = 0
        self.hidden = 0
        self.unlistable = 0      # folders that could not be listed, entries that could not be read
        self.special = 0         # neither a file nor a folder (a socket, a device)


def list_folder(root, follow):
    """Files under root in a fixed order. Symlinks are not followed unless `follow`."""
    out = Folder()
    out.names.add(os.path.basename(root))
    seen = {os.path.realpath(root)}

    def note(name):
        out.names.add(name)
        stem = name
        while "." in stem.strip("."):
            stem = stem.rsplit(".", 1)[0]
            out.names.add(stem)

    def walk(directory, rel):
        try:
            with os.scandir(directory) as it:
                entries = sorted(it, key=lambda e: os.fsencode(e.name))
        except OSError:
            out.unlistable += 1
            return
        for e in entries:
            if e.name.startswith(".") or e.name.startswith("~$"):
                out.hidden += 1
                continue
            sub = os.path.join(rel, e.name) if rel else e.name
            link = e.is_symlink()
            if link and not follow:
                out.symlinks += 1
                continue
            try:
                is_dir, is_file = e.is_dir(), e.is_file()
            except OSError:
                out.unlistable += 1
                continue
            if is_dir:
                real = os.path.realpath(e.path)
                if real in seen:
                    continue
                seen.add(real)
                out.names.add(e.name)
                walk(e.path, sub)
            elif is_file:
                note(e.name)
                if link:
                    note(os.path.basename(os.path.realpath(e.path)))
                out.files.append((sub, e.path))
            else:
                out.special += 1
    walk(root, "")
    return out


def extension(name):
    """(the report's extension word, the extension the upload name carries)."""
    base = os.path.basename(name)
    if "." not in base.strip("."):
        return "none", ""
    raw = base.rsplit(".", 1)[1].lower()
    return enum(raw, EXTENSIONS), (raw if UPLOAD_EXT_RE.fullmatch(raw) else "")


def size_bucket(size):
    """The size as a power-of-two bucket in KiB: the smallest one that holds it."""
    kib = (size + 1023) // 1024
    return 0 if kib == 0 else 1 << (kib - 1).bit_length()


def binary_size(kib):
    """A size in KiB in the largest binary unit that keeps it at 1 or more."""
    value = float(kib)
    for unit in ("KiB", "MiB", "GiB"):
        if value < 1024:
            return "{:g} {}".format(round(value, 1), unit)
        value /= 1024
    return "{:g} TiB".format(round(value, 1))


def machine_memory():
    try:
        total = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    except (AttributeError, ValueError, OSError):
        return "unknown"
    return binary_size(total // 1024) if total > 0 else "unknown"


def printable(name):
    return os.fsencode(name).decode("utf-8", "replace")


# ---------------------------------------------------------------- the scan (guard layer 3)

def fold(text):
    return unicodedata.normalize("NFKC", text).casefold()


def unaccent(text):
    return "".join(c for c in unicodedata.normalize("NFKD", text) if not unicodedata.combining(c))


class Needles(object):
    """What the report must not contain of the folder: whole names, and the words in them."""

    def __init__(self, names):
        self.whole, self.numeric, self.parts = set(), set(), set()
        for raw in names:
            for form in {fold(printable(raw)), unaccent(fold(printable(raw)))}:
                if form in VOCAB or form in VERSION_WORDS:
                    continue
                if LETTERS_RE.search(form):
                    if len(form) >= 3:
                        self.whole.add(form)
                    for part in WORD_RE.findall(form):
                        if part not in VOCAB and part not in VERSION_WORDS:
                            self.parts.add(part)
                elif len(form) >= 6:
                    self.numeric.add(form)

    def __len__(self):
        return len(self.whole) + len(self.numeric) + len(self.parts)


class Words(object):
    """Words of four letters or more from the converted texts, at most WORDS_MAX of them."""

    def __init__(self):
        self.seen = set()
        self.capped = False

    @staticmethod
    def of(*texts):
        found = set()
        for text in texts:
            if text:
                for w in WORD_RE.findall(fold(text)):
                    found.add(w)
                    plain = unaccent(w)
                    if plain != w:
                        found.add(plain)
        return found

    def add(self, found):
        for w in found:
            if len(self.seen) >= WORDS_MAX:
                self.capped = True
                return
            self.seen.add(w)


def substrings(text, lo):
    """Every piece of `text` of `lo` characters or more. The strings searched are short (at
    most 80 characters), so this is a few thousand pieces, whatever the number of names."""
    return {text[i:j] for i in range(len(text)) for j in range(i + lo, len(text) + 1)}


def check_names(strings, aliases, needles):
    """No file or folder name, and no word of one, inside a version string or an alias name.
    An alias named after what the files are named after is the sensitive alias. The pieces of
    each string are looked up among the names, not each name searched for in the string."""
    for s in strings:
        low = fold(s)
        where = "a model alias name" if s in aliases else "a version string"
        if not substrings(low, 3).isdisjoint(needles.whole) or \
                not substrings(low, 4).isdisjoint(needles.parts):
            raise GuardError("scan_file_name", where)
        # a numeric name counts where no digit stands right before or after it
        starts = [i for i in range(len(low)) if i == 0 or low[i - 1] not in DIGITS]
        ends = [j for j in range(1, len(low) + 1) if j == len(low) or low[j] not in DIGITS]
        for i in starts:
            for j in ends:
                if j - i >= 6 and low[i:j] in needles.numeric:
                    raise GuardError("scan_file_name", where)


def check_words(strings, aliases, words):
    """The version strings that hold a word of the converted texts. Alias names are ordinary
    words by nature, so they are not searched for document words; the closing summary lists
    them for the owner to read."""
    clashes = set()
    for s in strings:
        if s in aliases:
            continue
        for run in LETTERS_RE.findall(fold(s)):
            if run not in VERSION_WORDS and not substrings(run, 4).isdisjoint(words):
                clashes.add(s)
    return clashes


STRING_TOKEN = re.compile(r'"((?:[^"\\]|\\.)*)"')


def free_strings_of(text):
    """Every string in a serialised report that is neither fixed vocabulary nor a hash. What
    is left around the strings must be JSON's own punctuation and numbers."""
    if any(ord(c) > 126 for c in text):
        raise GuardError("scan_non_ascii")
    free = []

    def repl(m):
        s = json.loads(m.group(0))
        if s not in VOCAB and not HASH_RE.fullmatch(s):
            free.append(s)
        return ""
    rest = STRING_TOKEN.sub(repl, text)
    rest = re.sub(r"\b(?:true|false|null)\b", "", rest)
    if re.search(r"[^\s0-9.,:\[\]{}-]", rest):
        raise GuardError("scan_stray_text")
    return free


def final_scan(text, registry, needles, words):
    """Guard layer 3 over the exact text about to be written. Returns the free strings."""
    free = free_strings_of(text)
    allowed = registry.all()
    for s in free:
        if s not in allowed:
            raise GuardError("scan_unexpected_string")
    check_names(free, registry.aliases, needles)
    if check_words(free, registry.aliases, words.seen):
        raise GuardError("scan_document_word", "a version string")
    return free


def serialise(report):
    head = dict(report)
    files = head.pop("files")
    text = json.dumps(head, indent=1, ensure_ascii=False)
    rows = ",\n".join("  " + json.dumps(f, separators=(",", ":"), ensure_ascii=False)
                      for f in files)
    return text[:-2] + ',\n "files": [' + ("\n" + rows + "\n " if rows else "") + "]\n}\n"


# ---------------------------------------------------------------- markdown measures
# (as WP0a's convert-measure.py, with pictures counted too)

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


def strip_data_uris(md):
    """The markdown with every embedded picture's data cut down to `data:`; the picture's
    markup stays, so it is still counted as a picture."""
    return DATA_URI_RE.sub("data:", md) if md is not None and "data:" in md else md


def trigrams(text):
    return {text[i:i + 3] for i in range(len(text) - 2)} if len(text) >= 3 else {text}


def line_ratio_work(a, b):
    """What difflib's ratio costs for these lines, in its inner loop's steps: one search for
    the longest match visits, for each line of `a`, every equal line of `b` (the sum over the
    distinct lines of count in a times count in b), and the matches found split the search at
    most once per line of the shorter side. Measured: 500 equal lines against 500 lines of
    which every second one is that line take 1.3 s at 62,500,000; 1000 take 10 s at
    500,000,000; 5000 different lines against the same lines shuffled, 0.01 s at 25,000,000."""
    counts = {}
    for line in b:
        counts[line] = counts.get(line, 0) + 1
    seen = {}
    for line in a:
        seen[line] = seen.get(line, 0) + 1
    pairs = sum(n * counts.get(line, 0) for line, n in seen.items())
    return pairs * min(len(a), len(b))


def similarity(md_a, md_b):
    """(3-gram Jaccard, line ratio or None, the reason it is None or `none`)."""
    a, b = normalise_lines(md_a), normalise_lines(md_b)
    ta, tb = trigrams(" ".join(a)), trigrams(" ".join(b))
    union = ta | tb
    jac = len(ta & tb) / len(union) if union else 1.0
    if line_ratio_work(a, b) > LINE_RATIO_MAX_WORK:
        return round(jac, 4), None, "work_bound"
    ratio = difflib.SequenceMatcher(None, a, b, autojunk=False).ratio() if (a or b) else 1.0
    return round(jac, 4), round(ratio, 4), "none"


def two_digits(n):
    """A count rounded to two significant digits (57,428 is 57,000; 57 stays 57): a file's
    exact character and line counts would let someone holding a candidate document recognise
    it in the report."""
    if n < 100:
        return n
    unit = 10 ** (len(str(n)) - 2)
    return (n + unit // 2) // unit * unit


def rounded_counts(counts):
    return dict(counts, chars=two_digits(counts["chars"]), lines=two_digits(counts["lines"]))


def counted(md):
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
                if j != i + 1:
                    cells += len(re.split(r"(?<!\\)\|", lines[j].strip().strip("|")))
                j += 1
            i = j
            continue
        i += 1
    pictures = len(re.findall(r"<!--\s*image\s*-->", md)) + \
        len(re.findall(r"!\[[^\]]*\]\(", md)) + \
        len(re.findall(r"<img\b[^>]*\bsrc=[\"']?data:", md, flags=re.I))
    return {"chars": len(md), "lines": len(lines), "headings": heads, "tables": tables,
            "table_cells": cells, "pictures": pictures}


# ---------------------------------------------------------------- http

class Resp(object):
    def __init__(self, status=None, transport="ok", ctype="", body=b""):
        self.status, self.transport, self.ctype, self.body = status, transport, ctype, body


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def tls_context(ca_file, insecure):
    ctx = ssl.create_default_context()
    if ca_file:
        ctx.load_verify_locations(cafile=ca_file)
    if insecure:
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
    return ctx


def is_loopback(host):
    host = (host or "").strip("[]").lower()
    if host == "localhost" or host.endswith(".localhost"):
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def proxy_env_present():
    return any(os.environ.get(k) for k in ("http_proxy", "https_proxy", "all_proxy",
                                           "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"))


def transport_of(exc):
    r = exc.reason if isinstance(exc, urllib.error.URLError) else exc
    if isinstance(r, socket.timeout):
        return "timeout"
    if isinstance(r, (ssl.SSLError, ssl.CertificateError)):
        return "tls"
    if isinstance(r, socket.gaierror):
        return "dns"
    if isinstance(r, ConnectionRefusedError):
        return "refused"
    if isinstance(r, (ConnectionError, http.client.HTTPException)):
        return "reset"
    return "other"


class Http(object):
    """urllib with no redirects, no proxy unless asked for, and never a proxy for loopback."""

    def __init__(self, ca_file, insecure, use_env_proxy):
        ctx = tls_context(ca_file, insecure)
        self.direct = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=ctx),
            NoRedirect())
        self.proxied = urllib.request.build_opener(
            urllib.request.ProxyHandler(), urllib.request.HTTPSHandler(context=ctx),
            NoRedirect()) if use_env_proxy else self.direct

    def send(self, method, url, body, ctype, headers, timeout):
        req = urllib.request.Request(url, data=body, method=method)
        if ctype:
            req.add_header("Content-Type", ctype)
        if isinstance(body, Multipart):     # sent in pieces: its length is said up front
            req.add_header("Content-Length", str(body.length))
        for k, v in headers.items():
            req.add_header(k, v)
        host = urllib.parse.urlsplit(url).hostname
        opener = self.direct if is_loopback(host) else self.proxied
        try:
            try:
                with opener.open(req, timeout=timeout) as r:
                    return Resp(r.status, "ok", r.headers.get("Content-Type") or "", r.read())
            except urllib.error.HTTPError as e:
                try:
                    return Resp(e.code, "ok", e.headers.get("Content-Type") or "", e.read())
                finally:
                    e.close()
        except (OSError, http.client.HTTPException, ssl.CertificateError) as e:
            return Resp(None, transport_of(e))


class FileChanged(Exception):
    """The file's size changed between its hash and its upload."""


class Upload(object):
    """One file to send: read from disk in pieces of CHUNK at every send, never held whole;
    or the bytes of the built-in test page."""

    def __init__(self, path=None, size=0, data=None):
        self.path, self.data = path, data
        self.size = len(data) if data is not None else size

    def pieces(self):
        if self.data is not None:
            yield self.data
            return
        with open(self.path, "rb") as f:
            left = self.size
            while left > 0:
                piece = f.read(min(CHUNK, left))
                if not piece:
                    raise FileChanged()
                left -= len(piece)
                yield piece
            if f.read(1):
                raise FileChanged()


class Multipart(object):
    """A multipart/form-data body that is sent piece by piece: it can be iterated as often as
    it is sent (a 504's async fallback sends it again), and its length is known up front."""

    def __init__(self, fields, files):
        boundary = uuid.uuid4().hex
        self.ctype = "multipart/form-data; boundary=" + boundary
        self.parts = []
        for k, v in fields:
            self.parts.append(('--{}\r\nContent-Disposition: form-data; name="{}"\r\n\r\n{}\r\n'
                               .format(boundary, k, v)).encode("ascii"))
        for k, name, upload in files:
            self.parts.append(('--{}\r\nContent-Disposition: form-data; name="{}"; '
                               'filename="{}"\r\nContent-Type: application/octet-stream\r\n\r\n'
                               .format(boundary, k, name)).encode("ascii"))
            self.parts.append(upload)
            self.parts.append(b"\r\n")
        self.parts.append("--{}--\r\n".format(boundary).encode("ascii"))
        self.length = sum(p.size if isinstance(p, Upload) else len(p) for p in self.parts)

    def __iter__(self):
        for p in self.parts:
            if isinstance(p, Upload):
                for piece in p.pieces():
                    yield piece
            else:
                yield p


def multipart(fields, files):
    body = Multipart(fields, files)
    return body, body.ctype


def upload_name(index, ext):
    return "doc{:06d}{}".format(index, "." + ext if ext else "")


# ---------------------------------------------------------------- docling-serve

def zip_shape(body):
    """(layout word, entries, markdown entries, the archive or None)."""
    try:
        z = zipfile.ZipFile(io.BytesIO(body))
        names = [n for n in z.namelist() if not n.endswith("/")]
    except MemoryError:
        raise
    except Exception:       # whatever a broken archive raises (BadZipFile, zlib.error, ...)
        return "other", 0, 0, None
    md = [n for n in names if n.lower().endswith(".md")]
    if not names:
        layout = "none"
    elif all(re.match(r"[0-9]{6}/", n) for n in names):
        layout = "numbered_directories"
    elif all("/" not in n or n.startswith("artifacts/") for n in names):
        layout = "flat"
    else:
        layout = "other"
    return layout, len(names), len(md), z


def read_docling_answer(resp, stem=None):
    """One answer of a docling-shaped convert route, mapped. The markdown is returned beside
    the mapped fields and never put among them."""
    out = {"answer": "none", "status": "none", "errors": 0, "server_seconds": None,
           "zip_layout": "none", "zip_entries": 0, "zip_md_entries": 0, "detail": False}
    if resp.status is None:
        return out, None
    if "zip" in resp.ctype.lower() or resp.body[:4] == b"PK\x03\x04" or \
            resp.body[:4] == b"PK\x05\x06":
        out["answer"] = "zip"
        layout, n, n_md, z = zip_shape(resp.body)
        out["zip_layout"], out["zip_entries"], out["zip_md_entries"] = layout, n, n_md
        md = None
        if z is not None and stem is not None:
            mds = [n_ for n_ in z.namelist() if n_.lower().endswith(".md")]
            want = [n_ for n_ in mds if n_.rsplit("/", 1)[-1] == stem + ".md"] or \
                (mds if len(mds) == 1 else [])
            if want:
                try:
                    md = z.read(want[0]).decode("utf-8", "replace")
                    out["status"] = "success"
                except MemoryError:
                    raise
                except Exception:   # a corrupt member: BadZipFile, zlib.error, EOFError, ...
                    md = None
        return out, md
    ans = parse_json(resp.body)
    if ans is None:
        out["answer"] = "not_json"
        return out, None
    doc = ans.get("document") if isinstance(ans, dict) else None
    if not isinstance(doc, dict):
        out["answer"] = "json_other"
        out["detail"] = isinstance(ans, dict) and "detail" in ans
        return out, None
    out["answer"] = "json_document"
    out["status"] = enum(ans.get("status"), DOCLING_STATUS)
    out["errors"] = len(as_list(ans.get("errors")))
    out["server_seconds"] = num(ans.get("processing_time"))
    md = doc.get("md_content")
    # docling's ConvertDocumentResponse: a failed document keeps the shape, its text null.
    converted = out["status"] in ("success", "partial_success")
    out["complete"] = "filename" in doc and "md_content" in doc and \
        (isinstance(md, str) if converted else md is None or isinstance(md, str)) and \
        out["status"] not in ("none", "other") and isinstance(ans.get("errors"), list) and \
        type(ans.get("processing_time")) in (int, float)
    return out, (md if isinstance(md, str) else None)


def docling_async(ctx, body, ctype):
    """Submit to the async route, poll, fetch. Returns (the result's answer, the task's last
    status word)."""
    deadline = time.monotonic() + ctx.args.timeout
    resp = ctx.http.send("POST", ctx.docling + "/v1/convert/file/async", body, ctype,
                         ctx.docling_headers, ctx.args.timeout)
    task = as_dict(parse_json(resp.body)) if resp.status == 200 else {}
    task_id = task.get("task_id")
    if not isinstance(task_id, str) or not TASK_ID_RE.fullmatch(task_id):
        return resp, "none"
    status = "pending"
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            return Resp(None, "timeout"), status
        wait = min(ctx.args.poll_wait, left)
        t = time.monotonic()
        poll = ctx.http.send(
            "GET", "{}/v1/status/poll/{}?wait={}".format(ctx.docling, task_id, round(wait, 1)),
            None, None, ctx.docling_headers, wait + ctx.args.quick_timeout)
        if poll.status != 200:
            return poll, status
        status = enum(as_dict(parse_json(poll.body)).get("task_status"), DOCLING_STATUS)
        if status in ("success", "failure", "partial_success", "skipped", "other", "none"):
            break
        pause = POLL_PAUSE - (time.monotonic() - t)
        if pause > 0:
            time.sleep(pause)
    return ctx.http.send("GET", "{}/v1/result/{}".format(ctx.docling, task_id), None, None,
                         ctx.docling_headers, ctx.args.timeout), status


def docling_convert(ctx, name, data):
    """One file through docling-serve: (the report's fields, the markdown or None)."""
    rec = {"ok": False, "http": None, "transport": "ok", "route": "sync", "answer": "none",
           "status": "none", "errors": 0, "seconds": 0.0, "sync_wait_seconds": None,
           "server_seconds": None, "counted": None}
    fields = [("to_formats", "md")] + ([("target_type", "inbody")] if ctx.force_inbody else [])
    body, ctype = multipart(fields, [("files", name, data)])
    task_status = "none"
    t0 = time.monotonic()
    if ctx.args.docling_route == "sync":
        resp = ctx.http.send("POST", ctx.docling + "/v1/convert/file", body, ctype,
                             ctx.docling_headers, ctx.args.timeout)
        if resp.status == 504:
            rec["sync_wait_seconds"] = num(time.monotonic() - t0, 3)
            rec["route"] = "async_fallback"
            t0 = time.monotonic()
            resp, task_status = docling_async(ctx, body, ctype)
    else:
        rec["route"] = "async"
        resp, task_status = docling_async(ctx, body, ctype)
    rec["seconds"] = num(time.monotonic() - t0, 3)
    rec["http"], rec["transport"] = http_code(resp.status), resp.transport
    ans, md = read_docling_answer(resp, name.rsplit(".", 1)[0] if "." in name else name)
    if resp.status == 200 and ans["answer"] == "json_other" and task_status != "failure" \
            and not ctx.force_inbody:
        # Perhaps the deployment's default target keeps the result out of the body (a storage
        # target): ask for it in the body. If that brings the document, every later file is
        # asked for the same way and the report says so; if not, this answer stands.
        ctx.force_inbody = True
        again, again_md = docling_convert(ctx, name, data)
        if again["answer"] in ("json_document", "zip"):
            return again, again_md
        ctx.force_inbody = False
    rec["answer"] = ans["answer"]
    rec["status"] = ans["status"] if ans["status"] != "none" else task_status
    rec["errors"], rec["server_seconds"] = ans["errors"], ans["server_seconds"]
    rec["ok"] = resp.status == 200 and md is not None and \
        rec["status"] in ("success", "partial_success")
    return rec, (md if rec["ok"] else None)


def docling_default(ctx, uploads):
    """What docling-serve answers with no option at all (D26). Past its sync wait limit the
    question goes through the async route, like a file's."""
    out = {"run": True, "files_sent": len(uploads), "http": None, "transport": "ok",
           "route": ctx.args.docling_route, "answer": "none", "zip_layout": "none",
           "zip_entries": 0, "zip_md_entries": 0}
    body, ctype = multipart([], [("files", n, d) for n, d in uploads])
    if ctx.args.docling_route == "sync":
        resp = ctx.http.send("POST", ctx.docling + "/v1/convert/file", body, ctype,
                             ctx.docling_headers, ctx.args.timeout)
        if resp.status == 504:
            out["route"] = "async_fallback"
            resp, _ = docling_async(ctx, body, ctype)
    else:
        resp, _ = docling_async(ctx, body, ctype)
    ans, _ = read_docling_answer(resp)
    out["http"], out["transport"] = http_code(resp.status), resp.transport
    for k in ("answer", "zip_layout", "zip_entries", "zip_md_entries"):
        out[k] = ans[k]
    return out


NOT_RUN = {"run": False, "files_sent": 0, "http": None, "transport": "ok", "route": "sync",
           "answer": "none", "zip_layout": "none", "zip_entries": 0, "zip_md_entries": 0}


# ---------------------------------------------------------------- lmgw

def lmgw_convert(ctx, name, data, free=None, tool_versions=None):
    """One file through lmgw's native route: (the report's fields, the markdown or None).
    Only the probe page passes `tool_versions`: version strings are taken before the first
    document and never after."""
    rec = {"ok": False, "http": None, "transport": "ok", "status": "none", "error": "none",
           "format": "none", "warnings": {}, "tools": [], "seconds": 0.0,
           "server_seconds": None, "parts": None, "parts_by_source": None, "stats": None,
           "counted": None}
    body, ctype = multipart([("output", "markdown")], [("file", name, data)])
    t0 = time.monotonic()
    resp = ctx.http.send("POST", ctx.lmgw + "/v1/documents/convert", body, ctype,
                         ctx.lmgw_headers, ctx.args.timeout)
    rec["seconds"] = num(time.monotonic() - t0, 3)
    rec["http"], rec["transport"] = http_code(resp.status), resp.transport
    if resp.status is None:
        return rec, None
    ans = as_dict(parse_json(resp.body))
    if tool_versions is not None:
        for t in as_list(ans.get("tools")):
            t = as_dict(t)
            tool_versions[enum(t.get("id"), TOOL_IDS)] = free.version(t.get("version"))
    docs = as_list(ans.get("documents"))
    doc = docs[0] if docs and isinstance(docs[0], dict) else None
    if doc is None:
        err = ans.get("error")
        code = err.get("code") if isinstance(err, dict) else ans.get("code")
        rec["error"] = enum(code, LMGW_ERRORS) if code is not None else \
            ("other" if resp.status >= 400 else "none")
        return rec, None
    rec["status"] = enum(doc.get("status"), LMGW_STATUS)
    rec["format"] = enum(doc.get("format"), LMGW_FORMATS)
    if isinstance(doc.get("error"), dict):
        rec["error"] = enum(doc["error"].get("code"), LMGW_ERRORS)
    for w in as_list(doc.get("warnings")) + as_list(ans.get("warnings")):
        code = enum(as_dict(w).get("code"), LMGW_WARNINGS)
        rec["warnings"][code] = rec["warnings"].get(code, 0) + 1
    rec["tools"] = sorted({enum(as_dict(t).get("id"), TOOL_IDS)
                           for t in as_list(ans.get("tools"))})
    rec["parts"] = count(doc.get("parts"))
    stats = as_dict(doc.get("stats"))
    by = as_dict(stats.get("parts_by_source"))
    src = dict((k, count(by.get(k))) for k in ("text_layer", "ocr", "vision", "native"))
    src["asr"] = count(by.get("asr", 0))
    if None not in src.values():
        rec["parts_by_source"] = src
    heads = stats.get("headings")
    mapped = dict((k, count(stats.get(k)))
                  for k in ("tables", "table_cells", "pictures", "model_calls"))
    if isinstance(heads, list) and len(heads) == 6 and None not in [count(h) for h in heads] \
            and None not in mapped.values():
        mapped["headings"] = [count(h) for h in heads]
        rec["stats"] = mapped
    ms = num(stats.get("time_ms"))
    rec["server_seconds"] = None if ms is None else round(ms / 1000.0, 3)
    content = doc.get("content")
    rec["ok"] = rec["status"] in ("converted", "partial") and isinstance(content, str)
    return rec, (content if rec["ok"] else None)


def adapter_check(ctx, name, data):
    """lmgw's docling route: only whether the answer has docling's shape."""
    rec = {"http": None, "transport": "ok", "shape": "none", "status": "none", "seconds": 0.0}
    body, ctype = multipart([("to_formats", "md")], [("files", name, data)])
    headers = {"X-Api-Key": ctx.lmgw_key} if ctx.lmgw_key else {}
    t0 = time.monotonic()
    resp = ctx.http.send("POST", ctx.lmgw + "/v1/convert/file", body, ctype, headers,
                         ctx.args.timeout)
    rec["seconds"] = num(time.monotonic() - t0, 3)
    rec["http"], rec["transport"] = http_code(resp.status), resp.transport
    ans, _ = read_docling_answer(resp)
    rec["status"] = ans["status"]
    if ans["answer"] == "json_document":
        rec["shape"] = "docling_document" if ans.get("complete") else \
            "docling_document_incomplete"
    elif ans["answer"] == "json_other":
        rec["shape"] = "detail_error" if ans["detail"] else "json_other"
    else:
        rec["shape"] = enum(ans["answer"], ADAPTER_SHAPES)
    return rec


# ---------------------------------------------------------------- environment

def probe_pdf():
    """A one-page PDF with a line of this script's own text: both systems get it before any
    document, so a wrong URL, key or route stops the run at once."""
    stream = b"BT /F1 12 Tf 72 770 Td (convert-compare probe page) Tj ET"
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R "
        b"/Resources << /Font << /F1 5 0 R >> >> >>",
        b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    out, offsets = b"%PDF-1.4\n", []
    for i, obj in enumerate(objs):
        offsets.append(len(out))
        out += str(i + 1).encode() + b" 0 obj\n" + obj + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 6\n0000000000 65535 f \n"
    out += b"".join("{:010d} 00000 n \n".format(o).encode() for o in offsets)
    out += b"trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n" + str(xref).encode() + b"\n%%EOF\n"
    return out


def platform_word(text):
    low = text.lower() if isinstance(text, str) else None
    if low is None:
        return "none"
    for prefix, word in (("linux", "linux"), ("windows", "windows"), ("win32", "windows"),
                         ("darwin", "macos"), ("macos", "macos")):
        if low.startswith(prefix):
            return word
    return "other"


def gather_environment(ctx, free):
    """Everything the report says about the two systems, read before the first document. The
    version strings and alias names taken here are the only server text a report can carry."""
    a, quick = ctx.args, ctx.args.quick_timeout
    env = {"script": {
        "python": free.version("{}.{}.{}".format(*sys.version_info[:3])),
        "platform": platform_word(sys.platform), "cores": count(os.cpu_count())}}

    # docling-serve
    d = {"gpu": a.docling_gpu}
    r = ctx.http.send("GET", ctx.docling + "/health", None, None, ctx.docling_headers, quick)
    if r.status is None:
        unreachable("docling-serve", r.transport, a)
    d["health_http"] = http_code(r.status)
    r = ctx.http.send("GET", ctx.docling + "/version", None, None, ctx.docling_headers, quick)
    d["version_http"] = http_code(r.status)
    v = as_dict(parse_json(r.body)) if r.status == 200 else {}
    versions = {}
    for k in DOCLING_VERSION_KEYS:
        raw = v.get(k)
        if k == "python" and isinstance(raw, str):
            m = re.search(r"[0-9]+\.[0-9]+(?:\.[0-9]+)?", raw)
            raw = m.group(0) if m else raw
        versions[k] = free.version(raw) if r.status == 200 else "unavailable"
    d["versions"] = versions
    d["other_version_keys"] = len([k for k in v if k not in DOCLING_VERSION_KEYS])
    d["platform"] = platform_word(v.get("platform", v.get("plaform")))
    r = ctx.http.send("GET", ctx.docling + "/v1/capabilities", None, None,
                      ctx.docling_headers, quick)
    d["capabilities_http"] = http_code(r.status)
    caps = as_dict(parse_json(r.body)) if r.status == 200 else {}
    limits = as_dict(caps.get("limits"))
    d["target_default"] = enum(as_dict(caps.get("targets")).get("default"), TARGETS)
    for k in ("max_sources_per_request", "max_num_pages", "max_file_size"):
        d[k] = count(limits.get(k))
    d["api_key_required"] = flag(as_dict(caps.get("features")).get("api_key_required"))
    rec, _ = docling_convert(ctx, "probe.pdf", Upload(data=probe_pdf()))
    d["probe"] = {"http": rec["http"], "transport": rec["transport"], "ok": rec["ok"],
                  "seconds": rec["seconds"]}
    d["probe_status"] = rec["status"]
    env["docling"] = d

    # lmgw
    g = {"gpu": a.lmgw_gpu}
    r = ctx.http.send("GET", ctx.lmgw + "/version", None, None, {}, quick)
    if r.status is None:
        unreachable("lmgw", r.transport, a)
    g["version_http"] = http_code(r.status)
    v = as_dict(parse_json(r.body)) if r.status == 200 else {}
    g["version"] = free.version(v.get("lmgw")) if r.status == 200 else "unavailable"
    g["docling_compat"] = free.version(v.get("docling-serve-compat")) \
        if r.status == 200 else "unavailable"
    g["models_http"] = http_code(ctx.http.send("GET", ctx.lmgw + "/v1/models", None, None,
                                               ctx.lmgw_headers, quick).status)
    r = ctx.http.send("GET", ctx.lmgw + "/api/settings-full", None, None, ctx.lmgw_headers,
                      quick)
    g["settings_http"] = http_code(r.status)
    have = r.status == 200
    s = as_dict(parse_json(r.body)) if have else {}
    docs = as_dict(s.get("documents"))
    g["hold_active"] = flag(as_dict(s.get("hold")).get("active"))
    g["auth_enabled"] = flag(s.get("auth_enabled"))
    g["max_body_mib"] = count(s.get("max_body_mb"))
    g["deployment"] = enum(s.get("deployment"), DEPLOYMENTS)
    cpu = as_dict(s.get("host_cpu"))
    g["logical_cpus"], g["physical_cores"] = count(cpu.get("logical_cpus")), \
        count(cpu.get("physical_cores"))
    models = {}
    # GET /api/settings-full names them documents_page_reader and
    # documents_picture_alias.
    for word, key in (("vision", "page_reader"), ("picture", "picture_alias"),
                      ("asr", "asr_alias")):
        raw = docs.get(key, s.get("documents_" + key, s.get("documents." + key)))
        if not have:
            models[word] = "unavailable"
        elif key not in docs and ("documents_" + key) not in s and ("documents." + key) not in s:
            models[word] = "not_reported"
        else:
            models[word] = free.alias(raw)
    g["models"] = models
    r = ctx.http.send("POST", ctx.lmgw + "/api/op/doc_tools", b"{}", "application/json",
                      ctx.lmgw_headers, quick)
    g["tools_http"] = http_code(r.status)
    caps = as_dict(parse_json(r.body)) if r.status == 200 else {}
    tools = {}
    for row in as_list(caps.get("tools")):
        row = as_dict(row)
        tools[enum(row.get("id"), TOOL_IDS)] = {
            "state": enum(row.get("state"), TOOL_STATES),
            "version": free.version(row.get("version"))}
    g["tools"] = tools
    pool = as_dict(caps.get("pool"))
    inputs = as_dict(pool.get("derived_from"))
    g["tool_slots"], g["pool_cores"] = count(pool.get("slots")), count(inputs.get("cores"))
    mem = count(inputs.get("memory_bytes"))
    g["pool_memory_gib"] = None if mem is None else round(mem / float(2 ** 30), 1)
    probe_tools = {}     # a plain key's only view of the tools: those the probe page ran
    rec, _ = lmgw_convert(ctx, "probe.pdf", Upload(data=probe_pdf()), free, probe_tools)
    g["probe"] = {"http": rec["http"], "transport": rec["transport"], "ok": rec["ok"],
                  "seconds": rec["seconds"]}
    g["probe_status"], g["probe_error"] = rec["status"], rec["error"]
    g["probe_tools"] = probe_tools
    env["lmgw"] = g
    return env


def unreachable(name, transport, args):
    hint = ""
    if transport == "tls":
        hint = " (a private CA goes in --ca-file; --insecure switches the check off)"
    elif proxy_env_present() and not args.use_env_proxy:
        hint = " (proxy variables are set and ignored; --use-env-proxy uses them)"
    raise Stop(2, "{} did not answer: {}{}", Shown(name), transport, Shown(hint))


def string_hash(salt, text):
    return hmac.new(salt, b"string\0" + text.encode("utf-8"), hashlib.sha256).hexdigest()[:12]


def hashed(value, salt):
    """The environment as the state file keeps it: every server string as its salted hash."""
    if isinstance(value, dict):
        return dict((k, hashed(v, salt)) for k, v in value.items())
    if isinstance(value, str) and value not in VOCAB and not HASH_RE.fullmatch(value):
        return string_hash(salt, value)
    return value


def restore(stored, now, salt, differs):
    """The environment of a resumed run: the first reading, with each string put back where
    the server still reports the same one, and `changed` where it does not. A string the first
    reading did not have never enters: only the first reading's strings were checked against
    every measured file. `differs` collects a mark per difference."""
    if isinstance(stored, dict):
        now = as_dict(now)
        out = {}
        for k, v in stored.items():
            out[k] = v if k == "probe" else restore(v, now.get(k), salt, differs)
        if set(now) - set(stored):
            differs.append(1)
        return out
    if isinstance(stored, str) and HASH_RE.fullmatch(stored):
        if isinstance(now, str) and string_hash(salt, now) == stored:
            return now
        differs.append(1)
        return "changed"
    if stored != now and stored != "withheld":
        differs.append(1)
    return stored


def withhold(value, only=None):
    """The same structure with every server string (or only those in `only`) replaced by
    `withheld`."""
    if isinstance(value, dict):
        return dict((k, withhold(v, only)) for k, v in value.items())
    if isinstance(value, list):
        return [withhold(v, only) for v in value]
    if isinstance(value, str) and value not in VOCAB and not HASH_RE.fullmatch(value) \
            and (only is None or value in only):
        return "withheld"
    return value


def registry_of(env):
    """The free strings of a stored environment, as the mappers once let them through."""
    free = FreeStrings()
    aliases = set(as_dict(as_dict(env.get("lmgw")).get("models")).values())
    for s in free_strings_of(json.dumps(env, ensure_ascii=False)):
        (free.aliases if s in aliases else free.versions).add(s)
    return free


# ---------------------------------------------------------------- state and salt

class State(object):
    """The state file: the header with the environment (its strings as salted hashes), then
    one line per finished file. Every line passes the whitelist before it is written and
    after it is read, and holds no server text."""

    def __init__(self, path):
        self.path = path
        self.header = None
        self.entries = {}        # hash -> [entry, ...]
        self.defaults = None
        self.forced = False
        self.resumes = 0
        self.withheld = set()    # salted hashes of version strings withheld for a document word
        self.dropped = 0         # lines from the first unreadable one on

    @staticmethod
    def parse(line):
        try:
            row = json.loads(line.decode("utf-8"))
        except (ValueError, RecursionError):
            raise GuardError("state_unreadable", "state")
        kind = row.get("kind") if isinstance(row, dict) else None
        if not isinstance(kind, str) or kind not in STATE_LINE:
            raise GuardError("state_unreadable", "state")
        validate(row, STATE_LINE[kind], "state")
        return kind, row

    def load(self):
        """Read the rows. The first line must be the header. A line cut short by a crash, or
        a line further on that does not read or does not pass the whitelist, is dropped with
        every line after it (counted in `dropped`; `cut` takes them off the file): the rows
        before it are kept, and the files the dropped rows belonged to are measured again."""
        with open(self.path, "rb") as f:
            raw = f.read()
        lines = raw.split(b"\n")
        tail = lines.pop()
        keep = len(raw) - len(tail)
        self.dropped = 1 if tail else 0
        offset = 0
        for n, line in enumerate(lines):
            try:
                kind, row = self.parse(line)
                if (kind == "header") != (n == 0):
                    raise GuardError("state_unreadable", "state")
            except GuardError:
                if n == 0:
                    raise
                self.dropped += len(lines) - n
                keep = offset
                break
            offset += len(line) + 1
            if kind == "header":
                self.header = row
            elif kind == "file":
                self.add_entry(row["entry"], row.get("replaces", False))
            elif kind == "defaults":
                self.defaults = row["defaults"]
            elif kind == "forced":
                self.forced = True
            elif kind == "withheld":
                self.withheld.add(row["string"])
            else:
                self.resumes += 1
        if self.header is None:
            raise GuardError("state_unreadable", "state")
        self.keep, self.size = keep, len(raw)

    def cut(self):
        """Cut the dropped lines off the file (once the header is known to be this run's), so
        the next row written follows the kept ones."""
        if self.keep < self.size:
            os.truncate(self.path, self.keep)

    def add_entry(self, entry, replaces):
        """Rows of one hash stay in order (a folder may hold the same file twice); a row
        measured again by --retry-failed takes the place of the earliest row of that hash a
        retry would measure, so a later plain --resume reads the newer one."""
        rows = self.entries.setdefault(entry["hash"], [])
        if replaces:
            for i, old in enumerate(rows):
                if retryable(old):
                    rows[i] = entry
                    return
        rows.append(entry)

    def append(self, row, fresh=False):
        validate(row, STATE_LINE[row["kind"]], "state")
        text = json.dumps(row, separators=(",", ":"), ensure_ascii=False)
        if free_strings_of(text):       # numbers, fixed words and hashes only: no server text
            raise GuardError("scan_unexpected_string", "the state file")
        flags = os.O_WRONLY | os.O_CREAT | (os.O_TRUNC if fresh else os.O_APPEND)
        fd = os.open(self.path, flags, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text + "\n")
            sync(f)


def sync(f):
    f.flush()
    try:
        os.fsync(f.fileno())
    except OSError:         # a file system without fsync (some network mounts): flushed is enough
        pass


def read_salt(path):
    try:
        with open(path, encoding="ascii") as f:
            text = f.read().strip()
    except ValueError:
        text = ""
    if not re.fullmatch(r"[0-9a-f]{64}", text):
        raise Stop(2, "the salt file is not one this script wrote; move it away or pass --fresh")
    return bytes.fromhex(text)


def new_salt(path):
    salt = os.urandom(32)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="ascii") as f:
        f.write(salt.hex() + "\n")
    os.chmod(path, 0o600)
    return salt


def file_hash(salt, data):
    return hmac.new(salt, data, hashlib.sha256).hexdigest()[:12]


def hash_file(salt, path):
    """(file_hash of the file's bytes, its size), read in pieces of CHUNK."""
    mac = hmac.new(salt, digestmod=hashlib.sha256)
    size = 0
    with open(path, "rb") as f:
        while True:
            piece = f.read(CHUNK)
            if not piece:
                break
            mac.update(piece)
            size += len(piece)
    return mac.hexdigest()[:12], size


def set_aside(paths):
    """Move an earlier run's salt and state out of the way, never deleting one: to `.old`, or
    `.old.2`, `.old.3` when that is taken. Returns the suffix used."""
    n = 1
    while True:
        suffix = ".old" if n == 1 else ".old.{}".format(n)
        if not any(os.path.exists(p + suffix) for p in paths):
            break
        n += 1
    for p in paths:
        if os.path.exists(p):
            os.replace(p, p + suffix)
    return suffix


# ---------------------------------------------------------------- totals

def add_counted(total, part):
    for k in ("chars", "lines", "tables", "table_cells", "pictures"):
        total[k] += part[k]
    total["headings"] = [x + y for x, y in zip(total["headings"], part["headings"])]


def histogram(values):
    out = {}
    for v in values:
        out[v] = out.get(v, 0) + 1
    return out


def aggregate(entries):
    def med(values):
        return round(statistics.median(values), 4) if values else None

    def mean(values):
        return round(sum(values) / len(values), 4) if values else None

    def zero():
        return {"chars": 0, "lines": 0, "headings": [0] * 6, "tables": 0, "table_cells": 0,
                "pictures": 0}
    d = [e["docling"] for e in entries if e["docling"] is not None]
    g = [e["lmgw"] for e in entries if e["lmgw"] is not None]
    both = [e for e in entries if e["docling"] is not None and e["lmgw"] is not None and
            e["docling"]["ok"] and e["lmgw"]["ok"]]
    sims = [e["similarity"] for e in entries if e["similarity"]]
    ratios = [s["line_ratio"] for s in sims if s["line_ratio"] is not None]
    d_count, g_count = zero(), zero()
    for e in both:
        add_counted(d_count, e["docling"]["counted"])
        add_counted(g_count, e["lmgw"]["counted"])
    if len(both) == 1:      # a sum over one file is that file's count: rounded like its entry
        d_count, g_count = rounded_counts(d_count), rounded_counts(g_count)
    by = {"text_layer": 0, "ocr": 0, "vision": 0, "native": 0, "asr": 0}
    warnings, tools = {}, {}
    for x in g:
        for k in by:
            by[k] += (x["parts_by_source"] or {}).get(k, 0)
        for k, n in x["warnings"].items():
            warnings[k] = warnings.get(k, 0) + n
        for k in x["tools"]:
            tools[k] = tools.get(k, 0) + 1

    def http_counts(side):
        h = histogram(x["http"] for x in side if x["http"] is not None)
        return [{"http": k, "files": h[k]} for k in sorted(h)]
    return {
        "files": len(entries),
        "failed": histogram(e["failed"] for e in entries if e["failed"] != "none"),
        "docling_ok": sum(1 for x in d if x["ok"]), "lmgw_ok": sum(1 for x in g if x["ok"]),
        "both_ok": len(both),
        "docling_partial": sum(1 for x in d if x["status"] == "partial_success"),
        "lmgw_partial": sum(1 for x in g if x["status"] == "partial"),
        "docling_seconds_total": round(sum(x["seconds"] for x in d), 3),
        "docling_seconds_median": med([x["seconds"] for x in d if x["ok"]]),
        "lmgw_seconds_total": round(sum(x["seconds"] for x in g), 3),
        "lmgw_seconds_median": med([x["seconds"] for x in g if x["ok"]]),
        "docling_server_seconds_total": round(sum(x["server_seconds"] or 0 for x in d), 3),
        "lmgw_server_seconds_total": round(sum(x["server_seconds"] or 0 for x in g), 3),
        "compared": len(sims),
        "jaccard_mean": mean([s["jaccard"] for s in sims]),
        "jaccard_median": med([s["jaccard"] for s in sims]),
        "line_ratio_compared": len(ratios),
        "line_ratio_skipped": sum(1 for s in sims if s["line_ratio_skipped"] != "none"),
        "line_ratio_mean": mean(ratios), "line_ratio_median": med(ratios),
        "docling_counted": d_count, "lmgw_counted": g_count,
        "lmgw_parts": sum(x["parts"] or 0 for x in g), "lmgw_parts_by_source": by,
        "lmgw_model_calls": sum((x["stats"] or {}).get("model_calls", 0) for x in g),
        "docling_status": histogram(x["status"] for x in d),
        "docling_route": histogram(x["route"] for x in d),
        "docling_answer": histogram(x["answer"] for x in d),
        "docling_transport": histogram(x["transport"] for x in d),
        "docling_http": http_counts(d),
        "lmgw_status": histogram(x["status"] for x in g),
        "lmgw_error": histogram(x["error"] for x in g),
        "lmgw_warnings": warnings, "lmgw_transport": histogram(x["transport"] for x in g),
        "lmgw_http": http_counts(g), "lmgw_tools": tools,
        "adapter_shape": histogram(e["adapter"]["shape"] for e in entries if "adapter" in e),
    }


# ---------------------------------------------------------------- the run

class Ctx(object):
    pass


def base_url(text, flag_name):
    u = urllib.parse.urlsplit(text or "")
    if u.scheme not in ("http", "https") or not u.hostname:
        raise Stop(2, "{} wants an http:// or https:// address", Shown(flag_name))
    if u.username or u.password or u.query or u.fragment:
        raise Stop(2, "{} must be a plain address: no user, password or query (keys come from "
                      "the environment)", Shown(flag_name))
    text = text.rstrip("/")
    if text.endswith("/v1"):        # the address as an API client is given it: the routes add /v1
        text = text[:-3]
    return text, u.scheme, is_loopback(u.hostname)


def env_key(name):
    key = os.environ.get(name) or ""
    if key and not re.fullmatch(r"[\x21-\x7e]+", key):
        raise Stop(2, "{} holds characters a header cannot carry (spaces, line breaks or "
                      "non-ASCII)", Shown(name))
    return key


def unanswered(side):
    """No answer from the server itself: the connection failed, or a gateway in front of it
    said the server is not there."""
    return side is not None and (side["transport"] != "ok" or side["http"] in (502, 503))


def retryable(entry):
    """What --retry-failed measures again: a file a server did not answer, or one whose
    measurement failed here."""
    return entry["failed"] != "none" or unanswered(entry["docling"]) or \
        unanswered(entry["lmgw"])


def measure(ctx, index, path, size, digest):
    """One file through both systems: (entry, words of its outputs). The file is sent from
    disk in pieces; it is never read whole."""
    ext, upload_ext = extension(path)
    return measure_upload(ctx, index, Upload(path, size), ext, upload_ext, digest)


def measure_upload(ctx, index, upload, ext, upload_ext, digest):
    name = upload_name(index, upload_ext)
    entry = {"index": index, "hash": digest, "ext": ext,
             "size_kib_bucket": size_bucket(upload.size), "failed": "none"}
    entry["docling"], d_md = docling_convert(ctx, name, upload)
    entry["lmgw"], g_md = lmgw_convert(ctx, name, upload)
    d_md, g_md = strip_data_uris(d_md), strip_data_uris(g_md)
    if d_md is not None:
        entry["docling"]["counted"] = counted(d_md)
    if g_md is not None:
        entry["lmgw"]["counted"] = counted(g_md)
    entry["similarity"] = None
    if d_md is not None and g_md is not None:
        jac, ratio, skipped = similarity(d_md, g_md)
        entry["similarity"] = {"jaccard": jac, "line_ratio": ratio,
                               "line_ratio_skipped": skipped}
    if ctx.args.also_adapter:
        entry["adapter"] = adapter_check(ctx, name, upload)
    validate(entry, ENTRY, "files[]")
    return entry, Words.of(d_md, g_md)


def failed_entry(index, digest, ext, size, word):
    """The entry of a file whose measurement failed here: what is known before any request,
    and the fixed word for why."""
    return {"index": index, "hash": digest, "ext": ext, "size_kib_bucket": size_bucket(size),
            "failed": word, "docling": None, "lmgw": None, "similarity": None}


def failure_word(exc):
    if isinstance(exc, MemoryError):
        return "memory"
    if isinstance(exc, GuardError):
        return "guard"
    if isinstance(exc, FileChanged):
        return "file_changed"
    return "error"


def report_entry(entry):
    """A file's entry as the report has it: `chars` and `lines` rounded (the state keeps them
    exact for the sums)."""
    out = dict(entry)
    for side in ("docling", "lmgw"):
        if out[side] is not None and out[side]["counted"] is not None:
            out[side] = dict(out[side], counted=rounded_counts(out[side]["counted"]))
    return out


def test_page_converts(ctx, first):
    """After files in a row without an answer: are both servers up? Each must convert the
    test page again, or answer it as it did before the first file (`first`: the probe
    results then), so a system that never converted it does not stop the run here."""
    page = Upload(data=probe_pdf())
    for name, convert in (("docling", docling_convert), ("lmgw", lmgw_convert)):
        rec, _ = convert(ctx, "probe.pdf", page)
        if unanswered(rec) or not (rec["ok"] or (not first[name]["ok"] and
                                                 rec["http"] == first[name]["http"])):
            return False
    return True


def skipped_note(folder, stream):
    if folder.unlistable or folder.special:
        say("not read: {} folders or entries this user may not read, {} that are neither a "
            "file nor a folder", folder.unlistable, folder.special, stream=stream)


def progress(i, total, entry, reused):
    d, g = entry["docling"], entry["lmgw"]
    if entry["failed"] != "none":
        say("[{}/{}] {:<5} not measured: failed {}{}", i, total, entry["ext"], entry["failed"],
            Shown("  (from the state file)" if reused else ""))
        return

    def word(side, fail):
        if side["ok"]:
            return "ok"
        if side["transport"] != "ok":
            return side["transport"]
        if fail == "none":
            return Shown("http {}".format(side["http"]))       # a number this script mapped
        return fail
    sim = entry["similarity"]
    say("[{}/{}] {:<5} docling {} {}s ({})  lmgw {} {}s  similarity {}{}",
        i, total, entry["ext"], word(d, d["status"]), d["seconds"], d["route"],
        word(g, g["error"] if g["error"] != "none" else g["status"]), g["seconds"],
        sim["jaccard"] if sim else Shown("-"), Shown("  (from the state file)" if reused else ""))


def run(args):
    if not os.path.isdir(args.dir):
        raise Stop(2, "--dir is not a folder")
    root = os.path.realpath(args.dir)
    if not os.path.basename(root):
        raise Stop(2, "--dir cannot be the file system's root")
    folder = list_folder(root, args.follow_symlinks)
    by_ext = histogram(extension(rel)[0] for rel, _ in folder.files)

    if args.dry_run:
        say("{} files; nothing was sent or written", len(folder.files), stream=sys.stdout)
        for ext in sorted(by_ext, key=lambda k: (-by_ext[k], k)):
            say("  {:<6} {}", ext, by_ext[ext], stream=sys.stdout)
        sizes = []
        for _, path in folder.files:
            try:
                sizes.append(os.stat(path).st_size)
            except OSError:
                pass
        say("largest file: up to {}; this machine's memory: {}. Files are sent in pieces of "
            "{}, so a file's size does not set this script's memory (the servers' answers "
            "do).", Shown(binary_size(size_bucket(max(sizes)) if sizes else 0)),
            Shown(machine_memory()), Shown(binary_size(CHUNK // 1024)), stream=sys.stdout)
        say("skipped: {} hidden or lock files, {} symlinks{}", folder.hidden, folder.symlinks,
            Shown("" if not folder.symlinks or args.follow_symlinks
                  else " (--follow-symlinks would follow them)"), stream=sys.stdout)
        skipped_note(folder, sys.stdout)
        return 0

    if args.identify:
        return identify(args, root, folder)

    if not args.docling or not args.lmgw or not args.out:
        raise Stop(2, "a run needs --docling URL, --lmgw URL, --dir FOLDER and --out FILE")
    if not folder.files:
        raise Stop(2, "no files under --dir ({} hidden or lock files and {} symlinks were "
                      "skipped); nothing to do, no report written", folder.hidden, folder.symlinks)
    ctx = Ctx()
    ctx.args = args
    ctx.docling, d_scheme, d_loop = base_url(args.docling, "--docling")
    ctx.lmgw, g_scheme, g_loop = base_url(args.lmgw, "--lmgw")
    d_key, ctx.lmgw_key = env_key("DOCLING_KEY"), env_key("LMGW_KEY")
    ctx.docling_headers = {"X-Api-Key": d_key} if d_key else {}
    ctx.lmgw_headers = {"Authorization": "Bearer " + ctx.lmgw_key} if ctx.lmgw_key else {}
    if args.ca_file and not os.path.isfile(args.ca_file):
        raise Stop(2, "--ca-file is not a file")
    try:
        ctx.http = Http(args.ca_file, args.insecure, args.use_env_proxy)
    except (OSError, ssl.SSLError):
        raise Stop(2, "--ca-file could not be read as PEM certificates")
    ctx.force_inbody = False

    # where things are written: checked before any request
    out_path = os.path.abspath(args.out)
    out_dir = os.path.dirname(out_path)
    if os.path.commonpath([os.path.realpath(out_dir), root]) == root:
        raise Stop(2, "--out lies inside --dir; the report must not be written among the "
                      "documents")
    if os.path.exists(out_path) and not args.overwrite:
        raise Stop(2, "--out exists already; pick another name or pass --overwrite")
    if not os.path.isdir(out_dir) or not os.access(out_dir, os.W_OK):
        raise Stop(2, "the folder --out points into does not exist or is not writable")
    state_dir = os.path.realpath(args.state_dir or os.path.dirname(root))
    if os.path.commonpath([state_dir, root]) == root:
        raise Stop(2, "--state-dir lies inside --dir; the salt and state files must stay "
                      "outside the documents")
    if not os.path.isdir(state_dir) or not os.access(state_dir, os.W_OK):
        raise Stop(2, "the salt and state files go next to the folder, and that place is not "
                      "writable; pass --state-dir DIR")
    salt_path = os.path.join(state_dir, SALT_NAME)
    state = State(os.path.join(state_dir, STATE_NAME))
    have_state = os.path.exists(state.path) or os.path.exists(salt_path)
    if have_state and not (args.resume or args.fresh):
        raise Stop(2, "an earlier run left its salt and state files here: pass --resume to "
                      "continue it, or --fresh to start over (they are moved aside to .old)")
    if args.resume and not (os.path.exists(state.path) and os.path.exists(salt_path)):
        raise Stop(2, "--resume found no earlier run here (no salt and state file); run "
                      "without --resume{}", Shown(", with --fresh" if have_state else ""))
    resumed = bool(args.resume)
    if resumed:
        salt = read_salt(salt_path)
        try:
            state.load()
        except GuardError:
            state.header = None
        if state.header is None or state.header["schema"] != REPORT_SCHEMA:
            raise Stop(2, "the state file here is not one this revision of the script can "
                          "continue (another revision wrote it, or it was changed); --fresh "
                          "starts over")
        if state.header["folder_check"] != file_hash(salt, os.fsencode(root)):
            raise Stop(2, "the state file here belongs to another folder; pass --state-dir DIR "
                          "to keep each folder's files apart")
        state.cut()
        if state.dropped:
            say("the state file had {} lines that do not read (cut short by a crash, or "
                "changed); they were dropped with the lines after them, and their files are "
                "measured again", state.dropped)
        ctx.force_inbody = state.forced

    needles = Needles(folder.names)
    words = Words()
    say("{} files under --dir ({} hidden or lock files, {} symlinks skipped)",
        len(folder.files), folder.hidden, folder.symlinks)
    skipped_note(folder, sys.stderr)
    say("checking both systems with a built-in test page first ...")
    free = FreeStrings()
    env = gather_environment(ctx, free)
    first_probe = {"docling": dict(env["docling"]["probe"]), "lmgw": dict(env["lmgw"]["probe"])}
    preflight(env, args)
    changed = False
    if resumed:
        # The report keeps the environment read before the first file: its strings are the
        # ones every measured file's words were checked against.
        differs = []
        env = restore(state.header["environment"], env, salt, differs)
        free, changed = registry_of(env), bool(differs)
    if args.withhold_strings:
        env, free = withhold(env), FreeStrings()
    elif args.withhold_aliases:
        env = withhold(env, free.aliases)
        free.aliases = set()
    validate(env, ENVIRONMENT, "environment")
    check_names(sorted(free.all()), free.aliases, needles)
    started = int(time.time())
    if resumed:
        started = state.header["started_unix"]
        state.append({"kind": "resume", "unix": int(time.time())})
        # a version string an earlier invocation withheld for a document word stays withheld:
        # the words of the files measured then are not read again
        gone = set(s for s in free.versions if string_hash(salt, s) in state.withheld)
        env = withhold(env, gone)
        free.versions -= gone
    else:
        if have_state:          # --fresh: the earlier run's files are moved aside, not deleted
            suffix = set_aside([salt_path, state.path])
            say("the earlier run's salt and state files were moved aside: {}{} and {}{}",
                Shown(SALT_NAME), Shown(suffix), Shown(STATE_NAME), Shown(suffix))
        salt = new_salt(salt_path)
        state.append({"kind": "header", "schema": REPORT_SCHEMA,
                      "folder_check": file_hash(salt, os.fsencode(root)),
                      "started_unix": started, "environment": hashed(env, salt)}, fresh=True)

    def note_forced():
        if ctx.force_inbody and not state.forced:
            state.append({"kind": "forced"})
            state.forced = True
    note_forced()

    entries, unreadable, measured_now, streak, resent = [], 0, 0, 0, 0
    converted = []           # (size bucket, path, upload extension) of files docling converted
    total = len(folder.files)
    limit_hit = False
    for index, (_, path) in enumerate(folder.files, 1):
        ext = extension(path)[0]
        if args.limit is not None and measured_now >= args.limit and not state.entries:
            limit_hit = True        # nothing stored to take over: the file need not be read
            continue
        try:
            digest, size = hash_file(salt, path)
        except OSError:
            unreadable += 1
            say("[{}/{}] {:<5} could not be read; skipped", index, total, ext)
            continue
        stored, retry = None, False
        rows = state.entries.get(digest)
        if rows:
            stored = rows.pop(0)
            if args.retry_failed and retryable(stored):
                stored, retry = None, True
        if stored is not None:
            stored = dict(stored, index=index)
            entries.append(stored)
            if stored["docling"] is not None and stored["docling"]["ok"]:
                converted.append((stored["size_kib_bucket"], path, extension(path)[1]))
            if args.verbose_resume:
                progress(index, total, stored, True)
            continue
        if args.limit is not None and measured_now >= args.limit:
            limit_hit = True
            continue
        try:
            entry, found = measure(ctx, index, path, size, digest)
        except Stop:
            raise
        except Exception as exc:
            # Whatever happens inside one file's measurement (no memory for an answer, an
            # archive that does not unpack, an error in this script) is recorded with a fixed
            # word and the run goes on: a resumed run must never stop at the same file again.
            word, rule = failure_word(exc), getattr(exc, "rule", None)
            kind, where = exception_name(type(exc)), script_lines(exc.__traceback__)
            exc = None
            entry, found = failed_entry(index, digest, ext, size, word), set()
            say("[{}/{}] {:<5} not measured: unexpected {}{} at line {} of this script; "
                "recorded as failed: {}, and the run goes on", index, total, ext, Shown(kind),
                Shown(" (privacy rule " + rule + ")" if word == "guard" else ""),
                Shown(where), word)
        # layer 3, early: the version strings against this file's words; one that holds a
        # word is withheld from now on, in this invocation and in every resumed one
        clashes = check_words(sorted(free.versions), free.aliases, found)
        if clashes:
            for s in sorted(clashes):
                state.append({"kind": "withheld", "string": string_hash(salt, s)})
                state.withheld.add(string_hash(salt, s))
            env = withhold(env, clashes)
            free.versions -= clashes
            say("[{}/{}] {} version string(s) held a word of this file's converted text: "
                "withheld from the report (written `withheld`); the run goes on", index, total,
                len(clashes))
        words.add(found)
        if unanswered(entry["docling"]) or unanswered(entry["lmgw"]):
            streak += 1
        else:
            streak = 0
        row = {"kind": "file", "entry": entry}
        if retry:
            row["replaces"] = True
        state.append(row)
        note_forced()
        entries.append(entry)
        measured_now += 1
        if entry["docling"] is not None and entry["docling"]["ok"]:
            converted.append((entry["size_kib_bucket"], path, extension(path)[1]))
        if entry["failed"] == "none":
            progress(index, total, entry, False)
        if streak >= STOP_AFTER_TRANSPORT_ERRORS:
            resent += 1
            say("{} files in a row got no answer from a server; sending the test page again to "
                "see whether both are up ...", streak)
            if not test_page_converts(ctx, first_probe):
                raise Stop(1, "the test page got no answer either: the run stops here, and "
                              "what it measured is kept. Once the server answers again, the "
                              "same command with --resume continues and keeps those {} files' "
                              "results; with --resume --retry-failed they are measured again",
                           streak)
            say("  both answered it: the servers are up, so those files were too slow or too "
                "large for the connection (--timeout is {} s; a proxy in between may have "
                "limits of its own). They keep that result (--resume --retry-failed measures "
                "them again later); the run goes on.", num(args.timeout))
            streak = 0

    # D26: what docling-serve answers with no option, for one file and for several. A half
    # an earlier invocation could not ask (too few converted files then) is asked now.
    defaults = state.defaults or {"one_file": dict(NOT_RUN), "several_files": dict(NOT_RUN)}
    ask_one, ask_two = not defaults["one_file"]["run"], not defaults["several_files"]["run"]
    picks = []
    if (ask_one and converted) or (ask_two and len(converted) >= 2):
        for _, path, upload_ext in sorted(converted, key=lambda c: c[0])[:2]:
            try:
                size = os.stat(path).st_size
            except OSError:
                continue
            picks.append((upload_name(len(picks) + 1, upload_ext), Upload(path, size)))
    asks = ([picks[:1]] if ask_one and picks else []) + \
        ([picks] if ask_two and len(picks) == 2 else [])
    if asks:
        say("asking docling-serve what it answers with no option: {} ...", Shown(
            "one file, then two together" if len(asks) == 2 else
            "two files together" if len(asks[0]) == 2 else
            "one file (no second converted file to ask with two)"))
        try:
            for files in asks:
                defaults["one_file" if len(files) == 1 else "several_files"] = \
                    docling_default(ctx, files)
        except Exception as exc:
            kind, where = exception_name(type(exc)), script_lines(exc.__traceback__)
            exc = None
            say("  the question stopped: unexpected {} at line {} of this script; the report "
                "says what was not asked as not run", Shown(kind), Shown(where))
        state.append({"kind": "defaults", "defaults": defaults})

    run_block = {
        "started_unix": started, "finished_unix": int(time.time()),
        "invocations": state.resumes + 1 + (1 if resumed else 0),
        "files_in_folder": total, "files_measured": len(entries),
        "files_unreadable": unreadable, "hidden_skipped": folder.hidden,
        "symlinks_skipped": folder.symlinks, "symlinks_followed": bool(args.follow_symlinks),
        "folders_unreadable": folder.unlistable, "special_skipped": folder.special,
        "limit": args.limit, "resumed": resumed, "also_adapter": bool(args.also_adapter),
        "docling_route": args.docling_route, "docling_target_forced": ctx.force_inbody,
        "timeout_seconds": num(args.timeout), "quick_timeout_seconds": num(args.quick_timeout),
        "poll_wait_seconds": num(args.poll_wait), "insecure_tls": bool(args.insecure),
        "ca_file": bool(args.ca_file), "proxy_env_present": proxy_env_present(),
        "proxy_env_used": bool(args.use_env_proxy), "docling_key_sent": bool(d_key),
        "lmgw_key_sent": bool(ctx.lmgw_key), "docling_scheme": d_scheme, "lmgw_scheme": g_scheme,
        "docling_loopback": d_loop, "lmgw_loopback": g_loop,
        "strings_withheld": "withheld" in json.dumps(env),
        "environment_changed_during_run": bool(changed),
        "test_page_accepted": bool(args.accept_test_page), "test_page_resent": resent}
    exts = sorted({e["ext"] for e in entries})
    report = {
        "report": "convert-compare", "schema": REPORT_SCHEMA, "script_revision": SCRIPT_REVISION,
        "complete": not limit_hit and len(entries) + unreadable == total,
        "run": run_block, "environment": env, "defaults": defaults,
        "guard": {"names_checked": len(needles), "words_checked": len(words.seen),
                  "words_capped": words.capped, "free_strings": len(free.all()),
                  "strings_withheld_for_words": len(state.withheld)},
        "totals": aggregate(entries),
        "by_extension": dict((x, aggregate([e for e in entries if e["ext"] == x])) for x in exts),
        "files": [report_entry(e) for e in entries]}
    text = write_report(report, out_path, free, needles, words)
    summary(args, report, text, free)
    return 0


def preflight(env, args):
    """Stop before the first document when a system cannot be measured at all."""
    for name, block in (("docling-serve", env["docling"]), ("lmgw", env["lmgw"])):
        p = block["probe"]
        if p["transport"] != "ok":
            unreachable(name, p["transport"], args)
        if p["http"] in (502, 503):
            raise Stop(2, "{} is not up behind that address (http {})", Shown(name), p["http"])
        if p["http"] in (401, 403):
            raise Stop(2, "{} refused the key (http {}); check {}", Shown(name), p["http"],
                       Shown("DOCLING_KEY" if name != "lmgw" else "LMGW_KEY"))
        if p["http"] in (301, 302, 303, 307, 308):
            raise Stop(2, "{} answered with a redirect (http {}), which is not followed; give "
                          "the address it points to (https?)", Shown(name), p["http"])
        if p["http"] in (404, 405):
            raise Stop(2, "{} has no convert route at that address (http {}); check the URL",
                       Shown(name), p["http"])
    if env["lmgw"]["models_http"] in (401, 403):
        raise Stop(2, "lmgw refused the key (http {}); check LMGW_KEY", env["lmgw"]["models_http"])
    for name, block in (("docling-serve", env["docling"]), ("lmgw", env["lmgw"])):
        p = block["probe"]
        say("  {}: test page http {}, {}, {} s", Shown(name), p["http"] or 0,
            "ok" if p["ok"] else block["probe_status"], p["seconds"])
    for name, block in (("docling-serve", env["docling"]), ("lmgw", env["lmgw"])):
        p = block["probe"]
        if p["ok"]:
            continue
        if p["http"] == 200:
            say("  WARNING: {} answered the test page with http 200 but did not convert it: is "
                "something else at that address (a login page, another service)?", Shown(name))
            if not args.accept_test_page:
                raise Stop(2, "no document was sent. Check the address; to measure anyway, "
                              "run the same command with --accept-test-page")
        else:
            say("  WARNING: {} did not convert the test page (http {}); documents may fail the "
                "same way. The run goes on.", Shown(name), p["http"] or 0)
    if env["lmgw"]["settings_http"] != 200:
        say("  lmgw: its settings and tool list need the owner's bearer as LMGW_KEY; the report "
            "will say unavailable for them (http {})", env["lmgw"]["settings_http"] or 0)


def write_report(report, out_path, free, needles, words):
    """Guard layers 2 and 3 over the report, then the file. Nothing is written when one fires."""
    validate(report, REPORT)
    text = serialise(report)
    if json.loads(text) != report:
        raise GuardError("serialisation_differs")
    validate(json.loads(text), REPORT)
    final_scan(text, free, needles, words)
    tmp = out_path + ".part"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="ascii") as f:
        f.write(text)
        sync(f)
    os.replace(tmp, out_path)
    return text


def summary(args, report, text, free):
    t, r = report["totals"], report["run"]
    out = sys.stdout
    say("", stream=out)
    say("Report written: {} ({} bytes, {} KiB)", Shown(args.out), len(text),
        round(len(text) / 1024.0, 1), stream=out)
    say("  {} of {} files measured{}; docling-serve converted {}, lmgw {}, both {}",
        r["files_measured"], r["files_in_folder"],
        Shown("" if report["complete"] else " (not the whole folder: --resume continues)"),
        t["docling_ok"], t["lmgw_ok"], t["both_ok"], stream=out)
    unknown = sum(t[k].get("other", 0) for k in ("docling_status", "lmgw_status", "lmgw_error",
                                                 "lmgw_warnings", "lmgw_tools"))
    unknown += sum(1 for f in report["files"] if f["lmgw"] and f["lmgw"]["format"] == "other")
    if unknown:
        say("  {} codes from the servers were not in this script's lists and are written as "
            "`other`", unknown, stream=out)
    say("Read this before sharing the report:", stream=out)
    say("  It holds numbers, yes/no values, fixed code words, one 12-digit salted hash per file,",
        stream=out)
    say("  and this text taken from the servers:", stream=out)
    say("    version strings: {}", Shown(", ".join(sorted(free.versions)) or "(none)"),
        stream=out)
    say("    model alias names: {}", Shown(", ".join(sorted(free.aliases)) or "(none)"),
        stream=out)
    say("  It cannot hold file names, folder names, document text, error messages, URLs or keys:",
        stream=out)
    say("  checked against a whitelist of keys and value kinds, then searched for {} names from",
        report["guard"]["names_checked"], stream=out)
    say("  the folder and {} words from the converted texts{}{}.",
        report["guard"]["words_checked"],
        Shown(" of this run's files (the earlier files' words were searched for when they "
              "were measured)" if r["resumed"] else ""),
        Shown(" (the word list was full: WORDS_MAX)" if report["guard"]["words_capped"] else ""),
        stream=out)
    say("  Read it in full:  python3 -I {} --show {}", Shown(os.path.basename(sys.argv[0])),
        Shown(args.out), stream=out)
    say("  Each file's entry keeps small counts exactly (headings, tables, cells, pictures, "
        "pages, times;", stream=out)
    say("  characters and lines rounded): whoever holds a document and the same lmgw version "
        "could", stream=out)
    say("  tell from them whether that document was in the folder.", stream=out)
    say("  Stay on that machine (never copy): {} and {}, next to the folder or in --state-dir.",
        Shown(SALT_NAME), Shown(STATE_NAME), stream=out)


# ---------------------------------------------------------------- --show and --identify

def show(path):
    """Print a report, only if its bytes are exactly what this script writes for what they
    parse to: a key read twice, a changed number or anything added fails that."""
    try:
        with open(path, "rb") as f:
            text = f.read().decode("ascii")
        report = json.loads(text)
        validate(report, REPORT)
        free_strings_of(text)
        if serialise(report) != text:
            raise GuardError("serialisation_differs")
    except (OSError, ValueError, RecursionError, GuardError):
        raise Stop(1, "that file is not a report this script wrote (it does not pass the "
                      "whitelist, or its text is not exactly what the script writes); it is "
                      "not shown")
    out = sys.stdout

    def line(depth, text):
        emit(out, " " * (2 * depth) + text)

    def scalar(v):
        return json.dumps(v) if not isinstance(v, str) else v

    def walk(value, depth):
        for k, v in value.items():
            if isinstance(v, dict) and v and all(not isinstance(x, (dict, list))
                                                 for x in v.values()) and len(v) <= 8:
                line(depth, "{}: {}".format(k, ", ".join(
                    "{} {}".format(a, scalar(b)) for a, b in v.items())))
            elif isinstance(v, dict):
                line(depth, "{}:{}".format(k, "" if v else " (none)"))
                walk(v, depth + 1)
            elif isinstance(v, list) and all(not isinstance(x, (dict, list)) for x in v):
                line(depth, "{}: {}".format(k, " ".join(scalar(x) for x in v) or "(none)"))
            elif isinstance(v, list):
                line(depth, "{}: {}".format(k, ", ".join(
                    " ".join("{} {}".format(a, scalar(b)) for a, b in x.items()) for x in v)))
            else:
                line(depth, "{}: {}".format(k, scalar(v)))
    head = dict(report)
    files = head.pop("files")
    walk(head, 0)
    line(0, "files: {}".format(len(files)))
    for f in files:
        d, g, s = f["docling"], f["lmgw"], f["similarity"]
        line(1, "#{} {} {} <= {} KiB{}".format(
            f["index"], f["hash"], f["ext"], f["size_kib_bucket"],
            "" if f["failed"] == "none" else ", not measured: failed " + f["failed"]))
        for name, side in (("docling", d), ("lmgw", g)) + \
                ((("adapter", f["adapter"]),) if "adapter" in f else ()):
            if side is None:
                continue
            parts = []
            for k, v in side.items():
                if isinstance(v, dict):
                    v = "(" + ", ".join("{} {}".format(a, scalar(b)) for a, b in v.items()) + ")"
                elif isinstance(v, list):
                    v = "[" + " ".join(scalar(x) for x in v) + "]"
                parts.append("{} {}".format(k, scalar(v)))
            line(2, "{}: {}".format(name, "; ".join(parts)))
        line(2, "similarity: {}".format(
            "not compared" if s is None else "jaccard {}, line_ratio {}{}".format(
                s["jaccard"], scalar(s["line_ratio"]),
                "" if s["line_ratio_skipped"] == "none" else
                " (skipped: " + s["line_ratio_skipped"] + ")")))
    out.flush()
    return 0


def identify(args, root, folder):
    """Only on the machine that holds the documents: the files behind report hashes. This is the one mode that prints paths."""
    state_dir = os.path.realpath(args.state_dir or os.path.dirname(root))
    try:
        salt = read_salt(os.path.join(state_dir, SALT_NAME))
    except OSError:
        raise Stop(2, "no salt file next to the folder (or in --state-dir); without it a hash "
                      "cannot be traced back")
    wanted = set(h.lower() for h in args.identify)
    for h in wanted:
        if not HASH_RE.fullmatch(h):
            raise Stop(2, "--identify wants 12-digit hashes as the report has them")
    found = 0
    for index, (rel, path) in enumerate(folder.files, 1):
        try:
            digest = hash_file(salt, path)[0]
        except OSError:
            continue
        if digest in wanted:
            found += 1
            emit(sys.stdout, "{}  #{}  {}".format(digest, index, printable(rel)))
    say("{} of {} hashes found under --dir", found, len(wanted))
    return 0 if found else 1


# ---------------------------------------------------------------- main

def parse_args(argv):
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter,
        usage="python3 -I convert-compare.py --docling URL --lmgw URL --dir FOLDER --out FILE "
              "[options]")
    ap.add_argument("--docling", metavar="URL", help="docling-serve's address")
    ap.add_argument("--lmgw", metavar="URL", help="lmgw's address")
    ap.add_argument("--dir", metavar="FOLDER", help="the folder of documents (read only)")
    ap.add_argument("--out", metavar="FILE", help="the report to write (outside --dir)")
    ap.add_argument("--also-adapter", action="store_true",
                    help="also send each file to lmgw's docling route; records the answer's "
                         "shape only")
    ap.add_argument("--limit", type=int, metavar="N",
                    help="measure at most N files in this run (the report says it is partial)")
    ap.add_argument("--resume", action="store_true",
                    help="continue the earlier run whose salt and state files are here")
    ap.add_argument("--fresh", action="store_true",
                    help="start over with a new salt; the earlier run's salt and state are "
                         "moved aside (.old), not deleted")
    ap.add_argument("--retry-failed", action="store_true",
                    help="with --resume: measure again the files a server did not answer "
                         "(no connection, a timeout, http 502/503) and those whose measurement "
                         "failed here")
    ap.add_argument("--overwrite", action="store_true", help="replace an existing --out")
    ap.add_argument("--dry-run", action="store_true",
                    help="count the files per extension; nothing is sent or written")
    ap.add_argument("--state-dir", metavar="DIR",
                    help="where the salt and state files go (default: next to --dir)")
    ap.add_argument("--follow-symlinks", action="store_true",
                    help="follow symlinks under --dir (default: skipped and counted)")
    ap.add_argument("--withhold-strings", action="store_true",
                    help="write `withheld` in place of version strings and alias names")
    ap.add_argument("--withhold-aliases", action="store_true",
                    help="write `withheld` in place of the alias names only")
    ap.add_argument("--ca-file", metavar="PEM",
                    help="extra CA certificates for both servers, added to the system's")
    ap.add_argument("--insecure", action="store_true",
                    help="do not check TLS certificates (the report records it)")
    ap.add_argument("--use-env-proxy", action="store_true",
                    help="use the environment's proxy variables for non-loopback addresses "
                         "(default: ignored, direct connections)")
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, metavar="S",
                    help="seconds one conversion may stay silent, and an async task may take "
                         "(default {:.0f})".format(DEFAULT_TIMEOUT))
    ap.add_argument("--quick-timeout", type=float, default=DEFAULT_QUICK_TIMEOUT, metavar="S",
                    help="seconds for version, status and settings requests (default {:.0f})"
                    .format(DEFAULT_QUICK_TIMEOUT))
    ap.add_argument("--poll-wait", type=float, default=DEFAULT_POLL_WAIT, metavar="S",
                    help="seconds one status poll of docling's async route waits (default "
                         "{:.0f})".format(DEFAULT_POLL_WAIT))
    ap.add_argument("--docling-route", choices=("sync", "async"), default="sync",
                    help="sync (default; a 504 falls back to async for that file) or async for "
                         "every file")
    ap.add_argument("--docling-gpu", choices=GPU, default="unknown",
                    help="whether docling-serve runs on a GPU there (you say; default unknown)")
    ap.add_argument("--lmgw-gpu", choices=GPU, default="unknown",
                    help="whether lmgw's document models run on a GPU there (default unknown)")
    ap.add_argument("--accept-test-page", action="store_true",
                    help="measure even when a server answers the test page with http 200 but "
                         "does not convert it (default: stop before any document is sent)")
    ap.add_argument("--verbose-resume", action="store_true",
                    help="with --resume: also print a line for each file taken from the state")
    ap.add_argument("--show", metavar="REPORT", help="print a report in full and stop")
    ap.add_argument("--identify", nargs="+", metavar="HASH",
                    help="print the path of the file behind each hash (needs --dir "
                         "and the salt file)")
    args = ap.parse_args(argv)
    if args.show:
        return args
    if not args.dir:
        ap.error("--dir FOLDER is needed (or --show REPORT)")
    if args.limit is not None and args.limit < 1:
        ap.error("--limit wants a number of files, 1 or more")
    for name in ("timeout", "quick_timeout", "poll_wait"):
        v = getattr(args, name)
        if not (math.isfinite(v) and 0 < v <= 604800):
            ap.error("--{} wants seconds between 0 and 604800".format(name.replace("_", "-")))
    if args.resume and args.fresh:
        ap.error("--resume and --fresh cannot go together")
    if (args.retry_failed or args.verbose_resume) and not args.resume:
        ap.error("--retry-failed and --verbose-resume go with --resume")
    return args


def script_lines(tb):
    here = os.path.abspath(__file__)
    lines = [str(f.lineno) for f in traceback.extract_tb(tb)
             if os.path.abspath(f.filename) == here]
    return ", ".join(lines[-4:]) or "?"


def exception_name(kind):
    name = getattr(kind, "__name__", "")
    return name if isinstance(name, str) and \
        re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,60}", name) else "?"


def quiet_hook(kind, value, tb):
    say("unexpected {} at line {} of this script; no report was written. Nothing of the "
        "documents is in this message.", Shown(exception_name(kind)), Shown(script_lines(tb)))


def on_sigterm(signum, frame):
    raise KeyboardInterrupt()


EMITTING = [False]


class EmitOnly(object):
    """stderr before Python 3.8, which has no sys.unraisablehook: the interpreter writes an
    error raised while an object is dropped straight to sys.stderr, with the object's repr
    (which can hold a file's path). This stream lets through only what `emit` writes."""

    def __init__(self, stream):
        self.stream = stream

    def write(self, text):
        return self.stream.write(text) if EMITTING[0] else len(text)

    def __getattr__(self, name):
        return getattr(self.stream, name)


def quiet_output():
    """Nothing but `say` may print: a warning or an error raised while an object is dropped
    would otherwise show the object, which can be a file's path."""
    sys.excepthook = quiet_hook
    warnings.simplefilter("ignore")
    if hasattr(sys, "unraisablehook"):
        sys.unraisablehook = lambda unraisable: None
    elif not isinstance(sys.stderr, EmitOnly):
        sys.stderr = EmitOnly(sys.stderr)


def main(argv=None):
    sys.excepthook = quiet_hook
    warnings.simplefilter("ignore")
    try:
        signal.signal(signal.SIGTERM, on_sigterm)
    except ValueError:      # not the main thread
        pass
    # argparse prints its own usage errors to stderr: only what the owner typed
    args = parse_args(sys.argv[1:] if argv is None else argv)
    quiet_output()
    try:
        if args.show:
            return show(args.show)
        return run(args)
    except Stop as e:
        say(e.template, *e.args_)
        return e.code
    except GuardError as e:
        say("The privacy guard stopped the run: rule {} fired in {}. No report was written; "
            "what was measured so far stays in the state file.", Shown(e.rule), Shown(e.where))
        if e.rule.startswith("scan_"):
            say("If it fired in a version string or an alias name: run the same command again "
                "with --withhold-strings (or --withhold-aliases, for alias names only), and "
                "with --resume when files were measured already; the report is then written "
                "without those strings.")
        return 1
    except KeyboardInterrupt:
        say("interrupted; no report was written. The state is kept: the same command with "
            "--resume continues.")
        return 130
    except SystemExit:
        raise
    except BaseException as e:      # anything else: its type and where, never its text
        quiet_hook(type(e), e, e.__traceback__)
        return 1


if __name__ == "__main__":
    sys.exit(main())
