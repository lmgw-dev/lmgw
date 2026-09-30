//! llama-server CLI argv + `podman run` argv for one per-model container
//! (design §3.6).
//!
//! This is what replaced the INI world (§7's deleted `render_preset` /
//! `render_aux_preset`): each model's own container gets direct-mode CLI
//! args, following the design's list of CLI-specific differences from the
//! preset the router used to read:
//!
//! - Boolean flags are bare switches (`--jinja`), never `--jinja true` — a
//!   preset line always read `key = true`, a CLI flag is just present.
//! - A `false` on a default-on tri-state renders the `--no-*` twin
//!   (`reasoning_preserve`).
//! - Value-taking options keep `--key value` as two tokens.
//! - Model-relative paths (`gguf_path`, `mmproj`, `model-draft`) rewrite onto
//!   the `/models` mount.
//! - Direct mode has no router to name the model after the request, so
//!   `-m`/`--alias` are always emitted, not left to the caller's freeform
//!   args.
//! - `--embeddings`/`--reranking` replace the preset's `embedding =
//!   true`/`reranking = true` keys; rerankers still never get a `--pooling`
//!   flag (a llama-server quirk that outlived the preset renderer).
//! - No `--host`/`--port`/preset flags: the container always listens on
//!   `0.0.0.0:8080` internally (the port published on the host's loopback
//!   maps to it), and
//!   `idle_seconds` is lmgw-side now, so no sleep-related flag is rendered.
//!
//! The canonical-key dedup table (`SHORT_ALIASES`/`canonical_key` below) is
//! the surviving copy of the one the preset renderer carried (§7: "the
//! canonical-key dedup table survives in the argv renderer").

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{AuxKind, LlamaParams};
use crate::sdcpp_caps::SdcppCaps;

use super::Class;

/// llama-server's port inside the container: fixed, because the published
/// host port (§3.5) is what varies per model.
const CONTAINER_PORT: u16 = 8080;

/// Short llama-server aliases for flags that have a dedicated field, mapped
/// to the long name the structured renderer emits — the dedup table §7 kept
/// when it deleted the preset renderer.
const SHORT_ALIASES: &[(&str, &str)] = &[
    ("c", "ctx-size"),
    ("n", "n-predict"),
    ("predict", "n-predict"),
    ("ngl", "n-gpu-layers"),
    ("t", "threads"),
    ("b", "batch-size"),
    ("ub", "ubatch-size"),
    ("np", "parallel"),
    ("kvu", "kv-unified"),
    ("no-kvu", "no-kv-unified"),
    ("fa", "flash-attn"),
    ("ctk", "cache-type-k"),
    ("ctv", "cache-type-v"),
    ("cram", "cache-ram"),
    ("m", "model"),
    ("mm", "mmproj"),
    ("md", "model-draft"),
    ("ngld", "spec-draft-ngl"),
    ("rea", "reasoning"),
    ("rerank", "reranking"),
    ("temperature", "temp"),
    ("s", "seed"),
];

/// Dedups a short-spelled flag against a typed field's long name when
/// rendering — `config::hoist_promoted_args_into` deliberately does **not**
/// reuse this (review finding X2): this table maps every alias, including
/// several (`-ub`, `-n`/`-predict`, `-cram`, `-mm`, `-rea`, …) with no
/// promoted twin, and running the hoist through it silently promoted those
/// too on rows that never touched the feature that needed hoisting. It keeps
/// its own narrow `hoist_canonical_key` for the one pair (`-kvu`, `-no-kvu`)
/// that must hoist.
fn canonical_key(key: &str) -> &str {
    SHORT_ALIASES
        .iter()
        .find(|(short, _)| *short == key)
        .map(|(_, long)| *long)
        .unwrap_or(key)
}

/// llama-engine argv inputs for one model, split the same way the preset
/// renderer splits `LocalModel`/`AuxModel`: a chat model carries the full
/// [`LlamaParams`] field set, an aux model the slimmer embed/rerank one
/// (neither uses chat sampling/jinja/speculative-decode params).
#[derive(Debug, Clone, PartialEq)]
pub enum LlamaArgs {
    Chat {
        /// Relative to the models dir, like `LocalModel::gguf_path`.
        gguf_path: String,
        params: LlamaParams,
        /// Freeform flags with no dedicated field; deduped against `params`
        /// and the always-emitted keys (§3.6).
        args: Vec<String>,
    },
    Aux {
        gguf_path: String,
        kind: AuxKind,
        /// Never rendered for [`AuxKind::Rerank`] regardless of this value —
        /// see [`push_aux_flags`].
        pooling: Option<String>,
        ctx_size: Option<i64>,
        args: Vec<String>,
    },
}

