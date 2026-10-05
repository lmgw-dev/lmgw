//! Local (chat) model editor — the full llama-server parameter surface as a
//! structured, data-driven form. Also the "exception path": picking a GGUF
//! already on disk instead of going through the HF wizard.
//!
//! An editor page: the cards flow into as many columns as the pane holds,
//! beside them the server's view of the *saved* row (container state,
//! problems, the command line it renders to) stays in sight, and Save lives
//! in the page foot with a count of what changed. Leaving with unsaved
//! changes asks first.
//!
//! Save semantics mirror the ops plane exactly: filled fields are sent,
//! emptied optional fields go into `clear`, apply stays a separate step.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use lmgw_api_types::{
    GgufFiles, LlamaParams, LocalModelDetail, PlanResult, Rung, RungPlan, SettingsFull,
};
use serde_json::{json, Map, Value};

use super::local_test::{
    not_tested, run_load_test, test_key, LoadTestCard, Outcome, UnsavedTestModal,
};
use super::model_editors::HoldFallback;
use crate::catalog::use_model_catalog;
use crate::fmt::{grouped, human_bytes};
use crate::model_ops::ContainerStatusRow;
use crate::scope::Scope;
use crate::widgets::{
    use_dirty_guard, use_toasts, CopyBtn, Field, ImageClass, ImagePicker, Modal, PageFrame,
    SaveBar, Select,
};

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Int,
    Float,
    Text,
    /// (value, label) pairs; "" = unset.
    Sel(&'static [(&'static str, &'static str)]),
}

struct Spec {
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    kind: Kind,
}

const UNSET: (&str, &str) = ("", "llama.cpp default");

const CACHE_TYPES: &[(&str, &str)] = &[
    UNSET,
    ("f16", "f16"),
    ("bf16", "bf16"),
    ("q8_0", "q8_0"),
    ("q5_1", "q5_1"),
    ("q5_0", "q5_0"),
    ("q4_1", "q4_1"),
    ("q4_0", "q4_0"),
];

const SECTIONS: &[(&str, &[Spec])] = &[
    (
        "Context & batch",
        &[
            Spec {
                key: "ctx_size",
                label: "Context size",
                hint: "tokens",
                kind: Kind::Int,
            },
            Spec {
                key: "parallel",
                label: "Parallel slots",
                hint: "-np",
                kind: Kind::Int,
            },
            Spec {
                key: "kv_unified",
                label: "Unified KV",
                hint: "-kvu / -no-kvu",
                kind: Kind::Sel(&[
                    ("", "default (llama-server's own choice)"),
                    ("true", "on — one shared pool"),
                    ("false", "off — split per slot"),
                ]),
            },
            Spec {
                key: "kv_unified_per_slot",
                label: "Unified per-slot cap",
                hint: "--kv-unified-per-slot, tokens",
                kind: Kind::Int,
            },
            Spec {
                key: "batch_size",
                label: "Batch size",
                hint: "-b",
                kind: Kind::Int,
            },
            Spec {
                key: "ubatch_size",
                label: "Physical batch",
                hint: "-ub",
                kind: Kind::Int,
            },
            Spec {
                key: "threads",
                label: "Threads",
                hint: "-t",
                kind: Kind::Int,
            },
        ],
    ),
    (
        "GPU & memory",
        &[
            Spec {
                key: "n_gpu_layers",
                label: "GPU layers",
                hint: "-ngl",
                kind: Kind::Int,
            },
            Spec {
                key: "flash_attn",
                label: "Flash attention",
                hint: "",
                kind: Kind::Sel(&[UNSET, ("auto", "auto"), ("on", "on"), ("off", "off")]),
            },
            Spec {
                key: "cache_type_k",
                label: "KV cache K",
                hint: "",
                kind: Kind::Sel(CACHE_TYPES),
            },
            Spec {
                key: "cache_type_v",
                label: "KV cache V",
                hint: "",
                kind: Kind::Sel(CACHE_TYPES),
            },
            Spec {
                key: "cache_ram",
                label: "Cache RAM",
                hint: "MiB; -1 unlimited, 0 off",
                kind: Kind::Int,
            },
            Spec {
                key: "fit",
                label: "Fit",
                hint: "--fit: shrink unset args to device memory",
                kind: Kind::Text,
            },
            Spec {
                key: "fit_ctx",
                label: "Fit context floor",
                hint: "--fit-ctx",
                kind: Kind::Int,
            },
        ],
    ),
    (
        "Template & reasoning",
        &[
            Spec {
                key: "chat_template_file",
                label: "Template file",
                hint: "relative to models dir",
                kind: Kind::Text,
            },
            Spec {
                key: "reasoning_format",
                label: "Reasoning format",
                hint: "",
                kind: Kind::Sel(&[
                    UNSET,
                    ("auto", "auto"),
                    ("none", "none"),
                    ("deepseek", "deepseek"),
                    ("deepseek-legacy", "deepseek-legacy"),
                ]),
            },
            Spec {
                key: "reasoning",
                label: "Reasoning",
                hint: "",
                kind: Kind::Sel(&[UNSET, ("auto", "auto"), ("on", "on"), ("off", "off")]),
            },
            Spec {
                key: "reasoning_budget",
                label: "Thinking budget",
                hint: "tokens; -1 unrestricted, 0 off",
                kind: Kind::Int,
            },
            Spec {
                key: "n_predict",
                label: "Max output tokens (--n-predict)",
                hint: "tokens; -1 unbounded",
                kind: Kind::Int,
            },
            Spec {
                key: "reasoning_preserve",
                label: "Preserve reasoning",
                hint: "keep every turn's trace in history",
                kind: Kind::Sel(&[
                    ("", "template default"),
                    ("true", "preserve all turns"),
                    ("false", "only the last turn"),
                ]),
            },
            Spec {
                key: "reasoning_effort",
                label: "Reasoning effort",
                hint: "template variable, e.g. low / medium / high",
                kind: Kind::Text,
            },
        ],
    ),
    (
        "Sampling defaults",
        &[
            Spec {
                key: "temp",
                label: "Temperature",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "top_p",
                label: "Top-p",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "top_k",
                label: "Top-k",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "min_p",
                label: "Min-p",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "repeat_penalty",
                label: "Repeat penalty",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "presence_penalty",
                label: "Presence penalty",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "seed",
                label: "Seed",
                hint: "",
                kind: Kind::Int,
            },
        ],
    ),
    (
        "Multimodal & speculative",
        &[
            Spec {
                key: "mmproj_path",
                label: "Projector (mmproj)",
                hint: "relative to models dir",
                kind: Kind::Text,
            },
            Spec {
                key: "draft_gguf_path",
                label: "Draft model",
                hint: "speculative decoding weights",
                kind: Kind::Text,
            },
            Spec {
                key: "spec_type",
                label: "Speculation type",
                hint: "e.g. draft-mtp, draft-simple, ngram-mod",
                kind: Kind::Text,
            },
            Spec {
                key: "spec_draft_n_max",
                label: "Draft max",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "spec_draft_n_min",
                label: "Draft min",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "spec_draft_ngl",
                label: "Draft GPU layers",
                hint: "number, auto or all",
                kind: Kind::Text,
            },
        ],
    ),
];

fn param_string(p: &LlamaParams, key: &str) -> String {
    fn i(v: Option<i64>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    fn f(v: Option<f64>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    fn s(v: &Option<String>) -> String {
        v.clone().unwrap_or_default()
    }
    match key {
        "ctx_size" => i(p.ctx_size),
        "n_gpu_layers" => i(p.n_gpu_layers),
        "threads" => i(p.threads),
        "batch_size" => i(p.batch_size),
        "ubatch_size" => i(p.ubatch_size),
        "parallel" => i(p.parallel),
        "kv_unified" => match p.kv_unified {
            Some(true) => "true".into(),
            Some(false) => "false".into(),
            None => String::new(),
        },
        "kv_unified_per_slot" => i(p.kv_unified_per_slot),
        "flash_attn" => s(&p.flash_attn),
        "cache_type_k" => s(&p.cache_type_k),
        "cache_type_v" => s(&p.cache_type_v),
        "cache_ram" => i(p.cache_ram),
        "fit" => s(&p.fit),
        "fit_ctx" => i(p.fit_ctx),
        "chat_template_file" => s(&p.chat_template_file),
        "reasoning_format" => s(&p.reasoning_format),
        "reasoning" => s(&p.reasoning),
        "reasoning_budget" => i(p.reasoning_budget),
        "n_predict" => i(p.n_predict),
        "reasoning_preserve" => match p.reasoning_preserve {
            Some(true) => "true".into(),
            Some(false) => "false".into(),
            None => String::new(),
        },
        "reasoning_effort" => s(&p.reasoning_effort),
        "temp" => f(p.temp),
        "top_p" => f(p.top_p),
        "top_k" => i(p.top_k),
        "min_p" => f(p.min_p),
        "repeat_penalty" => f(p.repeat_penalty),
        "presence_penalty" => f(p.presence_penalty),
        "seed" => i(p.seed),
        "mmproj_path" => s(&p.mmproj_path),
        "draft_gguf_path" => s(&p.draft_gguf_path),
        "spec_type" => s(&p.spec_type),
        "spec_draft_n_max" => i(p.spec_draft_n_max),
        "spec_draft_n_min" => i(p.spec_draft_n_min),
        "spec_draft_ngl" => s(&p.spec_draft_ngl),
        _ => String::new(),
    }
}

/// One signal per spec key, created once.
fn make_fields() -> std::collections::HashMap<&'static str, RwSignal<String>> {
    SECTIONS
        .iter()
        .flat_map(|(_, specs)| specs.iter())
        .map(|s| (s.key, RwSignal::new(String::new())))
        .collect()
}

/// A form value, text or a checkbox, compared and restored as text — what
/// the dirty count and Discard work on.
#[derive(Clone, Copy)]
enum Val {
    Text(RwSignal<String>),
    Flag(RwSignal<bool>),
}

impl Val {
    fn read(self) -> String {
        match self {
            Val::Text(s) => s.get().trim().to_string(),
            Val::Flag(b) => b.get().to_string(),
        }
    }

    fn read_untracked(self) -> String {
        match self {
            Val::Text(s) => s.get_untracked().trim().to_string(),
            Val::Flag(b) => b.get_untracked().to_string(),
        }
    }

    fn write(self, v: &str) {
        match self {
            Val::Text(s) => s.set(v.to_string()),
            Val::Flag(b) => b.set(v == "true"),
        }
    }
}

/// One value of the form: its key, the card it sits on, the signal.
#[derive(Clone, Copy)]
struct Entry {
    key: &'static str,
    card: &'static str,
    val: Val,
}

/// Split a rendered command line at its whitespace, keeping quoted parts
/// (`'lmgw.instance=lmgw-dev'`) whole, quotes included.
fn shell_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => {
                quote = Some(c);
                start.get_or_insert(i);
            }
            (None, c) if c.is_whitespace() => {
                if let Some(b) = start.take() {
                    out.push(&s[b..i]);
                }
            }
            (None, _) => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(b) = start {
        out.push(&s[b..]);
    }
    out
}

