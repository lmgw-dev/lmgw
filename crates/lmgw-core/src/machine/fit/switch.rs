//! The switch to one card's model settings (machine-check design §1.4,
//! §4.2, D18, D112, D113, D138, WP12): what it changes, field by field, and
//! what it leaves. The check's settings item offers it (its action lists
//! [`Switch::changes`]) and `machine_settings_fit` writes it, both from this
//! one plan, worked out again under the settings lock when it is written
//! (D135), so what was confirmed is what is written.
//!
//! - **A class** set for the other card (its run arguments pass the other
//!   card's device, or its image is built for the other card while its
//!   arguments pass no other card's device either; on an AMD card without
//!   its compute node, ROCm's `/dev/kfd` too) gets the image and, for audio,
//!   the backend lmgw ships for this card ([`super::shipped`]) — an image
//!   already built for this card's backend stays — and its run arguments
//!   swapped ([`swapped`]): the other card's flags go, this card's are put
//!   in front, every other argument of the owner's stays. A class on the
//!   processor, one naming a CDI device of another kind, and, switching to
//!   NVIDIA's, one passing only the render nodes with an image that is not
//!   lmgw's for AMD cards stay as they are, and [`Switch::left`] says why.
//! - **A model row's own run arguments** set for the other card are swapped
//!   the same way, without this card's flags for a row on the processor.
//!   Arguments that then equal the class's are cleared, so the row follows
//!   its class again.
//! - **A model row's own image** that is one of the engines' published
//!   images built for the other card becomes the class's image for this
//!   card, cleared when it equals the class's. An image of the owner's that
//!   lmgw can tell is built for the other card (D139), and one it cannot
//!   tell on a row set for the other card, stay, and [`Switch::left`] names
//!   them.
//! - **A group of the owner's** (`--group-add video`) where AMD's settings
//!   add `--group-add keep-groups`: Podman refuses the two together, so such
//!   arguments stay as they are and [`Switch::left`] says so, rather than
//!   the switch writing arguments no start can use.

use std::collections::BTreeMap;

use lmgw_api_types::machine::{
    SettingsFit, SettingsFitChange, SettingsFitLeft, SettingsFitRunning, SettingsFitValue,
};
use lmgw_api_types::settings_labels as labels;
use serde_json::{Map, Value};

use super::present::{Present, Running};
use super::shipped::{amd_only, backend_is, other, rocm_only, values, Built};
use super::{device, Device};
use crate::config::Snapshot;
use crate::runtime::descriptor::model_runtime;
use crate::runtime::Class;
use crate::store::{ModelTable, RunOverrides, RunOverridesWrite};

/// The four classes, in the Settings page's order.
const CLASSES: [Class; 4] = [Class::Chat, Class::Aux, Class::Audio, Class::Image];

/// One class's part of the switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassWrite {
    pub class: Class,
    pub image: Option<String>,
    pub args: Option<Vec<String>>,
    /// The audio class's backend.
    pub backend: Option<String>,
}

/// What a switch to `fit` does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Switch {
    pub fit: SettingsFit,
    pub changes: Vec<SettingsFitChange>,
    pub left: Vec<SettingsFitLeft>,
    /// The classes' part.
    pub classes: Vec<ClassWrite>,
    /// The rows' part, written in the Settings save's transaction.
    pub rows: Vec<RunOverridesWrite>,
}

impl Switch {
    /// Whether the switch changes anything.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The classes' part as the Settings page's save takes it
    /// (`settings_set_full`'s `router`, `aux_router`, `audio`, `image`).
    pub fn patch(&self) -> Map<String, Value> {
        let mut patch = Map::new();
        for w in &self.classes {
            let mut obj = Map::new();
            if let Some(i) = &w.image {
                obj.insert("image".into(), Value::from(i.clone()));
            }
            if let Some(a) = &w.args {
                obj.insert("extra_run_args".into(), Value::from(a.clone()));
            }
            if let Some(b) = &w.backend {
                obj.insert("backend".into(), Value::from(b.clone()));
            }
            patch.insert(key(w.class).into(), Value::Object(obj));
        }
        patch
    }

