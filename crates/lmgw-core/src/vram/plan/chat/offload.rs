//! The tensors a chat row keeps in host memory by its tensor overrides, and
//! what op offload copies back to the card (llama.cpp `common/arg.cpp`,
//! `llama-model-loader.cpp`, `ggml-backend.cpp`, 171e8846b).
//!
//! **The overrides.** llama.cpp turns each of these into an ordered list of
//! `(regex, buffer)` pairs, in argv order, and gives a weight the buffer of
//! the first pattern found anywhere in its name (`std::regex_search`), else
//! its layer's own:
//! - `--n-cpu-moe N` (`-ncmoe`, the row's `n_cpu_moe` field): one pattern
//!   `blk\.{i}\.ffn_(up|down|gate|gate_up)_(ch|)exps` to the CPU for each
//!   layer `i < N` (`LLM_FFN_EXPS_REGEX`): the routed experts' matrices.
//!   Shared experts (`_shexp`) and the router (`ffn_gate_inp`) stay on the
//!   card.
//! - `--cpu-moe` (`-cmoe`): the same pattern without a layer, every layer's.
//! - `--n-cpu-ffn N` (`-ncffn`): `blk\.{i}\.ffn_(up|down|gate)\.`, a dense
//!   model's FFN of the first N layers.
//! - `--override-tensor` / `-ot` `pattern=buffer,…`, split at the first `=`.
//!
//! Every spelling counts: the aliases, and underscores for dashes
//! (`--n_cpu_moe`, `--override_tensor`, [`canonical_key`]), which
//! llama-server reads the same. A flag takes its value as the next
//! argument; llama-server refuses `--n-cpu-moe=7`, which is read here too
//! but never rendered.
//!
//! The field renders before the freeform args, so its patterns come first,
//! and a copy of `--n-cpu-moe` in the args is dropped beside it
//! (`argv::push_freeform_args`). A draft model has its own list
//! (`--spec-draft-n-cpu-moe`, `--spec-draft-cpu-moe`,
//! `--spec-draft-override-tensor` and their aliases).
//!
//! The patterns are matched here with the `fancy-regex` crate, which reads
//! ECMAScript's lookaheads and backreferences as `std::regex` does. One it
//! cannot compile is unreadable, and the estimate stays on the safe side of
//! it: an unreadable pattern to the CPU keeps nothing off the card, and an
//! unreadable pattern to a device may claim any tensor before the overrides
//! that follow it, so nothing a later override keeps in host memory is taken
//! off the card either. The note says what that leaves charged.
//!
//! **The expert cache.** `--moe-cache-mib N` (the row's `moe_cache_mib`
//! field) keeps the most used of the experts held in host memory in a cache
//! of N MiB on the card. It is charged in full whenever the row keeps routed
//! experts in host memory, or may (an unreadable pattern to the CPU, a
//! header without a tensor list), and not otherwise. The flag is newer than the
//! llama.cpp source read here: its semantics are its `--help` line's ("GPU
//! cache size in MiB for the MoE experts kept in the CPU", default 0, off).
//!
//! **Op offload.** A matrix product whose weight sits in host memory runs on
//! the card when its batch is at least `GGML_OP_OFFLOAD_MIN_BATCH` tokens
//! (32 by default), and the scheduler copies the weight into the card's
//! compute buffer for it, one weight per split (`--no-op-offload` turns it
//! off). The reserved graph is the ubatch's, so a row whose ubatch reaches 32
//! reserves room for the largest host-kept weight. That is charged on top of
//! the widest phase's activations, an upper estimate: the copy lives only as
//! long as its product, and a type llama.cpp repacks for the CPU
//! (`CPU_REPACK` is not a host buffer) is computed there without one.

use regex::Regex;

use crate::gguf::ModelSummary;
use crate::hf::fmt_bytes;
use crate::runtime::argv::{canonical_key, is_opt};

/// llama.cpp's `LLM_FFN_EXPS_REGEX`: a layer's routed experts.
pub(crate) const EXPERTS: &str = r"\.ffn_(up|down|gate|gate_up)_(ch|)exps";

/// [`EXPERTS`], compiled once.
pub(crate) fn experts_regex() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(EXPERTS).expect("llama.cpp's experts pattern compiles")
    });
    &RE
}

