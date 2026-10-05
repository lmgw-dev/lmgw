//! The window's device choice and echo mode (chat-voice §2.4, §12), as pure
//! rules: how a stored device is found again, which capture constraints an
//! echo mode asks for, and when the echo warning applies. Tested natively.
//!
//! The choice lives in `localStorage` because device ids belong to one
//! browser profile (the app window has its own): `lmgw.voice.input` and
//! `lmgw.voice.output` hold `{id, label}`, `lmgw.voice.echo` the mode.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const KEY_INPUT: &str = "lmgw.voice.input";
pub(crate) const KEY_OUTPUT: &str = "lmgw.voice.output";
pub(crate) const KEY_ECHO: &str = "lmgw.voice.echo";

/// A chosen device as stored: its id in this browser profile (an output's
/// PipeWire `node.name` in the app) and the label it had, which finds it
/// again when the id changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DevicePick {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
}

impl DevicePick {
    /// A stored value, read tolerantly: anything unreadable or naming no
    /// device is the system default.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str::<Self>(raw)
            .ok()
            .filter(|p| !p.id.is_empty() || !p.label.is_empty())
    }

    /// What a chosen entry stores: a placeholder name the page made up
    /// ("Microphone 2") is not stored, so it can never match another
    /// unnamed device later.
    pub(crate) fn of(d: &Device) -> Self {
        Self {
            id: d.id.clone(),
            label: if d.named {
                d.label.clone()
            } else {
                String::new()
            },
        }
    }
}

/// One entry of a device list, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Device {
    pub id: String,
    pub label: String,
    /// The label is the device's own, not a placeholder the page numbered.
    pub named: bool,
    /// The system's default device: from PipeWire in the app, from the
    /// browser's `default` entry in Chrome.
    pub is_default: bool,
}

/// What a stored choice comes to against the current list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Nothing stored: the system default.
    Default,
    /// Found, by id or (`by_label`) by label.
    Found { device: Device, by_label: bool },
    /// Stored, but neither its id nor its label is in the list: the system
    /// default is used and a note says so.
    Missing(DevicePick),
    /// The list hides ids and labels (no capture granted yet in this
    /// document): it cannot be told yet.
    Unknown(DevicePick),
}

/// Find a stored device in `list`: by id, then by label (§2.4). `known` is
/// false while the browser hides ids and labels.
pub(crate) fn resolve(stored: Option<&DevicePick>, list: &[Device], known: bool) -> Resolved {
    let Some(pick) = stored else {
        return Resolved::Default;
    };
    if !known {
        return Resolved::Unknown(pick.clone());
    }
    if let Some(d) = list.iter().find(|d| !pick.id.is_empty() && d.id == pick.id) {
        return Resolved::Found {
            device: d.clone(),
            by_label: false,
        };
    }
    if let Some(d) = list
        .iter()
        .find(|d| !pick.label.is_empty() && d.label == pick.label)
    {
        return Resolved::Found {
            device: d.clone(),
            by_label: true,
        };
    }
    Resolved::Missing(pick.clone())
}

impl Resolved {
    /// The device id to use, if any (`None`: the system default).
    pub(crate) fn id(&self) -> Option<&str> {
        match self {
            Resolved::Found { device, .. } => Some(&device.id),
            _ => None,
        }
    }

    /// The note the device list shows under the choice, if any.
    pub(crate) fn note(&self, what: &str) -> Option<String> {
        match self {
            Resolved::Missing(p) => Some(format!(
                "the {what} chosen here before ({}) is not present; using the system default",
                name_of(p)
            )),
            _ => None,
        }
    }
}

fn name_of(p: &DevicePick) -> &str {
    if p.label.is_empty() {
        &p.id
    } else {
        &p.label
    }
}

/// How the window handles the echo of lmgw's own voice (§12.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EchoMode {
    /// The input device cancels echo itself (the default setup): browser
    /// echo cancellation off, full duplex, barge-in.
    #[default]
    Device,
    /// The browser's echo cancellation.
    Browser,
    /// Headphones or a headset: nothing to cancel.
    NotNeeded,
    /// No echo handling: half duplex, no barge-in.
    None,
}

impl EchoMode {
    pub(crate) const ALL: [EchoMode; 4] = [
        EchoMode::Device,
        EchoMode::Browser,
        EchoMode::NotNeeded,
        EchoMode::None,
    ];

