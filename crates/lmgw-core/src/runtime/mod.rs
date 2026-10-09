//! Per-model container runtime: one container per model, and everything that
//! makes that true.
//!
//! See `docs/design/2026-08-30-per-model-containers-design.md`
//! §3.1 (class/engine/descriptor), §3.3 (naming/labels) and §3.6 (argv
//! rendering) for the design this module implements. It is what replaced
//! router mode outright (§7): the shared llama-server/audio.cpp containers,
//! their INI/JSON config renderers and their managers are gone, and every
//! local model — chat, aux, audio and (since the image-generation design)
//! image alike — is a container of its own, started from freshly rendered
//! argv and tracked in [`registry::Registry`].
//!
//! Module layout:
//! - [`argv`][]: [`argv::RenderSpec`]/[`argv::LlamaArgs`]/[`argv::EngineArgs`]
//!   and the two pure renderers — llama-server CLI args (or audio.cpp's fixed
//!   container command) for one model, and the full `podman run` argv for
//!   that model's container.
//! - [`audio`][]: per-model audio.cpp `server.json` rendering + the atomic
//!   config-dir writer (§3.6, "Audio config") — the audio-engine sibling of
//!   `argv`'s llama-flag rendering — and a row's engine settings and run
//!   args with its own backend and threads in effect (the CPU switch).
//! - [`Placement`]: whether a container computes on the GPU or the CPU,
//!   which every VRAM and GPU-hold decision reads.
//! - [`image`][]: the sd-server engine's own sibling — the capabilities body
//!   a started container answers with, and the two directories its argv
//!   points at (image-generation design §3).
//! - [`descriptor`]: [`descriptor::ModelRuntime`] — the per-model-vs-class
//!   override resolution (§3.1, §6) derived from a `Snapshot`, and the bridge
//!   from that descriptor to [`argv::RenderSpec`].
//! - [`lifecycle`]: when the registry's verbs run (§3.4, §3.7) — boot
//!   reconciliation and the legacy sweep, warm starts, the idle reaper,
//!   graceful shutdown, and the delete/disable/apply hygiene the ops surfaces
//!   call into.
//! - [`registry`]: [`registry::Registry`] and `acquire` (§3.2) — what lmgw
//!   believes is running, the one-start-per-model rule, and the RAII
//!   in-flight claim that closes the reap-between-check-and-forward race.
//! - This module: [`Class`] and the container naming/label scheme (§3.3).

pub mod argv;
pub mod audio;
pub mod descriptor;
pub mod image;
pub mod lifecycle;
mod placement;
pub mod registry;

pub use placement::Placement;

/// Which model table a runtime entry belongs to (§3.1).
///
/// The one class enum in the crate: the scheduler's own `vram::Container`
/// (`Chat | Aux | Audio`) was folded into this when admission moved onto the
/// registry (§3.2/§4), so a class means the same thing to the renderer, the
/// registry and the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Chat,
    Aux,
    Audio,
    /// stable-diffusion.cpp `sd-server` (image-generation design §3). Fourth
    /// and newest; everything the other three built — naming, labels,
    /// adoption, the hold sweep, the reaper — generalized to it without a new
    /// mechanism.
    Image,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Aux => "aux",
            Self::Audio => "audio",
            Self::Image => "image",
        }
    }

    /// Inverse of [`Self::as_str`]: read a class back out of a container's
    /// `lmgw.class` label (§3.3). `None` for anything this lmgw does not
    /// recognise — a label written by a newer version, or a hand-labelled
    /// container — which reconciliation (§3.4) treats as "not ours to adopt".
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(Self::Chat),
            "aux" => Some(Self::Aux),
            "audio" => Some(Self::Audio),
            "image" => Some(Self::Image),
            _ => None,
        }
    }

    /// The `lmgw.engine` label value (§3.3): `chat`/`aux` are both llama.cpp,
    /// `audio` runs audio.cpp and `image` runs stable-diffusion.cpp. Spelled
    /// `llama`/`audio`/`sdcpp` per §3.1's derivation table, not the
    /// model-file engine name — this is a label on the container, not the
    /// software's own identifier ([`crate::config::UpstreamKind::AudioCpp`]
    /// is a different concept with its own `audio_cpp` spelling; do not
    /// conflate the two).
    pub fn engine(self) -> &'static str {
        match self {
            Self::Chat | Self::Aux => "llama",
            Self::Audio => "audio",
            Self::Image => "sdcpp",
        }
    }
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lossy, filesystem/container-name-safe rendering of a model id (§3.3):
/// lowercase, `[a-z0-9]` runs kept, everything else collapsed to a single
/// `-`, edges trimmed, capped at 40 chars.
///
/// Lossy on purpose (`ORG/Model.Name` and `org-model-name` collide) — that is
/// exactly why [`container_name`] appends [`hash6`], which is computed over
/// the raw, un-slugged id and is what actually keeps distinct model ids
/// apart. `slug` only has to be readable, not unique.
pub fn slug(model_id: &str) -> String {
    let mut out = String::with_capacity(model_id.len());
    let mut last_was_dash = false;
    for c in model_id.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            // Non-ASCII letters/digits (accented, CJK, …) fall in here too:
            // `char::to_lowercase()` on them can yield further non-ASCII
            // output, which is no more container-name-safe than the input,
            // so they are collapsed rather than case-folded.
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    // Every character pushed above is single-byte ASCII, so byte slicing
    // cannot land inside a multi-byte codepoint.
    let capped = trimmed.get(..40).unwrap_or(trimmed);
    capped.trim_matches('-').to_string()
}

