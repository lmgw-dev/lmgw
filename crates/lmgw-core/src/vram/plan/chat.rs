//! A chat row's footprint: its weights, projector and drafter, its KV cache
//! as llama.cpp lays it out for the row's slots, and the compute buffers of
//! its contexts (decision-models design, Appendix C, chat).
//!
//! **Weights.** The files' bytes, less the input embeddings llama.cpp keeps
//! in host memory: `token_embd` beside an untied output head (a tied head is
//! copied to the card), and Gemma 4's per-layer embeddings, 1.5 GiB of an
//! E4B file ([`ModelSummary::host_input_bytes`]). Less, too, the weights
//! the row's tensor overrides keep in host memory: the `n_cpu_moe` field,
//! `--n-cpu-moe`, `--cpu-moe`, `--n-cpu-ffn` and `--override-tensor`
//! patterns to the CPU, matched against every tensor of every part; an
//! override to a device that matches the input embeddings charges them
//! ([`offload`]). A draft model's own overrides count against its file.
//! The expert cache (`moe_cache_mib`, `--moe-cache-mib`) is charged in full
//! beside them while routed experts are kept in host memory.
//! With op offload a context whose ubatch reaches 32 is charged the largest
//! host-kept weight on top of its compute buffer, the copy llama.cpp makes
//! of it for a large batch's product.
//!
//! **KV cache.** Full-attention layers hold the whole context (padded to 256
//! cells); sliding-window layers hold `window + ubatch` cells per slot, or
//! `window · slots + ubatch` in a unified cache ([`cc::Graph::window_cells`]);
//! layers that reuse an earlier layer's cache (`attention.shared_kv_layers`)
//! hold none, and neither does V under multi-head latent attention. Cells are
//! charged at the cache type's real size, block scales included (q8_0 is
//! 8.5 bits). A linear-attention layer keeps a recurrent state per slot, and
//! one more per draft position a speculative row may roll back
//! (`n_rs_seq`). Matches every probed model to the MiB. A sparse-attention
//! indexer (`qwen4exp`) keeps a key cache of its own beside it
//! ([`qsa`]).
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
use crate::vram::compute::qsa;

use super::{cpu_only, cpu_only_args, Footprint, GgufFiles, PlanCache};

pub(crate) mod fit;
pub(crate) mod offload;
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
        ubatch: crate::runtime::aux_models::LLAMA_DEFAULT_UBATCH.unsigned_abs(),
        batch: crate::runtime::aux_models::LLAMA_DEFAULT_BATCH.unsigned_abs(),
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
    Some(
        cache_bytes(&layout, &g, bits, bits)
            .saturating_add(qsa::cache_bytes(s, &layout, &g, bits))
            .saturating_add(state),
    )
}

/// The footprint of a chat row of `shape` whose files were read (`files`):
/// weights + KV cache + compute buffers (module doc), led by the parts a
/// split GGUF among them is missing ([`GgufFiles::flag`]). Pure, so the
/// plan can ask it at other contexts without reading the files again.
pub(crate) fn footprint(files: &ChatFiles, gguf_path: &str, shape: &Shape) -> Footprint {
    files.files.flag(sized(files, gguf_path, shape))
}

