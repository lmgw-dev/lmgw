//! Per-model runtime descriptor derivation (design §3.1) — additive, not yet
//! wired into anything live.
//!
//! `ModelRuntime` is what a later work package's `acquire`/reconciler will
//! read instead of walking `LocalModel`/`AuxModel`/`AudioModel` and the three
//! class-settings sections separately; this module only computes it from a
//! [`Snapshot`], resolving each per-model image/extra_run_args override
//! against its owning class's default (§6). Derived fresh on every call
//! rather than cached, the same way every other config view in this crate
//! is: cheap to recompute, never out of sync with a reload.

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use crate::config::{AudioModel, AudioSettings, AuxModel, ImageModel, LocalModel, Snapshot};
use crate::sdcpp_caps::SdcppCaps;

use super::argv::{EngineArgs, ImageArgs, LlamaArgs, RenderSpec};
use super::{audio as audio_cfg, container_name, image as image_cfg, Class, Placement};

/// The binary inside the stable-diffusion.cpp image (§2.5): its `ENTRYPOINT`
/// is `/sd-cli`, so the server has to be named explicitly.
pub const SD_SERVER_ENTRYPOINT: &str = "/sd-server";

/// sd-server's readiness route (§3): `GET /sdcpp/v1/capabilities`, which
/// answers only once `new_sd_ctx` exists — and which doubles as the
/// capabilities probe, so readiness and "what is this pipeline" are one
/// request.
pub const IMAGE_HEALTH_PATH: &str = "/sdcpp/v1/capabilities";

/// One model's resolved runtime inputs (§3.1): image/extra_run_args with the
/// per-model-vs-class-settings override already resolved, plus everything a
/// later `start(model)` needs to render argv and track idle/warm-start state.
#[derive(Debug, Clone)]
pub struct ModelRuntime {
    pub class: Class,
    /// Client-facing model id (`LocalModel::model_id` / `AuxModel::model_id`
    /// / `AudioModel::model_id`) — what `--alias` and the container name are
    /// built from.
    pub model_id: String,
    /// Resolved image: the per-model override, or the owning class's image.
    pub image: String,
    /// Resolved extra `podman run` args: the per-model override, or the
    /// owning class's `extra_run_args`.
    pub extra_run_args: Vec<String>,
    /// `0` = never idle-stop. Audio has no per-model idle column yet — its
    /// idle unload (§3.7) is new behavior this design introduces, and wiring
    /// a column for it is a later work package — so an audio runtime always
    /// reads `0` here.
    pub idle_seconds: i64,
    pub warm_start: bool,
    pub enabled: bool,
    /// llama-engine argv inputs (chat/aux). `None` for audio, which has no
    /// CLI-flag model at all — see [`Self::audio`].
    pub llama: Option<LlamaArgs>,
    /// The full audio.cpp model row, carried so the per-model `server.json`
    /// renderer (§3.6) can render its one entry without a second lookup.
    /// `None` for chat/aux.
    pub audio: Option<AudioModel>,
    /// `AudioSettings`' engine fields (`backend`/`device`/`threads`/
    /// `lazy_load`, §3.6) that flow into this model's own `server.json`,
    /// with the row's own `backend` and `threads` in effect
    /// ([`audio_cfg::engine_settings`], the CPU switch) — the class settings
    /// unchanged for a row that sets neither. Every render of the row's
    /// `server.json` reads this, never the class settings: the start, boot
    /// adoption's compare, `local_model_get`. `None` for chat/aux.
    pub audio_settings: Option<AudioSettings>,
    /// The full sd-server row, carried so the argv renderer can read its two
    /// JSON maps without a second lookup. `None` for every other class.
    ///
    /// No class-settings twin beside it, unlike audio: sd-server has no
    /// config file, and everything `ImageSettings` holds beyond
    /// `image`/`extra_run_args` (the models dir, the public prefix) is
    /// resolved by the caller into `AcquireSpec`/exposure rather than into
    /// argv.
    pub image_model: Option<ImageModel>,
    /// The flag vocabulary this model's argv is spelled against
    /// (image-generation design §2.6).
    ///
    /// `None` means "use the build lmgw shipped with"
    /// ([`SdcppCaps::embedded`]), which is what every caller that has not
    /// probed the image uses — a dashboard preview, a pre-flight, boot
    /// adoption on a box whose GPU is busy. The registry fills it from a
    /// cached `sd-server --help` of *this model's* image before a start, so a
    /// row using a flag newer than lmgw still renders (§3).
    pub sdcpp_caps: Option<Arc<SdcppCaps>>,
    /// Which rung of a ladder row this descriptor renders (ladder design
    /// §4.1, §5) — `Some` on every ladder row, the base included (index 0
    /// from [`model_runtime`]), `None` on every other row and class.
    ///
    /// Set here, where `-m` and `--ctx-size` are chosen, and read by
    /// everything that has to know what a container runs — the gate's
    /// facts, the ledger's charge, adoption — so none of them re-derives it
    /// from a row that may have been edited since.
    pub rung: Option<RungPos>,
}

