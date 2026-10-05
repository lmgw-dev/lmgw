//! Where the player's context plays (chat-voice §12.3, §12.4).
//!
//! - **In the app** the shell moves lmgw's own streams to the window's chosen
//!   sink (`audio_output_set`), resolved against `audio_outputs` by node name,
//!   then label. Without pipewire-utils nothing can be routed: "System
//!   default" is then simply where it plays, and a chosen output says once
//!   that it cannot be used.
//! - **In a browser with `setSinkId`** the context is *made* on the stored
//!   output ([`context_options`]: `sinkId` in `AudioContextOptions`), so its
//!   first sample already plays there; `setSinkId` confirms it later or falls
//!   back to the system default, saying so.
//! - Elsewhere it plays on the system default.
//!
//! One application runs at a time ([`Serial`]): a choice that changes while
//! one runs makes it run once more, reading the choice afresh, so the last
//! choice is the one that lands, whatever order the shell's writes finish in.

use wasm_bindgen::{JsCast, JsValue};

use super::super::devices::{resolve, DevicePick, Resolved};
use super::super::{listing, shell, stored_output};

/// One route application at a time (review M2): see the module docs.
#[derive(Debug, Default)]
pub(super) struct Serial {
    busy: bool,
    again: bool,
}

impl Serial {
    /// A route is wanted. `true`: start applying it (none runs); `false`: the
    /// running one runs again when it finishes.
    pub(super) fn want(&mut self) -> bool {
        if self.busy {
            self.again = true;
            false
        } else {
            self.busy = true;
            true
        }
    }

    /// One application finished. `true`: run it again (the choice changed
    /// meanwhile); `false`: done, its outcome is the one to show.
    pub(super) fn done(&mut self) -> bool {
        if self.again {
            self.again = false;
            true
        } else {
            self.busy = false;
            false
        }
    }
}

/// The playback context's options: 24 kHz, and with `with_sink` the stored
/// output's id as `sinkId` (a browser that can route a context), so the
/// context starts on it (review M1).
pub(super) fn context_options(rate: f32, with_sink: bool) -> web_sys::AudioContextOptions {
    let opts = web_sys::AudioContextOptions::new();
    opts.set_sample_rate(rate);
    if with_sink {
        if let Some(id) = stored_sink_id() {
            let _ = js_sys::Reflect::set(&opts, &"sinkId".into(), &JsValue::from_str(&id));
        }
    }
    opts
}

/// The stored output's id, where the browser can route a context to it.
pub(super) fn stored_sink_id() -> Option<String> {
    if shell::in_shell() || !listing::sink_id_supported() {
        return None;
    }
    stored_output().map(|p| p.id).filter(|id| !id.is_empty())
}

/// Apply the window's chosen output to `ctx` now; `Ok` says where it plays.
pub(super) async fn apply(ctx: &web_sys::AudioContext) -> Result<String, String> {
    let pick = stored_output();
    if shell::in_shell() {
        return in_shell(pick).await;
    }
    if !listing::sink_id_supported() {
        return Ok("the system default".into());
    }
    let raw = listing::raw_devices().await.unwrap_or_default();
    let (outs, known) = listing::of_kind(&raw, "audiooutput");
    let resolved = resolve(pick.as_ref(), &outs, known);
    let id = match &resolved {
        Resolved::Found { device, .. } => device.id.clone(),
        // Not confirmable yet: ask for the stored id, the default if the
        // browser refuses it.
        Resolved::Unknown(p) => p.id.clone(),
        _ => String::new(),
    };
    match set_sink_id(ctx, &id).await {
        Ok(()) => Ok(where_to(&resolved)),
        Err(e) if !id.is_empty() => {
            set_sink_id(ctx, "").await?;
            Err(format!(
                "the chosen output could not be used ({e}); playing on the system default"
            ))
        }
        Err(e) => Err(e),
    }
}

async fn in_shell(pick: Option<DevicePick>) -> Result<String, String> {
    let outs = shell::audio_outputs().await?;
    if let Some(outcome) = unroutable(&outs, pick.as_ref()) {
        return outcome;
    }
    let resolved = resolve(pick.as_ref(), &listing::of_shell(&outs), true);
    shell::audio_output_set(resolved.id()).await?;
    Ok(where_to(&resolved))
}

/// Without pipewire-utils (or with no sink listed) nothing can be routed,
/// and the system default is where lmgw plays: quietly when that is the
/// choice, said once when another output was chosen (review m4). `None`:
/// routing can go ahead.
fn unroutable(outs: &shell::Outputs, pick: Option<&DevicePick>) -> Option<Result<String, String>> {
    if outs.note.is_none() && !outs.outputs.is_empty() {
        return None;
    }
    let why = outs.note.as_deref().unwrap_or("no output is listed");
    Some(match pick {
        None => Ok("the system default".into()),
        Some(p) => Err(format!(
            "{why}: the chosen output ({}) cannot be used; playing on the system default",
            if p.label.is_empty() { &p.id } else { &p.label }
        )),
    })
}

/// Where the route sends lmgw, in the page's words.
fn where_to(r: &Resolved) -> String {
    match r {
        Resolved::Found { device, .. } => device.label.clone(),
        Resolved::Missing(p) => format!(
            "the system default (the chosen output, {}, is not present)",
            if p.label.is_empty() { &p.id } else { &p.label }
        ),
        _ => "the system default".into(),
    }
}

async fn set_sink_id(ctx: &web_sys::AudioContext, id: &str) -> Result<(), String> {
    let f = js_sys::Reflect::get(ctx, &JsValue::from_str("setSinkId"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or("this browser cannot choose an output")?;
    let p = f
        .call1(ctx, &JsValue::from_str(id))
        .map_err(|e| shell::js_text(&e))?;
    let p = p
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| "setSinkId answered no promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(p)
        .await
        .map(|_| ())
        .map_err(|e| shell::js_text(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_pipewire_utils_the_default_is_quiet_and_a_choice_is_said() {
        let none = shell::Outputs {
            outputs: Vec::new(),
            note: Some("system default only: pipewire-utils not found".into()),
        };
        assert_eq!(
            unroutable(&none, None),
            Some(Ok("the system default".to_string()))
        );
        let hp = DevicePick {
            id: "alsa_output.usb".into(),
            label: "Headphones".into(),
        };
        let e = unroutable(&none, Some(&hp)).unwrap().unwrap_err();
        assert!(e.starts_with("system default only: pipewire-utils not found"));
        assert!(e.contains("(Headphones) cannot be used"));
        // Utils present and a sink listed: route.
        let some = shell::Outputs {
            outputs: vec![shell::Output {
                name: "s".into(),
                ..Default::default()
            }],
            note: None,
        };
        assert_eq!(unroutable(&some, Some(&hp)), None);
        assert_eq!(unroutable(&some, None), None);
    }

    #[test]
    fn one_application_runs_and_changes_meanwhile_run_it_once_more() {
        let mut s = Serial::default();
        assert!(s.want(), "the first starts");
        // A resume storm and two output changes while it runs.
        for _ in 0..13 {
            assert!(!s.want());
        }
        assert!(s.done(), "one more run, reading the newest choice");
        assert!(!s.done(), "then it is done");
        assert!(s.want(), "a later change starts a new one");
        assert!(!s.done());
    }
}
