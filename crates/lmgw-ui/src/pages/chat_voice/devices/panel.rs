//! The devices popover's body: the microphone list with a test and a level
//! meter, the output list with a test tone and the output meter, and the
//! echo chip.
//!
//! The microphone test opens the capture exactly as dictation and realtime
//! do — the window's input, the echo mode's constraints, the 24 kHz worklet —
//! so what the meter shows is what they would send. It is released when the
//! test is stopped and when the popover closes; a microphone that ends on
//! its own (unplugged, permission taken back, page hidden) says so. Its
//! counters are on the root's `data-*` attributes for the media probes
//! (`scripts/media-probe.js`).

use leptos::prelude::*;

use super::super::audio::capture::{self, Capture, CaptureEvent};
use super::super::audio::devices::{Device, DevicePick, Resolved};
use super::super::audio::listing::{self, OutputKind};
use super::super::audio::player::{self, Route};
use super::super::audio::{pcm, shell};
use super::meter::{self, Bar, Meter};
use super::{EchoChip, VoiceDevices};
use crate::scope::Scope;

/// The microphone test.
#[derive(Debug, Clone, PartialEq)]
enum Mic {
    Closed,
    Opening,
    Open(capture::Info),
    Error(String),
    /// It ended on its own: why.
    Ended(String),
}

impl Mic {
    fn key(&self) -> &'static str {
        match self {
            Mic::Closed => "closed",
            Mic::Opening => "opening",
            Mic::Open(_) => "open",
            Mic::Error(_) => "error",
            Mic::Ended(_) => "ended",
        }
    }
}

/// The test tone.
#[derive(Debug, Clone, PartialEq)]
enum Tone {
    Idle,
    Starting,
    Playing,
    Done,
    Error(String),
}

impl Tone {
    fn key(&self) -> &'static str {
        match self {
            Tone::Idle => "idle",
            Tone::Starting => "starting",
            Tone::Playing => "playing",
            Tone::Done => "done",
            Tone::Error(_) => "error",
        }
    }
}

/// The test tone's chunks: 100 ms, so it is queued as several, back to back.
const TONE_CHUNK: usize = 2_400;

