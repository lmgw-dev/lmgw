//! `LlamaParams` (llama-server's common flags, one `Some` per CLI flag) and
//! hoisting legacy freeform `args` entries into their typed slot.

use serde::{Deserialize, Serialize};

/// Flags that used to live in [`LocalModel::args`](crate::config::LocalModel::args)
/// because they had no dedicated field, paired with the [`LlamaParams`] slot
/// each now owns.
///
/// Rows written before those fields existed still carry them as freeform
/// args. Rather than a data migration — `params` is a JSON blob, so SQL can't
/// reach into it — [`LocalModel::hoist_promoted_args`](crate::config::LocalModel::hoist_promoted_args) moves them on read.
/// That runs on every load, is idempotent, and keeps the dashboard's edit form
/// (which replaces the whole row) from silently dropping a projector it has no
/// input for.
const PROMOTED_ARGS: &[&str] = &[
    "mmproj",
    "no-mmproj",
    "ubatch-size",
    "cache-ram",
    "n-predict",
    "reasoning-format",
    "reasoning",
    "reasoning-budget",
    "reasoning-preserve",
    "no-reasoning-preserve",
    "reasoning-effort",
    "chat-template-file",
    "chat-template-kwargs",
    "fit",
    "fit-ctx",
    "kv-unified",
    "no-kv-unified",
    "kv-unified-per-slot",
    // `spec-type`/`model-draft` are deliberately *not* here (second-pass
    // review finding S5, reverting review finding 12's fix): promoting them
    // would hoist a freeform speculative-decode flag on every row at every
    // load, not only a ladder's — changing argv order (so a first boot after
    // upgrade fails the `Config.Cmd` match and replaces a running container
    // instead of adopting it), pulling the drafter into the VRAM plan, and
    // rewriting a relative `--model-draft` onto `/models/…`, all on rows
    // this phase promises to leave unchanged. `ops::validate_ladder` reads
    // the effective spec type / draft path itself instead — see
    // `effective_spec_type`/`effective_draft_path` there.
];

/// [`PROMOTED_ARGS`] that take no value of their own.
///
/// The generic rule below — "the next token that is not itself an option is
/// this flag's value" — is right for `--mmproj FILE` and wrong for a bare
/// switch: `--no-mmproj foo.gguf` would swallow `foo.gguf` and drop it on the
/// floor. Naming the switches here keeps the following token where it is.
const VALUELESS_PROMOTED_ARGS: &[&str] = &[
    "no-mmproj",
    "reasoning-preserve",
    "no-reasoning-preserve",
    "kv-unified",
    "no-kv-unified",
];

/// The guts of [`LocalModel::hoist_promoted_args`](crate::config::LocalModel::hoist_promoted_args), on a bare `(params,
/// args)` pair rather than a full [`LocalModel`](crate::config::LocalModel) — so `ops::local_model_set`
/// can run the exact same fold on a save's own params and freeform args
/// *before* `ops::validate_kv_unified` judges them (review finding 3).
/// Without this, `--kv-unified`/`-kvu` typed into `extra_args` at save time
/// would reach the pool ledger unvalidated until the row's next load, which
/// is also where this fold already ran (`store::list_local_models` →
/// [`LocalModel::hoist_promoted_args`](crate::config::LocalModel::hoist_promoted_args)).
///
/// The short spellings (`-kvu`, `-no-kvu`) this phase's own promoted flags
/// use, canonicalized before the [`PROMOTED_ARGS`] check so they hoist
/// exactly like their long twins instead of silently staying freeform text
/// nothing here recognizes.
///
/// Deliberately narrower than [`crate::runtime::argv::canonical_key`]'s full
/// `SHORT_ALIASES` table: that table also maps short flags with no promoted
/// twin at all (`-ub`, `-n`/`-predict`, `-cram`, `-mm`, `-rea`, …), and
/// running the hoist through it used to hoist those too on every row's next
/// load — changing argv order (so a first boot after upgrade fails the
/// `Config.Cmd` match and replaces a running container instead of adopting
/// it), pulling a projector into the VRAM plan, and rewriting published
/// capabilities, all on rows that predate this phase and never asked for any
/// of it (review finding X2, the same class of regression ladder §12 entry
/// 75 reverted for `spec-type`/`model-draft`). Every other promoted flag
/// keeps matching only its literal long spelling, exactly as it did before
/// this phase.
fn hoist_canonical_key(key: &str) -> &str {
    match key {
        "kvu" => "kv-unified",
        "no-kvu" => "no-kv-unified",
        other => other,
    }
}

