//! The local images of the three engines (container-builds §3 "Image", §9.1
//! Images tab, §15 `container_images` / `container_image_delete` /
//! `container_image_tag`).
//!
//! podman is the source of truth; lmgw keeps no image table. An image lmgw
//! built says so in its `dev.lmgw.*` labels (its provenance, even after the
//! run's row is gone); anything else — a `build.sh` tag, a ghcr pull — is
//! *external*, and belongs to an engine when its repository name says so or
//! a class uses it.
//!
//! **Who uses an image** ([`UsageIndex`]) is answered by image ID, never by
//! comparing strings: a class default, a model's image override (the four
//! classes, the four model tables) and every container, running or stopped,
//! each resolved to the ID it names right now. Two spellings of one image are one
//! image; a moved tag is a different one.

use std::collections::HashMap;

use lmgw_api_types::builds::{
    ContainerImage, ContainerImageDeleteResponse, ContainerImageTagResponse,
    ContainerImagesResponse, DiskInfo, ImageProvenance, ImageUse, ImageUseKind, ResolvedExtra,
};
use serde::Deserialize;

use super::model::{BuildRunStatus, Engine};
use super::run::{buildah_dirs, short_id, Podman};
use super::tags;
use crate::config::Snapshot;
use crate::runtime::registry::RuntimeView;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store;

// ---------------------------------------------------------------------------
// podman's views
// ---------------------------------------------------------------------------

/// One row of `podman images --format json`.
#[derive(Debug, Deserialize)]
struct ImageRow {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Names", default)]
    names: Option<Vec<String>>,
    #[serde(rename = "RepoDigests", default)]
    digests: Option<Vec<String>>,
    #[serde(rename = "Size", default)]
    size: u64,
    #[serde(rename = "Created", default)]
    created: i64,
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
}

/// A local image.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalImage {
    /// Full ID, no `sha256:`.
    pub id: String,
    /// Every `repo:tag` it carries.
    pub names: Vec<String>,
    pub digests: Vec<String>,
    pub size: u64,
    /// RFC 3339.
    pub created: String,
    pub labels: HashMap<String, String>,
}

impl From<ImageRow> for LocalImage {
    fn from(r: ImageRow) -> Self {
        Self {
            id: r.id.trim_start_matches("sha256:").to_string(),
            names: r.names.unwrap_or_default(),
            digests: r.digests.unwrap_or_default(),
            size: r.size,
            created: chrono::DateTime::from_timestamp(r.created, 0)
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default(),
            labels: r.labels.unwrap_or_default(),
        }
    }
}

/// Every local image, once per ID.
///
/// `podman images --format json` repeats an image's whole row once per tag
/// (podman 5: a three-tag image is three identical rows), so the rows are
/// merged by ID here — every consumer (who uses what, the Images tab, the
/// update check) wants the image, not the row. Names and digests are
/// unioned in the order podman gave them, in case a version ever splits them
/// across rows instead.
pub(crate) async fn local_images(podman: &Podman) -> Result<Vec<LocalImage>, String> {
    let out = podman.run_ok(&["images", "--format", "json"]).await?;
    if out.stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<ImageRow> = serde_json::from_str(&out.stdout)
        .map_err(|e| format!("podman images returned unreadable JSON: {e}"))?;
    Ok(merge_by_id(rows.into_iter().map(LocalImage::from)))
}

fn merge_by_id(images: impl Iterator<Item = LocalImage>) -> Vec<LocalImage> {
    let mut out: Vec<LocalImage> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for img in images {
        match at.get(&img.id) {
            Some(&i) => {
                let kept = &mut out[i];
                for n in img.names {
                    if !kept.names.contains(&n) {
                        kept.names.push(n);
                    }
                }
                for d in img.digests {
                    if !kept.digests.contains(&d) {
                        kept.digests.push(d);
                    }
                }
            }
            None => {
                at.insert(img.id.clone(), out.len());
                out.push(img);
            }
        }
    }
    out
}

/// One row of `podman ps [-a --external] --format json`.
#[derive(Debug, Deserialize)]
struct PsRow {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "ImageID", default)]
    image_id: String,
    #[serde(rename = "Names", default)]
    names: Option<Vec<String>>,
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Status", default)]
    status: String,
}

async fn ps_rows(podman: &Podman, args: &[&str]) -> Result<Vec<PsRow>, String> {
    let mut argv = vec!["ps"];
    argv.extend_from_slice(args);
    argv.extend_from_slice(&["--format", "json"]);
    let out = podman.run_ok(&argv).await?;
    if out.stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&out.stdout)
        .map_err(|e| format!("podman ps returned unreadable JSON: {e}"))
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether the last path component of `r` carries a `:tag`.
fn has_tag(r: &str) -> bool {
    r.rsplit('/').next().is_some_and(|last| last.contains(':'))
}

/// The spellings podman would look `reference` up by locally: as given,
/// with `:latest` when untagged, and under `localhost/` and Docker Hub when
/// it names no registry.
fn spellings(reference: &str) -> Vec<String> {
    let r = reference.trim();
    let tagged = if has_tag(r) || r.contains('@') {
        r.to_string()
    } else {
        format!("{r}:latest")
    };
    let mut out = vec![r.to_string(), tagged.clone()];
    let first = tagged.split('/').next().unwrap_or("");
    let has_registry = tagged.contains('/')
        && (first.contains('.') || first.contains(':') || first == "localhost");
    if !has_registry {
        out.push(format!("localhost/{tagged}"));
        if tagged.contains('/') {
            out.push(format!("docker.io/{tagged}"));
        } else {
            out.push(format!("docker.io/library/{tagged}"));
        }
    }
    out.dedup();
    out
}

