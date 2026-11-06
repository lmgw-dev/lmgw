//! The device lists the page offers (chat-voice §2.4, §12.3, §12.4): inputs
//! from `enumerateDevices`, outputs from the shell in the app, from
//! `enumerateDevices` where the browser can route a context
//! (`AudioContext.setSinkId`), else the system default alone.
//!
//! Browsers hide device labels — WebKitGTK also the ids — until a capture has
//! been granted in the document (measured in WP5: WebKitGTK 2.54 never asks
//! for device-info permission). The list says so instead of showing nameless
//! rows, and is read again once the microphone has been opened.

use wasm_bindgen::{JsCast, JsValue};

use super::devices::Device;
use super::shell;

/// Where the output list comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum OutputKind {
    /// The app shell routes lmgw's streams through PipeWire.
    Shell,
    /// The browser routes the playback context (`setSinkId`).
    SinkId,
    /// This browser cannot choose an output.
    #[default]
    DefaultOnly,
}

/// What the device popover lists.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Listing {
    pub inputs: Vec<Device>,
    /// The browser shows the inputs' ids and labels (a capture was granted).
    pub inputs_known: bool,
    pub outputs: Vec<Device>,
    pub outputs_known: bool,
    pub output_kind: OutputKind,
    /// Why the outputs are what they are, when that needs saying.
    pub output_note: Option<String>,
}

/// One `MediaDeviceInfo`, as read off the page.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RawDevice {
    pub kind: String,
    pub device_id: String,
    pub label: String,
    pub group_id: String,
}

/// Chrome's `default` and `communications` entries stand for another entry
/// of the list; "System default" is the page's own first row.
fn is_alias(id: &str) -> bool {
    id == "default" || id == "communications"
}

/// The entries of one kind, and whether the browser shows who they are.
/// Entries with no id (WebKitGTK before a grant) cannot be chosen. The entry
/// Chrome's `default` alias stands for is marked as the system default, so
/// choosing it is not mistaken for choosing another device (review m6).
pub(crate) fn of_kind(raw: &[RawDevice], kind: &str) -> (Vec<Device>, bool) {
    let mine: Vec<&RawDevice> = raw.iter().filter(|d| d.kind == kind).collect();
    let known = mine
        .iter()
        .any(|d| !d.label.is_empty() && !d.device_id.is_empty());
    let real: Vec<&RawDevice> = mine
        .iter()
        .copied()
        .filter(|d| !d.device_id.is_empty() && !is_alias(&d.device_id))
        .collect();
    let default = default_of(&mine, &real);
    let list = real
        .into_iter()
        .enumerate()
        .map(|(i, d)| Device {
            id: d.device_id.clone(),
            label: if d.label.is_empty() {
                format!(
                    "{} {}",
                    if kind == "audioinput" {
                        "Microphone"
                    } else {
                        "Output"
                    },
                    i + 1
                )
            } else {
                d.label.clone()
            },
            named: !d.label.is_empty(),
            is_default: default == Some(d.device_id.as_str()),
        })
        .collect();
    (list, known)
}

/// The entry Chrome's `default` alias stands for: by its label ("Default -
/// <the device's label>"), else the one entry sharing its `groupId`.
fn default_of<'a>(mine: &[&RawDevice], real: &[&'a RawDevice]) -> Option<&'a str> {
    let alias = mine.iter().find(|d| d.device_id == "default")?;
    if let Some((_, named)) = alias.label.split_once(" - ") {
        if let Some(d) = real
            .iter()
            .find(|d| !d.label.is_empty() && d.label == named)
        {
            return Some(&d.device_id);
        }
    }
    if alias.group_id.is_empty() {
        return None;
    }
    let mut same = real.iter().filter(|d| d.group_id == alias.group_id);
    match (same.next(), same.next()) {
        (Some(d), None) => Some(&d.device_id),
        _ => None,
    }
}

/// The shell's sinks as list entries: `node.name` is the id.
pub(crate) fn of_shell(outs: &shell::Outputs) -> Vec<Device> {
    outs.outputs
        .iter()
        .map(|o| Device {
            id: o.name.clone(),
            label: if o.description.is_empty() {
                o.name.clone()
            } else {
                o.description.clone()
            },
            named: true,
            is_default: o.default,
        })
        .collect()
}

/// `navigator.mediaDevices`, absent outside a secure context (§11.4).
pub(crate) fn media_devices() -> Option<web_sys::MediaDevices> {
    let nav: JsValue = window().navigator().into();
    let md = js_sys::Reflect::get(&nav, &JsValue::from_str("mediaDevices")).ok()?;
    md.dyn_into::<web_sys::MediaDevices>().ok()
}