pub fn hoist_promoted_args_into(params: &mut LlamaParams, args: &mut Vec<String>) {
    let mut kept: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].clone();
        // `--flag=value` is one token; split it so that spelling hoists
        // like the spaced one instead of being left behind.
        let (tok, inline) = match raw.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f.to_string(), Some(v.to_string())),
            _ => (raw.clone(), None),
        };
        let flag = hoist_canonical_key(tok.trim_start_matches('-'));
        if !tok.starts_with('-') || !PROMOTED_ARGS.contains(&flag) {
            kept.push(raw);
            i += 1;
            continue;
        }

        // A value belongs to this flag only if it is not itself a flag.
        // `argv::is_opt` is the shared predicate and using it matters: a
        // bare `starts_with('-')` classifies `-1` as a flag, which
        // silently discarded the documented value of `--reasoning-budget`.
        let (value, consumed) = match inline {
            Some(v) => (Some(v), 1),
            None if VALUELESS_PROMOTED_ARGS.contains(&flag) => (None, 1),
            None => match args.get(i + 1) {
                Some(v) if !crate::runtime::argv::is_opt(v) => (Some(v.clone()), 2),
                _ => (None, 1),
            },
        };
        i += consumed;

        // Not every spelling survives the trip into a typed field, and a
        // value that cannot be represented is left in `args` rather than
        // mangled: a path outside the mount point (see below) has no
        // relative form, and a `--chat-template-kwargs` value that is not
        // a JSON object has no map form.
        let mut hoistable = true;
        let rel = |v: &Option<String>| -> Option<String> {
            v.as_deref().map(|v| {
                v.trim_start_matches("/models/")
                    .trim_start_matches('/')
                    .to_string()
            })
        };
        let outside = |v: &Option<String>| {
            v.as_deref()
                .is_some_and(|v| v.starts_with('/') && !v.starts_with("/models/"))
        };

        match flag {
            "mmproj" if outside(&value) => hoistable = false,
            "chat-template-file" if outside(&value) => hoistable = false,
            "mmproj" => set_str(&mut params.mmproj_path, rel(&value)),
            "chat-template-file" => set_str(&mut params.chat_template_file, rel(&value)),
            "no-mmproj" => params.no_mmproj = true,
            "ubatch-size" => set_int(&mut params.ubatch_size, &value),
            "cache-ram" => set_int(&mut params.cache_ram, &value),
            "n-predict" => set_int(&mut params.n_predict, &value),
            "reasoning-budget" => set_int(&mut params.reasoning_budget, &value),
            "fit-ctx" => set_int(&mut params.fit_ctx, &value),
            "reasoning-format" => set_str(&mut params.reasoning_format, value.clone()),
            "reasoning" => set_str(&mut params.reasoning, value.clone()),
            "fit" => set_str(&mut params.fit, value.clone()),
            "reasoning-effort" => set_str(&mut params.reasoning_effort, value.clone()),
            // A bare switch means what its own name says; the `=false`
            // spelling flips it, and `--no-…=false` is a double negative.
            "reasoning-preserve" => set_bool(&mut params.reasoning_preserve, &value, true),
            "no-reasoning-preserve" => set_bool(&mut params.reasoning_preserve, &value, false),
            "kv-unified" => set_bool(&mut params.kv_unified, &value, true),
            "no-kv-unified" => set_bool(&mut params.kv_unified, &value, false),
            "kv-unified-per-slot" => set_int(&mut params.kv_unified_per_slot, &value),
            "chat-template-kwargs" => {
                match parse_chat_template_kwargs(value.as_deref().unwrap_or_default()) {
                    Ok(obj) if !obj.is_empty() => {
                        if params.chat_template_kwargs.is_empty() {
                            params.chat_template_kwargs = obj;
                        }
                    }
                    // Nothing to carry over (no value, or a literal `{}`),
                    // or not a JSON object at all: hoisting would throw
                    // away what the caller wrote. Leave it in `args`,
                    // where llama-server can reject it out loud.
                    _ => hoistable = false,
                }
            }
            _ => unreachable!("PROMOTED_ARGS and the match above must stay in step"),
        }

        if !hoistable {
            // Put the tokens back exactly as they were.
            for k in 0..consumed {
                kept.push(args[i - consumed + k].clone());
            }
        }
    }
    *args = kept;
    params.fold_reasoning_effort();
}