    /// `snap` as the switch leaves it: what §1.4 judges again to tell
    /// whether the switch makes the settings fit (D110), and what tells
    /// which running models it concerns ([`affected`]).
    pub fn apply(&self, snap: &Snapshot) -> Snapshot {
        let mut s = snap.clone();
        for w in &self.classes {
            let set = &mut s.settings;
            let (image, args) = (w.image.clone(), w.args.clone());
            match w.class {
                Class::Chat => {
                    set.router.image = image.unwrap_or_else(|| set.router.image.clone());
                    set.router.extra_run_args =
                        args.unwrap_or_else(|| set.router.extra_run_args.clone());
                }
                Class::Aux => {
                    let aux = &mut set.aux_router.base;
                    aux.image = image.unwrap_or_else(|| aux.image.clone());
                    aux.extra_run_args = args.unwrap_or_else(|| aux.extra_run_args.clone());
                }
                Class::Audio => {
                    set.audio.image = image.unwrap_or_else(|| set.audio.image.clone());
                    set.audio.extra_run_args =
                        args.unwrap_or_else(|| set.audio.extra_run_args.clone());
                    if let Some(b) = &w.backend {
                        set.audio.backend = b.clone();
                    }
                }
                Class::Image => {
                    set.image.image = image.unwrap_or_else(|| set.image.image.clone());
                    set.image.extra_run_args =
                        args.unwrap_or_else(|| set.image.extra_run_args.clone());
                }
            }
        }
        for r in &self.rows {
            let now = &r.now;
            match r.table {
                ModelTable::Local => {
                    if let Some(m) = s.local_models.iter_mut().find(|m| m.id == r.id) {
                        (m.image, m.extra_run_args) =
                            (now.image.clone(), now.extra_run_args.clone());
                    }
                }
                ModelTable::Aux => {
                    if let Some(m) = s.aux_models.iter_mut().find(|m| m.id == r.id) {
                        (m.image, m.extra_run_args) =
                            (now.image.clone(), now.extra_run_args.clone());
                    }
                }
                ModelTable::Audio => {
                    if let Some(m) = s.audio_models.iter_mut().find(|m| m.id == r.id) {
                        (m.image, m.extra_run_args) =
                            (now.image.clone(), now.extra_run_args.clone());
                    }
                }
                ModelTable::Image => {
                    if let Some(m) = s.image_models.iter_mut().find(|m| m.id == r.id) {
                        (m.image, m.extra_run_args) =
                            (now.image.clone(), now.extra_run_args.clone());
                    }
                }
            }
        }
        s
    }
}

/// What a model's container is started with, as far as the switch changes
/// it: its image, its run arguments and (audio) the backend in effect.
fn started_with(
    snap: &Snapshot,
    class: Class,
    model_id: &str,
) -> Option<(String, Vec<String>, Option<String>)> {
    model_runtime(snap, class, model_id).map(|m| {
        (
            m.image,
            m.extra_run_args,
            m.audio_settings.map(|a| a.backend),
        )
    })
}

/// The models of `running` whose container would be started with other
/// settings in `after` than in `before`: the ones a switch concerns (D136).
pub(crate) fn affected(before: &Snapshot, after: &Snapshot, running: &[Running]) -> Vec<Running> {
    running
        .iter()
        .filter(|r| {
            started_with(before, r.class, &r.model_id) != started_with(after, r.class, &r.model_id)
        })
        .cloned()
        .collect()
}

/// `running` as an offer lists it.
pub(crate) fn offered(running: &[Running]) -> Vec<SettingsFitRunning> {
    running
        .iter()
        .map(|r| SettingsFitRunning {
            class: r.class.as_str().to_string(),
            model: r.model_id.clone(),
            stop: None,
        })
        .collect()
}

