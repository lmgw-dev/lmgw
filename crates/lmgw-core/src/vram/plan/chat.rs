//! A chat row's footprint: its weights, projector and drafter, its KV cache
//! as llama.cpp lays it out for the row's slots, and the compute buffers of
//! its contexts (decision-models design, Appendix C, chat).
//!
//! **Weights.** The files' bytes, less the input embeddings llama.cpp keeps
//! in host memory: `token_embd` beside an untied output head (a tied head is
//! copied to the card), and Gemma 4's per-layer embeddings, 1.5 GiB of an
//! E4B file ([`ModelSummary::host_input_bytes`]).
//!
//! **KV cache.** Full-attention layers hold the whole context (padded to 256
//! cells); sliding-window layers hold `window + ubatch` cells per slot, or
//! `window · slots + ubatch` in a unified cache ([`cc::Graph::window_cells`]);
//! layers that reuse an earlier layer's cache (`attention.shared_kv_layers`)
//! hold none, and neither does V under multi-head latent attention. Cells are
//! charged at the cache type's real size, block scales included (q8_0 is
//! 8.5 bits). A linear-attention layer keeps a recurrent state per slot, and
//! one more per draft position a speculative row may roll back
//! (`n_rs_seq`). Matches every probed model to the MiB.
//!
//! **Second context.** A speculative row runs one: `draft-mtp` without a
//! draft model runs the weights' own MTP layers, with a cache of their own
//! at the draft cache types (f16 by default); a draft model runs its own
//! layers, with a cache of its own unless it shares the target's (a Gemma 4
//! MTP assistant shares all of its layers, so it reads the target's cache).
//! Each context reserves its own compute buffer ([`cc::bytes`]).
//!
//! **Compute buffer.** [`crate::vram::compute::chat`]: the masks, the f16
//! copy flash attention reads a quantized cache through, and the widest
//! phase's activations, per context.
//!
//! What a header cannot size is said in the note and left out: a recurrent
//! state without `ssm.*` keys, a draft model whose file cannot be read, a
//! cache type lmgw does not know (sized as f16). A projector's own compute
//! buffer and the CUDA context are the headroom's.

use std::path::PathBuf;

use crate::config::LocalModel;
use crate::gguf::{self, ModelSummary};
use crate::runtime::argv::flag_value;
use crate::vram::compute::chat::{self as cc, Graph};
use crate::vram::compute::layers::{Kind, Layout};

use super::{cpu_only, cpu_only_args, Footprint, GgufFiles, PlanCache};

pub(crate) mod shape;
pub(crate) use shape::{Second, Shape};

/// A chat row's files, read: the weights, the projector's and the draft
/// model's bytes, the weights' header and the draft model's.
pub(crate) struct ChatFiles {
    pub files: GgufFiles,
    /// The draft model's header, when the row names a draft model.
    pub draft: Option<Result<ModelSummary, String>>,
}

impl ChatFiles {
    /// Blocking: read `gguf_path`, `companions` and the draft model at
    /// `draft` under `models_dir`.
    pub(crate) fn read(
        models_dir: &str,
        gguf_path: &str,
        companions: &[String],
        draft: Option<&str>,
    ) -> Self {
        let root = PathBuf::from(models_dir);
        Self {
            files: GgufFiles::read(models_dir, gguf_path, companions),
            draft: draft.map(|d| {
                gguf::summarize(&root.join(d.trim_start_matches('/'))).map_err(|e| e.to_string())
            }),
        }
    }
}

/// Bits per 32-element block of a cache type, and the note an unknown one
/// earns (sized as f16).
fn bits(t: Option<&str>, which: &str, notes: &mut Vec<String>) -> u32 {
    shape::block_bits(t).unwrap_or_else(|| {
        notes.push(format!(
            "{which} cache type '{}' is not one lmgw knows: sized as f16",
            t.unwrap_or_default()
        ));
        512
    })
}

/// The KV cache a context of `layout` keeps at `g`, K and V at `bk` and `bv`
/// bits per 32 elements (module doc).
fn cache_bytes(layout: &Layout, g: &Graph, bk: u32, bv: u32) -> u64 {
    let of = |k: &Kind, cells: u64| {
        let bits =
            k.dk.saturating_mul(u64::from(bk))
                .saturating_add(k.dv.saturating_mul(u64::from(bv)));
        cells
            .saturating_mul(g.streams())
            .saturating_mul(k.own_heads)
            .saturating_mul(bits)
            / 256
    };
    let full = layout.full.as_ref().map_or(0, |k| of(k, g.full_cells()));
    let swa = layout
        .swa
        .as_ref()
        .map_or(0, |(k, w)| of(k, g.window_cells(*w)));
    full.saturating_add(swa)
}

