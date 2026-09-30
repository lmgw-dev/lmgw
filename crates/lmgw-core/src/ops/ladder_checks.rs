//! Ladder validation (ladder design §4.3): the unified-KV save-time
//! refusal, the freeform-flag helpers rule 6 depends on, and every rung
//! check from `validate_ladder` through `ladder_rung_plan`.

use serde_json::{json, Value};

use crate::runtime::Class;
use crate::state::SharedState;
use crate::store::NewLocalModel;

/// Save-time refusal for the unified-KV toggle (unified-KV design §3.3,
/// **decision D2**, binding). Runs on the *effective* values, from
/// [`crate::config::LlamaParams`]'s own helpers, so a row that reaches an
/// explicit split or unified state through defaults is judged exactly like
/// one that spells it out — this is the one place both `local_model_set`
/// arms (create and update) route through, so the dashboard and MCP both get
/// it (they share this one function). Both arms run this *after*
/// [`crate::config::hoist_promoted_args_into`] folds any promoted flag
/// (`--kv-unified`/`-kvu`, …) sitting in that save's freeform `extra_args`
/// into these typed fields — otherwise the very same refusal is one flag
/// spelling away from being skipped entirely until the row's next load
/// (review finding 3).
///
/// Three independent problems:
/// - **An explicit split that cannot actually split.** llama-server ignores
///   `--no-kv-unified` and forces unified — and 4 slots — whenever `parallel`
///   is left on auto: "n_parallel is set to auto, using n_parallel = 4 and
///   kv_unified = true" (`tools/server/server.cpp:156-161`, review finding
///   3). So `kv_unified: false` needs `parallel` fixed to a real slot count
///   (`≥ 1`) or it is a no-op the row's own command line contradicts —
///   [`crate::config::LlamaParams::effective_kv_unified`] would report it as
///   unified anyway.
/// - **An explicitly shared pool with nothing bounding it, or of unknown
///   size.** `kv_unified: true` with more than one effective slot needs
///   `n_predict` to guard the pool ledger ([`crate::gate::pool`]) — without
///   it a reservation has no ceiling to reserve against. It also needs the
///   pool ledger to actually know how big the pool is: `--fit` (default on)
///   shrinks an *unset* `ctx_size` to whatever device memory allows, so the
///   trained context is never a safe stand-in for the pool's real size
///   (review finding 2) — `ctx_size` (a positive value; llama-server reads
///   `0` as "from model", i.e. unset) or `kv_unified_per_slot` has to be set
///   too. Neither check fires when `kv_unified` is left `None`:
///   llama-server's own auto default reaching a shared pool is "left alone"
///   (spec §1), unguarded, on purpose — see
///   [`crate::config::LlamaParams::pool_guarded`].
/// - **A per-slot cap that cannot do anything.** `--kv-unified-per-slot`
///   help text is explicit that it only sizes/caps the *shared* pool
///   ("context limit per parallel slot … when set without -c, the shared KV
///   pool is sized to n_parallel*N"); on a split row each slot already gets
///   its own `ctx_size / parallel`, so setting it there can only be a stale
///   leftover from switching unified off — refused rather than silently
///   doing nothing. A non-positive cap is refused outright too: llama-server
///   reads it as a token count, never a sentinel.
pub(super) fn validate_kv_unified(params: &crate::config::LlamaParams) -> Result<(), String> {
    if params.kv_unified == Some(false) && !matches!(params.parallel, Some(n) if n >= 1) {
        return Err(
            "kv_unified=false has no effect while parallel is left on auto — llama-server \
             always unifies the KV cache (and uses 4 slots) whenever parallel is unset or \
             non-positive (\"n_parallel is set to auto … kv_unified = true\"); set parallel to \
             a real slot count (1 or more) to actually split the pool"
                .to_string(),
        );
    }
    if params.kv_unified == Some(true) && params.effective_slots() > 1 {
        if params.n_predict.is_none_or(|n| n <= 0) {
            return Err(
                "unified KV is on with more than one slot, so a shared pool has no bound to guard \
                 it — set Max output (n_predict), or turn unified KV off / leave it at default"
                    .to_string(),
            );
        }
        if params.ctx_size.is_none_or(|c| c <= 0) && params.kv_unified_per_slot.is_none() {
            return Err(
                "unified KV is on with more than one slot, so the pool ledger needs to know how \
                 big the shared pool is — llama-server's own --fit can shrink an unset ctx_size \
                 to whatever memory allows, so it is not a safe stand-in; set ctx_size (a \
                 positive value) or kv_unified_per_slot"
                    .to_string(),
            );
        }
    }
    if let Some(n) = params.kv_unified_per_slot {
        if n <= 0 {
            return Err("kv_unified_per_slot must be a positive token count".to_string());
        }
        if !params.effective_kv_unified() {
            return Err(
                "kv_unified_per_slot only sizes/caps a shared (unified) KV pool — it does \
                 nothing on a split row; turn unified KV on, or clear kv_unified_per_slot"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// §4.3's save-time refusals for a ladder row (ladder design). Runs on the
/// *effective* row — `params`/`args` after [`crate::config::hoist_promoted_args_into`]
/// folds any freeform *promoted* flag (`--kv-unified`, `--spec-type`,
/// `--model-draft`, …) into the typed fields this reads, the same ordering
/// [`validate_kv_unified`] depends on (review finding 3; `spec-type`/
/// `model-draft` joined `PROMOTED_ARGS` for the identical reason, review
/// finding 12): a freeform flag must not let a save bypass a rule the typed
/// field would have caught. `ctx_size`, `parallel` and the gguf itself
/// (`-m`) are never hoisted and need no such ordering (review finding 15,
/// correcting an earlier version of this comment that claimed they were):
/// none of the three is in `PROMOTED_ARGS`, because none of them can be
/// bypassed by a freeform spelling in the first place. The argv renderer's
/// own dedup (`push_freeform_args`) claims `ctx-size`/`parallel` for the
/// typed field whenever it is set — mandatory on a ladder, by rules 2–3 —
/// and always takes `-m` from `gguf_path`, so a freeform
/// `--ctx-size`/`-c`/`--parallel`/`-m` sitting in `args` renders as inert
/// text, never reaching llama-server. `ladder` is the patch's resolved value
/// (already through `clear`); `base_gguf_path` is the row's own `gguf_path`,
/// i.e. what rung 1 renders.
///
/// An empty `ladder` returns `Ok(())` immediately — "a row without a ladder
/// must validate exactly as before" (§8 WP1's own done-criterion). Every
/// refusal names the rung it is about, 1-based like every other rung surface
/// (§12 entry 11: the base is rung 1).
///
/// The row's effective `--spec-type`, from the typed field or a freeform
/// `--spec-type` in `args` (second-pass review finding S5). Rule 6 needs
/// this without `spec-type` ever joining `PROMOTED_ARGS`: promoting it would
/// hoist the flag into the typed field on *every* row at every load, not
/// only a ladder's, which is exactly the side effect the first version of
/// this fix (review finding 12) had and the review asked to revert.
fn effective_spec_type<'a>(
    params: &'a crate::config::LlamaParams,
    args: &'a [String],
) -> Option<&'a str> {
    params
        .spec_type
        .as_deref()
        .or_else(|| crate::modelinfo::arg_value(args, &["spec-type"]))
}

/// Whether the row names a draft model at all, from the typed field or a
/// freeform `--model-draft`/`-md` in `args` (finding S5, same reasoning as
/// [`effective_spec_type`]). Rule 6 only needs "is one set", never the path
/// itself.
fn effective_draft_path_is_set(params: &crate::config::LlamaParams, args: &[String]) -> bool {
    params.draft_gguf_path.is_some()
        || crate::modelinfo::arg_value(args, &["model-draft", "md"]).is_some()
}

pub(super) async fn validate_ladder(
    models_dir: &str,
    model_id: &str,
    base_gguf_path: &str,
    params: &crate::config::LlamaParams,
    args: &[String],
    ladder: &[crate::ladder::Rung],
) -> Result<(), String> {
    if ladder.is_empty() {
        return Ok(());
    }

    // Rule 1: max output is mandatory — nothing below can promise a rung
    // fits without a ceiling on the answer.
    let n_predict = params.n_predict.filter(|&n| n > 0).ok_or_else(|| {
        "a ladder needs Max output (n_predict) set to a positive token count — without a \
         ceiling on the answer, no rung can promise a request fits"
            .to_string()
    })?;

    // Rule 2: the KV cache has to actually split. `effective_kv_unified`
    // covers both ways this can go wrong: an explicit `kv_unified: true`, and
    // `parallel` left on auto (which llama-server unifies regardless of what
    // `kv_unified` says — fact 4).
    if params.effective_kv_unified() {
        return Err(
            "a ladder needs a split KV cache (one buffer per slot), so each rung's per-slot \
             context is a real guarantee — this row's cache is unified (kv_unified is on, or \
             parallel is left on auto, which llama-server always unifies regardless); set \
             parallel to a real slot count (1 or more) and leave kv_unified off"
                .to_string(),
        );
    }
    let slots = params.effective_slots().max(1);

    // Rule 3 (base case): rung 1 needs a real ctx_size to derive anything
    // from.
    let base_ctx = params.ctx_size.filter(|&c| c > 0).ok_or_else(|| {
        "a ladder needs the base row's ctx_size set to a positive token count — it is rung 1"
            .to_string()
    })?;

    // Rules 3 (strictly increasing, base included) and 4 (per-slot context
    // must leave room for a prompt past max output) — one pass, since both
    // read the same per-slot numbers.
    let mut per_slot = Vec::with_capacity(ladder.len() + 1);
    per_slot.push(base_ctx / slots);
    per_slot.extend(ladder.iter().map(|r| r.ctx_size / slots));
    for (i, &ps) in per_slot.iter().enumerate() {
        let rung_no = i + 1;
        if ps <= n_predict {
            return Err(format!(
                "rung {rung_no}'s per-slot context ({ps}) does not leave room for any prompt \
                 once max output ({n_predict}) is reserved — raise its ctx_size, or lower \
                 n_predict"
            ));
        }
        if i > 0 && ps <= per_slot[i - 1] {
            return Err(format!(
                "rung {rung_no}'s per-slot context ({ps}) does not exceed rung {}'s ({}) — \
                 every rung's per-slot context must be strictly larger than the last, base \
                 included",
                rung_no - 1,
                per_slot[i - 1]
            ));
        }
    }

    // Rules 5–6 read the GGUF headers, off the async runtime — same pattern
    // as `GateFacts::of_start`. Rung 1 (the base) is the reference every
    // higher rung is compared against; it is not itself re-checked here (its
    // own path/weights-ness is the create/update arms' existing job).
    let base_summary = read_gguf_summary(models_dir, base_gguf_path).await?;
    let base_tokenizer = read_tokenizer_signature(models_dir, base_gguf_path).await?;
    within_trained_context(1, base_gguf_path, per_slot[0], slots, &base_summary)?;

    for (i, rung) in ladder.iter().enumerate() {
        let rung_no = i + 2; // rung 1 is the base
        let summary = read_gguf_summary(models_dir, &rung.gguf_path)
            .await
            .map_err(|e| format!("rung {rung_no}: {e}"))?;
        within_trained_context(rung_no, &rung.gguf_path, per_slot[i + 1], slots, &summary)?;
        match crate::modelinfo::role_of(&summary) {
            "weights" => {}
            other => {
                return Err(format!(
                    "rung {rung_no}'s gguf '{}' is a {other}, not weights",
                    rung.gguf_path
                ))
            }
        }
        if summary.architecture != base_summary.architecture {
            return Err(format!(
                "rung {rung_no}'s gguf '{}' is architecture {:?}, which does not match rung 1's \
                 {:?} — every rung must tokenize identically",
                rung.gguf_path, summary.architecture, base_summary.architecture
            ));
        }
        let tokenizer = read_tokenizer_signature(models_dir, &rung.gguf_path)
            .await
            .map_err(|e| format!("rung {rung_no}: {e}"))?;
        if !tokenizer.tokenizes_identically_to(&base_tokenizer) {
            return Err(format!(
                "rung {rung_no}'s gguf '{}' does not tokenize identically to rung 1's — the fit \
                 check counts with whichever rung is running, so every rung must share the same \
                 tokenizer (model, pretokenizer, vocabulary, merges, token types, BOS/EOS)",
                rung.gguf_path
            ));
        }
        if params.chat_template_file.is_none()
            && tokenizer.chat_templates != base_tokenizer.chat_templates
        {
            // Every `tokenizer.chat_template*` key, not only the bare one
            // (review finding 11): llama.cpp picks a named variant — e.g.
            // `.tool_use` — when the request carries tools, so two rungs
            // that agree on the default template but differ on a named one
            // still render differently for a tool-calling request.
            return Err(format!(
                "rung {rung_no}'s gguf '{}' embeds a different chat template than rung 1's — \
                 llama-server renders with the GGUF's own template(s) when the row does not set \
                 chat_template_file, and the fit check counts with whichever rung is running, so \
                 every tokenizer.chat_template* key must be identical (or set chat_template_file \
                 to pin one)",
                rung.gguf_path
            ));
        }

        // Rule 5's projector companion (review finding 12): llama.cpp
        // refuses to pair a projector with a text model of another
        // embedding width (the projector's own width has to equal the
        // model's), so a rung that changes it would pass rule 5 (same
        // architecture, same tokenizer) and only fail once the ladder
        // actually climbed to it. `None` on either side is "the header
        // does not say" — nothing to compare, not refused.
        if crate::modelinfo::row_loads_projector(params, args) {
            if let (Some(base_dim), Some(rung_dim)) =
                (base_summary.embedding_length, summary.embedding_length)
            {
                if base_dim != rung_dim {
                    return Err(format!(
                        "rung {rung_no}'s gguf '{}' has embedding_length {rung_dim}, which does \
                         not match rung 1's {base_dim} — this row loads a projector, and \
                         llama.cpp refuses to pair one with a text model of another embedding \
                         width",
                        rung.gguf_path
                    ));
                }
            }
        }

        // Rule 6: `draft-mtp` needs either an external draft file (shared by
        // every rung) or MTP layers in this rung's own weights. Reads the
        // *effective* spec type / draft path — the typed field, or a
        // freeform `--spec-type`/`--model-draft`/`-md` (second-pass review
        // finding S5) — without hoisting either into the typed field, which
        // would change every row that carries them freeform, not only a
        // ladder's (see `PROMOTED_ARGS`'s doc comment).
        if effective_spec_type(params, args) == Some("draft-mtp")
            && !effective_draft_path_is_set(params, args)
            && !summary.has_mtp_layers
        {
            return Err(format!(
                "spec_type 'draft-mtp' needs MTP layers, but rung {rung_no}'s gguf '{}' has \
                 none — it would fail to load once the ladder climbed to it. Clear spec_type, \
                 or set draft_gguf_path to a real drafter GGUF",
                rung.gguf_path
            ));
        }
    }

    // Rule 7: the projector is row-level (shared by every rung), so this
    // checks once, against the base — the same call the gate's fit check
    // makes (`gate::fit_chat`) and `model_warnings` makes for a pool-guarded
    // (non-ladder) row.
    let row = crate::gate::count::ProjectorRow {
        model_id,
        gguf_path: base_gguf_path,
        params,
        args,
    };
    if let Err(reason) = crate::gate::count::image_token_bound(models_dir, row).await {
        return Err(reason);
    }

    Ok(())
}

/// Rule 3b (review finding 1): a rung's per-slot context must not be above
/// its GGUF's trained context. llama-server caps every slot there
/// (`server-context.cpp`, "capping"), so such a rung would promise one number
/// and hold another: a request judged to fit comes back truncated, and a
/// climb between two rungs both past it buys nothing but a reload. A header
/// that does not say is not refused — there is nothing to compare.
fn within_trained_context(
    rung_no: usize,
    gguf_path: &str,
    per_slot: i64,
    slots: i64,
    summary: &crate::gguf::ModelSummary,
) -> Result<(), String> {
    let Some(trained) = summary.context_length.and_then(|t| i64::try_from(t).ok()) else {
        return Ok(());
    };
    if trained > 0 && per_slot > trained {
        return Err(format!(
            "rung {rung_no}'s per-slot context ({per_slot}) is above its gguf '{gguf_path}''s \
             trained context ({trained}) — llama-server caps every slot at the trained context, \
             so the rung would hold {trained} tokens per slot, not {per_slot}. Lower its ctx_size \
             to at most {} (the trained context × {slots} slot(s))",
            trained.saturating_mul(slots)
        ));
    }
    Ok(())
}

/// Read a GGUF's header off the async runtime, folding a spawn failure and a
/// read failure into one message — the shared half of §4.3 rules 5–6's two
/// header reads ([`validate_ladder`], and later `read_tokenizer_signature`).
async fn read_gguf_summary(
    models_dir: &str,
    gguf_path: &str,
) -> Result<crate::gguf::ModelSummary, String> {
    let path = std::path::Path::new(models_dir).join(gguf_path);
    tokio::task::spawn_blocking(move || crate::gguf::summarize(&path))
        .await
        .map_err(|e| format!("reading '{gguf_path}' panicked: {e}"))?
        .map_err(|e| format!("gguf '{gguf_path}' could not be read: {e}"))
}

/// [`read_gguf_summary`], for the tokenizer-identity signature
/// ([`crate::gguf::read_tokenizer_signature`]) instead of the general
/// summary.
async fn read_tokenizer_signature(
    models_dir: &str,
    gguf_path: &str,
) -> Result<crate::gguf::TokenizerSignature, String> {
    let path = std::path::Path::new(models_dir).join(gguf_path);
    tokio::task::spawn_blocking(move || crate::gguf::read_tokenizer_signature(&path))
        .await
        .map_err(|e| format!("reading '{gguf_path}' panicked: {e}"))?
        .map_err(|e| format!("gguf '{gguf_path}' could not be read: {e}"))
}

/// Advisory-only half of §4.3: a rung whose footprint (weights + KV) exceeds
/// the card's capacity minus headroom. Never refuses — VRAM is measured at
/// climb time, not at save time (§4.2) — so a failure to even measure
/// (`!view.active`, no GPU telemetry) is silently "nothing to warn about",
/// the same as every other capacity-dependent check in this codebase.
///
/// Uses a throwaway [`crate::vram::plan::PlanCache`] rather than the
/// scheduler's own memoized one: this runs once per save, never on a hot
/// path, and the scheduler does not expose its cache to `ops`.
pub(super) async fn ladder_footprint_advisories(
    state: &SharedState,
    models_dir: &str,
    m: &NewLocalModel,
) -> Vec<String> {
    if m.ladder.is_empty() {
        return Vec::new();
    }
    let view = state.vram.view(state).await;
    if !view.active {
        return Vec::new();
    }
    let budget = view.capacity_bytes.saturating_sub(view.headroom_bytes);
    let plans = crate::vram::plan::PlanCache::default();
    let mut out = Vec::new();
    for (i, r) in m.ladder.iter().enumerate() {
        let rung_no = i + 2; // rung 1 is the base, checked by the row's own (non-ladder) advisory
        let probe = crate::config::LocalModel {
            id: 0,
            model_id: m.model_id.clone(),
            gguf_path: r.gguf_path.clone(),
            params: crate::config::LlamaParams {
                ctx_size: Some(r.ctx_size),
                ..m.params.clone()
            },
            args: m.args.clone(),
            idle_seconds: m.idle_seconds,
            enabled: m.enabled,
            public: m.public,
            image: m.image.clone(),
            extra_run_args: m.extra_run_args.clone(),
            warm_start: m.warm_start,
            hold_fallback_mode: m.hold_fallback_mode,
            hold_fallback: m.hold_fallback.clone(),
            capabilities_override: m.capabilities_override.clone(),
            ladder: Vec::new(),
        };
        let fp = plans.local(models_dir, &probe).await;
        if fp.total_bytes > budget {
            out.push(format!(
                "rung {rung_no}'s estimated footprint ({:.1} GiB: {}) is over the card's \
                 capacity minus headroom ({:.1} GiB) — it may not fit when the ladder climbs to \
                 it; VRAM is measured for real at climb time, this is only an estimate",
                fp.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                r.gguf_path,
                budget as f64 / (1024.0 * 1024.0 * 1024.0),
            ));
        }
    }
    out
}

/// Bundled inputs for [`ladder_rung_plan`] — every rung shares these with the
/// base by construction (design §4.1), so grouping them into one struct is
/// what keeps that function's own argument count sane rather than growing a
/// seventh, eighth, ninth positional parameter every time another shared
/// field turns out to matter for a footprint.
pub struct RungPlanInput<'a> {
    pub gguf_path: &'a str,
    pub ctx_size: i64,
    pub cache_type_k: Option<&'a str>,
    pub cache_type_v: Option<&'a str>,
    pub mmproj_path: Option<&'a str>,
    pub draft_gguf_path: Option<&'a str>,
    pub n_gpu_layers: Option<i64>,
}

/// `GET /api/ladder-rung-plan` (ladder design §4.2, §6): one rung's footprint,
/// MTP flag and trained context, for the editor's ladder table (WP5).
/// Per-slot context and switchover are plain arithmetic over fields the
/// editor already holds (mirrors of [`crate::ladder::LocalModel::per_slot_ctx`]
/// /`switchover`), so only the facts that need a file read live here — the
/// trained context among them, since llama-server caps every slot at it
/// ([`crate::ladder::slot_ctx`]) and the table has to show the slot the rung
/// really gets (review finding 1).
///
/// The caller passes the row's current *unsaved* form values, not a saved row
/// id — every rung shares cache types, projector and drafter with the base by
/// construction (§4.1), and the table has to update live as the owner edits,
/// before Save exists to read back from. Read-only: this never starts
/// anything and never runs §4.3's validation, so an editor mid-edit (a GGUF
/// picked but not yet a valid ctx_size, say) just gets an error to show as
/// "—", not a refusal.
pub async fn ladder_rung_plan(
    state: &SharedState,
    input: RungPlanInput<'_>,
) -> Result<Value, String> {
    if input.ctx_size <= 0 {
        return Err("ctx_size must be a positive token count".into());
    }
    let models_dir = state.snapshot().settings.router.models_dir.clone();
    let (rel, full) = crate::modelinfo::resolve(&models_dir, input.gguf_path, Class::Chat)?;
    // Through the shared cache (model capabilities design §3.5), the same
    // path `lmgw__model_inspect` reads: a table re-fetching the same GGUF as
    // the owner edits ctx_size beside it should not re-walk the header each
    // time.
    let summary = state.gguf_cache.summarize_cached(&full).await?;
    let has_mtp_layers = summary.has_mtp_layers;
    let trained_context = summary.context_length;
    let clean = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    // The companions get the same treatment as `gguf_path` (second-pass
    // review finding S10): `resolve` is what refuses `..`/an absolute path
    // outside the models dir and confirms the file is actually there, and a
    // footprint estimate built from an unresolved path is a footprint of
    // whatever `Path::join` happened to land on. Left unset, a companion is
    // just absent from the probe, same as before — a projector or a drafter
    // is optional, and this stays "no refusal on an incomplete edit" for the
    // fields the owner has not typed yet.
    let mmproj_path = match clean(input.mmproj_path) {
        Some(p) => Some(crate::modelinfo::resolve(&models_dir, &p, Class::Chat)?.0),
        None => None,
    };
    let draft_gguf_path = match clean(input.draft_gguf_path) {
        Some(p) => Some(crate::modelinfo::resolve(&models_dir, &p, Class::Chat)?.0),
        None => None,
    };
    let probe = crate::config::LocalModel {
        id: 0,
        model_id: String::new(),
        gguf_path: rel,
        params: crate::config::LlamaParams {
            ctx_size: Some(input.ctx_size),
            cache_type_k: clean(input.cache_type_k),
            cache_type_v: clean(input.cache_type_v),
            mmproj_path,
            draft_gguf_path,
            n_gpu_layers: input.n_gpu_layers,
            ..Default::default()
        },
        args: Vec::new(),
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: Vec::new(),
    };
    // A throwaway cache, like the save-time advisory above: this is not a hot
    // path, and `ops` has no accessor into the VRAM scheduler's own memoized
    // one.
    let footprint = crate::vram::plan::PlanCache::default()
        .local(&models_dir, &probe)
        .await;
    Ok(json!({
        "footprint": footprint,
        "has_mtp_layers": has_mtp_layers,
        "trained_context": trained_context,
    }))
}