/// The settings page's key of class `class`.
fn key(class: Class) -> &'static str {
    match class {
        Class::Chat => "router",
        Class::Aux => "aux_router",
        Class::Audio => "audio",
        Class::Image => "image",
    }
}

/// The backend lmgw ships for card `fit`.
fn backend_of(fit: SettingsFit) -> Built {
    match fit {
        SettingsFit::AmdVulkan => Built::Vulkan,
        SettingsFit::NvidiaCuda => Built::Cuda,
    }
}

/// Whether a class or row whose container gets `device`, with an image
/// built as `built`, is set for the card `fit` is not (module doc).
fn for_other(fit: SettingsFit, device: &Device, built: Option<Built>, present: &Present) -> bool {
    match fit {
        SettingsFit::AmdVulkan => match device {
            Device::Nvidia => true,
            Device::Amd => built == Some(Built::Cuda) || present.amd_without_kfd(),
            Device::Render => built == Some(Built::Cuda),
            _ => false,
        },
        SettingsFit::NvidiaCuda => match device {
            Device::Amd => true,
            Device::Render => matches!(built, Some(Built::Vulkan | Built::Rocm)),
            Device::Nvidia => built == Some(Built::Rocm),
            _ => false,
        },
    }
}

/// Whether a row's own run arguments that give its container `device` are
/// the other card's, its image (`built`, as it runs now) aside: an image of
/// the other card's is the image's to change, the class's or the row's own,
/// and the render nodes are what AMD's settings give. Switching to NVIDIA's,
/// the render nodes are AMD's with an image built for AMD cards (lmgw's own
/// Vulkan values).
fn args_for_other(
    fit: SettingsFit,
    device: &Device,
    built: Option<Built>,
    present: &Present,
) -> bool {
    match fit {
        SettingsFit::AmdVulkan => match device {
            Device::Nvidia => true,
            Device::Amd => present.amd_without_kfd(),
            _ => false,
        },
        SettingsFit::NvidiaCuda => match device {
            Device::Amd => true,
            Device::Render => matches!(built, Some(Built::Vulkan | Built::Rocm)),
            _ => false,
        },
    }
}

/// Why settings with `device` that the switch to `fit` does not change stay
/// as they are, when that is worth saying (module doc).
fn left_why(fit: SettingsFit, device: &Device, class_level: bool) -> Option<String> {
    match device {
        Device::None if class_level => {
            Some("these models are set to run on the processor, which the switch keeps".to_string())
        }
        Device::OtherCdi(kinds) => Some(format!(
            "it names a device only this computer's own settings describe ({})",
            kinds.join(", ")
        )),
        Device::Render if fit == SettingsFit::NvidiaCuda => Some(
            "it gives models only the graphics card's video devices, with an image that isn't \
             one of lmgw's for AMD cards, so it may be meant for another card; lmgw leaves it"
                .to_string(),
        ),
        _ => None,
    }
}

/// Run arguments a switch to AMD's settings cannot write: the owner's own
/// groups (`--group-add video`) beside the `--group-add keep-groups` AMD's
/// settings need, which Podman refuses together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupClash(pub Vec<String>);

impl GroupClash {
    fn why(&self) -> String {
        format!(
            "it gives models a group of their own ({}), and the settings for AMD cards give \
             them your own groups instead; the two can't be combined, so take that group out \
             and switch again",
            self.0.join(", ")
        )
    }
}

/// Every value `flag` has in `args`, split (`flag value`) or joined
/// (`flag=value`).
fn flag_values(args: &[String], flag: &str) -> Vec<String> {
    let joined = format!("{flag}=");
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        } else if let Some(v) = a.strip_prefix(&joined) {
            out.push(v.to_string());
        }
    }
    out
}

