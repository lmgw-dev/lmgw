//! The image picker (container-builds §9.1): the four class-image fields in
//! Settings → Runtimes and the four per-model "Image override" fields.
//!
//! The stored value stays a plain image string, so the control is a text
//! field first. Anything typed is a valid value: Enter or leaving the field
//! commits it, Esc puts the stored value back. Under the field opens a list of
//! what is on this machine for the class's engine: the builds lmgw made (each
//! build's moving tag first, "follows <build>", then its runs' immutable tags,
//! "pinned"), then every other local image of that engine. The list is fetched
//! when it opens, and once on mount for the status line. It never polls.
//!
//! Under the field a status line says what the value is on this machine, or
//! the inherited default when the value is empty: local, missing, broken, not
//! GPU-verified. It warns when the image belongs to another engine, or was
//! built for another GPU backend than the class runs.
//!
//! Deliberately not a generalized `ModelPicker`, which is tied to the model
//! catalog.

use std::sync::atomic::{AtomicU32, Ordering};

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{BuildRunStatus, ContainerImage, Engine};

use super::filter_words;
use super::popover::{self, Popover};
use crate::fmt::human_bytes;
use crate::pages::backends::{
    ago, engine_label, is_dev_tag, is_moving_tag, parse_ts, rebuild_tag_run, short_repo,
};

/// On mount, a list younger than this is good enough for the status line.
const MOUNT_FRESH_SECS: f64 = 30.0;
/// On open, the list is fetched again unless it is younger than this: one
/// fetch per open, not one for the mount and another for a click right after.
const OPEN_FRESH_SECS: f64 = 2.0;

// ---------------------------------------------------------------------------
// The pure part
// ---------------------------------------------------------------------------

/// The lmgw class an image field is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageClass {
    Chat,
    Aux,
    Audio,
    Image,
}

impl ImageClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Aux => "aux",
            Self::Audio => "audio",
            Self::Image => "image",
        }
    }

    /// The engine whose images the class runs (§3): chat and aux run
    /// llama-server, audio audio.cpp, image sd-server.
    pub fn engine(self) -> Engine {
        match self {
            Self::Chat | Self::Aux => Engine::Llama,
            Self::Audio => Engine::Audio,
            Self::Image => Engine::Sdcpp,
        }
    }
}

/// Where a suggested image comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A build's moving tag: picking it follows the build.
    Follows,
    /// A run's immutable tag: picking it pins that exact image.
    Pinned,
    /// Pulled from a registry.
    Registry,
    /// Anything else lmgw did not build (a `build.sh` tag).
    External,
}

impl Origin {
    fn built(self) -> bool {
        matches!(self, Self::Follows | Self::Pinned)
    }
}

/// One row of the list: an image reference to store, and what it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub image_ref: String,
    /// The image the reference names: one image carries several rows (its
    /// moving tag and its own), and the list's count is of images.
    pub image_id: String,
    pub origin: Origin,
    /// "follows Official master", "pinned · abc1234 · 2026-09-26",
    /// "registry".
    pub label: String,
    /// The build's name, for lmgw-built rows: a pinned row is found by it too.
    pub build: Option<String>,
    pub size: u64,
    pub created: String,
    pub run_status: Option<BuildRunStatus>,
}

impl Suggestion {
    fn matches(&self, words: &[String]) -> bool {
        let hay = format!(
            "{} {} {}",
            self.image_ref,
            self.label,
            self.build.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    }
}

/// The registry host an image reference names: its first path component,
/// when that looks like a host (a dot, a port, or `localhost`).
pub fn registry_of(r: &str) -> Option<&str> {
    let (first, _) = r.trim().split_once('/')?;
    (first.contains('.') || first.contains(':') || first == "localhost").then_some(first)
}

/// How podman would get an image that is not on the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    /// It names a registry: podman pulls it at start.
    Registry,
    /// A short name: podman tries to resolve it against its configured
    /// registries, which may or may not work.
    ShortName,
    /// `localhost/…`: there is nothing to pull it from.
    Never,
}

pub fn pull_of(r: &str) -> Pull {
    match registry_of(r) {
        Some("localhost") => Pull::Never,
        Some(_) => Pull::Registry,
        None => Pull::ShortName,
    }
}

/// The spellings podman may list `r` under: with the implicit `:latest`,
/// and a short name under `localhost/` and Docker Hub.
fn ref_forms(r: &str) -> Vec<String> {
    let v = r.trim().to_string();
    let last = v.rsplit('/').next().unwrap_or_default();
    let tagged = if v.contains('@') || last.contains(':') {
        v.clone()
    } else {
        format!("{v}:latest")
    };
    let mut forms = vec![v, tagged.clone()];
    if registry_of(&tagged).is_none() {
        forms.push(format!("localhost/{tagged}"));
        forms.push(if tagged.contains('/') {
            format!("docker.io/{tagged}")
        } else {
            format!("docker.io/library/{tagged}")
        });
    }
    forms.dedup();
    forms
}

/// The local image a stored reference names: by tag in any of its
/// spellings, or by (a prefix of at least 12 characters of) its ID.
pub fn find_image<'a>(r: &str, images: &'a [ContainerImage]) -> Option<&'a ContainerImage> {
    let v = r.trim();
    if v.is_empty() {
        return None;
    }
    let forms = ref_forms(v);
    if let Some(img) = images
        .iter()
        .find(|i| i.tags.iter().any(|t| forms.contains(t)))
    {
        return Some(img);
    }
    let id = v.strip_prefix("sha256:").unwrap_or(v);
    (id.len() >= 12 && id.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| images.iter().find(|i| i.id.starts_with(id)))
        .flatten()
}

/// What a stored value is on this machine.
#[derive(Clone, Debug, PartialEq)]
pub enum Presence {
    /// On this machine; the status of the run that built it, when lmgw did.
    Local(Option<BuildRunStatus>),
    /// Not on this machine, and how podman would get it.
    Missing(Pull),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValueStatus {
    pub presence: Presence,
    /// Engine and backend mismatches, in words.
    pub warnings: Vec<String>,
}

/// The status line's reading of `value` against the local images. `backend`
/// is the GPU backend the class runs (`audio.backend`), when it has one.
pub fn value_status(
    value: &str,
    images: &[ContainerImage],
    class: ImageClass,
    backend: Option<&str>,
) -> Option<ValueStatus> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let Some(img) = find_image(v, images) else {
        return Some(ValueStatus {
            presence: Presence::Missing(pull_of(v)),
            warnings: Vec::new(),
        });
    };
    let mut warnings = Vec::new();
    let want = class.engine();
    if let Some(e) = img.engine.filter(|e| *e != want) {
        warnings.push(format!(
            "{} image; the {} class runs {}",
            engine_label(e),
            class.as_str(),
            engine_label(want)
        ));
    }
    let want_be = backend.map(str::trim).filter(|b| !b.is_empty());
    let have_be = img
        .backend
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if let (Some(w), Some(h)) = (want_be, have_be) {
        if !w.eq_ignore_ascii_case(h) {
            warnings.push(format!("built for {h}; the class backend is {w}"));
        }
    }
    Some(ValueStatus {
        presence: Presence::Local(img.run_status),
        warnings,
    })
}