/// llama.cpp's `LLM_FFN_DENSE_REGEX`: a layer's dense FFN.
const DENSE: &str = r"\.ffn_(up|down|gate)\.";

/// `GGML_OP_OFFLOAD_MIN_BATCH`'s default in the CUDA backend: the batch from
/// which a product on a host-kept weight runs on the card.
pub(crate) const OP_OFFLOAD_MIN_BATCH: u64 = 32;

/// Which FFN matrices a `--n-cpu-*` flag keeps on the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Family {
    Experts,
    Dense,
    /// Every weight of a layer: the layers `--n-gpu-layers` leaves on the
    /// CPU (`llama_model::load_tensors`, b12244: the last N−1 repeating
    /// layers and the output layer go to the card, the first ones stay).
    /// Listed after every real override, since an override wins over the
    /// layer's own device ([`crate::vram::plan::chat`]).
    Layer,
}

impl Family {
    /// The family's pattern with the layer captured, compiled once.
    fn layer_regex(&self) -> &'static Regex {
        use std::sync::LazyLock;
        fn of(f: Family) -> Regex {
            Regex::new(&format!(r"blk\.([0-9]+){}", f.regex()))
                .expect("the family patterns compile")
        }
        static EXPERTS_AT: LazyLock<Regex> = LazyLock::new(|| of(Family::Experts));
        static DENSE_AT: LazyLock<Regex> = LazyLock::new(|| of(Family::Dense));
        static LAYER_AT: LazyLock<Regex> = LazyLock::new(|| of(Family::Layer));
        match self {
            Family::Experts => &EXPERTS_AT,
            Family::Dense => &DENSE_AT,
            Family::Layer => &LAYER_AT,
        }
    }

    fn regex(self) -> &'static str {
        match self {
            Family::Experts => EXPERTS,
            Family::Dense => DENSE,
            Family::Layer => r"\.",
        }
    }
}

/// One tensor override, as llama.cpp lists it (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Override {
    /// A pattern to a buffer: `--override-tensor`, `--cpu-moe`. `cpu` when
    /// the buffer is the CPU's.
    Pattern {
        flag: String,
        pattern: String,
        cpu: bool,
    },
    /// `--n-cpu-moe N` / `--n-cpu-ffn N`: the family's matrices of layers
    /// `0..layers`, to the CPU.
    FirstLayers {
        flag: String,
        family: Family,
        layers: u64,
    },
}

impl Override {
    fn flag(&self) -> &str {
        match self {
            Override::Pattern { flag, .. } | Override::FirstLayers { flag, .. } => flag,
        }
    }

    fn cpu(&self) -> bool {
        match self {
            Override::Pattern { cpu, .. } => *cpu,
            Override::FirstLayers { .. } => true,
        }
    }
}

/// Whose overrides: the row's model, or its draft model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Whose {
    Model,
    Draft,
}

/// The flag `key` (dashes off, before any `=`, through [`canonical_key`])
/// names, by its spellings in llama.cpp's `arg.cpp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Tensor,
    CpuMoe,
    NCpuMoe,
    NCpuFfn,
}

fn flag_of(key: &str, whose: Whose) -> Option<Flag> {
    let key = canonical_key(key);
    Some(match (whose, &*key) {
        (Whose::Model, "override-tensor" | "ot") => Flag::Tensor,
        (Whose::Model, "cpu-moe" | "cmoe") => Flag::CpuMoe,
        (Whose::Model, "n-cpu-moe" | "ncmoe") => Flag::NCpuMoe,
        (Whose::Model, "n-cpu-ffn" | "ncffn") => Flag::NCpuFfn,
        (Whose::Draft, "spec-draft-override-tensor" | "otd" | "override-tensor-draft") => {
            Flag::Tensor
        }
        (Whose::Draft, "spec-draft-cpu-moe" | "cmoed" | "cpu-moe-draft") => Flag::CpuMoe,
        (
            Whose::Draft,
            "spec-draft-n-cpu-moe" | "spec-draft-ncmoe" | "ncmoed" | "n-cpu-moe-draft",
        ) => Flag::NCpuMoe,
        _ => return None,
    })
}

/// A buffer name that is the CPU's (`CPU`; llama.cpp refuses one it doesn't
/// list).
fn is_cpu(buffer: &str) -> bool {
    buffer.trim().to_ascii_uppercase().starts_with("CPU")
}

