//! The local model editor's form: one signal per value, what was loaded (the
//! baseline a change is measured against), the fields that do not parse,
//! and the save's sparse patch.
//!
//! Save semantics mirror the ops plane exactly: filled fields are sent,
//! emptied optional fields go into `clear`, apply stays a separate step.

use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::{LlamaParams, LocalModelDetail, Rung};
use serde_json::{Map, Value};

use super::ladder::{ladder_is_dirty, new_rung_row, LadderShared, RungRow};
use super::specs::{group_of, param_string, Kind, SECTIONS};

/// A form value, text or a switch, compared and restored as text — what
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

/// The values a save posted.
pub(super) struct Snapshot {
    values: Vec<String>,
    ladder_on: bool,
    ladder: Vec<(String, String)>,
}

/// One value of the form: its key, the group it sits in, the signal.
#[derive(Clone, Copy)]
struct Entry {
    key: &'static str,
    group: &'static str,
    val: Val,
}

/// Everything the form edits.
#[derive(Clone, Copy)]
pub(super) struct LocalForm {
    fields: StoredValue<HashMap<&'static str, RwSignal<String>>>,
    pub model_id: RwSignal<String>,
    pub gguf_path: RwSignal<String>,
    pub jinja: RwSignal<bool>,
    pub no_mmproj: RwSignal<bool>,
    pub public: RwSignal<bool>,
    pub enabled: RwSignal<bool>,
    pub idle_seconds: RwSignal<String>,
    pub kwargs: RwSignal<String>,
    /// Owner override of the derived /v1/models capability facts
    /// (model-capabilities design §7) — same "JSON object as text, empty
    /// clears" convention as `kwargs`.
    pub capabilities_override: RwSignal<String>,
    pub extra_args: RwSignal<String>,
    /// Per-model container overrides (per-model-containers §3.1) — empty
    /// image/extra_run_args means "inherit the chat class settings", the
    /// same "empty clears" convention every other optional field uses.
    pub image: RwSignal<String>,
    pub extra_run_args: RwSignal<String>,
    pub warm_start: RwSignal<bool>,
    /// The row's one fallback (openai egress design §14.2): "" is none;
    /// `collect` sends it, or names it in `clear`.
    pub fallback: RwSignal<String>,
    /// The row's GPU pin (device-ledger design §8): card ids,
    /// comma-separated (`gpu_pin::pin_ids`); empty, lmgw picks the card.
    pub gpu_pin: RwSignal<String>,
    /// Ladder (ladder design §4.1, §6): off by default, on the moment a
    /// saved row has any rungs above the base. `rungs` holds only the
    /// *higher* rungs — the base stays `gguf_path` and `ctx_size`, so nothing
    /// that reads them for a plain row has to special-case a ladder one. A
    /// rung list is a dynamic Vec, not one signal, so it has its own dirty,
    /// baseline and discard handling beside the generic one.
    pub ladder_on: RwSignal<bool>,
    pub rungs: RwSignal<Vec<RungRow>>,
    next_rung_key: StoredValue<u64>,
    ladder_on_baseline: RwSignal<bool>,
    ladder_baseline: RwSignal<Vec<(String, String)>>,
    entries: StoredValue<Vec<Entry>>,
    /// What was loaded (or, for a new model, the blank form): a value that
    /// differs from it is an unsaved change.
    baseline: RwSignal<Option<Vec<String>>>,
    /// (group, key) of every unsaved change.
    pub dirty: Memo<Vec<(&'static str, &'static str)>>,
    /// Fields that would not parse, said at the field and counted in the
    /// toast (they block Save): the server would refuse them anyway, but later.
    errors: Memo<Vec<(&'static str, String)>>,
    /// Per-rung problems, keyed by the rung's own `key`: an empty GGUF path
    /// or a context size that does not parse.
    rung_errors: Memo<Vec<(u64, String)>>,
    /// The table itself: on when Ladder is on but has nothing to climb to.
    pub ladder_table_error: Signal<Option<String>>,
    /// What every rung shares with the base (design §4.1), gathered once so
    /// the table's derived columns and its footprint fetch read one snapshot.
    pub ladder_shared: Memo<LadderShared>,
}

impl LocalForm {
    pub fn new() -> Self {
        let fields = StoredValue::new(
            SECTIONS
                .iter()
                .flat_map(|(_, specs)| specs.iter())
                .map(|s| (s.key, RwSignal::new(String::new())))
                .collect::<HashMap<_, _>>(),
        );
        let model_id = RwSignal::new(String::new());
        let gguf_path = RwSignal::new(String::new());
        let jinja = RwSignal::new(false);
        let no_mmproj = RwSignal::new(false);
        let public = RwSignal::new(true);
        let enabled = RwSignal::new(true);
        let idle_seconds = RwSignal::new(String::from("0"));
        let kwargs = RwSignal::new(String::new());
        let capabilities_override = RwSignal::new(String::new());
        let extra_args = RwSignal::new(String::new());
        let image = RwSignal::new(String::new());
        let extra_run_args = RwSignal::new(String::new());
        let warm_start = RwSignal::new(false);
        let fallback = RwSignal::new(String::new());
        let gpu_pin = RwSignal::new(String::new());
        let ladder_on = RwSignal::new(false);
        let rungs = RwSignal::new(Vec::<RungRow>::new());
        let ladder_on_baseline = RwSignal::new(false);
        let ladder_baseline = RwSignal::new(Vec::<(String, String)>::new());

        let entries = StoredValue::new({
            let mut v = vec![
                ("model_id", Val::Text(model_id)),
                ("gguf_path", Val::Text(gguf_path)),
                ("idle_seconds", Val::Text(idle_seconds)),
                ("public", Val::Flag(public)),
                ("enabled", Val::Flag(enabled)),
                ("jinja", Val::Flag(jinja)),
                ("no_mmproj", Val::Flag(no_mmproj)),
                ("warm_start", Val::Flag(warm_start)),
                ("fallback", Val::Text(fallback)),
                ("image", Val::Text(image)),
                ("extra_run_args", Val::Text(extra_run_args)),
                ("gpu_pin", Val::Text(gpu_pin)),
            ];
            fields.with_value(|f| {
                for (_, specs) in SECTIONS {
                    for spec in *specs {
                        v.push((spec.key, Val::Text(f[spec.key])));
                    }
                }
            });
            v.extend([
                ("chat_template_kwargs", Val::Text(kwargs)),
                ("extra_args", Val::Text(extra_args)),
                ("capabilities_override", Val::Text(capabilities_override)),
            ]);
            v.into_iter()
                .map(|(key, val)| Entry {
                    key,
                    group: group_of(key),
                    val,
                })
                .collect::<Vec<_>>()
        });
        let baseline = RwSignal::new(None::<Vec<String>>);

        let dirty = Memo::new(move |_| {
            let mut out: Vec<(&'static str, &'static str)> = baseline.with(|b| {
                entries.with_value(|es| {
                    let now: Vec<String> = es.iter().map(|e| e.val.read()).collect();
                    changed_entries(b.as_deref(), &now)
                        .into_iter()
                        .map(|i| (es[i].group, es[i].key))
                        .collect::<Vec<_>>()
                })
            });
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
                            Kind::Int => {
                                raw.parse::<i64>().is_err().then_some("not a whole number")
                            }
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
            // §4.3 rule 1: Max output is mandatory on a ladder row — without
            // it no rung can promise a request fits. Skipped when the field
            // already has a parse error above, so this never doubles up.
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
            // Rung 1 (the base) is a row of the ladder table once Ladder is
            // on, and gets the live feedback every other rung gets.
            if ladder_on.get() && gguf_path.with(|g| g.trim().is_empty()) {
                out.push(("gguf_path", "a GGUF file is required".to_string()));
            }
            out
        });
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
        let ladder_table_error = Signal::derive(move || {
            (ladder_on.get() && rungs.with(Vec::is_empty)).then(|| {
                "Ladder is on but has no rungs above the base — add at least one, or turn \
                 Ladder off"
                    .to_string()
            })
        });
        let ladder_shared = Memo::new(move |_| {
            fields.with_value(|f| LadderShared {
                cache_type_k: f["cache_type_k"].get(),
                cache_type_v: f["cache_type_v"].get(),
                mmproj_path: f["mmproj_path"].get(),
                draft_gguf_path: f["draft_gguf_path"].get(),
                n_gpu_layers: f["n_gpu_layers"].get(),
                n_cpu_moe: f["n_cpu_moe"].get(),
                moe_cache_mib: f["moe_cache_mib"].get(),
                parallel: f["parallel"].get(),
                n_predict: f["n_predict"].get(),
            })
        });

        Self {
            fields,
            model_id,
            gguf_path,
            jinja,
            no_mmproj,
            public,
            enabled,
            idle_seconds,
            kwargs,
            capabilities_override,
            extra_args,
            image,
            extra_run_args,
            warm_start,
            fallback,
            gpu_pin,
            ladder_on,
            rungs,
            next_rung_key: StoredValue::new(0),
            ladder_on_baseline,
            ladder_baseline,
            entries,
            baseline,
            dirty,
            errors,
            rung_errors,
            ladder_table_error,
            ladder_shared,
        }
    }