/// A status chip: (class, label, what it means).
pub fn presence_chip(p: &Presence) -> (&'static str, &'static str, &'static str) {
    match p {
        Presence::Local(Some(BuildRunStatus::Broken)) => (
            "chip err ip-chip",
            "broken",
            "built here, but a verify check failed",
        ),
        Presence::Local(Some(BuildRunStatus::Unverified)) => (
            "chip warn ip-chip",
            "not GPU-verified",
            "built here, but never probed on the GPU; Verify now on Backends can finish that",
        ),
        Presence::Local(_) => ("chip ok ip-chip", "local", "on this machine"),
        Presence::Missing(Pull::Registry) => (
            "chip off ip-chip",
            "not local",
            "not present locally; podman will pull it at start",
        ),
        Presence::Missing(Pull::ShortName) => (
            "chip warn ip-chip",
            "not local",
            "not present locally; podman will try to resolve and pull this short name at start",
        ),
        Presence::Missing(Pull::Never) => (
            "chip err ip-chip",
            "missing",
            "not on this machine, and a localhost/ image can't be pulled",
        ),
    }
}

/// A list row's chip: the run's status for an image lmgw built, the source
/// for any other.
fn row_chip(s: &Suggestion) -> Option<(&'static str, &'static str)> {
    match (s.origin, s.run_status) {
        (Origin::Registry, _) => Some(("chip off ip-chip", "registry")),
        (Origin::External, _) => Some(("chip off ip-chip", "external")),
        (_, Some(BuildRunStatus::Succeeded | BuildRunStatus::UpToDate)) => {
            Some(("chip ok ip-chip", "verified"))
        }
        (_, Some(BuildRunStatus::Unverified)) => Some(("chip warn ip-chip", "not GPU-verified")),
        (_, Some(BuildRunStatus::Broken)) => Some(("chip err ip-chip", "broken")),
        (_, Some(BuildRunStatus::Running)) => Some(("chip live ip-chip", "building")),
        (_, Some(BuildRunStatus::Failed | BuildRunStatus::Canceled)) => {
            Some(("chip err ip-chip", "failed"))
        }
        (_, None) => None,
    }
}

/// `YYYY-MM-DD` (UTC) of a server timestamp; the raw text when it does not
/// parse.
pub fn date_of(ts: &str) -> String {
    let Some(secs) = parse_ts(ts) else {
        return ts.to_string();
    };
    // Civil date from days (Howard Hinnant's algorithm).
    let z = (secs as i64).div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn newest_first(a: &&ContainerImage, b: &&ContainerImage) -> std::cmp::Ordering {
    match (parse_ts(&a.created), parse_ts(&b.created)) {
        (Some(x), Some(y)) => y.total_cmp(&x),
        _ => b.created.cmp(&a.created),
    }
    .then_with(|| a.id.cmp(&b.id))
}

/// A tag podman lists that is a usable reference.
fn usable(tag: &str) -> bool {
    !tag.is_empty() && !tag.contains("<none>")
}

/// Every suggestion for `engine`: the builds first, by name, each with its
/// moving tag and then its runs' immutable tags (newest first); then every
/// other local image of the engine, newest first, one row per tag. A build is
/// called by the name the server reads for it (`provenance.build_name`, its
/// slug when it has no name); once the build is deleted, by the slug its
/// labels carry.
pub fn suggestions(images: &[ContainerImage], engine: Engine) -> Vec<Suggestion> {
    // One image per ID. The server lists each image once now; this stays as
    // a harmless guard against an older one that listed it once per tag.
    let mut ids = std::collections::HashSet::new();
    let mine: Vec<&ContainerImage> = images
        .iter()
        .filter(|i| i.engine == Some(engine) && ids.insert(i.id.as_str()))
        .collect();

    // (name, build id, slug, repo, instance, images). The repo is part of
    // the identity for images whose labels name no build (the WP0 spike's),
    // the instance for another instance's (build id 0, named by its labels).
    type BuildGroup<'a> = (String, i64, String, String, String, Vec<&'a ContainerImage>);
    let mut builds: Vec<BuildGroup> = Vec::new();
    for img in &mine {
        let Some(p) = &img.provenance else { continue };
        match builds
            .iter_mut()
            .find(|b| b.1 == p.build_id && b.2 == p.slug && b.3 == p.repo && b.4 == p.instance)
        {
            Some(b) => b.5.push(img),
            None => {
                let name = p
                    .build_name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| (!p.slug.is_empty()).then(|| p.slug.clone()))
                    .or_else(|| (!p.repo.is_empty()).then(|| short_repo(&p.repo)))
                    .unwrap_or_else(|| "unnamed build".to_string());
                builds.push((
                    name,
                    p.build_id,
                    p.slug.clone(),
                    p.repo.clone(),
                    p.instance.clone(),
                    vec![img],
                ));
            }
        }
    }
    builds.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then(a.1.cmp(&b.1))
            .then(a.4.cmp(&b.4))
    });

    let mut out: Vec<Suggestion> = Vec::new();
    for (name, _, slug, _, _, mut imgs) in builds {
        imgs.sort_by(newest_first);
        // The moving tag — production's spelling, or a dev instance's twin in
        // its own namespace (both are lmgw's builds).
        let moving = |t: &str| is_moving_tag(t, engine, &slug);
        for img in &imgs {
            for tag in img.tags.iter().filter(|t| moving(t)) {
                if out.iter().any(|s| &s.image_ref == tag) {
                    continue;
                }
                let dev = if is_dev_tag(tag) {
                    " · dev instance"
                } else {
                    ""
                };
                out.push(Suggestion {
                    image_ref: tag.clone(),
                    image_id: img.id.clone(),
                    origin: Origin::Follows,
                    label: format!("follows {name}{dev}"),
                    build: Some(name.clone()),
                    size: img.size,
                    created: img.created.clone(),
                    run_status: img.run_status,
                });
            }
        }
        for img in &imgs {
            let base = img
                .provenance
                .as_ref()
                .map(|p| p.base.chars().take(7).collect::<String>())
                .unwrap_or_default();
            let when = date_of(&img.created);
            for tag in img.tags.iter().filter(|t| !moving(t) && usable(t)) {
                if out.iter().any(|s| &s.image_ref == tag) {
                    continue;
                }
                // A rebuild that did not verify keeps only its `-r<run>` tag.
                let what = if rebuild_tag_run(tag).is_some() {
                    "unverified rebuild"
                } else {
                    "pinned"
                };
                let label = if base.is_empty() {
                    format!("{what} · {when}")
                } else {
                    format!("{what} · {base} · {when}")
                };
                out.push(Suggestion {
                    image_ref: tag.clone(),
                    image_id: img.id.clone(),
                    origin: Origin::Pinned,
                    label,
                    build: Some(name.clone()),
                    size: img.size,
                    created: img.created.clone(),
                    run_status: img.run_status,
                });
            }
        }
    }

    let mut others: Vec<&ContainerImage> = mine
        .iter()
        .filter(|i| i.provenance.is_none())
        .copied()
        .collect();
    others.sort_by(newest_first);
    for img in others {
        for tag in img.tags.iter().filter(|t| usable(t)) {
            if out.iter().any(|s| &s.image_ref == tag) {
                continue;
            }
            let origin = if pull_of(tag) == Pull::Registry {
                Origin::Registry
            } else {
                Origin::External
            };
            out.push(Suggestion {
                image_ref: tag.clone(),
                image_id: img.id.clone(),
                origin,
                label: String::new(),
                build: None,
                size: img.size,
                created: img.created.clone(),
                run_status: img.run_status,
            });
        }
    }
    out
}