fn gguf_path(llama: &LlamaArgs) -> &str {
    match llama {
        LlamaArgs::Chat { gguf_path, .. } | LlamaArgs::Aux { gguf_path, .. } => gguf_path,
    }
}

/// Which engine one model's container runs (§3.1, §3.6): llama.cpp
/// (chat/aux, argv-driven) or audio.cpp (config-file-driven — the model
/// selection lives in the mounted `server.json`, not in a flag).
///
/// The enum WP1's module doc promised ("RenderSpec/argv.rs may grow an enum
/// ... for llama args vs audio args"): [`RenderSpec`] used to carry a bare
/// [`LlamaArgs`] and audio simply never produced a [`RenderSpec`] at all
/// ([`crate::runtime::descriptor::ModelRuntime::render_spec`] returned
/// `None`). Now every class renders one, so the field has to say which shape
/// it is.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineArgs {
    Llama(LlamaArgs),
    /// audio.cpp's container command is the same regardless of which model
    /// is running (`server --config /config/<file>`, [`audio_container_args`]
    /// mirrors `audio::podman_run_args`'s tail exactly) — the per-model
    /// selection is entirely in [`RenderSpec::config_mount`]'s `server.json`.
    Audio,
    /// sd-server: argv again, but from two JSON maps rather than a struct
    /// (image-generation design §3, §4) — and with the flag *spellings* taken
    /// from the image's own `--help`, because sd-server mixes separators
    /// (`--clip_l`, `--diffusion-model`) in a way no substitution rule
    /// reproduces.
    Image(ImageArgs),
}

/// One image row's argv inputs, plus the vocabulary its keys are spelled
/// against.
///
/// The vocabulary is **carried, not fetched**: [`render_image_args`] has to
/// stay as pure as its llama sibling (the adoption path re-renders argv to
/// compare it against a running container, and a renderer that shelled out to
/// podman for a help text mid-comparison would be a different function every
/// time it ran). The caller resolves it — the registry from a cached probe of
/// the model's own image, everything else from
/// [`SdcppCaps::embedded`] — exactly as it resolves `image`/`extra_run_args`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageArgs {
    /// Flag key → path relative to the models dir. Rewritten onto `/models`.
    pub files: serde_json::Map<String, serde_json::Value>,
    /// Flag key → value. `true` renders as a bare switch, `false`/`null` as
    /// nothing at all, an array as the flag repeated once per element.
    pub args: serde_json::Map<String, serde_json::Value>,
    pub caps: Arc<SdcppCaps>,
}

