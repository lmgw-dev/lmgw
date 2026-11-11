//! Where one aux row runs (decision-models design §3.3): the row's own
//! `backend`, the `podman run` args that go with it, and its thread count —
//! the aux sibling of [`super::audio`]'s per-row CPU switch.
//!
//! **Placement** ([`placement`]): `backend: cpu` is a container on the CPU,
//! no `backend` one on the GPU. Only the field decides: a row that still
//! carries `--n-gpu-layers 0` in its freeform args is charged nothing by the
//! VRAM plan, as before, but stays a GPU container for the hold and the
//! ledger. The boot hoist moves that flag into the field only when the row's
//! image has no GPU backend (§3.3, WP2b, `lifecycle::hoist_aux_args`): on a
//! GPU image the device still carries the row's compute at `-ngl 0`, so it
//! stays where it is.
//!
//! **Run args** ([`run_args`]): a CPU row that inherits the class's
//! `extra_run_args` gets them without the GPU passthrough and with
//! `NVIDIA_VISIBLE_DEVICES=void` ([`cpu_run_args`]), the same rule as audio:
//! a CUDA build at `-ngl 0` with a visible device still opens a CUDA context.
//! A row's own override is used verbatim, and [`own_args_pass_gpu`] says when
//! it still hands the container a GPU.
//!
//! **Threads** ([`threads_in_effect`], the rule in
//! [`lmgw_api_types::aux_engine`] so the aux editor shows the same count):
//! the row's own count wins. Without one, a `--threads`/`-t` in its freeform
//! args renders as written. A CPU row that names neither runs this machine's
//! physical cores (the logical CPU count where the core topology cannot be
//! read, [`crate::host::cpu`]), as an audio row switched to the CPU does. A
//! GPU row that names neither renders nothing and llama-server picks.
//!
//! **Batching** ([`ubatch_in_effect`], [`batch_in_effect`]): the row's
//! `ubatch_size` / `batch_size` field, else a `--ubatch-size`/`-ub` or
//! `--batch-size`/`-b` its freeform args still carry, which renders while the
//! field is unset (the boot hoist moves it into the field). Neither means
//! llama-server's default. An input llama-server cannot split across
//! ubatches ([`splits_input`]: a non-causal encoder's, or any but a
//! last-token one's) is evaluated in one, so this is what bounds every input
//! such a row answers — the rule the aux plan, the row's findings and a
//! knowledge base's chunk check all read.

use crate::config::{AuxBackend, AuxKind, AuxModel, RouterSettings};
use crate::host::HostCpu;
use crate::runtime::argv::flag_value;
use crate::runtime::Placement;

pub use lmgw_api_types::audio_engine::{cpu_run_args, passes_gpu};
pub use lmgw_api_types::aux_engine::AuxThreadsSource;

/// Where a container started for a row with `backend` computes.
pub fn placement(backend: Option<AuxBackend>) -> Placement {
    match backend {
        Some(AuxBackend::Cpu) => Placement::Cpu,
        None => Placement::Gpu,
    }
}

/// `m`'s effective `podman run` args: its own override verbatim, else the
/// class's — without the GPU passthrough when `m` runs on the CPU.
pub fn run_args(m: &AuxModel, class: &RouterSettings) -> Vec<String> {
    match &m.extra_run_args {
        Some(own) => own.clone(),
        None if placement(m.backend) == Placement::Cpu => cpu_run_args(&class.extra_run_args),
        None => class.extra_run_args.clone(),
    }
}

/// `m` runs on the CPU, but its own run args — used verbatim — still hand
/// its container a GPU.
pub fn own_args_pass_gpu(m: &AuxModel) -> bool {
    placement(m.backend) == Placement::Cpu && m.extra_run_args.as_deref().is_some_and(passes_gpu)
}

/// What a CPU row whose own args still pass a GPU is told.
pub const OWN_ARGS_PASS_GPU: &str = "this row runs on the CPU, but its own run args still pass \
     the GPU to its container (a GPU --device, --gpus, a /dev/nvidia* volume, the NVIDIA runtime \
     or NVIDIA_VISIBLE_DEVICES): lmgw uses them as written, so llama-server can still open a \
     CUDA context on the card — remove those flags, or clear the override to inherit the class \
     args without them";

/// `m`'s thread count on `host` and where it comes from (module doc).
pub fn threads_in_effect(m: &AuxModel, host: HostCpu) -> (Option<i64>, AuxThreadsSource) {
    lmgw_api_types::aux_engine::threads_in_effect(
        m.threads,
        placement(m.backend) == Placement::Cpu,
        &m.args,
        &lmgw_api_types::HostCpu::from(host),
    )
}