/// First 6 hex chars of a stable hash of the raw `model_id` (§3.3): the part
/// of [`container_name`] that makes it injective even where [`slug`] is
/// lossy. `sha2` is already a workspace dependency (`config::hash_api_key`,
/// `lmgw-update`); reused here rather than pulling in a lighter hash crate for
/// a property this size does not need.
pub fn hash6(model_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(model_id.as_bytes());
    hex::encode(&digest[..3])
}

/// Container name for one model (§3.3): `<prefix>-<class>-<slug>-<hash6>`.
///
/// `prefix` is the `container_prefix` setting (default `lmgw`; a dev
/// instance sets its own, which is what removes the dev/prod name-collision
/// class entirely — see the sibling memory note on this). `class` keeps a
/// chat and an aux model that happen to share a `model_id` (unique per table
/// only, not across tables) from colliding. `hash6` keeps two model ids that
/// slug identically (`"a/b"` and `"a-b"`) from colliding within the same
/// `(prefix, class)`.
pub fn container_name(prefix: &str, class: Class, model_id: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        class.as_str(),
        slug(model_id),
        hash6(model_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_basics() {
        assert_eq!(slug("Qwen3.6-35B-A3B"), "qwen3-6-35b-a3b");
        assert_eq!(slug("org/Model-Name"), "org-model-name");
        assert_eq!(slug("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn slug_collapses_unicode_to_dashes() {
        // Non-ASCII input has nothing ASCII-alphanumeric to keep, so it
        // collapses to a run of dashes and then trims to empty — the
        // property that matters is that this does not panic and stays
        // container-name-safe, not that it is informative.
        assert_eq!(slug("日本語モデル"), "");
        assert_eq!(slug("café"), "caf");
    }

    #[test]
    fn slug_caps_length_without_trailing_dash() {
        let long = "a".repeat(50);
        assert_eq!(slug(&long).len(), 40);

        // A cut that would land right after a run of separators must not
        // leave a trailing dash behind.
        let boundary = format!("{}-{}", "a".repeat(39), "bbbb");
        let slugged = slug(&boundary);
        assert!(slugged.len() <= 40);
        assert!(!slugged.ends_with('-'), "{slugged:?}");
    }

    #[test]
    fn container_name_differs_by_class_for_the_same_model_id() {
        let chat = container_name("lmgw", Class::Chat, "shared-id");
        let aux = container_name("lmgw", Class::Aux, "shared-id");
        let audio = container_name("lmgw", Class::Audio, "shared-id");
        let image = container_name("lmgw", Class::Image, "shared-id");
        assert_ne!(chat, aux);
        assert_ne!(chat, audio);
        assert_ne!(aux, audio);
        assert_ne!(image, chat);
        assert_ne!(image, aux);
        assert_ne!(image, audio);
    }

    #[test]
    fn container_name_stays_distinct_for_adversarial_slug_collisions() {
        // Each pair slugs identically (or near-identically); hash6 is what
        // has to carry the distinction.
        let pairs = [
            ("a/b", "a-b"),
            ("Model.Name", "model-name"),
            ("qwen3.6-35b", "qwen3-6-35b"),
            ("日本語", "!!!"),
            ("foo--bar", "foo-bar"),
        ];
        for (a, b) in pairs {
            assert_ne!(
                container_name("lmgw", Class::Chat, a),
                container_name("lmgw", Class::Chat, b),
                "collision for {a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn engine_label_matches_class() {
        assert_eq!(Class::Chat.engine(), "llama");
        assert_eq!(Class::Aux.engine(), "llama");
        assert_eq!(Class::Audio.engine(), "audio");
        assert_eq!(Class::Image.engine(), "sdcpp");
    }

    #[test]
    fn every_class_round_trips_through_its_label() {
        for class in [Class::Chat, Class::Aux, Class::Audio, Class::Image] {
            assert_eq!(Class::parse(class.as_str()), Some(class));
        }
    }
}
