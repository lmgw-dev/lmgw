//! The realtime panel (chat-voice §9.1): in the composer's place, from the
//! top — the visualisation (§10), the captions line, the chips (§9.4), the
//! controls (mute, automatic / push-to-talk, stop talking, leave), and a
//! status line for loading, the hold, fallbacks, errors and the last turn's
//! timing.
//!
//! The panel root carries `data-voice-state` (§9.3) and the session's
//! counters as `data-voice-*` for the drive tests. Its buttons do not take
//! the focus on a pointer press, so Space never re-presses one (§9.2).
//!
//! **Focus view**: the panel takes the column and the transcript steps
//! aside (the orb by default there); the visualisation variant is chosen
//! per view, in this window.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::super::badges::timing_items;
use super::super::state::{under_block, NoteKind};
use super::super::viz::{store_choice, stored_choice, Variant, Viz, VizFeed};
use super::chips::Chips;
use super::machine::{Tone, Who};
use super::{use_realtime, Mic, Phase, Realtime};
use crate::widgets::Popover;

const ICON_MIC: &str = "M8 1.8a2.2 2.2 0 0 1 2.2 2.2v4a2.2 2.2 0 0 1-4.4 0V4A2.2 2.2 0 0 1 8 1.8z M3.8 7.6a4.2 4.2 0 0 0 8.4 0 M8 11.8v2.4 M5.8 14.2h4.4";
const ICON_SLASH: &str = "M2.5 2.5l11 11";
const ICON_STOP: &str = "M4.5 4.5h7v7h-7z";
const ICON_LEAVE: &str =
    "M2.2 9.6c3.4-3 8.2-3 11.6 0l-1.3 1.9-2.3-.9V8.9a7 7 0 0 0-4.4 0v1.7l-2.3.9z";
const ICON_FOCUS: &str = "M2.5 6V2.5H6 M10 2.5h3.5V6 M13.5 10v3.5H10 M6 13.5H2.5V10";
const ICON_UNFOCUS: &str = "M6 2.5V6H2.5 M13.5 6H10V2.5 M10 13.5V10h3.5 M2.5 10H6v3.5";

/// `mm:ss` since the session went live.
fn clock(ms: f64) -> String {
    let s = (ms.max(0.0) / 1000.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// The panel, while voice mode is on.
#[component]
pub(crate) fn RealtimePanel() -> impl IntoView {
    let Some(rt) = use_realtime() else {
        return ().into_any();
    };
    let pv = rt.pv;
    let choice = RwSignal::new(stored_choice());
    let variant = Signal::derive(move || choice.get().of(rt.focus.get()));
    // The session clock, ticking while the panel shows.
    let now = RwSignal::new(js_sys::Date::now());
    let tick = set_interval_with_handle(
        move || {
            now.try_set(js_sys::Date::now());
        },
        std::time::Duration::from_millis(1000),
    )
    .ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    let loading = Signal::derive(move || {
        rt.status.notes().with(|n| {
            n.iter()
                .find(|n| n.kind == NoteKind::Busy)
                .map(|n| n.text.clone())
        })
    });
    let held = Signal::derive(move || {
        let b = pv.block.get();
        rt.parts.current.with(|c| {
            c.as_ref()
                .and_then(|t| t.voice_resolved.as_ref())
                .is_some_and(|r| b.blocks(&r.asr) || b.blocks(&r.tts))
        })
    });
    let timing_json = Signal::derive(move || {
        rt.timing.with(|t| {
            t.as_ref()
                .map(|t| serde_json::to_value(TimingJson(t)).unwrap_or_default())
        })
    });
    let feed = VizFeed {
        state: rt.state.into(),
        muted: rt.muted.into(),
        loading,
        held,
        output: rt.output,
        input: rt.input,
        timing: timing_json,
        variant,
    };
    let live = Memo::new(move |_| matches!(rt.phase.get(), Phase::Live | Phase::Connecting));
    view! {
        <div class="rt-dock" class:focus=move || rt.focus.get()>
            <div
                class="rt-panel"
                data-rt-panel=""
                data-voice-state=move || rt.state.get().key()
                data-voice-phase=move || rt.phase.get().key()
                data-voice-mic=move || rt.mic.get().key()
                data-voice-ptt=move || rt.ptt.get().to_string()
                data-voice-muted=move || rt.muted.get().to_string()
                data-voice-chunks=move || rt.chunks.get().to_string()
                data-voice-audio-bytes=move || rt.audio_bytes.get().to_string()
                data-voice-played=move || rt.probe.with(|p| p.played.to_string())
                data-voice-truncated=move || rt.probe.with(|p| p.truncated_ms.map(|m| m.to_string()).unwrap_or_default())
                data-voice-truncates=move || rt.probe.with(|p| p.truncates.to_string())
                data-voice-cut-played=move || rt.probe.with(|p| p.cut_played_ms.map(|m| m.to_string()).unwrap_or_default())
                data-voice-truncated-ack=move || rt.probe.with(|p| p.truncated_ack_ms.map(|m| m.to_string()).unwrap_or_default())
                data-voice-commits=move || rt.probe.with(|p| p.commits.to_string())
                data-voice-heard=move || rt.heard.get()
                data-voice-spoken=move || rt.spoken.get()
                data-viz=move || variant.get().key()
                role="region"
                aria-label="Voice mode"
                // Focused on entry, so Space, M and Esc act here (review m5).
                tabindex="-1"
            >
                <div class="rt-stage">
                    <Viz feed=feed/>
                    <VizTools rt=rt choice=choice variant=variant/>
                </div>
                <CaptionLine rt=rt/>
                // The captions line changes with every paced word; this says
                // the user's words once final, and a cut (review m8).
                <div class="sr-only" aria-live="polite" data-rt-announce="">
                    {move || rt.announce.get()}
                </div>
                <Chips rt=rt/>
                <Controls rt=rt live=live now=now/>
                <StatusLine rt=rt/>
            </div>
        </div>
    }
    .into_any()
}

/// The timing as the visualisation's `setTiming` takes it (the event's
/// shape).
struct TimingJson<'a>(&'a super::super::spoken::VoiceTiming);

impl serde::Serialize for TimingJson<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let t = self.0;
        serde_json::json!({
            "end_of_turn_ms": t.end_of_turn_ms, "asr_ms": t.asr_ms,
            "first_token_ms": t.first_token_ms, "first_clause_ms": t.first_clause_ms,
            "first_audio_ms": t.first_audio_ms, "total_ms": t.total_ms,
            "to_first_audio_ms": t.to_first_audio_ms, "cold": t.cold,
        })
        .serialize(s)
    }
}