#[component]
pub(crate) fn DevicesPanel(dev: VoiceDevices) -> impl IntoView {
    let alive = Scope::new();
    let in_app = shell::in_shell();
    let secure = listing::secure();
    dev.refresh();

    let mic = RwSignal::new(Mic::Closed);
    let mic_muted = RwSignal::new(false);
    let chunks = RwSignal::new(0u64);
    let samples = RwSignal::new(0u64);
    let mic_meter = Meter::new();
    let held = StoredValue::new_local(None::<Capture>);
    let opening = StoredValue::new(0u64);

    let tone = RwSignal::new(Tone::Idle);
    let tone_pushed = RwSignal::new(0u64);
    let tone_played = RwSignal::new(0u64);
    let tone_item = StoredValue::new(None::<u32>);
    let tone_heard = RwSignal::new(0u64);
    let latency = RwSignal::new(0u64);
    let out_meter = Meter::new();
    let meters_on = StoredValue::new(false);
    let route = RwSignal::from(player::status());

    let start_meters = move || {
        if !alive.alive() || meters_on.get_value() {
            return;
        }
        meters_on.set_value(true);
        let mut buf = Vec::new();
        meter::run(alive, move || {
            let mut live = false;
            if let Some(a) = held.with_value(|c| c.as_ref().map(Capture::analyser)) {
                mic_meter.read(&a, &mut buf);
                live = true;
            }
            if tone.get_untracked() == Tone::Playing {
                if let Some(p) = player::existing() {
                    out_meter.read(&p.analyser(), &mut buf);
                    live = true;
                }
            }
            if !live {
                meters_on.set_value(false);
            }
            live
        });
    };

    let close_mic = move || {
        opening.try_update_value(|n| *n += 1);
        held.try_update_value(|c| {
            if let Some(c) = c.take() {
                c.stop();
            }
        });
    };
    let stop_mic = move || {
        close_mic();
        mic.set(Mic::Closed);
        mic_meter.reset();
    };
    let start_mic = move || {
        close_mic();
        let Some(generation) = opening.try_get_value() else {
            return;
        };
        mic.set(Mic::Opening);
        chunks.set(0);
        samples.set(0);
        mic_meter.reset();
        let opts = capture::Options::new(
            pcm::RATE,
            dev.echo.get_untracked(),
            dev.input.get_untracked(),
        );
        let on_chunk = Box::new(move |c: Vec<i16>| {
            chunks.try_update(|n| *n += 1);
            samples.try_update(|n| *n += c.len() as u64);
        });
        // A microphone that ends on its own is already stopped: drop it and
        // say why; a muted one says it delivers silence.
        let on_event = Box::new(move |e: CaptureEvent| match e {
            CaptureEvent::Muted => {
                mic_muted.try_set(true);
            }
            CaptureEvent::Unmuted => {
                mic_muted.try_set(false);
            }
            CaptureEvent::Ended(why) => {
                if opening.try_get_value() == Some(generation) {
                    held.try_update_value(|c| *c = None);
                    mic.try_set(Mic::Ended(why.message()));
                    mic_muted.try_set(false);
                }
            }
        });
        // Not scoped: a microphone that opens after the popover closed must
        // still be released, so this task always sees its answer.
        leptos::task::spawn_local(async move {
            let opened = capture::open(opts, on_chunk, on_event).await;
            let current = alive.alive() && opening.try_get_value() == Some(generation);
            match opened {
                Ok(c) if !current => c.stop(),
                Ok(c) => {
                    let info = c.info().clone();
                    mic_muted.set(c.muted_by_system());
                    held.set_value(Some(c));
                    mic.set(Mic::Open(info));
                    // A grant shows the device names (WebKitGTK: and ids).
                    dev.refresh();
                    start_meters();
                }
                Err(e) if current => mic.set(Mic::Error(e)),
                Err(_) => {}
            }
        });
    };
    // A different input or echo mode reopens a running test with it.
    Effect::new(move |first: Option<()>| {
        dev.input.track();
        dev.echo.track();
        if first.is_some() && matches!(mic.get_untracked(), Mic::Open(_) | Mic::Opening) {
            start_mic();
        }
    });

    // The Chat page's voice, when the popover is the composer's: an open
    // dictation microphone is finished before the tone plays into it.
    let page_voice = super::super::page::use_page_voice();
    let play_tone = move |_| {
        if let Some(pv) = page_voice {
            pv.dictation.before_playback();
        }
        // Made here, in the click: a context made in a gesture may start.
        let ready = player::player();
        tone.set(Tone::Starting);
        tone_pushed.set(0);
        tone_played.set(0);
        tone_heard.set(0);
        out_meter.reset();
        leptos::task::spawn_local(async move {
            // Resolves once the context runs and its output is routed.
            let p = match ready.await {
                Ok(p) => p,
                Err(e) => {
                    tone.try_set(Tone::Error(e));
                    return;
                }
            };
            // The page's one playback: this flushes whatever played.
            let item = p.begin();
            tone_item.try_set_value(Some(item));
            let pcm = pcm::test_tone(pcm::RATE);
            for c in pcm.chunks(TONE_CHUNK) {
                p.push(item, &pcm::le_bytes(c));
            }
            tone_pushed.try_set(pcm.len() as u64);
            latency.try_set(p.latency_samples());
            tone.try_set(Tone::Playing);
            start_meters();
            match p.end(item).await {
                Some(c) => {
                    tone_played.try_set(c.played);
                    tone_heard.try_set(c.heard);
                    tone.try_set(Tone::Done);
                }
                None => {
                    tone.try_set(Tone::Idle);
                }
            }
            p.release(item);
        });
    };

    on_cleanup(move || {
        close_mic();
        if let (Some(Some(item)), Some(p)) = (tone_item.try_get_value(), player::existing()) {
            leptos::task::spawn_local(async move {
                p.flush(Some(item)).await;
            });
        }
    });

    let labels = move || match dev.listing.with(|l| l.as_ref().map(|l| l.inputs_known)) {
        None => "reading",
        Some(true) => "known",
        Some(false) => "hidden",
    };

    view! {
        <div
            class="voice-dev"
            data-voice-devices=""
            data-mic=move || mic.with(Mic::key)
            data-mic-muted=move || mic_muted.get().to_string()
            data-chunks=move || chunks.get().to_string()
            data-samples=move || samples.get().to_string()
            data-rate=pcm::RATE.to_string()
            data-native-rate=move || {
                mic.with(|m| match m {
                    Mic::Open(i) => i.native_rate.to_string(),
                    _ => String::new(),
                })
            }
            data-peak=move || format!("{:.3}", mic_meter.peak.get())
            data-labels=labels
            data-tone=move || tone.with(Tone::key)
            data-tone-pushed=move || tone_pushed.get().to_string()
            data-tone-played=move || tone_played.get().to_string()
            data-tone-heard=move || tone_heard.get().to_string()
            data-latency=move || latency.get().to_string()
            data-out-peak=move || format!("{:.3}", out_meter.peak.get())
            data-player=move || {
                route.with(|s| match (&s.route, s.running) {
                    (Route::None, _) => "none",
                    (_, true) => "running",
                    (_, false) => "suspended",
                })
            }
            data-route=move || {
                route.with(|s| match &s.route {
                    Route::None => "none",
                    Route::Pending => "pending",
                    Route::Ok(_) => "ok",
                    Route::Failed(_) => "failed",
                })
            }
            data-output-kind=move || {
                dev.listing.with(|l| match l.as_ref().map(|l| l.output_kind) {
                    Some(OutputKind::Shell) => "shell",
                    Some(OutputKind::SinkId) => "sinkid",
                    Some(OutputKind::DefaultOnly) => "default",
                    None => "",
                })
            }
        >
            {(!secure)
                .then(|| {
                    view! {
                        <div class="notice warn">
                            "Voice needs https or localhost: this page is not a secure context, so the browser offers no microphone."
                        </div>
                    }
                })}
            <section class="vd-section" data-vd="input">
                <div class="vd-head">"Microphone"</div>
                <InputList dev=dev/>
                <div class="vd-test">
                    {move || {
                        if matches!(mic.get(), Mic::Open(_) | Mic::Opening) {
                            view! {
                                <button type="button" class="btn sm" data-vd-mic="stop" on:click=move |_| stop_mic()>
                                    "Stop"
                                </button>
                            }
                                .into_any()
                        } else {
                            view! {
                                <button
                                    type="button"
                                    class="btn sm"
                                    data-vd-mic="start"
                                    disabled=!secure
                                    title="Open the microphone to see its level (nothing is recorded or sent)"
                                    on:click=move |_| start_mic()
                                >
                                    "Test microphone"
                                </button>
                            }
                                .into_any()
                        }
                    }}
                    <Bar meter=mic_meter label="Microphone level"/>
                </div>
                {move || {
                    mic.with(|m| match m {
                        Mic::Open(i) => {
                            let rates = if i.native_rate == i.rate {
                                format!("{} kHz", i.rate / 1000)
                            } else {
                                format!("{} → {} kHz", fmt_khz(i.native_rate), i.rate / 1000)
                            };
                            let name = if i.label.is_empty() { "the system default" } else { &i.label };
                            Some(view! { <div class="vd-note dim">{format!("{name} · {rates}")}</div> }.into_any())
                        }
                        Mic::Error(e) | Mic::Ended(e) => Some(view! { <div class="vd-note warn">{e.clone()}</div> }.into_any()),
                        _ => None,
                    })
                }}
                {move || {
                    (mic_muted.get() && matches!(mic.get(), Mic::Open(_)))
                        .then(|| view! { <div class="vd-note warn">"The system muted this microphone: it delivers silence."</div> })
                }}
                {move || {
                    mic.with(|m| match m {
                        Mic::Open(i) => i.note.clone(),
                        _ => None,
                    })
                    .map(|n| view! { <div class="vd-note warn">{n}</div> })
                }}
            </section>
            <section class="vd-section" data-vd="output">
                <div class="vd-head">"Output"</div>
                <OutputList dev=dev in_app=in_app/>
                <div class="vd-test">
                    <button
                        type="button"
                        class="btn sm"
                        data-vd-tone=""
                        disabled=move || matches!(tone.get(), Tone::Starting | Tone::Playing)
                        title="Play a short chime on the chosen output"
                        on:click=play_tone
                    >
                        "Play test tone"
                    </button>
                    <Bar meter=out_meter label="Output level"/>
                </div>
                {move || {
                    let r = route.with(|s| s.route.clone());
                    let err = route.with(|s| s.error.clone());
                    match (err, r) {
                        (Some(e), _) => Some(view! { <div class="vd-note warn">{e}</div> }.into_any()),
                        (None, Route::Ok(to)) => {
                            Some(view! { <div class="vd-note dim">{format!("lmgw plays on {to}")}</div> }.into_any())
                        }
                        (None, Route::Failed(e)) => Some(view! { <div class="vd-note warn">{e}</div> }.into_any()),
                        (None, Route::Pending) => Some(view! { <div class="vd-note dim">"routing the output…"</div> }.into_any()),
                        (None, Route::None) => None,
                    }
                }}
                {move || match tone.get() {
                    // A playback that did not start is the player's error,
                    // said above already.
                    Tone::Error(e) if route.with(|s| s.error.as_deref() != Some(e.as_str())) => {
                        Some(view! { <div class="vd-note warn">{e}</div> }.into_any())
                    }
                    _ => None,
                }}
            </section>
            <section class="vd-section" data-vd="echo">
                <div class="vd-head">"Echo"</div>
                <EchoChip dev=dev/>
            </section>
        </div>
    }
}

