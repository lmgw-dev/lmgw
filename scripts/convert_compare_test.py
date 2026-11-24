#!/usr/bin/env python3
"""Offline test of scripts/convert-compare.py (WP0b of the document-conversion design, §10.1):
the script runs against two mock servers on loopback, a docling-serve one answering with the
shapes the 1.38.2 release sends (JSON for one file, the flat zip for several, the 504 of its sync
wait limit and the async task that follows, a key check) and an lmgw one (the native answer with
`stats`, warnings, error codes and messages, the admin reads, the docling route). What is checked
is the privacy guard above all: the whitelist rejects a planted file name, the scan catches a
document word or a file name inside a version string, an unknown code becomes `other`, and no
file name, folder name, document word or server message reaches the report, stdout or stderr.
No network beyond loopback, no lmgw, no docling.

Usage: python3 scripts/convert_compare_test.py   (ci/check.sh runs it)
"""

import _thread
import contextlib
import gc
import hashlib
import hmac
import http.server
import importlib.util
import io
import json
import os
import signal
import re
import shutil
import socketserver
import ssl
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "convert-compare.py")
_spec = importlib.util.spec_from_file_location("convert_compare", SCRIPT)
M = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(M)
M.POLL_PAUSE = 0.02          # the pause between two status polls of the async route

# The made-up documents. Their names, their folder's name and their words are what must never
# leave: every one of SECRETS is searched for in the report and in everything printed.
DOCS = {
    "Quartalsbericht Zephyrquartz 2031.pdf":
        "# Kranichfeder Bilanz\n\nDie Mondlaterne leuchtet im Nebelhafen.\n\n"
        "| Posten | Betrag |\n|---|---|\n| Sturmglas | 12 |\n| Wolkenanker | 7 |\n",
    "-leading dash.docx": "## Flusskiesel\n\nEin Absatz mit Tintenfalter und Glimmerpfad.\n",
    "übergröße ünïcode ✓.md": "# Schneehügel\n\nMoosglöckchen am Wegrand.\n",
    "unterordner eins/notiz mit leerzeichen.txt": "Dachsbau und Federkiel.\n",
    "langsam.pdf": "SLOWDOC\n\n# Schattenweber\n\nLangsamer Text vom Reiherstieg.\n",
    "kaputt.xlsx": "LMGWFAIL:no_reader\nDOCLINGFAIL\nPerlmuttzahl Elsternest\n",
    "neu.pptx": "LMGWFAIL:brand_new_code\nLMGWWARN\nSilberdistel Kupferfarn\n",
    "ohne-endung": "Wacholderpfad ganz ohne Endung.\n",
}
FOLDER_NAME = "Mandant Bernsteinufer"
SECRETS = [
    "quartalsbericht", "zephyrquartz", "kranichfeder", "mondlaterne", "nebelhafen", "sturmglas",
    "wolkenanker", "leading dash", "flusskiesel", "tintenfalter", "glimmerpfad",
    "übergröße", "ubergrosse", "ünïcode", "schneehügel", "moosglöckchen",
    "unterordner", "leerzeichen", "dachsbau", "federkiel", "langsam", "schattenweber",
    "reiherstieg", "kaputt", "perlmuttzahl", "elsternest", "silberdistel", "kupferfarn",
    "ohne-endung", "wacholderpfad", "bernsteinufer", "mandant", "schneehugel",
    # what the mock servers put into their messages and file names
    "serverwortlaut", "brand_new_code", "brand_new_warning",
]


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def handle_error(self, request, client_address):
        pass                # a client that hung up, or a refused TLS handshake


def parse_multipart(handler):
    """(fields, files) of a multipart body: files as (field, filename, bytes)."""
    ctype = handler.headers.get("Content-Type") or ""
    body = handler.rfile.read(int(handler.headers.get("Content-Length") or 0))
    m = re.search(r"boundary=([^;]+)", ctype)
    fields, files = {}, []
    if not m:
        return fields, files
    for part in body.split(b"--" + m.group(1).encode())[1:-1]:
        head, _, data = part.partition(b"\r\n\r\n")
        data = data[:-2]
        name = re.search(rb'name="([^"]*)"', head).group(1).decode()
        fn = re.search(rb'filename="([^"]*)"', head)
        if fn:
            files.append((name, fn.group(1).decode(), data))
        else:
            fields[name] = data.decode()
    return fields, files


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, status, body, ctype="application/json", extra=()):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def drain(self):
        self.rfile.read(int(self.headers.get("Content-Length") or 0))


def docling_document(name, text, status="success", errors=()):
    """docling's ConvertDocumentResponse as docling-serve 1.38.2 sends it."""
    return {"document": {"filename": name, "md_content": text, "json_content": None,
                         "html_content": None, "text_content": None, "doctags_content": None,
                         "doclang_content": None},
            "status": status, "errors": list(errors), "processing_time": 0.25, "timings": {},
            "confidence": None}


class Docling(Handler):
    def authorised(self):
        key = self.server.api_key
        if key and self.headers.get("X-Api-Key") != key:
            self.drain()
            self.send(401, {"detail": "Api key required serverwortlaut"})
            return False
        return True

    def do_GET(self):
        s = self.server
        if self.path == "/health":
            return self.send(200, {"status": "ok"})
        if self.path == "/version":
            return self.send(200, s.version)
        if self.path == "/v1/capabilities":
            return self.send(200, {
                "versions": s.version, "targets": {"allowed": ["inbody", "zip"],
                                                   "default": s.target_default},
                "limits": {"max_sources_per_request": 3, "max_num_pages": None,
                           "max_file_size": None, "max_document_timeout": 604800.0},
                "features": {"api_key_required": bool(s.api_key), "websocket": True}})
        if not self.authorised():
            return None
        m = re.match(r"/v1/status/poll/([^?]+)", self.path)
        if m:
            task = s.tasks.get(m.group(1))
            if task is None:
                return self.send(404, {"detail": "Task not found."})
            task["polls"] += 1
            state = "started" if task["polls"] < 2 else "success"
            return self.send(200, {"task_id": m.group(1), "task_type": "convert",
                                   "task_status": state, "task_position": None,
                                   "task_meta": None, "error_message": None, "failure": None})
        m = re.match(r"/v1/result/(.+)", self.path)
        if m:
            task = s.tasks.pop(m.group(1), None)
            if task is None:
                return self.send(404, {"detail": "Task result not found. Please wait for a "
                                                 "completion status."})
            return self.answer(task["fields"], task["files"])
        return self.send(404, {"detail": "Not Found"})

    def do_POST(self):
        s = self.server
        if not self.authorised():
            return None
        fields, files = parse_multipart(self)
        s.seen_names.extend(n for _, n, _ in files)
        s.seen_files.extend(d for _, _, d in files)
        s.seen_fields.append(dict(fields))
        if s.drop_marker and any(s.drop_marker in d for _, _, d in files):
            self.close_connection = True
            return self.connection.close()
        if s.login_page and self.path.startswith("/v1/convert/file"):
            return self.send(200, b"<html>Sign in serverwortlaut</html>", "text/html")
        if self.path == "/v1/convert/file/async":
            s.converts += 1
            task_id = "task-{}".format(len(s.tasks) + s.converts)
            s.tasks[task_id] = {"fields": fields, "files": files, "polls": 0}
            return self.send(200, {"task_id": task_id, "task_type": "convert",
                                   "task_status": "pending", "task_position": 1,
                                   "task_meta": None, "error_message": None, "failure": None})
        if self.path != "/v1/convert/file":
            return self.send(404, {"detail": "Not Found"})
        s.converts += 1
        if s.drop == "gateway":
            return self.send(502, b"<html>Bad Gateway serverwortlaut</html>", "text/html")
        if s.drop:
            self.close_connection = True
            return self.connection.close()
        if any(b"SLOWDOC" in d for _, _, d in files):
            return self.send(504, {"detail": "Conversion is taking too long. serverwortlaut "
                                             + files[0][2].decode()})
        return self.answer(fields, files)

    def answer(self, fields, files):
        s = self.server
        if s.storage_default and fields.get("target_type") != "inbody":
            return self.send(200, {"processing_time": 0.2, "num_converted": len(files),
                                   "num_succeeded": len(files), "num_failed": 0})
        if len(files) > 1 or s.zip_always:
            buf = io.BytesIO()
            with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
                for i, (_, name, data) in enumerate(files):
                    stem = name.rsplit(".", 1)[0]
                    prefix = "{:06d}/".format(i) if s.numbered else ""
                    z.writestr(prefix + stem + ".md", data)
            body = buf.getvalue()
            if len(files) == 1 and b"BROKENZIP" in files[0][2]:
                # a deflate stream that does not inflate: zlib.error on reading the member
                info = zipfile.ZipFile(io.BytesIO(body)).infolist()[0]
                start = 30 + len(info.filename.encode()) + len(info.extra)
                raw = bytearray(body)
                raw[start:start + info.compress_size] = b"\xff" * info.compress_size
                body = bytes(raw)
            return self.send(200, body, "application/zip", [(
                "Content-Disposition", 'attachment; filename="converted_docs.zip"')])
        _, name, data = files[0]
        text = data.decode()
        if "DOCLINGFAIL" in text:
            return self.send(200, docling_document(name, None, "failure", [{
                "component_type": "document_backend", "module_name": "Backend",
                "error_message": "serverwortlaut could not read: " + text}]))
        text = text.replace("# ", "## ")
        if s.embed_picture:
            # docling's default image mode: the picture's bytes inline, as base64
            text += "\n![Image](data:image/png;base64,iVBORw0KGgo9static9AAAANSUhEUgAAAAEAA" \
                    "AABCAYAAAAfFcSJ" + "QUFB" * 2000 + "9static9AAAA==)\n"
        return self.send(200, docling_document(name, text))


