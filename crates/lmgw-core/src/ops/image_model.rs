//! The image class (image-generation design §8)

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::config::{HoldFallbackMode, ImageModel, Snapshot};
use crate::hf;
use crate::runtime::argv;
use crate::runtime::{lifecycle, Class};
use crate::state::SharedState;
use crate::store::{self};

use super::*;

/// A `files` or `args` map as a patch carries it — the image class's twin of
/// [`ArgList`].
///
/// Two spellings for one map, and both are load-bearing. `/api/op` and the
/// dashboard send a real JSON object, because that is what the row stores.
/// The MCP plane cannot: its schemas are flat scalars on purpose (see the
/// module note on [`crate::mcp::selfadmin`] — a tool whose arguments need
/// hand-built JSON is a tool a small local model cannot call reliably), which
/// is the same reason `capabilities_override` accepts a JSON *string* there.
/// So a text block of `key = value` lines — the syntax an owner reads off
/// `sd-server --help` — is accepted wherever the object is.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum KeyMap {
    Map(Map<String, Value>),
    Text(String),
}

impl KeyMap {
    /// The map as it will be stored: keys canonicalized
    /// ([`crate::sdcpp_caps::canonical_key`] — `--diffusion-model`,
    /// `diffusion-model` and `diffusion_model` are one key), values typed.
    ///
    /// In the text form a line is `key=value`, `key value`, or a bare `key`
    /// for a switch; a leading `--` is tolerated so a flag pasted out of the
    /// help works. A value that parses as a JSON number or boolean is stored
    /// as one — `steps = 4` has to reach the argv as `4`, not as `"4"` — and
    /// everything else is stored as the string it is, which is what a path or
    /// a sampler name wants. Nothing is coerced the other way: an object form
    /// keeps whatever types the caller sent.
    pub fn entries(&self) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        match self {
            Self::Map(m) => {
                for (k, v) in m {
                    let key = crate::sdcpp_caps::canonical_key(k);
                    if key.is_empty() {
                        return Err("a files/args key must not be empty".into());
                    }
                    out.insert(key, v.clone());
                }
            }
            Self::Text(t) => {
                for line in t.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let (key, value) = match line.split_once('=') {
                        Some((k, v)) => (k, v.trim()),
                        None => match line.split_once(char::is_whitespace) {
                            Some((k, v)) => (k, v.trim()),
                            None => (line, ""),
                        },
                    };
                    let key = crate::sdcpp_caps::canonical_key(key);
                    if key.is_empty() {
                        return Err(format!("'{line}' has no key before its value"));
                    }
                    out.insert(key, parse_scalar(value));
                }
            }
        }
        Ok(out)
    }
}

/// One `key = value` value from the text form. Empty is the switch the bare
/// key asked for; a JSON number or boolean keeps its type; everything else is
/// the literal string, never a guess at what it might have meant.
fn parse_scalar(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::Bool(true);
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ (Value::Number(_) | Value::Bool(_))) => v,
        // `cfg_scale = "1.0"` is the quoted spelling of the string `1.0`, and
        // the quotes are JSON syntax rather than two bytes of the value — left
        // in, they render as `--cfg-scale "1.0"` and sd-server reads a value
        // it cannot parse.
        Ok(Value::String(s)) => Value::String(s),
        _ => Value::String(raw.to_string()),
    }
}

/// A list of short names as a patch carries it (`modes`): a JSON array, or a
/// comma/whitespace-separated string for the same reason [`KeyMap`] takes
/// text.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum StrList {
    List(Vec<String>),
    Text(String),
}