    /// The stored and wire name.
    pub(crate) fn key(self) -> &'static str {
        match self {
            EchoMode::Device => "device",
            EchoMode::Browser => "browser",
            EchoMode::NotNeeded => "not_needed",
            EchoMode::None => "none",
        }
    }

    /// A stored value; anything else is the default.
    pub(crate) fn parse(raw: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|m| m.key() == raw.trim())
            .unwrap_or_default()
    }

    /// Its name in the page. `in_app` is kept for a label that differs in
    /// the app window: WebKitGTK's canceller was the open question until the
    /// owner verified barge-in with it there on 2026-10-04 (§12.1).
    pub(crate) fn label(self, _in_app: bool) -> &'static str {
        match self {
            EchoMode::Device => "Input cancels echo",
            EchoMode::Browser => "Browser echo cancellation",
            EchoMode::NotNeeded => "Not needed (headphones)",
            EchoMode::None => "None: half duplex",
        }
    }

    /// One line on what it does.
    pub(crate) fn hint(self) -> &'static str {
        match self {
            EchoMode::Device => {
                "The microphone or its driver removes lmgw's voice, against the default output. Full duplex, you can interrupt."
            }
            EchoMode::Browser => {
                "The browser removes lmgw's voice from the microphone. Full duplex, you can interrupt."
            }
            EchoMode::NotNeeded => {
                "lmgw's voice does not reach the microphone. Full duplex, you can interrupt."
            }
            EchoMode::None => {
                "The microphone is ignored while lmgw speaks; you cannot interrupt it by talking."
            }
        }
    }

    /// Realtime runs half duplex in this mode (`half_duplex: true`).
    pub(crate) fn half_duplex(self) -> bool {
        self == EchoMode::None
    }
}