/// A token that is an option, not a value: `-1` is a value.
fn is_flag(t: &str) -> bool {
    t.starts_with('-')
        && t.chars()
            .nth(1)
            .is_some_and(|c| !c.is_ascii_digit() && c != '.')
}

/// `podman run … image llama-server-args` one option per line, each with its
/// value, so a flag can be found by eye instead of in one 900-character line.
/// A bare word after an option that already has its value (the image) gets a
/// line of its own.
fn flag_lines(cmd: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    // The current line is an option still waiting for its value.
    let mut open = false;
    for t in shell_tokens(cmd) {
        let flag = is_flag(t);
        if flag || (!open && !cur.is_empty() && lines_started(&lines, &cur)) {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            cur.push_str(t);
            open = flag;
            continue;
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(t);
        open = false;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.join("\n")
}

/// Before the first option the words are the command itself ("podman run"):
/// they share a line. After it, a bare word that follows a complete option
/// starts its own.
fn lines_started(lines: &[String], cur: &str) -> bool {
    !lines.is_empty() || is_flag(cur.split(' ').next().unwrap_or(""))
}

// ---------------------------------------------------------------------------
// Ladder (ladder design §4.1–§4.2, §6): the rung table's own arithmetic
// mirrors `lmgw_core::ladder::LocalModel::per_slot_ctx`/`switchover`, which
// lives in lmgw-core and is not reachable from wasm — see that module's docs
// for why the two derived numbers are duplicated here rather than shared.
// Footprint and MTP are not: those need a GGUF header read, so they come from
// `/api/ladder-rung-plan`.
// ---------------------------------------------------------------------------

/// One rung above the base, as the table edits it: a raw-text pair like every
/// other numeric field on this form (so an in-progress edit — a half-typed
/// context size — shows its own parse error instead of silently discarding
/// the keystroke).
#[derive(Clone, Copy)]
struct RungRow {
    /// Stable across reorders — there is no reordering here, but `<For>`
    /// still needs a key that outlives an index.
    key: u64,
    gguf_path: RwSignal<String>,
    ctx_size: RwSignal<String>,
}

/// Fresh `RungRow`, keyed from the shared counter — a plain fn rather than a
/// closure so every call site (fill-from-load, discard, Add rung) can share
/// one counter without fighting over which of them owns the closure.
fn new_rung_row(next_key: StoredValue<u64>, gguf_path: String, ctx_size: String) -> RungRow {
    let key = next_key.get_value();
    next_key.set_value(key + 1);
    RungRow {
        key,
        gguf_path: RwSignal::new(gguf_path),
        ctx_size: RwSignal::new(ctx_size),
    }
}

/// `rungs.get()` with each row's displayed rung number (2, the base is 1) —
/// a named fn rather than `.collect::<Vec<_>>()` inline in `each=`, whose
/// turbofish the `view!` macro's tag scanner reads the `<`/`>` of as HTML.
fn enumerate_rungs(rows: Vec<RungRow>) -> Vec<(usize, RungRow)> {
    rows.into_iter().enumerate().collect()
}

/// The fields every rung shares with the base (design §4.1): template,
/// reasoning, sampling, projector, drafter, cache types, `parallel` — of
/// which only the ones the footprint/MTP call and the arithmetic actually
/// read are gathered here, straight off the same `fields` signals the rest
/// of the form edits.
#[derive(Clone, PartialEq)]
struct LadderShared {
    cache_type_k: String,
    cache_type_v: String,
    mmproj_path: String,
    draft_gguf_path: String,
    n_gpu_layers: String,
    parallel: String,
    n_predict: String,
}

/// `LlamaParams::effective_slots` (config/llama_params.rs), mirrored: `parallel` when it
/// names a real slot count, otherwise llama-server's own auto default of 4.
fn effective_slots(parallel_raw: &str) -> i64 {
    parallel_raw
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|&n| n >= 1)
        .unwrap_or(4)
}

/// `ctx_size / effective_slots` — `None` while `ctx_size` does not parse to a
/// positive token count yet (an in-progress edit, or an empty row).
fn per_slot_ctx(ctx_size_raw: &str, parallel_raw: &str) -> Option<i64> {
    let ctx: i64 = ctx_size_raw.trim().parse().ok().filter(|&c| c > 0)?;
    Some(ctx / effective_slots(parallel_raw).max(1))
}

/// A slot's real context: `per_slot` capped at the weights' trained context,
/// the way llama-server caps every slot (`lmgw_core::ladder::slot_ctx`,
/// mirrored). The second value says whether the cap bit — a rung §4.3
/// refuses to save, and the table says why before Save does.
fn capped_slot(per_slot: i64, trained: Option<u64>) -> (i64, bool) {
    match trained
        .and_then(|t| i64::try_from(t).ok())
        .filter(|&t| t > 0)
    {
        Some(t) if per_slot > t => (t, true),
        _ => (per_slot, false),
    }
}

/// `per_slot - n_predict` — `None` without a positive Max output, the same
/// case §4.3 rule 1 refuses to save.
fn switchover(per_slot: i64, n_predict_raw: &str) -> Option<i64> {
    let n_predict: i64 = n_predict_raw.trim().parse().ok().filter(|&n| n > 0)?;
    Some(per_slot - n_predict)
}

/// Whether the ladder toggle/rung list differ from what was loaded (second
/// pass, S8). The toggle itself changing is always the whole story: off→on
/// or on→off is a real change whatever the rung list holds, because Save
/// always sends `clear: "ladder"` while off, no matter what is still sitting
/// in the rung list from before it was cleared. Only when the toggle reads
/// the same both ways does the rung list get compared — and only then,
/// because "off, off, some leftover rungs from an add-then-undo" must not
/// read as dirty: Save's outcome (`clear: "ladder"`) would be identical to
/// the baseline's.
fn ladder_is_dirty(
    on: bool,
    was_on: bool,
    now: &[(String, String)],
    baseline: &[(String, String)],
) -> bool {
    on != was_on || ((on || was_on) && now != baseline)
}

/// A whole number, thousands-grouped, negative values included — `grouped`
/// only takes `u64`, and a misconfigured switchover (context too small for
/// max output) can be negative before save-time validation catches it.
fn grouped_signed(n: i64) -> String {
    if n < 0 {
        format!("−{}", grouped((-n) as u64))
    } else {
        grouped(n as u64)
    }
}

/// The query string for `/api/ladder-rung-plan` — `None` while `gguf_path` or
/// `ctx_size` do not yet name a real rung to ask about.
/// The raw key/value pairs for `/api/ladder-rung-plan` — `None` while
/// `gguf_path` or `ctx_size` do not yet name a real rung to ask about. Split
/// from [`rung_plan_query`]'s actual percent-encoding so the decision (ask or
/// not, which of the shared fields are worth sending) is a plain function a
/// host test can call directly — `urlenc` needs a JS host (`js_sys`), which
/// is why that one function is the one thing here without its own test; see
/// this module's test module docs.
fn rung_plan_pairs(
    gguf_path: &str,
    ctx_size_raw: &str,
    shared: &LadderShared,
) -> Option<Vec<(&'static str, String)>> {
    let gguf_path = gguf_path.trim();
    if gguf_path.is_empty() {
        return None;
    }
    let ctx: i64 = ctx_size_raw.trim().parse().ok().filter(|&c| c > 0)?;
    let mut pairs = vec![
        ("gguf_path", gguf_path.to_string()),
        ("ctx_size", ctx.to_string()),
    ];
    let mut add = |k: &'static str, v: &str| {
        let v = v.trim();
        if !v.is_empty() {
            pairs.push((k, v.to_string()));
        }
    };
    add("cache_type_k", &shared.cache_type_k);
    add("cache_type_v", &shared.cache_type_v);
    add("mmproj_path", &shared.mmproj_path);
    add("draft_gguf_path", &shared.draft_gguf_path);
    if let Ok(n) = shared.n_gpu_layers.trim().parse::<i64>() {
        pairs.push(("n_gpu_layers", n.to_string()));
    }
    Some(pairs)
}