/// The variant menu and the focus toggle, over the visualisation's corner.
#[component]
fn VizTools(
    rt: Realtime,
    choice: RwSignal<super::super::viz::VizChoice>,
    variant: Signal<Variant>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let pick = move |v: Variant| {
        let c = choice.get_untracked().with(rt.focus.get_untracked(), v);
        store_choice(c);
        choice.set(c);
        open.set(false);
    };
    view! {
        <div class="rt-viz-tools">
            <button
                type="button"
                node_ref=anchor
                class="rt-tool"
                data-rt-viz-menu=""
                title=move || format!(
                    "Visualisation in the {}: {} — click to choose another (kept for this window)",
                    if rt.focus.get() { "focus view" } else { "chat panel" },
                    variant.get().label()
                )
                aria-haspopup="dialog"
                aria-expanded=move || open.get().to_string()
                on:mousedown=|ev| ev.prevent_default()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                {move || variant.get().label()}
                <span class="select-arrow">"▾"</span>
            </button>
            <button
                type="button"
                class="rt-tool rt-tool-icon"
                data-rt-focus=move || rt.focus.get().to_string()
                title=move || if rt.focus.get() {
                    "Back to the chat panel: the transcript shows again"
                } else {
                    "Focus view: the panel takes the column, the transcript steps aside"
                }
                aria-pressed=move || rt.focus.get().to_string()
                on:mousedown=|ev| ev.prevent_default()
                on:click=move |_| rt.focus.update(|f| *f = !*f)
            >
                <svg viewBox="0 0 16 16" aria-hidden="true">
                    <path d=move || if rt.focus.get() { ICON_UNFOCUS } else { ICON_FOCUS }></path>
                </svg>
            </button>
        </div>
        <Popover open=open anchor=anchor class="rt-pop" min_width=280>
            <div class="vd-section" role="radiogroup" aria-label="Visualisation">
                <div class="vd-head">
                    {move || if rt.focus.get() { "Visualisation · focus view" } else { "Visualisation · chat panel" }}
                </div>
                {Variant::ALL
                    .into_iter()
                    .map(|v| view! {
                        <button
                            type="button"
                            role="radio"
                            class="echo-mode"
                            class:sel=move || variant.get() == v
                            aria-checked=move || (variant.get() == v).to_string()
                            data-viz-pick=v.key()
                            on:click=move |_| pick(v)
                        >
                            <span class="echo-mode-name">{v.label()}</span>
                            <span class="echo-mode-hint">{v.hint()}</span>
                        </button>
                    })
                    .collect_view()}
            </div>
        </Popover>
    }
}