impl StrList {
    pub fn items(&self) -> Vec<String> {
        match self {
            Self::List(v) => v
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Self::Text(t) => t
                .split([',', ' ', '\n', '\t'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        }
    }
}

/// Sparse patch for an image (stable-diffusion.cpp) model — the fourth class's
/// twin of [`AuxModelPatch`], and the one departure from the audio precedent
/// the design makes on purpose (§8): audio CRUD lives in the web layer alone,
/// this one is shared by `/api/op/image_model_set` and `lmgw__image_model_set`,
/// because an image row is `files` + `args` and nothing an agent cannot check.
///
/// Same conventions as every other patch here: a field left out keeps what the
/// row has, an empty string clears the fields that can be cleared, and `clear`
/// names the rest.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImageModelPatch {
    pub action: String,
    pub id: Option<i64>,
    pub model_id: Option<String>,
    /// Flag key → path relative to the image models dir. Replaces the stored
    /// map wholesale (a map merge would leave no way to *remove* a file).
    pub files: Option<KeyMap>,
    /// Runtime + default-generation flags, same key convention.
    pub args: Option<KeyMap>,
    /// `img_gen` / `vid_gen`; empty restores the class default (`img_gen`).
    pub modes: Option<StrList>,
    /// The pipeline takes reference images and may serve `/v1/images/edits`.
    pub edit: Option<bool>,
    pub enabled: Option<bool>,
    /// Per-model image override; empty clears back to the class image.
    pub image: Option<String>,
    /// `podman run` args override; name it in `clear` to revert to the class
    /// setting. An empty list inherits too (it is never stored as one);
    /// blank text leaves the current value.
    pub extra_run_args: Option<ArgList>,
    pub warm_start: Option<bool>,
    pub idle_seconds: Option<i64>,
    /// `inherit` (default — no fallback for this class) | `none` | `alias`.
    pub hold_fallback_mode: Option<String>,
    pub hold_fallback: Option<String>,
    /// Owner override of the derived `/v1/models` facts; a JSON object, or a
    /// JSON string holding one.
    pub capabilities_override: Option<Value>,
    /// Field names to reset: `args`, `modes`, `extra_run_args`, `image`,
    /// `hold_fallback`, `capabilities_override`. Not `files` — a row with no
    /// files names no pipeline, so emptying it could only ever be refused.
    pub clear: Option<String>,
}

impl ImageModelPatch {
    fn clears(&self, field: &str) -> bool {
        clear_has(self.clear.as_deref(), field)
    }