/// Caller-resolved inputs to render one model's container (§3.6). Decoupled
/// from the config tables on purpose: later work packages resolve per-model
/// overrides (image, extra run args, warm start, …) against class settings
/// and hand the result here as plain values, so this renderer never needs to
/// know about `Settings`/`Snapshot` at all.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSpec {
    pub class: Class,
    /// Client-facing model id: what a request's `"model"` field names, and
    /// what `--alias` makes llama-server answer to.
    pub model_id: String,
    /// Image to run (per-model override or class default — resolved by the
    /// caller).
    pub image: String,
    /// Full container name, e.g. from [`super::container_name`].
    pub container_name: String,
    /// The `container_prefix` setting — the `lmgw.instance` label value.
    /// Kept alongside `container_name` (rather than parsed back out of it) so
    /// reconciliation (§3.4) filters on the label, never on name patterns.
    pub container_prefix: String,
    /// Host port to publish, mapped to the container's internal 8080.
    pub host_port: u16,
    /// Host directory mounted read-only at `/models`.
    pub models_dir: String,
    /// Host directory mounted read-only at `/config`; `None` for llama-engine
    /// models (everything is a CLI flag, nothing is a mounted config file).
    /// Audio's per-model `server.json` dir (§3.6) is what sets this.
    pub config_mount: Option<PathBuf>,
    /// Extra `podman run` flags spliced in before the port/mount/image
    /// arguments — GPU/CDI flags, per-model `-e CUDA_VISIBLE_DEVICES`,
    /// memory limits (§3.1). Resolved by the caller; this renderer only
    /// splices what it is given, it does not know the default.
    pub extra_run_args: Vec<String>,
    /// Run the container under podman's init process (`--init`).
    ///
    /// A field rather than a class check inside [`podman_run_argv`], because
    /// it is a property of the *process*, not of the table the model came
    /// from: sd-server installs no SIGTERM handler, so as PID 1 it ignores
    /// `podman stop` until the grace runs out and SIGKILL lands — 10 s of
    /// held VRAM per stop, measured (image-generation design §12.5). With
    /// `--init` the signal reaches it and it exits in ~0.3 s, idle or
    /// mid-generation. Any future engine with the same deafness sets this
    /// too.
    pub init: bool,
    /// Override the image's `ENTRYPOINT` (`--entrypoint <bin>`), rendered
    /// immediately before the image name.
    ///
    /// `None` for images whose own entrypoint is the server. The sd.cpp image
    /// ships `/sd-cli` as its entrypoint and the server as a second binary
    /// (§2.5), so the image class always sets `/sd-server` here.
    pub entrypoint: Option<String>,
    /// Which engine this model runs and its argv/config inputs (§3.1, §3.6):
    /// llama-server CLI flags for chat/aux, or the fixed audio.cpp container
    /// command for audio (audio's per-model selection happens entirely
    /// through `config_mount`, never through a flag — see [`EngineArgs`]).
    pub engine: EngineArgs,
    /// GET path polled until it answers 200 to mark the container ready
    /// (§3.6, §10.3/§10.7): llama-server's `/health` for chat/aux. audio.cpp
    /// has no `/health` route; `GET /v1/models` is what it does answer (the
    /// same path `catalog::fetch_models` polls), so that is the audio probe.
    /// Set by the
    /// caller (`descriptor::render_spec`, from `class`) — the registry never
    /// guesses at it.
    pub health_path: &'static str,
}

// ---------------------------------------------------------------------------
// Freeform argument text (the extra-args textarea and its tool-plane twin)
// ---------------------------------------------------------------------------

/// True for tokens that start an option (`--ctx-size`, `-ngl`) as opposed to
/// values; negative numbers (`-1`) are values.
///
/// Lives here rather than in the deleted `router` module because every reader
/// of it — this renderer, the extra-args round trip below, `config`'s
/// duplicate-flag detection — is about argv, not about the INI it used to
/// serve.
pub fn is_opt(tok: &str) -> bool {
    tok.starts_with('-') && tok.parse::<f64>().is_err()
}

/// Parse the freeform extra-args textarea (shell quoting honored, newlines
/// are whitespace) into the stored token list.
pub fn parse_args_text(text: &str) -> Result<Vec<String>, String> {
    // HTML textareas submit CRLF; shlex only treats `\n` as whitespace, so a
    // stray `\r` would stick to the adjacent token and corrupt the argv.
    let text = text.replace('\r', "\n");
    if text.trim().is_empty() {
        return Ok(vec![]);
    }
    shlex::split(&text).ok_or_else(|| "extra args: unbalanced quote".into())
}

/// Inverse of [`parse_args_text`] for display: one option (plus its values)
/// per line, values re-quoted where needed.
pub fn args_to_lines(args: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for tok in args {
        let quoted = shlex::try_quote(tok)
            .map(|q| q.into_owned())
            .unwrap_or_else(|_| tok.clone());
        match lines.last_mut() {
            Some(line) if !is_opt(tok) => {
                line.push(' ');
                line.push_str(&quoted);
            }
            _ => lines.push(quoted),
        }
    }
    lines.join("\n")
}

