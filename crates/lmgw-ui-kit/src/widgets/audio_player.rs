//! The kit's audio player row: a play/pause button, a scrubber and the time,
//! over a hidden `<audio>`. The browser's own controls are a different widget
//! in every engine (WebKitGTK's is a grey GTK bar), so a clip in the dashboard
//! looks the same everywhere. One player playing does not stop another yet.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icon::{Ico, Icon};

/// `m:ss`, `h:mm:ss` from an hour on; the placeholder while the length is
/// not known (a stream has none).
fn clock(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "–:––".to_string();
    }
    let total = seconds as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The clip at `src`: Play/Pause, a scrubber and "position / length". A new
/// `src` resets the position. The arrow keys on the scrubber seek 5 s (or a
/// twentieth of a shorter clip), PageUp/PageDown a tenth, Home/End go to the
/// ends.
///
/// A `src` of `None` is no clip: Play and the scrubber are disabled and the
/// time reads "–:––". A clip the browser cannot load shows a failed state:
/// the button takes the error colour and says so in its name and tooltip, and
/// the time is replaced by "Can't play this clip".
#[component]
pub fn AudioPlayer(
    #[prop(into)] src: Signal<Option<String>>,
    #[prop(optional)] autoplay: bool,
    /// What the clip is, for the button's tooltip.
    #[prop(optional, into)]
    label: Option<TextProp>,
    /// `"none"` or `"metadata"`: how much the browser fetches before Play.
    #[prop(optional)]
    preload: Option<&'static str>,
) -> impl IntoView {
    let audio: NodeRef<html::Audio> = NodeRef::new();
    let position = RwSignal::new(0.0_f64);
    let length = RwSignal::new(f64::NAN);
    let playing = RwSignal::new(false);
    let failed = RwSignal::new(false);

    // A different clip starts over; the element reloads on its own.
    Effect::new(move |_| {
        src.track();
        position.set(0.0);
        length.set(f64::NAN);
        playing.set(false);
        failed.set(false);
    });
    let no_clip = move || src.with(Option::is_none);

    let read_length = move || {
        if let Some(a) = audio.get_untracked() {
            length.set(a.duration());
        }
    };
    let toggle = move |_| {
        let Some(a) = audio.get_untracked() else {
            return;
        };
        if a.paused() {
            if let Ok(p) = a.play() {
                // A refused play (no gesture, a bad source) shows as the
                // button staying on Play.
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = wasm_bindgen_futures::JsFuture::from(p).await;
                });
            }
        } else {
            let _ = a.pause();
        }
    };
    let seek = move |ev: web_sys::Event| {
        let Some(v) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            .and_then(|i| i.value().parse::<f64>().ok())
        else {
            return;
        };
        position.set(v);
        if let Some(a) = audio.get_untracked() {
            a.set_current_time(v);
        }
    };
    let known = move || length.get().is_finite() && length.get() > 0.0;
    let action = move || {
        if failed.get() {
            "The clip could not be loaded"
        } else if playing.get() {
            "Pause"
        } else {
            "Play"
        }
    };
    let seek_to = move |to: f64| {
        let to = to.clamp(0.0, length.get_untracked());
        position.set(to);
        if let Some(a) = audio.get_untracked() {
            a.set_current_time(to);
        }
    };
    // A range's own step is 10 ms (the drag's grain): keys move by clip.
    let key = move |ev: web_sys::KeyboardEvent| {
        let len = length.get_untracked();
        if !len.is_finite() || len <= 0.0 {
            return;
        }
        let at = position.get_untracked();
        let step = (len / 20.0).min(5.0);
        let to = match ev.key().as_str() {
            "ArrowRight" | "ArrowUp" => at + step,
            "ArrowLeft" | "ArrowDown" => at - step,
            "PageUp" => at + len / 10.0,
            "PageDown" => at - len / 10.0,
            "Home" => 0.0,
            "End" => len,
            _ => return,
        };
        ev.prevent_default();
        seek_to(to);
    };

    view! {
        <div class="audio-player" class:failed=move || failed.get()>
            <audio
                node_ref=audio
                src=move || src.get()
                preload=preload
                autoplay=autoplay
                on:timeupdate=move |_| {
                    if let Some(a) = audio.get_untracked() {
                        position.set(a.current_time());
                    }
                }
                on:loadedmetadata=move |_| read_length()
                on:durationchange=move |_| read_length()
                on:play=move |_| playing.set(true)
                on:pause=move |_| playing.set(false)
                on:ended=move |_| playing.set(false)
                on:error=move |_| failed.set(true)
            ></audio>
            <button
                type="button"
                class="icon-btn sm"
                aria-label=action
                title=move || {
                    if failed.get() {
                        return action().to_string();
                    }
                    label.as_ref().map(|l| l.get().to_string()).unwrap_or_else(|| action().to_string())
                }
                disabled=move || no_clip() || failed.get()
                on:click=toggle
            >
                {move || {
                    let ico = if failed.get() {
                        Ico::Close
                    } else if playing.get() {
                        Ico::Pause
                    } else {
                        Ico::Play
                    };
                    view! { <Icon ico=ico/> }
                }}
            </button>
            <input
                type="range"
                class="audio-scrub"
                aria-label="Position"
                min="0"
                step="0.01"
                max=move || if known() { length.get().to_string() } else { "0".to_string() }
                prop:value=move || position.get().to_string()
                aria-valuetext=move || {
                    format!("{} of {}", clock(position.get()), clock(length.get()))
                }
                disabled=move || !known() || failed.get()
                on:keydown=key
                on:input=seek
            />
            <span class="audio-time mono-sm">
                {move || {
                    if failed.get() {
                        "Can't play this clip".to_string()
                    } else if no_clip() {
                        clock(f64::NAN)
                    } else {
                        format!("{} / {}", clock(position.get()), clock(length.get()))
                    }
                }}
            </span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn the_time_reads_m_ss_and_h_mm_ss() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(3.4), "0:03");
        assert_eq!(clock(72.0), "1:12");
        assert_eq!(clock(3600.0), "1:00:00");
        assert_eq!(clock(3725.9), "1:02:05");
        assert_eq!(clock(f64::NAN), "–:––");
        assert_eq!(clock(f64::INFINITY), "–:––");
    }
}