/// One line: the user's words, then the reply's spoken words, the newest
/// winning (clipped at the left).
#[component]
fn CaptionLine(rt: Realtime) -> impl IntoView {
    let c = rt.caption;
    // The words fade in at the left only when they do not fit.
    let clip: NodeRef<html::Span> = NodeRef::new();
    let text: NodeRef<html::Span> = NodeRef::new();
    let clipped = RwSignal::new(false);
    Effect::new(move |_| {
        c.track();
        rt.focus.track();
        // The frame may come after the panel went (a leave right after a
        // caption change): its node refs are gone then.
        request_animation_frame(move || {
            if let (Some(cl), Some(t)) = (
                clip.try_get_untracked().flatten(),
                text.try_get_untracked().flatten(),
            ) {
                clipped.try_set(t.offset_width() > cl.client_width() + 1);
            }
        });
    });
    view! {
        <div class="rt-caption" data-rt-caption="" aria-live="off">
            <div class="cap-inner">
                {move || c.with(|c| c.who).map(|w| {
                    let (cls, label) = match w {
                        Who::User => ("cap-who user", "You"),
                        Who::Assistant => ("cap-who assistant", "Assistant"),
                    };
                    view! { <span class=cls>{label}</span> }
                })}
                <span class="cap-clip" class:clipped=move || clipped.get() node_ref=clip>
                    <span
                        node_ref=text
                        class="cap-text"
                        class:hint=move || c.with(|c| c.tone == Tone::Hint)
                        class:past=move || c.with(|c| c.tone == Tone::Past)
                    >
                        {move || c.with(|c| c.text.clone())}
                    </span>
                </span>
                {move || c.with(|c| c.tag).map(|t| view! { <span class="cap-tag">{t}</span> })}
            </div>
        </div>
    }
}

#[component]
fn Controls(rt: Realtime, live: Memo<bool>, now: RwSignal<f64>) -> impl IntoView {
    let state = rt.state;
    view! {
        <div class="rt-controls">
            <span class="chip state-chip" data-s=move || state.get().key()>
                <span class="dot"></span>
                <span class="state-label">{move || state.get().label()}</span>
            </span>
            <span class="mono rt-timer">
                {move || match rt.since.get() {
                    Some(t) => clock(now.get() - t),
                    None => "--:--".into(),
                }}
            </span>
            <span class="spacer"></span>
            <button
                type="button"
                class="btn sm rt-mute"
                class:on=move || rt.muted.get()
                disabled=move || !live.get()
                title=move || if rt.muted.get() { "Unmute the microphone (M)" } else { "Mute the microphone (M): silence flows, so an open turn ends" }
                aria-pressed=move || rt.muted.get().to_string()
                data-rt-mute=""
                on:mousedown=|ev| ev.prevent_default()
                on:click=move |_| rt.toggle_mute()
            >
                <svg class="ico" viewBox="0 0 16 16" aria-hidden="true">
                    <path d=ICON_MIC></path>
                    <path class="slash" d=ICON_SLASH></path>
                </svg>
                <span>{move || if rt.muted.get() { "Unmute" } else { "Mute" }}</span>
            </button>
            <div class="seg rt-mode" role="group" aria-label="Turn detection">
                <button
                    type="button"
                    class="seg-btn"
                    class:active=move || !rt.ptt.get()
                    disabled=move || !live.get()
                    title="Automatic: the gateway hears when you start and stop (the thread's turn detection)"
                    data-rt-mode="auto"
                    on:mousedown=|ev| ev.prevent_default()
                    on:click=move |_| rt.set_ptt(false)
                >
                    "Auto"
                </button>
                <button
                    type="button"
                    class="seg-btn"
                    class:active=move || rt.ptt.get()
                    disabled=move || !live.get()
                    title="Push to talk: hold Space (or the Talk button) while you speak"
                    data-rt-mode="ptt"
                    on:mousedown=|ev| ev.prevent_default()
                    on:click=move |_| rt.set_ptt(true)
                >
                    "Push to talk"
                </button>
            </div>
            <Show when=move || rt.ptt.get()>
                <button
                    type="button"
                    class="btn sm rt-talk"
                    class:on=move || rt.talking.get()
                    disabled=move || !live.get()
                    title="Hold while you speak (or hold Space)"
                    data-rt-talk=""
                    on:mousedown=|ev| ev.prevent_default()
                    on:pointerdown=move |ev: web_sys::PointerEvent| {
                        if ev.button() == 0 {
                            ev.prevent_default();
                            // Its release reaches the button wherever it
                            // happens: drifting off it does not end the
                            // utterance mid-sentence (review NIT 6).
                            if let Some(el) = ev
                                .current_target()
                                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            {
                                let _ = el.set_pointer_capture(ev.pointer_id());
                            }
                            rt.talk_down();
                        }
                    }
                    on:pointerup=move |_| rt.talk_up()
                    on:pointercancel=move |_| rt.talk_up()
                    on:lostpointercapture=move |_| rt.talk_up()
                >
                    <svg class="ico" viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_MIC></path></svg>
                    <span>"Talk"</span>
                </button>
            </Show>
            <button
                type="button"
                class="btn sm rt-stop"
                disabled=move || !live.get() || !rt.can_stop()
                title="Stop the voice: what was not heard is kept greyed, not sent to the model (Space in automatic mode)"
                data-rt-stop=""
                on:mousedown=|ev| ev.prevent_default()
                on:click=move |_| rt.stop_talking()
            >
                <svg class="ico" viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_STOP></path></svg>
                <span>"Stop talking"</span>
            </button>
            <button
                type="button"
                class="btn sm danger rt-leave"
                title="Leave voice mode (Esc): the microphone is released, the composer comes back"
                data-rt-leave=""
                on:mousedown=|ev| ev.prevent_default()
                on:click=move |_| rt.leave()
            >
                <svg class="ico" viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_LEAVE></path></svg>
                <span>"Leave"</span>
            </button>
        </div>
    }
}