/// The recurrent state of `layout`'s layers without attention, `states`
/// copies of it (slots × snapshots), or 0 with a note naming `whose` layers
/// when the header has no `ssm.*` keys to size it by.
fn recurrent(
    s: &ModelSummary,
    layout: &Layout,
    states: u64,
    whose: &str,
    notes: &mut Vec<String>,
) -> u64 {
    if layout.recurrent_layers == 0 {
        return 0;
    }
    match gguf::recurrent_state_bytes(s) {
        Some(per) => per
            .saturating_mul(layout.recurrent_layers)
            .saturating_mul(states),
        None => {
            notes.push(format!(
                "the recurrent state of {whose} {} layers without attention is not sized (the \
                 header states no ssm.state_size / ssm.inner_size)",
                layout.recurrent_layers
            ));
            0
        }
    }
}

/// The KV cache of a model of header `s` at `ctx` tokens for one slot at
/// llama-server's default ubatch, at cache type `cache` (both K and V), laid
/// out as a chat row's footprint lays it out, its recurrent state included:
/// what `model_inspect` reports for a chat model. `None` when the header
/// cannot be laid out or the type is not one lmgw knows.
pub(crate) fn one_slot_kv(s: &ModelSummary, ctx: u64, cache: &str) -> Option<u64> {
    let layout = Layout::of(s).ok()?;
    let bits = shape::block_bits(Some(cache))?;
    let g = Graph {
        ctx,
        ubatch: crate::runtime::aux::LLAMA_DEFAULT_UBATCH.unsigned_abs(),
        batch: crate::runtime::aux::LLAMA_DEFAULT_BATCH.unsigned_abs(),
        slots: 1,
        unified: false,
        flash_attn_off: false,
        k_copied: false,
        v_copied: false,
        outputs: 1,
        single_layer: false,
        swa_full: false,
    };
    let state = match layout.recurrent_layers {
        0 => 0,
        n => gguf::recurrent_state_bytes(s)
            .unwrap_or(0)
            .saturating_mul(n),
    };
    Some(cache_bytes(&layout, &g, bits, bits).saturating_add(state))
}

