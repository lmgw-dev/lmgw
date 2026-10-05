//! Where a model's container computes: on the GPU, or on the CPU (the audio
//! class's per-row CPU switch).
//!
//! One predicate that every VRAM and GPU-hold decision reads. A `Cpu`
//! container charges nothing, is never an eviction victim and never evicts,
//! learns no residency, and keeps serving while the GPU hold is on — the
//! owner reads the hold as "lmgw may use zero VRAM", and a CPU container
//! uses none. A benchmark's lease still covers it: the run measures the
//! whole machine.
//!
//! Recorded on the registry entry when its container starts (or is
//! adopted), from the descriptor the container is rendered from: a row
//! flipped while its container runs keeps the old placement on the entry
//! until it is stopped. The entry is the truth for the running container,
//! the row ([`crate::config::Snapshot::placement`]) for the next start.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Every class and row but an audio row whose backend in effect is
    /// `cpu`.
    #[default]
    Gpu,
    Cpu,
}

impl Placement {
    /// The placement of an engine running on `backend` (audio.cpp's word),
    /// matched exactly as audio.cpp matches it: a padded `"cpu "` in
    /// `server.json` is no CPU backend there (it refuses to start), so it is
    /// none here either — the stricter answer, charged and held as a GPU
    /// container.
    pub fn of_backend(backend: &str) -> Self {
        match backend {
            "cpu" => Self::Cpu,
            _ => Self::Gpu,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }

    pub fn is_gpu(&self) -> bool {
        *self == Self::Gpu
    }
}