/// A rung's place in its ladder: `index` is 0-based (0 is the base, ladder
/// design §12 entry 11), `of` the number of rungs, base included. Every
/// outside surface prints `index + 1` / `of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RungPos {
    pub index: usize,
    pub of: usize,
}

/// What the VRAM ledger charges for a container started at a rung (ladder
/// design §5): its weights file and its `--ctx-size`, next to its place in the
/// ladder. Recorded on the registry entry when the start is claimed, from the
/// descriptor the container is rendered from, so a climbed model is charged
/// for the rung it runs — never for the row's base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RungCharge {
    pub index: usize,
    pub of: usize,
    /// Relative to the chat models dir, like `LocalModel::gguf_path`.
    pub gguf_path: String,
    pub ctx_size: Option<i64>,
}

impl RungCharge {
    /// The weights file's name, for a surface that shows which file runs.
    pub fn gguf_file(&self) -> &str {
        file_name(&self.gguf_path)
    }
}

/// The last component of a models-dir relative path.
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl ModelRuntime {
    /// Where a container started from this descriptor computes: `Cpu` for an
    /// audio row whose backend in effect is `cpu`, `Gpu` for every other row
    /// and class.
    pub fn placement(&self) -> Placement {
        self.audio_settings
            .as_ref()
            .map_or(Placement::Gpu, |s| Placement::of_backend(&s.backend))
    }

    /// The configuration an audio container started from this descriptor
    /// runs, as the residency it teaches is keyed
    /// ([`crate::vram::residency::resident_key`]) — `None` for every other
    /// class.
    pub fn resident_key(&self) -> Option<String> {
        let (m, s) = self.audio.as_ref().zip(self.audio_settings.as_ref())?;
        Some(crate::vram::residency::resident_key(m, s))
    }

    /// The ledger's charge for a start from this descriptor — `None` unless it
    /// renders a ladder rung ([`Self::rung`]).
    pub fn rung_charge(&self) -> Option<RungCharge> {
        let pos = self.rung?;
        let Some(LlamaArgs::Chat {
            gguf_path, params, ..
        }) = &self.llama
        else {
            return None;
        };
        Some(RungCharge {
            index: pos.index,
            of: pos.of,
            gguf_path: gguf_path.clone(),
            ctx_size: params.ctx_size,
        })
    }

    /// The full `podman run` argv inputs for this model (§3.6), given the
    /// caller-resolved container placement.
    ///
    /// `data_dir` is lmgw's own data directory (`state.rs`'s `data_dir`, the
    /// parent of `audiocpp/`, `llama-server/`, `lmgw.sqlite`, …) — unused for
    /// chat/aux, and for audio it is where [`audio_cfg::config_dir`] derives
    /// this model's own config dir and where its `server.json` gets written
    /// (§3.6): rendering that per-model config is no longer a later work
    /// package, it happens right here, synchronously, before the spec comes
    /// back. That makes this method fallible where it used to be infallible
    /// (`Option`) — the only failure mode is the config write itself.
    pub fn render_spec(
        &self,
        container_prefix: &str,
        host_port: u16,
        models_dir: &str,
        data_dir: &Path,
    ) -> std::io::Result<RenderSpec> {
        self.render_spec_inner(container_prefix, host_port, models_dir, data_dir, true)
    }

    /// [`Self::render_spec`] for a start that may not write into
    /// `models_dir` (`AcquireSpec::may_write_models_dir`, a dev instance on a
    /// models dir outside its data dir): the image class's LoRA and upscaler
    /// dirs are not created, and the start goes ahead with a log line
    /// ([`image_cfg::note_dirs_not_created`]). Audio's `server.json` lives
    /// under the data dir and is written as on every start.
    pub fn render_spec_sparing_models_dir(
        &self,
        container_prefix: &str,
        host_port: u16,
        models_dir: &str,
        data_dir: &Path,
    ) -> std::io::Result<RenderSpec> {
        if let Some(model) = &self.image_model {
            image_cfg::note_dirs_not_created(models_dir, model);
        }
        self.render_spec_inner(container_prefix, host_port, models_dir, data_dir, false)
    }

    /// [`Self::render_spec`] without the filesystem half — what a **read**
    /// verb renders with.
    ///
    /// `ops::command_line_preview` and `image_model_get` answer "what would
    /// this model be started with", and a question must not create
    /// directories to answer itself: a models dir that is read-only, on a
    /// missing mount, or not configured at all would otherwise replace the
    /// command line with an io error, which is the one thing the caller did
    /// not ask about. The start path keeps creating them, where a failure
    /// belongs.
    pub fn preview_spec(
        &self,
        container_prefix: &str,
        host_port: u16,
        models_dir: &str,
        data_dir: &Path,
    ) -> std::io::Result<RenderSpec> {
        self.render_spec_inner(container_prefix, host_port, models_dir, data_dir, false)
    }

    fn render_spec_inner(
        &self,
        container_prefix: &str,
        host_port: u16,
        models_dir: &str,
        data_dir: &Path,
        create_dirs: bool,
    ) -> std::io::Result<RenderSpec> {
        if let Some(model) = &self.image_model {
            // The LoRA/upscaler directories the unconditional flags point at
            // (§12.2) — created here, where audio writes its `server.json`,
            // because both are "the filesystem this container needs before it
            // can answer" and both must fail the start rather than the
            // request.
            if create_dirs {
                image_cfg::ensure_dirs(models_dir, model)?;
            }
            return Ok(RenderSpec {
                class: self.class,
                model_id: self.model_id.clone(),
                image: self.image.clone(),
                container_name: container_name(container_prefix, self.class, &self.model_id),
                container_prefix: container_prefix.to_string(),
                host_port,
                models_dir: models_dir.to_string(),
                config_mount: None,
                extra_run_args: self.extra_run_args.clone(),
                init: true,
                entrypoint: Some(SD_SERVER_ENTRYPOINT.to_string()),
                engine: EngineArgs::Image(ImageArgs {
                    files: model.files.clone(),
                    args: model.args.clone(),
                    caps: self.sdcpp_caps.clone().unwrap_or_else(SdcppCaps::embedded),
                }),
                health_path: IMAGE_HEALTH_PATH,
            });
        }
        let (engine, config_mount, health_path) = match (&self.llama, &self.audio) {
            (Some(llama), None) => (EngineArgs::Llama(llama.clone()), None, "/health"),
            (None, Some(model)) => {
                let settings = self.audio_settings.as_ref().expect(
                    "an audio ModelRuntime always carries its class settings, see audio_runtime",
                );
                let dir = audio_cfg::config_dir(data_dir, &self.model_id);
                let json = audio_cfg::render_single_model_config(settings, model);
                audio_cfg::write_config(&dir, &json)?;
                // audio.cpp has no `/health` route (§3.6/§10.3 measured only
                // llama-server's); `GET /v1/models` is what it does answer —
                // the same path `catalog::fetch_models` already polls for the
                // shared router-mode upstream today.
                (EngineArgs::Audio, Some(dir), "/v1/models")
            }
            (llama, audio) => unreachable!(
                "a ModelRuntime must carry exactly one of llama/audio/image (llama={}, \
                 audio={}, image=false)",
                llama.is_some(),
                audio.is_some()
            ),
        };
        Ok(RenderSpec {
            class: self.class,
            model_id: self.model_id.clone(),
            image: self.image.clone(),
            container_name: container_name(container_prefix, self.class, &self.model_id),
            container_prefix: container_prefix.to_string(),
            host_port,
            models_dir: models_dir.to_string(),
            config_mount,
            extra_run_args: self.extra_run_args.clone(),
            // Both are the image class's business alone: llama-server and
            // audiocpp_server handle SIGTERM themselves and their images'
            // entrypoints are the servers.
            init: false,
            entrypoint: None,
            engine,
            health_path,
        })
    }
}

