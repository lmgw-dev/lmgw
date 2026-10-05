//! Acquire inputs

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::runtime::descriptor::ModelRuntime;

/// Everything one [`Registry::acquire`](crate::runtime::registry::Registry::acquire) needs that the registry does not
/// already own.
///
/// `class` and `model_id` — the registry key (§3.2) — come from `runtime`,
/// which is the same descriptor the start sequence renders argv from; passing
/// them separately would only create a way for the key and the thing started
/// under it to disagree. The registry deliberately holds no `Snapshot`: the
/// caller resolves the descriptor and the class-dependent placement (which
/// `models_dir`, which config mount) and hands over plain values, exactly the
/// way [`crate::runtime::argv`] is decoupled from `Settings`.
pub struct AcquireSpec<'a> {
    pub runtime: &'a ModelRuntime,
    /// The `container_prefix` setting.
    pub container_prefix: &'a str,
    /// The owning class's models dir, mounted read-only at `/models`.
    pub models_dir: &'a str,
    /// lmgw's own data directory (`state.rs`'s `data_dir`). Threaded straight
    /// through to [`ModelRuntime::render_spec`], which is the thing that
    /// actually needs it — only audio does (§3.6's per-model config dir);
    /// chat/aux ignore it. Replaces what used to be a caller-resolved
    /// `config_mount: Option<PathBuf>` here: now that the config-dir scheme
    /// is fixed (`audio::config_dir`), there is nothing left for a caller to
    /// resolve, only a root directory to hand over.
    pub data_dir: &'a Path,
    /// Whether the start may create what its argv points at inside
    /// `models_dir` (the image class's LoRA and upscaler dirs). `false` on a
    /// dev instance whose models dir lies outside its data dir
    /// ([`crate::config::dev_models_dir_refusal`]): the start goes ahead and
    /// creates nothing there.
    pub may_write_models_dir: bool,
    /// `vram.load_timeout_seconds`. Bounds the readiness poll, nothing else.
    pub load_timeout: Duration,
    /// `vram.unload_timeout_seconds`. Recorded on the entry and used by a
    /// later [`Registry::stop`](crate::runtime::registry::Registry::stop); see [`Entry::stop_timeout`](crate::runtime::registry::Entry::stop_timeout).
    pub stop_timeout: Duration,
}

/// [`AcquireSpec`], owned — what a climb's start takes into the task it is
/// spawned in ([`ClimbTicket::start`](crate::runtime::registry::ClimbTicket::start)), which outlives the request that
/// triggered it (ladder design §12 entry 21).
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub runtime: ModelRuntime,
    pub container_prefix: String,
    pub models_dir: String,
    pub data_dir: PathBuf,
    pub may_write_models_dir: bool,
    pub load_timeout: Duration,
    pub stop_timeout: Duration,
}

impl StartSpec {
    pub fn of(spec: &AcquireSpec<'_>) -> Self {
        Self {
            runtime: spec.runtime.clone(),
            container_prefix: spec.container_prefix.to_string(),
            models_dir: spec.models_dir.to_string(),
            data_dir: spec.data_dir.to_path_buf(),
            may_write_models_dir: spec.may_write_models_dir,
            load_timeout: spec.load_timeout,
            stop_timeout: spec.stop_timeout,
        }
    }

    pub fn as_spec(&self) -> AcquireSpec<'_> {
        AcquireSpec {
            runtime: &self.runtime,
            container_prefix: &self.container_prefix,
            models_dir: &self.models_dir,
            data_dir: &self.data_dir,
            may_write_models_dir: self.may_write_models_dir,
            load_timeout: self.load_timeout,
            stop_timeout: self.stop_timeout,
        }
    }
}