    /// The text signal of a `LlamaParams` field.
    pub fn field(&self, key: &str) -> RwSignal<String> {
        self.fields.with_value(|f| f[key])
    }

    pub fn is_dirty(&self, key: &'static str) -> Signal<bool> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(|d| d.iter().any(|(_, k)| *k == key)))
    }

    /// Whether any field of `groups` is changed.
    pub fn groups_dirty(&self, groups: &'static [&'static str]) -> Signal<bool> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(|d| d.iter().any(|(g, _)| groups.contains(g))))
    }

    pub fn error_of(&self, key: &'static str) -> Signal<Option<String>> {
        let errors = self.errors;
        Signal::derive(move || {
            errors.with(|e| e.iter().find(|(k, _)| *k == key).map(|(_, m)| m.clone()))
        })
    }

    /// Whether a field of `groups` does not parse.
    pub fn groups_invalid(&self, groups: &'static [&'static str]) -> Signal<bool> {
        let errors = self.errors;
        Signal::derive(move || {
            errors.with(|e| e.iter().any(|(k, _)| groups.contains(&group_of(k))))
        })
    }

    pub fn rung_error_of(&self, key: u64) -> Signal<Option<String>> {
        let rung_errors = self.rung_errors;
        Signal::derive(move || {
            rung_errors.with(|e| e.iter().find(|(k, _)| *k == key).map(|(_, m)| m.clone()))
        })
    }

    pub fn dirty_count(&self) -> Signal<usize> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(Vec::len))
    }

    /// What is unsaved, group by group ("Context (2) · GPU (1)").
    pub fn dirty_detail(&self) -> Signal<String> {
        let dirty = self.dirty;
        Signal::derive(move || {
            dirty.with(|d| {
                let mut groups: Vec<(&str, usize)> = Vec::new();
                for (g, _) in d {
                    match groups.iter_mut().find(|(c, _)| c == g) {
                        Some(c) => c.1 += 1,
                        None => groups.push((g, 1)),
                    }
                }
                groups
                    .iter()
                    .map(|(c, n)| format!("{c} ({n})"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
        })
    }

    /// Fields that do not parse, rungs included: Save waits for them.
    pub fn invalid(&self) -> Signal<usize> {
        let (errors, rung_errors, table) = (self.errors, self.rung_errors, self.ladder_table_error);
        Signal::derive(move || {
            errors.with(Vec::len) + rung_errors.with(Vec::len) + usize::from(table.get().is_some())
        })
    }

    fn ladder_snapshot(&self) -> Vec<(String, String)> {
        self.rungs
            .get_untracked()
            .iter()
            .map(|r| {
                (
                    r.gguf_path.get_untracked().trim().to_string(),
                    r.ctx_size.get_untracked().trim().to_string(),
                )
            })
            .collect()
    }

    /// What the form holds right now, to be the baseline of a save that
    /// posts it: taken when the body is built, so what is typed while the
    /// save is in flight stays a change.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            values: self
                .entries
                .with_value(|es| es.iter().map(|e| e.val.read_untracked()).collect()),
            ladder_on: self.ladder_on.get_untracked(),
            ladder: self.ladder_snapshot(),
        }
    }

    /// What the form holds now is what is saved.
    pub fn rebase(&self) {
        self.rebase_to(self.snapshot());
    }

    /// What was posted is what is saved; the rest stays a change.
    pub fn rebase_to(&self, sent: Snapshot) {
        self.baseline.set(Some(sent.values));
        self.ladder_on_baseline.set(sent.ladder_on);
        self.ladder_baseline.set(sent.ladder);
    }

    /// The params into the form.
    pub fn fill(&self, p: &LlamaParams) {
        self.fields.with_value(|f| {
            for (key, sig) in f {
                sig.set(param_string(p, key));
            }
        });
        self.jinja.set(p.jinja);
        self.no_mmproj.set(p.no_mmproj);
        self.kwargs.set(if p.chat_template_kwargs.is_empty() {
            String::new()
        } else {
            serde_json::to_string_pretty(&p.chat_template_kwargs).unwrap_or_default()
        });
    }

    /// The saved row into the form, which then has no changes.
    pub fn load(&self, d: &LocalModelDetail) {
        self.model_id.set(d.model_id.clone());
        self.gguf_path.set(d.gguf_path.clone());
        self.public.set(d.public);
        self.enabled.set(d.enabled);
        self.idle_seconds.set(d.idle_seconds.to_string());
        self.extra_args.set(d.extra_args.join("\n"));
        self.image.set(d.image.clone().unwrap_or_default());
        self.extra_run_args
            .set(d.extra_run_args.clone().unwrap_or_default().join("\n"));
        self.warm_start.set(d.warm_start);
        self.fallback.set(d.fallback.clone().unwrap_or_default());
        self.gpu_pin.set(crate::pages::gpu_pin::pin_text(
            d.gpu_pin.as_deref().unwrap_or_default(),
        ));
        self.capabilities_override.set(
            d.capabilities_override
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default(),
        );
        self.ladder_on.set(!d.ladder.is_empty());
        let next = self.next_rung_key;
        self.rungs.set(
            d.ladder
                .iter()
                .map(|r| new_rung_row(next, r.gguf_path.clone(), r.ctx_size.to_string()))
                .collect(),
        );
        self.fill(&d.params);
        self.rebase();
    }

    /// Back to what was loaded.
    pub fn discard(&self) {
        let entries = self.entries;
        self.baseline.with_untracked(|b| {
            entries.with_value(|es| {
                for (e, v) in es.iter().zip(b.iter().flatten()) {
                    e.val.write(v);
                }
            })
        });
        self.ladder_on.set(self.ladder_on_baseline.get_untracked());
        let next = self.next_rung_key;
        self.rungs.set(
            self.ladder_baseline
                .get_untracked()
                .into_iter()
                .map(|(g, c)| new_rung_row(next, g, c))
                .collect(),
        );
    }

    pub fn add_rung(&self) {
        let next = self.next_rung_key;
        self.rungs
            .update(|r| r.push(new_rung_row(next, String::new(), String::new())));
    }

    /// What a "Fit again" save wrote: the fields take the saved values, as
    /// saved; other unsaved edits stay as they are.
    pub fn fit_saved(&self, changes: Vec<lmgw_api_types::model_get::FitChange>) {
        for c in changes {
            let to = c.to.map(|v| v.to_string()).unwrap_or_default();
            if let Some(sig) = self.fields.with_value(|f| f.get(c.key.as_str()).copied()) {
                sig.set(to.clone());
            }
            let at = self
                .entries
                .with_value(|es| es.iter().position(|e| e.key == c.key.as_str()));
            if let Some(i) = at {
                self.baseline.update(|b| {
                    if let Some(v) = b.as_mut().and_then(|b| b.get_mut(i)) {
                        *v = to.clone();
                    }
                });
            }
        }
    }

    /// The sparse patch: filled → value, emptied optional → `clear`.
    pub fn collect(&self) -> Result<Map<String, Value>, String> {
        let mut args = Map::new();
        let mut clear: Vec<&'static str> = Vec::new();
        self.fields.with_value(|f| -> Result<(), String> {
            for (_, specs) in SECTIONS {
                for spec in *specs {
                    let raw = f[spec.key].get_untracked();
                    let raw = raw.trim();
                    if raw.is_empty() {
                        clear.push(spec.key);
                        continue;
                    }
                    let val = match spec.kind {
                        Kind::Int => {
                            Value::from(raw.parse::<i64>().map_err(|_| {
                                format!("{}: '{raw}' is not an integer", spec.label)
                            })?)
                        }
                        Kind::Float => Value::from(
                            raw.parse::<f64>()
                                .map_err(|_| format!("{}: '{raw}' is not a number", spec.label))?,
                        ),
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
        let kw = self.kwargs.get_untracked();
        if kw.trim().is_empty() {
            clear.push("chat_template_kwargs");
        } else {
            args.insert("chat_template_kwargs".into(), Value::from(kw.trim()));
        }
        let cov = self.capabilities_override.get_untracked();
        if cov.trim().is_empty() {
            clear.push("capabilities_override");
        } else {
            args.insert("capabilities_override".into(), Value::from(cov.trim()));
        }
        args.insert("jinja".into(), Value::from(self.jinja.get_untracked()));
        args.insert(
            "no_mmproj".into(),
            Value::from(self.no_mmproj.get_untracked()),
        );
        args.insert("public".into(), Value::from(self.public.get_untracked()));
        args.insert("enabled".into(), Value::from(self.enabled.get_untracked()));
        args.insert(
            "idle_seconds".into(),
            Value::from(
                self.idle_seconds
                    .get_untracked()
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "idle seconds must be an integer".to_string())?,
            ),
        );
        // One argument per line.
        let flags: Vec<String> = self
            .extra_args
            .get_untracked()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        args.insert("extra_args".into(), Value::from(flags));
        let img = self.image.get_untracked();
        if img.trim().is_empty() {
            clear.push("image");
        } else {
            args.insert("image".into(), Value::from(img.trim()));
        }
        // One argument per line.
        let era: Vec<String> = self
            .extra_run_args
            .get_untracked()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        if era.is_empty() {
            clear.push("extra_run_args");
        } else {
            args.insert("extra_run_args".into(), Value::from(era));
        }
        args.insert(
            "warm_start".into(),
            Value::from(self.warm_start.get_untracked()),
        );
        // "None" clears a stored one; clearing none is no change.
        let fb = self.fallback.get_untracked().trim().to_string();
        if fb.is_empty() {
            clear.push("fallback");
        } else {
            args.insert("fallback".into(), Value::from(fb));
        }
        // The pin as the op takes it: the cards, an empty list clearing it
        // (device-ledger design §8).
        args.insert(
            "gpu_pin".into(),
            crate::pages::gpu_pin::pin_value(&self.gpu_pin.get_untracked()),
        );
        let mid = self.model_id.get_untracked().trim().to_string();
        if mid.is_empty() {
            return Err("model id is required".into());
        }
        args.insert("model_id".into(), Value::from(mid));
        let gp = self.gguf_path.get_untracked().trim().to_string();
        if gp.is_empty() {
            return Err("a GGUF file is required".into());
        }
        args.insert("gguf_path".into(), Value::from(gp));
        // Ladder (ladder design §4.1, §6): the higher rungs, or `clear` when
        // the switch is off — the same "filled → value, emptied → clear"
        // convention as `image`/`extra_run_args` above. Rows are already
        // shown as invalid before Save is reachable, but this is the one
        // place that turns them into real values or a hard refusal.
        if self.ladder_on.get_untracked() {
            let rows = self.rungs.get_untracked();
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
                let ctx_size = raw
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| format!("rung {rung_no}: '{}' is not an integer", raw.trim()))?;
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
    }

    /// Mirror of `LlamaParams::pool_unguarded_shared` (config/llama_params.rs):
    /// this form edits a sparse patch, not a loaded `LlamaParams`, so the
    /// rule is duplicated here, read off the same three fields (§3.3's "slots
    /// share one pool unguarded" note). Also mirrors
    /// `LlamaParams::effective_kv_unified` (review finding 3): llama-server
    /// forces unified whenever `parallel` is auto, overriding an explicit
    /// "false" — so auto wins outright and "false" only has a say once
    /// `parallel` names a real slot count.
    pub fn pool_unguarded(&self) -> bool {
        let ku = self.field("kv_unified").get();
        let parallel = self
            .field("parallel")
            .get()
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 1);
        let slots = parallel.unwrap_or(4);
        let effective_unified = parallel.is_none() || ku == "true";
        let guarded = ku == "true"
            && slots > 1
            && self
                .field("n_predict")
                .get()
                .trim()
                .parse::<i64>()
                .is_ok_and(|n| n > 0);
        effective_unified && slots > 1 && !guarded
    }
}

/// The indexes of the entries whose value differs from the baseline. Before
/// the form has taken a baseline (`None`, a saved row whose answer has not
/// come yet) nothing is changed: there is nothing to measure against.
fn changed_entries(baseline: Option<&[String]>, now: &[String]) -> Vec<usize> {
    let Some(b) = baseline else {
        return Vec::new();
    };
    now.iter()
        .enumerate()
        .filter(|(i, v)| b.get(*i).is_none_or(|was| was != *v))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod dirty_tests {
    use super::changed_entries;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nothing_is_changed_before_a_baseline_exists() {
        assert!(changed_entries(None, &v(&["a", "b"])).is_empty());
    }

    #[test]
    fn differing_entries_are_listed_once_a_baseline_exists() {
        let base = v(&["a", "b", "c"]);
        assert_eq!(changed_entries(Some(&base), &v(&["a", "x", "c"])), vec![1]);
        assert!(changed_entries(Some(&base), &base).is_empty());
        // An entry past the baseline's end counts as changed.
        assert_eq!(changed_entries(Some(&base[..2]), &base), vec![2]);
    }
}
