//! The ladder (ladder design §4.1–§4.2, §6): a rung table instead of one
//! GGUF. The rung table's own arithmetic mirrors
//! `lmgw_core::ladder::LocalModel::per_slot_ctx`/`switchover`, which lives in
//! lmgw-core and is not reachable from wasm — see that module's docs for why
//! the two derived numbers are duplicated here rather than shared.
//! Footprint and MTP are not: those need a GGUF header read, so they come
//! from `/api/ladder-rung-plan`.
//!
//! Also the GGUF file browser every file row of the editor uses.

use std::time::Duration;

use leptos::prelude::*;
use lmgw_api_types::{GgufFiles, RungPlan};

use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::Modal;

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// One rung above the base, as the table edits it: a raw-text pair like every
/// other numeric field on this form (so an in-progress edit — a half-typed
/// context size — shows its own parse error instead of silently discarding
/// the keystroke).
#[derive(Clone, Copy)]
pub(super) struct RungRow {
    /// Stable across reorders — there is no reordering here, but `<For>`
    /// still needs a key that outlives an index.
    pub(super) key: u64,
    pub(super) gguf_path: RwSignal<String>,
    pub(super) ctx_size: RwSignal<String>,
}

/// Fresh `RungRow`, keyed from the shared counter — a plain fn rather than a
/// closure so every call site (fill-from-load, discard, Add rung) can share
/// one counter without fighting over which of them owns the closure.
pub(super) fn new_rung_row(
    next_key: StoredValue<u64>,
    gguf_path: String,
    ctx_size: String,
) -> RungRow {
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
pub(super) fn enumerate_rungs(rows: Vec<RungRow>) -> Vec<(usize, RungRow)> {
    rows.into_iter().enumerate().collect()
}

/// The fields every rung shares with the base (design §4.1): template,
/// reasoning, sampling, projector, drafter, cache types, `parallel` — of
/// which only the ones the footprint/MTP call and the arithmetic actually
/// read are gathered here, straight off the same `fields` signals the rest
/// of the form edits.
#[derive(Clone, PartialEq)]
pub(super) struct LadderShared {
    pub(super) cache_type_k: String,
    pub(super) cache_type_v: String,
    pub(super) mmproj_path: String,
    pub(super) draft_gguf_path: String,
    pub(super) n_gpu_layers: String,
    pub(super) n_cpu_moe: String,
    pub(super) moe_cache_mib: String,
    pub(super) parallel: String,
    pub(super) n_predict: String,
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
pub(super) fn ladder_is_dirty(
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
    for (k, v) in [
        ("n_gpu_layers", &shared.n_gpu_layers),
        ("n_cpu_moe", &shared.n_cpu_moe),
        ("moe_cache_mib", &shared.moe_cache_mib),
    ] {
        if let Ok(n) = v.trim().parse::<i64>() {
            pairs.push((k, n.to_string()));
        }
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

/// "Browse" and its modal over `/api/gguf-files`: the files in the models
/// dir, those whose name looks like `role` first. A pick writes the path
/// into `value`. Beside every file field of the editor and each rung's.
#[component]
pub(super) fn GgufBrowse(value: RwSignal<String>, role: &'static str) -> impl IntoView {
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
        <button type="button" class="btn" on:click=move |_| open.set(true)>
            "Browse"
        </button>
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
pub(super) fn LadderRungRow(
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
    let footprint_disp = move || plan_ok().map(|p| fmt_bytes(p.footprint.total_bytes));
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
                <div class="row" style="flex-wrap:nowrap">
                    <input
                        class="input mono"
                        style="flex:1; min-width:0"
                        aria-label=format!("Rung {rung_no} GGUF")
                        prop:value=move || gguf_path.get()
                        on:input=move |ev| gguf_path.set(event_target_value(&ev))
                    />
                    <GgufBrowse value=gguf_path role="weights"/>
                </div>
                {move || error.get().map(|m| view! { <div class="field-err" role="alert">{m}</div> })}
            </td>
            <td>
                <input
                    class="input mono"
                    style="max-width:8rem"
                    aria-label=format!("Rung {rung_no} context")
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
        capped_slot, effective_slots, grouped_signed, ladder_is_dirty, per_slot_ctx,
        rung_plan_pairs, switchover, LadderShared,
    };

    // ------------------------------------------------------------------
    // Ladder table arithmetic (design §4.2) — the UI's mirror of
    // `lmgw_core::ladder::LocalModel::per_slot_ctx`/`switchover`. The
    // runtime rung/climbing badges themselves need a live registry entry to
    // show anything (no model is started in this crate's tests, and there
    // is no DOM-mounting test harness here — no `wasm-bindgen-test`, every
    // other `#[cfg(test)]` module in this crate is plain-Rust helpers like
    // these), so the pure derivations are what is covered here; the rest was
    // checked live on a dev instance.
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
            n_cpu_moe: String::new(),
            moe_cache_mib: String::new(),
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

    /// Every rung keeps the base row's experts in host memory, so its
    /// footprint is asked with them.
    #[test]
    fn rung_plan_pairs_carries_the_expert_offload() {
        let mut s = shared("", "", "", "", "");
        s.n_cpu_moe = "41".into();
        s.moe_cache_mib = "6144".into();
        let pairs = rung_plan_pairs("m.gguf", "4096", &s).unwrap();
        assert!(pairs.contains(&("n_cpu_moe", "41".to_string())));
        assert!(pairs.contains(&("moe_cache_mib", "6144".to_string())));
    }
}
