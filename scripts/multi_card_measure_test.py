#!/usr/bin/env python3
"""Offline test of scripts/multi-card-measure.py (WP0 of the device-ledger design): the parsing
of lmgw's /api/vram, nvidia-smi, llama-server's log and a GGUF header, llama.cpp's layer split as
the script models it, the plans and their free-memory checks against four test cards, a dry run that must write nothing, and a run and an interrupted run against a fake
lmgw and a fake host that must leave no row behind. No network, no lmgw, no GPU.

Usage: python3 scripts/multi_card_measure_test.py   (ci/check.sh runs it)
"""

import contextlib
import importlib.util
import io
import json
import os
import struct
import tempfile
import unittest
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("multi_card_measure",
                                               os.path.join(HERE, "multi-card-measure.py"))
M = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(M)

MIB = 2**20

# Four test cards: NVML order, totals and the free memory as test inputs. The ids and PCI
# addresses are made up.
UUIDS = [f"GPU-0000000{i}-aaaa-bbbb-cccc-00000000000{i}" for i in range(4)]
PCI = ["00000000:17:00.0", "00000000:31:00.0", "00000000:B1:00.0", "00000000:CA:00.0"]
ADA = "NVIDIA RTX 6000 Ada Generation"
BW = "NVIDIA RTX PRO 6000 Blackwell Workstation Edition"
NAMES = [ADA, ADA, BW, ADA]
TOTAL = [49140, 49140, 97887, 49140]
FREE = [7641, 12220, 22690, 16143]


def vram_fixture(kept=None):
    devs = []
    for i in range(4):
        d = {"id": UUIDS[i], "index": i, "name": NAMES[i], "total_bytes": TOTAL[i] * MIB,
             "used_bytes": (TOTAL[i] - FREE[i]) * MIB, "free_bytes": FREE[i] * MIB,
             "lmgw_share_bytes": 0, "outside_share_bytes": (TOTAL[i] - FREE[i]) * MIB,
             "share_unknown": None, "kept_free": {"starting_bytes": 0, "image_peak_bytes": 0,
                                                  "pending_load_bytes": 0}, "resident": []}
        if kept and i in kept:
            d["kept_free"]["starting_bytes"] = kept[i] * MIB
        devs.append(d)
    return {"enabled": True, "active": True, "telemetry": "nvml", "telemetry_ok": True,
            "devices": devs, "resident": [], "queue": []}


def smi_gpus_text(free=None):
    free = free or FREE
    return "\n".join(f"{i}, {UUIDS[i]}, {PCI[i]}, {NAMES[i]}, {TOTAL[i]}, {TOTAL[i] - free[i]}, {free[i]}"
                     for i in range(4)) + "\n"


# llama-server's lines as remembered from llama.cpp (late 2025); the parser is tolerant and the
# live run keeps the raw lines, so a format that moved shows in the report.
LOG_SPLIT2 = "\n".join([
    "ggml_cuda_init: found 2 CUDA devices:",
    f"  Device 0: {BW}, compute capability 12.0, VMM: yes",
    f"  Device 1: {ADA}, compute capability 8.9, VMM: yes",
    "build: 12352 (abcdef0) with cc (GCC) 13.3.0 for x86_64-linux-gnu",
    f"llama_model_load_from_file_impl: using device CUDA0 ({BW}) (0000:b1:00.0) - 22000 MiB free",
    f"llama_model_load_from_file_impl: using device CUDA1 ({ADA}) (0000:ca:00.0) - 15800 MiB free",
    *[f"load_tensors: layer {il:3d} assigned to device {'CUDA0' if il < 15 else 'CUDA1'}, is_swa = 0"
      for il in range(36)],
    "load_tensors: offloading 36 repeating layers to GPU",
    "load_tensors: offloading output layer to GPU",
    "load_tensors: offloaded 37/37 layers to GPU",
    "load_tensors:        CUDA0 model buffer size =   850.00 MiB",
    "load_tensors:        CUDA1 model buffer size =  1500.00 MiB",
    "load_tensors:   CPU_Mapped model buffer size =   304.00 MiB",
    "llama_kv_cache:      CUDA0 KV buffer size =   960.00 MiB",
    "llama_kv_cache:      CUDA1 KV buffer size =  1344.00 MiB",
    "llama_context:      CUDA0 compute buffer size =   120.00 MiB",
    "llama_context:      CUDA1 compute buffer size =   301.75 MiB",
    "llama_context:  CUDA_Host compute buffer size =    40.00 MiB",
    "srv    load_model: loading draft model '/models/Qwen3-0.6B-Q8_0.gguf'",
    "load_tensors:        CUDA0 model buffer size =   300.00 MiB",
    "load_tensors:        CUDA1 model buffer size =   310.00 MiB",
    "main: server is listening on http://0.0.0.0:8080",
])