/// Render one model's full container command line as a single copyable
/// string: `podman run … <image> <engine args…>`, shell-quoted.
///
/// The honest answer to "what will actually be run", which is what the
/// deleted `preset_section` used to be for (§8: local_edit's "Rendered
/// preset" panel becomes "Rendered command line").
pub fn command_line(spec: &RenderSpec) -> String {
    std::iter::once("podman".to_string())
        .chain(podman_run_argv(spec))
        .map(|tok| {
            shlex::try_quote(&tok)
                .map(|q| q.into_owned())
                .unwrap_or(tok)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rewrite a models-dir-relative path onto the `/models` mount, exactly like
/// the preset's `path_kv!` did.
fn mount_path(p: &str) -> String {
    format!("/models/{}", p.trim_start_matches('/'))
}

/// Render the llama-server CLI args for one model (not including `podman
/// run …` — see [`podman_run_argv`] for the full wrapping command). Empty for
/// an [`EngineArgs::Audio`] spec — audio has no CLI-flag surface at all; call
/// [`render_engine_args`] to dispatch on the engine correctly instead of
/// calling this directly unless the spec is already known to be llama-shaped.
pub fn render_llama_args(spec: &RenderSpec) -> Vec<String> {
    let EngineArgs::Llama(llama) = &spec.engine else {
        return Vec::new();
    };
    let mut out: Vec<String> = vec![
        "-m".into(),
        mount_path(gguf_path(llama)),
        "--alias".into(),
        spec.model_id.clone(),
        "--host".into(),
        "0.0.0.0".into(),
        "--port".into(),
        CONTAINER_PORT.to_string(),
    ];
    // Always-emitted keys are claimed up front so a freeform arg cannot
    // relabel the model or repoint host/port, the same protection
    // `render_preset` gives `model`/`sleep-idle-seconds`
    // (`extra_args_cannot_override_the_section_basics`).
    let mut taken: BTreeSet<String> = ["model", "alias", "host", "port"]
        .into_iter()
        .map(String::from)
        .collect();

    match llama {
        LlamaArgs::Chat { params, args, .. } => {
            push_chat_params(&mut out, params, &mut taken);
            push_freeform_args(&mut out, args, &taken);
        }
        LlamaArgs::Aux {
            kind,
            pooling,
            ctx_size,
            args,
            ..
        } => {
            push_aux_flags(&mut out, *kind, pooling.as_deref(), *ctx_size, &mut taken);
            push_freeform_args(&mut out, args, &taken);
        }
    }
    out
}

/// The audio.cpp container command tail: everything after the image name.
/// Mirrors `audio::podman_run_args`'s tail exactly (`server --config
/// /config/<CONFIG_FILE_NAME>`) — audio.cpp takes no model-selecting flag,
/// the mounted `server.json` at that path is the model.
fn render_audio_args() -> Vec<String> {
    vec![
        "server".into(),
        "--config".into(),
        format!("/config/{}", super::audio::CONFIG_FILE_NAME),
    ]
}

/// sd-server's container command tail (image-generation design §3):
///
/// ```text
/// --listen-ip 0.0.0.0 --listen-port 8080 --eager-load
/// --lora-model-dir /models/loras --hires-upscalers-dir /models/upscalers
/// --diffusion-model /models/… --vae /models/… --llm /models/…
/// <runtime flags…> <generation defaults…>
/// ```
///
/// **The five unconditional flags are claimed**, exactly as
/// [`render_llama_args`] claims `model`/`alias`/`host`/`port`, and for
/// measured reasons rather than taste (§12): the default listen address is the
/// container's own loopback, which a published port cannot reach; lazy loading
/// makes "ready" mean "the port answers" rather than "the weights are
/// resident", which would make the admission ledger's figure meaningless; and
/// an unset `--lora-model-dir`/`--hires-upscalers-dir` makes the capabilities
/// route throw a filesystem exception. A row may repoint the two directories
/// through its own `files` entry — it may not remove the flags.
///
/// **Every key is spelled by the image, never by substitution.** A canonical
/// key (`clip_l`, `diffusion_model`) goes through
/// [`SdcppCaps::flag_for`]; a key the image does not know is an error rather
/// than a guess, because the alternative is a container that exits with an
/// unhelpful usage dump.
///
/// Fallible, unlike its siblings: `model` vs `diffusion_model` exclusivity and
/// the unknown-key check are the two things that cannot be repaired later —
/// the pre-flight (`ops::image_model_problems`) reports them at save time, and
/// this is the backstop for a row that reached a start anyway.
pub fn render_image_args(spec: &RenderSpec) -> Result<Vec<String>, String> {
    let EngineArgs::Image(img) = &spec.engine else {
        return Ok(Vec::new());
    };
    let caps = &img.caps;

    // Exactly one of the two ways to name a pipeline (§2.6) — asked under the
    // spelling *this build* uses, because `-m` and `--model` are one flag and
    // a row that writes `m` names a checkpoint just as much as one that writes
    // `model`.
    let named = |k: &str| {
        img.files.iter().any(|(key, v)| {
            caps.resolve_key(key) == k && v.as_str().is_some_and(|v| !v.trim().is_empty())
        })
    };
    match (named("model"), named("diffusion_model")) {
        (true, true) => {
            return Err(
                "files names both 'model' and 'diffusion_model' — an all-in-one \
                 checkpoint and a standalone diffusion model are alternatives, \
                 not a pair"
                    .into(),
            )
        }
        (false, false) => {
            return Err(
                "files names neither 'model' (an all-in-one checkpoint) nor \
                 'diffusion_model' (a standalone diffusion model) — one of the two \
                 is what loads the pipeline"
                    .into(),
            )
        }
        _ => {}
    }

    let mut out: Vec<String> = vec![
        "--listen-ip".into(),
        "0.0.0.0".into(),
        "--listen-port".into(),
        CONTAINER_PORT.to_string(),
        "--eager-load".into(),
    ];
    let mut taken: BTreeSet<String> = super::image::CLAIMED_FLAGS
        .iter()
        .map(|k| (*k).to_string())
        .collect();

    // The two directory flags, at the row's path or the class default — the
    // flag itself is not the row's to drop.
    for (key, default) in super::image::DEFAULT_DIRS {
        let flag = caps
            .flag_for(key)
            .ok_or_else(|| format!("this image has no '{key}' flag to point at a directory"))?;
        out.push(flag.to_string());
        out.push(mount_path(&super::image::dir_path_from(
            &img.files,
            &caps_key_in(caps, &img.files, key),
            default,
        )));
        taken.insert((*key).to_string());
    }

    // The weight files, in the map's own (sorted) order, each rewritten onto
    // the `/models` mount.
    //
    // Claimed-ness is decided on the key the *vocabulary* resolves to, never
    // on the stored spelling: `l` is `--listen-ip` and `m` is `--model`, so a
    // raw-key guard would let an alias render a second copy of a flag lmgw
    // already spelled itself.
    for (key, value) in &img.files {
        let Some(flag) = caps.get(key) else {
            return Err(unknown_key_message(caps, "files", key));
        };
        if !taken.insert(flag.key.clone()) {
            continue;
        }
        let Some(path) = value.as_str().map(str::trim).filter(|p| !p.is_empty()) else {
            return Err(format!(
                "files key '{key}' must be a non-empty path relative to the image models dir"
            ));
        };
        out.push(flag.flag.clone());
        out.push(mount_path(path));
    }

    // Runtime + generation flags. The stored JSON type decides the shape: a
    // `true` is the switch it says it is, and a number keeps its own
    // formatting rather than being routed through a float.
    for (key, value) in &img.args {
        let Some(flag) = caps.get(key) else {
            return Err(unknown_key_message(caps, "args", key));
        };
        if !taken.insert(flag.key.clone()) {
            // A claimed key is dropped silently, the same resolution
            // `push_freeform_args` gives a colliding llama flag: the
            // structured side wins, and a leftover copy is neutralized rather
            // than turned into a start failure. `image_model_problems` names
            // it at save time, which is where an owner can act on it.
            continue;
        }
        push_image_value(&mut out, &flag.flag, value)?;
    }
    Ok(out)
}

/// Render one `args` entry. `true` is a bare switch; `false`/`null` render
/// nothing at all (sd-server has no `--no-*` twins to say the opposite with,
/// so "off" is the absence of the flag); an array repeats the flag, which is
/// what sd-server's "can be used multiple times" options want.
fn push_image_value(
    out: &mut Vec<String>,
    flag: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    use serde_json::Value;
    match value {
        Value::Bool(true) => out.push(flag.to_string()),
        Value::Bool(false) | Value::Null => {}
        Value::String(v) => {
            out.push(flag.to_string());
            out.push(v.clone());
        }
        Value::Number(n) => {
            out.push(flag.to_string());
            out.push(n.to_string());
        }
        Value::Array(items) => {
            for item in items {
                push_image_value(out, flag, item)?;
            }
        }
        Value::Object(_) => {
            return Err(format!(
                "args value for '{flag}' is a JSON object; sd-server flags take a \
                 string, a number, a switch, or a list of those"
            ))
        }
    }
    Ok(())
}

/// The spelling this row stored a canonical key under, so the two directory
/// flags read a `files` entry written as `lora-model-dir` or `lora_model_dir`
/// alike. The canonical key itself when the row does not name it at all.
fn caps_key_in(
    caps: &SdcppCaps,
    files: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> String {
    files
        .keys()
        .find(|k| caps.resolve_key(k) == key)
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

fn unknown_key_message(caps: &SdcppCaps, what: &str, key: &str) -> String {
    let base = format!("{what} key '{key}' is not an sd-server flag in this image");
    match caps.suggest(key) {
        Some(hit) => format!("{base} (did you mean '{hit}'?)"),
        None => base,
    }
}

/// Dispatch on [`EngineArgs`] to render the right container command tail —
/// llama CLI flags, the fixed audio.cpp invocation, or sd-server's argv. What
/// [`podman_run_argv`] actually calls; [`render_llama_args`] stays public and
/// llama-specific for the existing per-flag unit tests.
///
/// An image row that cannot be rendered comes back **empty**, not as an error:
/// this signature is the one the adoption path compares against a running
/// container's `Cmd`, and an empty tail never equals a real one, so such a
/// container is replaced rather than adopted. The error text itself is what
/// [`render_image_args`] gives the start path, which is where it belongs — a
/// start must fail loudly, a comparison must only fail to match.
pub fn render_engine_args(spec: &RenderSpec) -> Vec<String> {
    match &spec.engine {
        EngineArgs::Llama(_) => render_llama_args(spec),
        EngineArgs::Audio => render_audio_args(),
        EngineArgs::Image(_) => render_image_args(spec).unwrap_or_default(),
    }
}

/// Emit the structured chat params as CLI tokens, in the same field order
/// `push_params_as_ini` uses, so a diff between the two renderers stays a
/// diff of syntax rather than of coverage. Returns nothing — `taken` is
/// filled in place so [`push_freeform_args`] can see it.
fn push_chat_params(out: &mut Vec<String>, p: &LlamaParams, taken: &mut BTreeSet<String>) {
    macro_rules! value_flag {
        ($key:expr, $v:expr) => {
            if let Some(v) = $v {
                out.push(format!("--{}", $key));
                out.push(v.to_string());
                taken.insert($key.to_string());
            }
        };
    }
    macro_rules! path_flag {
        ($key:expr, $v:expr) => {
            if let Some(p) = $v.as_deref().filter(|p: &&str| !p.is_empty()) {
                out.push(format!("--{}", $key));
                out.push(mount_path(p));
                taken.insert($key.to_string());
            }
        };
    }
    macro_rules! switch {
        ($key:expr, $on:expr) => {
            if $on {
                out.push(format!("--{}", $key));
                taken.insert($key.to_string());
            }
        };
    }

    value_flag!("ctx-size", &p.ctx_size);
    value_flag!("n-predict", &p.n_predict);
    value_flag!("n-gpu-layers", &p.n_gpu_layers);
    value_flag!("threads", &p.threads);
    value_flag!("batch-size", &p.batch_size);
    value_flag!("ubatch-size", &p.ubatch_size);
    value_flag!("parallel", &p.parallel);
    // Tri-state switch, same shape as `reasoning_preserve` below: `Some(true)`
    // is `--kv-unified`, `Some(false)` is `--no-kv-unified`, `None` leaves
    // llama-server's own default alone (unified exactly when `parallel` is
    // unset — `LlamaParams::effective_kv_unified`).
    if let Some(on) = p.kv_unified {
        let key = if on { "kv-unified" } else { "no-kv-unified" };
        out.push(format!("--{key}"));
        taken.insert("kv-unified".to_string());
        taken.insert("no-kv-unified".to_string());
    }
    value_flag!("kv-unified-per-slot", &p.kv_unified_per_slot);
    value_flag!("flash-attn", &p.flash_attn);
    value_flag!("cache-type-k", &p.cache_type_k);
    value_flag!("cache-type-v", &p.cache_type_v);
    value_flag!("cache-ram", &p.cache_ram);
    switch!("jinja", p.jinja);
    path_flag!("chat-template-file", &p.chat_template_file);
    value_flag!("reasoning-format", &p.reasoning_format);
    value_flag!("reasoning", &p.reasoning);
    value_flag!("reasoning-budget", &p.reasoning_budget);
    // Tri-state switch: `Some(true)` is the positive flag, `Some(false)` is
    // the `--no-*` twin, `None` leaves the template's own default alone.
    // Both names are claimed either way, matching `render_preset`'s
    // `reasoning_preserve` handling exactly.
    if let Some(on) = p.reasoning_preserve {
        let key = if on {
            "reasoning-preserve"
        } else {
            "no-reasoning-preserve"
        };
        out.push(format!("--{key}"));
        taken.insert("reasoning-preserve".to_string());
        taken.insert("no-reasoning-preserve".to_string());
    }
    value_flag!("reasoning-effort", &p.reasoning_effort);
    if !p.chat_template_kwargs.is_empty() {
        let json = serde_json::Value::Object(p.chat_template_kwargs.clone()).to_string();
        out.push("--chat-template-kwargs".into());
        out.push(json);
        taken.insert("chat-template-kwargs".to_string());
    }
    value_flag!("temp", &p.temp);
    value_flag!("top-p", &p.top_p);
    value_flag!("top-k", &p.top_k);
    value_flag!("min-p", &p.min_p);
    value_flag!("repeat-penalty", &p.repeat_penalty);
    value_flag!("presence-penalty", &p.presence_penalty);
    value_flag!("seed", &p.seed);
    // `--mmproj` and `--no-mmproj` are mutually exclusive; an explicit path
    // wins, mirroring `push_params_as_ini`. If neither is set, both keys stay
    // unclaimed so freeform args can still supply one.
    if p.mmproj_path.as_deref().is_some_and(|s| !s.is_empty()) {
        path_flag!("mmproj", &p.mmproj_path);
        taken.insert("no-mmproj".to_string());
    } else if p.no_mmproj {
        switch!("no-mmproj", true);
        taken.insert("mmproj".to_string());
    }
    path_flag!("model-draft", &p.draft_gguf_path);
    value_flag!("spec-type", &p.spec_type);
    value_flag!("spec-draft-n-max", &p.spec_draft_n_max);
    value_flag!("spec-draft-n-min", &p.spec_draft_n_min);
    value_flag!("spec-draft-ngl", &p.spec_draft_ngl);
    value_flag!("fit", &p.fit);
    value_flag!("fit-ctx", &p.fit_ctx);
}

/// Emit the aux kind flag (+ pooling for embedders), mirroring
/// the deleted `render_aux_preset`'s claims exactly.
fn push_aux_flags(
    out: &mut Vec<String>,
    kind: AuxKind,
    pooling: Option<&str>,
    ctx_size: Option<i64>,
    taken: &mut BTreeSet<String>,
) {
    // Both kind flags are claimed in every section, not just this section's
    // own: a stale `--reranking` left in an embedder's freeform args would
    // otherwise flip it into a reranker, and llama-server answers
    // `/v1/embeddings` against a reranker with HTTP 200 and an all-zero
    // vector (spike-verified) — a corpus of noise that looks healthy.
    taken.insert("embeddings".to_string());
    taken.insert("reranking".to_string());
    match kind {
        AuxKind::Embed => {
            out.push("--embeddings".to_string());
            if let Some(p) = pooling.filter(|p| !p.is_empty()) {
                out.push("--pooling".to_string());
                out.push(p.to_string());
                taken.insert("pooling".to_string());
            }
        }
        AuxKind::Rerank => {
            out.push("--reranking".to_string());
            // `--reranking` selects rank pooling internally; a `--pooling`
            // flag from *any* source breaks reranking outright (llama-server
            // quirk, spike-verified). Claimed unconditionally — even a
            // structured `pooling` value on a rerank row (which should never
            // be set, but nothing stops it structurally) must not leak
            // through, and neither may a freeform `--pooling`.
            taken.insert("pooling".to_string());
        }
    }
    if let Some(c) = ctx_size {
        out.push("--ctx-size".to_string());
        out.push(c.to_string());
        taken.insert("ctx-size".to_string());
    }
}

/// Append freeform CLI args, dropping any whose key (or short-alias
/// canonical form) is already `taken`.
///
/// This is the CLI-mode sibling of `push_args_as_ini`'s duplicate guard, and
/// resolves collisions the same way: **the structured/always-emitted side
/// silently wins**, the colliding freeform token (and its value, if it took
/// one) is dropped rather than erroring. Silent-wins beats erroring here for
/// the reason the preset renderer's guard gave: the value the UI and tools show is
/// the structured one, and a stale freeform copy is exactly the kind of
/// leftover this guard exists to neutralize without surfacing a spurious
/// failure on every model that has one.
///
/// Unlike the preset's version, tokens are kept verbatim rather than
/// rewritten into `key = value` form — this list becomes real `execve`
/// arguments, not text re-parsed by an INI loader, so there is no format to
/// normalize into.
fn push_freeform_args(out: &mut Vec<String>, args: &[String], taken: &BTreeSet<String>) {
    let args: Vec<&String> = args.iter().filter(|t| !t.is_empty()).collect();
    let mut i = 0;
    while i < args.len() {
        let tok = args[i];
        if !is_opt(tok) {
            // A bare token with nothing to key it on (a stray value, or a
            // preset-only `KEY=value` env-style leftover from before the
            // per-model migration). llama-server args have no positional
            // form to collide with, so it passes through unchanged.
            out.push(tok.clone());
            i += 1;
            continue;
        }
        let key_part = tok.trim_start_matches('-');
        let (key, inline_value) = match key_part.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (key_part, None),
        };
        let dup = taken.contains(key) || taken.contains(canonical_key(key));
        if inline_value.is_some() {
            if !dup {
                out.push(tok.clone());
            }
            i += 1;
            continue;
        }
        // llama-server options take at most one argument.
        let takes_value = i + 1 < args.len() && !is_opt(args[i + 1]);
        if !dup {
            out.push(tok.clone());
            if takes_value {
                out.push(args[i + 1].clone());
            }
        }
        i += if takes_value { 2 } else { 1 };
    }
}

/// Build the full `podman run` argv for one model's container (§3.3 + §3.6 +
/// the prod template, §10.4). Pure; unit-tested.
///
/// No `--rm`: a stopped container keeps its logs for post-mortem, and
/// `--replace` collects it on the next start (§3.6). Everything after the
/// image name is [`render_engine_args`]'s output, handed straight to the
/// image's entrypoint.
pub fn podman_run_argv(spec: &RenderSpec) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--replace".into()];
    if spec.init {
        args.push("--init".into());
    }
    args.extend([
        "--name".into(),
        spec.container_name.clone(),
        "--label".into(),
        format!("lmgw.instance={}", spec.container_prefix),
        "--label".into(),
        format!("lmgw.class={}", spec.class.as_str()),
        "--label".into(),
        format!("lmgw.model={}", spec.model_id),
        "--label".into(),
        format!("lmgw.engine={}", spec.class.engine()),
    ]);
    args.extend(spec.extra_run_args.iter().cloned());
    // Loopback only: every client reaches a model through lmgw's own
    // routes, which carry its auth, policy and logging — a port published on
    // every interface would hand the LAN an unauthenticated llama-server.
    args.extend([
        "-p".into(),
        format!("127.0.0.1:{}:{}", spec.host_port, CONTAINER_PORT),
        "-v".into(),
        format!("{}:/models:ro", spec.models_dir.trim_end_matches('/')),
    ]);
    if let Some(cfg) = &spec.config_mount {
        args.push("-v".into());
        args.push(format!("{}:/config:ro", cfg.display()));
    }
    // Last of the podman-side flags, immediately before the image: the
    // image's own ENTRYPOINT is what this replaces, and putting it anywhere
    // else in the line would still work but would read as if it belonged to
    // the mount above it.
    if let Some(bin) = &spec.entrypoint {
        args.push("--entrypoint".into());
        args.push(bin.clone());
    }
    args.push(spec.image.clone());
    args.extend(render_engine_args(spec));
    args
}