/// The overrides of `whose` in a row whose `n_cpu_moe` field is `field` and
/// whose freeform args are `args`, in llama.cpp's order (module doc). A
/// field of 0 or less keeps nothing on the CPU (llama-server refuses a
/// negative one), and still drops a copy in the args.
pub(crate) fn overrides(field: Option<i64>, args: &[String], whose: Whose) -> Vec<Override> {
    let mut out = Vec::new();
    if let Some(n) = field.filter(|_| whose == Whose::Model) {
        if n > 0 {
            out.push(Override::FirstLayers {
                flag: format!("--n-cpu-moe {n}"),
                family: Family::Experts,
                layers: n.unsigned_abs(),
            });
        }
    }
    let mut i = 0;
    while i < args.len() {
        let tok = &args[i];
        i += 1;
        if !is_opt(tok) {
            continue;
        }
        let bare = tok.trim_start_matches('-');
        let (key, inline) = match bare.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (bare, None),
        };
        let Some(flag) = flag_of(key, whose) else {
            continue;
        };
        // The flag as written, for the notes.
        let key = tok.split('=').next().unwrap_or(tok);
        if flag == Flag::CpuMoe {
            out.push(Override::Pattern {
                flag: key.to_string(),
                pattern: EXPERTS.into(),
                cpu: true,
            });
            continue;
        }
        let value = match inline {
            Some(v) => v,
            None => match args.get(i) {
                Some(v) if !is_opt(v) => {
                    i += 1;
                    v.clone()
                }
                _ => continue,
            },
        };
        match flag {
            Flag::Tensor => {
                for o in value.split(',') {
                    if let Some((pattern, buffer)) = o.split_once('=') {
                        out.push(Override::Pattern {
                            flag: format!("{key} {o}"),
                            pattern: pattern.to_string(),
                            cpu: is_cpu(buffer),
                        });
                    }
                }
            }
            Flag::NCpuMoe | Flag::NCpuFfn => {
                // The field wins over a copy in the args: the renderer drops
                // the copy.
                if flag == Flag::NCpuMoe && whose == Whose::Model && field.is_some() {
                    continue;
                }
                let Ok(n) = value.trim().parse::<u64>() else {
                    continue;
                };
                if n > 0 {
                    out.push(Override::FirstLayers {
                        flag: format!("{key} {n}"),
                        family: if flag == Flag::NCpuMoe {
                            Family::Experts
                        } else {
                            Family::Dense
                        },
                        layers: n,
                    });
                }
            }
            Flag::CpuMoe => {}
        }
    }
    out
}

/// `--op-offload` / `--no-op-offload` in `args`, the last one winning; on by
/// default.
pub(crate) fn op_offload(args: &[String]) -> bool {
    args.iter()
        .rev()
        .filter(|a| is_opt(a))
        .find_map(|a| match &*canonical_key(a.trim_start_matches('-')) {
            "op-offload" => Some(true),
            "no-op-offload" => Some(false),
            _ => None,
        })
        .unwrap_or(true)
}

/// An override, compiled.
enum Matcher {
    Pattern(fancy_regex::Regex),
    /// The family's pattern with the layer captured, and the layer bound.
    Layers(Regex, u64),
    /// A pattern `fancy-regex` could not compile, and why.
    Unreadable(String),
}

impl Matcher {
    fn of(o: &Override) -> Self {
        match o {
            Override::Pattern { pattern, .. } => match fancy_regex::Regex::new(pattern) {
                Ok(r) => Matcher::Pattern(r),
                Err(e) => Matcher::Unreadable(e.to_string()),
            },
            Override::FirstLayers { family, layers, .. } => {
                Matcher::Layers(family.layer_regex().clone(), *layers)
            }
        }
    }

    /// llama.cpp's search: anywhere in `name`. A layer is written as
    /// `%d`, so `blk.05` is no layer 5. A search `fancy-regex` gives up on
    /// (its backtracking limit, which a tensor name does not come near) is
    /// no match.
    fn matches(&self, name: &str) -> bool {
        match self {
            Matcher::Pattern(r) => r.is_match(name).unwrap_or(false),
            Matcher::Layers(r, layers) => r.captures_iter(name).any(|c| {
                let digits = &c[1];
                digits
                    .parse::<u64>()
                    .is_ok_and(|n| n < *layers && n.to_string() == digits)
            }),
            Matcher::Unreadable(_) => false,
        }
    }
}