/// Which device the capture asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want<'a> {
    /// The system default: no `deviceId`.
    Default,
    /// A device found in the list.
    Exact(&'a str),
    /// A stored id the list cannot confirm yet (ids hidden before the first
    /// grant): asked for as a preference, so a stale one falls back.
    Ideal(&'a str),
}

/// The `audio` constraints of `getUserMedia` for an echo mode (§12.1's
/// table). Every mode but `browser` turns the browser's canceller off — two
/// cancellers in series hurt. `device` and `not_needed` also turn Chrome's
/// noise suppression and gain control off: the input device processes its
/// own signal (WebKitGTK has neither, and ignores the names). `none` is a
/// plain microphone with no processing of its own, so the browser keeps its
/// defaults there. Mono is asked for; the capture worklet mixes down
/// whatever comes.
pub(crate) fn constraints(mode: EchoMode, want: Want) -> Value {
    let mut c = match mode {
        EchoMode::Browser => json!({ "echoCancellation": true }),
        EchoMode::None => json!({ "echoCancellation": false }),
        EchoMode::Device | EchoMode::NotNeeded => {
            json!({ "echoCancellation": false, "noiseSuppression": false, "autoGainControl": false })
        }
    };
    c["channelCount"] = json!({ "ideal": 1 });
    match want {
        Want::Default => {}
        Want::Exact(id) => c["deviceId"] = json!({ "exact": id }),
        Want::Ideal(id) => c["deviceId"] = json!({ "ideal": id }),
    }
    c
}

/// §12.2's warning: in `device` mode the input cancels echo against the
/// default output's monitor, so lmgw playing anywhere else would make it
/// interrupt itself. `plays_on` is the output resolved against the list.
/// There is no graph tracing: a chosen output that is not the system default
/// is enough.
pub(crate) fn echo_warning(mode: EchoMode, plays_on: &Resolved) -> Option<String> {
    if mode != EchoMode::Device {
        return None;
    }
    match plays_on {
        Resolved::Found { device, .. } if !device.is_default => Some(format!(
            "your input cancels echo from the default output; lmgw plays on {}",
            if device.label.is_empty() {
                &device.id
            } else {
                &device.label
            }
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, label: &str) -> Device {
        Device {
            id: id.into(),
            label: label.into(),
            named: true,
            is_default: false,
        }
    }

    fn pick(id: &str, label: &str) -> DevicePick {
        DevicePick {
            id: id.into(),
            label: label.into(),
        }
    }

    #[test]
    fn a_stored_device_is_found_by_id_first() {
        let list = [dev("a", "USB mic"), dev("b", "Headset")];
        // The id wins over a label that names another device.
        let r = resolve(Some(&pick("b", "USB mic")), &list, true);
        assert_eq!(
            r,
            Resolved::Found {
                device: dev("b", "Headset"),
                by_label: false
            }
        );
        assert_eq!(r.id(), Some("b"));
        assert_eq!(r.note("microphone"), None);
    }

    #[test]
    fn then_by_label_when_its_id_changed() {
        let list = [dev("a2", "USB mic"), dev("b", "Headset")];
        let r = resolve(Some(&pick("a", "USB mic")), &list, true);
        assert_eq!(
            r,
            Resolved::Found {
                device: dev("a2", "USB mic"),
                by_label: true
            }
        );
    }

    #[test]
    fn else_the_system_default_with_a_note() {
        let list = [dev("b", "Headset")];
        let r = resolve(Some(&pick("a", "USB mic")), &list, true);
        assert_eq!(r, Resolved::Missing(pick("a", "USB mic")));
        assert_eq!(r.id(), None);
        assert_eq!(
            r.note("microphone").unwrap(),
            "the microphone chosen here before (USB mic) is not present; using the system default"
        );
        // An empty label never matches an unlabelled entry.
        let r = resolve(Some(&pick("x", "")), &[dev("y", "")], true);
        assert!(matches!(r, Resolved::Missing(_)));
        assert!(r.note("output").unwrap().contains("(x)"));
    }

    #[test]
    fn nothing_stored_is_the_default_and_hidden_ids_are_unknown() {
        assert_eq!(resolve(None, &[dev("a", "A")], true), Resolved::Default);
        let r = resolve(Some(&pick("a", "A")), &[], false);
        assert_eq!(r, Resolved::Unknown(pick("a", "A")));
        assert_eq!(
            r.note("microphone"),
            None,
            "no note before the list can tell"
        );
    }

    #[test]
    fn a_placeholder_name_is_not_stored() {
        let unnamed = Device {
            named: false,
            ..dev("x1", "Microphone 2")
        };
        assert_eq!(DevicePick::of(&unnamed), pick("x1", ""));
        assert_eq!(DevicePick::of(&dev("a", "USB mic")), pick("a", "USB mic"));
    }

    #[test]
    fn a_stored_pick_is_read_tolerantly() {
        assert_eq!(
            DevicePick::parse(r#"{"id":"a","label":"USB mic"}"#),
            Some(pick("a", "USB mic"))
        );
        assert_eq!(
            DevicePick::parse(r#"{"label":"USB mic","extra":1}"#),
            Some(pick("", "USB mic"))
        );
        assert_eq!(DevicePick::parse(r#"{"id":"","label":""}"#), None);
        assert_eq!(DevicePick::parse("not json"), None);
        assert_eq!(DevicePick::parse(""), None);
    }

    #[test]
    fn the_echo_mode_defaults_to_the_input_device() {
        assert_eq!(EchoMode::default(), EchoMode::Device);
        for m in EchoMode::ALL {
            assert_eq!(EchoMode::parse(m.key()), m);
        }
        assert_eq!(EchoMode::parse(""), EchoMode::Device);
        assert_eq!(EchoMode::parse("bogus"), EchoMode::Device);
        assert!(EchoMode::None.half_duplex());
        assert!(!EchoMode::Device.half_duplex());
        assert_eq!(
            EchoMode::Browser.label(true),
            EchoMode::Browser.label(false)
        );
    }

    #[test]
    fn only_browser_mode_asks_for_the_browsers_canceller() {
        for m in [EchoMode::Device, EchoMode::NotNeeded] {
            let c = constraints(m, Want::Default);
            assert_eq!(c["echoCancellation"], false, "{m:?}");
            assert_eq!(c["noiseSuppression"], false, "{m:?}");
            assert_eq!(c["autoGainControl"], false, "{m:?}");
            assert!(c.get("deviceId").is_none());
        }
        // `none`: only the canceller off, as §12.1's table says.
        let c = constraints(EchoMode::None, Want::Default);
        assert_eq!(c["echoCancellation"], false);
        assert!(c.get("noiseSuppression").is_none());
        assert!(c.get("autoGainControl").is_none());
        let c = constraints(EchoMode::Browser, Want::Exact("a"));
        assert_eq!(c["echoCancellation"], true);
        assert!(c.get("noiseSuppression").is_none());
        assert_eq!(c["deviceId"], json!({ "exact": "a" }));
        let c = constraints(EchoMode::Device, Want::Ideal("a"));
        assert_eq!(c["deviceId"], json!({ "ideal": "a" }));
        assert_eq!(c["channelCount"], json!({ "ideal": 1 }));
    }

    #[test]
    fn the_echo_warning_applies_in_device_mode_off_the_default_output() {
        let headphones = Resolved::Found {
            device: dev("hp", "Headphones"),
            by_label: false,
        };
        assert_eq!(
            echo_warning(EchoMode::Device, &headphones).unwrap(),
            "your input cancels echo from the default output; lmgw plays on Headphones"
        );
        // Not in the other modes.
        for m in [EchoMode::Browser, EchoMode::NotNeeded, EchoMode::None] {
            assert_eq!(echo_warning(m, &headphones), None, "{m:?}");
        }
        // Not on the system default, chosen as such or as the device that
        // is the default, nor when the choice fell back to the default.
        assert_eq!(echo_warning(EchoMode::Device, &Resolved::Default), None);
        let the_default = Resolved::Found {
            device: Device {
                is_default: true,
                ..dev("spk", "Speakers")
            },
            by_label: false,
        };
        assert_eq!(echo_warning(EchoMode::Device, &the_default), None);
        assert_eq!(
            echo_warning(
                EchoMode::Device,
                &Resolved::Missing(pick("hp", "Headphones"))
            ),
            None
        );
        assert_eq!(
            echo_warning(
                EchoMode::Device,
                &Resolved::Unknown(pick("hp", "Headphones"))
            ),
            None
        );
    }
}