/// How many images the rows are: a build's image is listed under its
/// moving tag and its own immutable tag, and counts once.
pub fn image_count(all: &[Suggestion]) -> usize {
    all.iter()
        .map(|s| s.image_id.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// The DOM id of a list item, for `aria-activedescendant`: the list's id and
/// the item's, with everything outside `[A-Za-z0-9-]` spelled `_xx` (hex of
/// each byte), so two references never share an id.
pub fn option_id(list_id: &str, nav_id: &str) -> String {
    let mut out = format!("{list_id}-");
    for b in nav_id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

/// One line of the open list. Every item renders exactly one element, so
/// the keyboard's position is also a child index of the list.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// The typed text itself, as a value of its own.
    Custom(String),
    Head {
        label: &'static str,
        count: usize,
    },
    Row(Suggestion),
}

impl Item {
    fn nav_id(&self) -> String {
        match self {
            Item::Custom(_) => "c:".to_string(),
            Item::Head { label, .. } => format!("h:{label}"),
            Item::Row(s) => format!("r:{}", s.image_ref),
        }
    }

    fn render_key(&self) -> String {
        match self {
            Item::Custom(q) => format!("c:{q}"),
            Item::Head { label, count } => format!("h:{label}:{count}"),
            Item::Row(s) => format!("r:{}:{:?}", s.image_ref, s.run_status),
        }
    }

    fn navigable(&self) -> bool {
        !matches!(self, Item::Head { .. })
    }
}

/// The open list for `query` (empty = everything): the typed text first
/// when it is not itself a suggestion, then the builds, then the other
/// images. Every word of the query has to match a row.
pub fn build_items(all: &[Suggestion], query: &str) -> Vec<Item> {
    let q = query.trim();
    let words = filter_words(q);
    let mut out = Vec::new();
    if !q.is_empty() && !all.iter().any(|s| s.image_ref == q) {
        out.push(Item::Custom(q.to_string()));
    }
    let (built, other): (Vec<&Suggestion>, Vec<&Suggestion>) = all
        .iter()
        .filter(|s| s.matches(&words))
        .partition(|s| s.origin.built());
    for (label, rows) in [("Builds", built), ("Other local images", other)] {
        if rows.is_empty() {
            continue;
        }
        out.push(Item::Head {
            label,
            count: rows.len(),
        });
        out.extend(rows.into_iter().cloned().map(Item::Row));
    }
    out
}

/// What committing the typed text does: the value to store, or `None` to
/// leave the stored one (and put it back in the box). Text is trimmed; empty
/// clears the value only where empty means something (inherit).
pub fn commit_value(typed: &str, current: &str, clearable: bool) -> Option<String> {
    let t = typed.trim();
    if t == current || (t.is_empty() && !clearable) {
        None
    } else {
        Some(t.to_string())
    }
}

/// Where the keyboard stands after typing `typed`: on the typed text
/// itself, or on the row that is exactly it, so Enter takes what was typed.
/// Nowhere when the box is empty: Enter then commits the empty box (clears
/// an override) instead of taking whatever row happens to be first.
fn typed_active(items: &[Item], typed: &str) -> Option<String> {
    let t = typed.trim();
    if t.is_empty() {
        return None;
    }
    items
        .iter()
        .find(|i| match i {
            Item::Custom(_) => true,
            Item::Row(s) => s.image_ref == t,
            Item::Head { .. } => false,
        })
        .map(Item::nav_id)
}

/// The next navigable item from `from` in the direction given, `steps`
/// times; stays put at either end.
fn step(items: &[Item], from: Option<usize>, down: bool, steps: usize) -> Option<usize> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let mut at = from;
    let mut probe = from;
    let mut moved = 0;
    loop {
        let next = match (probe, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(k), true) if k + 1 < n => k + 1,
            (Some(k), false) if k > 0 => k - 1,
            _ => break,
        };
        probe = Some(next);
        if items[next].navigable() {
            at = Some(next);
            moved += 1;
            if moved == steps {
                break;
            }
        }
    }
    at
}

// ---------------------------------------------------------------------------
// The shared list
// ---------------------------------------------------------------------------

/// The local images, shared by every picker on the page: four pickers on
/// Settings are one fetch, not four. Each image's build name comes with it
/// (`provenance.build_name`), so "follows <build>" needs no second call.
#[derive(Clone, Copy)]
pub struct ImageList {
    images: RwSignal<Option<Vec<ContainerImage>>>,
    /// The last fetch's failure; `images` keeps what the one before got.
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    fetched_at: StoredValue<f64>,
    generation: StoredValue<u64>,
}

impl ImageList {
    fn new() -> Self {
        Self {
            images: RwSignal::new(None),
            error: RwSignal::new(None),
            loading: RwSignal::new(false),
            fetched_at: StoredValue::new(0.0),
            generation: StoredValue::new(0),
        }
    }

