//! Audio internals: the audio.cpp container (`audiocpp_server`) and the
//! model-config validation rules the `/api` plane enforces before writing a
//! row.
//!
//! Exposure no longer goes through a managed `upstreams` row. Per-model
//! containers §5 deleted that row (and `ensure_audio_upstream` with it): an
//! enabled audio model resolves straight out of the `audio_models` table onto
//! the synthetic [`audio_upstream`](crate::config::Snapshot::audio_upstream),
//! under `settings.audio.public_prefix`, and reaches its own container through
//! the same admission every other local model does.
//!
//! Callers are the `/api` plane and [`crate::ops`]; the askama page this
//! served went away at the P8 cutover.

use std::collections::HashSet;

use lmgw_api_types as dto;

use crate::audio::{self, CatalogSnapshot, SpecPackage};
use crate::state::SharedState;
use crate::store::{self, NewAudioModel};

// ---------------------------------------------------------------------------
// Model config validation
// ---------------------------------------------------------------------------

/// audio.cpp server task names (CLI `--task` values).
///
/// The canonical list is audio.cpp's own `kVocabulary` table
/// (`src/framework/runtime/task_vocabulary.cpp`), which upstream introduced so
/// the four lists it replaced could not drift apart. `midi` — score /
/// note-event transcription, served by `muscriptor` and `sheetsage2` — is the
/// fourteenth row and the one this list was missing.
pub(crate) const AUDIO_TASKS: [&str; 14] = [
    "tts", "asr", "gen", "clon", "vc", "svc", "s2s", "sep", "vad", "diar", "align", "vdes", "spk",
    "midi",
];

/// A preset must name a voice audio.cpp can actually load: one it ships
/// (`voice_id`) or a clip to clone (`voice_ref`). Anything else loads fine and
/// then fails at synthesis time, one request at a time.
pub(crate) fn check_voice_preset(
    what: &str,
    p: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    if p.contains_key("voice_id") || p.contains_key("voice_ref") {
        return Ok(p.clone());
    }
    Err(format!("{what}: needs a `voice_id` or a `voice_ref` path"))
}