/// The footprint of a chat row of `shape` whose files were read (`files`):
/// weights + KV cache + compute buffers (module doc). Pure, so the plan can
/// ask it at other contexts without reading the files again.
pub(crate) fn footprint(files: &ChatFiles, gguf_path: &str, shape: &Shape) -> Footprint {
    let weights = files
        .files
        .main_bytes
        .saturating_add(files.files.companion_bytes);
    let s = match &files.files.summary {
        Ok(s) => s,
        Err(e) => {
            return Footprint::new(
                weights,
                0,
                None,
                Some(format!(
                    "KV cache and compute buffer not sized: '{gguf_path}' could not be read \
                     ({e}) — the estimate is weights only"
                )),
            )
        }
    };
    // The input embeddings llama.cpp keeps in host memory are not on the
    // card ([`ModelSummary::host_input_bytes`]), unless a tensor override
    // may have sent them there: llama.cpp applies `-ot` to them too.
    let mut notes = Vec::new();
    let weights = if shape.tensors_to_device && s.host_input_bytes > 0 {
        notes.push(format!(
            "its input embeddings ({}) are charged to the card: a --override-tensor in its \
             args sends tensors to a device buffer, and llama.cpp applies it to them too",
            crate::hf::fmt_bytes(s.host_input_bytes)
        ));
        weights
    } else {
        weights.saturating_sub(s.host_input_bytes.min(files.files.main_bytes))
    };
    let not_sized = |ctx: Option<u64>, why: String, notes: &[String]| {
        let fp = Footprint::new(weights, 0, ctx, Some(why));
        notes.iter().fold(fp, |fp, n| fp.noting(n.clone()))
    };
    let Some(ctx) = shape.ctx.or(s.context_length).filter(|c| *c > 0) else {
        return not_sized(
            None,
            "KV cache and compute buffer not sized: the row sets no ctx_size and the GGUF \
             declares no context length"
                .into(),
            &notes,
        );
    };
    let layout = match Layout::of(s) {
        Ok(l) => l,
        Err(why) => {
            return not_sized(
                Some(ctx),
                format!(
                    "KV cache and compute buffer not sized: {why}, and none of it is guessable"
                ),
                &notes,
            )
        }
    };
    let (bk, bv) = (
        bits(shape.cache_k.as_deref(), "K", &mut notes),
        bits(shape.cache_v.as_deref(), "V", &mut notes),
    );
    let graph = |outputs: u64, k: Option<&str>, v: Option<&str>, single_layer: bool| Graph {
        ctx,
        ubatch: shape.ubatch,
        batch: shape.batch,
        slots: shape.slots,
        unified: shape.unified,
        flash_attn_off: shape.flash_attn_off,
        k_copied: shape::copied(k),
        v_copied: shape::copied(v),
        outputs,
        single_layer,
        swa_full: shape.swa_full,
    };
    let main = graph(
        shape.main_outputs(),
        shape.cache_k.as_deref(),
        shape.cache_v.as_deref(),
        false,
    );
    let mut kv = cache_bytes(&layout, &main, bk, bv).saturating_add(recurrent(
        s,
        &layout,
        shape.slots.saturating_mul(1 + shape.rollback),
        "its",
        &mut notes,
    ));
    let mut compute = match cc::bytes(s, &layout, &main) {
        Ok(c) => c,
        Err(why) => {
            notes.push(format!("compute buffer not sized: {why}"));
            0
        }
    };
    let (dk, dv) = (
        shape.draft_cache_k.as_deref(),
        shape.draft_cache_v.as_deref(),
    );
    let (dbk, dbv) = (
        bits(dk, "the draft context's K", &mut notes),
        bits(dv, "the draft context's V", &mut notes),
    );
    match &shape.second {
        Second::None => {}
        Second::Mtp => match Layout::mtp(s, &layout) {
            Some(mtp) => {
                let g = graph(shape.slots, dk, dv, true);
                kv = kv.saturating_add(cache_bytes(&mtp, &g, dbk, dbv));
                match cc::bytes(s, &mtp, &g) {
                    Ok(c) => compute = compute.saturating_add(c),
                    Err(why) => {
                        notes.push(format!("the MTP context's compute buffer not sized: {why}"))
                    }
                }
            }
            None => notes.push(
                "spec_type draft-mtp, but the header has no MTP layers (nextn_predict_layers): \
                 llama-server cannot start it, and no MTP context is sized"
                    .into(),
            ),
        },
        Second::Draft { path, block } => match &files.draft {
            Some(Ok(d)) => match Layout::of(d) {
                Ok(dl) => {
                    // A draft model whose every layer reuses the target's
                    // cache reads that cache, at the target's types.
                    let shares = d
                        .shared_kv_layers
                        .is_some_and(|n| n >= d.block_count.unwrap_or(u64::MAX));
                    // An EAGLE3 or DFlash drafter is an encoder to llama.cpp
                    // (`llama_model_has_encoder`), so its context reserves
                    // an output per batch token (measured: a
                    // DFlash drafter, 803 MiB of compute at batch 1024);
                    // a block drafter's otherwise `n_max + 1` per slot.
                    let outputs = if matches!(d.architecture.as_deref(), Some("dflash" | "eagle3"))
                    {
                        shape.batch
                    } else {
                        shape
                            .slots
                            .saturating_mul(if *block { shape.draft_n_max + 1 } else { 1 })
                    };
                    let g = if shares {
                        graph(
                            outputs,
                            shape.cache_k.as_deref(),
                            shape.cache_v.as_deref(),
                            false,
                        )
                    } else {
                        let g = graph(outputs, dk, dv, false);
                        // Its own cache, and a hybrid drafter's recurrent
                        // state, one per slot (a draft context keeps no
                        // rollback snapshots).
                        kv = kv
                            .saturating_add(cache_bytes(&dl, &g, dbk, dbv))
                            .saturating_add(recurrent(
                                d,
                                &dl,
                                shape.slots,
                                "the draft model's",
                                &mut notes,
                            ));
                        g
                    };
                    match cc::bytes(d, &dl, &g) {
                        Ok(c) => compute = compute.saturating_add(c),
                        Err(why) => {
                            notes.push(format!("the draft model's compute buffer not sized: {why}"))
                        }
                    }
                }
                Err(why) => notes.push(format!(
                    "the draft model's KV cache and compute buffer not sized: {why}"
                )),
            },
            Some(Err(e)) => notes.push(format!(
                "the draft model's KV cache and compute buffer not sized: '{path}' could not be \
                 read ({e})"
            )),
            None => notes.push(format!(
                "the draft model's KV cache and compute buffer not sized: '{path}' was not read"
            )),
        },
    }
    let note = (!notes.is_empty()).then(|| notes.join(". "));
    Footprint::new(weights, kv, Some(ctx), note).with_compute(compute)
}

impl PlanCache {
    /// Chat row `m`'s footprint (module doc), on the CPU when its
    /// `n_gpu_layers` is 0.
    pub async fn local(&self, models_dir: &str, m: &LocalModel) -> Footprint {
        let shape = Shape::of(&m.params, &m.args);
        let draft = match &shape.second {
            Second::Draft { path, .. } => Some(path.clone()),
            _ => None,
        };
        // The projector and the drafter load beside the weights. The fields
        // first; a copy still riding in the args renders too.
        let mmproj = m
            .params
            .mmproj_path
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| {
                flag_value(&m.args, "mmproj")
                    .map(|p| p.trim().trim_start_matches("/models/").to_string())
            });
        let companions: Vec<String> = [mmproj, draft.clone()]
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty())
            .collect();
        // The structured field first; the freeform escape hatch is read too
        // because the argv renderer honours whichever one is present.
        let cpu = m.params.n_gpu_layers == Some(0) || cpu_only_args(&m.args);
        let key = format!(
            "local|{models_dir}|{}|{}|{shape:?}|{cpu}",
            m.gguf_path,
            companions.join(",")
        );
        let (dir, path) = (models_dir.to_string(), m.gguf_path.clone());
        self.get_or_insert(key, move || {
            let files = ChatFiles::read(&dir, &path, &companions, draft.as_deref());
            let fp = footprint(&files, &path, &shape);
            if cpu {
                cpu_only(fp)
            } else {
                fp
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