/// Every configured model's runtime descriptor: chat, then aux, then audio,
/// then image — the same class order [`crate::ops::models`] lists them in.
pub fn model_runtimes(snap: &Snapshot) -> Vec<ModelRuntime> {
    let mut out = Vec::with_capacity(
        snap.local_models.len()
            + snap.aux_models.len()
            + snap.audio_models.len()
            + snap.image_models.len(),
    );
    out.extend(snap.local_models.iter().map(|m| chat_runtime(snap, m, 0)));
    out.extend(snap.aux_models.iter().map(|m| aux_runtime(snap, m)));
    out.extend(snap.audio_models.iter().map(|m| audio_runtime(snap, m)));
    out.extend(snap.image_models.iter().map(|m| image_runtime(snap, m)));
    out
}

/// One model's descriptor by class + client-facing id, or `None` if no such
/// model is configured. Always rung 0 (the base) — see [`model_runtime_at`]
/// for a chosen rung.
pub fn model_runtime(snap: &Snapshot, class: Class, model_id: &str) -> Option<ModelRuntime> {
    match class {
        Class::Chat => snap
            .local_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| chat_runtime(snap, m, 0)),
        Class::Aux => snap
            .aux_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| aux_runtime(snap, m)),
        Class::Audio => snap
            .audio_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| audio_runtime(snap, m)),
        Class::Image => snap
            .image_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| image_runtime(snap, m)),
    }
}