/// Validate a `default_voice_preset` value against the presets it may name: a
/// bare string must be a declared preset (unless none are declared, in which
/// case it is a model-native voice id audio.cpp resolves itself), and an
/// inline object must pass [`check_voice_preset`].
pub(crate) fn check_default_voice_preset(
    v: &serde_json::Value,
    presets: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    match v {
        serde_json::Value::String(name) => {
            if !presets.is_empty() && !presets.contains_key(name) {
                return Err(format!(
                    "default voice preset: `{name}` is not one of the presets ({})",
                    presets.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            Ok(())
        }
        serde_json::Value::Object(obj) => {
            check_voice_preset("default voice preset", obj).map(|_| ())
        }
        _ => Err("default voice preset: must be a preset name or an object".into()),
    }
}

/// Every `voice_ref` under the container's `/models` mount must exist on the
/// host, because audiocpp_server opens the reference clips at **startup**: one
/// unreadable path and the container exits instead of serving, which shows up
/// as a dead container some time after the save that caused it. Paths outside
/// `/models` are left alone — an extra `podman run -v` can put them there.
pub(crate) fn check_voice_refs(state: &SharedState, m: &NewAudioModel) -> Result<(), String> {
    let root = state
        .snapshot()
        .settings
        .audio
        .models_dir
        .trim()
        .to_string();
    let refs = m
        .voice_presets
        .values()
        .chain(m.default_voice_preset.iter())
        .filter_map(|p| p.get("voice_ref"))
        .filter_map(|v| v.as_str());
    for r in refs {
        let Some(rel) = r.strip_prefix("/models/") else {
            continue;
        };
        if root.is_empty() {
            return Err(format!(
                "voice_ref `{r}`: the audio models dir is not configured, so nothing is mounted at /models"
            ));
        }
        let host = std::path::Path::new(root.trim_end_matches('/')).join(rel);
        if !host.is_file() {
            return Err(format!(
                "voice_ref `{r}`: no such file ({}) — audiocpp_server refuses to start without it",
                host.display()
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Model-spec catalog (audio.cpp's own `model_specs/*.json`)
// ---------------------------------------------------------------------------
//
// Same shape as the pre-cutover page: the snapshot is read from memory, then
// the kv store — never fetched implicitly, only by the explicit refresh — and
// "install a package" means queueing its files through the shared HF download
// queue with target `audio`. Creating the audio model row stays a separate,
// user-confirmed step (the old UI's per-package "serve" form; here the
// prefilled editor), because family/task/voice config is not derivable from
// the files alone.

/// kv key for the persisted spec catalog.
const CATALOG_KV_KEY: &str = "audio:catalog";

/// The cached catalog: in-memory first, then the persisted snapshot (which is
/// then memoized). `None` = nothing fetched yet on this machine.
pub(crate) async fn catalog_cached(state: &SharedState) -> Option<CatalogSnapshot> {
    if let Some(c) = state.audio_catalog.lock().unwrap().clone() {
        return Some(c);
    }
    let json = store::get_kv(&state.db, CATALOG_KV_KEY).await.ok()??;
    let snapshot = serde_json::from_str::<CatalogSnapshot>(&json).ok()?;
    *state.audio_catalog.lock().unwrap() = Some(snapshot.clone());
    Some(snapshot)
}

/// Live-fetch the catalog and persist it (memory + kv).
///
/// The refresh also lists each package repo on Hugging Face once
/// ([`audio::published`]), so the browser knows which packages a download
/// would be refused for. Listing failures do not fail the refresh: they keep
/// the repo's previous listing and become warnings on the snapshot.
pub(crate) async fn catalog_refresh(state: &SharedState) -> Result<CatalogSnapshot, String> {
    // The snapshot this replaces: a spec file that fails now keeps its
    // family from it, and a repo that cannot be listed keeps its listing.
    let previous = catalog_cached(state).await;
    let mut snapshot = audio::fetch_catalog(&state.http, previous.as_ref()).await?;
    let previous = previous.map(|s| s.listings).unwrap_or_default();
    let token = state.snapshot().settings.hf_token.clone();
    let (listings, warnings) =
        audio::published::list_repos(&state.http, &token, &snapshot.specs, &previous).await;
    snapshot.listings = listings;
    snapshot.warnings.extend(warnings);
    *state.audio_catalog.lock().unwrap() = Some(snapshot.clone());
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let _ = store::set_kv(&state.db, CATALOG_KV_KEY, &json).await;
    }
    Ok(snapshot)
}

mod install;
mod served;
mod suggest;
use served::RowWeights;
use suggest::{suggest_mode, suggest_task};

/// Client-facing id suggestion for a package (`pocket_tts_english_q8_0` →
/// `pocket-tts-english-q8-0`).
fn suggest_model_id(package_id: &str) -> String {
    package_id.replace('_', "-")
}

/// Model-root path suggestion in *this gateway's* download layout (HF
/// `owner/repo/file`, not the spec's `target_directory`): the parent dir of
/// the package's first file, e.g.
/// `audio-cpp/audio.cpp-gguf/PocketTTS-GGUF/english`.
fn suggest_path(repo: &str, pkg: &SpecPackage) -> String {
    let Some(first) = pkg.files.first() else {
        return String::new();
    };
    let dest = crate::hf::dest_rel_path(repo, first).unwrap_or_else(|_| first.to_string());
    match dest.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => dest,
    }
}

/// Compose the browser view: catalog snapshot × tracked downloads × configured
/// audio models. Pure, so the install-state rules are unit-testable.
///
/// `absent` are the tracked rows whose file is gone from disk
/// ([`install::absent_on_disk`]), and `rows` the enabled audio rows with the
/// weights they load ([`served::gather`]) — both read by the caller.
///
/// `mode` is `audio.catalog_revision`: whether a download follows a spec's
/// pin, which decides the listing a package's availability is read from.
pub(crate) fn catalog_view(
    snapshot: &CatalogSnapshot,
    tracked: &[store::HfModelRow],
    rows: &[RowWeights],
    absent: &HashSet<i64>,
    mode: crate::config::CatalogRevision,
) -> dto::AudioCatalog {
    let mut families: Vec<dto::AudioFamily> = snapshot
        .specs
        .iter()
        .map(|spec| {
            let recommended_id = spec
                .recommended()
                .map(|p| p.id.as_str())
                .unwrap_or_default();
            let mut packages: Vec<dto::AudioPackage> = spec
                .packages
                .iter()
                .map(|pkg| {
                    let repo = spec.package_repo(pkg);
                    // Rows this gateway tracks for the package's files. Split
                    // GGUFs expand at queue time, so a row set can be larger
                    // than `files` — matching by file name stays exact.
                    let rows: Vec<&store::HfModelRow> = match repo {
                        Some(repo) => tracked
                            .iter()
                            .filter(|t| t.repo == repo && pkg.files.contains(&t.file))
                            .collect(),
                        None => Vec::new(),
                    };
                    // Installed = every spec file downloaded and still on
                    // disk (audio-class gap 4): a spec that grew, or a file
                    // that went, leaves the package short of those.
                    let missing = install::missing_files(&pkg.files, &rows, absent);
                    let installed = repo.is_some() && missing.is_empty();
                    let partial = !installed && !rows.is_empty();
                    // Downloaded once, short of files now, nothing on its
                    // way: what "complete install" is for.
                    let incomplete = partial
                        && missing.len() < pkg.files.len()
                        && !rows.iter().any(|r| install::in_flight(r));
                    let size = match rows.iter().map(|r| r.size_bytes).sum::<Option<i64>>() {
                        Some(total) if installed => crate::hf::fmt_bytes(total.max(0) as u64),
                        _ => String::new(),
                    };
                    let suggested_path = repo.map(|r| suggest_path(r, pkg)).unwrap_or_default();
                    let download = spec.package_download(pkg);
                    let pinned_commit = audio::pins::spec_pin(spec, pkg).unwrap_or_default();
                    let revision = audio::pins::download_revision(spec, pkg, mode);
                    let (unpublished_files, availability_note) = match repo {
                        Some(r) => availability(snapshot, r, revision, &pkg.files),
                        None => Default::default(),
                    };
                    dto::AudioPackage {
                        id: pkg.id.clone(),
                        display_name: pkg.display_name.clone(),
                        description: pkg.description.clone(),
                        format: pkg.format.clone(),
                        precision: pkg.precision.clone(),
                        gated: download.map(|d| d.gated).unwrap_or(false),
                        unavailable_reason: spec
                            .package_reason(pkg)
                            .unwrap_or_default()
                            .to_string(),
                        revision: download
                            .and_then(|d| d.revision.clone())
                            .unwrap_or_default(),
                        pinned_commit: pinned_commit.to_string(),
                        pin_followed: !pinned_commit.is_empty()
                            && mode == crate::config::CatalogRevision::Pinned,
                        // Under `pinned`, files from a revision other than
                        // the one the spec names now say so.
                        downloaded_from: audio::pins::downloaded_from(
                            &rows,
                            (mode == crate::config::CatalogRevision::Pinned).then_some(revision),
                        ),
                        unpublished_files,
                        availability_note,
                        file_count: pkg.files.len() as u32,
                        size,
                        recommended: pkg.id == recommended_id,
                        installed,
                        partial,
                        missing_files: if rows.is_empty() { Vec::new() } else { missing },
                        incomplete,
                        repo: repo.unwrap_or_default().to_string(),
                        // Filled in below, once every package's install
                        // state is known (the directory fallback needs it).
                        served: false,
                        served_by: Vec::new(),
                        serving_unclear: String::new(),
                        download_ids: rows.iter().map(|r| r.id).collect(),
                        suggested_model_id: suggest_model_id(&pkg.id),
                        suggested_path,
                        suggested_task: suggest_task(spec, pkg),
                        suggested_mode: suggest_mode(spec),
                    }
                })
                .collect();
            let installed: HashSet<String> = packages
                .iter()
                .filter(|p| p.installed)
                .map(|p| p.id.clone())
                .collect();
            let mut serving = served::family_serving(spec, rows, &installed);
            for p in &mut packages {
                if let Some(s) = serving.packages.remove(&p.id) {
                    p.served = !s.served_by.is_empty();
                    p.served_by = s.served_by;
                    p.serving_unclear = s.unclear.join("; ");
                }
            }
            let option = |o: &crate::audio::SpecOption| dto::AudioFamilyOption {
                name: o.name.clone(),
                kind: o.kind.clone(),
                description: o.description.clone(),
                required: o.required,
                default: o.default.clone(),
                min: o.min,
                max: o.max,
                values: o.values.clone(),
            };
            dto::AudioFamily {
                family: spec.family.clone(),
                display_name: spec.display_name.clone(),
                category: spec.category.clone(),
                description: spec.description.clone(),
                tasks: spec.tasks.clone(),
                languages: spec.languages.clone(),
                status: spec.status.clone(),
                tags: spec.tags.clone(),
                docs: spec.docs.clone(),
                summary: spec.summary.clone(),
                builtin_voices: spec.builtin_voices.clone(),
                default_voice: spec.default_voice.clone().unwrap_or_default(),
                capabilities: spec.capabilities.clone(),
                options: dto::AudioFamilyOptions {
                    request: spec.options.request.iter().map(option).collect(),
                    load: spec.options.load.iter().map(option).collect(),
                    session: spec.options.session.iter().map(option).collect(),
                },
                any_installed: packages.iter().any(|p| p.installed),
                // A package of it is loaded — not merely "a row names this
                // family", which also held for a row loading nothing at all.
                served: packages.iter().any(|p| p.served),
                serving_note: serving.unmatched.join("; "),
                packages,
            }
        })
        .collect();
    // What is already on this machine floats to the top (stable otherwise:
    // `fetch_catalog` sorts by family).
    families.sort_by_key(|f| !(f.any_installed || f.served));
    let mut warnings = snapshot.warnings.clone();
    // A snapshot from before the refresh listed the repos: said once here,
    // not as a note on every package.
    let has_repos = families
        .iter()
        .any(|f| f.packages.iter().any(|p| !p.repo.is_empty()));
    if snapshot.listings.is_empty() && has_repos {
        warnings.insert(0, NOT_CHECKED.to_string());
    }
    dto::AudioCatalog {
        fetched_at: snapshot.fetched_at.clone(),
        families,
        warnings,
    }
}

/// [`audio::published::availability`] of a package's files in the listing
/// of `repo` at the revision its download takes. A snapshot listed before
/// that revision was (one from before pins, a family carried over from an
/// earlier refresh) says so rather than nothing.
fn availability(
    snapshot: &CatalogSnapshot,
    repo: &str,
    revision: &str,
    files: &[String],
) -> (Vec<String>, String) {
    use audio::published::{listing_key, listing_label};
    let label = listing_label(repo, revision);
    match snapshot.listings.get(&listing_key(repo, revision)) {
        Some(listing) => audio::published::availability(&label, files, Some(listing)),
        // Nothing listed at all: the catalog says it once (NOT_CHECKED).
        None if snapshot.listings.is_empty() => Default::default(),
        None => (
            Vec::new(),
            format!(
                "whether {label} publishes these files has not been checked yet — a catalog \
                 refresh checks it"
            ),
        ),
    }
}

/// The catalog's line for a snapshot whose package files were never checked.
const NOT_CHECKED: &str = "which package files Hugging Face publishes has not been checked yet \
                           — refreshing the catalog checks it";

/// The catalog as the UI reads it — cached only, no implicit network call.
pub(crate) async fn catalog(state: &SharedState) -> dto::AudioCatalog {
    let Some(snapshot) = catalog_cached(state).await else {
        return dto::AudioCatalog::default();
    };
    let tracked = store::list_hf_models_by_target(&state.db, "audio")
        .await
        .unwrap_or_default();
    let models = store::list_audio_models(&state.db)
        .await
        .unwrap_or_default();
    let absent = absent_rows(state, &tracked).await;
    let rows = row_weights(state, models).await;
    let mode = state.snapshot().settings.audio.catalog_revision;
    catalog_view(&snapshot, &tracked, &rows, &absent, mode)
}

/// [`served::gather`] on the blocking pool: each enabled row's root is walked
/// for the weights it loads.
async fn row_weights(
    state: &SharedState,
    models: Vec<crate::config::AudioModel>,
) -> Vec<RowWeights> {
    let dir = state.snapshot().settings.audio.models_dir.clone();
    tokio::task::spawn_blocking(move || served::gather(&dir, &models))
        .await
        .unwrap_or_default()
}

/// [`install::absent_on_disk`] on the blocking pool, against the audio
/// models dir.
async fn absent_rows(state: &SharedState, tracked: &[store::HfModelRow]) -> HashSet<i64> {
    let dir = state.snapshot().settings.audio.models_dir.clone();
    let rows = tracked.to_vec();
    tokio::task::spawn_blocking(move || install::absent_on_disk(&dir, &rows))
        .await
        .unwrap_or_default()
}

/// Queue a package's files (the WebUI's "install") through the shared HF
/// download machinery, target `audio`. Fire-and-forget like every other
/// download: rows land in the queue and progress is read back from
/// `/api/hf/downloads`.
pub(crate) async fn catalog_download(
    state: &SharedState,
    family: &str,
    package: &str,
) -> Result<dto::AudioCatalogInstall, String> {
    let snapshot = catalog_cached(state)
        .await
        .ok_or("catalog unavailable — refresh the catalog first")?;
    let spec = snapshot
        .specs
        .iter()
        .find(|s| s.family == family)
        .ok_or_else(|| format!("unknown model family '{family}'"))?;
    let pkg = spec
        .packages
        .iter()
        .find(|p| p.id == package)
        .ok_or_else(|| format!("unknown package '{package}' in family '{family}'"))?;
    let repo = spec
        .package_repo(pkg)
        .ok_or_else(|| format!("package '{package}' names no download source"))?
        .to_string();
    // The spec's pin, unless the setting says latest (audio/pins.rs).
    let mode = state.snapshot().settings.audio.catalog_revision;
    let revision = audio::pins::download_revision(spec, pkg, mode).to_string();
    let pinned = revision != audio::pins::MAIN;
    // A gated repo answers every file with a 401 until a token with the
    // accepted licence is configured — queueing first would spend the click on
    // a list of identical failures instead of saying what is missing.
    let gated = pkg
        .download
        .as_ref()
        .or(spec.default_download.as_ref())
        .map(|d| d.gated)
        .unwrap_or(false);
    if gated && state.snapshot().settings.hf_token.trim().is_empty() {
        return Err(format!(
            "'{repo}' is a gated Hugging Face repo: accept its licence on huggingface.co and \
             set a Hugging Face token under Settings → Tokens & updates first — without one every \
             file comes back 401"
        ));
    }
    // Only what the package lacks (audio-class gap 4): a package installed
    // before its spec grew gets the new file, not every file again. One
    // never downloaded gets everything.
    let tracked = store::list_hf_models_by_target(&state.db, "audio")
        .await
        .unwrap_or_default();
    let rows: Vec<&store::HfModelRow> = tracked
        .iter()
        .filter(|t| t.repo == repo && pkg.files.contains(&t.file))
        .collect();
    let absent = absent_rows(state, &tracked).await;
    let missing = install::missing_files(&pkg.files, &rows, &absent);
    // Under pinned, the package's installed files from another commit than
    // the pin come along: a pin moves when its files were re-tested
    // together, and fetching only what is missing would leave one package
    // from two commits (a new tokenizer beside an old GGUF).
    // Only those the pin changed: one whose bytes are the same at the pin is
    // recorded at it instead of fetched again.
    let (stale, recorded) = match mode {
        crate::config::CatalogRevision::Pinned => {
            let stale = install::at_another_revision(&rows, &absent, &revision);
            install::changed_at_pin(state, &rows, stale, &revision).await
        }
        crate::config::CatalogRevision::Latest => (Vec::new(), Vec::new()),
    };
    let unchanged = match recorded.is_empty() {
        true => String::new(),
        false => format!(
            "; unchanged at the pin, so recorded at it rather than fetched again: {}",
            recorded.join(", ")
        ),
    };
    let wanted: Vec<String> = missing
        .iter()
        .chain(stale.iter().filter(|f| !missing.contains(f)))
        .cloned()
        .collect();
    if wanted.is_empty() {
        return Ok(dto::AudioCatalogInstall {
            ok: true,
            family: family.to_string(),
            package: package.to_string(),
            repo,
            revision,
            files_queued: 0,
            downloads: Vec::new(),
            message: format!(
                "every file of {} is already installed — nothing to download{unchanged}",
                pkg.display_name
            ),
        });
    }
    let completing = !rows.is_empty();
    let pin_note = pinned.then(|| audio::pins::no_fallback(&repo, &revision));
    let queued = super::hf::queue_files_at(
        state,
        &repo,
        &wanted,
        "audio",
        &revision,
        pin_note.as_deref(),
    )
    .await?;
    let at = match pinned {
        true => format!(
            " at commit {}, the one the spec pins",
            audio::pins::short(&revision)
        ),
        false => String::new(),
    };
    // Hand back the queued rows so the caller can follow its own download
    // instead of diffing the whole tracked list (same as `ops::hf_add`).
    let tracked = store::list_hf_models(&state.db).await.unwrap_or_default();
    let downloads = tracked
        .iter()
        .filter(|r| r.repo == repo && wanted.contains(&r.file))
        .map(|r| dto::QueuedDownload {
            id: r.id,
            file: r.file.clone(),
            dest_path: r.dest_path.clone(),
            status: r.status.clone(),
        })
        .collect();
    Ok(dto::AudioCatalogInstall {
        ok: true,
        family: family.to_string(),
        package: package.to_string(),
        repo,
        revision,
        files_queued: queued as u32,
        downloads,
        message: if completing {
            let lacks = match missing.is_empty() {
                true => String::new(),
                false => format!("the file(s) it lacks: {}", missing.join(", ")),
            };
            let moved = match stale.is_empty() {
                true => String::new(),
                false => format!(
                    "{}the installed file(s) from another commit than the pin, so the package \
                     is one commit again: {}",
                    if lacks.is_empty() { "" } else { "; " },
                    stale.join(", ")
                ),
            };
            format!(
                "completing {} — downloading {queued} file(s){at}: {lacks}{moved}{unchanged}",
                pkg.display_name
            )
        } else {
            format!(
                "downloading {} — {queued} file(s){at}; the package is servable once they finish",
                pkg.display_name
            )
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{UpstreamKind, AUDIO_UPSTREAM_ID, AUDIO_UPSTREAM_NAME};
    use crate::state::AppState;
    use serde_json::{json, Map, Value};

    fn presets(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    /// An enabled audio model is routable on the strength of its row alone:
    /// `audio/<id>` resolves onto the synthetic audio upstream with the prefix
    /// stripped — no alias, no `upstreams` row to provision, and no running
    /// container. The base URL it carries is the unheld placeholder, because a
    /// local route is only forwardable through a hold (§5).
    #[tokio::test]
    async fn an_enabled_audio_model_routes_without_any_upstream_row() {
        let state = AppState::init_for_tests().await.unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.audio.public_prefix = "audio".into();
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();

        store::insert_audio_model(
            &state.db,
            &NewAudioModel {
                model_id: "qwen3-asr".into(),
                family: "qwen3_asr".into(),
                path: "audio-cpp/audio.cpp-gguf/Qwen3-ASR-0.6B-GGUF".into(),
                task: "asr".into(),
                mode: "offline".into(),
                lazy: None,
                busy_timeout_ms: None,
                backend: None,
                threads: None,
                load_options: Default::default(),
                session_options: Default::default(),
                default_request_options: Default::default(),
                model_spec_override: None,
                config_id: None,
                weight_id: None,
                voice_presets: Default::default(),
                default_voice_preset: None,
                enabled: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
            },
        )
        .await
        .unwrap();
        state.reload_snapshot().await.unwrap();

        let snap = state.snapshot();
        let route = snap.resolve("audio/qwen3-asr").unwrap();
        assert_eq!(route.upstream.id, AUDIO_UPSTREAM_ID);
        assert_eq!(route.upstream.name, AUDIO_UPSTREAM_NAME);
        assert_eq!(route.upstream.kind, UpstreamKind::AudioCpp);
        assert_eq!(route.upstream_model, "qwen3-asr");
        assert!(!route.upstream.expose_all, "nothing fans out a catalog now");
        assert_eq!(route.upstream.base_url, snap.audio_upstream().base_url);
        assert!(store::list_upstreams(&state.db).await.unwrap().is_empty());
    }

    /// A voice that audio.cpp cannot resolve fails one request at a time, long
    /// after the config was saved — so it is caught on save.
    #[test]
    fn a_preset_without_a_voice_is_rejected() {
        let err = check_voice_preset("voice preset 'narrator'", &presets(json!({"speed": 2})))
            .unwrap_err();
        assert!(err.contains("voice_id"), "{err}");
    }

    #[test]
    fn an_inline_default_preset_needs_no_named_presets() {
        check_default_voice_preset(&json!({"voice_id": "alba"}), &Map::new()).unwrap();
        let err = check_default_voice_preset(&json!({"speed": 2}), &Map::new()).unwrap_err();
        assert!(err.contains("voice_id"), "{err}");
    }

    #[test]
    fn a_bare_default_must_name_a_declared_preset() {
        let declared = presets(json!({"narrator": {"voice_id": "alba"}}));
        check_default_voice_preset(&json!("narrator"), &declared).unwrap();
        let err = check_default_voice_preset(&json!("narrater"), &declared).unwrap_err();
        assert!(err.contains("narrater"), "{err}");
        // With no presets defined at all it is a model-native voice id, which
        // audio.cpp resolves itself — nothing here can check it.
        check_default_voice_preset(&json!("alba"), &Map::new()).unwrap();
    }

    #[test]
    fn a_default_that_is_neither_a_name_nor_an_object_is_rejected() {
        let err = check_default_voice_preset(&json!(7), &Map::new()).unwrap_err();
        assert!(err.contains("preset name or an object"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Spec catalog
    // -----------------------------------------------------------------------

    const PINNED: crate::config::CatalogRevision = crate::config::CatalogRevision::Pinned;

    fn spec_snapshot() -> CatalogSnapshot {
        let spec = audio::parse_spec(&json!({
            "family": "pocket_tts",
            "display_name": "Pocket TTS",
            "description": "Small multilingual TTS.",
            "category": "tts",
            "tasks": ["clone", "tts"],
            "modes": ["streaming", "offline"],
            "languages": ["en", "de"],
            "ui": { "recommended_package": "pocket_tts_english_q8_0" },
            "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-GGUF" } },
            "packages": [
                {
                    "id": "pocket_tts_english_q8_0",
                    "display_name": "English · q8_0",
                    "format": "gguf",
                    "precision": "q8_0",
                    "files": ["english/model.gguf", "english/voices.bin"],
                },
                {
                    "id": "pocket_tts_multi_f16",
                    "display_name": "Multilingual · f16",
                    "format": "gguf",
                    "precision": "f16",
                    "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-Multi" },
                    "files": ["multi/model.gguf"],
                },
                {
                    "id": "pocket_tts_no_source",
                    "display_name": "Bring your own",
                    "format": "gguf",
                    "precision": "",
                    "files": ["byo/model.gguf"],
                },
            ],
        }));
        // The third package inherits the family download, so drop it to model
        // the "no download source" case the old UI rendered as plain text.
        let mut spec = spec;
        spec.default_download = None;
        spec.packages[0].download = Some(crate::audio::SpecDownload {
            kind: "huggingface_snapshot".into(),
            repo: "audio-cpp/PocketTTS-GGUF".into(),
            reason: String::new(),
            revision: Some("main".into()),
            gated: false,
        });
        CatalogSnapshot {
            fetched_at: "2026-08-29T10:00:00Z".into(),
            specs: vec![spec],
            listings: Default::default(),
            warnings: Vec::new(),
        }
    }

    fn tracked(repo: &str, file: &str, status: &str, size: i64) -> store::HfModelRow {
        store::HfModelRow {
            id: (file.len() * 10) as i64,
            repo: repo.into(),
            file: file.into(),
            dest_path: format!("{repo}/{file}"),
            target: "audio".into(),
            etag: None,
            size_bytes: Some(size),
            status: status.into(),
            error: None,
            downloaded_at: None,
            requested_revision: Some("main".into()),
            resolved_commit: None,
        }
    }

    /// The browser view is the old page's install-state rules verbatim:
    /// installed = every file downloaded, partial = some tracked, and a
    /// package with no repo in its spec cannot be installed at all. The
    /// prefill (id/path/task/mode) is what the pre-cutover "serve" form put
    /// into the create form.
    #[test]
    fn catalog_view_derives_install_state_and_the_editor_prefill() {
        let snapshot = spec_snapshot();
        let rows = vec![
            tracked(
                "audio-cpp/PocketTTS-GGUF",
                "english/model.gguf",
                "done",
                1000,
            ),
            tracked(
                "audio-cpp/PocketTTS-GGUF",
                "english/voices.bin",
                "done",
                24_000,
            ),
            tracked("audio-cpp/PocketTTS-Multi", "multi/model.gguf", "queued", 0),
        ];
        let view = catalog_view(&snapshot, &rows, &[], &HashSet::new(), PINNED);
        assert_eq!(view.fetched_at, "2026-08-29T10:00:00Z");
        let fam = &view.families[0];
        assert_eq!(fam.display_name, "Pocket TTS");
        assert_eq!(fam.tasks, ["clone", "tts"]);
        assert!(fam.any_installed);
        assert!(!fam.served);

        let english = &fam.packages[0];
        assert!(english.installed && !english.partial);
        assert!(english.recommended, "ui.recommended_package wins");
        assert_eq!(english.file_count, 2);
        assert_eq!(english.size, "24.4 KiB");
        assert_eq!(english.repo, "audio-cpp/PocketTTS-GGUF");
        // `clone` is not a server task name; it maps to `clon`.
        assert_eq!(english.suggested_task, "clon");
        assert_eq!(english.suggested_mode, "streaming");
        assert_eq!(english.suggested_model_id, "pocket-tts-english-q8-0");
        assert_eq!(
            english.suggested_path, "audio-cpp/PocketTTS-GGUF/english",
            "the download layout's dir, not the spec's target_directory"
        );
        assert_eq!(english.download_ids.len(), 2);

        let multi = &fam.packages[1];
        assert!(!multi.installed && multi.partial, "queued = partial");
        assert!(multi.size.is_empty(), "size only once it is on disk");

        let byo = &fam.packages[2];
        assert!(byo.repo.is_empty(), "no download source");
        assert!(!byo.installed && !byo.partial);
        assert!(byo.suggested_path.is_empty());
    }

    /// An enabled row marks the package whose weights it loads — by the
    /// file, not by sharing a directory with it — and with it the family
    /// (what floats it to the top of the browser).
    #[test]
    fn catalog_view_marks_the_package_a_row_loads() {
        let row = |candidates: &[&str]| RowWeights {
            model_id: "pocket-tts".into(),
            family: "pocket_tts".into(),
            path: "audio-cpp/PocketTTS-GGUF/english".into(),
            weight_id: None,
            candidates: candidates.iter().map(|c| c.to_string()).collect(),
            any_gguf: !candidates.is_empty(),
        };
        let english = "audio-cpp/PocketTTS-GGUF/english/model.gguf";
        let view = catalog_view(
            &spec_snapshot(),
            &[],
            &[row(&[english])],
            &HashSet::new(),
            PINNED,
        );
        let fam = &view.families[0];
        assert!(fam.served);
        assert!(fam.packages[0].served);
        assert_eq!(fam.packages[0].served_by, ["pocket-tts"]);
        assert!(fam.packages[0].serving_unclear.is_empty());
        assert!(!fam.packages[1].served);

        // A pick across two packages serves neither and says so on both.
        let multi = "audio-cpp/PocketTTS-Multi/multi/model.gguf";
        let view = catalog_view(
            &spec_snapshot(),
            &[],
            &[row(&[english, multi])],
            &HashSet::new(),
            PINNED,
        );
        let fam = &view.families[0];
        assert!(
            !fam.served,
            "no package of it is loaded, so the family is not serving"
        );
        assert!(fam.serving_note.is_empty(), "the packages say it");
        for p in &fam.packages[..2] {
            assert!(!p.served && p.served_by.is_empty(), "{p:?}");
            assert!(p.serving_unclear.contains("no weight_id"), "{p:?}");
        }
        assert!(
            fam.packages[2].serving_unclear.is_empty(),
            "no download source"
        );

        // A row loading a GGUF no package ships: no "serving" chip with
        // nothing under it, a family note instead.
        let stray = "audio-cpp/PocketTTS-GGUF/english/old.gguf";
        let view = catalog_view(
            &spec_snapshot(),
            &[],
            &[row(&[stray])],
            &HashSet::new(),
            PINNED,
        );
        let fam = &view.families[0];
        assert!(!fam.served);
        assert!(
            fam.serving_note
                .contains("loads old.gguf under audio-cpp/PocketTTS-GGUF/english"),
            "{}",
            fam.serving_note
        );
    }

    /// A refresh's listings reach the packages: a file its repo does not
    /// publish (the Orukeet case), a repo that could not be listed — and,
    /// before anything was listed, one line for the whole catalog.
    #[test]
    fn catalog_view_says_which_files_are_not_published() {
        use crate::audio::published::RepoListing;
        let mut snapshot = spec_snapshot();
        let view = catalog_view(&snapshot, &[], &[], &HashSet::new(), PINNED);
        assert_eq!(view.warnings, [NOT_CHECKED]);
        assert!(view.families[0]
            .packages
            .iter()
            .all(|p| p.availability_note.is_empty() && p.unpublished_files.is_empty()));

        snapshot.listings.insert(
            "audio-cpp/PocketTTS-GGUF".into(),
            RepoListing {
                listed_at: "2026-10-02T12:00:00Z".into(),
                present: Some(["english/model.gguf".to_string()].into()),
                checked: ["english/model.gguf", "english/voices.bin"]
                    .map(String::from)
                    .into(),
                error: String::new(),
            },
        );
        snapshot.listings.insert(
            "audio-cpp/PocketTTS-Multi".into(),
            RepoListing {
                error: "offline".into(),
                ..Default::default()
            },
        );
        snapshot.warnings = vec!["could not list audio-cpp/PocketTTS-Multi".into()];
        let view = catalog_view(&snapshot, &[], &[], &HashSet::new(), PINNED);
        assert_eq!(
            view.warnings, snapshot.warnings,
            "checked: no not-checked line"
        );
        let [english, multi, byo] = &view.families[0].packages[..] else {
            panic!("three packages");
        };
        assert_eq!(english.unpublished_files, ["english/voices.bin"]);
        assert!(
            english
                .availability_note
                .contains("has no english/voices.bin (listed 2026-10-02)"),
            "{}",
            english.availability_note
        );
        assert!(multi.unpublished_files.is_empty());
        assert!(multi.availability_note.ends_with(": offline"), "{multi:?}");
        assert!(byo.unpublished_files.is_empty() && byo.availability_note.is_empty());
    }

    /// A package the spec pins to a commit: the chip's facts, and which
    /// listing "not published" is read from — the pin under `pinned`, `main`
    /// under `latest`.
    #[test]
    fn catalog_view_reads_a_pinned_package_at_the_revision_it_downloads() {
        use crate::audio::published::{listing_key, RepoListing};
        use crate::config::CatalogRevision;
        let pin = "607a30d783dfa663caf39e06633721c8d4cfcd7e";
        let mut snapshot = spec_snapshot();
        let multi = &mut snapshot.specs[0].packages[1];
        multi.download.as_mut().unwrap().revision = Some(pin.into());
        let repo = "audio-cpp/PocketTTS-Multi";
        let listed = |present: &[&str]| RepoListing {
            listed_at: "2026-10-02T12:00:00Z".into(),
            present: Some(present.iter().map(|f| f.to_string()).collect()),
            checked: ["multi/model.gguf".to_string()].into(),
            error: String::new(),
        };
        // At main the file is gone (a re-upload); at the pin it is there.
        snapshot
            .listings
            .insert(listing_key(repo, "main"), listed(&[]));
        snapshot
            .listings
            .insert(listing_key(repo, pin), listed(&["multi/model.gguf"]));

        let view = catalog_view(&snapshot, &[], &[], &HashSet::new(), PINNED);
        let p = &view.families[0].packages[1];
        assert_eq!(p.pinned_commit, pin);
        assert!(p.pin_followed);
        assert!(p.unpublished_files.is_empty(), "{p:?}");

        let view = catalog_view(
            &snapshot,
            &[],
            &[],
            &HashSet::new(),
            CatalogRevision::Latest,
        );
        let p = &view.families[0].packages[1];
        assert_eq!(p.pinned_commit, pin);
        assert!(!p.pin_followed);
        assert_eq!(p.unpublished_files, ["multi/model.gguf"]);
        assert!(
            p.availability_note.starts_with(&format!("{repo} has no")),
            "{p:?}"
        );
        // Unpinned packages: no pin, nothing followed.
        let english = &view.families[0].packages[0];
        assert!(english.pinned_commit.is_empty() && !english.pin_followed);

        // Listed before the pin was (an older snapshot): said, not silent.
        snapshot.listings.remove(&listing_key(repo, pin));
        let view = catalog_view(&snapshot, &[], &[], &HashSet::new(), PINNED);
        let p = &view.families[0].packages[1];
        assert!(
            p.availability_note.starts_with(&format!(
                "whether {repo} at 607a30d publishes these files has not"
            )),
            "{p:?}"
        );
    }

    /// Nothing cached → an empty view, and no network call: the page opens
    /// instantly and offers Refresh instead of hanging on GitHub.
    #[tokio::test]
    async fn the_catalog_endpoint_is_empty_until_refreshed() {
        let state = AppState::init_for_tests().await.unwrap();
        let view = catalog(&state).await;
        assert!(view.fetched_at.is_empty());
        assert!(view.families.is_empty());
        let err = catalog_download(&state, "pocket_tts", "x")
            .await
            .unwrap_err();
        assert!(err.contains("refresh the catalog first"), "{err}");
    }
}