/// [`footprint`] without the missing-part lead.
fn sized(files: &ChatFiles, gguf_path: &str, shape: &Shape) -> Footprint {
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
    // card ([`ModelSummary::host_input_bytes`]), and neither are the weights
    // its tensor overrides keep there (`n_cpu_moe`, [`offload`]); an
    // override to a device may send the input embeddings to the card after
    // all, since llama.cpp applies the overrides to them too.
    let mut notes = Vec::new();
    let host = s.host_input_bytes.min(files.files.main_bytes);
    // `--n-gpu-layers N` short of all of them leaves the first layers on the
    // CPU, after every real override ([`offload::Family::Layer`]).
    let first_gpu = first_gpu_layer(s, &shape.fit);
    let mut overrides = shape.overrides.clone();
    if let Some(first) = first_gpu {
        overrides.push(offload::Override::FirstLayers {
            flag: format!("--n-gpu-layers {}", shape.fit.gpu_layers.unwrap_or(0)),
            family: offload::Family::Layer,
            layers: first,
        });
        notes.push(format!(
            "--n-gpu-layers {} puts {} of its {} layers and the output layer on the card, and \
             their share of the KV cache{}",
            shape.fit.gpu_layers.unwrap_or(0),
            s.block_count.unwrap_or(0).saturating_sub(first),
            s.block_count.unwrap_or(0),
            if shape.fit.kv_on_host {
                " (none: --no-kv-offload keeps it in host memory)"
            } else {
                ""
            }
        ));
    }
    let placed = offload::place(s, &overrides, true, "its");
    let mut weights = match &placed {
        Some(p) => {
            notes.extend(p.notes.iter().cloned());
            weights
                .saturating_sub(host)
                .saturating_add(p.onto_card)
                .saturating_sub(p.off_card)
        }
        None if shape.tensors_to_device && s.host_input_bytes > 0 => {
            notes.push(format!(
                "its input embeddings ({}) are charged to the card: a --override-tensor in its \
                 args sends tensors to a device buffer, and llama.cpp applies it to them too",
                crate::hf::fmt_bytes(s.host_input_bytes)
            ));
            weights
        }
        None => weights.saturating_sub(host),
    };
    let placed = placed.unwrap_or_default();
    // The expert cache sits on the card beside the weights, while the row
    // keeps routed experts in host memory ([`offload`]).
    if shape.moe_cache_mib > 0 {
        let cache = shape.moe_cache_mib.saturating_mul(1024 * 1024);
        if placed.experts_off > 0 || placed.experts_unknown {
            weights = weights.saturating_add(cache);
            notes.push(format!(
                "its cache for the experts kept in host memory (--moe-cache-mib {}) is charged \
                 to the card in full",
                shape.moe_cache_mib
            ));
        } else {
            notes.push(format!(
                "--moe-cache-mib {} is not charged: the row keeps no routed experts in host \
                 memory for it to cache",
                shape.moe_cache_mib
            ));
        }
    }
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
    // The cache sits with its layer: only the layers on the card count, and
    // none with `--no-kv-offload`.
    let on_card = match first_gpu {
        Some(first) => Layout::of_from(s, first).unwrap_or_else(|_| layout.clone()),
        None => layout.clone(),
    };
    let mut kv = cache_bytes(&on_card, &main, bk, bv)
        .saturating_add(qsa::cache_bytes(s, &on_card, &main, bk))
        .saturating_add(recurrent(
            s,
            &on_card,
            shape.slots.saturating_mul(1 + shape.rollback),
            "its",
            &mut notes,
        ));
    if shape.fit.kv_on_host {
        if first_gpu.is_none() {
            notes.push(
                "--no-kv-offload keeps its KV cache in host memory: none of it is charged to the \
                 card"
                    .into(),
            );
        }
        kv = 0;
    }
    let mut compute = match cc::bytes(s, &layout, &main) {
        Ok(c) => c,
        Err(why) => {
            notes.push(format!("compute buffer not sized: {why}"));
            0
        }
    };
    // Op offload copies a host-kept weight into a context's compute buffer
    // when its ubatch reaches the backend's minimum ([`offload`]).
    let offloads =
        shape.op_offload && shape.ubatch.min(shape.batch).min(ctx) >= offload::OP_OFFLOAD_MIN_BATCH;
    let copy = |compute: &mut u64, context: &str, bytes: u64, notes: &mut Vec<String>| {
        if offloads && bytes > 0 {
            *compute = compute.saturating_add(bytes);
            notes.push(offload::copy_note(context, bytes));
        }
    };
    copy(
        &mut compute,
        "its main context's",
        placed.largest_main,
        &mut notes,
    );
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
                copy(
                    &mut compute,
                    "the MTP context's",
                    placed.largest_mtp,
                    &mut notes,
                );
            }
            None => notes.push(
                "spec_type draft-mtp, but the header has no MTP layers (nextn_predict_layers): \
                 llama-server cannot start it, and no MTP context is sized"
                    .into(),
            ),
        },
        Second::Draft { path, block } => match &files.draft {
            Some(Ok(d)) => {
                // The draft model's own overrides keep its weights in host
                // memory the same way (its input embeddings are charged).
                let dp = offload::place(d, &shape.draft_overrides, false, "the draft model's")
                    .unwrap_or_default();
                weights = weights.saturating_sub(dp.off_card);
                notes.extend(dp.notes);
                copy(
                    &mut compute,
                    "the draft context's",
                    dp.largest_main,
                    &mut notes,
                );
                match Layout::of(d) {
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
                        let outputs =
                            if matches!(d.architecture.as_deref(), Some("dflash" | "eagle3")) {
                                shape.batch
                            } else {
                                shape.slots.saturating_mul(if *block {
                                    shape.draft_n_max + 1
                                } else {
                                    1
                                })
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
                            Err(why) => notes
                                .push(format!("the draft model's compute buffer not sized: {why}")),
                        }
                    }
                    Err(why) => notes.push(format!(
                        "the draft model's KV cache and compute buffer not sized: {why}"
                    )),
                }
            }
            Some(Err(e)) => {
                notes.push(format!(
                    "the draft model's KV cache and compute buffer not sized: '{path}' could not \
                     be read ({e})"
                ));
                if !shape.draft_overrides.is_empty() {
                    notes.push(
                        "what the draft model's tensor overrides keep in host memory is charged \
                         to the card: its header was not read"
                            .into(),
                    );
                }
            }
            None => notes.push(format!(
                "the draft model's KV cache and compute buffer not sized: '{path}' was not read"
            )),
        },
    }
    let note = (!notes.is_empty()).then(|| notes.join(". "));
    Footprint::new(weights, kv, Some(ctx), note).with_compute(compute)
}

/// The first layer `fit`'s `--n-gpu-layers` puts on the card when that
/// leaves some on the CPU: llama.cpp counts the output layer as one more
/// layer and fills from the back (`i_gpu_start = n_layer + 1 − N`, b12244).
/// `None` for all of them, auto, or a header without a layer count.
pub(crate) fn first_gpu_layer(s: &ModelSummary, fit: &shape::Fit) -> Option<u64> {
    let n = fit.gpu_layers.filter(|n| *n > 0)?;
    let layers = s.block_count.filter(|b| *b > 0)?;
    (n <= layers).then(|| layers + 1 - n)
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
            let files =
                std::sync::Arc::new(ChatFiles::read(&dir, &path, &companions, draft.as_deref()));
            let fp = footprint(&files, &path, &shape);
            if cpu {
                // The CPU note replaces the footprint's; a missing part
                // still stops the load, so it still leads.
                files.files.flag(cpu_only(fp))
            } else {
                // A row llama.cpp's `--fit` would shrink carries how far
                // ([`fit`]); admission fits it to the card.
                let sizer = fit::Sizer::of(files.clone(), &path, &shape);
                Footprint {
                    llama: sizer.map(|s| fit::LlamaFit {
                        sizer: std::sync::Arc::new(s),
                        target: Default::default(),
                        fits_card: true,
                    }),
                    ..fp
                }
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