/// [`model_runtime`], for a chosen rung (ladder design §4.1, §8 WP1).
/// 0-indexed like every other rung number in code (design §12 entry 11 —
/// only external surfaces are 1-based); `rung` beyond
/// [`crate::config::LocalModel::top_rung`] falls back to the base rather than
/// panicking on a stale caller, the same defensive stance
/// [`chat_runtime`] takes.
///
/// Chat only: every other class ignores `rung` and behaves exactly like
/// [`model_runtime`] — ladders are out of scope for aux/audio/image (design
/// §11). This is additive; WP2 is what actually wires a climbed rung into a
/// start, this only provides the function.
pub fn model_runtime_at(
    snap: &Snapshot,
    class: Class,
    model_id: &str,
    rung: usize,
) -> Option<ModelRuntime> {
    match class {
        Class::Chat => snap
            .local_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| chat_runtime(snap, m, rung)),
        _ => model_runtime(snap, class, model_id),
    }
}

/// `rung = 0` renders byte-identical argv to before this parameter existed
/// (the base: `m.gguf_path` / `m.params` unmodified). `rung > 0` overrides
/// only `-m` (the rung's own gguf) and `--ctx-size` (design §4.1) — every
/// other field of `params`, and `args` verbatim, stay the row's, so a
/// freeform `--ctx-size`/`-c` in `args` is still deduped away by the typed
/// field exactly as it is on the base (`render_llama_args`'s `taken` set).
/// Out-of-range falls back to the base rather than panicking: a stale caller
/// asking for a rung a row no longer has should get *a* command line, not a
/// crash — `model_runtime_at`'s caller is expected to have checked
/// [`crate::config::LocalModel::top_rung`] already.
fn chat_runtime(snap: &Snapshot, m: &LocalModel, rung: usize) -> ModelRuntime {
    let r = &snap.settings.router;
    let (gguf_path, params, index) = match rung.checked_sub(1).and_then(|i| m.ladder.get(i)) {
        Some(higher) => {
            let mut params = m.params.clone();
            params.ctx_size = Some(higher.ctx_size);
            (higher.gguf_path.clone(), params, rung)
        }
        None => (m.gguf_path.clone(), m.params.clone(), 0),
    };
    ModelRuntime {
        class: Class::Chat,
        model_id: m.model_id.clone(),
        image: m.image.clone().unwrap_or_else(|| r.image.clone()),
        extra_run_args: m
            .extra_run_args
            .clone()
            .unwrap_or_else(|| r.extra_run_args.clone()),
        idle_seconds: m.idle_seconds,
        warm_start: m.warm_start,
        enabled: m.enabled,
        llama: Some(LlamaArgs::Chat {
            gguf_path,
            params: Box::new(params),
            args: m.args.clone(),
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: m.is_ladder().then_some(RungPos {
            index,
            of: m.top_rung() + 1,
        }),
    }
}

/// The higher rungs (1..=top) of every ladder row, as descriptors — what boot
/// reconciliation compares a running container against besides each row's
/// base, so a container a previous lmgw had climbed is adopted at the rung it
/// runs rather than removed (ladder design §3.1: a crash is not a container
/// stop, and the rung's load is already on the card). Never a start: every
/// start of a model renders [`model_runtime`], the base (§3.5).
pub fn higher_rungs(snap: &Snapshot) -> Vec<ModelRuntime> {
    snap.local_models
        .iter()
        .flat_map(|m| (1..=m.top_rung()).map(move |k| chat_runtime(snap, m, k)))
        .collect()
}

fn aux_runtime(snap: &Snapshot, m: &AuxModel) -> ModelRuntime {
    let r = &snap.settings.aux_router;
    ModelRuntime {
        class: Class::Aux,
        model_id: m.model_id.clone(),
        image: m.image.clone().unwrap_or_else(|| r.image.clone()),
        extra_run_args: m
            .extra_run_args
            .clone()
            .unwrap_or_else(|| r.extra_run_args.clone()),
        idle_seconds: m.idle_seconds,
        warm_start: m.warm_start,
        enabled: m.enabled,
        llama: Some(LlamaArgs::Aux {
            gguf_path: m.gguf_path.clone(),
            kind: m.kind,
            pooling: m.pooling.clone(),
            ctx_size: m.ctx_size,
            args: m.args.clone(),
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

fn audio_runtime(snap: &Snapshot, m: &AudioModel) -> ModelRuntime {
    let a = &snap.settings.audio;
    ModelRuntime {
        class: Class::Audio,
        model_id: m.model_id.clone(),
        image: m.image.clone().unwrap_or_else(|| a.image.clone()),
        // A CPU row inheriting the class's args runs without its GPU
        // passthrough (`audio_cfg::run_args`).
        extra_run_args: audio_cfg::run_args(m, a),
        // No per-model idle column yet — see the field doc on `idle_seconds`.
        idle_seconds: 0,
        warm_start: m.warm_start,
        enabled: m.enabled,
        llama: None,
        audio: Some(m.clone()),
        audio_settings: Some(audio_cfg::engine_settings(m, a, crate::host::cpu())),
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

/// The image class's descriptor (image-generation design §3, §4).
///
/// Two differences from [`audio_runtime`], both deliberate: `idle_seconds` is
/// the row's own column rather than a hardcoded 0 (a loaded pipeline holds
/// 7–13 GiB, so idle unload matters more here than anywhere), and no class
/// settings are captured beside the row — sd-server has no `server.json`, so
/// there is nothing per-class that reaches argv.
fn image_runtime(snap: &Snapshot, m: &ImageModel) -> ModelRuntime {
    let i = &snap.settings.image;
    ModelRuntime {
        class: Class::Image,
        model_id: m.model_id.clone(),
        image: m.image.clone().unwrap_or_else(|| i.image.clone()),
        extra_run_args: m
            .extra_run_args
            .clone()
            .unwrap_or_else(|| i.extra_run_args.clone()),
        idle_seconds: m.idle_seconds,
        warm_start: m.warm_start,
        enabled: m.enabled,
        llama: None,
        audio: None,
        audio_settings: None,
        image_model: Some(m.clone()),
        // Filled by the registry from a probe of this model's image just
        // before a start; everything else renders against the embedded
        // vocabulary.
        sdcpp_caps: None,
        rung: None,
    }
}