fn fmt_khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{}", hz / 1000)
    } else {
        format!("{:.1}", f64::from(hz) / 1000.0)
    }
}

/// One choosable row.
#[component]
fn Opt(
    #[prop(into)] label: Signal<String>,
    #[prop(optional_no_strip)] tag: Option<&'static str>,
    #[prop(into)] sel: Signal<bool>,
    #[prop(optional)] disabled: bool,
    on_pick: Callback<()>,
) -> impl IntoView {
    view! {
        <button
            type="button"
            role="radio"
            class="select-opt vd-opt"
            class:sel=move || sel.get()
            aria-checked=move || sel.get().to_string()
            disabled=disabled
            on:click=move |_| on_pick.run(())
        >
            <span class="vd-opt-name">{move || label.get()}</span>
            {tag.map(|t| view! { <span class="vd-tag">{t}</span> })}
        </button>
    }
}

fn picked_default(r: &Resolved) -> bool {
    matches!(r, Resolved::Default | Resolved::Missing(_))
}

fn picked(r: &Resolved, d: &Device) -> bool {
    r.id() == Some(d.id.as_str())
}

#[component]
fn InputList(dev: VoiceDevices) -> impl IntoView {
    let resolved = Memo::new(move |_| dev.input_resolved());
    view! {
        <div class="vd-list" role="radiogroup" aria-label="Microphone">
            <Opt
                label="System default".to_string()
                sel=Signal::derive(move || resolved.with(picked_default))
                on_pick=Callback::new(move |()| dev.set_input(None))
            />
            {move || {
                let l = dev.listing.get();
                let r = resolved.get();
                let mut rows = Vec::new();
                if let Resolved::Unknown(p) = &r {
                    rows.push(
                        view! {
                            <Opt
                                label=format!("{} (chosen here before)", if p.label.is_empty() { &p.id } else { &p.label })
                                sel=Signal::derive(|| true)
                                disabled=true
                                on_pick=Callback::new(|()| {})
                            />
                        }
                            .into_any(),
                    );
                }
                for d in l.as_ref().map(|l| l.inputs.clone()).unwrap_or_default() {
                    let sel = picked(&r, &d);
                    let pick = DevicePick::of(&d);
                    rows.push(
                        view! {
                            <Opt
                                label=d.label.clone()
                                sel=Signal::derive(move || sel)
                                on_pick=Callback::new(move |()| dev.set_input(Some(pick.clone())))
                            />
                        }
                            .into_any(),
                    );
                }
                rows
            }}
        </div>
        {move || {
            let l = dev.listing.get();
            let r = resolved.get();
            let hidden = l.as_ref().is_some_and(|l| !l.inputs_known);
            let mut notes = Vec::new();
            if hidden {
                notes.push(
                    "Device names appear after the microphone is first used in this window: Test microphone lists them."
                        .to_string(),
                );
            }
            notes.extend(r.note("microphone"));
            notes
                .into_iter()
                .map(|n| view! { <div class="vd-note dim" data-vd-note="">{n}</div> })
                .collect_view()
        }}
    }
}