fn rung_plan_query(gguf_path: &str, ctx_size_raw: &str, shared: &LadderShared) -> Option<String> {
    let pairs = rung_plan_pairs(gguf_path, ctx_size_raw, shared)?;
    Some(
        pairs
            .into_iter()
            .map(|(k, v)| format!("{k}={}", urlenc(&v)))
            .collect::<Vec<_>>()
            .join("&"),
    )
}

/// Settle a fast-changing key a beat after it stops changing, so a rung's
/// footprint/MTP is fetched once per pause instead of once per keystroke —
/// the same one-request-per-pause idea as `traffic.rs`'s `debounced`, just
/// over a derived key rather than a text box's own value.
fn debounce_key(key: Memo<Option<String>>) -> Signal<Option<String>> {
    let out = RwSignal::new(key.get_untracked());
    let gen = StoredValue::new(0u64);
    Effect::new(move |prev: Option<()>| {
        let k = key.get();
        if prev.is_none() {
            out.set(k);
            return;
        }
        gen.update_value(|g| *g += 1);
        let mine = gen.get_value();
        set_timeout(
            move || {
                if gen.try_get_value() == Some(mine) {
                    out.set(k);
                }
            },
            Duration::from_millis(300),
        );
    });
    out.into()
}

#[component]
pub fn LocalModelEdit() -> impl IntoView {
    let params = use_params_map();
    // The router keeps this view when only the id changes — a create that
    // lands on its new row, a link from one model to another — so each id
    // gets an editor of its own rather than one still holding the last.
    let id = Memo::new(move |_| params.with(|p| p.get("id").unwrap_or_default()));
    move || view! { <Editor id_param=id.get()/> }
}