    /// Every name in `clear` has to be a field this patch can actually reset —
    /// the same refusal [`LocalModelPatch`] gives, and for the same reason: a
    /// misspelled name that is silently ignored reads as a clear that worked.
    /// `files` is called out by name because it is the one field an owner
    /// would reasonably expect to be clearable and deliberately is not.
    fn check_clear(&self) -> Result<(), String> {
        for name in clear_names(self.clear.as_deref()) {
            match name {
                "args"
                | "modes"
                | "extra_run_args"
                | "image"
                | "hold_fallback"
                | "capabilities_override" => {}
                "files" => {
                    return Err(
                        "clear: 'files' is not clearable — a row with no files names no \
                         pipeline, so an empty map could only ever be refused at save time. \
                         Send the files map you want instead; it replaces the stored one \
                         wholesale."
                            .into(),
                    )
                }
                other => return Err(format!("clear: '{other}' is not a clearable field name")),
            }
        }
        Ok(())
    }
}

/// The container image an image row runs: its own override, else the class
/// default — which vocabulary its keys are checked against (§3.1, §3.6).
fn effective_image_image(state: &SharedState, override_image: &Option<String>) -> String {
    override_image
        .clone()
        .unwrap_or_else(|| state.snapshot().settings.image.image.clone())
}

/// The sd-server flag vocabulary one row is saved against: the image's own
/// `--help`, read through a throwaway container, or the vocabulary lmgw ships
/// with when that container cannot run.
///
/// A vocabulary read never blocks a save (WP1's rule for the start path, held
/// to here): the binary links `libcuda.so.1` directly, so `--help` needs the
/// card, and a card busy with another model would otherwise make every image
/// row unsaveable. The degradation is reported rather than hidden — the second
/// half of the pair is the sentence the response carries.
async fn image_vocabulary(
    state: &SharedState,
    image: &str,
) -> (std::sync::Arc<crate::sdcpp_caps::SdcppCaps>, Option<String>) {
    let snap = state.snapshot();
    match state
        .runtime()
        .sdcpp_caps(
            &snap.settings.container_prefix,
            image,
            &snap.settings.image.extra_run_args,
        )
        .await
    {
        Ok(caps) => (caps, None),
        Err(e) => {
            let caps = crate::sdcpp_caps::SdcppCaps::embedded();
            let note = format!(
                "the flag vocabulary of '{image}' could not be read ({e}), so the keys were \
                 checked against the {} flags lmgw ships with — a flag this build added since \
                 would be refused here and still start",
                caps.len()
            );
            (caps, Some(note))
        }
    }
}

/// Whether `files` names flag `key` (a canonical key) with a non-empty value,
/// **under any spelling** this build accepts for it: `m` is `--model`, and a
/// row that spells it that way names a checkpoint just as much as one that
/// writes `model`.
pub(super) fn image_names(
    caps: &crate::sdcpp_caps::SdcppCaps,
    files: &Map<String, Value>,
    key: &str,
) -> bool {
    files.iter().any(|(k, v)| {
        caps.resolve_key(k) == key && v.as_str().is_some_and(|v| !v.trim().is_empty())
    })
}

/// The problems of an image row's two maps that are about the **keys** alone:
/// a key this build has no flag for, a key naming a flag lmgw renders itself,
/// and a path flag sitting in `args` instead of `files`.
///
/// Shared by the refusal at save time ([`check_image_row`]) and the advisory
/// sweep ([`image_model_problems`]), which differ only in what they do with
/// the list. Every check resolves the stored key through the vocabulary first,
/// because an alias is the same flag: `args: {"l": "0.0.0.0"}` is
/// `--listen-ip` and would otherwise render a second copy of the flag the
/// container is reached on.
pub(super) fn image_key_problems(
    caps: &crate::sdcpp_caps::SdcppCaps,
    files: &Map<String, Value>,
    args: &Map<String, Value>,
) -> Vec<String> {
    use crate::runtime::image as image_cfg;

    let mut out = caps.validate_keys(files, args);
    for (what, map) in [("files", files), ("args", args)] {
        for key in map.keys() {
            if !caps.is_known(key) {
                continue; // already reported by name
            }
            let resolved = caps.resolve_key(key);
            if image_cfg::CLAIMED_FLAGS.contains(&resolved.as_str()) {
                out.push(format!(
                    "{what} key '{key}' is {}, which lmgw renders itself on every start — the \
                     address and port the container answers on are not the row's to set, and \
                     this key is dropped",
                    caps.flag_for(key).unwrap_or(&resolved)
                ));
                continue;
            }
            // A path in `args` is passed through verbatim, so it names a host
            // path inside a container that has only `/models` mounted — the
            // file is simply not there. `files` is the map whose values are
            // rewritten onto that mount.
            if what == "args" && caps.is_path_flag(key) {
                out.push(format!(
                    "args key '{key}' takes a path ({}), and a path in args is passed through \
                     unchanged — it would name a host path the container cannot see. Move it \
                     to files, whose values are resolved under the image models dir and \
                     rendered as /models/…",
                    caps.flag_for(key).unwrap_or(&resolved)
                ));
            }
        }
    }
    out
}

/// The three questions §4 says an image row has to answer before it is stored,
/// as refusals rather than as warnings: every key is a flag this image has,
/// the row names exactly one of the two ways to load a pipeline, and every
/// path it names is on disk under the class models dir.
///
/// The advisory twin is [`image_model_problems`], which runs on every group
/// apply against the *embedded* vocabulary — a row that was saved when the
/// probe worked must not become unreadable when the card is busy.
fn check_image_row(
    caps: &crate::sdcpp_caps::SdcppCaps,
    models_dir: &str,
    files: &Map<String, Value>,
    args: &Map<String, Value>,
) -> Result<(), String> {
    use crate::runtime::image as image_cfg;

    let unknown = image_key_problems(caps, files, args);
    match unknown.len() {
        0 => {}
        // One key is the usual case and reads as a sentence; several are
        // listed rather than reported one failed save at a time.
        1 => return Err(unknown.into_iter().next().expect("length checked")),
        n => return Err(format!("{n} keys were refused — {}", unknown.join("; "))),
    }

    let named = |k: &str| image_names(caps, files, k);
    match (named("model"), named("diffusion_model")) {
        (true, true) => {
            return Err(
                "files names both 'model' and 'diffusion_model' — an all-in-one checkpoint and \
                 a standalone diffusion model are alternatives, not a pair. Keep the one this \
                 pipeline ships."
                    .into(),
            )
        }
        (false, false) => {
            return Err(
                "files names neither 'model' (an all-in-one checkpoint) nor 'diffusion_model' \
                 (a standalone diffusion model) — one of the two is what loads the pipeline, \
                 and everything else (vae, clip_l, t5xxl, llm, …) hangs off it."
                    .into(),
            )
        }
        _ => {}
    }

    let defaults: Vec<&str> = image_cfg::DEFAULT_DIRS.iter().map(|(k, _)| *k).collect();
    for (key, value) in files {
        let resolved = caps.resolve_key(key);
        let Some(rel) = value.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
            return Err(format!(
                "files key '{key}' must be a non-empty path relative to the image models dir"
            ));
        };
        check_rel_path(rel, "image models dir")?;
        // lmgw creates these two itself before every start (§3), so their
        // absence now is not something the owner has to fix.
        if defaults.contains(&resolved.as_str()) {
            continue;
        }
        if models_dir.trim().is_empty() {
            return Err(format!(
                "the image models directory is not configured, so '{rel}' cannot be resolved — \
                 set it first (Settings → Runtimes → Image, or the `image.models_dir` section of \
                 /api/op/settings_set_full). Every files value is relative to that directory."
            ));
        }
        let path = image_cfg::file_target(models_dir, rel);
        let ok = if image_cfg::is_dir_key(&resolved) {
            path.is_dir()
        } else {
            path.is_file()
        };
        if !ok {
            return Err(format!(
                "{key} '{rel}' is not {} under the image models directory ({models_dir}) — \
                 download the files first and name them exactly as they sit on disk",
                if image_cfg::is_dir_key(&resolved) {
                    "a directory"
                } else {
                    "a file"
                }
            ));
        }
    }
    Ok(())
}

