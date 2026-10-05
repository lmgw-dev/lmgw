//! Audio in the page (chat-voice §11, §12): the one playback context and its
//! player, the microphone's capture, the device lists, the window's device
//! choice and echo mode, and the bridge to the app shell's output routing.
//!
//! - [`player`] — one `AudioContext({sampleRate: 24000})` per page, created at
//!   the first voice press and kept; `assets/voice/player-worklet.js` plays
//!   queued PCM16 chunks per item and reports what was played; the graph is
//!   player → gain → `AnalyserNode` (the output tap) → destination.
//! - [`capture`] — `getUserMedia` with the echo mode's constraints into a
//!   context at the device's own rate; `assets/voice/capture-worklet.js`
//!   resamples to 24 kHz (realtime) or 16 kHz (dictation) and posts 40 ms
//!   PCM16 chunks; a mic `AnalyserNode` taps the source. Every track is
//!   stopped the moment a capture stops, and on `pagehide`.
//! - [`listing`], [`shell`] — what the device popover lists, and the shell's
//!   `audio_outputs` / `audio_output_set` in the app window.
//! - [`devices`], [`pcm`] — the pure rules and formats, tested natively.
//!
//! The device choice and echo mode live in this browser profile's
//! `localStorage` (§2.4); this module reads and writes them.

pub(crate) mod capture;
pub(crate) mod devices;
pub(crate) mod listing;
pub(crate) mod pcm;
pub(crate) mod player;
pub(crate) mod shell;

use devices::{DevicePick, EchoMode, KEY_ECHO, KEY_INPUT, KEY_OUTPUT};

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn read(key: &str) -> Option<String> {
    storage()?.get_item(key).ok().flatten()
}

/// The window's chosen microphone (`None`: the system default).
pub(crate) fn stored_input() -> Option<DevicePick> {
    read(KEY_INPUT).and_then(|v| DevicePick::parse(&v))
}

/// The window's chosen output (`None`: the system default).
pub(crate) fn stored_output() -> Option<DevicePick> {
    read(KEY_OUTPUT).and_then(|v| DevicePick::parse(&v))
}

/// The window's echo mode (default: the input device cancels echo).
pub(crate) fn stored_echo() -> EchoMode {
    read(KEY_ECHO)
        .map(|v| EchoMode::parse(&v))
        .unwrap_or_default()
}

/// Store a device choice; `None` goes back to the system default. A storage
/// that refuses (private mode, a full quota) only means it is not kept.
pub(crate) fn store_pick(key: &str, pick: Option<&DevicePick>) {
    let Some(s) = storage() else { return };
    let _ = match pick {
        Some(p) => s.set_item(key, &serde_json::to_string(p).unwrap_or_default()),
        None => s.remove_item(key),
    };
}

pub(crate) fn store_echo(mode: EchoMode) {
    if let Some(s) = storage() {
        let _ = s.set_item(KEY_ECHO, mode.key());
    }
}