#[component]
fn Editor(id_param: String) -> impl IntoView {
    let create_mode = id_param == "new";
    let row_id: Option<i64> = id_param.parse().ok();

    let toasts = use_toasts();
    let catalog = use_model_catalog();
    let navigate = use_navigate();
    let scope = Scope::new();

    // Create-mode prefill: `?gguf=…&model_id=…`, used by Wiring's "wire up"
    // affordance for a GGUF sitting on disk unreferenced.
    let query = use_query_map();
    let prefill = |key: &str| -> String {
        if create_mode {
            query.read_untracked().get(key).unwrap_or_default()
        } else {
            String::new()
        }
    };

    let fields = StoredValue::new(make_fields());
    let model_id = RwSignal::new(String::new());
    let gguf_path = RwSignal::new(String::new());
    let jinja = RwSignal::new(false);
    let no_mmproj = RwSignal::new(false);
    let public = RwSignal::new(true);
    let enabled = RwSignal::new(true);
    let idle_seconds = RwSignal::new(String::from("0"));
    let kwargs = RwSignal::new(String::new());
    // Owner override of the derived /v1/models capability facts
    // (model-capabilities design §7) — same "JSON object as text, empty
    // clears" convention as `kwargs` above.
    let capabilities_override = RwSignal::new(String::new());
    let extra_args = RwSignal::new(String::new());
    // Per-model container overrides (per-model-containers §3.1) — empty
    // image/extra_run_args means "inherit the chat class settings", same
    // "empty clears" convention every other optional field on this form
    // uses (see `collect` below).
    let image = RwSignal::new(String::new());
    let extra_run_args = RwSignal::new(String::new());
    let warm_start = RwSignal::new(false);
    // GPU-hold fallback (gpu-hold design §2/§3.2): the mode, and the model
    // when it routes elsewhere; `collect` writes them as
    // `hold_fallback_mode`/`hold_fallback`.
    let hold_mode = RwSignal::new(String::from("inherit"));
    let hold_alias = RwSignal::new(String::new());
    let loaded = RwSignal::new(create_mode);
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(None::<String>);
    let reload = RwSignal::new(0u32);
    // Test (local_test.rs): the load test on the row as saved, its answer
    // kept in the aside; with unsaved edits it asks first, and "Save and
    // test" runs it once the save has gone through. Whether one is running
    // is app-wide, keyed by the saved id (read below, once `detail` exists).
    let ops = crate::ops_state::use_ops();
    let test_result = RwSignal::new(None::<Outcome>);
    let test_ask = RwSignal::new(false);
    let test_after_save = RwSignal::new(false);

    // Ladder (ladder design §4.1, §6): off by default, on the moment a saved
    // row has any rungs above the base. `rungs` holds only the *higher*
    // rungs — the base stays `gguf_path`/`fields["ctx_size"]` above, the same
    // fields it always was, so nothing that reads them for a plain row has
    // to special-case a ladder one. Kept out of `entries`/`Val` (a flat
    // string per field) because a rung list is a dynamic Vec, not one
    // signal; it gets its own dirty/baseline/discard handling right beside
    // the generic one below rather than inside it.
    let ladder_on = RwSignal::new(false);
    let rungs = RwSignal::new(Vec::<RungRow>::new());
    let next_rung_key = StoredValue::new(0u64);
    let ladder_on_baseline = RwSignal::new(false);
    let ladder_baseline = RwSignal::new(Vec::<(String, String)>::new());

    // Everything the form edits, card by card: the dirty count, the marks on
    // the fields and Discard all read this one list.
    let entries = StoredValue::new({
        let mut v = vec![
            ("model_id", "Identity", Val::Text(model_id)),
            ("gguf_path", "Identity", Val::Text(gguf_path)),
            ("idle_seconds", "Identity", Val::Text(idle_seconds)),
            ("public", "Identity", Val::Flag(public)),
            ("enabled", "Identity", Val::Flag(enabled)),
            ("jinja", "Identity", Val::Flag(jinja)),
            ("no_mmproj", "Identity", Val::Flag(no_mmproj)),
            ("warm_start", "Container", Val::Flag(warm_start)),
            ("hold_fallback_mode", "Container", Val::Text(hold_mode)),
            ("hold_fallback", "Container", Val::Text(hold_alias)),
            ("image", "Container", Val::Text(image)),
            ("extra_run_args", "Container", Val::Text(extra_run_args)),
        ];
        fields.with_value(|f| {
            for (card, specs) in SECTIONS {
                for spec in *specs {
                    v.push((spec.key, *card, Val::Text(f[spec.key])));
                }
            }
        });
        v.extend([
            ("chat_template_kwargs", "Flags", Val::Text(kwargs)),
            ("extra_args", "Flags", Val::Text(extra_args)),
            (
                "capabilities_override",
                "Overrides",
                Val::Text(capabilities_override),
            ),
        ]);
        v.into_iter()
            .map(|(key, card, val)| Entry { key, card, val })
            .collect::<Vec<_>>()
    });
    // What was loaded (or, for a new model, the blank form): a value that
    // differs from it is an unsaved change.
    let baseline = RwSignal::new(Vec::<String>::new());
    let ladder_snapshot = move || -> Vec<(String, String)> {
        rungs
            .get_untracked()
            .iter()
            .map(|r| {
                (
                    r.gguf_path.get_untracked().trim().to_string(),
                    r.ctx_size.get_untracked().trim().to_string(),
                )
            })
            .collect()
    };
    let rebase = move || {
        baseline.set(entries.with_value(|es| es.iter().map(|e| e.val.read_untracked()).collect()));
        ladder_on_baseline.set(ladder_on.get_untracked());
        ladder_baseline.set(ladder_snapshot());
    };
    if create_mode {
        rebase();
        // Prefilled from a link: not saved yet, so it counts as a change.
        model_id.set(prefill("model_id"));
        gguf_path.set(prefill("gguf"));
    }
    let dirty = Memo::new(move |_| {
        let mut out: Vec<(&'static str, &'static str)> = baseline.with(|b| {
            entries.with_value(|es| {
                es.iter()
                    .enumerate()
                    .filter(|(i, e)| b.get(*i).is_none_or(|v| *v != e.val.read()))
                    .map(|(_, e)| (e.card, e.key))
                    .collect::<Vec<_>>()
            })
        });
        // The ladder toggle and its rung list live outside `entries` (a
        // dynamic Vec, not one signal per field — see where they are
        // declared), so they get their own comparison against
        // `ladder_baseline` here instead of going through the generic loop
        // above.
        let ladder_now: Vec<(String, String)> = rungs
            .get()
            .iter()
            .map(|r| {
                (
                    r.gguf_path.get().trim().to_string(),
                    r.ctx_size.get().trim().to_string(),
                )
            })
            .collect();
        if ladder_is_dirty(
            ladder_on.get(),
            ladder_on_baseline.get(),
            &ladder_now,
            &ladder_baseline.get(),
        ) {
            out.push(("Ladder", "ladder"));
        }
        out
    });
    let dirty_count = Signal::derive(move || dirty.with(Vec::len));
    let dirty_detail = Signal::derive(move || {
        dirty.with(|d| {
            let mut cards: Vec<(&str, usize)> = Vec::new();
            for (card, _) in d {
                match cards.iter_mut().find(|(c, _)| c == card) {
                    Some(c) => c.1 += 1,
                    None => cards.push((card, 1)),
                }
            }
            cards
                .iter()
                .map(|(c, n)| format!("{c} ({n})"))
                .collect::<Vec<_>>()
                .join(" · ")
        })
    });
    let is_dirty = move |key: &'static str| {
        Signal::derive(move || dirty.with(|d| d.iter().any(|(_, k)| *k == key)))
    };

    // Fields that would not parse, said at the field and counted in the foot
    // (they block Save): the server would refuse them anyway, but later.
    let errors = Memo::new(move |_| {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        fields.with_value(|f| {
            for (_, specs) in SECTIONS {
                for spec in *specs {
                    let raw = f[spec.key].get();
                    let raw = raw.trim();
                    if raw.is_empty() {
                        continue;
                    }
                    let bad = match spec.kind {
                        Kind::Int => raw.parse::<i64>().is_err().then_some("not a whole number"),
                        Kind::Float => raw.parse::<f64>().is_err().then_some("not a number"),
                        _ => None,
                    };
                    if let Some(why) = bad {
                        out.push((spec.key, format!("\u{201c}{raw}\u{201d} is {why}")));
                    }
                }
            }
        });
        if idle_seconds.with(|v| v.trim().parse::<i64>().is_err()) {
            out.push((
                "idle_seconds",
                "a whole number of seconds (0 = never sleep)".into(),
            ));
        }
        // §4.3 rule 1: Max output is mandatory on a ladder row — without it
        // no rung can promise a request fits. Skipped when the field already
        // has a parse error above, so this never doubles up on the same key.
        if ladder_on.get() && !out.iter().any(|(k, _)| *k == "n_predict") {
            let raw = fields.with_value(|f| f["n_predict"].get());
            match raw.trim().parse::<i64>() {
                Ok(n) if n > 0 => {}
                Ok(_) => out.push((
                    "n_predict",
                    "must be a positive token count — a ladder needs a real cap on the answer"
                        .into(),
                )),
                Err(_) => out.push((
                    "n_predict",
                    "required when Ladder is on — without a cap on the answer, no rung can \
                     promise a request fits"
                        .into(),
                )),
            }
        }
        // Rung 1 (the base) is a row of the ladder table once Ladder is on,
        // and every other row gets live feedback (`rung_errors` below) — the
        // base gets the same rather than waiting for a Save click to say so.
        if ladder_on.get() && gguf_path.with(|g| g.trim().is_empty()) {
            out.push(("gguf_path", "a GGUF file is required".to_string()));
        }
        out
    });
    let error_of = move |key: &'static str| {
        Signal::derive(move || {
            errors.with(|e| e.iter().find(|(k, _)| *k == key).map(|(_, m)| m.clone()))
        })
    };
    // Per-rung problems, keyed by the row's own `key` (not a `&'static str` —
    // there is no bound on how many rungs exist, so `error_of`'s lookup does
    // not fit): an empty GGUF path or a context size that does not parse.
    let rung_errors = Memo::new(move |_| {
        let mut out: Vec<(u64, String)> = Vec::new();
        if !ladder_on.get() {
            return out;
        }
        for r in rungs.get().iter() {
            if r.gguf_path.get().trim().is_empty() {
                out.push((r.key, "a GGUF file is required".to_string()));
                continue;
            }
            let ctx = r.ctx_size.get();
            let ctx = ctx.trim();
            if ctx.is_empty() {
                out.push((r.key, "a context size is required".to_string()));
            } else if ctx.parse::<i64>().is_err() {
                out.push((
                    r.key,
                    format!("\u{201c}{ctx}\u{201d} is not a whole number"),
                ));
            }
        }
        out
    });
    let rung_error_of = move |key: u64| {
        Signal::derive(move || {
            rung_errors.with(|e| e.iter().find(|(k, _)| *k == key).map(|(_, m)| m.clone()))
        })
    };
    // The table itself, not any one row: on when Ladder is on but nothing has
    // been added to climb to yet.
    let ladder_table_error = Signal::derive(move || {
        (ladder_on.get() && rungs.with(Vec::is_empty)).then(|| {
            "Ladder is on but has no rungs above the base — add at least one, or turn Ladder off"
                .to_string()
        })
    });
    let invalid = Signal::derive(move || {
        errors.with(Vec::len)
            + rung_errors.with(Vec::len)
            + usize::from(ladder_table_error.get().is_some())
    });

    use_dirty_guard().watch_page("this model", Signal::derive(move || dirty_count.get() > 0));

    let fill = move |p: &LlamaParams| {
        fields.with_value(|f| {
            for (key, sig) in f {
                sig.set(param_string(p, key));
            }
        });
        jinja.set(p.jinja);
        no_mmproj.set(p.no_mmproj);
        kwargs.set(if p.chat_template_kwargs.is_empty() {
            String::new()
        } else {
            serde_json::to_string_pretty(&p.chat_template_kwargs).unwrap_or_default()
        });
    };

    let detail = LocalResource::new(move || {
        reload.get();
        let id = row_id.unwrap_or(-1);
        async move {
            if id < 0 {
                return None;
            }
            crate::api::get::<LocalModelDetail>(format!("/api/local-model?id={id}"))
                .await
                .ok()
        }
    });
    // Chat class defaults, for the per-model image/extra_run_args overrides'
    // placeholders — an empty override field inherits these (§3.1).
    let class_settings =
        LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    Effect::new(move |_| {
        let Some(Some(d)) = detail.get() else { return };
        // Only seed the form on first load / after save, never mid-edit.
        if loaded.get_untracked() && reload.get_untracked() == 0 {
            return;
        }
        model_id.set(d.model_id.clone());
        gguf_path.set(d.gguf_path.clone());
        public.set(d.public);
        enabled.set(d.enabled);
        idle_seconds.set(d.idle_seconds.to_string());
        extra_args.set(d.extra_args.clone());
        image.set(d.image.clone().unwrap_or_default());
        extra_run_args.set(d.extra_run_args.clone().unwrap_or_default());
        warm_start.set(d.warm_start);
        hold_mode.set(match d.hold_fallback_mode.as_str() {
            "alias" | "none" => d.hold_fallback_mode.clone(),
            _ => "inherit".to_string(),
        });
        hold_alias.set(d.hold_fallback.clone().unwrap_or_default());
        capabilities_override.set(
            d.capabilities_override
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default(),
        );
        ladder_on.set(!d.ladder.is_empty());
        rungs.set(
            d.ladder
                .iter()
                .map(|r| new_rung_row(next_rung_key, r.gguf_path.clone(), r.ctx_size.to_string()))
                .collect(),
        );
        fill(&d.params);
        rebase();
        loaded.set(true);
    });

    // Collect the sparse patch: filled → value, emptied optional → clear.
    let collect =
        move || -> Result<Map<String, Value>, String> {
            let mut args = Map::new();
            let mut clear: Vec<&'static str> = Vec::new();
            fields.with_value(|f| -> Result<(), String> {
                for (_, specs) in SECTIONS {
                    for spec in *specs {
                        let raw = f[spec.key].get_untracked();
                        let raw = raw.trim();
                        if raw.is_empty() {
                            clear.push(spec.key);
                            continue;
                        }
                        let val = match spec.kind {
                            Kind::Int => Value::from(raw.parse::<i64>().map_err(|_| {
                                format!("{}: '{raw}' is not an integer", spec.label)
                            })?),
                            Kind::Float => {
                                Value::from(raw.parse::<f64>().map_err(|_| {
                                    format!("{}: '{raw}' is not a number", spec.label)
                                })?)
                            }
                            Kind::Sel(_)
                                if spec.key == "reasoning_preserve" || spec.key == "kv_unified" =>
                            {
                                Value::from(raw == "true")
                            }
                            _ => Value::from(raw),
                        };
                        args.insert(spec.key.to_string(), val);
                    }
                }
                Ok(())
            })?;
            let kw = kwargs.get_untracked();
            if kw.trim().is_empty() {
                clear.push("chat_template_kwargs");
            } else {
                args.insert("chat_template_kwargs".into(), Value::from(kw.trim()));
            }
            let cov = capabilities_override.get_untracked();
            if cov.trim().is_empty() {
                clear.push("capabilities_override");
            } else {
                args.insert("capabilities_override".into(), Value::from(cov.trim()));
            }
            args.insert("jinja".into(), Value::from(jinja.get_untracked()));
            args.insert("no_mmproj".into(), Value::from(no_mmproj.get_untracked()));
            args.insert("public".into(), Value::from(public.get_untracked()));
            args.insert("enabled".into(), Value::from(enabled.get_untracked()));
            args.insert(
                "idle_seconds".into(),
                Value::from(
                    idle_seconds
                        .get_untracked()
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| "idle seconds must be an integer".to_string())?,
                ),
            );
            args.insert("extra_args".into(), Value::from(extra_args.get_untracked()));
            let img = image.get_untracked();
            if img.trim().is_empty() {
                clear.push("image");
            } else {
                args.insert("image".into(), Value::from(img.trim()));
            }
            let era = extra_run_args.get_untracked();
            if era.trim().is_empty() {
                clear.push("extra_run_args");
            } else {
                args.insert("extra_run_args".into(), Value::from(era));
            }
            args.insert("warm_start".into(), Value::from(warm_start.get_untracked()));
            // Sending `hold_fallback_mode` always resets `hold_fallback` to
            // `None` server-side when the mode is not `alias`
            // (`ops::resolve_hold_fallback`), so there is no separate
            // `clear` entry to add here.
            let mode = hold_mode.get_untracked();
            args.insert("hold_fallback_mode".into(), Value::from(mode.as_str()));
            if mode == "alias" {
                let a = hold_alias.get_untracked().trim().to_string();
                if a.is_empty() {
                    return Err(
                        "hold fallback: pick the model to route to, or choose another mode".into(),
                    );
                }
                args.insert("hold_fallback".into(), Value::from(a));
            }
            let mid = model_id.get_untracked().trim().to_string();
            if mid.is_empty() {
                return Err("model id is required".into());
            }
            args.insert("model_id".into(), Value::from(mid));
            let gp = gguf_path.get_untracked().trim().to_string();
            if gp.is_empty() {
                return Err("a GGUF file is required".into());
            }
            args.insert("gguf_path".into(), Value::from(gp));
            // Ladder (ladder design §4.1, §6): the higher rungs, or `clear` when
            // the toggle is off — the same "filled → value, emptied → clear"
            // convention as `image`/`extra_run_args` above. Rows are already
            // shown as invalid by `rung_errors` before Save is even reachable
            // (`invalid` blocks it), but `collect` is the one place that has to
            // turn them into either real values or a hard refusal.
            if ladder_on.get_untracked() {
                let rows = rungs.get_untracked();
                if rows.is_empty() {
                    return Err(
                        "Ladder is on but has no rungs above the base — add at least one, or turn \
                     Ladder off"
                            .into(),
                    );
                }
                let mut ladder = Vec::with_capacity(rows.len());
                for (i, r) in rows.iter().enumerate() {
                    let rung_no = i + 2; // rung 1 is the base
                    let gp = r.gguf_path.get_untracked().trim().to_string();
                    if gp.is_empty() {
                        return Err(format!("rung {rung_no} needs a GGUF file"));
                    }
                    let raw = r.ctx_size.get_untracked();
                    let ctx_size = raw.trim().parse::<i64>().map_err(|_| {
                        format!("rung {rung_no}: '{}' is not an integer", raw.trim())
                    })?;
                    ladder.push(Rung {
                        gguf_path: gp,
                        ctx_size,
                    });
                }
                args.insert(
                    "ladder".into(),
                    serde_json::to_value(&ladder).map_err(|e| e.to_string())?,
                );
            } else {
                clear.push("ladder");
            }
            if !clear.is_empty() {
                args.insert("clear".into(), Value::from(clear.join(",")));
            }
            Ok(args)
        };

    let on_save = Callback::new(move |()| {
        // Asked for by "Save and test" — for this save only, whatever
        // becomes of it.
        let then_test = test_after_save.get_untracked();
        test_after_save.set(false);
        if saving.get_untracked() || invalid.get_untracked() > 0 {
            return;
        }
        let mut args = match collect() {
            Ok(a) => a,
            Err(e) => {
                save_error.set(Some(e));
                return;
            }
        };
        // The id the row is saved under — the form's, which may be a rename.
        let saved_mid = args
            .get("model_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if create_mode {
            args.insert("action".into(), json!("create"));
        } else {
            args.insert("action".into(), json!("update"));
            args.insert("id".into(), json!(row_id.unwrap_or_default()));
        }
        saving.set(true);
        save_error.set(None);
        let navigate = navigate.clone();
        spawn_local(async move {
            let res =
                crate::api::post::<Value, _>("/api/op/local_model_set", &Value::Object(args)).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    // Pickers everywhere list what the gateway serves; this
                    // just changed it.
                    catalog.refresh();
                    // Left while saving: the toast said how it went, and
                    // there is no form to update or navigate from.
                    if !scope.alive() {
                        return;
                    }
                    rebase();
                    if create_mode {
                        if let Some(id) = v.get("id").and_then(Value::as_i64) {
                            navigate(&format!("/models/local/{id}"), Default::default());
                        }
                    } else {
                        reload.update(|n| *n += 1);
                        // A container still up on the previous configuration
                        // (mid-request when the save stopped it) is what a
                        // test now would load: say so instead of passing it.
                        let kept = v
                            .get("container_kept_previous_config")
                            .and_then(Value::as_bool)
                            == Some(true);
                        if then_test && kept {
                            not_tested(
                                test_result,
                                format!(
                                    "Not tested: '{saved_mid}' is still serving a request on the \
                                     previous configuration, so a test now would load that one. \
                                     Test again once it is idle, or stop it first."
                                ),
                            );
                        } else if then_test {
                            run_load_test(ops, toasts, saved_mid, test_result);
                        }
                    }
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    });
    let on_discard = Callback::new(move |()| {
        baseline.with_untracked(|b| {
            entries.with_value(|es| {
                for (e, v) in es.iter().zip(b) {
                    e.val.write(v);
                }
            })
        });
        ladder_on.set(ladder_on_baseline.get_untracked());
        rungs.set(
            ladder_baseline
                .get_untracked()
                .into_iter()
                .map(|(g, c)| new_rung_row(next_rung_key, g, c))
                .collect(),
        );
        save_error.set(None);
    });

    // Re-plan: overwrite the form's params with what the planner derives from
    // the GGUF right now. Never saves by itself.
    let planning = RwSignal::new(false);
    let replan = move |_| {
        let gp = gguf_path.get_untracked().trim().to_string();
        if gp.is_empty() || planning.get_untracked() {
            return;
        }
        planning.set(true);
        scope.spawn(async move {
            let res = crate::api::get::<PlanResult>(format!(
                "/api/local-model-plan?path={}",
                urlenc(&gp)
            ))
            .await;
            planning.set(false);
            match res {
                Ok(plan) => {
                    match serde_json::from_value::<LlamaParams>(Value::Object(plan.params.clone()))
                    {
                        Ok(p) => {
                            fill(&p);
                            if model_id.get_untracked().trim().is_empty() {
                                model_id.set(plan.model_id.clone());
                            }
                            toasts.ok("form filled from the plan — review and save");
                        }
                        Err(e) => toasts.err(format!("plan did not parse: {e}")),
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // The editor's mirror of `LlamaParams::pool_unguarded_shared` (config/llama_params.rs):
    // this form edits a sparse patch, not a loaded `LlamaParams`, so the core
    // helper isn't reachable from wasm — the rule is duplicated here instead,
    // read straight off the same three fields the generic form already
    // tracks (§3.3's "slots share one pool unguarded" note).
    let kv_unified_note = move || {
        let ku = fields.with_value(|f| f["kv_unified"].get());
        let parallel = fields
            .with_value(|f| f["parallel"].get())
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 1);
        let slots = parallel.unwrap_or(4);
        // Mirrors `LlamaParams::effective_kv_unified` (config/llama_params.rs, review
        // finding 3): llama-server forces unified whenever `parallel` is
        // auto (`parallel` above is already `None` for unset *or*
        // non-positive), overriding an explicit "false" — so auto wins
        // outright and "false" only has a say once `parallel` names a real
        // slot count.
        let effective_unified = parallel.is_none() || ku == "true";
        let guarded = ku == "true"
            && slots > 1
            && fields
                .with_value(|f| f["n_predict"].get())
                .trim()
                .parse::<i64>()
                .is_ok_and(|n| n > 0);
        (effective_unified && slots > 1 && !guarded)
            .then(|| view! { <div class="notice warn">"slots share one pool unguarded"</div> })
    };

    // Every rung shares these with the base by construction (design §4.1):
    // gathered once so the table's per-rung derived columns (and the
    // footprint/MTP fetch) all read the same snapshot.
    let ladder_shared = Memo::new(move |_| {
        fields.with_value(|f| LadderShared {
            cache_type_k: f["cache_type_k"].get(),
            cache_type_v: f["cache_type_v"].get(),
            mmproj_path: f["mmproj_path"].get(),
            draft_gguf_path: f["draft_gguf_path"].get(),
            n_gpu_layers: f["n_gpu_layers"].get(),
            parallel: f["parallel"].get(),
            n_predict: f["n_predict"].get(),
        })
    });
    let add_rung = move |_| {
        rungs.update(|r| r.push(new_rung_row(next_rung_key, String::new(), String::new())));
    };

    // The row as saved: what the test loads, whatever the form says now.
    let saved = move || detail.get().flatten().map(|d| (d.model_id, d.enabled));
    let testing = Signal::derive(move || saved().is_some_and(|(mid, _)| ops.busy(&test_key(&mid))));
    let test_saved = Callback::new(move |()| {
        if let Some(mid) = detail.get_untracked().flatten().map(|d| d.model_id) {
            run_load_test(ops, toasts, mid, test_result);
        }
    });
    let save_and_test = Callback::new(move |()| {
        test_after_save.set(true);
        on_save.run(());
    });
    let on_test = move |_| {
        if dirty_count.get_untracked() > 0 {
            test_ask.set(true);
        } else {
            test_saved.run(());
        }
    };

    let title = if create_mode {
        "New local model"
    } else {
        "Local model"
    };
    let check = move |label: &'static str, sig: RwSignal<bool>, key: &'static str| {
        let d = is_dirty(key);
        view! {
            <label class="row dim check-line" class:dirty=move || d.get() style="gap:5px">
                <input
                    type="checkbox"
                    prop:checked=move || sig.get()
                    on:change=move |ev| sig.set(event_target_checked(&ev))
                />
                {label}
            </label>
        }
    };

    view! {
        <PageFrame
            title=title
            head_extra=move || view! { <span class="sub mono-sm">{move || model_id.get()}</span> }
            actions=move || {
                view! {
                    <a class="btn ghost" href="/models">
                        "Back to models"
                    </a>
                    {(!create_mode)
                        .then(|| {
                            view! {
                                <button
                                    class="btn"
                                    disabled=move || {
                                        testing.get() || saving.get()
                                            || !saved().is_some_and(|(_, enabled)| enabled)
                                    }
                                    title=move || {
                                        if saved().is_some_and(|(_, enabled)| !enabled) {
                                            "Enable it and save first: a disabled model is not served, so there is nothing to test"
                                        } else {
                                            "Load test on the saved model: starts its container if it is not running (which may evict others under the VRAM ledger), then generates one token"
                                        }
                                    }
                                    on:click=on_test
                                >
                                    {move || if testing.get() { "Testing…" } else { "Test" }}
                                </button>
                            }
                        })}
                    <button
                        class="btn"
                        disabled=move || planning.get()
                        title="Read the GGUF's metadata and fill the parameters from it; nothing is saved"
                        on:click=replan
                    >
                        {move || if planning.get() { "Planning…" } else { "Fill from plan" }}
                    </button>
                }
            }
            footer=move || {
                view! {
                    <SaveBar
                        dirty_count=dirty_count
                        detail=dirty_detail
                        invalid=invalid
                        saving=saving
                        error=save_error
                        on_save=on_save
                        on_discard=on_discard
                        save_label=if create_mode { "Create model" } else { "Save changes" }
                    />
                }
            }
        >
            <UnsavedTestModal
                open=test_ask
                detail=dirty_detail
                invalid=Signal::derive(move || invalid.get() > 0)
                saving=saving
                enabled_after=enabled
                on_save_and_test=save_and_test
                on_test_saved=test_saved
            />
            <Show when=move || loaded.get()>
                <div class="editor">
                    <div class="editor-main card-flow">
                        <section class="card edit-section">
                            <h3>"Identity"</h3>
                            <div class="field-grid">
                                <Field label="Model id" dirty=is_dirty("model_id")>
                                    <input
                                        class="input mono"
                                        prop:value=move || model_id.get()
                                        on:input=move |ev| model_id.set(event_target_value(&ev))
                                    />
                                </Field>
                                <Field
                                    label="Idle seconds before sleep"
                                    unit="0 = never"
                                    dirty=is_dirty("idle_seconds")
                                    error=error_of("idle_seconds")
                                >
                                    <input
                                        class="input mono"
                                        prop:value=move || idle_seconds.get()
                                        on:input=move |ev| idle_seconds.set(event_target_value(&ev))
                                    />
                                </Field>
                                // Rung 1 of the ladder table below once Ladder is on
                                // (design §6) — same field, same signal, just shown
                                // in the other place.
                                <Show when=move || !ladder_on.get()>
                                    <GgufField
                                        label="Weights (GGUF)"
                                        value=gguf_path
                                        role="weights"
                                        dirty=is_dirty("gguf_path")
                                    />
                                </Show>
                            </div>
                            <div class="row check-row">
                                {check("public (routable without an alias)", public, "public")}
                                {check("enabled", enabled, "enabled")}
                                {check("jinja (use the embedded chat template)", jinja, "jinja")}
                                {check("refuse auto-loaded projector", no_mmproj, "no_mmproj")}
                                {check("Ladder (a rung table instead of one GGUF)", ladder_on, "ladder")}
                            </div>
                        </section>

                        <Show when=move || ladder_on.get()>
                            <section class="card edit-section wide">
                                <h3>"Ladder " <span class="dim cmd-note">"design §4, §6"</span></h3>
                                <p class="field-hint wide">
                                    "One rung runs at a time, in this model's one container. A \
                                     request that does not fit the running rung climbs straight \
                                     to the smallest rung that fits, after the running rung has \
                                     drained — every other conversation on this model moves up \
                                     with it. It only comes back down when the container stops \
                                     (the idle reaper, an eviction, the hold, an apply or \
                                     restart); the next start is always the base rung."
                                </p>
                                <div class="table-scroll">
                                    <table class="data ladder-table">
                                        <thead>
                                            <tr>
                                                <th>"Rung"</th>
                                                <th>"GGUF"</th>
                                                <th>"Context"</th>
                                                <th class="num-h">"Per slot"</th>
                                                <th class="num-h">"Switchover"</th>
                                                <th class="num-h">"Footprint"</th>
                                                <th>"MTP"</th>
                                                <th></th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            <LadderRungRow
                                                rung_no=1
                                                gguf_path=gguf_path
                                                ctx_size=fields.with_value(|f| f["ctx_size"])
                                                shared=ladder_shared
                                                error=error_of("gguf_path")
                                            />
                                            <For
                                                each=move || enumerate_rungs(rungs.get())
                                                key=|(_, r)| r.key
                                                let:item
                                            >
                                                {
                                                    let (i, r) = item;
                                                    let key = r.key;
                                                    view! {
                                                        <LadderRungRow
                                                            rung_no=i + 2
                                                            gguf_path=r.gguf_path
                                                            ctx_size=r.ctx_size
                                                            shared=ladder_shared
                                                            error=rung_error_of(key)
                                                            on_remove=Callback::new(move |()| {
                                                                rungs.update(|rows| rows.retain(|x| x.key != key));
                                                            })
                                                        />
                                                    }
                                                }
                                            </For>
                                        </tbody>
                                    </table>
                                </div>
                                <div class="row" style="align-items:center">
                                    <button type="button" class="btn ghost sm" on:click=add_rung>
                                        "+ Add rung"
                                    </button>
                                    <div class="spacer" style="flex:1"></div>
                                    <Field
                                        label="Max output"
                                        hint="--n-predict, required on a ladder — every request is clamped to it"
                                        dirty=is_dirty("n_predict")
                                        error=error_of("n_predict")
                                    >
                                        <input
                                            class="input mono"
                                            style="max-width:10rem"
                                            prop:value=move || fields.with_value(|f| f["n_predict"].get())
                                            on:input=move |ev| {
                                                fields
                                                    .with_value(|f| f["n_predict"].set(event_target_value(&ev)))
                                            }
                                        />
                                    </Field>
                                </div>
                                {move || {
                                    ladder_table_error
                                        .get()
                                        .map(|m| view! { <div class="field-err" role="alert">{m}</div> })
                                }}
                            </section>
                        </Show>

                        <section class="card edit-section">
                            <h3>"Container"</h3>
                            {check("start at boot (warm start)", warm_start, "warm_start")}
                            <Field
                                label="Hold fallback"
                                hint="While a GPU hold is on. Inherit follows the global fallback; None refuses with a 503 even when one is set."
                                hint_extra=|| {
                                    view! {
                                        <a href=super::settings::href("hold.fallback_alias")>
                                            "Settings → GPU → Hold"
                                        </a>
                                    }
                                }
                                dirty=Signal::derive(move || {
                                    is_dirty("hold_fallback_mode").get() || is_dirty("hold_fallback").get()
                                })
                            >
                                <HoldFallback
                                    mode=hold_mode
                                    alias=hold_alias
                                    inherit_label="Inherit · global fallback"
                                    tasks=&["chat"]
                                />
                            </Field>
                            <div class="field-grid">
                                <Field label="Image override" dirty=is_dirty("image") wide=true>
                                    <ImagePicker
                                        value=image
                                        class=ImageClass::Chat
                                        clearable=true
                                        placeholder=Signal::derive(move || {
                                            class_settings
                                                .get()
                                                .and_then(|r| r.ok())
                                                .map(|s| s.router.image)
                                                .unwrap_or_default()
                                        })
                                    />
                                </Field>
                                <Field
                                    label="Extra podman run args override"
                                    unit="one per line"
                                    hint="An override replaces the class args whole: to run without some of them, list the ones to keep (an empty override inherits). Blank fields inherit the chat class settings:"
                                    hint_extra=|| {
                                        view! {
                                            <a href=super::settings::href("router.extra_run_args")>
                                                "Settings → Runtimes → Chat"
                                            </a>
                                        }
                                    }
                                    dirty=is_dirty("extra_run_args")
                                    wide=true
                                >
                                    <textarea
                                        class="input mono ta"
                                        placeholder=move || {
                                            class_settings
                                                .get()
                                                .and_then(|r| r.ok())
                                                .map(|s| s.router.extra_run_args.join("\n"))
                                                .unwrap_or_default()
                                        }
                                        prop:value=move || extra_run_args.get()
                                        on:input=move |ev| extra_run_args.set(event_target_value(&ev))
                                    ></textarea>
                                </Field>
                            </div>
                        </section>

                        {SECTIONS
                            .iter()
                            .map(|(section_title, specs)| {
                                view! {
                                    <section class="card edit-section">
                                        <h3>{*section_title}</h3>
                                        <div class="field-grid">
                                            {specs
                                                .iter()
                                                .map(|spec| {
                                                    let sig = fields.with_value(|f| f[spec.key]);
                                                    view! {
                                                        <SpecField
                                                            spec=spec
                                                            sig=sig
                                                            dirty=is_dirty(spec.key)
                                                            error=error_of(spec.key)
                                                        />
                                                    }
                                                })
                                                .collect_view()}
                                        </div>
                                        {(*section_title == "Context & batch")
                                            .then(|| {
                                                view! {
                                                    <div class="kv-unified-explain">
                                                        <div class="field-hint wide">
                                                            "Unified KV lets every parallel slot share one KV pool, \
                                                             so a single conversation can use the whole context \
                                                             instead of being capped to ctx_size / parallel slots. \
                                                             llama-server aborts every running request when a \
                                                             shared pool overflows, which is why lmgw guards an \
                                                             explicitly unified row with a token ledger and needs \
                                                             Max output (n_predict) to bound it. Default follows \
                                                             llama-server's own choice: unified exactly when \
                                                             Parallel slots is left blank."
                                                        </div>
                                                        {kv_unified_note}
                                                    </div>
                                                }
                                            })}
                                    </section>
                                }
                            })
                            .collect_view()}

                        <section class="card edit-section">
                            <h3>"Template variables & extra flags"</h3>
                            <div class="field-grid">
                                <Field
                                    label="chat_template_kwargs"
                                    unit="JSON object"
                                    dirty=is_dirty("chat_template_kwargs")
                                >
                                    <textarea
                                        class="input mono ta"
                                        prop:value=move || kwargs.get()
                                        on:input=move |ev| kwargs.set(event_target_value(&ev))
                                    ></textarea>
                                </Field>
                                <Field
                                    label="Extra llama-server flags"
                                    unit="one per line"
                                    dirty=is_dirty("extra_args")
                                >
                                    <textarea
                                        class="input mono ta"
                                        prop:value=move || extra_args.get()
                                        on:input=move |ev| extra_args.set(event_target_value(&ev))
                                    ></textarea>
                                </Field>
                            </div>
                        </section>

                        <section class="card edit-section">
                            <h3>"Owner overrides"</h3>
                            <Field
                                label="Capability override"
                                unit="JSON object"
                                hint="Merged over what lmgw derives for /v1/models — optional keys capabilities (deep-merged), max_output_tokens, notes (appended). Only for facts the GGUF does not state; published with source \"owner\". Blank clears."
                                dirty=is_dirty("capabilities_override")
                            >
                                <textarea
                                    class="input mono ta"
                                    prop:value=move || capabilities_override.get()
                                    on:input=move |ev| capabilities_override.set(event_target_value(&ev))
                                ></textarea>
                            </Field>
                        </section>
                    </div>

                    <aside class="editor-aside">
                        {(!create_mode)
                            .then(|| {
                                view! {
                                    <section class="card edit-section">
                                        <ContainerStatusRow
                                            class="chat"
                                            model_id=Signal::derive(move || {
                                                detail
                                                    .get()
                                                    .flatten()
                                                    .map(|d| d.model_id)
                                                    .unwrap_or_default()
                                            })
                                        />
                                    </section>
                                    <LoadTestCard testing=testing result=test_result/>
                                }
                            })}
                        {move || {
                            let problems = detail
                                .get()
                                .flatten()
                                .map(|d| d.problems)
                                .unwrap_or_default();
                            (!problems.is_empty())
                                .then(|| {
                                    view! {
                                        <section class="card edit-section problems">
                                            <h3>"Problems " <span class="count bad">{problems.len()}</span></h3>
                                            {problems
                                                .iter()
                                                .map(|p| view! { <div class="problem">{p.clone()}</div> })
                                                .collect_view()}
                                        </section>
                                    }
                                })
                        }}
                        <section class="card edit-section cmd-card">
                            <h3>"Command line " <span class="dim cmd-note">"as saved"</span></h3>
                            {move || match detail.get().flatten() {
                                Some(d) => {
                                    view! {
                                        <div class="cmd-head">
                                            <span class="dim">
                                                {move || {
                                                    if dirty_count.get() > 0 {
                                                        "Unsaved edits are not in it yet."
                                                    } else {
                                                        "What this model's own container is started with; the published port is allocated per start."
                                                    }
                                                }}
                                            </span>
                                            <CopyBtn text=d.command_line.clone() title="Copy the command as one line"/>
                                        </div>
                                        <pre class="preset cmd-lines">{flag_lines(&d.command_line)}</pre>
                                    }
                                        .into_any()
                                }
                                None if create_mode => {
                                    view! { <p class="dim">"Rendered once the model is saved."</p> }.into_any()
                                }
                                None => view! { <p class="dim">"Loading…"</p> }.into_any(),
                            }}
                        </section>
                    </aside>
                </div>
            </Show>
        </PageFrame>
    }
}

#[component]
fn SpecField(
    spec: &'static Spec,
    sig: RwSignal<String>,
    #[prop(into)] dirty: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
) -> impl IntoView {
    view! {
        <div class="field" class:dirty=move || dirty.get() class:invalid=move || error.get().is_some()>
            <label title=spec.hint>
                {spec.label}
                {(!spec.hint.is_empty()).then(|| view! { <span class="field-unit">{spec.hint}</span> })}
            </label>
            {match spec.kind {
                Kind::Sel(options) => {
                    let opts: Vec<(String, String)> = options
                        .iter()
                        .map(|(v, l)| (v.to_string(), l.to_string()))
                        .collect();
                    view! { <Select value=sig options=Signal::derive(move || opts.clone())/> }
                        .into_any()
                }
                _ => view! {
                    <input
                        class="input mono"
                        prop:value=move || sig.get()
                        on:input=move |ev| sig.set(event_target_value(&ev))
                    />
                }
                    .into_any(),
            }}
            {move || error.get().map(|m| view! { <div class="field-err" role="alert">{m}</div> })}
        </div>
    }
}

/// GGUF path input + "browse" modal over /api/gguf-files — the shared half of
/// [`GgufField`] (a normal form row) and the ladder table's per-rung cell
/// ([`LadderRungRow`]), which wants the same picker without the field
/// wrapper and label.
#[component]
fn GgufPicker(value: RwSignal<String>, role: &'static str) -> impl IntoView {
    let open = RwSignal::new(false);
    let files = LocalResource::new(move || {
        let want = open.get();
        async move {
            if !want {
                return None;
            }
            crate::api::get::<GgufFiles>("/api/gguf-files").await.ok()
        }
    });
    view! {
        <div style="display:contents">
            <div class="row" style="flex-wrap:nowrap">
                <input
                    class="input mono"
                    style="flex:1"
                    prop:value=move || value.get()
                    on:input=move |ev| value.set(event_target_value(&ev))
                />
                <button class="btn" on:click=move |_| open.set(true)>
                    "Browse"
                </button>
            </div>
            <Modal open=open title="GGUF files in the models dir" fill=true>
                {move || match files.get().flatten() {
                    None => view! { <div class="dim">"Scanning…"</div> }.into_any(),
                    Some(gf) => {
                        let mut fs = gf.files;
                        fs.sort_by(|a, b| {
                            (a.role_guess != role)
                                .cmp(&(b.role_guess != role))
                                .then(a.path.cmp(&b.path))
                        });
                        view! {
                            <div class="wiz-files fill-pane">
                                <For each=move || fs.clone() key=|f| f.path.clone() let:f>
                                    <button
                                        class="wiz-file"
                                        on:click={
                                            let path = f.path.clone();
                                            move |_| {
                                                value.set(path.clone());
                                                open.set(false);
                                            }
                                        }
                                    >
                                        <span class="mono-sm">{f.path.clone()}</span>
                                        <span class="type-badge">{f.role_guess.clone()}</span>
                                        {(!f.used_by.is_empty())
                                            .then(|| {
                                                view! {
                                                    <span class="dim">
                                                        {format!("used by {}", f.used_by.join(", "))}
                                                    </span>
                                                }
                                            })}
                                        <span class="spacer" style="flex:1"></span>
                                        <span class="dim mono-sm">{f.size.clone()}</span>
                                    </button>
                                </For>
                            </div>
                        }
                            .into_any()
                    }
                }}
            </Modal>
        </div>
    }
}

/// GGUF path input + "browse" modal over /api/gguf-files — the exception path
/// for files the HF wizard did not download.
#[component]
fn GgufField(
    label: &'static str,
    value: RwSignal<String>,
    role: &'static str,
    #[prop(into)] dirty: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="field wide" class:dirty=move || dirty.get()>
            <label>{label}</label>
            <GgufPicker value=value role=role/>
        </div>
    }
}

/// The footprint/MTP cells' fallback when `/api/ladder-rung-plan` has not
/// resolved to a value (second pass, S9's third bullet): the error's own
/// message, shown as the tooltip on a red dash, when the fetch itself failed
/// — an unresolved `mmproj_path`/`draft_gguf_path` (S10), say — versus a
/// plain dim dash while it is merely unknown or still in flight. A
/// permanent "—" with no way to tell those two apart reads exactly like a
/// stuck spinner; this is the one place that tells them apart.
fn unknown_or_err(err: Option<String>) -> impl IntoView {
    match err {
        Some(e) => view! { <span class="status-err" title=e>{"—"}</span> }.into_any(),
        None => view! { <span class="dim">{"—"}</span> }.into_any(),
    }
}

/// One row of the ladder table (design §6): the base (rung 1, bound to the
/// row's own `gguf_path`/`ctx_size` — the same fields it always had) and
/// every higher rung render through this one component, so the two shapes
/// cannot drift.
///
/// Per-slot context and switchover are arithmetic over `shared` and need no
/// round trip; footprint and MTP come from `/api/ladder-rung-plan`, debounced
/// so a rung's whole row of numbers settles a beat after the owner stops
/// typing rather than on every keystroke.
#[component]
fn LadderRungRow(
    rung_no: usize,
    gguf_path: RwSignal<String>,
    ctx_size: RwSignal<String>,
    #[prop(into)] shared: Signal<LadderShared>,
    #[prop(into)] error: Signal<Option<String>>,
    /// `None` for the base row — rung 1 is never removed, only replaced by
    /// turning Ladder off.
    #[prop(optional)]
    on_remove: Option<Callback<()>>,
) -> impl IntoView {
    let configured = move || per_slot_ctx(&ctx_size.get(), &shared.get().parallel);

    let plan_key =
        Memo::new(move |_| rung_plan_query(&gguf_path.get(), &ctx_size.get(), &shared.get()));
    let plan_key = debounce_key(plan_key);
    // The `Result` is kept, not `.ok()`-discarded (second pass, S9's third
    // bullet): the endpoint can genuinely refuse now (an unresolved
    // `mmproj_path`/`draft_gguf_path`, S10), and a permanent "—" with no
    // reason reads exactly like a fetch that is still in flight — indistinct
    // from a stuck spinner. `crate::api::Error` and `RungPlan` are both
    // `Clone`, so the resource holding the whole `Result` costs nothing extra.
    let plan = LocalResource::new(move || {
        let q = plan_key.get();
        async move {
            match q {
                Some(q) => {
                    Some(crate::api::get::<RungPlan>(format!("/api/ladder-rung-plan?{q}")).await)
                }
                None => None,
            }
        }
    });
    let plan_ok = move || plan.get().flatten().and_then(|r| r.ok());
    let plan_err = move || {
        plan.get()
            .flatten()
            .and_then(|r| r.err())
            .map(|e| e.to_string())
    };
    let footprint_disp = move || plan_ok().map(|p| human_bytes(p.footprint.total_bytes));
    // The slot the rung really gets: what is configured, capped at the
    // weights' trained context once the plan has read it (review finding 1).
    let trained = move || plan_ok().and_then(|p| p.trained_context);
    let per_slot = move || configured().map(|ps| capped_slot(ps, trained()));
    let switchover_disp =
        move || per_slot().and_then(|(ps, _)| switchover(ps, &shared.get().n_predict));
    let capped_note = move || match (per_slot(), configured(), trained()) {
        (Some((_, true)), Some(ps), Some(t)) => Some(format!(
            "{} configured, but the weights are trained at {} — llama-server caps the slot \
             there, so this rung cannot be saved; lower its context",
            grouped_signed(ps),
            grouped(t)
        )),
        _ => None,
    };

    view! {
        <tr class:invalid=move || error.get().is_some()>
            <td class="dim mono-sm">{rung_no}</td>
            <td>
                <GgufPicker value=gguf_path role="weights"/>
                {move || error.get().map(|m| view! { <div class="field-err" role="alert">{m}</div> })}
            </td>
            <td>
                <input
                    class="input mono"
                    style="max-width:8rem"
                    prop:value=move || ctx_size.get()
                    on:input=move |ev| ctx_size.set(event_target_value(&ev))
                />
            </td>
            <td class="num mono-sm">
                {move || per_slot().map(|(ps, _)| grouped_signed(ps)).unwrap_or_else(|| "—".to_string())}
                {move || capped_note().map(|m| view! { <div class="field-warn">{m}</div> })}
            </td>
            <td class="num mono-sm">
                {move || switchover_disp().map(grouped_signed).unwrap_or_else(|| "—".to_string())}
            </td>
            <td class="num mono-sm">
                {move || match footprint_disp() {
                    Some(v) => view! { <span>{v}</span> }.into_any(),
                    None => unknown_or_err(plan_err()).into_any(),
                }}
            </td>
            <td class="mono-sm" style="text-align:center">
                {move || match plan_ok() {
                    Some(p) if p.has_mtp_layers => view! { <span>{"✓"}</span> }.into_any(),
                    Some(_) => view! { <span class="dim">{"—"}</span> }.into_any(),
                    None => unknown_or_err(plan_err()).into_any(),
                }}
            </td>
            <td class="actions">
                {on_remove
                    .map(|remove| {
                        view! {
                            <button
                                type="button"
                                class="btn ghost sm"
                                on:click=move |_| remove.run(())
                            >
                                "Remove"
                            </button>
                        }
                    })}
            </td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        capped_slot, effective_slots, flag_lines, grouped_signed, ladder_is_dirty, per_slot_ctx,
        rung_plan_pairs, shell_tokens, switchover, LadderShared,
    };

    #[test]
    fn quoted_parts_stay_one_token() {
        assert_eq!(
            shell_tokens("podman run --label 'a=b c' -v \"x y\":/m"),
            ["podman", "run", "--label", "'a=b c'", "-v", "\"x y\":/m"]
        );
    }

    #[test]
    fn a_command_line_reads_one_option_per_line() {
        let cmd = "podman run -d --replace --label 'lmgw.class=chat' -p 0:8080 \
                   localhost/llama:latest -m /models/x.gguf --jinja --seed -1 --temp 0.15";
        assert_eq!(
            flag_lines(cmd),
            "podman run\n-d\n--replace\n--label 'lmgw.class=chat'\n-p 0:8080\n\
             localhost/llama:latest\n-m /models/x.gguf\n--jinja\n--seed -1\n--temp 0.15"
        );
    }

    // ------------------------------------------------------------------
    // Ladder table arithmetic (design §4.2) — the UI's mirror of
    // `lmgw_core::ladder::LocalModel::per_slot_ctx`/`switchover`. The
    // runtime rung/climbing badges themselves need a live registry entry to
    // show anything (no model is started in this crate's tests, and there
    // is no DOM-mounting test harness here — no `wasm-bindgen-test`, every
    // other `#[cfg(test)]` module in this crate is plain-Rust helpers like
    // these), so the pure derivations are what is covered here; the rest was
    // checked live on the dev instance (see the WP5 report).
    // ------------------------------------------------------------------

    #[test]
    fn effective_slots_is_parallel_or_the_auto_default_of_four() {
        assert_eq!(effective_slots(""), 4);
        assert_eq!(effective_slots("0"), 4);
        assert_eq!(effective_slots("-1"), 4);
        assert_eq!(effective_slots("garbage"), 4);
        assert_eq!(effective_slots("2"), 2);
    }

    #[test]
    fn a_slot_is_capped_at_the_trained_context_and_says_so() {
        assert_eq!(capped_slot(8192, Some(4096)), (4096, true));
        assert_eq!(capped_slot(4096, Some(4096)), (4096, false));
        assert_eq!(capped_slot(2048, Some(4096)), (2048, false));
        assert_eq!(
            capped_slot(8192, None),
            (8192, false),
            "unknown caps nothing"
        );
    }

    #[test]
    fn ladder_is_dirty_ignores_leftover_rungs_once_the_toggle_is_back_to_baseline() {
        // Second pass S8: on, add a rung, off again — the toggle matches the
        // baseline (both off), so the leftover rung must not count.
        let leftover = vec![("top.gguf".to_string(), "16384".to_string())];
        assert!(!ladder_is_dirty(false, false, &leftover, &[]));
        // Same shape the other way: baseline had a ladder, toggled off then
        // back on with the rungs untouched.
        let baseline = vec![("top.gguf".to_string(), "16384".to_string())];
        assert!(!ladder_is_dirty(true, true, &baseline, &baseline));
    }

    #[test]
    fn ladder_is_dirty_when_the_toggle_itself_differs_from_the_baseline() {
        // The toggle flipping is dirty on its own, whatever the rung lists
        // say — Save's `ladder` vs `clear: "ladder"` differ either way.
        assert!(ladder_is_dirty(true, false, &[], &[]));
        assert!(ladder_is_dirty(false, true, &[], &[]));
        let same = vec![("top.gguf".to_string(), "16384".to_string())];
        assert!(ladder_is_dirty(false, true, &same, &same));
    }

    #[test]
    fn ladder_is_dirty_compares_rungs_once_the_toggle_agrees() {
        let baseline = vec![("top.gguf".to_string(), "16384".to_string())];
        let edited = vec![("top.gguf".to_string(), "32768".to_string())];
        assert!(ladder_is_dirty(true, true, &edited, &baseline));
        assert!(!ladder_is_dirty(true, true, &baseline, &baseline));
    }

    #[test]
    fn per_slot_ctx_divides_by_slots_and_is_none_until_ctx_parses() {
        assert_eq!(per_slot_ctx("32768", "2"), Some(16384));
        assert_eq!(per_slot_ctx("32768", ""), Some(32768 / 4));
        assert_eq!(per_slot_ctx("", "2"), None);
        assert_eq!(per_slot_ctx("not a number", "2"), None);
        assert_eq!(per_slot_ctx("0", "2"), None);
    }

    #[test]
    fn switchover_is_per_slot_minus_max_output_and_needs_a_positive_one() {
        assert_eq!(switchover(16384, "512"), Some(16384 - 512));
        assert_eq!(switchover(16384, ""), None);
        assert_eq!(switchover(16384, "0"), None);
        assert_eq!(switchover(16384, "-1"), None);
    }

    #[test]
    fn grouped_signed_groups_negative_numbers_too() {
        assert_eq!(grouped_signed(16384), "16\u{202F}384");
        assert_eq!(grouped_signed(-512), "−512");
        assert_eq!(grouped_signed(0), "0");
    }

    fn shared(k: &str, v: &str, mmproj: &str, draft: &str, ngl: &str) -> LadderShared {
        LadderShared {
            cache_type_k: k.to_string(),
            cache_type_v: v.to_string(),
            mmproj_path: mmproj.to_string(),
            draft_gguf_path: draft.to_string(),
            n_gpu_layers: ngl.to_string(),
            parallel: String::new(),
            n_predict: String::new(),
        }
    }

    #[test]
    fn rung_plan_pairs_is_none_until_gguf_and_ctx_are_both_real() {
        let s = shared("", "", "", "", "");
        assert_eq!(rung_plan_pairs("", "4096", &s), None);
        assert_eq!(rung_plan_pairs("top.gguf", "", &s), None);
        assert_eq!(rung_plan_pairs("top.gguf", "not a number", &s), None);
        assert!(rung_plan_pairs("top.gguf", "4096", &s).is_some());
    }

    #[test]
    fn rung_plan_pairs_carries_only_the_shared_fields_that_are_set() {
        let s = shared("q8_0", "", "", "", "");
        let pairs = rung_plan_pairs("weights/top.gguf", "4096", &s).unwrap();
        assert!(pairs.contains(&("gguf_path", "weights/top.gguf".to_string())));
        assert!(pairs.contains(&("ctx_size", "4096".to_string())));
        assert!(pairs.contains(&("cache_type_k", "q8_0".to_string())));
        assert!(!pairs.iter().any(|(k, _)| *k == "cache_type_v"));
        assert!(!pairs.iter().any(|(k, _)| *k == "mmproj_path"));
        assert!(!pairs.iter().any(|(k, _)| *k == "draft_gguf_path"));
        assert!(!pairs.iter().any(|(k, _)| *k == "n_gpu_layers"));
    }

    #[test]
    fn rung_plan_pairs_passes_n_gpu_layers_only_when_it_parses() {
        let s = shared("", "", "", "", "999");
        let pairs = rung_plan_pairs("m.gguf", "4096", &s).unwrap();
        assert!(pairs.contains(&("n_gpu_layers", "999".to_string())));
        let s = shared("", "", "", "", "auto");
        let pairs = rung_plan_pairs("m.gguf", "4096", &s).unwrap();
        assert!(!pairs.iter().any(|(k, _)| *k == "n_gpu_layers"));
    }
}