/// `repo:tag@sha256:…` → `repo@sha256:…`: a digest reference that also
/// names a tag (what `podman pull` accepts, and what a pinned class image is
/// often written as) is the digest — the tag is ignored, as podman ignores
/// it — and podman's `RepoDigests` carry no tag. `None` for a reference
/// without both.
fn digest_without_tag(r: &str) -> Option<String> {
    let (name, digest) = r.split_once('@')?;
    let (dir, last) = match name.rsplit_once('/') {
        Some((d, l)) => (Some(d), l),
        None => (None, name),
    };
    let (repo, _tag) = last.split_once(':')?;
    Some(match dir {
        Some(d) => format!("{d}/{repo}@{digest}"),
        None => format!("{repo}@{digest}"),
    })
}

/// The local image `reference` (a tag, a digest reference — with or without
/// a tag before the digest — or an ID of at least 12 hex characters) names,
/// and the name it matched (`None` for an ID).
pub(crate) fn resolve<'a>(
    reference: &str,
    images: &'a [LocalImage],
) -> Option<(&'a LocalImage, Option<String>)> {
    let r = reference.trim();
    if r.is_empty() {
        return None;
    }
    let bare = r.trim_start_matches("sha256:");
    if bare.len() >= 12 && is_hex(bare) {
        if let Some(img) = images.iter().find(|i| i.id.starts_with(bare)) {
            return Some((img, None));
        }
    }
    let by_digest = digest_without_tag(r);
    for s in spellings(r)
        .into_iter()
        .chain(by_digest.iter().flat_map(|d| spellings(d)))
    {
        if let Some(img) = images
            .iter()
            .find(|i| i.names.contains(&s) || i.digests.contains(&s))
        {
            return Some((img, Some(s)));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// One user of an image, with what it names.
#[derive(Debug, Clone)]
struct Use {
    use_: ImageUse,
    /// The configured reference (config users only).
    reference: Option<String>,
    /// The image ID it resolves to right now; `None` when it names nothing
    /// on this machine.
    id: Option<String>,
    /// The local name a config reference matched, for "same tag" questions.
    name: Option<String>,
}

/// Every local image and everything that uses one, resolved to image IDs.
pub(crate) struct UsageIndex {
    pub images: Vec<LocalImage>,
    uses: Vec<Use>,
}

impl UsageIndex {
    /// `podman images`, `podman ps -a --external` and the snapshot's eight
    /// image fields. Every container counts, not only the running ones: a
    /// stopped one — exited, created, a buildah working container — keeps
    /// its image from `podman rmi` just the same.
    pub async fn load(state: &SharedState) -> Result<Self, String> {
        let podman = Podman::of(state);
        let images = local_images(&podman).await?;
        let containers = ps_rows(&podman, &["-a", "--external"]).await?;
        let snap = state.snapshot();
        let s = &snap.settings;
        let mut uses = Vec::new();
        for c in configured_images(&snap) {
            let hit = resolve(&c.reference, &images);
            uses.push(Use {
                use_: ImageUse {
                    kind: c.kind,
                    class: c.class.into(),
                    model_id: c.model_id,
                    container: None,
                },
                id: hit.as_ref().map(|(i, _)| i.id.clone()),
                name: hit.and_then(|(_, n)| n),
                reference: Some(c.reference),
            });
        }
        let registry = state.runtime().list();
        for c in containers {
            let image_id = c.image_id.trim_start_matches("sha256:").to_string();
            if image_id.is_empty() {
                continue;
            }
            // What plain `podman ps` lists: the running ones, nothing else.
            let kind = if c.state.eq_ignore_ascii_case("running") {
                ImageUseKind::RunningContainer
            } else {
                ImageUseKind::StoppedContainer
            };
            let labels = c.labels.unwrap_or_default();
            let name = c
                .names
                .and_then(|n| n.into_iter().next())
                .unwrap_or_else(|| short_id(&c.id).to_string());
            // Every model container lmgw starts is labelled with its class
            // and model; one that is not (started by an older lmgw, relabelled
            // by hand) is still found by name, so "recreate the containers on
            // this image" can address every model container it lists.
            let label = |k: &str| labels.get(k).filter(|v| !v.is_empty()).cloned();
            let (class, model_id) = match (label("lmgw.class"), label("lmgw.model")) {
                (Some(class), Some(model)) => (class, Some(model)),
                (class, model) => {
                    match container_owner(&name, &registry, &s.container_prefix, &snap) {
                        Some((class, model)) => (class, Some(model)),
                        None => (class.unwrap_or_default(), model),
                    }
                }
            };
            uses.push(Use {
                use_: ImageUse {
                    kind,
                    class,
                    model_id,
                    container: Some(name),
                },
                reference: None,
                id: Some(image_id),
                name: None,
            });
        }
        Ok(Self { images, uses })
    }

    /// The ID `reference` names right now.
    pub fn id_of(&self, reference: &str) -> Option<String> {
        resolve(reference, &self.images).map(|(i, _)| i.id.clone())
    }

    /// Everything using image `id`.
    pub fn users_of(&self, id: &str) -> Vec<ImageUse> {
        let id = id.trim_start_matches("sha256:");
        self.uses
            .iter()
            .filter(|u| {
                u.id.as_deref().is_some_and(|x| {
                    !x.is_empty() && !id.is_empty() && (x.starts_with(id) || id.starts_with(x))
                })
            })
            .map(|u| u.use_.clone())
            .collect()
    }

    /// The config users that name the local tag `name` itself (not merely
    /// the same image under another tag) — what untagging `name` breaks.
    pub fn users_of_name(&self, name: &str) -> Vec<ImageUse> {
        self.uses
            .iter()
            .filter(|u| u.name.as_deref() == Some(name))
            .map(|u| u.use_.clone())
            .collect()
    }

    /// Who a move of `moving` concerns (§6): the config users naming it —
    /// by any spelling, resolvable or not — and the running containers of
    /// the image it pointed at before.
    pub fn followers(&self, moving: &str, old_id: Option<&str>) -> Vec<ImageUse> {
        let wanted = spellings(moving);
        let mut out: Vec<ImageUse> = self
            .uses
            .iter()
            .filter(|u| {
                u.reference
                    .as_deref()
                    .is_some_and(|r| spellings(r).iter().any(|s| wanted.contains(s)))
            })
            .map(|u| u.use_.clone())
            .collect();
        if let Some(old) = old_id {
            out.extend(
                self.uses
                    .iter()
                    .filter(|u| u.use_.kind == ImageUseKind::RunningContainer)
                    .filter(|u| u.id.as_deref() == Some(old))
                    .map(|u| u.use_.clone()),
            );
        }
        out
    }
}

/// One image reference the configuration names.
#[derive(Debug, Clone)]
pub(crate) struct ConfiguredImage {
    pub kind: ImageUseKind,
    pub class: &'static str,
    /// Set for a model override.
    pub model_id: Option<String>,
    /// Trimmed, never empty.
    pub reference: String,
}

/// Every image the configuration names: the four class defaults, then every
/// model's override (the four model tables). A blank one — a model that
/// follows its class — is left out. What [`UsageIndex`] resolves to IDs, and
/// what the registry update check (§8) picks its registry images from.
pub(crate) fn configured_images(snap: &Snapshot) -> Vec<ConfiguredImage> {
    let s = &snap.settings;
    let defaults = [
        ("chat", &s.router.image),
        ("aux", &s.aux_router.image),
        ("audio", &s.audio.image),
        ("image", &s.image.image),
    ]
    .into_iter()
    .map(|(class, image)| (ImageUseKind::ClassDefault, class, None, Some(image)));
    let overrides = snap
        .local_models
        .iter()
        .map(|m| ("chat", &m.model_id, &m.image))
        .chain(
            snap.aux_models
                .iter()
                .map(|m| ("aux", &m.model_id, &m.image)),
        )
        .chain(
            snap.audio_models
                .iter()
                .map(|m| ("audio", &m.model_id, &m.image)),
        )
        .chain(
            snap.image_models
                .iter()
                .map(|m| ("image", &m.model_id, &m.image)),
        )
        .map(|(class, model, image)| {
            (
                ImageUseKind::ModelOverride,
                class,
                Some(model.clone()),
                image.as_ref(),
            )
        });
    defaults
        .chain(overrides)
        .filter_map(|(kind, class, model_id, image)| {
            let reference = image?.trim();
            (!reference.is_empty()).then(|| ConfiguredImage {
                kind,
                class,
                model_id,
                reference: reference.to_string(),
            })
        })
        .collect()
}

/// The `(class, model id)` of the model container called `name`, when its
/// labels do not say: the runtime registry's entry of that name, else the
/// configured model whose container would carry it
/// ([`crate::runtime::container_name`]). `None` for a container that is no
/// model's — an agent's, or something else on the machine.
fn container_owner(
    name: &str,
    registry: &[RuntimeView],
    prefix: &str,
    snap: &Snapshot,
) -> Option<(String, String)> {
    if let Some(v) = registry.iter().find(|v| v.container_name == name) {
        return Some((v.class.as_str().to_string(), v.model_id.clone()));
    }
    let configured = snap
        .local_models
        .iter()
        .map(|m| (Class::Chat, &m.model_id))
        .chain(snap.aux_models.iter().map(|m| (Class::Aux, &m.model_id)))
        .chain(
            snap.audio_models
                .iter()
                .map(|m| (Class::Audio, &m.model_id)),
        )
        .chain(
            snap.image_models
                .iter()
                .map(|m| (Class::Image, &m.model_id)),
        );
    for (class, model) in configured {
        if crate::runtime::container_name(prefix, class, model) == name {
            return Some((class.as_str().to_string(), model.clone()));
        }
    }
    None
}

/// `the chat class default, chat model 'qwen', container lmgw-chat-qwen-a1b2c3`.
pub(crate) fn describe_users(users: &[ImageUse]) -> String {
    users
        .iter()
        .map(|u| match u.kind {
            ImageUseKind::ClassDefault => format!("the {} class default", u.class),
            ImageUseKind::ModelOverride => format!(
                "{} model '{}'",
                u.class,
                u.model_id.as_deref().unwrap_or("?")
            ),
            ImageUseKind::RunningContainer => format!(
                "running container {}",
                u.container.as_deref().unwrap_or("?")
            ),
            ImageUseKind::StoppedContainer => format!(
                "stopped container {}",
                u.container.as_deref().unwrap_or("?")
            ),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a forced delete does about `users`, for the refusal that offers it:
/// the containers it stops and removes, the config users it leaves naming a
/// missing image — only what these users need said.
fn force_effects(users: &[ImageUse]) -> String {
    let containers: Vec<&str> = users
        .iter()
        .filter(|u| is_container(u.kind))
        .filter_map(|u| u.container.as_deref())
        .collect();
    let config: Vec<ImageUse> = users
        .iter()
        .filter(|u| !is_container(u.kind))
        .cloned()
        .collect();
    let mut effects = Vec::new();
    if !containers.is_empty() {
        effects.push(format!("stops and removes {}", containers.join(", ")));
    }
    if !config.is_empty() {
        effects.push(format!(
            "leaves {} naming a missing image",
            describe_users(&config)
        ));
    }
    effects.join(" and ")
}

fn is_container(kind: ImageUseKind) -> bool {
    matches!(
        kind,
        ImageUseKind::RunningContainer | ImageUseKind::StoppedContainer
    )
}

// ---------------------------------------------------------------------------
// Engines
// ---------------------------------------------------------------------------

/// The engine a `dev.lmgw.engine` label names — the engine's own spelling or
/// its image repository's (`sd-server`, as the spike images carry).
fn label_engine(labels: &HashMap<String, String>) -> Option<Engine> {
    let v = labels.get("dev.lmgw.engine")?.trim();
    Engine::parse(v).or(match v {
        "llama-server" | "llama.cpp" => Some(Engine::Llama),
        "audio-cpp" | "audio.cpp" => Some(Engine::Audio),
        "sd-server" | "sd.cpp" => Some(Engine::Sdcpp),
        _ => None,
    })
}

/// The engine an external image's repository name suggests.
fn name_engine(names: &[String]) -> Option<Engine> {
    const LLAMA: [&str; 5] = [
        "llama.cpp",
        "llama-cpp",
        "llamacpp",
        "ik_llama",
        "llama-server",
    ];
    const AUDIO: [&str; 3] = ["audio.cpp", "audiocpp", "audio-cpp"];
    const SD: [&str; 5] = ["stable-diffusion", "sd-server", "sd.cpp", "sdcpp", "sd-cpp"];
    for n in names {
        let repo = n.rsplit_once(':').map_or(n.as_str(), |(r, _)| r);
        let repo = repo.to_ascii_lowercase();
        let hit = |words: &[&str]| words.iter().any(|w| repo.contains(w));
        if hit(&LLAMA) {
            return Some(Engine::Llama);
        }
        if hit(&AUDIO) {
            return Some(Engine::Audio);
        }
        if hit(&SD) {
            return Some(Engine::Sdcpp);
        }
    }
    None
}

fn class_engine(class: &str) -> Option<Engine> {
    match class {
        "chat" | "aux" => Some(Engine::Llama),
        "audio" => Some(Engine::Audio),
        "image" => Some(Engine::Sdcpp),
        _ => None,
    }
}

/// What the image list needs of a local `build_runs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunFacts {
    status: BuildRunStatus,
    slug: String,
    image_id: Option<String>,
}

/// Whether an image was built by another lmgw instance than `instance`:
/// its `dev.lmgw.instance` label names a different one. An image without
/// the label predates it and is judged by [`own_run`]'s image-ID check alone.
fn other_instance(labels: &HashMap<String, String>, instance: &str) -> bool {
    labels
        .get("dev.lmgw.instance")
        .map(|v| v.trim())
        .is_some_and(|v| !v.is_empty() && v != instance)
}

/// The local run that built `image_id`: the one its `dev.lmgw.run` label
/// names, **only if the image is this instance's** ([`other_instance`]) **and
/// only if that run recorded exactly this image**. Run ids are per data dir,
/// the image store is not — an image a dev instance (or an earlier data dir)
/// built carries a run id that here belongs to some other build's run, whose
/// status must not be shown as this image's.
fn own_run<'a>(
    labels: &HashMap<String, String>,
    image_id: &str,
    runs: &'a HashMap<i64, RunFacts>,
    instance: &str,
) -> Option<&'a RunFacts> {
    if other_instance(labels, instance) {
        return None;
    }
    let id = labels.get("dev.lmgw.run")?.parse::<i64>().ok()?;
    let image_id = image_id.trim_start_matches("sha256:");
    runs.get(&id).filter(|r| {
        r.image_id
            .as_deref()
            .is_some_and(|built| built.trim_start_matches("sha256:") == image_id)
    })
}

/// An image's provenance from its labels, with its build's current name
/// when that build still exists. `builds` is `id → (slug, name)`; a row with
/// the label's id but another slug is a different build that inherited a
/// reused id, not this image's. `own` is [`own_run`].
///
/// An image another instance built ([`other_instance`]) is described by its
/// labels alone: its build and run ids are that instance's, so they are
/// reported as 0 (and no build name) rather than pointing at whatever build
/// and run carry those ids here, and `other_instance` says why.
fn provenance(
    labels: &HashMap<String, String>,
    own: Option<&RunFacts>,
    builds: &HashMap<i64, (String, String)>,
    instance: &str,
) -> ImageProvenance {
    let get = |k: &str| labels.get(k).cloned().unwrap_or_default();
    let foreign = other_instance(labels, instance);
    let run_id = if foreign {
        0
    } else {
        get("dev.lmgw.run").parse::<i64>().unwrap_or(0)
    };
    let slug = labels
        .get("dev.lmgw.slug")
        .cloned()
        .or_else(|| own.map(|r| r.slug.clone()))
        .unwrap_or_default();
    let build_id = if foreign {
        0
    } else {
        get("dev.lmgw.build").parse().unwrap_or(0)
    };
    let build_name = builds
        .get(&build_id)
        .filter(|_| !foreign)
        .filter(|(s, _)| slug.is_empty() || *s == slug)
        .map(|(s, name)| {
            if name.trim().is_empty() {
                s.clone()
            } else {
                name.clone()
            }
        });
    ImageProvenance {
        build_id,
        build_name,
        run_id,
        slug,
        repo: get("dev.lmgw.repo"),
        git_ref: get("dev.lmgw.ref"),
        base: get("dev.lmgw.base"),
        extras: serde_json::from_str::<Vec<ResolvedExtra>>(&get("dev.lmgw.extras"))
            .unwrap_or_default(),
        instance: get("dev.lmgw.instance"),
        other_instance: foreign,
    }
}

// ---------------------------------------------------------------------------
// The ops
// ---------------------------------------------------------------------------

/// `container_images` (§15): every local image of the three engines — or of
/// `engine` — newest first, one per ID, with provenance (and its build's
/// current name), run status, users and, for a registry image in use, the
/// last registry update check (§8, the stored result: no registry is asked
/// here) compared with the digests podman holds for it now — plus the disk
/// footer unless `disk` is false.
pub async fn list_images(
    state: &SharedState,
    engine: Option<Engine>,
    disk: bool,
) -> Result<ContainerImagesResponse, String> {
    let usage = UsageIndex::load(state).await?;
    let updates = state.builds.updates().loaded(state).await;
    let runs: HashMap<i64, RunFacts> = store::list_build_runs(&state.db, None, 0)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| {
            (
                r.id,
                RunFacts {
                    status: r.status,
                    slug: r.slug,
                    image_id: r.image_id,
                },
            )
        })
        .collect();
    let builds: HashMap<i64, (String, String)> = store::list_builds(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|b| (b.id, (b.spec.slug, b.spec.name)))
        .collect();
    // An image a class runs is that class's engine's, whatever it is called.
    let class_engines: HashMap<String, Engine> = usage
        .uses
        .iter()
        .filter_map(|u| Some((u.id.clone()?, class_engine(&u.use_.class)?)))
        .collect();
    let instance = state.builds.instance_id();
    let mut images: Vec<ContainerImage> = Vec::new();
    for img in &usage.images {
        let lmgw = img.labels.contains_key("dev.lmgw.run");
        let Some(eng) = label_engine(&img.labels)
            .or_else(|| name_engine(&img.names))
            .or_else(|| class_engines.get(&img.id).copied())
        else {
            continue;
        };
        if engine.is_some_and(|e| e != eng) {
            continue;
        }
        let own = own_run(&img.labels, &img.id, &runs, instance);
        images.push(ContainerImage {
            id: img.id.clone(),
            tags: img.names.clone(),
            engine: Some(eng),
            backend: img.labels.get("dev.lmgw.backend").cloned(),
            size: img.size,
            created: img.created.clone(),
            provenance: lmgw.then(|| provenance(&img.labels, own, &builds, instance)),
            external: !lmgw,
            used_by: usage.users_of(&img.id),
            run_status: own.map(|r| r.status),
            registry_update: updates.registry_update(img),
        });
    }
    images.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    Ok(ContainerImagesResponse {
        images,
        disk: if disk {
            disk_info(state).await
        } else {
            DiskInfo::default()
        },
        disk_skipped: !disk,
    })
}

/// `container_image_delete` (§15): remove `image` (a tag or an ID). A tag of
/// an image that has others is only untagged, and refused while a class
/// default or model override names that tag; the last tag, or an ID, removes
/// the image, refused while anything uses it. The refusal lists the users and
/// what `force` would do about them; with `force` it is done: every container
/// on the image is stopped and removed first — a model's through the runtime,
/// so its registry entry goes with it — and the config users are left naming
/// a missing image, listed in the answer. Refused outright on a dev instance
/// (§10).
pub async fn delete_image(
    state: &SharedState,
    image: &str,
    force: bool,
) -> Result<ContainerImageDeleteResponse, String> {
    state.refuse_in_dev("deleting an image")?;
    let usage = UsageIndex::load(state).await?;
    let (img, name) = resolve(image, &usage.images)
        .ok_or_else(|| format!("there is no image '{image}' on this machine"))?;
    let untag_only = name.is_some() && img.names.len() > 1;
    let users = match (&name, untag_only) {
        (Some(n), true) => usage.users_of_name(n),
        _ => usage.users_of(&img.id),
    };
    if !users.is_empty() && !force {
        return Err(format!(
            "'{image}' is in use by {}. Deleting it anyway (force) {}.",
            describe_users(&users),
            force_effects(&users)
        ));
    }
    let podman = Podman::of(state);
    let mut removed_containers: Vec<String> = Vec::new();
    let so_far = |removed: &[String]| {
        if removed.is_empty() {
            String::new()
        } else {
            format!(" (already removed: {})", removed.join(", "))
        }
    };
    // The containers first: podman refuses to remove an image a container
    // still uses. A model's is found by its container name, never by its
    // labels alone — a dev instance's container carries the same model.
    let registry = state.runtime().list();
    for u in users.iter().filter(|u| is_container(u.kind)) {
        let Some(container) = u.container.as_deref() else {
            continue;
        };
        if let Some(v) = registry.iter().find(|v| v.container_name == container) {
            state
                .runtime()
                .stop(v.class, &v.model_id, true)
                .await
                .map_err(|e| {
                    format!(
                        "stopping {container} failed: {e}{}",
                        so_far(&removed_containers)
                    )
                })?;
        }
        podman
            .rm_force(&[container.to_string()])
            .await
            .map_err(|e| {
                format!(
                    "removing container {container} failed: {e}{}",
                    so_far(&removed_containers)
                )
            })?;
        removed_containers.push(container.to_string());
    }
    let refs: Vec<&str> = match &name {
        Some(n) if untag_only => vec![n.as_str()],
        // The whole image: every name it has (an ID with several tags is
        // refused by a plain `rmi`, and `--force` would take containers too).
        _ if !img.names.is_empty() => img.names.iter().map(String::as_str).collect(),
        _ => vec![img.id.as_str()],
    };
    let removed = podman
        .rmi(&refs)
        .await
        .map_err(|e| format!("{e}{}", so_far(&removed_containers)))?;
    Ok(ContainerImageDeleteResponse {
        removed,
        removed_containers,
        still_named_by: users
            .into_iter()
            .filter(|u| !is_container(u.kind))
            .collect(),
    })
}

/// A reference `podman tag` may write: non-empty, no whitespace or control
/// characters, not an option.
fn validate_reference(what: &str, r: &str) -> Result<(), String> {
    if r.is_empty() || r.starts_with('-') || r.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{what} '{r}' is not an image reference"));
    }
    Ok(())
}

/// On a dev instance, refuse `verb`ing `name` unless it is in the dev
/// namespace ([`tags::is_dev_name`]): a name in production's store is not a
/// dev instance's to add, move or take away.
fn refuse_prod_name_in_dev(state: &SharedState, verb: &str, name: &str) -> Result<(), String> {
    if state.dev() && !tags::is_dev_name(name) {
        return Err(format!(
            "{verb} '{name}' is refused on a dev instance (LMGW_DEV is set): it only touches \
             names under {}…, never production's",
            tags::DEV_REPO_PREFIX
        ));
    }
    Ok(())
}

/// `container_image_tag` (§15): add and/or remove one tag on `image`, then
/// report its tags. Everything is checked before anything changes, so a
/// refused half never leaves the other half applied:
///
/// - adding a name that names **another** image moves it — refused while
///   anything follows that name (a class default or model override naming
///   it, or a container running the image it names now), with the list;
/// - removing is refused while a class default or model override names that
///   tag;
/// - on a dev instance, both only for names in the dev namespace.
pub async fn tag_image(
    state: &SharedState,
    image: &str,
    add: Option<&str>,
    remove: Option<&str>,
) -> Result<ContainerImageTagResponse, String> {
    let add = add.map(str::trim).filter(|s| !s.is_empty());
    let remove = remove.map(str::trim).filter(|s| !s.is_empty());
    if add.is_none() && remove.is_none() {
        return Err("nothing to do: give a tag to add or one to remove".into());
    }
    let usage = UsageIndex::load(state).await?;
    let (img, _) = resolve(image, &usage.images)
        .ok_or_else(|| format!("there is no image '{image}' on this machine"))?;
    let id = img.id.clone();
    if let Some(t) = add {
        validate_reference("tag", t)?;
        refuse_prod_name_in_dev(state, "adding the tag", t)?;
        if let Some((other, Some(name))) = resolve(t, &usage.images) {
            if other.id != id {
                let users = usage.followers(&name, Some(&other.id));
                if !users.is_empty() {
                    return Err(format!(
                        "'{name}' names image {} now, and moving it here would switch {} to \
                         another image — point those elsewhere (or stop the containers) first",
                        short_id(&other.id),
                        describe_users(&users)
                    ));
                }
            }
        }
    }
    let untag = match remove {
        None => None,
        Some(t) => {
            refuse_prod_name_in_dev(state, "removing the tag", t)?;
            let name = match resolve(t, &usage.images) {
                Some((i, Some(n))) if i.id == id => n,
                _ => return Err(format!("'{t}' is not a tag of '{image}'")),
            };
            let users = usage.users_of_name(&name);
            if !users.is_empty() {
                return Err(format!(
                    "'{name}' is named by {} — point those at another image first",
                    describe_users(&users)
                ));
            }
            Some(name)
        }
    };
    let podman = Podman::of(state);
    if let Some(t) = add {
        podman.tag(&id, t).await?;
    }
    if let Some(name) = untag {
        podman.untag(&id, &name).await?;
    }
    Ok(ContainerImageTagResponse {
        tags: podman.repo_tags(&id).await?,
    })
}

/// One row of `podman system df --format json`.
#[derive(Debug, Deserialize)]
struct DfRow {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "RawSize", default)]
    raw_size: u64,
    #[serde(rename = "RawReclaimable", default)]
    raw_reclaimable: u64,
}