/// Is the page a secure context (https, or localhost / 127.0.0.1)?
pub(crate) fn secure() -> bool {
    js_sys::Reflect::get(&window(), &JsValue::from_str("isSecureContext"))
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Can this browser route a playback context (`AudioContext.setSinkId`)?
pub(crate) fn sink_id_supported() -> bool {
    let Ok(ctor) = js_sys::Reflect::get(&window(), &JsValue::from_str("AudioContext")) else {
        return false;
    };
    js_sys::Reflect::get(&ctor, &JsValue::from_str("prototype"))
        .and_then(|p| js_sys::Reflect::has(&p, &JsValue::from_str("setSinkId")))
        .unwrap_or(false)
}

/// Every media device the page may see.
pub(crate) async fn raw_devices() -> Result<Vec<RawDevice>, String> {
    let md = media_devices().ok_or_else(|| "voice needs https or localhost".to_string())?;
    let promise = md.enumerate_devices().map_err(|e| shell::js_text(&e))?;
    let list = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(|e| shell::js_text(&e))?;
    let field = |d: &JsValue, k: &str| {
        js_sys::Reflect::get(d, &JsValue::from_str(k))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default()
    };
    Ok(js_sys::Array::from(&list)
        .iter()
        .map(|d| RawDevice {
            kind: field(&d, "kind"),
            device_id: field(&d, "deviceId"),
            label: field(&d, "label"),
            group_id: field(&d, "groupId"),
        })
        .collect())
}

/// Read both lists.
pub(crate) async fn list() -> Listing {
    let raw = raw_devices().await.unwrap_or_default();
    let (inputs, inputs_known) = of_kind(&raw, "audioinput");
    let mut l = Listing {
        inputs,
        inputs_known,
        ..Default::default()
    };
    if shell::in_shell() {
        l.output_kind = OutputKind::Shell;
        match shell::audio_outputs().await {
            Ok(outs) => {
                l.outputs = of_shell(&outs);
                l.outputs_known = true;
                l.output_note = outs.note;
            }
            Err(e) => l.output_note = Some(format!("the outputs could not be listed: {e}")),
        }
    } else if sink_id_supported() {
        l.output_kind = OutputKind::SinkId;
        let (outputs, known) = of_kind(&raw, "audiooutput");
        l.outputs = outputs;
        l.outputs_known = known;
    }
    // Otherwise `DefaultOnly`: the one row says the browser cannot choose.
    l
}

fn window() -> web_sys::Window {
    web_sys::window().expect("a window")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(kind: &str, id: &str, label: &str) -> RawDevice {
        RawDevice {
            kind: kind.into(),
            device_id: id.into(),
            label: label.into(),
            group_id: String::new(),
        }
    }

    fn grouped(kind: &str, id: &str, label: &str, group: &str) -> RawDevice {
        RawDevice {
            group_id: group.into(),
            ..raw(kind, id, label)
        }
    }

    #[test]
    fn hidden_entries_list_nothing_to_choose() {
        // WebKitGTK before a grant: ids and labels empty.
        let (list, known) = of_kind(
            &[raw("audioinput", "", ""), raw("audioinput", "", "")],
            "audioinput",
        );
        assert!(list.is_empty());
        assert!(!known);
    }

    #[test]
    fn chromes_alias_entries_are_left_to_system_default() {
        let (list, known) = of_kind(
            &[
                raw("audioinput", "default", "Default - USB mic"),
                raw("audioinput", "communications", "Communications - USB mic"),
                raw("audioinput", "a1", "USB mic"),
                raw("audiooutput", "o1", "Speakers"),
                raw("videoinput", "v1", "Camera"),
            ],
            "audioinput",
        );
        assert!(known);
        assert_eq!(
            list,
            vec![Device {
                id: "a1".into(),
                label: "USB mic".into(),
                named: true,
                is_default: true,
            }]
        );
    }

    #[test]
    fn the_entry_chromes_default_stands_for_is_the_system_default() {
        // By the alias's label.
        let (list, _) = of_kind(
            &[
                grouped("audiooutput", "default", "Default - Speakers", "g1"),
                grouped("audiooutput", "o1", "Headphones", "g2"),
                grouped("audiooutput", "o2", "Speakers", "g1"),
            ],
            "audiooutput",
        );
        assert_eq!(
            list.iter()
                .map(|d| (d.id.as_str(), d.is_default))
                .collect::<Vec<_>>(),
            vec![("o1", false), ("o2", true)]
        );
        // By the one entry sharing its group, when the label does not say.
        let (list, _) = of_kind(
            &[
                grouped("audiooutput", "default", "Default", "g2"),
                grouped("audiooutput", "o1", "Headphones", "g2"),
                grouped("audiooutput", "o2", "Speakers", "g1"),
            ],
            "audiooutput",
        );
        assert!(list[0].is_default && !list[1].is_default);
        // Two entries in its group and no label to tell: neither is marked.
        let (list, _) = of_kind(
            &[
                grouped("audiooutput", "default", "Default", "g1"),
                grouped("audiooutput", "o1", "HDMI 1", "g1"),
                grouped("audiooutput", "o2", "HDMI 2", "g1"),
            ],
            "audiooutput",
        );
        assert!(list.iter().all(|d| !d.is_default));
        // No alias (WebKitGTK, Firefox): nothing marked.
        let (list, _) = of_kind(
            &[grouped("audiooutput", "o1", "Speakers", "g1")],
            "audiooutput",
        );
        assert!(!list[0].is_default);
    }

    #[test]
    fn an_unlabelled_entry_with_an_id_is_numbered() {
        // Chrome before a grant shows ids but no labels.
        let (list, known) = of_kind(&[raw("audiooutput", "x", "")], "audiooutput");
        assert!(!known);
        assert_eq!(list[0].label, "Output 1");
        assert!(!list[0].named, "a placeholder, never stored as its label");
    }

    #[test]
    fn shell_sinks_are_listed_by_node_name_with_the_default_marked() {
        let outs = shell::Outputs {
            outputs: vec![
                shell::Output {
                    name: "alsa_output.usb".into(),
                    description: "Headphones".into(),
                    serial: 77,
                    default: false,
                },
                shell::Output {
                    name: "bare_sink".into(),
                    description: String::new(),
                    serial: 41,
                    default: true,
                },
            ],
            note: None,
        };
        let l = of_shell(&outs);
        assert_eq!(l[0].id, "alsa_output.usb");
        assert_eq!(l[0].label, "Headphones");
        assert!(!l[0].is_default);
        assert_eq!(l[1].label, "bare_sink");
        assert!(l[1].is_default);
    }
}