/// The `--threads` the row's fields render: its own count, else this
/// machine's cores on the CPU, else nothing. A count its freeform args name
/// gets none: the renderer would claim the key and drop the count the owner
/// wrote.
pub fn rendered_threads(m: &AuxModel, host: HostCpu) -> Option<i64> {
    match threads_in_effect(m, host) {
        (n, source) if source.rendered() => n,
        _ => None,
    }
}

/// llama.cpp's default `--ubatch-size` (`common_params::n_ubatch`): what a
/// row that sets no ubatch runs with, an aux row and a chat row alike. On a
/// row whose inputs llama-server cannot split ([`splits_input`]) it bounds
/// every input ([`ubatch_in_effect`]).
pub const LLAMA_DEFAULT_UBATCH: i64 = 512;

/// Whether llama-server can split one embedding or rerank input of an aux
/// row across ubatches: its `can_split` (`server-context.cpp`), which allows
/// it only for a model with a KV cache whose answer is read from the last
/// token — `last` pooling, or a causal reranker's rank pooling. Every other
/// input is evaluated in one ubatch, so the ubatch in effect bounds it, and
/// an input past it fails with a 500 ("input (N tokens) is too large to
/// process"): a non-causal encoder (no KV cache), and a mean-, cls- or
/// none-pooled one whatever its attention.
///
/// `causal` is the header's `attention.causal`, unknown read as causal;
/// `pooling` the pooling the server runs (the row's, else the header's). A
/// decision row is not judged here: its type decides (decision-models
/// design §3.4). Measured 2026-10-13: Qwen3-Embedding-0.6B (last) and
/// Qwen3-Reranker-0.6B (causal) answered 3,500-token inputs at the default
/// ubatch; embeddinggemma-300M (mean) refused 1,492 tokens until its ubatch
/// was raised to 2048.
///
/// **The causal reranker's split depends on the build.** llama.cpp added
/// that branch in b11223 ([`CAUSAL_RERANK_SPLIT_BUILD`]); on an older image
/// a causal reranker's input is evaluated in one ubatch like an encoder's,
/// and one past it gets the same 500. This answers for the current builds;
/// the plan's rationale and the knowledge-base limit name the build rather
/// than key on it, since lmgw does not gate on build numbers.
pub fn splits_input(kind: AuxKind, pooling: Option<&str>, causal: bool) -> bool {
    match kind {
        AuxKind::Embed => causal && pooling == Some("last"),
        AuxKind::Rerank | AuxKind::Decision => causal,
    }
}

/// The llama.cpp build that lets llama-server split a causal reranker's
/// input across ubatches: commit 4da6337 ("server : allow RANK pooling batch
/// splitting for causal LLM rerankers (ie. Qwen3 and Qwen3-VL)", #28876,
/// 2026-09-27), read from the build pool's history. b11192 (2026-09-26) has
/// `can_split = !need_embd || (memory && pooling == LAST)` and no rank
/// branch. Named in texts, never compared against ([`splits_input`]).
pub const CAUSAL_RERANK_SPLIT_BUILD: u32 = 11223;

/// llama.cpp's default `--batch-size` (`common_params::n_batch`): what a row
/// that sets no batch runs with. llama.cpp lowers the ubatch to it.
pub const LLAMA_DEFAULT_BATCH: i64 = 2048;

/// Where a count an aux row runs with comes from ([`ubatch_in_effect`],
/// [`batch_in_effect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountFrom {
    /// The row's own field.
    Field,
    /// A copy in its freeform args, which renders while the field is unset.
    Args,
}

/// The `--ubatch-size` `m` runs with and where it comes from (module doc);
/// `None` when neither the field nor the args name one, and llama-server's
/// default applies.
pub fn ubatch_in_effect(m: &AuxModel) -> Option<(i64, CountFrom)> {
    count_in_effect(m.ubatch_size, &m.args, "ubatch-size")
}

/// The `--batch-size` `m` runs with, by the same rule as
/// [`ubatch_in_effect`].
pub fn batch_in_effect(m: &AuxModel) -> Option<(i64, CountFrom)> {
    count_in_effect(m.batch_size, &m.args, "batch-size")
}

/// A count field, else the flag `key` in the freeform args when its value is
/// a number.
fn count_in_effect(field: Option<i64>, args: &[String], key: &str) -> Option<(i64, CountFrom)> {
    if let Some(n) = field {
        return Some((n, CountFrom::Field));
    }
    flag_value(args, key)
        .and_then(|v| v.trim().parse().ok())
        .map(|n| (n, CountFrom::Args))
}
