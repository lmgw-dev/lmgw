//! Benchmark runs (benchmark design §8.1): `lmgw__bench_plan` through
//! `lmgw__bench_delete`. Their dispatch is `selfadmin/bench.rs`.

use serde_json::Value;

use crate::mcp::selfadmin::{bool_p, int_p, num_p, str_p, Builtin};

/// The request arguments `lmgw__bench_plan` and `lmgw__bench_start` share:
/// the row, the flat overrides (§3.5), the phases and the repetitions.
fn request_props() -> Vec<(&'static str, Value)> {
    vec![
        (
            "model_id",
            str_p("The local chat model to benchmark (its model_id, see lmgw__models)."),
        ),
        (
            "rung",
            int_p("A ladder row's rung: 0 = base (default), up to the row's top rung."),
        ),
        (
            "image",
            str_p(
                "Run it on this image instead of the row's (a tag or ID from \
                 lmgw__container_images) — comparing two builds is two runs of one row with \
                 two images. It must already be on this machine.",
            ),
        ),
        (
            "ctx_size",
            int_p("Override --ctx-size for this run only (the row is never changed)."),
        ),
        ("parallel", int_p("Override -np (slots) for this run only.")),
        ("ubatch_size", int_p("Override -ub for this run only.")),
        ("batch_size", int_p("Override -b for this run only.")),
        (
            "cache_type_k",
            str_p("Override the K cache type (f16, q8_0, q4_0, …) for this run only."),
        ),
        (
            "cache_type_v",
            str_p("Override the V cache type for this run only."),
        ),
        (
            "flash_attn",
            str_p("Override flash attention (auto, on, off) for this run only."),
        ),
        (
            "kv_unified",
            bool_p("Override --kv-unified for this run only."),
        ),
        ("n_gpu_layers", int_p("Override -ngl for this run only.")),
        (
            "no_draft",
            bool_p("Run without the row's speculative drafter. Default false."),
        ),
        (
            "phases",
            str_p(
                "Comma-separated phases to run: probes, prefill, decode, concurrent, mixed \
                 (load always runs). Empty or absent: all of them.",
            ),
        ),
        (
            "repetitions",
            int_p("Repetitions per point (median, min and max are stored). Default 3."),
        ),
    ]
}

pub(super) fn tools() -> Vec<Builtin> {
    let mut start_props = request_props();
    start_props.push((
        "notes",
        str_p("Free text stored with the run (why you ran it)."),
    ));
    vec![
        Builtin {
            name: "lmgw__bench_plan",
            writes: false,
            description:
                "Preview a benchmark run of one local chat model without starting anything: the \
                 effective settings after your overrides, the exact podman command line, the \
                 build (image and its labels), the points each phase would measure (derived \
                 from the row's numbers and marked provisional — a run reads the real slot \
                 count and context from the live server), which behaviour probes apply, and \
                 every container a start would stop, with whether it is busy. 'blocked' says \
                 why a start would be refused right now: the GPU hold is on, another run is \
                 going (with its run_id), the row is missing, disabled or not a chat model, the \
                 rung does not exist, or the image is not on this machine. Always call this \
                 before lmgw__bench_start and show the user what will be stopped.",
            props: request_props(),
            required: &["model_id"],
        },
        Builtin {
            name: "lmgw__bench_start",
            writes: true,
            description:
                "Start a benchmark run: one local chat model, as its row is configured (plus \
                 your overrides), measured through a fixed suite — load time and VRAM, \
                 behaviour probes (chat, thinking on/off, tool calls, JSON schema, determinism, \
                 vision, reasoning history, needle), prefill throughput and time to first token \
                 at several prompt lengths, decode at several depths, concurrent streams, and a \
                 long prompt arriving mid-decode — with power and energy throughout. It takes \
                 the WHOLE GPU: it STOPS EVERY MODEL on the card (chat, embedding, audio, image; \
                 busy ones after their requests finish, nothing is killed) and, until the run \
                 ends, BLOCKS ALL LOCAL TRAFFIC — requests for local models go to their \
                 fallback alias or are refused with 503 'gpu_benchmark'. Stopped models are not \
                 restarted; they load again on their next request. It is refused while the GPU \
                 hold is on, and while another run is going (one at a time). A run of a large \
                 model at a long context takes minutes; its top points come from the model's \
                 real context, nothing is capped. Returns run_id and job_id at once: follow it \
                 with lmgw__bench_run id=<run_id>; lmgw__bench_cancel ends it early (partial \
                 results are kept). Call lmgw__bench_plan first and confirm what will be \
                 stopped with the user.",
            props: start_props,
            required: &["model_id"],
        },
        Builtin {
            name: "lmgw__bench_runs",
            writes: false,
            description:
                "List benchmark runs, newest first: model, quant, rung, image and build, GPU, \
                 status (running, done, failed, canceled, aborted, interrupted), the headline \
                 numbers (prefill tok/s and TTFT at 2048 tokens, decode tok/s at depth 64 and at \
                 the deepest point, aggregate tok/s at all slots, decode tokens per joule, load \
                 time, VRAM after load, the mixed-phase stall), probes passed of judged, and \
                 against the previous comparable run (same model file, settings, suite version \
                 and GPU) how many metrics regressed or improved beyond threshold_pct.",
            props: vec![
                ("model_id", str_p("Only this model's runs.")),
                ("limit", int_p("At most this many runs. Absent: every run.")),
                (
                    "before",
                    int_p("Only runs older than this run id (the next page)."),
                ),
                (
                    "threshold_pct",
                    num_p(
                        "The regression threshold in percent, widened by each metric's measured \
                         noise. Default 5.",
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__bench_run",
            writes: false,
            description:
                "One benchmark run in full: its identity (model file and quant, build, GPU, \
                 settings, command line), every probe with its evidence, every measured point \
                 per phase (median, min, max over the repetitions, energy and tokens per joule), \
                 phase errors, and the comparison with its previous comparable run: per headline \
                 metric the delta, the noise band and a verdict (regression, improvement, same, \
                 missing, or not_same_point when the two runs measured it at different points — \
                 a run that stopped early; its delta is information and counts for nothing), and \
                 probes that changed outcome. The 500 ms power/VRAM/temperature \
                 timeline is left out unless timeline=true — it is the bulk of a run.",
            props: vec![
                ("id", int_p("The run id (lmgw__bench_runs).")),
                (
                    "threshold_pct",
                    num_p("The regression threshold in percent. Default 5."),
                ),
                (
                    "timeline",
                    bool_p("Include the 500 ms timeline samples. Default false."),
                ),
            ],
            required: &["id"],
        },
        Builtin {
            name: "lmgw__bench_cancel",
            writes: true,
            description:
                "Cancel the benchmark run that is going: its in-flight requests are dropped, \
                 its container removed, the GPU released, and the run kept as 'canceled' with \
                 the points measured so far. Models it stopped are not restarted.",
            props: vec![(
                "run_id",
                int_p("Optional: the run you mean — refused if another one is going."),
            )],
            required: &[],
        },
        Builtin {
            name: "lmgw__bench_delete",
            writes: true,
            description:
                "Delete a finished benchmark run for good (nothing else prunes runs). A run \
                 still going must be canceled first.",
            props: vec![("id", int_p("The run id (lmgw__bench_runs)."))],
            required: &["id"],
        },
    ]
}