impl LlamaParams {
    /// Move a `reasoning_effort` written into the freeform
    /// [`Self::chat_template_kwargs`] over to [`Self::reasoning_effort`].
    ///
    /// `--reasoning-effort` and a `reasoning_effort` kwarg are two routes to
    /// one template variable, and a preset that took both would leave the
    /// winner to llama-server. Folding them makes the dashboard's effort
    /// control the single answer; a non-string value (no effort level is a
    /// number) is left in the map untouched unless the field already holds
    /// one.
    pub fn fold_reasoning_effort(&mut self) {
        match self.chat_template_kwargs.get("reasoning_effort").cloned() {
            None => {}
            Some(_) if self.reasoning_effort.is_some() => {
                self.chat_template_kwargs.remove("reasoning_effort");
            }
            Some(serde_json::Value::String(v)) => {
                self.reasoning_effort = Some(v);
                self.chat_template_kwargs.remove("reasoning_effort");
            }
            Some(_) => {}
        }
    }
}

/// Parse a `--chat-template-kwargs` value: the JSON **object** llama-server
/// documents (`'{"reasoning_effort":"medium"}'`). An empty string is no
/// kwargs at all; anything else that is not an object is rejected here rather
/// than at model load, where it surfaces as an opaque llama-server exit.
pub fn parse_chat_template_kwargs(
    s: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(o)) => Ok(o),
        Ok(_) => Err(
            "chat template kwargs: must be a JSON object, e.g. {\"reasoning_effort\":\"medium\"}"
                .into(),
        ),
        Err(e) => Err(format!("chat template kwargs: invalid JSON ({e})")),
    }
}

/// Fill an unset tri-state switch from a hoisted arg. `on` is what the flag's
/// own name asserts (`--reasoning-preserve` = true, `--no-…` = false); an
/// explicit `=false` inverts it, so `--no-reasoning-preserve=false` is true.
fn set_bool(slot: &mut Option<bool>, value: &Option<String>, on: bool) {
    if slot.is_some() {
        return;
    }
    let asserted = match value.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "false" | "off" | "0" | "no"
        ),
    };
    *slot = Some(on == asserted);
}

/// Fill an unset numeric param from a hoisted arg, ignoring an unparsable one
/// (it was already being passed verbatim to llama-server, which would have
/// rejected it; dropping it here does not make anything newly broken).
fn set_int(slot: &mut Option<i64>, value: &Option<String>) {
    if slot.is_none() {
        *slot = value.as_deref().and_then(|v| v.parse().ok());
    }
}

fn set_str(slot: &mut Option<String>, value: Option<String>) {
    if slot.is_none() {
        *slot = value.filter(|v| !v.is_empty());
    }
}