class Lmgw(Handler):
    def authorised(self):
        key = self.server.key
        sent = self.headers.get("Authorization") or ""
        api = self.headers.get("X-Api-Key") or ""
        if key and sent != "Bearer " + key and api != key:
            self.drain()
            self.send(401, {"error": {"code": "authentication_error",
                                      "message": "serverwortlaut: no such key"}})
            return False
        return True

    def do_GET(self):
        s = self.server
        if self.path == "/version":
            return self.send(200, {"lmgw": s.version, "docling-serve-compat": "1.38"})
        if not self.authorised():
            return None
        if self.path == "/v1/models":
            return self.send(200, {"data": []})
        if self.path == "/api/settings-full":
            if not s.admin:
                return self.send(401, {"code": "session_required", "message": "serverwortlaut"})
            return self.send(200, dict(s.settings))
        return self.send(404, {"error": {"code": "not_found", "message": "serverwortlaut"}})

    def do_POST(self):
        s = self.server
        if not self.authorised():
            return None
        if self.path == "/api/op/doc_tools":
            self.drain()
            if not s.admin:
                return self.send(401, {"code": "session_required", "message": "serverwortlaut"})
            return self.send(200, {
                "tools": [{"id": "poppler", "label": "PDF tools", "state": "found",
                           "version": "26.08.0", "path": "/usr/bin/pdftotext"},
                          {"id": "tesseract", "label": "OCR", "state": "missing",
                           "version": None, "path": None},
                          {"id": "example-new-tool", "state": "found", "version": "1.0"}],
                "pool": {"slots": 3, "derived": 3, "by_hand": False, "running": 0, "waiting": 0,
                         "derived_from": {"cores": 4, "memory_bytes": 8 * 2 ** 30,
                                          "memory_source": "physical",
                                          "charges": {
                                              "pdf_file": {"bytes": 5 * 2 ** 29, "fit": 3,
                                                           "per_caller": 2},
                                              "page_crops": {"bytes": 6 * 2 ** 30, "fit": 1,
                                                             "per_caller": 1},
                                              "ocr_page": {"bytes": 2 * 2 ** 30, "fit": 3,
                                                           "per_caller": 2},
                                              "reader_run": {"bytes": 5 * 2 ** 30, "fit": 1,
                                                             "per_caller": 1}}}}})
        fields, files = parse_multipart(self)
        s.seen_names.extend(n for _, n, _ in files)
        s.seen_files.extend(d for _, _, d in files)
        if self.path == "/v1/convert/file":
            _, name, data = files[0]
            return self.send(200, docling_document(name, data.decode()))
        if self.path != "/v1/documents/convert":
            return self.send(404, {"error": {"code": "not_found", "message": "serverwortlaut"}})
        s.converts += 1
        if s.interrupt_at and s.converts == s.interrupt_at:
            _thread.interrupt_main()
        _, name, data = files[0]
        text = data.decode()
        stats = {"parts_by_source": {"text_layer": 0, "ocr": 0, "vision": 0, "native": 1},
                 "headings": [text.count("\n# ") + text.startswith("# "), text.count("## "),
                              0, 0, 0, 0],
                 "tables": text.count("\n|---"), "table_cells": 6 * text.count("\n|---"),
                 "pictures": 0, "model_calls": 0, "time_ms": 12}
        doc = {"index": 0, "name": name, "status": "converted", "format": "docx", "parts": 1,
               "content": text, "warnings": [], "stats": stats, "error": None}
        tools = [{"id": "poppler", "version": "26.08.0"}]
        status = 200
        if "LMGWWARN" in text:
            doc["warnings"] = [
                {"code": "header_removed", "message": "serverwortlaut " + text, "tool": None,
                 "parts": [], "option": None},
                {"code": "brand_new_warning", "message": text, "parts": []}]
        m = re.search(r"LMGWFAIL:(\w+)", text)
        if m:
            status = 415
            doc.update(status="failed", content=None, parts=0, error={
                "code": m.group(1), "message": "serverwortlaut cannot read " + text,
                "tool": None})
        return self.send(status, {
            "documents": [doc], "tools": tools, "warnings": [],
            "summary": {"files": 1, "converted": int(status == 200), "partial": 0,
                        "failed": int(status != 200), "skipped": 0}})


DOCLING_VERSION = {
    "docling-serve": "1.38.2", "docling-jobkit": "3.9.1", "docling": "2.142.1",
    "docling-core": "2.104.0", "docling-ibm-models": "4.0.4", "docling-parse": "7.23.0",
    "python": "cpython-312 (3.12.15)",
    "plaform": "Linux-0.0.0-example.x86_64-x86_64-with-glibc2.34"}


def start(handler, tls=None, **attrs):
    server = Server(("127.0.0.1", 0), handler)
    defaults = dict(
        api_key=None, version=dict(DOCLING_VERSION), target_default="inbody", tasks={},
        seen_names=[], seen_files=[], seen_fields=[], converts=0, drop=False, drop_marker=None,
        login_page=False, embed_picture=False, storage_default=False,
        zip_always=False, numbered=False, key=None, admin=True, interrupt_at=0,
        settings={"hold": {"active": False}, "auth_enabled": True, "max_body_mb": 64,
                  "deployment": "server", "version": "0.7.0",
                  "host_cpu": {"logical_cpus": 4, "physical_cores": 2}})
    if handler is Lmgw:
        defaults["version"] = "0.7.0+118"
    defaults.update(attrs)
    for k, v in defaults.items():
        setattr(server, k, v)
    scheme = "http"
    if tls:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(*tls)
        server.socket = ctx.wrap_socket(server.socket, server_side=True)
        scheme = "https"
    threading.Thread(target=server.serve_forever, args=(0.02,), daemon=True).start()
    server.url = "{}://127.0.0.1:{}".format(scheme, server.server_address[1])
    return server


def stop(*servers):
    for s in servers:
        s.shutdown()
        s.server_close()