/// `files` values as they are **stored**: the in-container `/models/` prefix —
/// what the rendered command line shows, and what a caller copying a path back
/// out of it naturally pastes — and any leading slash dropped, exactly as
/// every other class's path field is normalized on the way in. Left un-dropped
/// it would render as `/models//models/…` and the model would fail to load
/// over a paste.
fn normalize_image_files(files: &mut Map<String, Value>) {
    for value in files.values_mut() {
        if let Some(s) = value.as_str() {
            *value = Value::String(strip_models_prefix(s));
        }
    }
}

/// `files` / `args` as they will be stored, from the patch and the row it is
/// applied to. A supplied map replaces; `clear` empties; neither keeps.
fn merged_map(
    patch: &Option<KeyMap>,
    cleared: bool,
    current: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    match patch {
        Some(m) => m.entries(),
        None if cleared => Ok(Map::new()),
        None => Ok(current.clone()),
    }
}

/// Create, update, delete, enable or disable an image (stable-diffusion.cpp)
/// model.
///
/// The ops-level home of the class's CRUD, shared by the dashboard's
/// `/api/op/image_model_set` and by `lmgw__image_model_set` — the departure
/// from audio that §8 argues for: the point of this gateway's tool plane is
/// that an agent can bring a model up end to end, and an image row is two
/// validated maps, which is exactly the shape the aux tools already handle.
///
/// Conventions are [`aux_model_set`]'s throughout: sparse patch, the class
/// models dir as the path boundary, `reload_snapshot()` after every write, and
/// container hygiene keyed on the *current* model id so a rename drops the
/// container that exists.
pub async fn image_model_set(state: &SharedState, p: ImageModelPatch) -> Result<Value, String> {
    let snap = state.snapshot();
    let all = snap.image_models.clone();
    let find = || -> Result<ImageModel, String> {
        if let Some(id) = p.id {
            return all
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .ok_or_else(|| format!("no image model with id {id}"));
        }
        let mid = opt(&p.model_id).ok_or("this action requires id or model_id")?;
        all.iter()
            .find(|m| m.model_id == mid)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no image model with model_id '{mid}'; configured: {}",
                    if all.is_empty() {
                        "(none)".to_string()
                    } else {
                        all.iter()
                            .map(|m| m.model_id.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            })
    };
    let models_dir = snap.settings.image.models_dir.clone();
    let reload = || async { state.reload_snapshot().await.map_err(|e| e.to_string()) };
    p.check_clear()?;

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("model_id", opt(&p.model_id).is_some()),
                ("files", p.files.is_some()),
            ])?;
            let model_id = opt(&p.model_id).expect("checked above");
            refuse_if_candidate_alias_name(&snap, &snap.image_public_name(&model_id))?;
            let mut files = merged_map(&p.files, false, &Map::new())?;
            normalize_image_files(&mut files);
            let args = merged_map(&p.args, false, &Map::new())?;
            let (caps, vocab_note) =
                image_vocabulary(state, &effective_image_image(state, &opt(&p.image))).await;
            check_image_row(&caps, &models_dir, &files, &args)?;
            let (hold_fallback_mode, hold_fallback) = resolve_hold_fallback_text(
                &snap,
                p.hold_fallback_mode.as_deref(),
                p.hold_fallback.as_deref(),
                p.clear.as_deref(),
                (HoldFallbackMode::Inherit, None),
            )?;
            let new = store::NewImageModel {
                model_id: model_id.clone(),
                files,
                args,
                modes: p.modes.as_ref().map(StrList::items).unwrap_or_default(),
                edit: p.edit.unwrap_or(false),
                enabled: p.enabled.unwrap_or(true),
                image: opt(&p.image),
                // A named clear wins over the value on create as on update:
                // the editor sends a blank field as `""` *and* names it. An
                // empty list without the clear is no override either.
                extra_run_args: match &p.extra_run_args {
                    _ if p.clears("extra_run_args") => None,
                    Some(a) => run_args_override(Some(a.tokens()?)),
                    None => None,
                },
                warm_start: p.warm_start.unwrap_or(false),
                idle_seconds: p.idle_seconds.unwrap_or(300),
                hold_fallback_mode,
                hold_fallback,
                capabilities_override: match &p.capabilities_override {
                    Some(v) => parse_capabilities_override(v)?,
                    None => None,
                },
            };
            let mut warnings: Vec<String> = vocab_note.into_iter().collect();
            warnings.extend(image_model_problems(&models_dir, &as_image_model(&new)));
            let id = store::insert_image_model(&state.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            let snap = reload().await?;
            Ok(json!({
                "ok": true, "id": id, "warnings": warnings,
                "public_name": snap.image_public_name(&model_id),
                "command_line": command_line_preview(state, Class::Image, &model_id),
                "message": format!(
                    "image model '{model_id}' created — clients request it as '{}' on \
                     /v1/images/generations{}; run lmgw__local_model_test model_id={model_id} \
                     target=image to prove it loads and draws (its container starts on the \
                     first request)",
                    snap.image_public_name(&model_id),
                    if new.edit { " and /v1/images/edits" } else { "" },
                ),
            }))
        }
        "update" | "enable" | "disable" => {
            let cur = find()?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let mut files = merged_map(&p.files, false, &cur.files)?;
            normalize_image_files(&mut files);
            let args = merged_map(&p.args, p.clears("args"), &cur.args)?;
            let image = match p.image.as_deref().map(str::trim) {
                _ if p.clears("image") => None,
                Some("") => None,
                Some(s) => Some(s.to_string()),
                None => cur.image.clone(),
            };
            let (caps, vocab_note) =
                image_vocabulary(state, &effective_image_image(state, &image)).await;
            check_image_row(&caps, &models_dir, &files, &args)?;
            // An empty list inherits the class; blank text is "not supplied".
            let extra_run_args = if p.clears("extra_run_args") {
                None
            } else {
                match &p.extra_run_args {
                    Some(a) if a.is_blank_text() => cur.extra_run_args.clone(),
                    Some(a) => Some(a.tokens()?),
                    None => cur.extra_run_args.clone(),
                }
            };
            let extra_run_args = run_args_override(extra_run_args);
            let (hold_fallback_mode, hold_fallback) = resolve_hold_fallback_text(
                &snap,
                p.hold_fallback_mode.as_deref(),
                p.hold_fallback.as_deref(),
                p.clear.as_deref(),
                (cur.hold_fallback_mode, cur.hold_fallback.clone()),
            )?;
            let new_image_model_id = match (p.id, opt(&p.model_id)) {
                (Some(_), Some(m)) => m,
                _ => cur.model_id.clone(),
            };
            // Same rename guard as `model_set`/`local_model_set` (review
            // finding X4).
            if new_image_model_id != cur.model_id {
                refuse_if_candidate_alias_name(
                    &snap,
                    &snap.image_public_name(&new_image_model_id),
                )?;
            }
            let new = store::NewImageModel {
                model_id: new_image_model_id,
                files,
                args,
                modes: match &p.modes {
                    Some(m) => m.items(),
                    None if p.clears("modes") => Vec::new(),
                    None => cur.modes.clone(),
                },
                edit: p.edit.unwrap_or(cur.edit),
                enabled,
                image,
                extra_run_args,
                warm_start: p.warm_start.unwrap_or(cur.warm_start),
                idle_seconds: p.idle_seconds.unwrap_or(cur.idle_seconds),
                hold_fallback_mode,
                hold_fallback,
                capabilities_override: if p.clears("capabilities_override") {
                    None
                } else {
                    match &p.capabilities_override {
                        Some(v) => parse_capabilities_override(v)?,
                        None => cur.capabilities_override.clone(),
                    }
                },
            };
            let mut warnings: Vec<String> = vocab_note.into_iter().collect();
            warnings.extend(image_model_problems(&models_dir, &as_image_model(&new)));
            store::update_image_model(&state.db, cur.id, &new)
                .await
                .map_err(|e| e.to_string())?;
            // The learned peak belongs to the pipeline that was measured
            // (image-generation §9). A different file set is a different
            // pipeline, and one changed flag — `offload_to_cpu`, `vae_tiling`,
            // a default width — moves the compute buffers by gigabytes, so the
            // measurement no longer describes anything. Reset rather than kept
            // and quietly wrong; said out loud rather than reset in silence,
            // because it is a promise admission has stopped making.
            let peak_reset =
                cur.peak_extra_bytes.is_some() && (new.files != cur.files || new.args != cur.args);
            if peak_reset {
                store::set_image_model_peak(&state.db, cur.id, None)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            reload().await?;
            // Container hygiene (per-model-containers §3.4/§3.6), keyed on the
            // *current* model id: an image container's argv names every file
            // and flag, so stopping it is the whole apply — the next start
            // renders the new row. A rename is a drop, because the container
            // belongs to a model that no longer exists.
            let note = if new.model_id != cur.model_id {
                lifecycle::drop_model(state, Class::Image, &cur.model_id).await;
                Some(format!(
                    "it was renamed from '{}', so that container was stopped and removed — the \
                     next request starts '{}' fresh",
                    cur.model_id, new.model_id
                ))
            } else if enabled {
                lifecycle::stop_for_apply(state, Class::Image, &cur.model_id).await
            } else {
                lifecycle::drop_model(state, Class::Image, &cur.model_id).await;
                Some(format!(
                    "'{}' is disabled — its container was stopped and removed",
                    cur.model_id
                ))
            };
            let peak_note = if peak_reset {
                format!(
                    "; learned peak reset — {} no longer describes this pipeline, so admission \
                     charges nothing for its compute buffers until one generation has taught \
                     them again",
                    hf::fmt_bytes(cur.peak_extra_bytes.unwrap_or(0))
                )
            } else {
                String::new()
            };
            Ok(json!({
                "ok": true, "id": cur.id, "warnings": warnings,
                "peak_reset": peak_reset,
                "command_line": command_line_preview(state, Class::Image, &new.model_id),
                "message": match note {
                    Some(note) => format!(
                        "image model '{}' updated — {note}{peak_note}", new.model_id
                    ),
                    None => format!(
                        "image model '{}' updated — it is not running, so the next request \
                         starts it with the new configuration{peak_note}", new.model_id
                    ),
                },
            }))
        }
        "delete" => {
            let cur = find()?;
            store::delete_image_model(&state.db, cur.id)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            // §3.4: immediately, not at the next boot.
            lifecycle::drop_model(state, Class::Image, &cur.model_id).await;
            Ok(json!({
                "ok": true, "id": cur.id,
                "message": format!(
                    "image model '{}' deleted — its container was stopped and removed",
                    cur.model_id
                ),
            }))
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable)"
        )),
    }
}