/// Loading, the hold, fallbacks and errors (the panel's own notes, §9.7.7),
/// what a block does to its models before anything is said (§2.3), the
/// running tool, an end with Re-enter, and the last turn's timing (§9.5).
#[component]
fn StatusLine(rt: Realtime) -> impl IntoView {
    let pv = rt.pv;
    let line = move |stage: &'static str, what: &'static str| {
        Memo::new(move |_| {
            let b = pv.block.get();
            rt.parts.current.with(|c| {
                let r = c.as_ref()?.voice_resolved.as_ref()?;
                let s = if stage == "asr" { &r.asr } else { &r.tts };
                b.blocks(s)
                    .then(|| under_block(s, b))
                    .flatten()
                    .map(|l| format!("{what}: {l}"))
            })
        })
    };
    let asr_hold = line("asr", "speech-to-text");
    let tts_hold = line("tts", "text-to-speech");
    let ended = Memo::new(move |_| match rt.phase.get() {
        Phase::Ended(why) => Some(why),
        _ => None,
    });
    view! {
        // The end's note is an alert of its own, beside the status region, not
        // inside it: nested live regions are said twice by some readers
        // (review NIT 7).
        <div class="rt-status">
            {move || ended.get().map(|why| view! {
                <span class="vs-note err rt-ended" data-rt-ended="" role="alert">
                    {format!("voice session ended: {why}")}
                    <button type="button" class="btn primary sm" data-rt-reenter=""
                        title="Open a new voice session for this conversation (nothing reconnects by itself)"
                        on:click=move |_| rt.reenter()>
                        "Re-enter"
                    </button>
                </span>
            })}
            <div class="rt-notes" data-rt-status="" role="status">
            <Show when=move || rt.phase.get() == Phase::Connecting>
                <span class="vs-note busy" data-rt-connecting="">
                    {move || match rt.mic.get() {
                        Mic::Opening => "opening the microphone…",
                        _ => "connecting the voice session…",
                    }}
                </span>
            </Show>
            {move || asr_hold.get().map(|l| view! { <span class="vs-note warn" data-hold-line="asr">{l}</span> })}
            {move || tts_hold.get().map(|l| view! { <span class="vs-note warn" data-hold-line="tts">{l}</span> })}
            {move || rt.tool.get().map(|t| view! {
                <span class="vs-note busy" data-rt-tool="">{format!("running {t}…")}</span>
            })}
            <For each=move || rt.status.shown() key=|n| (n.key.clone(), n.text.clone()) let:n>
                {
                    let dismissable = matches!(n.kind, NoteKind::Error | NoteKind::Warn | NoteKind::Hold | NoteKind::Info);
                    view! {
                        <span class=format!("vs-note {}", n.kind.class()) data-note=n.key.clone()>
                            {n.text.clone()}
                            {n.link.map(|(href, label)| view! {
                                " " <a class="link-btn" href=href data-note-link="">{label}</a>
                            })}
                            {dismissable.then(|| {
                                let n = n.clone();
                                view! {
                                    <button type="button" class="vs-x" aria-label="Dismiss"
                                        on:click=move |_| rt.status.dismiss(&n)>
                                        "✕"
                                    </button>
                                }
                            })}
                        </span>
                    }
                }
            </For>
            </div>
            {move || rt.timing.get().map(|t| {
                let details = t.details();
                view! {
                    // Every turn's timing is no news to announce.
                    <details class="voice-timing rt-timing" data-rt-timing="" aria-live="off">
                        <summary class="dim mono-sm">"last turn: "{timing_items(&t)}</summary>
                        <ul class="dim mono-sm">
                            {details.into_iter().map(|d| view! { <li>{d}</li> }).collect_view()}
                        </ul>
                    </details>
                }
            })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn the_session_clock_reads_minutes_and_seconds() {
        assert_eq!(clock(0.0), "00:00");
        assert_eq!(clock(65_400.0), "01:05");
        assert_eq!(clock(-5.0), "00:00");
    }
}