#[component]
fn OutputList(dev: VoiceDevices, in_app: bool) -> impl IntoView {
    let resolved = Memo::new(move |_| dev.output_resolved());
    let kind = Memo::new(move |_| dev.listing.with(|l| l.as_ref().map(|l| l.output_kind)));
    view! {
        <div class="vd-list" role="radiogroup" aria-label="Output">
            <Opt
                label=Signal::derive(move || {
                    if kind.get() == Some(OutputKind::DefaultOnly) {
                        "System default (this browser cannot choose an output)".to_string()
                    } else {
                        "System default".to_string()
                    }
                })
                sel=Signal::derive(move || resolved.with(picked_default))
                on_pick=Callback::new(move |()| dev.set_output(None))
            />
            {move || {
                let l = dev.listing.get();
                let r = resolved.get();
                let mut rows = Vec::new();
                if let Resolved::Unknown(p) = &r {
                    rows.push(
                        view! {
                            <Opt
                                label=format!("{} (chosen here before)", if p.label.is_empty() { &p.id } else { &p.label })
                                sel=Signal::derive(|| true)
                                disabled=true
                                on_pick=Callback::new(|()| {})
                            />
                        }
                            .into_any(),
                    );
                }
                for d in l.as_ref().map(|l| l.outputs.clone()).unwrap_or_default() {
                    let sel = picked(&r, &d);
                    let tag = d.is_default.then_some("system default");
                    let pick = DevicePick::of(&d);
                    rows.push(
                        view! {
                            <Opt
                                label=d.label.clone()
                                tag=tag
                                sel=Signal::derive(move || sel)
                                on_pick=Callback::new(move |()| dev.set_output(Some(pick.clone())))
                            />
                        }
                            .into_any(),
                    );
                }
                rows
            }}
        </div>
        {move || {
            let l = dev.listing.get();
            let r = resolved.get();
            let mut notes = Vec::new();
            if let Some(n) = l.as_ref().and_then(|l| l.output_note.clone()) {
                notes.push(n);
            }
            if l.as_ref().is_some_and(|l| l.output_kind == OutputKind::SinkId && !l.outputs_known) {
                notes.push(
                    "Output names appear after the microphone is first used in this window.".to_string(),
                );
            }
            notes.extend(r.note("output"));
            if in_app && l.as_ref().is_some_and(|l| l.output_kind == OutputKind::Shell) {
                notes.push(
                    "The app moves lmgw's own playback to the chosen output; other programs stay where they are."
                        .to_string(),
                );
            }
            notes
                .into_iter()
                .map(|n| view! { <div class="vd-note dim" data-vd-note="">{n}</div> })
                .collect_view()
        }}
    }
}