def gguf_bytes():
    """A tiny GGUF v3: qwen3, two layers, a token list to skip, five tensors whose data are
    `sizes` long (aligned to 32), the embedding tied (no output.weight)."""
    def s(x):
        b = x.encode()
        return struct.pack("<Q", len(b)) + b

    kv = [
        s("general.architecture") + struct.pack("<I", 8) + s("qwen3"),
        s("qwen3.block_count") + struct.pack("<I", 4) + struct.pack("<I", 2),
        s("general.alignment") + struct.pack("<I", 4) + struct.pack("<I", 32),
        s("tokenizer.ggml.tokens") + struct.pack("<I", 9) + struct.pack("<I", 8)
        + struct.pack("<Q", 3) + s("a") + s("bb") + s("ccc"),
        s("tokenizer.ggml.scores") + struct.pack("<I", 9) + struct.pack("<I", 6)
        + struct.pack("<Q", 3) + struct.pack("<fff", 0.0, 1.0, 2.0),
    ]
    tensors = [("token_embd.weight", 1024), ("blk.0.attn_q.weight", 224),
               ("blk.0.ffn_up_exps.weight", 320), ("blk.1.attn_q.weight", 224),
               ("output_norm.weight", 32)]
    infos, off = [], 0
    for name, size in tensors:
        infos.append(s(name) + struct.pack("<I", 1) + struct.pack("<Q", size // 4)
                     + struct.pack("<I", 0) + struct.pack("<Q", off))
        off += size
    head = b"GGUF" + struct.pack("<I", 3) + struct.pack("<QQ", len(tensors), len(kv))
    head += b"".join(kv) + b"".join(infos)
    head += b"\0" * ((32 - len(head) % 32) % 32)
    return head + b"\x01" * off


def args(*extra):
    a = M.args_parser().parse_args(["--base", "https://lmgw.test:8443", "--token", "t", *extra])
    a.cases_run_order = False
    a.public_prefix = ""
    return a


def ctx_fixture():
    # lmgw's estimate of the 4B at 16384 (design §1): 3737 MiB.
    fp = {"weights": 2374 * MIB, "kv": 1152 * MIB, "compute": 211 * MIB, "total": 3737 * MIB}
    lw = {"per_layer": [60 * MIB] * 36, "exps": [0] * 36, "input": 300 * MIB,
          "output": 320 * MIB, "extra": 0}
    return {"class_args": ["--device", "nvidia.com/gpu=all", "--security-opt", "label=disable"],
            "image": "ghcr.io/ggml-org/llama.cpp:server-cuda", "models_dir": "/data/models",
            "globals": ["--cdi-spec-dir", "/run/user/0/lmgw-machine/lmgw/cdi"],
            "footprints": {c: dict(fp) for c in M.CASES if c != "order"},
            "n_layer": {M.BASE_FILE: 36}, "layouts": {M.BASE_FILE: lw},
            "sizes": {M.BASE_FILE: 2500 * MIB, "draft.gguf": 600 * MIB},
            "files": {M.BASE_FILE, "draft.gguf"},
            "flags": {"tensor-split", "split-mode", "device-draft"}}


def cards_now(kept=None):
    return M.apply_smi(M.cards_from_vram(vram_fixture(kept)), M.parse_smi_gpus(smi_gpus_text()))


class Parsing(unittest.TestCase):
    def test_cards_from_vram_take_what_lmgw_keeps_free_off(self):
        cards = M.cards_from_vram(vram_fixture(kept={2: 1000}))
        self.assertEqual([c.index for c in cards], [0, 1, 2, 3])
        self.assertEqual(cards[2].free, (22690 - 1000) * MIB)
        self.assertEqual(cards[2].id, UUIDS[2])
        self.assertEqual(cards[2].label(), f"{BW} (card 2)")

    def test_smi_and_the_lower_free_figure(self):
        gpus = M.parse_smi_gpus(smi_gpus_text())
        self.assertEqual(gpus[2]["pci"], "0000:b1:00.0")
        self.assertEqual(gpus[2]["free"], 22690 * MIB)
        lower = [f - 500 if i == 1 else f + 500 for i, f in enumerate(FREE)]
        cards = M.apply_smi(M.cards_from_vram(vram_fixture()), M.parse_smi_gpus(smi_gpus_text(lower)))
        self.assertEqual(cards[1].free, (12220 - 500) * MIB)
        self.assertEqual(cards[0].free, 7641 * MIB)
        apps = M.parse_smi_apps(f"{UUIDS[1]}, 4242, 5256\n{UUIDS[2]}, 77, [N/A]\nnoise\n")
        self.assertEqual(apps, [(UUIDS[1], 4242, 5256 * MIB), (UUIDS[2], 77, None)])

    def test_an_lmgw_without_card_ids_is_matched_by_index(self):
        v = vram_fixture()
        for d in v["devices"]:
            d.pop("id", None)
            d.pop("kept_free", None)
        cards = M.apply_smi(M.cards_from_vram(v), M.parse_smi_gpus(smi_gpus_text()))
        self.assertEqual([c.id for c in cards], UUIDS)
        self.assertEqual(cards[2].pci, "0000:b1:00.0")

    def test_pci_spellings_meet(self):
        self.assertEqual(M.norm_pci("00000000:B1:00.0"), M.norm_pci("0000:b1:00.0"))
        self.assertEqual(M.norm_pci("nothing"), "")

    def test_engine_log(self):
        log = M.parse_engine_log(LOG_SPLIT2)
        self.assertEqual(log["cuda_found"], 2)
        self.assertEqual([d["name"] for d in log["cuda_devices"]], [BW, ADA])
        self.assertEqual([(u["dev"], u["pci"]) for u in log["using"]],
                         [("CUDA0", "0000:b1:00.0"), ("CUDA1", "0000:ca:00.0")])
        self.assertEqual(log["offloaded"], "37/37")
        main = M.buffers_by_dev(log, "main")
        self.assertEqual(main["CUDA0"], {"model": 850 * MIB, "KV": 960 * MIB, "compute": 120 * MIB})
        self.assertEqual(main["CUDA1"]["compute"], int(301.75 * MIB))
        self.assertEqual(main["CUDA_Host"], {"compute": 40 * MIB})
        draft = M.buffers_by_dev(log, "draft")
        self.assertEqual(draft, {"CUDA0": {"model": 300 * MIB}, "CUDA1": {"model": 310 * MIB}})
        self.assertEqual(M.layer_counts(log["layers"], 36), {"CUDA0": 15, "CUDA1": 21})
        self.assertTrue(M.entries_verdict([15, 22], ["CUDA0", "CUDA1"],
                                          M.layer_counts(log["layers"], 36), 36).startswith("yes"))
        self.assertTrue(M.entries_verdict([15, 21], ["CUDA0", "CUDA1"],
                                          M.layer_counts(log["layers"], 36), 36).startswith("the repeating"))
        self.assertTrue(M.entries_verdict([10, 27], ["CUDA0", "CUDA1"],
                                          M.layer_counts(log["layers"], 36), 36).startswith("no"))
        self.assertIn("no layer assignment", M.entries_verdict([15, 22], ["CUDA0", "CUDA1"], {}, 36))

    def test_list_devices_matched_by_free_memory_among_like_cards(self):
        out = "\n".join(["load_backend: loaded CUDA backend from /app/libggml-cuda.so",
                         "Available devices:",
                         f"  CUDA0: {BW} (97887 MiB, 22000 MiB free)",
                         f"  CUDA1: {ADA} (49140 MiB, 16000 MiB free)",
                         f"  CUDA2: {ADA} (49140 MiB, 7500 MiB free)"])
        devs = M.parse_list_devices(out)
        self.assertEqual([d["dev"] for d in devs], ["CUDA0", "CUDA1", "CUDA2"])
        m = M.match_devices(devs, M.parse_smi_gpus(smi_gpus_text()))
        self.assertEqual([d["card"] for d in m], [2, 3, 0])
        self.assertFalse(m[0]["ambiguous"])
        self.assertTrue(m[1]["ambiguous"])
        by_pci = M.match_devices(M.parse_engine_log(LOG_SPLIT2)["using"], M.parse_smi_gpus(smi_gpus_text()))
        self.assertEqual([(d["card"], d["how"]) for d in by_pci], [(2, "pci"), (3, "pci")])


class Layers(unittest.TestCase):
    def test_layer_counts_that_sum_to_n_layer_plus_one(self):
        a = M.assign_layers([15, 22], 36)
        self.assertEqual(len(a), 37)
        self.assertEqual(a.count(0), 15)
        self.assertEqual(a.count(1), 22)
        self.assertEqual(a[36], 1)  # the output layer, on the last card
        a3 = M.assign_layers([9, 13, 15], 36)
        self.assertEqual([a3.count(d) for d in range(3)], [9, 13, 15])
        cpu = M.assign_layers([1, 1], 36, n_gpu_layers=10)
        self.assertEqual(cpu[:27], [None] * 27)
        self.assertEqual(cpu[36], 1)

    def test_default_entries(self):
        for total in (29, 37, 49, 65):
            for k in (1, 2, 3):
                e = M.default_entries(total, k)
                self.assertEqual(len(e), k)
                self.assertEqual(sum(e), total)
                self.assertEqual(len(set(e)), k)

    def test_gguf_header_and_the_weights_per_layer(self):
        raw = gguf_bytes()
        layout = M.read_gguf_layout(io.BytesIO(raw), len(raw))
        self.assertEqual((layout["arch"], layout["n_layer"]), ("qwen3", 2))
        self.assertEqual(layout["tensors"]["blk.0.ffn_up_exps.weight"], 320)
        self.assertEqual(layout["tensors"]["output_norm.weight"], 32)
        lw = M.layer_weights(layout)
        self.assertEqual(lw["per_layer"], [544, 224])
        self.assertEqual(lw["exps"], [320, 0])
        self.assertEqual(lw["input"], 1024)
        self.assertEqual(lw["output"], 32 + 1024)  # tied: the embedding again
        p = M.predict_cards(lw, M.assign_layers([1, 2], 2), 2, kv_total=200, n_cpu_moe=1)
        self.assertEqual(p["weights"], [544 - 320, 224 + 1056])
        self.assertEqual(p["kv"], [100, 100])
        self.assertEqual(p["layers"], [1, 1])
        self.assertEqual(p["output_card"], 1)
        with self.assertRaises(ValueError):
            M.read_gguf_layout(io.BytesIO(b"NOPE" + raw[4:]), len(raw))


class RunArgs(unittest.TestCase):
    def test_the_class_card_goes_and_the_rest_stays(self):
        cls = ["--device", "nvidia.com/gpu=all", "--security-opt", "label=disable", "--gpus=all",
               "-e", "NVIDIA_VISIBLE_DEVICES=all", "--env=FOO=1", "--device", "/dev/fuse",
               "--device=/dev/dri/renderD128", "--runtime", "nvidia", "--shm-size", "1g"]
        self.assertEqual(M.strip_gpu_args(cls), ["--security-opt", "label=disable", "--env=FOO=1",
                                                 "--device", "/dev/fuse", "--shm-size", "1g"])
        got = M.run_args_for(cls[:4], UUIDS[1:3], nvd=True, pci_order=True)
        self.assertEqual(got, ["--security-opt", "label=disable",
                               "--device", f"nvidia.com/gpu={UUIDS[1]}",
                               "--device", f"nvidia.com/gpu={UUIDS[2]}",
                               "-e", f"NVIDIA_VISIBLE_DEVICES={UUIDS[1]},{UUIDS[2]}",
                               "-e", "CUDA_DEVICE_ORDER=PCI_BUS_ID"])


class Plans(unittest.TestCase):
    def plan(self, case, *extra, ctx=None, kept=None):
        return M.make_plan(case, cards_now(kept), ctx or ctx_fixture(), args(*extra))

    def test_one_card_by_uuid_on_the_roomiest_card(self):
        p = self.plan("uuid")
        self.assertEqual(p.status, "ready", p.reason)
        self.assertEqual([c.index for c in p.cards], [2])
        self.assertEqual(p.body["extra_run_args"], ["--security-opt", "label=disable",
                                                    "--device", f"nvidia.com/gpu={UUIDS[2]}"])
        self.assertEqual(p.body["extra_args"], ["-v"])
        self.assertEqual(p.body["model_id"], "wp0-uuid")
        self.assertEqual((p.body["n_gpu_layers"], p.body["fit"], p.body["ctx_size"]), (999, "off", 16384))
        # (weights + KV) x 1.25 + compute + 1024 MiB for the engine
        self.assertEqual(p.planned, [int(3526 * MIB * 1.25) + 211 * MIB + 1024 * MIB])
        nvd = self.plan("uuid-nvd")
        self.assertIn(f"NVIDIA_VISIBLE_DEVICES={UUIDS[2]}", nvd.body["extra_run_args"])

    def test_splits_take_the_roomiest_cards_in_pci_order(self):
        p2 = self.plan("split2")
        self.assertEqual(p2.status, "ready", p2.reason)
        self.assertEqual([c.index for c in p2.cards], [2, 3])
        self.assertEqual(p2.entries, [15, 22])
        self.assertEqual(p2.body["extra_args"], ["--split-mode", "layer", "--tensor-split", "15,22", "-v"])
        self.assertIn("CUDA_DEVICE_ORDER=PCI_BUS_ID", p2.body["extra_run_args"])
        self.assertIn(f"NVIDIA_VISIBLE_DEVICES={UUIDS[2]},{UUIDS[3]}", p2.body["extra_run_args"])
        self.assertGreater(p2.planned[1], p2.planned[0])  # the larger share and the output layer
        p3 = self.plan("split3", "--split3", "10,12,15", "--split-without-nvd")
        self.assertEqual([c.index for c in p3.cards], [1, 2, 3])
        self.assertEqual(p3.body["extra_args"][3], "10,12,15")
        self.assertFalse(any(a.startswith("NVIDIA_VISIBLE") for a in p3.body["extra_run_args"]))
        bad = self.plan("split3", "--split3", "10,27")
        self.assertEqual(bad.status, "skip")

    def test_a_card_without_room_skips_the_case_and_says_why(self):
        p = self.plan("uuid", "--margin-mib", "20000")
        self.assertEqual(p.status, "skip")
        self.assertTrue(any("does not fit" in n for n in p.notes))
        self.assertIn("plus the margin", p.reason)
        # lmgw keeping 10 GiB free on card 2 for a start in flight leaves card 3 the roomiest.
        moved = self.plan("uuid", kept={2: 10240})
        self.assertEqual([c.index for c in moved.cards], [3])
        order = self.plan("order", "--margin-mib", "16000")  # card 2 alone holds 768 + 16000
        self.assertEqual(order.status, "skip")
        self.assertIn("fewer than two cards", order.reason)

    def test_order_passes_every_card_with_room_by_uuid_in_reverse(self):
        p = self.plan("order")
        self.assertEqual(p.status, "ready", p.reason)
        self.assertEqual([c.index for c in p.cards], [0, 1, 2, 3])
        (n1, a1), (n2, a2) = p.podman
        devices = [a1[i + 1] for i, x in enumerate(a1) if x == "--device"]
        self.assertEqual(devices, [f"nvidia.com/gpu={u}" for u in reversed(UUIDS)])
        self.assertEqual(a1[:3], ["podman", "--cdi-spec-dir", "/run/user/0/lmgw-machine/lmgw/cdi"])
        self.assertEqual(a1[-2:], ["ghcr.io/ggml-org/llama.cpp:server-cuda", "--list-devices"])
        self.assertNotIn("CUDA_DEVICE_ORDER=PCI_BUS_ID", a1)
        self.assertIn("CUDA_DEVICE_ORDER=PCI_BUS_ID", a2)
        # Card 0 (7641 MiB free) drops out under a 7000 MiB margin; three remain.
        p = self.plan("order", "--margin-mib", "7000")
        self.assertEqual([c.index for c in p.cards], [1, 2, 3])

    def test_cases_without_their_files_or_flags_are_skipped(self):
        self.assertIn("--draft", self.plan("draft").reason)
        self.assertIn("--vl-file", self.plan("mmproj").reason)
        self.assertIn("--moe-file", self.plan("moe-cpu").reason)
        self.assertIn("not in the chat models dir", self.plan("draft", "--draft", "elsewhere.gguf").reason)
        ctx = ctx_fixture()
        ctx["files"].add("moe.gguf")
        ctx["n_layer"]["moe.gguf"] = 16
        self.assertIn("--moe-cache-mib", self.plan("moe-cache", "--moe-file", "moe.gguf", ctx=ctx).reason)
        ctx["flags"].discard("device-draft")
        self.assertIn("--device-draft", self.plan("draft-dev", "--draft", "draft.gguf", ctx=ctx).reason)
        d = self.plan("draft-dev", "--draft", "draft.gguf")
        self.assertEqual(d.status, "ready", d.reason)
        self.assertEqual(d.body["draft_gguf_path"], "draft.gguf")
        self.assertEqual(d.body["spec_type"], "draft-simple")
        self.assertIn("--device-draft", d.body["extra_args"])
        big = self.plan("uuid", "--max-model-mib", "2048")
        self.assertIn("--allow-large", big.reason)
        self.assertEqual(self.plan("uuid", "--max-model-mib", "2048", "--allow-large").status, "ready")

    def test_plan_text(self):
        t = M.plan_text(self.plan("split2"))
        self.assertIn("--tensor-split 15,22 (layer counts; sum 37)", t)
        self.assertIn("local_model_set:", t)
        self.assertIn("SKIPPED", M.plan_text(self.plan("uuid", "--margin-mib", "20000")))


class FakeLmgw(M.Lmgw):
    """lmgw's API as the script uses it, in memory. `models` are the rows; a started row's
    process is `pid` on its cards."""

    def __init__(self, world):
        super().__init__("https://lmgw.test:8443", "t")
        self.w = world

    def call(self, path, body=None, timeout=60):
        w = self.w
        u = urllib.parse.urlparse(path)
        q = dict(urllib.parse.parse_qsl(u.query))
        w.calls.append((u.path, q, body))
        if u.path == "/api/version":
            return 200, {"version": "0.7.0-test"}
        if u.path == "/api/vram":
            v = vram_fixture()
            v["resident"] = [{"model": m, "estimated_bytes": 3737 * MIB,
                              "charge": [{"cards": w.cards_of(m), "bytes": 3737 * MIB}], "cards": []}
                             for m in w.running]
            return 200, v
        if u.path == "/api/settings-full":
            return 200, {"container_prefix": "lmgw", "router": {
                "image": "ghcr.io/ggml-org/llama.cpp:server-cuda", "models_dir": w.models_dir,
                "extra_run_args": ["--device", "nvidia.com/gpu=all", "--security-opt", "label=disable"],
                "public_prefix": ""}}
        if u.path == "/api/gguf-files":
            return 200, {"models_dir": w.models_dir,
                         "files": [{"path": M.BASE_FILE, "size_bytes": 2500 * MIB}]}
        if u.path == "/api/llama-flags":
            return 200, {"flags": ["tensor-split", "split-mode", "device-draft", "verbose"]}
        if u.path == "/api/model-inspect":
            return 200, {"block_count": 2}
        if u.path == "/api/ladder-rung-plan":
            return 200, {"footprint": {"weights_bytes": 2374 * MIB, "kv_cache_bytes": 1152 * MIB,
                                       "compute_bytes": 211 * MIB, "total_bytes": 3737 * MIB}}
        if u.path == "/api/local-model":
            m = q.get("model_id")
            if m in w.models:
                return 200, {"model_id": m, "command_line": f"podman run … {m}"}
            return 400, {"code": "op_failed", "message": f"no local model with model_id '{m}'"}
        if u.path == "/v1/chat/completions":
            return 200, {"usage": {"completion_tokens": 5}}
        if u.path.startswith("/api/op/"):
            return self.op_(u.path[len("/api/op/"):], body)
        return 404, {"message": f"no fake for {u.path}"}

    def op_(self, name, a):
        w = self.w
        w.ops.append((name, a.get("action"), a.get("model") or a.get("model_id")))
        if name == "local_model_set" and a["action"] == "create":
            w.models[a["model_id"]] = a
            return 200, {"ok": True, "id": len(w.models), "warnings": [], "advisories": []}
        if name == "local_model_set" and a["action"] == "delete":
            w.running.discard(a["model_id"])
            if w.models.pop(a["model_id"], None) is None:
                return 400, {"message": "no such row"}
            return 200, {"ok": True, "message": f"local model '{a['model_id']}' deleted"}
        if name == "container":
            m = a["model"]
            if a["action"] == "start":
                if m == w.interrupt_on:
                    raise KeyboardInterrupt
                if m == w.no_answer_on:
                    return 0, "no answer from lmgw: timed out"
                w.running.add(m)
                return 200, {"ok": True, "message": "started"}
            if a["action"] == "stop":
                w.running.discard(m)
                return 200, {"ok": True, "message": "stopped"}
            if a["action"] == "logs":
                return 200, {"ok": True, "logs": w.log_of(m)}
        return 400, {"message": f"no fake for op {name}"}


class FakeHost(M.Host):
    def __init__(self, world):
        self.docker = ""
        self.smi_ok = world.smi_ok
        self.w = world

    def gpus(self):
        return M.parse_smi_gpus(smi_gpus_text())

    def apps(self):
        # Another program's llama-server on card 2 throughout, which is never counted.
        out = [(UUIDS[2], 999, 30000 * MIB)]
        for m in sorted(self.w.running):
            for u in self.w.cards_of(m):
                out.append((u, self.w.pid_of(m), 2000 * MIB))
        return out

    def spec_globals(self, prefix):
        return ["--cdi-spec-dir", "/run/user/0/lmgw-machine/lmgw/cdi"]

    def podman(self, argv, timeout=300):
        self.w.podman.append(argv)

        class R:
            returncode = 0
            stdout = "Available devices:\n" + "\n".join(
                f"  CUDA{i}: {NAMES[c]} ({TOTAL[c]} MiB, {FREE[c] - 300} MiB free)"
                for i, c in enumerate((2, 0, 1, 3) if "CUDA_DEVICE_ORDER=PCI_BUS_ID" not in argv
                                      else (0, 1, 2, 3)))
            stderr = ""
        return R()


class World:
    def __init__(self, tmp, smi_ok=True):
        self.models_dir = tmp
        self.models, self.running = {}, set()
        self.calls, self.ops, self.podman = [], [], []
        self.smi_ok = smi_ok
        self.interrupt_on = None
        self.no_answer_on = None
        with open(os.path.join(tmp, M.BASE_FILE), "wb") as f:
            f.write(gguf_bytes())

    def pid_of(self, m):
        return 4000 + list(self.models).index(m)

    def alias_of(self, pid):
        return next((m for m in self.models if self.pid_of(m) == pid), "someone-else")

    def cards_of(self, m):
        a = self.models[m]["extra_run_args"]
        return [x.split("=", 1)[1] for x in a if x.startswith("nvidia.com/gpu=")]

    def log_of(self, m):
        n = len(self.cards_of(m))
        lines = [f"ggml_cuda_init: found {n} CUDA devices:"]
        lines += [f"load_tensors:        CUDA{i} model buffer size =   600.00 MiB" for i in range(n)]
        lines += [f"llama_kv_cache:      CUDA{i} KV buffer size =   500.00 MiB" for i in range(n)]
        lines += [f"llama_context:      CUDA{i} compute buffer size =   200.00 MiB" for i in range(n)]
        return "\n".join(lines)


class Live(unittest.TestCase):
    """main() against the fakes."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.saved = (M.Lmgw, M.Host, M.alias_of)

    def tearDown(self):
        M.Lmgw, M.Host, M.alias_of = self.saved
        self.tmp.cleanup()

    def main(self, world, *argv):
        M.Lmgw = lambda *_a, **_k: FakeLmgw(world)
        M.Host = lambda *_a, **_k: FakeHost(world)
        M.alias_of = world.alias_of
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            try:
                code = M.main(["--base", "https://lmgw.test:8443", "--token", "t", *argv])
            except SystemExit as e:
                code = e.code
        return code, out.getvalue()

    def test_a_dry_run_writes_nothing(self):
        w = World(self.tmp.name)
        code, out = self.main(w, "--cases", "order,uuid,split2,draft")
        self.assertEqual(code, 0, out)
        self.assertIn("DRY RUN", out)
        self.assertIn("## uuid:", out)
        self.assertIn("--list-devices", out)
        self.assertIn("SKIPPED: needs --draft", out)
        self.assertEqual(w.ops, [])
        self.assertTrue(all(body is None for _, _, body in w.calls))
        self.assertEqual(w.podman, [])
        self.assertFalse(any(n.startswith("wp0-") for n in os.listdir(os.getcwd())))

    def test_run_needs_nvidia_smi_here(self):
        w = World(self.tmp.name, smi_ok=False)
        code, out = self.main(w, "--run")
        self.assertIn("needs nvidia-smi", str(code))
        self.assertEqual(w.ops, [])

    def test_a_row_left_by_a_killed_run_stops_a_run(self):
        w = World(self.tmp.name)
        w.models["wp0-split2"] = {"extra_run_args": []}
        code, out = self.main(w, "--run", "--out-dir", os.path.join(self.tmp.name, "o"))
        self.assertEqual(code, 1)
        self.assertIn("--clean-leftovers", out)
        self.assertEqual(w.ops, [])
        code, out = self.main(w, "--clean-leftovers")  # a dry run: says, removes nothing
        self.assertEqual(code, 0)
        self.assertIn("wp0-split2", w.models)

    def test_a_run_measures_and_removes_its_rows(self):
        w = World(self.tmp.name)
        out_dir = os.path.join(self.tmp.name, "out")
        code, out = self.main(w, "--run", "--cases", "order,uuid,split2", "--no-chat",
                              "--settle-s", "1", "--out-dir", out_dir)
        self.assertEqual(code, 0, out)
        self.assertEqual(w.models, {})
        self.assertEqual(w.running, set())
        created = [m for n, act, m in w.ops if n == "local_model_set" and act == "create"]
        deleted = [m for n, act, m in w.ops if n == "local_model_set" and act == "delete"]
        self.assertEqual(created, ["wp0-uuid", "wp0-split2"])
        self.assertEqual(sorted(deleted), sorted(created))
        # The two --list-devices variants, then the clean-up's removal of a container left by one.
        self.assertEqual(len(w.podman), 3)
        self.assertEqual(w.podman[2], ["podman", "rm", "-f", "--ignore", "wp0-list-devices"])
        with open(os.path.join(out_dir, "report.md")) as f:
            report = f.read()
        self.assertIn("### uuid:", report)
        self.assertIn("| Card | Engine device |", report)
        self.assertIn("runs, on that card alone", report)
        self.assertIn("### The engine's cost per card", report)
        self.assertIn("CUDA0: card 2, CUDA1: card 0", report)
        self.assertIn("CUDA0: card 0, CUDA1: card 1", report)
        with open(os.path.join(out_dir, "raw.json")) as f:
            raw = json.load(f)
        self.assertEqual([r["case"] for r in raw], ["order", "uuid", "split2"])
        uuid = raw[1]["cards"][0]
        # 2000 MiB held by the process, 600 + 500 + 200 named in the log: 700 MiB overhead.
        self.assertEqual((uuid["measured"], uuid["overhead"]), (2000 * MIB, 700 * MIB))
        split2 = raw[2]["cards"]
        self.assertEqual([(c["dev"], c["measured"]) for c in split2],
                         [("CUDA0", 2000 * MIB), ("CUDA1", 2000 * MIB)])
        self.assertIn("taken as the cards in PCI order", report)

    def test_an_interrupted_run_removes_what_it_created(self):
        w = World(self.tmp.name)
        w.interrupt_on = "wp0-split2"
        code, out = self.main(w, "--run", "--cases", "uuid,split2,split3", "--no-chat",
                              "--settle-s", "1", "--out-dir", os.path.join(self.tmp.name, "o"))
        self.assertIn("interrupted", out)
        self.assertEqual(w.models, {})
        self.assertEqual(w.running, set())
        self.assertNotIn(("local_model_set", "create", "wp0-split3"), w.ops)
        self.assertEqual(code, 0)

    def test_a_start_without_an_answer_fails_its_case_and_the_rest_go_on(self):
        w = World(self.tmp.name)
        w.no_answer_on = "wp0-uuid"
        out_dir = os.path.join(self.tmp.name, "o")
        code, out = self.main(w, "--run", "--cases", "uuid,split2", "--no-chat", "--settle-s", "1",
                              "--out-dir", out_dir)
        self.assertEqual(code, 0, out)
        self.assertIn("failed: the start failed", out)
        self.assertEqual(w.models, {})
        with open(os.path.join(out_dir, "raw.json")) as f:
            raw = json.load(f)
        self.assertEqual([r["status"] for r in raw], ["failed", "measured"])


if __name__ == "__main__":
    unittest.main(verbosity=1)