    fn refresh(self) {
        let gen = self.generation.get_value() + 1;
        self.generation.set_value(gen);
        self.loading.set(true);
        spawn_local(async move {
            // No disk footer: the picker shows none, and it is most of the
            // op's time.
            let res = crate::backends_api::container_images(false).await;
            if self.generation.try_get_value() != Some(gen) {
                return;
            }
            match res {
                Ok(r) => {
                    self.images.set(Some(r.images));
                    self.error.set(None);
                    self.fetched_at.set_value(js_sys::Date::now());
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
            self.loading.set(false);
        });
    }

    fn refresh_if_older(self, secs: f64) {
        let Some(at) = self.fetched_at.try_get_value() else {
            return;
        };
        if !self.loading.get_untracked() && js_sys::Date::now() - at > secs * 1000.0 {
            self.refresh();
        }
    }
}

/// Install the shared list. Nothing is fetched until a picker mounts.
pub fn provide_image_list() {
    provide_context(ImageList::new());
}

fn use_image_list() -> ImageList {
    use_context::<ImageList>().unwrap_or_else(ImageList::new)
}

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// The widget
// ---------------------------------------------------------------------------

/// An image-reference field with the local images of the class's engine
/// under it.
///
/// Keyboard: typing filters the list and offers the text itself first; ↓/↑
/// open the list and move in it, PgUp/PgDn too; Enter takes the highlighted
/// row, or the typed text; Esc closes the list, and a second Esc puts the
/// stored value back. Leaving the field commits what it holds. A commit
/// fires a bubbling `change` from the widget, so a guarded modal notices it
/// like any native control.
#[component]
pub fn ImagePicker(
    /// The stored image string, written on commit. Empty = inherit.
    value: RwSignal<String>,
    /// The class the image runs in: whose engine's images are suggested.
    class: ImageClass,
    /// Shown while the field is empty: the class default an override
    /// inherits. The status line reads it then.
    #[prop(optional, into)]
    placeholder: MaybeProp<String>,
    /// Empty is a value (inherit): a clear button, and committing an empty
    /// field clears it. Otherwise an emptied field puts the value back.
    #[prop(optional)]
    clearable: bool,
    /// The GPU backend the class runs (`audio.backend`): an image built for
    /// another one gets a warning.
    #[prop(optional, into)]
    backend: MaybeProp<String>,
    /// Called with every committed value, after `value` is set.
    #[prop(optional)]
    on_change: Option<Callback<String>>,
    /// Whether the field is on screen. A picker mounted hidden (a Settings
    /// category not shown, a folded section) fetches nothing for its status
    /// line until it is first shown; unset, it is shown from the start.
    #[prop(optional, into)]
    visible: Option<Signal<bool>>,
) -> impl IntoView {
    let list = use_image_list();
    let engine = class.engine();
    let uid = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let list_id = format!("ip-list-{uid}");

    let open = RwSignal::new(false);
    let text = RwSignal::new(value.get_untracked());
    // The list narrows by the text only once it is typed into: opening on a
    // stored value shows everything.
    let filtering = RwSignal::new(false);
    let active = RwSignal::new(None::<String>);
    let root: NodeRef<html::Div> = NodeRef::new();
    let field: NodeRef<html::Div> = NodeRef::new();
    let input: NodeRef<html::Input> = NodeRef::new();
    let list_el: NodeRef<html::Div> = NodeRef::new();
    // A press on the field or the arrow while the list is open: light
    // dismiss closes it on that press, and the click must not reopen it.
    let was_open = StoredValue::new(false);

    match visible {
        None => list.refresh_if_older(MOUNT_FRESH_SECS),
        // Once, when first shown; nothing is tracked after that.
        Some(shown) => {
            Effect::new(move |fetched: Option<bool>| {
                if fetched == Some(true) {
                    return true;
                }
                let now = shown.get();
                if now {
                    list.refresh_if_older(MOUNT_FRESH_SECS);
                }
                now
            });
        }
    }

    // The stored value moved underneath (Discard, a reload): the box follows.
    Effect::new(move |_| {
        let v = value.get();
        if text.get_untracked() != v {
            text.set(v);
        }
    });

    // Typed but not committed yet (Enter or leaving the field commits): an
    // unsaved change all the same, for the page's dirty guard — a reload, or a
    // link that does not take the focus first — and for a guarded modal,
    // which the typing tells (below).
    let pending = Signal::derive(move || {
        text.with(|t| value.with(|v| commit_value(t, v, clearable).is_some()))
    });
    if let Some(g) = super::dirty_guard::try_use_dirty_guard() {
        g.watch_page("an image field", pending);
    }

    // The field's accessible name is its form label: tied by id once the
    // field is in the page, or said outright when it sits under none.
    let label_id = format!("ip-label-{uid}");
    Effect::new(move |_| {
        if let Some(i) = input.get() {
            name_input(&i, &label_id, class);
        }
    });

    let all = Memo::new(move |_| {
        list.images.with(|imgs| {
            imgs.as_deref()
                .map(|i| suggestions(i, engine))
                .unwrap_or_default()
        })
    });
    let items = Memo::new(move |_| {
        let q = if filtering.get() {
            text.get()
        } else {
            String::new()
        };
        all.with(|a| build_items(a, &q))
    });

    let show = move || {
        filtering.set(false);
        active.set(Some(format!("r:{}", value.get_untracked())));
        open.set(true);
        list.refresh_if_older(OPEN_FRESH_SECS);
    };
    let focus_input = move || {
        if let Some(i) = input.get_untracked() {
            let _ = i.focus();
        }
    };
    let commit = move |v: String| {
        text.set(v.clone());
        if value.get_untracked() == v {
            return;
        }
        value.set(v.clone());
        if let Some(cb) = on_change {
            cb.run(v);
        }
        // From the widget, not the input: the input is `data-untracked`, so
        // its typing does not count as an edit, and a commit does.
        if let Some(r) = root.get_untracked() {
            popover::fire_change(&r);
        }
    };
    // Also run from the field's blur, which fires when a closing modal takes
    // the focused field out of the page, after this widget is disposed.
    let commit_typed = move || {
        let (Some(typed), Some(current)) = (text.try_get_untracked(), value.try_get_untracked())
        else {
            return;
        };
        match commit_value(&typed, &current, clearable) {
            Some(v) => commit(v),
            None => text.set(current),
        }
    };
    let activate = move |it: Item| match it {
        Item::Custom(q) => {
            open.set(false);
            match commit_value(&q, &value.get_untracked(), clearable) {
                Some(v) => commit(v),
                None => text.set(value.get_untracked()),
            }
        }
        Item::Row(s) => {
            open.set(false);
            commit(s.image_ref);
        }
        Item::Head { .. } => {}
    };
    let active_index = move || {
        active.with_untracked(|a| {
            a.as_ref()
                .and_then(|id| items.with_untracked(|v| v.iter().position(|i| &i.nav_id() == id)))
        })
    };
    let move_to = move |idx: usize| {
        let Some(id) = items.with_untracked(|v| v.get(idx).map(Item::nav_id)) else {
            return;
        };
        active.set(Some(id));
        if let Some(l) = list_el.get_untracked() {
            popover::reveal_child(&l, idx);
        }
    };

    let on_key = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        match key.as_str() {
            "ArrowDown" | "ArrowUp" | "PageDown" | "PageUp" => {
                ev.prevent_default();
                if !open.get_untracked() {
                    show();
                    return;
                }
                let down = matches!(key.as_str(), "ArrowDown" | "PageDown");
                let steps = if key.starts_with("Page") {
                    list_el
                        .get_untracked()
                        .map(|l| popover::page_rows(&l))
                        .unwrap_or(8)
                } else {
                    1
                };
                let at = items.with_untracked(|v| step(v, active_index(), down, steps));
                if let Some(i) = at {
                    move_to(i);
                }
            }
            "Enter" => {
                ev.prevent_default();
                let picked = open
                    .get_untracked()
                    .then(|| {
                        active_index().and_then(|i| items.with_untracked(|v| v.get(i).cloned()))
                    })
                    .flatten()
                    .filter(Item::navigable);
                open.set(false);
                match picked {
                    Some(it) => activate(it),
                    None => commit_typed(),
                }
            }
            "Escape" => {
                if open.get_untracked() {
                    // Ours alone: inside a modal, Esc must not close the
                    // modal too.
                    ev.prevent_default();
                    ev.stop_propagation();
                    open.set(false);
                } else if text.get_untracked() != value.get_untracked() {
                    ev.prevent_default();
                    ev.stop_propagation();
                    text.set(value.get_untracked());
                }
            }
            "Tab" => open.set(false),
            _ => {}
        }
    };

    // The status line: the value, or the default it inherits.
    let status = Memo::new(move |_| {
        let v = value.get();
        let (eff, inherited) = if v.trim().is_empty() {
            (placeholder.get().unwrap_or_default(), true)
        } else {
            (v, false)
        };
        let be = backend.get();
        list.images.with(|imgs| {
            imgs.as_deref()
                .and_then(|i| value_status(&eff, i, class, be.as_deref()))
                .map(|s| (s, inherited))
        })
    });
    let status_line = move || {
        if let Some((s, inherited)) = status.get() {
            let (cls, label, what) = presence_chip(&s.presence);
            let note = matches!(s.presence, Presence::Missing(_)).then_some(what);
            return Some(
                view! {
                    <div class="ip-status">
                        {inherited.then(|| view! { <span class="dim">"default"</span> })}
                        <span class=cls title=what>
                            <span class="dot"></span>
                            {label}
                        </span>
                        {note.map(|n| view! { <span class="dim">{n}</span> })}
                        {s
                            .warnings
                            .into_iter()
                            .map(|w| view! { <span class="ip-warn">{w}</span> })
                            .collect_view()}
                    </div>
                }
                .into_any(),
            );
        }
        let err = list.error.get()?;
        list.images.with(Option::is_none).then(|| {
            view! {
                <div class="ip-status dim" title=err>
                    "couldn't list local images"
                </div>
            }
            .into_any()
        })
    };

    let top_line = move || {
        let n = all.with(|a| image_count(a));
        let eng = engine_label(engine);
        match (list.images.with(Option::is_some), list.error.get()) {
            (false, Some(e)) => view! {
                <span class="mp-err" title=e.clone()>
                    "Couldn't list local images. Type any image reference."
                </span>
            }
            .into_any(),
            (false, None) => view! { <span>"Loading local images…"</span> }.into_any(),
            (true, err) => {
                let head = match n {
                    0 => format!("No local {eng} images"),
                    1 => format!("1 local {eng} image"),
                    n => format!("{n} local {eng} images"),
                };
                view! {
                    <span>{head}</span>
                    {list.loading.get().then(|| view! { <span>" · refreshing…"</span> })}
                    {err
                        .map(|e| {
                            view! { <span class="mp-err" title=e>" · couldn't refresh"</span> }
                        })}
                    <span>" · Enter keeps what you type"</span>
                }
                .into_any()
            }
        }
    };

    let list_id_c = list_id.clone();
    // The keyboard's row, for a screen reader: focus stays in the field.
    let active_desc = {
        let lid = list_id.clone();
        move || {
            if !open.get() {
                return None;
            }
            active
                .get()
                .filter(|id| items.with(|v| v.iter().any(|i| i.navigable() && &i.nav_id() == id)))
                .map(|id| option_id(&lid, &id))
        }
    };
    let lid_rows = StoredValue::new(list_id.clone());
    view! {
        <div class="ip" node_ref=root>
            <div class="ip-box" node_ref=field>
                <input
                    class="input mono ip-input"
                    node_ref=input
                    role="combobox"
                    aria-autocomplete="list"
                    aria-expanded=move || open.get().to_string()
                    aria-controls=list_id_c
                    aria-activedescendant=active_desc
                    autocomplete="off"
                    spellcheck="false"
                    data-untracked
                    placeholder=move || placeholder.get().unwrap_or_default()
                    prop:value=move || text.get()
                    on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                    on:click=move |_| {
                        let was = was_open.get_value();
                        was_open.set_value(false);
                        if !was && !open.get_untracked() {
                            show();
                        }
                    }
                    on:input=move |ev| {
                        text.set(event_target_value(&ev));
                        // The input is `data-untracked` (moving in the list
                        // is no edit), so the typing is told from the widget.
                        if pending.get_untracked() {
                            if let Some(r) = root.get_untracked() {
                                fire_input(&r);
                            }
                        }
                        filtering.set(true);
                        if !open.get_untracked() {
                            open.set(true);
                            list.refresh_if_older(OPEN_FRESH_SECS);
                        }
                        // Enter means what was typed, and a row is one ↓
                        // away.
                        active.set(text.with_untracked(|t| items.with_untracked(|v| typed_active(v, t))));
                        if let Some(l) = list_el.get_untracked() {
                            l.set_scroll_top(0);
                        }
                    }
                    on:keydown=on_key
                    on:blur=move |_| {
                        open.try_set(false);
                        commit_typed();
                    }
                />
                <Show when=move || clearable && !text.with(String::is_empty)>
                    <button
                        type="button"
                        class="btn ghost sm ip-btn"
                        title="Clear: inherit the class default"
                        aria-label="Clear"
                        tabindex="-1"
                        // Focus stays in the field: its blur would commit
                        // the text this is about to clear.
                        on:mousedown=|ev| ev.prevent_default()
                        on:click=move |_| {
                            open.set(false);
                            commit(String::new());
                            focus_input();
                        }
                    >
                        "✕"
                    </button>
                </Show>
                <button
                    type="button"
                    class="btn ghost sm ip-btn"
                    title="Local images"
                    aria-label="Show local images"
                    tabindex="-1"
                    on:mousedown=|ev| ev.prevent_default()
                    on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                    on:click=move |_| {
                        let was = was_open.get_value();
                        was_open.set_value(false);
                        if was || open.get_untracked() {
                            open.set(false);
                        } else {
                            show();
                        }
                        focus_input();
                    }
                >
                    "▾"
                </button>
            </div>
            {status_line}
            <Popover open=open anchor=field class="mp-pop ip-pop" min_width=460>
                // Presses in the list keep focus in the field, so picking a
                // row is not also a blur that commits the typed text.
                <div class="pop-inner mp" on:mousedown=|ev| ev.prevent_default()>
                    <div class="mp-count ip-top">{top_line}</div>
                    <div
                        class="pop-list mp-list"
                        role="listbox"
                        aria-label="Local images"
                        id=list_id.clone()
                        node_ref=list_el
                    >
                        <For each=move || items.get() key=Item::render_key let:it>
                            {lid_rows.with_value(|l| render_item(it, l, value, active, activate))}
                        </For>
                        <Show when=move || {
                            list.images.with(Option::is_some) && items.with(Vec::is_empty)
                        }>
                            <div class="pop-empty">
                                {move || {
                                    if all.with(Vec::is_empty) {
                                        "Nothing on this machine yet: type a registry reference, or build one on "
                                            .to_string()
                                    } else {
                                        format!(
                                            "Nothing local matches \u{201c}{}\u{201d}",
                                            text.get().trim(),
                                        )
                                    }
                                }}
                                <Show when=move || all.with(Vec::is_empty)>
                                    // A press here keeps the focus in the
                                    // field (no blur), so what was typed is
                                    // committed first: the page's guard then
                                    // asks about it before leaving.
                                    <a href="/backends" on:mousedown=move |_| commit_typed()>
                                        "Backends"
                                    </a>
                                    "."
                                </Show>
                            </div>
                        </Show>
                    </div>
                </div>
            </Popover>
        </div>
    }
}

/// Tie the field to its form label (`.field > label`, given an id when it
/// has none), else name it after the class.
fn name_input(input: &web_sys::HtmlInputElement, label_id: &str, class: ImageClass) {
    let label = input
        .closest(".field")
        .ok()
        .flatten()
        .and_then(|f| f.query_selector(":scope > label").ok().flatten());
    match label {
        Some(l) => {
            if l.id().is_empty() {
                l.set_id(label_id);
            }
            let _ = input.set_attribute("aria-labelledby", &l.id());
        }
        None => {
            let _ = input.set_attribute("aria-label", &format!("{} image", class.as_str()));
        }
    }
}

/// A bubbling `input` from the widget: a guarded modal counts it as typing.
fn fire_input(el: &web_sys::HtmlElement) {
    let init = web_sys::EventInit::new();
    init.set_bubbles(true);
    if let Ok(ev) = web_sys::Event::new_with_event_init_dict("input", &init) {
        let _ = el.dispatch_event(&ev);
    }
}

fn render_item(
    it: Item,
    list_id: &str,
    value: RwSignal<String>,
    active: RwSignal<Option<String>>,
    activate: impl Fn(Item) + Copy + Send + Sync + 'static,
) -> AnyView {
    let nav = it.nav_id();
    let oid = option_id(list_id, &nav);
    let is_active = {
        let nav = nav.clone();
        move || active.with(|a| a.as_deref() == Some(nav.as_str()))
    };
    let hover = {
        let nav = nav.clone();
        let ok = it.navigable();
        move |_| {
            if ok && active.with_untracked(|a| a.as_deref() != Some(nav.as_str())) {
                active.set(Some(nav.clone()));
            }
        }
    };
    let click = {
        let it = it.clone();
        move |_| activate(it.clone())
    };
    match it {
        Item::Head { label, count } => view! {
            <div class="mp-head ip-head" role="presentation">
                <span>{label}</span>
                <span class="count">{count}</span>
            </div>
        }
        .into_any(),
        Item::Custom(q) => view! {
            <div
                class="mp-row mp-custom"
                class:active=is_active
                role="option"
                id=oid
                aria-selected="false"
                on:pointermove=hover
                on:click=click
            >
                "Use \u{201c}"
                <span class="mp-id">{q}</span>
                "\u{201d} as typed"
            </div>
        }
        .into_any(),
        Item::Row(s) => {
            let sel = {
                let r = s.image_ref.clone();
                move || value.with(|v| *v == r)
            };
            let (repo, tag) = match s.image_ref.rsplit_once(':') {
                Some((repo, tag)) if !tag.contains('/') && !repo.is_empty() => {
                    (format!("{repo}:"), tag.to_string())
                }
                _ => (String::new(), s.image_ref.clone()),
            };
            let chip = row_chip(&s);
            let title = match &s.build {
                Some(b) => format!("{}\nbuild: {b}", s.image_ref),
                None => s.image_ref.clone(),
            };
            let pinned = s.origin == Origin::Pinned;
            let created = ago(&s.created);
            view! {
                <div
                    class="mp-row ip-row"
                    class:ip-pin=pinned
                    class:sel=sel.clone()
                    class:active=is_active
                    role="option"
                    id=oid
                    aria-selected=move || sel().to_string()
                    title=title
                    on:pointermove=hover
                    on:click=click
                >
                    <span class="mp-id">
                        <span class="mp-pfx">{repo}</span>
                        {tag}
                    </span>
                    <span class="ip-meta">
                        {chip.map(|(c, l)| view! { <span class=c>{l}</span> })}
                        {(!s.label.is_empty()).then(|| view! { <span class="ip-label">{s.label.clone()}</span> })}
                        <span>{human_bytes(s.size)}</span>
                        <span>{created}</span>
                    </span>
                </div>
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::ImageProvenance;

    fn built(id: &str, build_id: i64, slug: &str, tags: &[&str], created: &str) -> ContainerImage {
        let name = match build_id {
            1 => Some("Official master".to_string()),
            2 => Some("ik main".to_string()),
            _ => None,
        };
        ContainerImage {
            id: id.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            engine: Some(Engine::Llama),
            backend: Some("cuda".into()),
            size: 1_000_000,
            created: created.into(),
            provenance: Some(ImageProvenance {
                build_id,
                build_name: name,
                slug: slug.into(),
                base: "abcdef1234567".into(),
                ..Default::default()
            }),
            external: false,
            run_status: Some(BuildRunStatus::Succeeded),
            ..Default::default()
        }
    }

    fn external(id: &str, engine: Engine, tags: &[&str], created: &str) -> ContainerImage {
        ContainerImage {
            id: id.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            engine: Some(engine),
            created: created.into(),
            external: true,
            ..Default::default()
        }
    }

    fn sample() -> Vec<ContainerImage> {
        vec![
            // Two runs of "ik-main": the newer one holds the moving tag.
            built(
                "aaa111",
                2,
                "ik-main",
                &["localhost/lmgw-llama-server:ik-main-abc1234-000001"],
                "2026-09-20T10:00:00Z",
            ),
            built(
                "aaa222",
                2,
                "ik-main",
                &[
                    "localhost/lmgw-llama-server:ik-main",
                    "localhost/lmgw-llama-server:ik-main-abc1234-000002",
                ],
                "2026-09-25T10:00:00Z",
            ),
            built(
                "bbb111",
                1,
                "official-master",
                &[
                    "localhost/lmgw-llama-server:official-master",
                    "localhost/lmgw-llama-server:official-master-abc1234-000003",
                ],
                "2026-09-24T10:00:00Z",
            ),
            external(
                "ccc111",
                Engine::Llama,
                &["localhost/llama-server-cuda:full-latest"],
                "2026-09-01T00:00:00Z",
            ),
            external(
                "ccc222",
                Engine::Llama,
                &["ghcr.io/ggml-org/llama.cpp:server-cuda"],
                "2026-09-10T00:00:00Z",
            ),
            external(
                "ddd111",
                Engine::Audio,
                &["localhost/audio-cpp:latest"],
                "2026-09-11T00:00:00Z",
            ),
        ]
    }

    fn refs(s: &[Suggestion]) -> Vec<&str> {
        s.iter().map(|s| s.image_ref.as_str()).collect()
    }

    #[test]
    fn every_class_suggests_its_own_engines_images() {
        assert_eq!(ImageClass::Chat.engine(), Engine::Llama);
        assert_eq!(ImageClass::Aux.engine(), Engine::Llama);
        assert_eq!(ImageClass::Audio.engine(), Engine::Audio);
        assert_eq!(ImageClass::Image.engine(), Engine::Sdcpp);
        let audio = suggestions(&sample(), Engine::Audio);
        assert_eq!(refs(&audio), ["localhost/audio-cpp:latest"]);
        assert!(suggestions(&sample(), Engine::Sdcpp).is_empty());
    }

    #[test]
    fn builds_come_first_by_name_moving_tag_then_newest_pins() {
        let s = suggestions(&sample(), Engine::Llama);
        assert_eq!(
            refs(&s),
            [
                // "ik main" sorts before "Official master" (case-insensitive)
                "localhost/lmgw-llama-server:ik-main",
                "localhost/lmgw-llama-server:ik-main-abc1234-000002",
                "localhost/lmgw-llama-server:ik-main-abc1234-000001",
                "localhost/lmgw-llama-server:official-master",
                "localhost/lmgw-llama-server:official-master-abc1234-000003",
                // then the other images, newest first
                "ghcr.io/ggml-org/llama.cpp:server-cuda",
                "localhost/llama-server-cuda:full-latest",
            ]
        );
        assert_eq!(s[0].origin, Origin::Follows);
        assert_eq!(s[0].label, "follows ik main");
        assert_eq!(s[1].origin, Origin::Pinned);
        assert_eq!(s[1].label, "pinned · abcdef1 · 2026-09-25");
        assert_eq!(s[5].origin, Origin::Registry);
        assert_eq!(s[6].origin, Origin::External);
    }

    #[test]
    fn a_deleted_build_is_called_by_its_slug() {
        // The server names a build without a name by its slug itself; a
        // deleted build's images carry no name at all, only their labels.
        let mut imgs = sample();
        for i in &mut imgs {
            if let Some(p) = i.provenance.as_mut().filter(|p| p.build_id == 2) {
                p.build_name = None;
            }
        }
        let s = suggestions(&imgs, Engine::Llama);
        assert_eq!(s[0].label, "follows ik-main");
        assert!(s.iter().any(|x| x.label == "follows Official master"));
        // an empty name is no name
        imgs[2].provenance.as_mut().unwrap().build_name = Some(" ".into());
        let s = suggestions(&imgs, Engine::Llama);
        assert!(s.iter().any(|x| x.label == "follows official-master"));
    }

    #[test]
    fn an_image_listed_once_per_tag_is_one_image() {
        let mut imgs = sample();
        let dup = imgs[3].clone();
        imgs.push(dup);
        let s = suggestions(&imgs, Engine::Llama);
        assert_eq!(
            s.iter()
                .filter(|x| x.image_ref == "localhost/llama-server-cuda:full-latest")
                .count(),
            1
        );
    }

    #[test]
    fn labels_without_a_build_are_named_by_their_repository() {
        let mut spike = built(
            "fff",
            0,
            "",
            &["localhost/lmgw-spike:ik"],
            "2026-09-26T01:00:00Z",
        );
        spike.provenance.as_mut().unwrap().repo =
            "https://github.com/ikawrakow/ik_llama.cpp".into();
        let mut other = built(
            "ggg",
            0,
            "",
            &["localhost/lmgw-spike:official"],
            "2026-09-26T02:00:00Z",
        );
        other.provenance.as_mut().unwrap().repo = "https://github.com/ggml-org/llama.cpp".into();
        let s = suggestions(&[spike, other], Engine::Llama);
        let builds: Vec<_> = s.iter().map(|x| x.build.as_deref().unwrap()).collect();
        assert_eq!(builds, ["ggml-org/llama.cpp", "ikawrakow/ik_llama.cpp"]);
    }

    #[test]
    fn a_dev_instances_build_follows_and_pins_in_its_own_namespace() {
        let mut dev = built(
            "ddd",
            0,
            "official-master",
            &[
                "localhost/lmgw-dev-llama-server:official-master",
                "localhost/lmgw-dev-llama-server:official-master-abc1234-000004",
            ],
            "2026-09-26T03:00:00Z",
        );
        {
            let p = dev.provenance.as_mut().unwrap();
            p.instance = "dev1".into();
            p.other_instance = true;
        }
        let mut imgs = sample();
        imgs.push(dev);
        let s = suggestions(&imgs, Engine::Llama);
        let dev_follows = s
            .iter()
            .find(|x| x.image_ref == "localhost/lmgw-dev-llama-server:official-master")
            .unwrap();
        assert_eq!(dev_follows.origin, Origin::Follows);
        assert_eq!(dev_follows.label, "follows official-master · dev instance");
        let dev_pin = s
            .iter()
            .find(|x| x.image_ref.ends_with("official-master-abc1234-000004"))
            .unwrap();
        assert_eq!(dev_pin.origin, Origin::Pinned);
        // production's build of that slug is its own group, still "follows"
        assert!(s.iter().any(
            |x| x.image_ref == "localhost/lmgw-llama-server:official-master"
                && x.label == "follows Official master"
        ));
    }

    #[test]
    fn a_rebuild_that_did_not_verify_says_so() {
        let mut r = built(
            "rrr",
            1,
            "official-master",
            &["localhost/lmgw-llama-server:official-master-abc1234-def567-r12"],
            "2026-09-26T05:00:00Z",
        );
        r.run_status = Some(BuildRunStatus::Unverified);
        let mut imgs = sample();
        imgs.push(r);
        let s = suggestions(&imgs, Engine::Llama);
        let row = s.iter().find(|x| x.image_ref.ends_with("-r12")).unwrap();
        assert_eq!(row.label, "unverified rebuild · abcdef1 · 2026-09-26");
        assert_eq!(row.origin, Origin::Pinned);
    }

    #[test]
    fn a_build_never_made_current_offers_only_its_pins() {
        let imgs = vec![built(
            "eee",
            3,
            "exp",
            &["localhost/lmgw-llama-server:exp-abc1234-000009"],
            "2026-09-26T00:00:00Z",
        )];
        let s = suggestions(&imgs, Engine::Llama);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].origin, Origin::Pinned);
    }

    #[test]
    fn the_list_narrows_by_every_word_and_offers_the_typed_text_first() {
        let all = suggestions(&sample(), Engine::Llama);
        let items = build_items(&all, "official 000003");
        assert_eq!(items[0], Item::Custom("official 000003".into()));
        assert!(matches!(
            items[1],
            Item::Head {
                label: "Builds",
                count: 1
            }
        ));
        assert!(
            matches!(&items[2], Item::Row(s) if s.image_ref.ends_with("official-master-abc1234-000003"))
        );
        assert_eq!(items.len(), 3);
        // A pinned row is found by its build's name too.
        let by_name = build_items(&all, "official");
        assert_eq!(
            by_name.iter().filter(|i| matches!(i, Item::Row(_))).count(),
            2
        );
        // The exact reference of a row is not offered twice.
        let exact = build_items(&all, "localhost/lmgw-llama-server:ik-main");
        assert!(!exact.iter().any(|i| matches!(i, Item::Custom(_))));
        // Nothing typed: every row, under its two heads.
        let every = build_items(&all, "");
        assert!(matches!(
            every[0],
            Item::Head {
                label: "Builds",
                count: 5
            }
        ));
        assert!(every.iter().any(|i| matches!(
            i,
            Item::Head {
                label: "Other local images",
                count: 2
            }
        )));
    }

    #[test]
    fn the_header_counts_images_not_their_tags() {
        let all = suggestions(&sample(), Engine::Llama);
        // 7 rows: "ik-main" is one image under its moving tag and its own
        // tag, "official-master" too.
        assert_eq!(all.len(), 7);
        assert_eq!(image_count(&all), 5);
        assert_eq!(image_count(&[]), 0);
    }

    #[test]
    fn option_ids_are_distinct_and_id_safe() {
        let a = option_id("ip-list-3", "r:localhost/lmgw-llama-server:ik-main");
        assert_eq!(a, "ip-list-3-r_3alocalhost_2flmgw-llama-server_3aik-main");
        assert!(a
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        // `:` and `/` are not folded into one character
        assert_ne!(option_id("l", "r:a/b"), option_id("l", "r:a:b"));
        assert_ne!(option_id("l", "r:a_2fb"), option_id("l", "r:a/b"));
        assert_eq!(option_id("l", "c:"), "l-c_3a");
    }

    #[test]
    fn enter_after_typing_means_the_typed_text() {
        let all = suggestions(&sample(), Engine::Llama);
        // not a suggestion: the "as typed" row
        let items = build_items(&all, "ghcr.io/x/y:1");
        assert_eq!(typed_active(&items, "ghcr.io/x/y:1").as_deref(), Some("c:"));
        // exactly a suggestion: that row, not the first one containing it
        let exact = "localhost/lmgw-llama-server:ik-main-abc1234-000001";
        let items = build_items(&all, exact);
        assert_eq!(typed_active(&items, exact), Some(format!("r:{exact}")));
        // an emptied box: nothing, so Enter clears rather than picks
        let items = build_items(&all, "  ");
        assert_eq!(typed_active(&items, "  "), None);
    }

    #[test]
    fn the_keyboard_skips_the_group_heads() {
        let all = suggestions(&sample(), Engine::Llama);
        let items = build_items(&all, "");
        // 0 is the Builds head: down from nowhere lands on the first row.
        assert_eq!(step(&items, None, true, 1), Some(1));
        // from the last build row, down skips the next head
        assert_eq!(step(&items, Some(5), true, 1), Some(7));
        assert_eq!(step(&items, Some(7), false, 1), Some(5));
        // stays put at the ends
        assert_eq!(step(&items, Some(1), false, 1), Some(1));
        let last = items.len() - 1;
        assert_eq!(step(&items, Some(last), true, 3), Some(last));
    }

    #[test]
    fn enter_and_blur_commit_trimmed_text_and_empty_only_where_it_means_inherit() {
        assert_eq!(
            commit_value("  ghcr.io/x/y:1 ", "", true),
            Some("ghcr.io/x/y:1".into())
        );
        assert_eq!(commit_value("a:b", "a:b", false), None);
        assert_eq!(commit_value("a:b ", "a:b", false), None);
        // Emptied: an override clears (inherit), a class default stays.
        assert_eq!(commit_value("", "a:b", true), Some(String::new()));
        assert_eq!(commit_value("  ", "a:b", false), None);
    }

    #[test]
    fn a_value_is_found_under_any_spelling_podman_lists() {
        let imgs = sample();
        let hit = |v: &str| find_image(v, &imgs).map(|i| i.id.as_str());
        assert_eq!(hit("localhost/lmgw-llama-server:ik-main"), Some("aaa222"));
        // implicit localhost/ and :latest
        assert_eq!(hit("audio-cpp"), Some("ddd111"));
        assert_eq!(hit("localhost/audio-cpp"), Some("ddd111"));
        assert_eq!(
            hit("ghcr.io/ggml-org/llama.cpp:server-cuda"),
            Some("ccc222")
        );
        assert_eq!(hit("ghcr.io/ggml-org/llama.cpp"), None);
        // an ID prefix of 12+ hex characters
        let mut with_id = imgs.clone();
        with_id[0].id = "0123456789abcdef0123".into();
        assert_eq!(
            find_image("0123456789ab", &with_id).map(|i| i.tags[0].as_str()),
            Some("localhost/lmgw-llama-server:ik-main-abc1234-000001")
        );
        assert!(find_image("0123456", &with_id).is_none());
    }

    #[test]
    fn status_says_local_broken_unverified_or_how_a_missing_image_arrives() {
        let mut imgs = sample();
        imgs[0].run_status = Some(BuildRunStatus::Broken);
        imgs[2].run_status = Some(BuildRunStatus::Unverified);
        let st = |v: &str| value_status(v, &imgs, ImageClass::Chat, None).map(|s| s.presence);
        assert_eq!(st(""), None);
        assert_eq!(
            st("localhost/lmgw-llama-server:ik-main"),
            Some(Presence::Local(Some(BuildRunStatus::Succeeded)))
        );
        assert_eq!(
            st("localhost/lmgw-llama-server:ik-main-abc1234-000001"),
            Some(Presence::Local(Some(BuildRunStatus::Broken)))
        );
        assert_eq!(
            st("localhost/lmgw-llama-server:official-master"),
            Some(Presence::Local(Some(BuildRunStatus::Unverified)))
        );
        assert_eq!(
            st("localhost/llama-server-cuda:full-latest"),
            Some(Presence::Local(None))
        );
        assert_eq!(
            st("ghcr.io/other/image:1"),
            Some(Presence::Missing(Pull::Registry))
        );
        assert_eq!(st("localhost/gone:1"), Some(Presence::Missing(Pull::Never)));
        assert_eq!(st("gone:1"), Some(Presence::Missing(Pull::ShortName)));
        assert_eq!(
            presence_chip(&Presence::Local(Some(BuildRunStatus::Unverified))).1,
            "not GPU-verified"
        );
        assert_eq!(presence_chip(&Presence::Missing(Pull::Never)).1, "missing");
    }

    #[test]
    fn another_engines_image_or_another_backend_is_a_warning() {
        let mut imgs = sample();
        imgs[5].backend = Some("cpu".into());
        let audio_in_chat =
            value_status("localhost/audio-cpp:latest", &imgs, ImageClass::Chat, None).unwrap();
        assert_eq!(
            audio_in_chat.warnings,
            ["audio.cpp image; the chat class runs llama.cpp"]
        );
        let wrong_backend = value_status(
            "localhost/audio-cpp:latest",
            &imgs,
            ImageClass::Audio,
            Some("cuda"),
        )
        .unwrap();
        assert_eq!(
            wrong_backend.warnings,
            ["built for cpu; the class backend is cuda"]
        );
        // Same backend, or no label to compare: nothing to say.
        assert!(value_status(
            "localhost/audio-cpp:latest",
            &imgs,
            ImageClass::Audio,
            Some("CPU")
        )
        .unwrap()
        .warnings
        .is_empty());
        assert!(value_status(
            "localhost/llama-server-cuda:full-latest",
            &imgs,
            ImageClass::Chat,
            Some("vulkan")
        )
        .unwrap()
        .warnings
        .is_empty());
    }

    #[test]
    fn registries_are_told_from_short_names_and_localhost() {
        assert_eq!(pull_of("ghcr.io/a/b:1"), Pull::Registry);
        assert_eq!(pull_of("localhost:5000/a:1"), Pull::Registry);
        assert_eq!(pull_of("docker.io/library/node:24"), Pull::Registry);
        assert_eq!(pull_of("localhost/a:1"), Pull::Never);
        assert_eq!(pull_of("node:24"), Pull::ShortName);
        assert_eq!(pull_of("library/node:24"), Pull::ShortName);
    }

    #[test]
    fn dates_are_the_utc_day() {
        assert_eq!(date_of("2026-09-25T23:30:00Z"), "2026-09-25");
        assert_eq!(date_of("2026-09-25T23:30:00-02:00"), "2026-09-26");
        assert_eq!(date_of("1790000000"), "2026-09-21");
        assert_eq!(date_of("yesterday"), "yesterday");
    }
}
