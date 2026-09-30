#!/usr/bin/env python3
"""Mock audio.cpp spec catalog + Hugging Face hub, in one origin.

Point a dev gateway at it with both overrides:

    LMGW_AUDIO_CATALOG_ENDPOINT=http://127.0.0.1:8902 \
    HF_ENDPOINT=http://127.0.0.1:8902 target/debug/examples/headless 127.0.0.1:8901

It answers the four requests the catalog flow makes, so the whole
refresh → install → serve path can be exercised without touching GitHub or
huggingface.co (and without multi-GB downloads):

  GET /repos/<repo>/git/trees/main?recursive=1      the spec listing
  GET /<repo>/main/model_specs/<family>.json        one spec
  GET /api/models/<owner>/<name>/tree/main          HF repo file list
  GET|HEAD /<owner>/<name>/resolve/main/<file>      the "weights"

Environment:
  SIZE=<bytes>   size of each fake weights file (default 6 MiB)
  SLOW=<s>       seconds to spread one download over (default 8) — this is
                 what makes the in-progress (amber) state catchable

Usage: mock-audio-catalog.py [port=8902]
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SIZE = int(os.environ.get("SIZE", 6 * 1024 * 1024))
SLOW = float(os.environ.get("SLOW", "8"))
CHUNKS = 40

SPECS = {
    "pocket_tts": {
        "family": "pocket_tts",
        "display_name": "Pocket TTS",
        "description": "Compact multilingual text-to-speech with voice cloning.",
        "category": "tts",
        "tasks": ["tts", "clone"],
        "modes": ["offline", "streaming"],
        "languages": ["en", "de", "fr"],
        "ui": {"recommended_package": "pocket_tts_english_q8_0"},
        "package_defaults": {
            "download": {"kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-GGUF"}
        },
        "packages": [
            {
                "id": "pocket_tts_english_q8_0",
                "display_name": "English · q8_0",
                "default": True,
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "pocket-tts/english",
                "files": ["english/model.gguf", "english/voices.bin"],
            },
            {
                "id": "pocket_tts_multi_f16",
                "display_name": "Multilingual · f16",
                "format": "gguf",
                "precision": "f16",
                "target_directory": "pocket-tts/multi",
                "files": ["multi/model.gguf"],
            },
        ],
    },
    "qwen3_asr": {
        "family": "qwen3_asr",
        "display_name": "Qwen3 ASR 0.6B",
        "description": "Streaming speech recognition, 0.6B parameters.",
        "category": "asr",
        "tasks": ["asr"],
        "modes": ["streaming", "offline"],
        "languages": ["en", "zh", "de"],
        "package_defaults": {
            "download": {"kind": "huggingface_snapshot", "repo": "audio-cpp/Qwen3-ASR-GGUF"}
        },
        "packages": [
            {
                "id": "qwen3_asr_q4_k_m",
                "display_name": "q4_K_M",
                "default": True,
                "format": "gguf",
                "precision": "q4_K_M",
                "target_directory": "qwen3-asr",
                "files": ["model-q4_k_m.gguf"],
            },
            {
                "id": "qwen3_asr_f16",
                "display_name": "f16",
                "format": "gguf",
                "precision": "f16",
                "target_directory": "qwen3-asr",
                "files": ["model-f16.gguf"],
            },
        ],
    },
    "fish_audio_s2": {
        "family": "fish_audio_s2",
        "display_name": "Fish Audio S2 Pro",
        "description": "Expressive TTS with reference-clip voice cloning.",
        "category": "tts",
        "tasks": ["tts", "clone"],
        "modes": ["offline"],
        "languages": ["en", "ja", "zh"],
        "package_defaults": {
            "download": {"kind": "huggingface_snapshot", "repo": "audio-cpp/Fish-Audio-S2-GGUF"}
        },
        "packages": [
            {
                "id": "fish_audio_s2_q8_0",
                "display_name": "q8_0",
                "default": True,
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "fish-audio-s2",
                "files": ["fish-s2-q8_0.gguf"],
            }
        ],
    },
    "musicgen_small": {
        "family": "musicgen_small",
        "display_name": "MusicGen Small",
        "description": "Text-to-music generation, small checkpoint.",
        "category": "gen",
        "tasks": ["music"],
        "modes": ["offline"],
        "languages": [],
        "packages": [
            {
                "id": "musicgen_small_q8_0",
                "display_name": "q8_0 (no source in spec)",
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "musicgen",
                "files": ["musicgen-small-q8_0.gguf"],
            }
        ],
    },
}

# HF repo → files, derived from the specs so the two views always agree.
REPO_FILES: dict[str, list[str]] = {}
for spec in SPECS.values():
    default_repo = spec.get("package_defaults", {}).get("download", {}).get("repo")
    for pkg in spec["packages"]:
        repo = pkg.get("download", {}).get("repo", default_repo)
        if repo:
            REPO_FILES.setdefault(repo, []).extend(pkg["files"])


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quieter than the default
        sys.stderr.write("mock-audio-catalog: " + fmt % args + "\n")

    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _404(self):
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _route(self):
        p = self.path.split("?", 1)[0]
        # Spec listing (git trees API).
        if p.endswith("/git/trees/main"):
            return "trees", None
        # One spec file: /<owner>/<repo>/main/model_specs/<family>.json
        if "/main/model_specs/" in p:
            return "spec", p.rsplit("/", 1)[-1].removesuffix(".json")
        # HF file list: /api/models/<owner>/<name>/tree/main
        if p.startswith("/api/models/") and "/tree/main" in p:
            return "hf_tree", p[len("/api/models/"):].split("/tree/main")[0]
        # HF file: /<owner>/<name>/resolve/main/<file>
        if "/resolve/main/" in p:
            repo, file = p.lstrip("/").split("/resolve/main/", 1)
            return "hf_file", (repo, file)
        return None, None

    def do_HEAD(self):
        kind, arg = self._route()
        if kind == "hf_file":
            self.send_response(200)
            self.send_header("Content-Length", str(SIZE))
            self.send_header("ETag", '"mock-etag-v1"')
            self.end_headers()
        else:
            self._404()

    def do_GET(self):
        kind, arg = self._route()
        if kind == "trees":
            self._json({"tree": [{"path": "README.md"}] + [
                {"path": f"model_specs/{name}.json"} for name in SPECS
            ]})
        elif kind == "spec" and arg in SPECS:
            self._json(SPECS[arg])
        elif kind == "hf_tree":
            files = REPO_FILES.get(arg, [])
            self._json([
                {"type": "file", "path": f, "size": SIZE} for f in files
            ])
        elif kind == "hf_file":
            repo, file = arg
            if file not in REPO_FILES.get(repo, []):
                return self._404()
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(SIZE))
            self.send_header("ETag", '"mock-etag-v1"')
            self.end_headers()
            chunk = b"GGUF\x00" + b"\0" * (SIZE // CHUNKS - 5)
            sent = 0
            for _ in range(CHUNKS):
                if sent + len(chunk) > SIZE:
                    chunk = chunk[: SIZE - sent]
                self.wfile.write(chunk)
                self.wfile.flush()
                sent += len(chunk)
                if sent >= SIZE:
                    break
                time.sleep(SLOW / CHUNKS)
            if sent < SIZE:
                self.wfile.write(b"\0" * (SIZE - sent))
        else:
            self._404()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8902
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