/// `args` without every flag `drop` names with its value, split or joined.
fn without(args: &[String], drop: fn(&str, &str) -> bool) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some((flag, value)) = a.split_once('=').filter(|(f, _)| f.starts_with("--")) {
            if drop(flag, value) {
                i += 1;
                continue;
            }
        } else if let Some(value) = args.get(i + 1) {
            if drop(a, value) {
                i += 2;
                continue;
            }
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Whether `args` already hold `flag value`, split or as `flag=value`.
fn holds(args: &[String], flag: &str, value: &str) -> bool {
    flag_values(args, flag).iter().any(|v| v == value)
}

/// Run arguments for card `fit` (D138): the other card's flags gone (its
/// devices; switching to NVIDIA's, AMD's groups, ROCm's seccomp profile and
/// gfx override; switching to AMD's Vulkan settings, ROCm's two), this
/// card's put in front unless the models run on the processor (`gpu`
/// false), every other argument kept. A class's and a
/// row's alike, so a round trip ends where it began.
pub(crate) fn swapped(
    own: &[String],
    fit: SettingsFit,
    class: Class,
    gpu: bool,
) -> Result<Vec<String>, GroupClash> {
    let mut kept = lmgw_api_types::audio_engine::strip_gpu_devices(own);
    kept = match fit {
        SettingsFit::NvidiaCuda => without(&kept, amd_only),
        SettingsFit::AmdVulkan => without(&kept, rocm_only),
    };
    if !gpu {
        return Ok(kept);
    }
    let mut out: Vec<String> = Vec::new();
    for pair in values(fit, class).args.chunks(2) {
        if let [flag, value] = pair {
            if !holds(&kept, flag, value) {
                out.push(flag.clone());
                out.push(value.clone());
            }
        }
    }
    out.extend(kept);
    let groups = flag_values(&out, "--group-add");
    if groups.iter().any(|g| g == "keep-groups") {
        let others: Vec<String> = groups.into_iter().filter(|g| g != "keep-groups").collect();
        if !others.is_empty() {
            return Err(GroupClash(others));
        }
    }
    Ok(out)
}

/// A class's image, run arguments and (audio) backend now.
fn class_now(snap: &Snapshot, class: Class) -> (String, Vec<String>, Option<String>) {
    let s = &snap.settings;
    match class {
        Class::Chat => (
            s.router.image.clone(),
            s.router.extra_run_args.clone(),
            None,
        ),
        Class::Aux => (
            s.aux_router.image.clone(),
            s.aux_router.extra_run_args.clone(),
            None,
        ),
        Class::Audio => (
            s.audio.image.clone(),
            s.audio.extra_run_args.clone(),
            Some(s.audio.backend.clone()),
        ),
        Class::Image => (s.image.image.clone(), s.image.extra_run_args.clone(), None),
    }
}

/// A model row's own settings, as the snapshot holds them.
struct Row {
    class: Class,
    table: ModelTable,
    id: i64,
    model_id: String,
    image: Option<String>,
    args: Option<Vec<String>>,
}

fn rows(snap: &Snapshot) -> Vec<Row> {
    let row = |class,
               table,
               id,
               model_id: &String,
               image: &Option<String>,
               args: &Option<Vec<String>>| Row {
        class,
        table,
        id,
        model_id: model_id.clone(),
        image: image.clone(),
        args: args.clone(),
    };
    let mut out = Vec::new();
    for m in &snap.local_models {
        out.push(row(
            Class::Chat,
            ModelTable::Local,
            m.id,
            &m.model_id,
            &m.image,
            &m.extra_run_args,
        ));
    }
    for m in &snap.aux_models {
        out.push(row(
            Class::Aux,
            ModelTable::Aux,
            m.id,
            &m.model_id,
            &m.image,
            &m.extra_run_args,
        ));
    }
    for m in &snap.audio_models {
        out.push(row(
            Class::Audio,
            ModelTable::Audio,
            m.id,
            &m.model_id,
            &m.image,
            &m.extra_run_args,
        ));
    }
    for m in &snap.image_models {
        out.push(row(
            Class::Image,
            ModelTable::Image,
            m.id,
            &m.model_id,
            &m.image,
            &m.extra_run_args,
        ));
    }
    out
}

fn text(v: &str) -> SettingsFitValue {
    SettingsFitValue::Text(v.to_string())
}

fn args_value(v: &[String]) -> SettingsFitValue {
    SettingsFitValue::Args(v.to_vec())
}

fn change(
    class: Class,
    model: Option<&str>,
    field: &str,
    from: Option<SettingsFitValue>,
    to: Option<SettingsFitValue>,
) -> SettingsFitChange {
    SettingsFitChange {
        class: class.as_str().to_string(),
        model: model.map(str::to_string),
        field: field.to_string(),
        from,
        to,
    }
}

fn left_at(
    class: Class,
    model: Option<&str>,
    field: &str,
    value: SettingsFitValue,
    why: String,
) -> SettingsFitLeft {
    SettingsFitLeft {
        class: class.as_str().to_string(),
        model: model.map(str::to_string),
        field: field.to_string(),
        value,
        why,
    }
}

/// The switch to card `fit`'s settings, from the settings and rows in
/// `snap` and what `present` says of this computer (module doc).
pub(crate) fn plan(snap: &Snapshot, fit: SettingsFit, present: &Present) -> Switch {
    let mut changes = Vec::new();
    let mut left = Vec::new();
    let mut classes = Vec::new();
    let away = other(fit);
    // Each class's image and run arguments after the switch.
    let mut after: BTreeMap<&'static str, (String, Vec<String>)> = BTreeMap::new();

    for class in CLASSES {
        let (image, args, backend) = class_now(snap, class);
        after.insert(class.as_str(), (image.clone(), args.clone()));
        let d = device(&args);
        let built = present.built(&image);
        let args_at = labels::run_args_at(class.as_str()).to_string();
        if !for_other(fit, &d, built, present) {
            if let Some(why) = left_why(fit, &d, true) {
                left.push(left_at(class, None, &args_at, args_value(&args), why));
            }
            continue;
        }
        let new_args = match swapped(&args, fit, class, true) {
            Ok(a) => a,
            Err(clash) => {
                left.push(left_at(
                    class,
                    None,
                    &args_at,
                    args_value(&args),
                    clash.why(),
                ));
                continue;
            }
        };
        let to = values(fit, class);
        let new_image = if built == Some(backend_of(fit)) {
            image.clone()
        } else {
            to.image.clone()
        };
        let mut w = ClassWrite {
            class,
            image: None,
            args: None,
            backend: None,
        };
        if new_image != image {
            changes.push(change(
                class,
                None,
                &labels::image_at(class.as_str()).to_string(),
                Some(text(&image)),
                Some(text(&new_image)),
            ));
            w.image = Some(new_image.clone());
        }
        if new_args != args {
            changes.push(change(
                class,
                None,
                &args_at,
                Some(args_value(&args)),
                Some(args_value(&new_args)),
            ));
            w.args = Some(new_args.clone());
        }
        if let (Some(now), Some(want)) = (&backend, &to.backend) {
            if backend_is(away, now) && now.trim() != want {
                changes.push(change(
                    class,
                    None,
                    &labels::AUDIO_BACKEND_AT.to_string(),
                    Some(text(now)),
                    Some(text(want)),
                ));
                w.backend = Some(want.clone());
            }
        }
        after.insert(class.as_str(), (new_image, new_args));
        if w.image.is_some() || w.args.is_some() || w.backend.is_some() {
            classes.push(w);
        }
    }

    let mut writes = Vec::new();
    for r in rows(snap) {
        let Some(eff) = model_runtime(snap, r.class, &r.model_id) else {
            continue;
        };
        let eff_built = present.built(&eff.image);
        let unit_other = for_other(fit, &device(&eff.extra_run_args), eff_built, present);
        let (class_image, class_args) = after.get(r.class.as_str()).cloned().unwrap_or_default();
        let mut now = RunOverrides {
            image: r.image.clone(),
            extra_run_args: r.args.clone(),
        };
        let model = Some(r.model_id.as_str());
        if let Some(own) = &r.args {
            let d = device(own);
            if args_for_other(fit, &d, eff_built, present) {
                match swapped(own, fit, r.class, eff.placement().is_gpu()) {
                    Ok(new) => {
                        now.extra_run_args = (new != class_args && !new.is_empty()).then_some(new)
                    }
                    Err(clash) => left.push(left_at(
                        r.class,
                        model,
                        labels::RUN_ARGS_OVERRIDE,
                        args_value(own),
                        clash.why(),
                    )),
                }
            } else if let Some(why) = left_why(fit, &d, false) {
                left.push(left_at(
                    r.class,
                    model,
                    labels::RUN_ARGS_OVERRIDE,
                    args_value(own),
                    why,
                ));
            }
        }
        if let Some(own) = &r.image {
            let built = present.built(own);
            let wrong = built.is_some_and(|b| !present.runs_here(b, fit));
            let away_built = built.is_some_and(|b| b.card() == away);
            if Present::shipped(own) && (wrong || (away_built && unit_other)) {
                let new = values(fit, r.class).image;
                now.image = (new != class_image).then_some(new);
            } else if let (true, Some(b)) = (wrong, built) {
                left.push(left_at(
                    r.class,
                    model,
                    labels::IMAGE_OVERRIDE,
                    text(own),
                    format!(
                        "it is built for {} cards ({}) and isn't one of lmgw's images, so lmgw \
                         doesn't replace it: pick one for {} cards on the model's page",
                        card_name(b.card()),
                        b.name(),
                        card_name(fit)
                    ),
                ));
            } else if built.is_none() && unit_other {
                left.push(left_at(
                    r.class,
                    model,
                    labels::IMAGE_OVERRIDE,
                    text(own),
                    format!(
                        "lmgw can't tell which card this image is built for: if it is one for \
                         {} cards, pick one for {} cards on the model's page",
                        card_name(away),
                        card_name(fit)
                    ),
                ));
            }
        }
        if now.image != r.image {
            changes.push(change(
                r.class,
                model,
                labels::IMAGE_OVERRIDE,
                r.image.as_deref().map(text),
                now.image.as_deref().map(text),
            ));
        }
        if now.extra_run_args != r.args {
            changes.push(change(
                r.class,
                model,
                labels::RUN_ARGS_OVERRIDE,
                r.args.as_deref().map(args_value),
                now.extra_run_args.as_deref().map(args_value),
            ));
        }
        if now.image != r.image || now.extra_run_args != r.args {
            writes.push(RunOverridesWrite {
                table: r.table,
                id: r.id,
                model_id: r.model_id.clone(),
                was: RunOverrides {
                    image: r.image,
                    extra_run_args: r.args,
                },
                now,
            });
        }
    }
    Switch {
        fit,
        changes,
        left,
        classes,
        rows: writes,
    }
}

/// The card's maker, as the sentences name it.
pub(crate) fn card_name(fit: SettingsFit) -> &'static str {
    match fit {
        SettingsFit::AmdVulkan => "AMD",
        SettingsFit::NvidiaCuda => "NVIDIA",
    }
}