/// Common llama-server params (§8). Each `Some` value becomes one CLI flag;
/// `None` is omitted so llama-server's own default applies. Stored as one
/// JSON object in `local_models.params` (same pattern as `settings`), so
/// adding a field here needs no migration — `#[serde(default)]` fills it in
/// for rows written before it existed.
///
/// The bar for a dedicated field is "a model cannot be served correctly
/// without it". Multimodal projectors and speculative drafters both clear it:
/// leaving them to the freeform `args` escape hatch meant the caller had to
/// know that `--mmproj` takes an absolute in-container path while its sibling
/// `model-draft` takes a relative one — an asymmetry invisible from outside
/// the source. Everything here is rendered by
/// [`crate::runtime::argv::render_llama_args`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaParams {
    pub ctx_size: Option<i64>,
    /// `--n-predict`: the most tokens one response may generate. `None` =
    /// llama-server's own default (-1, unbounded by anything but the
    /// context). This is the number `/v1/models` reports as
    /// `max_output_tokens`; leave it unset rather than inventing one.
    pub n_predict: Option<i64>,
    pub n_gpu_layers: Option<i64>,
    pub threads: Option<i64>,
    pub batch_size: Option<i64>,
    /// `-ub`: physical batch size.
    pub ubatch_size: Option<i64>,
    /// `-np`: parallel request slots.
    pub parallel: Option<i64>,
    /// `-kvu/--kv-unified` (on) or `-no-kvu/--no-kv-unified` (off): one shared
    /// KV buffer across every slot instead of one buffer per slot (candidate-
    /// aliases/unified-KV design §2.1 fact 1, §3.1). `None` leaves
    /// llama-server's own default alone, which is unified exactly when
    /// [`Self::parallel`] is unset — see [`Self::effective_kv_unified`].
    /// Turning this explicitly on with more than one effective slot makes the
    /// row **guarded** once [`Self::n_predict`] is also set (fact 3: a full
    /// shared pool aborts every running request, not just the one that
    /// overflowed it) — see [`Self::pool_guarded`].
    pub kv_unified: Option<bool>,
    /// `--kv-unified-per-slot`: caps the per-request context of a unified row
    /// (design §2.1 fact 2), and — only when [`Self::ctx_size`] is unset —
    /// sizes the shared pool itself to `effective_slots() × this`. Meaningless
    /// on a split pool (each slot already has its own `ctx_size / parallel`),
    /// so [`crate::ops`]'s save-time check refuses it there. `None` leaves
    /// llama-server's own default (unset, no cap beyond the trained context).
    pub kv_unified_per_slot: Option<i64>,
    /// `auto` / `on` / `off`.
    pub flash_attn: Option<String>,
    /// KV cache quantization (`f16`, `q8_0`, `q4_0`, …).
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    /// `--cache-ram` (`-cram`): host RAM in MiB llama-server may spend
    /// keeping the prompt state of idle slots around, so returning to an
    /// earlier conversation skips reprocessing it. `-1` = no limit, `0` =
    /// disabled. `None` leaves llama.cpp's own default (8192 MiB) in place —
    /// this is an override, not a cap lmgw invents.
    pub cache_ram: Option<i64>,
    /// `--jinja`: use the model's embedded chat template.
    pub jinja: bool,
    /// `--chat-template-file`: override the embedded template. Relative to the
    /// models dir, rendered as `/models/…` like the GGUF paths.
    pub chat_template_file: Option<String>,
    /// `--reasoning-format`: `auto` | `none` | `deepseek` | `deepseek-legacy`.
    pub reasoning_format: Option<String>,
    /// `--reasoning`: `on` | `off` | `auto`.
    pub reasoning: Option<String>,
    /// `--reasoning-budget`: thinking token budget (-1 unrestricted, 0 off).
    pub reasoning_budget: Option<i64>,
    /// `--reasoning-preserve` / `--no-reasoning-preserve`: keep the thinking
    /// trace of *every* assistant turn in the history instead of only the
    /// last one. Tri-state — `None` leaves the template's own default alone,
    /// which is the only right answer for a template that does not advertise
    /// `supports_preserve_reasoning`.
    pub reasoning_preserve: Option<bool>,
    /// `--reasoning-effort`: how hard a template that reads a
    /// `reasoning_effort` variable (Qwen3.8, GPT-OSS, …) thinks when the
    /// request does not say. A request-level `reasoning_effort` — or a
    /// `chat_template_kwargs.reasoning_effort` — still overrides it, so this
    /// is a default and not a ceiling.
    ///
    /// Not an enum: llama-server hands the value to the template verbatim
    /// without checking it (`none` and `banana` both arrive intact), so what
    /// is meaningful is the template's business. Unset omits the flag, which
    /// is exactly what llama.cpp's own `default` sentinel does.
    pub reasoning_effort: Option<String>,
    /// `--chat-template-kwargs`: further variables handed to the jinja chat
    /// template, as one JSON object. Freeform because the keys a template
    /// understands are a property of that template, not of llama.cpp. Also
    /// the fallback route to an effort level on a build too old for
    /// `--reasoning-effort`: both end up as the same template variable.
    pub chat_template_kwargs: serde_json::Map<String, serde_json::Value>,
    pub temp: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub seed: Option<i64>,
    /// `--mmproj`: multimodal (vision/audio) projector. Relative to the models
    /// dir exactly like `gguf_path`; the renderer adds the `/models/` prefix,
    /// so callers never deal with in-container paths.
    pub mmproj_path: Option<String>,
    /// `--no-mmproj`: refuse to auto-load a projector sitting next to the
    /// weights. The text-only twin of a multimodal repo needs this.
    pub no_mmproj: bool,
    /// External draft model for speculative decoding (`--model-draft`),
    /// e.g. an MTP-heads GGUF. Relative to the models dir like `gguf_path`;
    /// rendered as `model-draft = /models/…`.
    pub draft_gguf_path: Option<String>,
    /// `--spec-type`: comma list of speculative decoding types
    /// (`draft-mtp`, `draft-dflash`, `draft-simple`, `ngram-mod`, …).
    pub spec_type: Option<String>,
    /// `--spec-draft-n-max` / `--spec-draft-n-min`: draft token window.
    pub spec_draft_n_max: Option<i64>,
    pub spec_draft_n_min: Option<i64>,
    /// `--spec-draft-ngl`: draft model GPU layers (number, `auto`, `all`).
    pub spec_draft_ngl: Option<String>,
    /// `--fit`: let llama-server shrink *unset* arguments to fit device
    /// memory. Explicitly-set values are never touched by it, so this cannot
    /// silently cap a context length configured here.
    pub fit: Option<String>,
    /// `--fit-ctx`: floor on the context `--fit` may choose.
    pub fit_ctx: Option<i64>,
}