/// Where a row's overrides put its weights (module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Placed {
    /// The bytes the overrides keep in host memory, beyond the input
    /// embeddings llama.cpp keeps there anyway.
    pub off_card: u64,
    /// The bytes of those input embeddings an override sends to a device.
    pub onto_card: u64,
    /// The largest weight kept in host memory in the layers the main
    /// context runs, and in the MTP layers: what op offload copies to the
    /// card ([`OP_OFFLOAD_MIN_BATCH`]).
    pub largest_main: u64,
    pub largest_mtp: u64,
    /// Of [`Self::off_card`], the routed experts' ([`EXPERTS`]): what
    /// `--moe-cache-mib` caches on the card.
    pub experts_off: u64,
    /// Whether the row keeps routed experts in host memory isn't known: the
    /// header lists no tensors, or an unreadable pattern may keep some there
    /// ([`Matcher::Unreadable`]).
    pub experts_unknown: bool,
    /// What a reader of the footprint needs to know, one sentence each.
    pub notes: Vec<String>,
}

/// The layer of a `blk.N.…` tensor.
fn layer_of(name: &str) -> Option<u64> {
    name.strip_prefix("blk.")?.split('.').next()?.parse().ok()
}

/// Where `overrides` put the weights of `s` (module doc), or `None` without
/// overrides. `host_inputs` says whether `s`'s host-kept input embeddings
/// are subtracted from its bytes already (the model's are, a draft model's
/// are not). `whose` names the model in the notes ("its", "the draft
/// model's").
pub(crate) fn place(
    s: &ModelSummary,
    overrides: &[Override],
    host_inputs: bool,
    whose: &str,
) -> Option<Placed> {
    if overrides.is_empty() {
        return None;
    }
    let mut p = Placed::default();
    let matchers: Vec<Matcher> = overrides.iter().map(Matcher::of).collect();
    // The first unreadable pattern to a device: it may claim any tensor
    // ahead of the overrides after it (module doc).
    let shadow = overrides
        .iter()
        .zip(&matchers)
        .position(|(o, m)| matches!(m, Matcher::Unreadable(_)) && !o.cpu());
    for (i, (o, m)) in overrides.iter().zip(&matchers).enumerate() {
        if let Matcher::Unreadable(why) = m {
            let what = if o.cpu() {
                // It may keep experts in host memory that the cache is for.
                p.experts_unknown = true;
                "the weights it keeps in host memory are charged to the card".to_string()
            } else if Some(i) == shadow && i + 1 < overrides.len() {
                format!(
                    "it may send any tensor to a device ahead of the overrides after it ({}), so \
                     what they keep in host memory is charged to the card, and so are the input \
                     embeddings",
                    flags(&overrides[i + 1..])
                )
            } else {
                "the input embeddings it may send to a device are charged to the card".to_string()
            };
            p.notes.push(format!(
                "{whose} tensor override '{}' is not a pattern lmgw can match ({why}): {what}",
                o.flag()
            ));
        }
    }
    let list = &s.tensor_list;
    if list.tensors.is_empty() {
        p.notes.push(format!(
            "{whose} header lists no tensors, so the weights its tensor overrides ({}) keep in \
             host memory are charged to the card",
            flags(overrides)
        ));
        if host_inputs && overrides.iter().any(|o| !o.cpu()) {
            p.onto_card = s.host_input_bytes;
            p.notes.push(onto_note(whose, p.onto_card));
        }
        p.experts_unknown = overrides.iter().any(Override::cpu);
        return Some(p);
    }
    let main_layers = s
        .block_count
        .map(|b| b.saturating_sub(s.nextn_predict_layers.unwrap_or(0)));
    let tied = s.head_tensors.output.is_none();
    let mut kept = vec![0u64; overrides.len()];
    let mut shadowed = 0u64;
    let expert = experts_regex();
    for (name, bytes) in &list.tensors {
        let host_kept = host_inputs
            && (name == "per_layer_token_embd.weight" || (name == "token_embd.weight" && !tied));
        let Some(k) = matchers.iter().position(|m| m.matches(name)) else {
            continue;
        };
        let cpu = overrides[k].cpu();
        if cpu && !host_kept {
            if shadow.is_some_and(|d| k > d) {
                // The unreadable pattern before it may have sent it to a
                // device: it stays charged, and so may be the experts the
                // cache is for.
                shadowed = shadowed.saturating_add(*bytes);
                p.experts_unknown |= expert.is_match(name);
                continue;
            }
            kept[k] = kept[k].saturating_add(*bytes);
            p.off_card = p.off_card.saturating_add(*bytes);
            if expert.is_match(name) {
                p.experts_off = p.experts_off.saturating_add(*bytes);
            }
            // A layer's weight is copied for the context that runs that
            // layer; one outside the layers (the output head) for both.
            let (main, mtp) = match (layer_of(name), main_layers) {
                (Some(l), Some(m)) => (l < m, l >= m),
                (Some(_), None) => (true, false),
                (None, _) => (true, true),
            };
            if main {
                p.largest_main = p.largest_main.max(*bytes);
            }
            if mtp {
                p.largest_mtp = p.largest_mtp.max(*bytes);
            }
        } else if !cpu && host_kept {
            p.onto_card = p.onto_card.saturating_add(*bytes);
        }
    }
    if shadow.is_some() && host_inputs {
        p.onto_card = s.host_input_bytes;
    }
    let by: Vec<(&str, u64)> = overrides
        .iter()
        .zip(&kept)
        .filter(|(o, b)| o.cpu() && **b > 0)
        .map(|(o, b)| (o.flag(), *b))
        .collect();
    let which = match by.as_slice() {
        [] => None,
        [(flag, _)] => Some(format!("by {flag}")),
        _ => Some(
            by.iter()
                .map(|(flag, b)| format!("{} by {flag}", fmt_bytes(*b)))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    };
    if let Some(which) = which {
        p.notes.push(format!(
            "{} of {whose} weights stay in host memory and are not charged to the card ({which})",
            fmt_bytes(p.off_card)
        ));
    }
    if shadowed > 0 {
        p.notes.push(format!(
            "{} of {whose} weights that the overrides after the unreadable one keep in host \
             memory are charged to the card",
            fmt_bytes(shadowed)
        ));
    }
    let idle: Vec<&str> = overrides
        .iter()
        .zip(&kept)
        .zip(&matchers)
        .enumerate()
        .filter(|(i, ((o, b), m))| {
            o.cpu()
                && **b == 0
                && !matches!(m, Matcher::Unreadable(_))
                && !shadow.is_some_and(|d| *i > d)
        })
        .map(|(_, ((o, _), _))| o.flag())
        .collect();
    if !idle.is_empty() {
        p.notes.push(format!(
            "{}: none of {whose} tensors is kept in host memory by it (none matches, or an \
             earlier override took it)",
            idle.join(", ")
        ));
    }
    if p.onto_card > 0 {
        p.notes.push(onto_note(whose, p.onto_card));
    }
    if !list.complete(s.split_count) {
        p.notes.push(format!(
            "only {} of the {} parts' tensors were read, so what the overrides keep in host \
             memory from the other parts is charged to the card",
            list.parts,
            s.split_count.unwrap_or(1)
        ));
    }
    Some(p)
}

/// The note for input embeddings an override to a device charges.
fn onto_note(whose: &str, bytes: u64) -> String {
    format!(
        "{whose} input embeddings ({}) are charged to the card: a tensor override in its args \
         sends them to a device buffer, and llama.cpp applies the overrides to them too",
        fmt_bytes(bytes)
    )
}

/// The overrides' flags, for a note.
fn flags(overrides: &[Override]) -> String {
    overrides
        .iter()
        .map(Override::flag)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The note for an op-offload copy of `bytes` charged to `context`'s
/// compute buffer (module doc).
pub(crate) fn copy_note(context: &str, bytes: u64) -> String {
    format!(
        "with op offload llama.cpp copies a host-kept weight to the card for a batch of \
         {OP_OFFLOAD_MIN_BATCH} tokens or more, so {context} compute buffer is charged the \
         largest one ({}), an upper estimate: the copy lives only for its product, and a type \
         llama.cpp repacks for the CPU is computed there without one",
        fmt_bytes(bytes)
    )
}

#[cfg(test)]
mod tests;