/// The Images tab footer (§9.1, §11.3): `podman system df` totals, and the
/// build leftovers still on disk — `/var/tmp/buildahNNN` directories and
/// buildah `Storage` containers — listed for information only. Nothing is
/// removed here. A podman that cannot answer leaves the totals at zero (and
/// says so in the log); the footer is not worth failing the list for.
pub async fn disk_info(state: &SharedState) -> DiskInfo {
    let podman = Podman::of(state);
    let mut disk = DiskInfo::default();
    match podman.run_ok(&["system", "df", "--format", "json"]).await {
        Ok(out) => match serde_json::from_str::<Vec<DfRow>>(&out.stdout) {
            Ok(rows) => {
                if let Some(r) = rows.iter().find(|r| r.kind == "Images") {
                    disk.images_total = r.raw_size;
                    disk.reclaimable = r.raw_reclaimable;
                }
            }
            Err(e) => tracing::warn!("podman system df returned unreadable JSON: {e}"),
        },
        Err(e) => tracing::warn!("disk footer: {e}"),
    }
    disk.buildah_orphans = buildah_dirs(&state.builds.buildah_tmp())
        .into_iter()
        .map(|d| d.display().to_string())
        .collect();
    match ps_rows(&podman, &["-a", "--external"]).await {
        Ok(rows) => disk.buildah_orphans.extend(
            rows.into_iter()
                .filter(|r| {
                    r.state.eq_ignore_ascii_case("storage")
                        || r.status.eq_ignore_ascii_case("storage")
                })
                .map(|r| {
                    let name = r
                        .names
                        .and_then(|n| n.into_iter().next())
                        .unwrap_or_default();
                    format!("buildah container {name} ({})", short_id(&r.id))
                }),
        ),
        Err(e) => tracing::warn!("disk footer: {e}"),
    }
    disk
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(id: &str, names: &[&str]) -> LocalImage {
        LocalImage {
            id: id.into(),
            names: names.iter().map(|n| n.to_string()).collect(),
            ..LocalImage::default()
        }
    }

    #[test]
    fn references_resolve_by_any_local_spelling_or_an_id_prefix() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let images = vec![
            img(&a, &["localhost/llama-server-cuda:official-latest"]),
            img(&b, &["docker.io/library/busybox:latest", "ghcr.io/x/sd:v1"]),
        ];
        let id = |r: &str| resolve(r, &images).map(|(i, _)| i.id.clone());
        assert_eq!(
            id("localhost/llama-server-cuda:official-latest"),
            Some(a.clone())
        );
        assert_eq!(id("llama-server-cuda:official-latest"), Some(a.clone()));
        assert_eq!(id("busybox"), Some(b.clone()));
        assert_eq!(id("ghcr.io/x/sd:v1"), Some(b.clone()));
        assert_eq!(id(&format!("sha256:{}", &a[..12])), Some(a.clone()));
        assert_eq!(id("ghcr.io/x/sd:v2"), None);
        assert_eq!(id("aaaa"), None, "shorter than an ID is a name");
    }

    #[test]
    fn a_digest_reference_with_a_tag_matches_by_its_digest() {
        let a = "a".repeat(64);
        let d = format!("sha256:{}", "d".repeat(64));
        let images = vec![LocalImage {
            id: a.clone(),
            names: vec!["ghcr.io/ggml-org/llama.cpp:server-cuda".into()],
            digests: vec![format!("ghcr.io/ggml-org/llama.cpp@{d}")],
            ..LocalImage::default()
        }];
        let id = |r: &str| resolve(r, &images).map(|(i, _)| i.id.clone());
        assert_eq!(
            id(&format!("ghcr.io/ggml-org/llama.cpp@{d}")),
            Some(a.clone())
        );
        assert_eq!(
            id(&format!("ghcr.io/ggml-org/llama.cpp:server-cuda@{d}")),
            Some(a.clone())
        );
        assert_eq!(
            id(&format!("ghcr.io/ggml-org/llama.cpp:any-tag@{d}")),
            Some(a.clone()),
            "the digest decides, as podman's pull does"
        );
        let other = format!("sha256:{}", "e".repeat(64));
        assert_eq!(
            id(&format!("ghcr.io/ggml-org/llama.cpp:server-cuda@{other}")),
            None,
            "another digest is another image, whatever the tag names here"
        );
        assert_eq!(
            id(&format!("ghcr.io/ggml-org/other:server-cuda@{other}")),
            None
        );
        assert_eq!(
            digest_without_tag("localhost:5000/x/y:t@sha256:1").as_deref(),
            Some("localhost:5000/x/y@sha256:1"),
            "a registry port is not a tag"
        );
        assert_eq!(digest_without_tag("x/y@sha256:1"), None);
        assert_eq!(digest_without_tag("x/y:t"), None);
    }

    #[test]
    fn podmans_row_per_tag_becomes_one_image_per_id() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let row = |id: &str, names: &[&str], digests: &[&str]| LocalImage {
            id: id.into(),
            names: names.iter().map(|n| n.to_string()).collect(),
            digests: digests.iter().map(|d| d.to_string()).collect(),
            ..LocalImage::default()
        };
        let three = ["x:1", "x:2", "x:3"];
        let merged = merge_by_id(
            vec![
                row(&a, &three, &["x@sha256:1"]),
                row(&b, &["y:1"], &[]),
                row(&a, &three, &["x@sha256:1"]),
                row(&a, &["x:3", "x:4"], &["x@sha256:2"]),
            ]
            .into_iter(),
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, a);
        assert_eq!(merged[0].names, ["x:1", "x:2", "x:3", "x:4"]);
        assert_eq!(merged[0].digests, ["x@sha256:1", "x@sha256:2"]);
        assert_eq!(merged[1].names, ["y:1"]);
    }

    /// Found in the container-builds e2e: a dev instance's run 1 built
    /// `official-master`; on production (or any other data dir) run 1 is some
    /// other build's, and its status must not become this image's.
    #[test]
    fn run_status_comes_only_from_the_run_that_built_the_image() {
        let mine = "a".repeat(64);
        let other = "b".repeat(64);
        let mut labels = HashMap::new();
        labels.insert("dev.lmgw.run".to_string(), "1".to_string());
        labels.insert("dev.lmgw.slug".to_string(), "official-master".to_string());
        let run = |status, slug: &str, image: Option<&str>| RunFacts {
            status,
            slug: slug.into(),
            image_id: image.map(str::to_string),
        };
        let mut runs = HashMap::new();
        runs.insert(1, run(BuildRunStatus::Broken, "ik-main", Some(&other)));
        assert_eq!(
            own_run(&labels, &mine, &runs, "i1"),
            None,
            "another image's run 1"
        );
        runs.insert(1, run(BuildRunStatus::Failed, "ik-main", None));
        assert_eq!(
            own_run(&labels, &mine, &runs, "i1"),
            None,
            "a run that built nothing"
        );
        runs.insert(
            1,
            run(
                BuildRunStatus::Succeeded,
                "official-master",
                Some(&format!("sha256:{mine}")),
            ),
        );
        assert_eq!(
            own_run(&labels, &mine, &runs, "i1").map(|r| r.status),
            Some(BuildRunStatus::Succeeded)
        );
        assert_eq!(
            own_run(&HashMap::new(), &mine, &runs, "i1"),
            None,
            "unlabelled"
        );

        // The slug fallback (an image without the slug label) obeys the same
        // rule: never another image's run.
        labels.remove("dev.lmgw.slug");
        runs.insert(1, run(BuildRunStatus::Succeeded, "ik-main", Some(&other)));
        let p = provenance(
            &labels,
            own_run(&labels, &mine, &runs, "i1"),
            &HashMap::new(),
            "i1",
        );
        assert_eq!(p.run_id, 1, "the label is still reported as it is");
        assert_eq!(p.slug, "");
    }

    /// Two instances share the image store: another one's run 1 is not ours,
    /// even when our run 1 recorded the same image ID (a copied database).
    #[test]
    fn another_instances_image_is_described_by_its_labels_alone() {
        let id = "a".repeat(64);
        let mut labels = HashMap::new();
        for (k, v) in [
            ("dev.lmgw.instance", "other"),
            ("dev.lmgw.run", "1"),
            ("dev.lmgw.build", "3"),
            ("dev.lmgw.slug", "official-master"),
        ] {
            labels.insert(k.to_string(), v.to_string());
        }
        let mut runs = HashMap::new();
        runs.insert(
            1,
            RunFacts {
                status: BuildRunStatus::Succeeded,
                slug: "official-master".into(),
                image_id: Some(id.clone()),
            },
        );
        let mut builds = HashMap::new();
        builds.insert(3, ("official-master".to_string(), "mine".to_string()));
        assert_eq!(own_run(&labels, &id, &runs, "mine1"), None);
        let p = provenance(&labels, None, &builds, "mine1");
        assert!(p.other_instance);
        assert_eq!((p.build_id, p.run_id, p.build_name), (0, 0, None));
        assert_eq!(p.slug, "official-master");
        assert_eq!(p.instance, "other");
        // Our own label: trusted as before.
        labels.insert("dev.lmgw.instance".into(), "mine1".into());
        let own = own_run(&labels, &id, &runs, "mine1");
        assert_eq!(own.map(|r| r.status), Some(BuildRunStatus::Succeeded));
        let p = provenance(&labels, own, &builds, "mine1");
        assert!(!p.other_instance);
        assert_eq!((p.build_id, p.run_id), (3, 1));
        assert_eq!(p.build_name.as_deref(), Some("mine"));
    }

    #[test]
    fn engines_come_from_labels_then_names() {
        let mut labels = HashMap::new();
        labels.insert("dev.lmgw.engine".to_string(), "sd-server".to_string());
        assert_eq!(label_engine(&labels), Some(Engine::Sdcpp));
        labels.insert("dev.lmgw.engine".to_string(), "llama".to_string());
        assert_eq!(label_engine(&labels), Some(Engine::Llama));
        let n = |s: &str| name_engine(&[s.to_string()]);
        assert_eq!(
            n("localhost/llama-server-cuda:ik-latest"),
            Some(Engine::Llama)
        );
        assert_eq!(
            n("ghcr.io/ggml-org/llama.cpp:server-cuda"),
            Some(Engine::Llama)
        );
        assert_eq!(n("ghcr.io/0xshug0/audio.cpp:cuda"), Some(Engine::Audio));
        assert_eq!(
            n("ghcr.io/leejet/stable-diffusion.cpp:cuda"),
            Some(Engine::Sdcpp)
        );
        assert_eq!(n("docker.io/library/postgres:17-alpine"), None);
    }
}