class Case(unittest.TestCase):
    """A folder of made-up documents and two mock servers per test."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="convert-compare-test-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.folder = os.path.join(self.tmp, FOLDER_NAME)
        for rel, text in DOCS.items():
            # as bytes, so the names are UTF-8 on disk whatever locale the test runs under
            path = os.path.join(self.folder, *rel.split("/")).encode("utf-8")
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(text.encode("utf-8"))
        self.out = os.path.join(self.tmp, "report.json")
        self.env = dict(os.environ)
        for k in list(os.environ):
            if k.lower().endswith("_proxy") or k in ("DOCLING_KEY", "LMGW_KEY"):
                del os.environ[k]
        self.addCleanup(self.restore_env)
        self.servers = []

    def restore_env(self):
        os.environ.clear()
        os.environ.update(self.env)
        stop(*self.servers)

    def serve(self, docling=None, lmgw=None, tls=None):
        self.docling = start(Docling, tls, **(docling or {}))
        self.lmgw = start(Lmgw, tls, **(lmgw or {}))
        self.servers += [self.docling, self.lmgw]

    def run_script(self, *extra, **kw):
        """(exit code, stdout + stderr) of one run against the mocks."""
        argv = ["--docling", self.docling.url, "--lmgw", self.lmgw.url, "--dir",
                kw.get("folder", self.folder), "--out", self.out] + list(extra)
        return self.call(argv)

    def call(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = M.main(argv)
            except SystemExit as e:
                code = e.code
        return code, out.getvalue() + err.getvalue()

    def report(self):
        with open(self.out, encoding="ascii") as f:
            return json.load(f)

    def assert_clean(self, text, what):
        low = text.casefold()
        for secret in SECRETS:
            self.assertNotIn(secret, low, "{} holds a name or a word of the documents".format(
                what))

    def by_ext(self, report, ext):
        rows = [f for f in report["files"] if f["ext"] == ext]
        self.assertTrue(rows, ext)
        return rows


class HappyPath(Case):
    def test_run_report_and_output(self):
        self.serve()
        code, printed = self.run_script("--also-adapter")
        self.assertEqual(code, 0, printed)
        with open(self.out, encoding="ascii") as f:
            text = f.read()
        self.assert_clean(text, "the report")
        self.assert_clean(printed, "the output")
        r = json.loads(text)
        M.validate(r, M.REPORT)
        self.assertTrue(r["complete"])
        self.assertEqual(r["run"]["files_measured"], len(DOCS))
        self.assertEqual([f["index"] for f in r["files"]], list(range(1, len(DOCS) + 1)))
        t = r["totals"]
        self.assertEqual((t["files"], t["docling_ok"], t["lmgw_ok"], t["both_ok"]),
                         (8, 7, 6, 6))
        # the servers saw neutral names only, never a file's own
        names = self.docling.seen_names + self.lmgw.seen_names
        self.assertTrue(names)
        for n in names:
            self.assertRegex(n, r"^(doc[0-9]{6}(\.[a-z0-9]{1,8})?|probe\.pdf)$")
        # a leading dash, spaces, non-ASCII letters and a subfolder with a space are just files
        for ext in ("docx", "md", "txt", "none"):
            row = self.by_ext(r, ext)[0]
            self.assertTrue(row["docling"]["ok"] and row["lmgw"]["ok"], ext)
            self.assertEqual(row["similarity"]["jaccard"], 1.0)
        # extensions are fixed words; a file without one says so
        self.assertEqual(sorted(r["by_extension"]),
                         ["docx", "md", "none", "pdf", "pptx", "txt", "xlsx"])
        # the hash is the salted HMAC of the file's bytes, and the salt is private
        salt_path = os.path.join(self.tmp, M.SALT_NAME)
        self.assertEqual(stat.S_IMODE(os.stat(salt_path).st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.stat(self.out).st_mode), 0o600)
        with open(salt_path) as f:
            salt = bytes.fromhex(f.read().strip())
        self.assertNotIn(salt.hex(), text)
        data = DOCS["kaputt.xlsx"].encode()
        want = hmac.new(salt, data, hashlib.sha256).hexdigest()[:12]
        self.assertEqual(self.by_ext(r, "xlsx")[0]["hash"], want)
        # what was compared
        pdf = [f for f in r["files"] if f["ext"] == "pdf" and f["docling"]["route"] == "sync"][0]
        self.assertEqual(pdf["docling"]["counted"]["tables"], 1)
        self.assertEqual(pdf["docling"]["counted"]["table_cells"], 6)
        self.assertEqual(pdf["docling"]["counted"]["headings"], [0, 1, 0, 0, 0, 0])
        self.assertEqual(pdf["lmgw"]["counted"]["headings"], [1, 0, 0, 0, 0, 0])
        self.assertEqual(pdf["lmgw"]["stats"]["tables"], 1)
        self.assertEqual(pdf["lmgw"]["parts_by_source"]["native"], 1)
        self.assertEqual(pdf["lmgw"]["server_seconds"], 0.012)
        self.assertGreater(pdf["similarity"]["jaccard"], 0.9)
        self.assertEqual(pdf["adapter"]["shape"], "docling_document")
        # docling's sync wait limit: the 504 falls back to the async route for that file
        slow = [f for f in r["files"] if f["docling"]["route"] == "async_fallback"]
        self.assertEqual(len(slow), 1)
        self.assertTrue(slow[0]["docling"]["ok"])
        self.assertIsNotNone(slow[0]["docling"]["sync_wait_seconds"])
        self.assertEqual(t["docling_route"], {"sync": 7, "async_fallback": 1})
        # errors as codes
        bad = self.by_ext(r, "xlsx")[0]
        self.assertEqual((bad["docling"]["status"], bad["docling"]["errors"]), ("failure", 1))
        self.assertEqual((bad["lmgw"]["http"], bad["lmgw"]["error"]), (415, "no_reader"))
        self.assertIsNone(bad["similarity"])
        # D26: what docling-serve answers by default
        d = r["defaults"]
        self.assertEqual((d["one_file"]["answer"], d["one_file"]["http"]), ("json_document", 200))
        self.assertEqual((d["one_file"]["route"], d["several_files"]["route"]), ("sync", "sync"))
        self.assertEqual((d["several_files"]["answer"], d["several_files"]["zip_layout"],
                          d["several_files"]["zip_md_entries"]), ("zip", "flat", 2))
        self.assertNotIn("to_formats", self.docling.seen_fields[-1])
        # the environment: versions through the pattern, the rest as fixed words
        e = r["environment"]
        self.assertEqual(e["docling"]["versions"]["docling-serve"], "1.38.2")
        self.assertEqual(e["docling"]["versions"]["python"], "3.12.15")
        self.assertEqual((e["docling"]["platform"], e["docling"]["other_version_keys"]),
                         ("linux", 1))
        self.assertEqual(e["lmgw"]["version"], "0.7.0+118")
        self.assertEqual(e["lmgw"]["tools"]["poppler"], {"state": "found", "version": "26.08.0"})
        self.assertEqual(e["lmgw"]["tools"]["tesseract"], {"state": "missing", "version": "none"})
        self.assertIn("other", e["lmgw"]["tools"])
        self.assertEqual(e["lmgw"]["models"]["vision"], "not_reported")
        self.assertEqual((e["lmgw"]["tool_slots"], e["lmgw"]["pool_memory_gib"]), (3, 8.0))
        # the closing summary names the free text, and --show prints the report in full
        self.assertIn("version strings: 0.7.0+118", printed)
        self.assertIn("model alias names: (none)", printed)
        code, shown = self.call(["--show", self.out])
        self.assertEqual(code, 0)
        self.assertIn("docling-serve 1.38.2", shown)
        self.assertIn(want, shown)
        self.assert_clean(shown, "--show")

    def test_unknown_codes_become_other_and_messages_stay_out(self):
        self.serve()
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        r = self.report()
        new = self.by_ext(r, "pptx")[0]["lmgw"]
        self.assertEqual((new["status"], new["error"]), ("failed", "other"))
        self.assertEqual(new["warnings"], {"header_removed": 1, "other": 1})
        self.assertEqual(r["totals"]["lmgw_error"], {"none": 6, "no_reader": 1, "other": 1})
        self.assertIn("2 codes from the servers were not in this script's lists", printed)
        with open(self.out, encoding="ascii") as f:
            self.assert_clean(f.read(), "the report")
        self.assert_clean(printed, "the output")
        with open(os.path.join(self.tmp, M.STATE_NAME), encoding="utf-8") as f:
            state = f.read()
        self.assert_clean(state, "the state file")
        # the state file holds no server text at all: strings are kept as salted hashes
        self.assertEqual(M.free_strings_of(state.replace("\n", " ")), [])
        for version in ("1.38.2", "0.7.0+118", "26.08.0"):
            self.assertNotIn(version, state)

    def test_alias_names_are_kept_and_listed(self):
        settings = {"documents_page_reader": "local/example-vision",
                    "documents_picture_alias": "", "hold": {"active": True}}
        self.serve(lmgw={"settings": settings})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        g = self.report()["environment"]["lmgw"]
        self.assertEqual(g["models"], {"vision": "local/example-vision", "picture": "none",
                                       "asr": "not_reported"})
        self.assertEqual(g["hold_active"], True)
        self.assertNotIn("vision_pages", g)
        self.assertIn("model alias names: local/example-vision", printed)

    def test_both_models_are_read_by_their_settings_names(self):
        settings = {"documents_page_reader": "local/example-pages",
                    "documents_picture_alias": "local/example-pictures"}
        self.serve(lmgw={"settings": settings})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        g = self.report()["environment"]["lmgw"]
        self.assertEqual((g["models"]["vision"], g["models"]["picture"]),
                         ("local/example-pages", "local/example-pictures"))
        self.assertIn("local/example-pictures", printed)

    def test_alias_named_like_the_files_stops_before_any_document(self):
        settings = {"documents_page_reader": "local/bernsteinufer-ocr"}
        self.serve(lmgw={"settings": settings})
        code, printed = self.run_script()
        self.assertEqual(code, 1)
        self.assertIn("scan_file_name fired in a model alias name", printed)
        self.assert_clean(printed, "the output")
        self.assertEqual(set(self.docling.seen_names), {"probe.pdf"})
        self.assertEqual(os.listdir(self.tmp), [FOLDER_NAME])
        code, printed = self.run_script("--withhold-aliases")
        self.assertEqual(code, 0, printed)
        e = self.report()["environment"]
        self.assertEqual(e["lmgw"]["models"]["vision"], "withheld")
        self.assertEqual(e["lmgw"]["version"], "0.7.0+118")
        self.assertIn("model alias names: (none)", printed)
        with open(self.out, encoding="ascii") as f:
            self.assert_clean(f.read(), "the report")

    def test_plain_key_says_unavailable(self):
        self.serve(lmgw={"admin": False})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        g = self.report()["environment"]["lmgw"]
        self.assertEqual((g["settings_http"], g["tools_http"], g["tools"]), (401, 401, {}))
        self.assertEqual(g["models"]["vision"], "unavailable")
        self.assertEqual(g["probe_tools"], {"poppler": "26.08.0"})
        self.assert_clean(printed, "the output")


class Guard(Case):
    def good_report(self):
        self.serve()
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        return self.report()

    def test_schema_rejects_a_planted_file_name(self):
        r = self.good_report()
        os.remove(self.out)
        free = M.registry_of(r["environment"])
        none = M.Needles([])
        plants = [
            ("schema_value_kind", lambda x: x["files"][0].__setitem__(
                "ext", "Quartalsbericht Zephyrquartz 2031.pdf")),
            ("schema_unknown_key", lambda x: x["files"][0].__setitem__(
                "name", "Quartalsbericht Zephyrquartz 2031.pdf")),
            ("schema_unknown_key", lambda x: x["totals"]["lmgw_error"].__setitem__(
                "kaputt.xlsx", 1)),
            ("schema_value_kind", lambda x: x["files"][0]["lmgw"].__setitem__(
                "error", "cannot read kaputt.xlsx")),
            ("schema_value_kind", lambda x: x["environment"]["lmgw"].__setitem__(
                "version", "Quartalsbericht Zephyrquartz 2031.pdf")),
            ("schema_value_kind", lambda x: x["files"][0].__setitem__("hash", "kaputt.xlsx")),
            ("schema_value_kind", lambda x: x["files"][0].__setitem__("index", "7")),
            ("schema_value_kind", lambda x: x["files"][0].__setitem__("index", True)),
            ("schema_value_kind", lambda x: x["run"].__setitem__("timeout_seconds",
                                                                 float("nan"))),
            ("schema_value_kind", lambda x: x["environment"]["docling"]["versions"].__setitem__(
                "docling", "1.0\n")),
            ("schema_missing_key", lambda x: x["files"][0].pop("hash")),
        ]
        for rule, plant in plants:
            bad = json.loads(json.dumps(r))
            plant(bad)
            with self.assertRaises(M.GuardError) as c:
                M.write_report(bad, self.out, free, none, M.Words())
            self.assertEqual(c.exception.rule, rule)
            self.assertFalse(os.path.exists(self.out), "a rejected report was written")
            self.assertFalse(os.path.exists(self.out + ".part"))

    def test_scan_catches_what_the_schema_lets_through(self):
        r = self.good_report()
        os.remove(self.out)
        needles = M.Needles(["Zephyrquartz-Plan.pdf", "Zephyrquartz-Plan", "4711000815.pdf",
                             "4711000815"])
        words = M.Words()
        words.add(M.Words.of("Die Mondlaterne leuchtet im Nebelhafen"))

        def attempt(value, registered=True):
            bad = json.loads(json.dumps(r))
            bad["environment"]["lmgw"]["version"] = value
            free = M.registry_of(r["environment"])
            if registered:
                free.versions.add(value)
            with self.assertRaises(M.GuardError) as c:
                M.write_report(bad, self.out, free, needles, words)
            self.assertFalse(os.path.exists(self.out))
            return c.exception.rule
        # a document word that looks like a version string
        self.assertEqual(attempt("Mondlaterne"), "scan_document_word")
        self.assertEqual(attempt("1.2-nebelhafen.3"), "scan_document_word")
        self.assertEqual(attempt("xMONDLATERNEx"), "scan_document_word")
        # a file name, its stem, a word of it, a numeric name
        self.assertEqual(attempt("Zephyrquartz-Plan.pdf"), "scan_file_name")
        self.assertEqual(attempt("v1-zephyrquartz-plan"), "scan_file_name")
        self.assertEqual(attempt("zephyrquartz"), "scan_file_name")
        self.assertEqual(attempt("build-4711000815"), "scan_file_name")
        # a string no mapper let through, even a harmless one
        self.assertEqual(attempt("9.9.9", registered=False), "scan_unexpected_string")
        # and the report still passes untouched
        M.write_report(r, self.out, M.registry_of(r["environment"]), needles, words)
        self.assertTrue(os.path.exists(self.out))

    def test_scan_ignores_hashes_and_the_schemas_own_words(self):
        # a hash may spell a word, and file names may equal the report's own vocabulary
        needles = M.Needles(["index.html", "files", "pdf.pdf", "added.txt", "12.pdf", "2031"])
        words = M.Words()
        words.add(M.Words.of("added faced index files tables success other version python"))
        r = self.good_report()
        r["files"][0]["hash"] = "3added91face"
        text = M.serialise(r)
        M.final_scan(text, M.registry_of(r["environment"]), needles, words)

    def test_document_word_in_a_version_string_is_withheld_and_stays_withheld(self):
        # a word of the last file, spelt without its umlaut so that it passes as a version
        version = dict(DOCLING_VERSION, docling="Schneehugel")
        self.serve(docling={"version": version})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        self.assertIn("[8/8] 1 version string(s) held a word", printed)
        self.assert_clean(printed, "the output")
        r = self.report()
        e = r["environment"]
        self.assertEqual(e["docling"]["versions"]["docling"], "withheld")
        self.assertEqual(e["docling"]["versions"]["docling-serve"], "1.38.2")
        self.assertEqual(e["lmgw"]["version"], "0.7.0+118")
        self.assertEqual((r["run"]["strings_withheld"], r["guard"]["strings_withheld_for_words"]),
                         (True, 1))
        self.assertTrue(r["complete"])
        with open(self.out, encoding="ascii") as f:
            self.assert_clean(f.read(), "the report")
        # a resumed run reads no file's words again: the string stays withheld all the same
        before = self.docling.converts
        code, printed = self.run_script("--resume", "--overwrite")
        self.assertEqual(code, 0, printed)
        self.assertEqual(self.report()["environment"]["docling"]["versions"]["docling"],
                         "withheld")
        self.assert_clean(printed, "the output")
        with open(self.out, encoding="ascii") as f:
            self.assert_clean(f.read(), "the report")
        self.assertLess(self.docling.converts - before, 3)       # the test page, D26 done
        # --withhold-strings is still the big switch
        code, printed = self.run_script("--resume", "--overwrite", "--withhold-strings")
        self.assertEqual(code, 0, printed)
        self.assertEqual(self.report()["environment"]["lmgw"]["version"], "withheld")
        self.assertIn("version strings: (none)", printed)

    def test_file_name_in_a_version_string_stops_before_any_document(self):
        self.serve(lmgw={"version": "zephyrquartz"})
        code, printed = self.run_script()
        self.assertEqual(code, 1)
        self.assertIn("scan_file_name", printed)
        self.assert_clean(printed, "the output")
        self.assertFalse(os.path.exists(self.out))
        self.assertEqual(set(self.docling.seen_names), {"probe.pdf"})

    def test_unparseable_strings_become_fixed_words(self):
        version = dict(DOCLING_VERSION)
        version["docling"] = "Die Mondlaterne (geheim)"
        version["docling-core"] = 7
        settings = {"documents_page_reader": "Mandant Bernsteinufer!"}
        self.serve(docling={"version": version, "target_default": "Bernsteinufer"},
                   lmgw={"version": "x" * 41, "settings": settings})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        e = self.report()["environment"]
        self.assertEqual(e["docling"]["versions"]["docling"], "unparseable")
        self.assertEqual(e["docling"]["versions"]["docling-core"], "unparseable")
        self.assertEqual(e["docling"]["target_default"], "other")
        self.assertEqual(e["lmgw"]["version"], "unparseable")
        self.assertEqual(e["lmgw"]["models"]["vision"], "unparseable")
        self.assertEqual(e["lmgw"]["deployment"], "none")

    def test_console_prints_only_what_it_owns(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            M.say("{} {} {} {} {}", 3, "pdf", "0123456789ab", "Quartalsbericht.pdf",
                  M.Shown("shown"))
        self.assertEqual(err.getvalue(), "3 pdf 0123456789ab ? shown\n")
        with open(SCRIPT, "rb") as f:
            source = f.read().decode("ascii")       # ASCII only: --help prints under LANG=C
        self.assertNotIn("print(", source)
        self.assertNotIn("traceback.print", source)
        self.assertNotIn("logging", source)

    def test_unexpected_error_prints_type_and_line_only(self):
        self.serve()
        real = M.aggregate

        def broken(entries):
            raise ValueError("cannot total Quartalsbericht Zephyrquartz 2031.pdf: Mondlaterne")
        M.aggregate = broken
        try:
            code, printed = self.run_script()
        finally:
            M.aggregate = real
        self.assertEqual(code, 1)
        self.assertIn("unexpected ValueError at line", printed)
        self.assertNotIn("Traceback", printed)
        self.assert_clean(printed, "the output")
        self.assertFalse(os.path.exists(self.out))
        # the last-resort hook says the same
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            try:
                raise KeyError("Mondlaterne")
            except KeyError:
                M.quiet_hook(*sys.exc_info())
        self.assertIn("unexpected KeyError", err.getvalue())
        self.assert_clean(err.getvalue(), "the hook's output")

    def test_show_refuses_a_file_that_is_not_a_report(self):
        r = self.good_report()
        r["files"][0]["ext"] = "Quartalsbericht Zephyrquartz 2031.pdf"
        with open(self.out, "w") as f:
            json.dump(r, f)
        code, printed = self.call(["--show", self.out])
        self.assertEqual(code, 1)
        self.assert_clean(printed, "--show")


class Runs(Case):
    def test_resume_after_an_interruption(self):
        # The interruption is _thread.interrupt_main, which does nothing where SIGINT is ignored:
        # in a command started in the background (`&` in a script, scripts/long-run.sh, a non-interactive
        # background shell). Python's own handler for the test's length, as in a terminal.
        before = signal.signal(signal.SIGINT, signal.default_int_handler)
        self.addCleanup(signal.signal, signal.SIGINT, before)
        self.serve(lmgw={"interrupt_at": 4})          # the probe page, then the third file
        code, printed = self.run_script()
        self.assertEqual(code, 130, printed)
        self.assertIn("--resume", printed)
        self.assertFalse(os.path.exists(self.out))
        state = os.path.join(self.tmp, M.STATE_NAME)
        with open(state, encoding="utf-8") as f:
            done = [json.loads(line) for line in f if '"kind":"file"' in line]
        self.assertEqual(len(done), 2)
        self.assertEqual(stat.S_IMODE(os.stat(state).st_mode), 0o600)
        # a second run without a decision is refused, and nothing is lost
        code, printed = self.run_script()
        self.assertEqual(code, 2)
        self.assertIn("--resume", printed)
        # a line cut short by a crash is dropped
        with open(state, "a", encoding="utf-8") as f:
            f.write('{"kind":"file","entry":{"index":3,"ha')
        self.lmgw.interrupt_at = 0
        before = self.lmgw.converts
        code, printed = self.run_script("--resume")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertTrue(r["complete"])
        self.assertEqual((r["run"]["resumed"], r["run"]["invocations"]), (True, 2))
        self.assertEqual(r["run"]["files_measured"], len(DOCS))
        self.assertEqual([f["index"] for f in r["files"]], list(range(1, len(DOCS) + 1)))
        self.assertEqual([f["hash"] for f in r["files"][:2]],
                         [d["entry"]["hash"] for d in done])
        # the strings come back from their hashes, and nothing is reported as changed
        self.assertEqual(r["environment"]["docling"]["versions"]["docling-serve"], "1.38.2")
        self.assertEqual(r["environment"]["lmgw"]["tools"]["poppler"]["version"], "26.08.0")
        self.assertFalse(r["run"]["environment_changed_during_run"])
        # the two finished files were not sent again: the probe page and the six others
        self.assertEqual(self.lmgw.converts - before, 1 + len(DOCS) - 2)
        self.assert_clean(printed, "the output")
        # --fresh starts over with a new salt
        with open(os.path.join(self.tmp, M.SALT_NAME)) as f:
            salt = f.read()
        code, printed = self.run_script("--fresh", "--overwrite", "--limit", "1")
        self.assertEqual(code, 0, printed)
        with open(os.path.join(self.tmp, M.SALT_NAME)) as f:
            self.assertNotEqual(f.read(), salt)
        self.assertFalse(self.report()["complete"])

    def test_limit_then_resume_writes_one_whole_report(self):
        self.serve()
        code, printed = self.run_script("--limit", "3")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual((r["complete"], r["run"]["files_measured"], r["run"]["limit"]),
                         (False, 3, 3))
        code, printed = self.run_script("--resume")
        self.assertEqual(code, 2)            # the report exists already
        code, printed = self.run_script("--resume", "--overwrite")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual((r["complete"], r["run"]["files_measured"]), (True, len(DOCS)))

    def test_a_version_that_changes_between_runs_is_not_taken(self):
        self.serve()
        self.assertEqual(self.run_script("--limit", "2")[0], 0)
        self.lmgw.version = "0.7.1+119"
        self.lmgw.settings = dict(self.lmgw.settings, documents_page_reader="local/new")
        code, printed = self.run_script("--resume", "--overwrite")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual(r["environment"]["lmgw"]["version"], "changed")
        self.assertEqual(r["environment"]["lmgw"]["models"]["vision"], "not_reported")
        self.assertEqual(r["environment"]["docling"]["versions"]["docling"], "2.142.1")
        self.assertTrue(r["run"]["environment_changed_during_run"])
        self.assertNotIn("0.7.1+119", printed)
        self.assertNotIn("local/new", printed)

    def test_a_changed_state_line_drops_it_and_keeps_the_rows_before(self):
        self.serve()
        self.assertEqual(self.run_script("--limit", "3")[0], 0)
        state = os.path.join(self.tmp, M.STATE_NAME)
        with open(state, encoding="utf-8") as f:
            lines = f.read().splitlines()
        self.assertEqual([json.loads(x)["kind"] for x in lines],
                         ["header", "file", "file", "file", "defaults"])
        row = json.loads(lines[2])
        row["entry"]["ext"] = "Quartalsbericht Zephyrquartz 2031.pdf"
        with open(state, "w", encoding="utf-8") as f:
            f.write("\n".join(lines[:2] + [json.dumps(row)] + lines[3:]) + "\n")
        before = self.lmgw.converts
        code, printed = self.run_script("--resume", "--overwrite")
        self.assertEqual(code, 0, printed)
        self.assertIn("the state file had 3 lines that do not read", printed)
        self.assert_clean(printed, "the output")
        r = self.report()
        self.assertTrue(r["complete"])
        self.assertEqual(r["run"]["files_measured"], len(DOCS))
        with open(self.out, encoding="ascii") as f:
            self.assert_clean(f.read(), "the report")
        # the first file was kept; the changed one and those after it were measured again
        self.assertEqual(self.lmgw.converts - before, 1 + len(DOCS) - 1)
        with open(state, encoding="utf-8") as f:
            self.assert_clean(f.read(), "the state file")
        # a changed header still refuses
        with open(state, encoding="utf-8") as f:
            lines = f.read().splitlines()
        with open(state, "w", encoding="utf-8") as f:
            f.write("\n".join(["{}"] + lines[1:]) + "\n")
        code, printed = self.run_script("--resume", "--overwrite")
        self.assertEqual(code, 2)
        self.assertIn("--fresh", printed)
        with open(os.path.join(self.tmp, M.SALT_NAME), "w") as f:
            f.write("not a salt\n")
        code, printed = self.run_script("--resume", "--overwrite")
        self.assertEqual(code, 2)
        self.assertIn("salt file", printed)
        # --fresh moves the earlier files aside, never deletes them
        for n, suffix in ((1, ".old"), (2, ".old.2")):
            code, printed = self.run_script("--fresh", "--overwrite", "--limit", "1")
            self.assertEqual(code, 0, printed)
            self.assertIn("moved aside", printed)
            for name in (M.SALT_NAME, M.STATE_NAME):
                self.assertTrue(os.path.exists(os.path.join(self.tmp, name + suffix)), n)
        with open(os.path.join(self.tmp, M.SALT_NAME + ".old")) as f:
            self.assertEqual(f.read(), "not a salt\n")

    def test_state_of_another_folder_is_refused(self):
        self.serve()
        self.assertEqual(self.run_script("--limit", "1")[0], 0)
        other = os.path.join(self.tmp, "anderer ordner")
        os.makedirs(other)
        with open(os.path.join(other, "eins.txt"), "w") as f:
            f.write("Dachsbau\n")
        code, printed = self.run_script("--resume", "--overwrite", folder=other)
        self.assertEqual(code, 2)
        self.assertIn("--state-dir", printed)
        state_dir = os.path.join(self.tmp, "state")
        os.makedirs(state_dir)
        code, printed = self.run_script("--overwrite", "--state-dir", state_dir, folder=other)
        self.assertEqual(code, 0, printed)
        self.assertTrue(os.path.exists(os.path.join(state_dir, M.SALT_NAME)))

    def test_a_server_that_stops_answering_stops_the_run(self):
        self.serve()
        real = M.gather_environment
        mode = [True]

        def then_drop(ctx, free):
            env = real(ctx, free)          # the probe page still gets through
            self.docling.drop = mode[0]
            return env
        M.gather_environment = then_drop
        try:
            code, printed = self.run_script()
            self.assertEqual(code, 1)
            self.assertIn("--resume --retry-failed", printed)
            self.assertIn("docling reset", printed)
            self.assertFalse(os.path.exists(self.out))
            # a gateway that says the server is gone counts the same
            self.docling.drop = False
            mode[0] = "gateway"
            code, printed = self.run_script("--resume", "--retry-failed")
            self.assertEqual(code, 1)
            self.assertIn("docling http 502", printed)
            self.assert_clean(printed, "the output")
        finally:
            M.gather_environment = real
        # while it is gone, the probe page stops the next attempt before any file
        code, printed = self.run_script("--resume", "--retry-failed")
        self.assertEqual(code, 2)
        self.assertIn("not up behind that address (http 502)", printed)
        self.docling.drop = False
        code, printed = self.run_script("--resume", "--retry-failed")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual(r["totals"]["docling_transport"], {"ok": len(DOCS)})
        self.assertEqual(r["totals"]["docling_http"], [{"http": 200, "files": len(DOCS)}])
        self.assertTrue(r["complete"])

    def test_empty_folder(self):
        self.serve()
        empty = os.path.join(self.tmp, "leer")
        os.makedirs(empty)
        with open(os.path.join(empty, ".versteckt"), "w") as f:
            f.write("x")
        code, printed = self.run_script(folder=empty)
        self.assertEqual(code, 2)
        self.assertIn("no files under --dir", printed)
        self.assertFalse(os.path.exists(self.out))
        self.assertEqual(sorted(os.listdir(self.tmp)), sorted([FOLDER_NAME, "leer"]))
        self.assertEqual(self.docling.converts + self.lmgw.converts, 0)

    def test_dry_run_counts_only(self):
        code, printed = self.call(["--dir", self.folder, "--dry-run"])
        self.assertEqual(code, 0)
        self.assertIn("8 files", printed)
        self.assertRegex(printed, r"pdf\s+2")
        self.assertRegex(printed, r"none\s+1")
        self.assert_clean(printed, "the dry run")
        self.assertEqual(os.listdir(self.tmp), [FOLDER_NAME])

    def test_symlinks_are_not_followed_unless_asked(self):
        outside = os.path.join(self.tmp, "ausserhalb.txt")
        with open(outside, "w") as f:
            f.write("Glimmerpfad\n")
        os.symlink(outside, os.path.join(self.folder, "verweis.txt"))
        os.symlink(self.tmp, os.path.join(self.folder, "schleife"))
        self.serve()
        code, printed = self.run_script("--limit", "1")
        self.assertEqual(code, 0, printed)
        r = self.report()["run"]
        self.assertEqual((r["files_in_folder"], r["symlinks_skipped"]), (len(DOCS), 2))
        code, printed = self.call(["--dir", self.folder, "--dry-run", "--follow-symlinks"])
        self.assertEqual(code, 0)
        self.assertRegex(printed, r"^1[0-9] files", printed)

    def test_a_folder_that_cannot_be_listed_is_counted(self):
        locked = os.path.join(self.folder, "verschlossen")
        os.makedirs(locked)
        os.chmod(locked, 0)
        self.addCleanup(os.chmod, locked, 0o700)
        os.mkfifo(os.path.join(self.folder, "leitung"))
        if os.access(locked, os.R_OK):
            self.skipTest("this user reads any folder (root): the unreadable folder is skipped")
        code, printed = self.call(["--dir", self.folder, "--dry-run"])
        self.assertEqual(code, 0)
        self.assertIn("not read: 1 folders or entries", printed)
        self.assertIn("1 that are neither", printed)
        self.assert_clean(printed.replace("verschlossen", ""), "the dry run")
        self.assertNotIn("verschlossen", printed)
        self.serve()
        code, printed = self.run_script("--limit", "1")
        self.assertEqual(code, 0, printed)
        r = self.report()["run"]
        self.assertEqual((r["folders_unreadable"], r["special_skipped"]), (1, 1))

    def test_script_runs_isolated_as_a_file(self):
        p = subprocess.run([sys.executable, "-I", SCRIPT, "--dir", self.folder, "--dry-run"],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(b"8 files", p.stdout)
        p = subprocess.run([sys.executable, "-I", SCRIPT, "--help"], stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(b"--withhold-strings", p.stdout)

    def test_a_closed_pipe_is_not_an_error(self):
        self.serve()
        self.assertEqual(self.run_script()[0], 0)
        p = subprocess.Popen([sys.executable, "-I", SCRIPT, "--show", self.out],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        p.stdout.close()                    # `--show report.json | head`, the reader gone
        err = p.stderr.read()
        p.stderr.close()
        self.assertEqual((p.wait(), err), (0, b""))

    def test_identify_finds_the_file_where_it_lives(self):
        self.serve()
        self.assertEqual(self.run_script()[0], 0)
        row = self.by_ext(self.report(), "xlsx")[0]
        code, printed = self.call(["--dir", self.folder, "--identify", row["hash"]])
        self.assertEqual(code, 0)
        self.assertIn("kaputt.xlsx", printed)
        code, printed = self.call(["--dir", self.folder, "--identify", "0123456789ab"])
        self.assertEqual(code, 1)


class Deployments(Case):
    def test_keys_come_from_the_environment(self):
        self.serve(docling={"api_key": "example-docling-key"}, lmgw={"key": "example-lmgw-key"})
        code, printed = self.run_script()
        self.assertEqual(code, 2)
        self.assertIn("refused the key", printed)
        self.assert_clean(printed, "the output")
        self.assertEqual(os.listdir(self.tmp), [FOLDER_NAME])
        os.environ["DOCLING_KEY"] = "example-docling-key"
        os.environ["LMGW_KEY"] = "wrong"
        code, printed = self.run_script()
        self.assertEqual(code, 2)
        self.assertIn("LMGW_KEY", printed)
        os.environ["LMGW_KEY"] = "example-lmgw-key"
        code, printed = self.run_script("--also-adapter")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual((r["run"]["docling_key_sent"], r["run"]["lmgw_key_sent"]), (True, True))
        self.assertTrue(r["environment"]["docling"]["api_key_required"])
        self.assertEqual(r["totals"]["adapter_shape"], {"docling_document": len(DOCS)})
        with open(self.out, encoding="ascii") as f:
            text = f.read()
        self.assertNotIn("example-docling-key", text + printed)
        self.assertNotIn("example-lmgw-key", text + printed)
        os.environ["LMGW_KEY"] = "two words"
        self.assertEqual(self.run_script("--overwrite", "--fresh")[0], 2)

    def test_storage_target_by_default_is_asked_in_the_body(self):
        self.serve(docling={"storage_default": True})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertTrue(r["run"]["docling_target_forced"])
        self.assertEqual(r["totals"]["docling_ok"], 7)
        self.assertEqual(r["defaults"]["one_file"]["answer"], "json_other")

    def test_an_answer_without_a_document_is_not_taken_for_a_storage_target(self):
        real = Docling.answer

        def odd(handler, fields, files):
            if b"Dachsbau" in files[0][2]:
                return handler.send(200, {"kind": "SomethingElse", "serverwortlaut": True})
            return real(handler, fields, files)
        Docling.answer = odd
        try:
            self.serve()
            code, printed = self.run_script()
        finally:
            Docling.answer = real
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertFalse(r["run"]["docling_target_forced"])
        self.assertEqual(r["totals"]["docling_answer"], {"json_document": 7, "json_other": 1})
        self.assertFalse(any("target_type" in f for f in self.docling.seen_fields[-3:]))

    def test_zip_by_default_and_numbered_directories(self):
        self.serve(docling={"zip_always": True, "numbered": True})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual(r["totals"]["docling_answer"], {"zip": len(DOCS)})
        self.assertEqual(r["totals"]["docling_ok"], len(DOCS))
        d = r["defaults"]
        self.assertEqual((d["one_file"]["answer"], d["one_file"]["zip_layout"]),
                         ("zip", "numbered_directories"))
        self.assertEqual(d["several_files"]["zip_layout"], "numbered_directories")

    def test_async_route_for_every_file(self):
        self.serve()
        code, printed = self.run_script("--docling-route", "async", "--poll-wait", "1")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual(r["totals"]["docling_route"], {"async": len(DOCS)})
        self.assertEqual(r["totals"]["docling_ok"], 7)
        d = r["defaults"]
        self.assertEqual((d["one_file"]["route"], d["one_file"]["answer"]),
                         ("async", "json_document"))
        self.assertEqual((d["several_files"]["answer"], d["several_files"]["zip_layout"]),
                         ("zip", "flat"))

    def test_default_answer_past_the_sync_wait_limit(self):
        # only the slow file converts on docling's side, so the question is asked with it
        for rel in list(DOCS):
            if rel != "langsam.pdf":
                os.remove(os.path.join(self.folder, *rel.split("/")).encode("utf-8"))
        self.serve()
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        d = self.report()["defaults"]
        self.assertEqual((d["one_file"]["route"], d["one_file"]["http"], d["one_file"]["answer"]),
                         ("async_fallback", 200, "json_document"))
        self.assertFalse(d["several_files"]["run"])
        self.assertIn("no second converted file", printed)

    def test_proxy_variables_are_ignored_and_loopback_never_uses_one(self):
        os.environ["http_proxy"] = os.environ["HTTPS_PROXY"] = "http://127.0.0.1:9"
        self.serve()
        code, printed = self.run_script("--limit", "1")
        self.assertEqual(code, 0, printed)
        r = self.report()["run"]
        self.assertEqual((r["proxy_env_present"], r["proxy_env_used"]), (True, False))
        code, printed = self.run_script("--limit", "1", "--fresh", "--overwrite",
                                        "--use-env-proxy")
        self.assertEqual(code, 0, printed)
        self.assertTrue(self.report()["run"]["proxy_env_used"])

    def test_unreachable_server_stops_before_anything_is_written(self):
        self.serve()
        stop(self.lmgw)
        self.servers.remove(self.lmgw)
        code, printed = self.run_script()
        self.assertEqual(code, 2)
        self.assertIn("lmgw did not answer: refused", printed)
        self.assertEqual(os.listdir(self.tmp), [FOLDER_NAME])
        code, printed = self.call(["--docling", "ftp://docling.example", "--lmgw",
                                   self.lmgw.url, "--dir", self.folder, "--out", self.out])
        self.assertEqual(code, 2)
        code, printed = self.call(["--docling", "http://user:example@docling.example", "--lmgw",
                                   self.lmgw.url, "--dir", self.folder, "--out", self.out])
        self.assertEqual(code, 2)
        self.assertNotIn("example", printed)

    def test_report_and_state_stay_out_of_the_folder(self):
        self.serve()
        inside = os.path.join(self.folder, "report.json")
        code, printed = self.call(["--docling", self.docling.url, "--lmgw", self.lmgw.url,
                                   "--dir", self.folder, "--out", inside])
        self.assertEqual(code, 2)
        self.assertEqual(self.run_script("--state-dir", self.folder)[0], 2)
        self.assertEqual(os.listdir(self.tmp), [FOLDER_NAME])


class Tls(Case):
    def test_flags_parse_and_set_the_context(self):
        args = M.parse_args(["--docling", "https://docling.example", "--lmgw",
                             "https://lmgw.example", "--dir", self.folder, "--out", self.out,
                             "--ca-file", "/example/ca.pem", "--insecure"])
        self.assertEqual((args.ca_file, args.insecure), ("/example/ca.pem", True))
        self.assertEqual(M.tls_context(None, False).verify_mode, ssl.CERT_REQUIRED)
        self.assertTrue(M.tls_context(None, False).check_hostname)
        loose = M.tls_context(None, True)
        self.assertEqual((loose.verify_mode, loose.check_hostname), (ssl.CERT_NONE, False))
        self.serve()
        code, printed = self.run_script("--insecure", "--limit", "1")
        self.assertEqual(code, 0, printed)
        self.assertEqual((self.report()["run"]["insecure_tls"], self.report()["run"]["ca_file"]),
                         (True, False))
        code, printed = self.run_script("--ca-file", os.path.join(self.tmp, "missing.pem"),
                                        "--fresh", "--overwrite")
        self.assertEqual(code, 2)
        self.assertIn("--ca-file", printed)

    def test_private_ca_and_insecure_against_tls_servers(self):
        if not shutil.which("openssl"):
            self.skipTest("openssl is not installed: the TLS run against a private CA is "
                          "skipped")
        cert, key = os.path.join(self.tmp, "cert.pem"), os.path.join(self.tmp, "key.pem")
        made = subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
             "-subj", "/CN=convert-compare.example", "-addext", "subjectAltName=IP:127.0.0.1",
             "-keyout", key, "-out", cert], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if made.returncode != 0:
            self.skipTest("this openssl cannot make the test certificate (-addext)")
        self.serve(tls=(cert, key))
        code, printed = self.run_script("--limit", "1")
        self.assertEqual(code, 2)
        self.assertIn("did not answer: tls", printed)
        self.assertIn("--ca-file", printed)
        code, printed = self.run_script("--limit", "1", "--ca-file", cert)
        self.assertEqual(code, 0, printed)
        r = self.report()["run"]
        self.assertEqual((r["ca_file"], r["insecure_tls"], r["docling_scheme"]),
                         (True, False, "https"))
        code, printed = self.run_script("--limit", "1", "--insecure", "--fresh", "--overwrite")
        self.assertEqual(code, 0, printed)
        self.assertTrue(self.report()["run"]["insecure_tls"])


class ReviewRound(Case):
    """What the adversarial review of 2026-11-19 found, one test per finding."""

    def small_folder(self, files):
        folder = os.path.join(self.tmp, "Kleiner Ordner")
        os.makedirs(folder)
        for name, text in files:
            with open(os.path.join(folder, name), "w", encoding="utf-8") as f:
                f.write(text)
        return folder

    def test_files_are_sent_in_pieces_never_read_whole(self):
        self.serve()
        root = os.fsencode(os.path.realpath(self.folder))

        class Pieces(object):
            def __init__(self, f):
                self.f = f

            def read(self, n=-1):
                if n is None or n < 0 or n > M.CHUNK:
                    raise MemoryError("Quartalsbericht Zephyrquartz 2031.pdf read whole")
                return self.f.read(n)

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                self.f.close()

        def guarded(path, mode="r", *a, **kw):
            f = open(path, mode, *a, **kw)
            inside = isinstance(path, (str, bytes)) and os.fsencode(path).startswith(root)
            return Pieces(f) if inside and "b" in mode else f
        real_chunk = M.CHUNK
        M.open, M.CHUNK = guarded, 16
        try:
            code, printed = self.run_script("--also-adapter")
        finally:
            del M.open
            M.CHUNK = real_chunk
        self.assertEqual(code, 0, printed)
        self.assertNotIn("unexpected", printed)
        r = self.report()
        self.assertEqual((r["totals"]["docling_ok"], r["totals"]["lmgw_ok"]), (7, 6))
        # every byte arrived, framed right, at all three routes
        sent = set(text.encode("utf-8") for text in DOCS.values())
        self.assertLessEqual(sent, set(self.docling.seen_files))
        self.assertLessEqual(sent, set(self.lmgw.seen_files))
        self.assertEqual(r["totals"]["adapter_shape"], {"docling_document": len(DOCS)})

    def test_a_file_that_fails_here_is_recorded_and_the_run_goes_on(self):
        folder = self.small_folder([
            ("a archiv.docx", "BROKENZIP Zinnoberkraut\n"),
            ("b riesig.pdf", "SPEICHER Zinnoberkraut\n"),
            ("c gut.txt", "Ganz gewoehnlicher Text\n")])
        self.serve(docling={"zip_always": True})
        real = M.counted

        def boom(md):
            if "SPEICHER" in md:
                raise MemoryError("b riesig.pdf: Zinnoberkraut")
            return real(md)
        M.counted = boom
        try:
            code, printed = self.run_script(folder=folder)
            self.assertEqual(code, 0, printed)
            # and a resumed run does not stop at that file either
            code, again = self.run_script("--resume", "--overwrite", folder=folder)
            self.assertEqual(code, 0, again)
        finally:
            M.counted = real
        for text in (printed, again):
            self.assertNotIn("zinnoberkraut", text.casefold())
            self.assertNotIn("riesig", text)
        self.assertIn("[2/3] pdf   not measured: unexpected MemoryError at line", printed)
        self.assertIn("recorded as failed: memory", printed)
        r = self.report()
        broken, big, good = r["files"]
        self.assertEqual((broken["failed"], broken["docling"]["answer"], broken["docling"]["ok"]),
                         ("none", "zip", False))
        self.assertEqual((big["failed"], big["docling"], big["lmgw"]), ("memory", None, None))
        self.assertTrue(good["docling"]["ok"])
        self.assertEqual(r["totals"]["failed"], {"memory": 1})
        self.assertTrue(r["complete"])
        code, shown = self.call(["--show", self.out])
        self.assertEqual(code, 0)
        self.assertIn("not measured: failed memory", shown)
        # once memory is there, --retry-failed measures it, and a plain resume keeps that
        self.assertEqual(self.run_script("--resume", "--retry-failed", "--overwrite",
                                         folder=folder)[0], 0)
        self.assertEqual(self.run_script("--resume", "--overwrite", folder=folder)[0], 0)
        r = self.report()
        self.assertEqual((r["files"][1]["failed"], r["totals"]["failed"]), ("none", {}))
        code, printed = self.call(["--dir", folder, "--dry-run"])
        self.assertIn("largest file: up to 1 KiB", printed)

    def test_chars_and_lines_are_rounded_per_file_and_exact_in_the_sums(self):
        self.assertEqual([M.two_digits(n) for n in (0, 7, 99, 100, 157, 57428, 9960)],
                         [0, 7, 99, 100, 160, 57000, 10000])
        long_text = "".join("Zeile {} am Kieselweg\n".format(n) for n in range(157))
        folder = self.small_folder([("eins.txt", long_text), ("zwei.txt", "Kurz.\n")])
        self.serve()
        self.assertEqual(self.run_script(folder=folder)[0], 0)
        r = self.report()
        with open(os.path.join(self.tmp, M.STATE_NAME), encoding="utf-8") as f:
            stored = [json.loads(x)["entry"] for x in f if '"kind":"file"' in x]
        first = r["files"][0]["docling"]["counted"]
        exact = stored[0]["docling"]["counted"]
        self.assertEqual((exact["lines"], first["lines"]), (157, 160))
        self.assertEqual(first["chars"], M.two_digits(exact["chars"]))
        self.assertNotEqual(first["chars"], exact["chars"])
        self.assertEqual(r["totals"]["docling_counted"]["chars"],
                         sum(e["docling"]["counted"]["chars"] for e in stored))
        self.assertEqual(r["totals"]["lmgw_counted"]["lines"], 158)
        # a sum over a single file is that file's count: rounded like its entry
        self.assertEqual(self.run_script("--fresh", "--overwrite", "--limit", "1",
                                         folder=folder)[0], 0)
        self.assertEqual(self.report()["totals"]["docling_counted"]["lines"], 160)
        self.assertEqual(self.report()["by_extension"]["txt"]["docling_counted"]["lines"], 160)

    def test_three_unanswered_files_are_checked_then_retried_then_kept(self):
        folder = self.small_folder(
            [("a{}.txt".format(n), "VERLOREN {}\n".format(n)) for n in range(3)] +
            [("b{}.txt".format(n), "Angekommen {}\n".format(n)) for n in range(2)])
        self.serve(docling={"drop_marker": b"VERLOREN"})
        code, printed = self.run_script(folder=folder)
        self.assertEqual(code, 0, printed)
        self.assertIn("3 files in a row got no answer from a server; sending the test page",
                      printed)
        self.assertIn("both answered it", printed)
        r = self.report()
        self.assertEqual((r["run"]["test_page_resent"], r["totals"]["docling_transport"]),
                         (1, {"reset": 3, "ok": 2}))
        self.assertTrue(r["complete"])
        self.docling.drop_marker = None
        self.assertEqual(self.run_script("--resume", "--retry-failed", "--overwrite",
                                         folder=folder)[0], 0)
        self.assertEqual(self.report()["totals"]["docling_ok"], 5)
        # the documented last step, a plain resume, reads the retried rows, not the old ones
        before = self.docling.converts
        code, printed = self.run_script("--resume", "--overwrite", folder=folder)
        self.assertEqual(code, 0, printed)
        self.assertEqual(self.report()["totals"]["docling_ok"], 5)
        self.assertEqual(self.docling.converts - before, 1)      # the test page only

    def test_default_answer_for_two_is_asked_once_two_files_converted(self):
        folder = self.small_folder(
            [("a{}.txt".format(n), "DOCLINGFAIL {}\n".format(n)) for n in range(2)] +
            [("b{}.txt".format(n), "Angekommen {}\n".format(n)) for n in range(3)])
        self.serve()
        self.assertEqual(self.run_script("--limit", "3", folder=folder)[0], 0)
        d = self.report()["defaults"]
        self.assertEqual((d["one_file"]["run"], d["several_files"]["run"]), (True, False))
        code, printed = self.run_script("--resume", "--overwrite", folder=folder)
        self.assertEqual(code, 0, printed)
        self.assertIn("asking docling-serve what it answers with no option: two files", printed)
        d = self.report()["defaults"]
        self.assertEqual((d["one_file"]["answer"], d["several_files"]["run"],
                          d["several_files"]["answer"]), ("json_document", True, "zip"))

    def test_line_ratio_work_is_bounded(self):
        same = "\n".join(["value 12"] * 2000)
        every_second = "\n".join(["label x", "value 12"] * 1000)
        t0 = time.monotonic()
        jac, ratio, skipped = M.similarity(same, every_second)
        self.assertLess(time.monotonic() - t0, 1.0)
        self.assertEqual((ratio, skipped), (None, "work_bound"))
        prose = ["Satz {} vom Reiherstieg".format(n) for n in range(5000)]
        self.assertEqual(M.similarity("\n".join(prose), "\n".join(prose[::-1]))[2], "none")
        small = M.similarity("\n".join(["value 12"] * 100),
                             "\n".join(["label x", "value 12"] * 50))
        self.assertEqual(small[2], "none")
        self.assertIsNotNone(small[1])
        self.assertEqual(M.line_ratio_work(["a", "a", "b"], ["a", "b", "b", "c"]), 4 * 3)

    def test_embedded_pictures_neither_count_as_text_nor_trip_the_guard(self):
        self.serve(docling={"embed_picture": True}, lmgw={"version": "7.0.2-static"})
        code, printed = self.run_script()
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertEqual(r["environment"]["lmgw"]["version"], "7.0.2-static")
        self.assertEqual(r["guard"]["strings_withheld_for_words"], 0)
        row = [f for f in r["files"] if f["ext"] == "docx"][0]
        self.assertEqual(row["docling"]["counted"]["pictures"], 1)
        self.assertLess(row["docling"]["counted"]["chars"], 300)
        self.assertEqual(M.strip_data_uris("a ![x](data:image/png;base64,QUJD) b"),
                         "a ![x](data:) b")
        self.assertEqual(M.counted('<img src="data:image/png;base64,QUJD">')["pictures"], 1)

    def test_show_refuses_text_that_is_not_the_scripts_own(self):
        self.serve()
        self.assertEqual(self.run_script()[0], 0)
        with open(self.out, encoding="ascii") as f:
            text = f.read()
        self.assertEqual(self.call(["--show", self.out])[0], 0)
        planted = text.replace('{\n "report": "convert-compare"',
                               '{\n "report": "GEHEIM Vertrag mit ACME",\n'
                               ' "report": "convert-compare"', 1)
        digits = re.sub(r'("jaccard":1\.0)', r"\g<1>0000000000000000001", text, count=1)
        self.assertNotEqual(planted, text)
        self.assertNotEqual(digits, text)
        for bad in (planted, digits):
            with open(self.out, "w", encoding="ascii") as f:
                f.write(bad)
            code, shown = self.call(["--show", self.out])
            self.assertEqual(code, 1)
            self.assertNotIn("GEHEIM", shown)
            self.assertNotIn("jaccard", shown)

    def test_a_test_page_answered_by_a_login_page_stops_before_any_document(self):
        self.serve(docling={"login_page": True})
        code, printed = self.run_script()
        self.assertEqual(code, 2)
        self.assertIn("WARNING: docling-serve answered the test page with http 200 but did not "
                      "convert it", printed)
        self.assertIn("--accept-test-page", printed)
        self.assertEqual(set(self.docling.seen_names), {"probe.pdf"})
        self.assertEqual(os.listdir(self.tmp), [FOLDER_NAME])
        self.assert_clean(printed, "the output")
        code, printed = self.run_script("--accept-test-page")
        self.assertEqual(code, 0, printed)
        r = self.report()
        self.assertTrue(r["run"]["test_page_accepted"])
        self.assertEqual(r["totals"]["docling_answer"], {"not_json": len(DOCS)})

    def test_many_numbered_file_names_are_checked_fast(self):
        # as list_folder notes them: each file's name and its stem
        needles = M.Needles([name for n in range(100000)
                             for name in ("{:08d}.pdf".format(n), "{:08d}".format(n))])
        strings = ["1.38.2", "0.7.0+118", "26.08.0", "local/example-vision:8b"]
        t0 = time.monotonic()
        M.check_names(strings, {"local/example-vision:8b"}, needles)
        self.assertLess(time.monotonic() - t0, 1.0)
        for hit in ("build-00004711", "00099999", "v00004711.x"):
            with self.assertRaises(M.GuardError):
                M.check_names([hit], set(), needles)
        M.check_names(["1-100004711"], set(), needles)        # a digit before: another number

    def test_errors_while_dropping_an_object_print_nothing(self):
        class Leaky(object):
            def __repr__(self):
                return "<Leaky Quartalsbericht Zephyrquartz 2031.pdf>"

            def __del__(self):
                raise ValueError("Mondlaterne")
        err = io.StringIO()
        hook = getattr(sys, "unraisablehook", None)
        try:
            with contextlib.redirect_stderr(err):
                M.quiet_output()
                leaky = Leaky()
                del leaky
                gc.collect()
                M.say("still printed: {}", 3)
        finally:
            if hook is not None:
                sys.unraisablehook = hook
        self.assertEqual(err.getvalue(), "still printed: 3\n")

    def test_an_entry_the_whitelist_refuses_is_dropped_not_written(self):
        self.serve()
        real = M.lmgw_convert

        def planting(ctx, name, data, free=None, tool_versions=None):
            rec, md = real(ctx, name, data, free, tool_versions)
            if name.startswith("doc000003"):
                rec["format"] = "Quartalsbericht Zephyrquartz"
            return rec, md
        M.lmgw_convert = planting
        try:
            code, printed = self.run_script()
        finally:
            M.lmgw_convert = real
        self.assertEqual(code, 0, printed)
        self.assertIn("(privacy rule schema_value_kind)", printed)
        self.assert_clean(printed, "the output")
        r = self.report()
        self.assertEqual(r["files"][2]["failed"], "guard")
        with open(self.out, encoding="ascii") as f:
            self.assert_clean(f.read(), "the report")
        with open(os.path.join(self.tmp, M.STATE_NAME), encoding="utf-8") as f:
            self.assert_clean(f.read(), "the state file")

    def test_text_that_parses_back_to_other_kinds_is_refused(self):
        self.serve()
        self.assertEqual(self.run_script()[0], 0)
        r = self.report()
        os.remove(self.out)
        real = M.serialise
        M.serialise = lambda report: real(report).replace('"index":1,', '"index":true,', 1)
        try:
            with self.assertRaises(M.GuardError) as c:
                M.write_report(r, self.out, M.registry_of(r["environment"]), M.Needles([]),
                               M.Words())
        finally:
            M.serialise = real
        self.assertEqual(c.exception.rule, "schema_value_kind")
        self.assertFalse(os.path.exists(self.out))

    def test_scan_refuses_non_ascii_and_stray_text(self):
        for text, rule in (('{"a": "Gr\u00fc\u00dfe"}', "scan_non_ascii"),
                           ('{"a": 1}\u00e9', "scan_non_ascii"),
                           ('{"a": 1, Mondlaterne}', "scan_stray_text"),
                           ('{"a": NaN}', "scan_stray_text"),
                           ('{"a": 1} // Nebelhafen', "scan_stray_text")):
            with self.assertRaises(M.GuardError) as c:
                M.free_strings_of(text)
            self.assertEqual(c.exception.rule, rule, text)
        self.assertEqual(M.free_strings_of('{"files": [1, -2.5, true, null], "index": "1.38.2"}'),
                         ["1.38.2"])


class Units(unittest.TestCase):
    def test_base_url(self):
        self.assertEqual(M.base_url("http://127.0.0.1:8001/v1/", "--lmgw"),
                         ("http://127.0.0.1:8001", "http", True))
        self.assertEqual(M.base_url("https://docling.example/serve", "--docling"),
                         ("https://docling.example/serve", "https", False))
        for bad in ("docling.example:5001", "ftp://docling.example", "http://",
                    "https://docling.example/?key=example"):
            with self.assertRaises(M.Stop):
                M.base_url(bad, "--docling")

    def test_extension_and_size_bucket(self):
        self.assertEqual(M.extension("a/b/Bericht.PDF"), ("pdf", "pdf"))
        self.assertEqual(M.extension("notes"), ("none", ""))
        self.assertEqual(M.extension("plan.geheimprojekt"), ("other", ""))
        self.assertEqual(M.extension("drawing.dwg"), ("other", "dwg"))
        self.assertEqual(M.extension("a.b c"), ("other", ""))
        self.assertEqual([M.size_bucket(n) for n in (0, 1, 1024, 1025, 3000, 2 ** 20, 2 ** 20 + 1)],
                         [0, 1, 1, 2, 4, 1024, 2048])
        self.assertEqual(M.upload_name(12, "pdf"), "doc000012.pdf")
        self.assertEqual(M.upload_name(3, ""), "doc000003")

    def test_mappers(self):
        self.assertEqual(M.enum("no_reader", M.LMGW_ERRORS), "no_reader")
        # WP4's codes, formats and tool: a report names them instead of "other".
        for code in ("ocr_low_confidence", "ocr_page_reduced", "ocr_page_turned",
                     "ocr_language_missing"):
            self.assertEqual(M.enum(code, M.LMGW_WARNINGS), code)
        self.assertEqual(M.enum("invalid_option", M.LMGW_ERRORS), "invalid_option")
        for fmt in ("html", "epub", "eml", "xls", "image"):
            self.assertEqual(M.enum(fmt, M.LMGW_FORMATS), fmt)
        for tool in ("reader", "tesseract"):
            self.assertEqual(M.enum(tool, M.TOOL_IDS), tool)
        # A file the page-reading model was to read fails with the model's
        # own code, and a web source with its own: none of them is "other".
        for code in ("model_held", "model_not_allowed", "model_failed", "source_refused",
                     "source_unreachable", "source_timeout", "source_too_large",
                     "source_http_status"):
            self.assertEqual(M.enum(code, M.LMGW_ERRORS), code)
        for code in ("model_cannot_see", "model_answer_malformed", "page_scaled"):
            self.assertEqual(M.enum(code, M.LMGW_WARNINGS), code)
        self.assertEqual(M.enum("brand_new", M.LMGW_ERRORS), "other")
        self.assertEqual(M.enum(None, M.LMGW_ERRORS), "none")
        self.assertEqual(M.enum(["no_reader"], M.LMGW_ERRORS), "other")
        self.assertEqual(M.enum(None, M.TOOL_IDS), "other")
        self.assertEqual((M.count(3), M.count(-1), M.count(True), M.count("3"), M.count(2.0)),
                         (3, None, None, None, None))
        self.assertEqual((M.num(0.123456), M.num(float("inf")), M.num("1"), M.num(True)),
                         (0.1235, None, None, None))
        self.assertEqual((M.http_code(200), M.http_code(99), M.http_code("200")),
                         (200, None, None))
        free = M.FreeStrings()
        self.assertEqual(free.version("1.38.2"), "1.38.2")
        self.assertEqual(free.version("1.38.2\n"), "unparseable")
        self.assertEqual(free.version("0123456789ab"), "unparseable")
        self.assertEqual(free.version("a b"), "unparseable")
        self.assertEqual(free.version(None), "none")
        self.assertEqual(free.alias("local/example:8b"), "local/example:8b")
        self.assertEqual(free.alias("two words"), "unparseable")
        self.assertEqual(free.all(), {"1.38.2", "local/example:8b"})

    def test_markdown_measures(self):
        md = "# A\n\n## B\n\ntext <!-- image --> ![x](y.png)\n\n| a | b |\n|---|---|\n| 1 | 2 |\n"
        c = M.counted(md)
        self.assertEqual((c["headings"], c["tables"], c["table_cells"], c["pictures"]),
                         ([1, 1, 0, 0, 0, 0], 1, 4, 2))
        self.assertEqual(M.similarity(md, md), (1.0, 1.0, "none"))
        jac, ratio, _ = M.similarity("alpha beta gamma", "entirely different words")
        self.assertLess(jac, 0.2)
        self.assertEqual(ratio, 0.0)
        real = M.LINE_RATIO_MAX_WORK
        M.LINE_RATIO_MAX_WORK = 3
        try:
            self.assertEqual(M.similarity("a\nb", "a\nb")[1:], (None, "work_bound"))
        finally:
            M.LINE_RATIO_MAX_WORK = real

    def test_zip_shapes(self):
        def archive(names):
            buf = io.BytesIO()
            with zipfile.ZipFile(buf, "w") as z:
                for n in names:
                    z.writestr(n, "x")
            return buf.getvalue()
        self.assertEqual(M.zip_shape(archive(["a.md", "b.md", "artifacts/p.png"]))[:3],
                         ("flat", 3, 2))
        self.assertEqual(M.zip_shape(archive(["000000/a.md", "000001/b.md"]))[:3],
                         ("numbered_directories", 2, 2))
        self.assertEqual(M.zip_shape(archive(["out/a.md"]))[:3], ("other", 1, 1))
        self.assertEqual(M.zip_shape(archive([]))[:3], ("none", 0, 0))
        self.assertEqual(M.zip_shape(b"not a zip")[:3], ("other", 0, 0))

    def test_probe_page_is_a_pdf_with_a_right_xref(self):
        pdf = M.probe_pdf()
        xref = int(re.search(rb"startxref\n(\d+)", pdf).group(1))
        self.assertEqual(pdf[xref:xref + 4], b"xref")
        for n, off in enumerate(re.findall(rb"(\d{10}) 00000 n", pdf), 1):
            self.assertTrue(pdf[int(off):].startswith("{} 0 obj".format(n).encode()))

    def test_vocabulary_and_schema_agree(self):
        for word in ("pdf", "other", "no_reader", "withheld", "docling-serve", "files"):
            self.assertIn(word, M.VOCAB)
        for word in M.VOCAB:
            self.assertRegex(word, r"^[a-z0-9_-]+$")
        self.assertEqual(M.MIN_PYTHON, (3, 6))


if __name__ == "__main__":
    unittest.main()