impl LlamaParams {
    /// Whether [`Self::parallel`] is llama-server's own auto case: unset, or
    /// non-positive (`0` reads as "from model" the same way `ctx_size` does —
    /// never a real slot count). Shared by every helper below so "auto" means
    /// one thing everywhere (review finding 1/3).
    fn parallel_is_auto(&self) -> bool {
        !matches!(self.parallel, Some(n) if n >= 1)
    }

    /// Whether this row's KV cache is a single pool shared by every slot
    /// (unified-KV design §3.1, review finding 3, binding). llama-server
    /// forces unified — and 4 slots — whenever `parallel` is left on auto,
    /// **overriding an explicit `--no-kv-unified`**: "n_parallel is set to
    /// auto, using n_parallel = 4 and kv_unified = true"
    /// (`tools/server/server.cpp:156-161`). So auto wins outright; `kv_unified`
    /// only has a say once `parallel` names a real slot count.
    pub fn effective_kv_unified(&self) -> bool {
        self.parallel_is_auto() || self.kv_unified == Some(true)
    }

    /// How many parallel request slots this row actually runs: `parallel`
    /// when it names at least one, otherwise llama-server's own auto count.
    /// Auto is 4, not "however many fit" — `tools/server/server.cpp:156-161`
    /// hardcodes it: "n_parallel is set to auto, using n_parallel = 4".
    pub fn effective_slots(&self) -> i64 {
        match self.parallel {
            Some(n) if n >= 1 => n,
            _ => 4,
        }
    }