/// Run arguments as one line, each argument that holds a space or a quote
/// in quotes (`-e "A=b c"`), so no two lists read alike.
pub(crate) fn line(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || c == '"' || c == '\'')
            {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A value as a detail line shows it.
pub(crate) fn shown(v: &SettingsFitValue) -> String {
    match v {
        SettingsFitValue::Text(t) if t.is_empty() => "(none)".to_string(),
        SettingsFitValue::Text(t) => t.clone(),
        SettingsFitValue::Args(a) if a.is_empty() => "(none)".to_string(),
        SettingsFitValue::Args(a) => line(a),
    }
}

/// Where a change or a setting left is, as a detail line names it.
fn at(class: &str, model: Option<&String>, field: &str) -> String {
    match model {
        Some(m) => format!("model {m} ({class}), {field}"),
        None => field.to_string(),
    }
}

/// One change as a detail line: the field, then the value as it is and as
/// it would become.
pub(crate) fn change_line(c: &SettingsFitChange) -> String {
    let side = |v: &Option<SettingsFitValue>| match v {
        Some(v) => shown(v),
        None => "the class's".to_string(),
    };
    format!(
        "{}: {} → {}",
        at(&c.class, c.model.as_ref(), &c.field),
        side(&c.from),
        side(&c.to)
    )
}

/// One setting left as it is, as a detail line.
pub(crate) fn left_line(l: &SettingsFitLeft) -> String {
    format!(
        "{} stays {}: {}",
        at(&l.class, l.model.as_ref(), &l.field),
        shown(&l.value),
        l.why
    )
}

#[cfg(test)]
mod tests {
    use super::super::present::AmdCard;
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    const NVIDIA: [&str; 4] = [
        "--device",
        "nvidia.com/gpu=all",
        "--security-opt",
        "label=disable",
    ];
    const AMD: [&str; 6] = [
        "--device",
        "/dev/dri",
        "--group-add",
        "keep-groups",
        "--security-opt",
        "label=disable",
    ];

    #[test]
    fn a_rows_own_flags_are_swapped_and_the_rest_kept() {
        let own = v(&[
            "--device",
            "nvidia.com/gpu=all",
            "--security-opt",
            "label=disable",
            "--shm-size",
            "8g",
            "-e",
            "NVIDIA_VISIBLE_DEVICES=all",
        ]);
        assert_eq!(
            swapped(&own, SettingsFit::AmdVulkan, Class::Chat, true),
            Ok(v(&[
                "--device",
                "/dev/dri",
                "--group-add",
                "keep-groups",
                "--security-opt",
                "label=disable",
                "--shm-size",
                "8g",
            ]))
        );
        // On the processor: the card's flags go, none come.
        assert_eq!(
            swapped(&own, SettingsFit::AmdVulkan, Class::Aux, false),
            Ok(v(&["--security-opt", "label=disable", "--shm-size", "8g"]))
        );
        // AMD's ROCm flags, back to NVIDIA's: the groups, the seccomp
        // profile and the gfx override go with the devices, in either
        // spelling.
        let rocm = v(&[
            "--device=/dev/kfd",
            "--device",
            "/dev/dri",
            "--group-add",
            "keep-groups",
            "--security-opt=seccomp=unconfined",
            "--security-opt=label=disable",
            "-e",
            "HSA_OVERRIDE_GFX_VERSION=11.0.2",
            "--shm-size",
            "8g",
        ]);
        assert_eq!(
            swapped(&rocm, SettingsFit::NvidiaCuda, Class::Chat, true),
            Ok(v(&[
                "--device",
                "nvidia.com/gpu=all",
                "--security-opt=label=disable",
                "--shm-size",
                "8g",
            ]))
        );
    }

    /// Each class's NVIDIA defaults, with an argument of the owner's after
    /// them, become AMD's and come back as they were (D138).
    #[test]
    fn a_round_trip_ends_where_it_began() {
        for class in CLASSES {
            let mut nvidia = values(SettingsFit::NvidiaCuda, class).args;
            assert_eq!(
                swapped(&nvidia, SettingsFit::AmdVulkan, class, true),
                Ok(values(SettingsFit::AmdVulkan, class).args),
                "{class:?}"
            );
            nvidia.extend(v(&["--shm-size", "16g", "-e", "A=b c"]));
            let amd = swapped(&nvidia, SettingsFit::AmdVulkan, class, true).unwrap();
            assert_eq!(&amd[..6], &AMD, "{class:?}");
            assert_eq!(
                swapped(&amd, SettingsFit::NvidiaCuda, class, true),
                Ok(nvidia),
                "{class:?}"
            );
        }
        let amd = v(&AMD);
        assert_eq!(
            swapped(&amd, SettingsFit::NvidiaCuda, Class::Chat, true),
            Ok(v(&NVIDIA))
        );
    }

    /// A group of the owner's beside AMD's `keep-groups` is refused by
    /// Podman: the switch leaves such arguments and says which group.
    #[test]
    fn a_group_of_the_owners_is_not_combined_with_keep_groups() {
        let own = v(&["--device", "nvidia.com/gpu=all", "--group-add", "video"]);
        let clash = swapped(&own, SettingsFit::AmdVulkan, Class::Chat, true).unwrap_err();
        assert_eq!(clash, GroupClash(vec!["video".into()]));
        assert!(clash.why().contains("(video)"), "{}", clash.why());
        let joined = v(&["--gpus=all", "--group-add=render"]);
        assert!(swapped(&joined, SettingsFit::AmdVulkan, Class::Chat, true).is_err());
        // To NVIDIA's, the owner's group stays: nothing adds keep-groups.
        let amd_own = v(&["--device", "/dev/kfd", "--group-add", "video"]);
        assert_eq!(
            swapped(&amd_own, SettingsFit::NvidiaCuda, Class::Chat, true),
            Ok(v(&[
                "--device",
                "nvidia.com/gpu=all",
                "--security-opt",
                "label=disable",
                "--group-add",
                "video"
            ]))
        );
    }

    #[test]
    fn which_settings_are_the_other_cards() {
        let none = Present::default();
        let no_kfd = Present {
            amd: Some(AmdCard { kfd: false }),
            ..Present::default()
        };
        let (cuda, vulkan, rocm) = (Some(Built::Cuda), Some(Built::Vulkan), Some(Built::Rocm));
        // Switching to AMD's: NVIDIA's device, or a CUDA image on AMD's
        // devices; ROCm's compute node only where the card has none.
        assert!(for_other(
            SettingsFit::AmdVulkan,
            &Device::Nvidia,
            None,
            &none
        ));
        assert!(for_other(
            SettingsFit::AmdVulkan,
            &Device::Render,
            cuda,
            &none
        ));
        assert!(!for_other(
            SettingsFit::AmdVulkan,
            &Device::Render,
            vulkan,
            &none
        ));
        assert!(!for_other(
            SettingsFit::AmdVulkan,
            &Device::Amd,
            rocm,
            &none
        ));
        assert!(for_other(
            SettingsFit::AmdVulkan,
            &Device::Amd,
            rocm,
            &no_kfd
        ));
        assert!(!for_other(
            SettingsFit::AmdVulkan,
            &Device::None,
            cuda,
            &none
        ));
        // Switching to NVIDIA's: AMD's compute node, or an AMD image on the
        // render nodes; a Vulkan image on NVIDIA's device runs.
        assert!(for_other(
            SettingsFit::NvidiaCuda,
            &Device::Amd,
            None,
            &none
        ));
        assert!(for_other(
            SettingsFit::NvidiaCuda,
            &Device::Render,
            vulkan,
            &none
        ));
        assert!(!for_other(
            SettingsFit::NvidiaCuda,
            &Device::Render,
            None,
            &none
        ));
        assert!(!for_other(
            SettingsFit::NvidiaCuda,
            &Device::Nvidia,
            vulkan,
            &none
        ));
        assert!(for_other(
            SettingsFit::NvidiaCuda,
            &Device::Nvidia,
            rocm,
            &none
        ));
    }

    #[test]
    fn a_value_with_a_space_reads_apart_from_two() {
        assert_eq!(line(&v(&["-e", "A=b c"])), r#"-e "A=b c""#);
        assert_eq!(line(&v(&["-e", "A=b", "c"])), "-e A=b c");
        assert_eq!(shown(&SettingsFitValue::Args(vec![])), "(none)");
    }
}