/// A [`store::NewImageModel`] as the row it would become, for the checks that
/// read rows. The id is a placeholder — nothing in the checks reads it.
fn as_image_model(n: &store::NewImageModel) -> ImageModel {
    ImageModel {
        id: 0,
        model_id: n.model_id.clone(),
        files: n.files.clone(),
        args: n.args.clone(),
        modes: n.modes.clone(),
        edit: n.edit,
        enabled: n.enabled,
        image: n.image.clone(),
        extra_run_args: n.extra_run_args.clone(),
        warm_start: n.warm_start,
        idle_seconds: n.idle_seconds,
        hold_fallback_mode: n.hold_fallback_mode,
        hold_fallback: n.hold_fallback.clone(),
        capabilities_override: n.capabilities_override.clone(),
        // Not part of a patch at all — the checks that read rows never look at
        // it, and the real row keeps whatever was learned (or has just been
        // reset by the save, which `image_model_set` does explicitly).
        peak_extra_bytes: None,
        peak_learned_at: None,
    }
}

/// Read back one image model, or `None` when nothing matches — the image half
/// of [`local_model_get`], same shape where the fields overlap.
pub(super) fn image_model_get(
    snap: &Snapshot,
    state: &SharedState,
    id: Option<i64>,
    model_id: Option<&str>,
) -> Result<Option<Value>, String> {
    let found = match (id, model_id) {
        (Some(id), _) => snap.image_models.iter().find(|m| m.id == id),
        (None, Some(mid)) => snap.image_models.iter().find(|m| m.model_id == mid),
        (None, None) => return Err("pass id or model_id".into()),
    };
    let Some(m) = found else {
        return Ok(None);
    };
    let models_dir = snap.settings.image.models_dir.clone();
    let present: Map<String, Value> = m
        .files
        .iter()
        .map(|(k, v)| {
            let rel = v.as_str().unwrap_or_default();
            let ok = !models_dir.trim().is_empty() && {
                let path = crate::runtime::image::file_target(&models_dir, rel);
                if crate::runtime::image::is_dir_key(k) {
                    path.is_dir()
                } else {
                    path.is_file()
                }
            };
            (k.clone(), json!(ok))
        })
        .collect();
    Ok(Some(json!({
        "id": m.id,
        "class": "image",
        "model_id": m.model_id,
        "public_name": snap.image_public_name(&m.model_id),
        "files": m.files,
        "files_present": present,
        "args": m.args,
        "modes": m.modes(),
        "edit": m.edit,
        "endpoints": if m.edit {
            json!(["/v1/images/generations", "/v1/images/edits"])
        } else {
            json!(["/v1/images/generations"])
        },
        "enabled": m.enabled,
        "image": m.image,
        "extra_run_args": m.extra_run_args.as_ref().map(|a| argv::args_to_lines(a)),
        "warm_start": m.warm_start,
        "idle_seconds": m.idle_seconds,
        "hold_fallback_mode": m.hold_fallback_mode.as_str(),
        "hold_fallback": m.hold_fallback.clone(),
        "capabilities_override": m.capabilities_override.clone(),
        // Learned, not configured (image-generation §9): what one generation
        // was measured to need above this pipeline's idle residency, and what
        // admission keeps free while it is resident. `null` means no
        // generation has run since the row last changed — the sentence beside
        // it is what an agent reading this needs, because the hole it names is
        // invisible everywhere else.
        "peak_extra_bytes": m.peak_extra_bytes,
        "peak_learned_at": m.peak_learned_at,
        "peak": match m.peak_extra_bytes {
            Some(p) => format!(
                "one generation needed {} above idle (learned {}); admission keeps that much \
                 free for it while it is resident",
                hf::fmt_bytes(p),
                m.peak_learned_at.as_deref().unwrap_or("at an unknown time"),
            ),
            None => "not learned yet — the compute buffers of a generation are 2-6 GiB this \
                     gateway cannot see until one has run. Generate once at the largest size \
                     you use (the Image lab, or lmgw__local_model_test) and admission will \
                     account for it."
                .to_string(),
        },
        "command_line": command_line_preview(state, Class::Image, &m.model_id),
        "problems": image_model_problems(&models_dir, m),
    })))
}