    /// The total shared KV cells a unified row's pool holds, derived the same
    /// way llama-server sizes it (design §2.1 fact 2, §3.2, review finding 2):
    /// `ctx_size` when it is a real value (`ctx_size ≤ 0` reads as "from
    /// model" — llama-server's own sentinel for unset — never a pool of zero
    /// or negative size); otherwise `kv_unified_per_slot × effective_slots()`
    /// when the per-slot cap is set ("`--kv-unified-per-slot N` without `-c`
    /// sizes the pool to `np × N`" — `tools/server/server.cpp:164-172`).
    ///
    /// **No trained-context fallback.** `--fit` (default on) shrinks an
    /// *unset* `ctx_size` to whatever device memory allows, so the trained
    /// context is never a safe stand-in for the pool's real size — publishing
    /// it here would claim a number llama-server never actually promised.
    /// `None` when neither term is known — this feeds both
    /// [`Self::per_request_ctx`] and the pool ledger's capacity
    /// ([`crate::gate::pool`]), which is why it is its own method rather than
    /// inlined into the former.
    pub fn pool_tokens(&self) -> Option<i64> {
        self.ctx_size.filter(|&c| c > 0).or_else(|| {
            self.kv_unified_per_slot
                .map(|cap| cap * self.effective_slots())
        })
    }

    /// The per-request context a client can actually use — what `/v1/models`
    /// publishes and quickdoc sizes its windows against (design §3.2, review
    /// finding 2).
    ///
    /// **Unified:** the minimum of the pool ([`Self::pool_tokens`]), the
    /// per-slot cap, and the trained context — each only when known
    /// (`n_ctx_slot()` "is capped by `--kv-unified-per-slot` and by the
    /// trained context", `server-context.cpp:4027`), but **`None` outright
    /// when the pool itself is unknown**: an auto row with `ctx_size` unset
    /// (no per-slot cap either) publishes nothing, exactly as it did before
    /// this toggle existed, rather than quietly borrowing the trained context
    /// as a pool size llama-server never actually sized to. An auto row that
    /// *does* set `ctx_size` publishes exactly what it does today unless the
    /// trained context is smaller.
    ///
    /// **Split:** `ctx_size / parallel`, unchanged from before this toggle
    /// existed — `trained` is deliberately not consulted here, matching
    /// today's behaviour (and today's `None` when `ctx_size` is unset or
    /// `≤ 0`) so that turning this toggle off can never move a split row's
    /// published context.
    pub fn per_request_ctx(&self, trained: Option<i64>) -> Option<i64> {
        if self.effective_kv_unified() {
            let pool = self.pool_tokens()?;
            [Some(pool), self.kv_unified_per_slot, trained]
                .into_iter()
                .flatten()
                .min()
        } else {
            let total = self.ctx_size.filter(|&c| c > 0)?;
            Some(total / self.parallel.unwrap_or(1).max(1))
        }
    }

    /// Whether the pool ledger ([`crate::gate::pool`]) must guard this row —
    /// **decision D1**: only when unified is explicitly on, more than one
    /// slot is effectively in play, and a max-output ceiling exists to bound
    /// a reservation by. A row that only reaches a shared pool through
    /// llama-server's auto default (`kv_unified: None`, `parallel: None`) is
    /// left alone (spec §1) — whatever its `n_predict`, it never counts as
    /// guarded, because there was never an explicit choice to guard.
    pub fn pool_guarded(&self) -> bool {
        self.kv_unified == Some(true)
            && self.effective_slots() > 1
            && self.n_predict.is_some_and(|n| n > 0)
    }

    /// Whether this row's slots share one pool **without** the ledger's
    /// guard — the condition the editor's "slots share one pool unguarded"
    /// note (§3.3) fires on. True exactly when the row is effectively
    /// unified with more than one slot but [`Self::pool_guarded`] does not
    /// hold (missing/zero `n_predict`, most commonly).
    pub fn pool_unguarded_shared(&self) -> bool {
        self.effective_kv_unified() && self.effective_slots() > 1 && !self.pool_guarded()
    }
}
