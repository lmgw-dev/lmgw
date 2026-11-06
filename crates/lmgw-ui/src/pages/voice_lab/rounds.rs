//! The results column: the round in flight, then every kept round, newest
//! first — each with what it asked, who answered it (a fallback named as
//! one), and its candidates, which play inline. Save puts a candidate into
//! the voice library under a name; a taken name warns and asks before it
//! replaces anything.

use leptos::prelude::*;
use lmgw_api_types::voice_lab::{
    VoiceCandidate, VoiceRound, VoiceRounds, VoiceSampleSaveArgs, VoiceSampleSaved,
};

use super::page::{enc, Lab};
use crate::api;
use crate::widgets::ConfirmButton;

/// The one save being written at a time, and the session's saves.
#[derive(Clone, Copy)]
pub(super) struct SaveState {
    /// The candidate whose name box is open: (round, candidate).
    pub open: RwSignal<Option<(String, u32)>>,
    pub name: RwSignal<String>,
    /// The library has that name: the warning, until confirmed or changed.
    pub taken: RwSignal<Option<String>>,
    pub busy: RwSignal<bool>,
    pub error: RwSignal<String>,
    /// What this session saved: (round, candidate, voice name).
    pub saved: RwSignal<Vec<(String, u32, String)>>,
}

impl SaveState {
    pub fn new() -> Self {
        Self {
            open: RwSignal::new(None),
            name: RwSignal::new(String::new()),
            taken: RwSignal::new(None),
            busy: RwSignal::new(false),
            error: RwSignal::new(String::new()),
            saved: RwSignal::new(Vec::new()),
        }
    }
}

/// A length in seconds, one decimal.
pub(super) fn secs(ms: Option<u64>) -> String {
    ms.map(|ms| format!("{:.1} s", ms as f64 / 1000.0))
        .unwrap_or_else(|| "length unknown".into())
}

/// Whether a candidate is much shorter than the round's others: a model
/// that skipped words makes a clip whose transcript (the text lmgw asked
/// for) is wrong, and a cloning model then speaks the missing words.
/// Measured 2026-10-19: a Qwen3-TTS VoiceDesign candidate that dropped the
/// sentence's second half was 13.2 s against 17–19 s. A hint, never a block.
pub(super) fn looks_short(c: &VoiceCandidate, round: &VoiceRound) -> bool {
    let mut d: Vec<u64> = round
        .candidates
        .iter()
        .filter_map(|c| c.duration_ms)
        .collect();
    if d.len() < 3 {
        return false;
    }
    d.sort_unstable();
    let median = d[d.len() / 2];
    c.duration_ms.is_some_and(|ms| ms * 5 < median * 4)
}

/// A round's time as the head shows it: `2026-10-19 14:02`.
fn when(rfc3339: &str) -> String {
    rfc3339.get(..16).unwrap_or(rfc3339).replace('T', " ")
}

#[component]
pub(super) fn RoundsList(lab: Lab) -> impl IntoView {
    view! {
        {move || {
            lab.pending.get().map(|p| {
                view! {
                    <div class="card result-card">
                        <div class="run-head">
                            <span class="run-model mono-sm">{p.alias.clone()}</span>
                            <span class="spacer"></span>
                            <span class="chip live">
                                <i class="dot"></i>
                                {format!(
                                    "rendering {} {}…",
                                    p.count,
                                    if p.count == 1 { "candidate" } else { "candidates" },
                                )}
                            </span>
                        </div>
                        <p class="dim mini-note">
                            "One after another; the round lands here whole. Stop keeps what is made."
                        </p>
                    </div>
                }
            })
        }}
        <For each=move || lab.rounds.get() key=|r| (r.id.clone(), r.candidates.len()) let:r>
            <RoundCard lab=lab round=r/>
        </For>
    }
}

/// Who answered, when it was not the model asked.
fn answered_line(r: &VoiceRound) -> Option<String> {
    (r.answered_by != r.alias).then(|| {
        let why = match r.fallback_reason.as_deref() {
            Some("hold") => " — the GPU hold's fallback",
            Some("external_vram") => " — the card was taken by another program",
            Some("benchmark") => " — a benchmark holds the card",
            Some("gpu_setup") => " — the graphics card isn't ready for local models",
            Some("engine_download") => " — the model's engine could not be downloaded",
            Some(_) | None => " — its configured fallback",
        };
        format!("answered by {}{why}", r.answered_by)
    })
}

#[component]
fn RoundCard(lab: Lab, round: VoiceRound) -> impl IntoView {
    let id = round.id.clone();
    let discard = Callback::new(move |()| {
        let id = id.clone();
        lab.scope.spawn(async move {
            let url = format!("/voice-lab/api/rounds/{}/delete", enc(&id));
            match api::post::<VoiceRounds, _>(url, &()).await {
                Ok(r) => lab.set_rounds(r),
                Err(e) => lab.toasts.err(e.to_string()),
            }
        });
    });
    let mut meta: Vec<String> = Vec::new();
    if let Some(d) = &round.description {
        meta.push(format!("“{d}”"));
    }
    if let Some(v) = &round.voice {
        meta.push(format!("voice {v}"));
    }
    if let Some(l) = &round.language {
        meta.push(format!("language {l}"));
    }
    meta.push(if round.seeds_sent {
        "a seed each".into()
    } else {
        "no seed reaches this model".into()
    });
    let model = match &round.row {
        Some(r) => format!("{} · {}", round.upstream_model, r.family),
        None => format!("{} on {}", round.upstream_model, round.upstream),
    };
    let candidates = round.candidates.clone();
    let r2 = round.clone();
    view! {
        <div class="card result-card vl-round">
            <div class="run-head">
                <span class="run-model mono-sm" title=model>{round.alias.clone()}</span>
                {answered_line(&round).map(|a| view! {
                    <span class="chip live" title="a configured fallback answered this round">
                        <i class="dot"></i>
                        {a}
                    </span>
                })}
                <span class="spacer"></span>
                <span class="mono-sm dim" title=round.created_at.clone()>{when(&round.created_at)}</span>
                <ConfirmButton
                    label="Discard"
                    confirm="Discard this round? Saved samples stay."
                    class="btn ghost sm"
                    title="Delete this round and its candidates' audio"
                    on_confirm=discard
                />
            </div>
            <div class="dim mini-note vl-meta">{meta.join(" · ")}</div>
            {round.licence.as_ref().and_then(|l| l.label().map(|x| x.to_string())).map(|l| view! {
                <div class="dim mini-note">{format!("licence: {l}")}</div>
            })}
            {round.language_note.clone().map(|n| view! { <div class="mini-note vl-warn">{n}</div> })}
            {round.shaping.clone().map(|s| view! {
                <div class="dim mini-note mono-sm" title="x-lmgw-speech: what lmgw changed in the request for this model">
                    {format!("shaped: {s}")}
                </div>
            })}
            {round.sent_text.clone().map(|t| view! {
                <div class="dim mini-note vl-meta" title="lmgw's speech shaping changed the text for this model: this is what the candidates say, and the transcript a saved sample gets">
                    {format!("sent as: “{t}”")}
                </div>
            })}
            {round.stopped.clone().map(|s| view! {
                <div class="notice warn">{format!("{} of {} made — {s}", round.candidates.len(), round.asked)}</div>
            })}
            <div class="vl-cands">
                {candidates
                    .into_iter()
                    .map(|c| view! { <Candidate lab=lab round=r2.clone() c=c/> })
                    .collect_view()}
            </div>
        </div>
    }
}

#[component]
fn Candidate(lab: Lab, round: VoiceRound, c: VoiceCandidate) -> impl IntoView {
    let key = (round.id.clone(), c.index);
    let (k1, k2, k3) = (key.clone(), key.clone(), key.clone());
    let s = lab.save;
    let is_open = move || s.open.get().as_ref() == Some(&k1);
    let saved_as = move || {
        s.saved.with(|v| {
            v.iter()
                .rev()
                .find(|(r, i, _)| (r, *i) == (&k2.0, k2.1))
                .map(|(_, _, n)| n.clone())
        })
    };
    let open = move |_| {
        s.open.set(Some(k3.clone()));
        s.taken.set(None);
        s.error.set(String::new());
    };
    let round_id = round.id.clone();
    let index = c.index;
    let submit = Callback::new(move |overwrite: bool| {
        let args = VoiceSampleSaveArgs {
            round: round_id.clone(),
            candidate: index,
            name: s.name.get_untracked(),
            overwrite,
        };
        s.busy.set(true);
        s.error.set(String::new());
        lab.scope.spawn(async move {
            match api::post::<VoiceSampleSaved, _>("/voice-lab/api/samples", &args).await {
                Ok(saved) => {
                    s.saved.update(|v| {
                        v.push((args.round.clone(), args.candidate, saved.voice.clone()))
                    });
                    s.open.set(None);
                    s.taken.set(None);
                    lab.toasts.ok(saved.message.clone());
                    lab.load_samples();
                    lab.clone.sample.set(saved.voice);
                }
                Err(api::Error::Api(e)) if e.code == "name_taken" => s.taken.set(Some(e.message)),
                Err(e) => s.error.set(e.to_string()),
            }
            s.busy.set(false);
        });
    });
    view! {
        <div class="vl-cand">
            <div class="vl-cand-head">
                <b>{format!("#{}", c.index)}</b>
                {c.seed.map(|seed| view! { <span class="mono-sm dim" title="the seed it was sent">{format!("seed {seed}")}</span> })}
                <span class="mono-sm dim">{secs(c.duration_ms)}</span>
                {looks_short(&c, &round).then(|| view! {
                    <span
                        class="chip warn"
                        title="Much shorter than the round's other candidates: the model may have skipped words. The sample's transcript is the whole text, so listen to the end before saving it — a cloning model speaks words its reference lacks."
                    >
                        "shorter"
                    </span>
                })}
            </div>
            <audio controls preload="none" src=c.url.clone()></audio>
            <div class="vl-cand-foot">
                {move || match saved_as() {
                    Some(n) => view! {
                        <span class="chip ok" title="saved to the voice library this session">
                            {format!("saved as {n}")}
                        </span>
                    }
                    .into_any(),
                    None => ().into_any(),
                }}
                <span class="spacer"></span>
                <button class="btn sm" title="Save this candidate to the voice library" on:click=open>
                    "Save…"
                </button>
            </div>
            <Show when=is_open.clone()>
                <div class="vl-save">
                    <input
                        class="input"
                        placeholder="voice name, e.g. anna"
                        prop:value=move || s.name.get()
                        on:input=move |ev| {
                            s.name.set(event_target_value(&ev));
                            s.taken.set(None);
                        }
                    />
                    <button
                        class="btn primary sm"
                        disabled=move || s.busy.get() || s.name.get().trim().is_empty() || s.taken.get().is_some()
                        on:click=move |_| submit.run(false)
                    >
                        "Save"
                    </button>
                    <button class="btn ghost sm" on:click=move |_| s.open.set(None)>"Cancel"</button>
                </div>
                {move || s.taken.get().map(|why| {
                    view! {
                        <div class="notice warn vl-taken">
                            {why}
                            <div class="row" style="margin-top:6px">
                                <button
                                    class="btn danger sm"
                                    disabled=move || s.busy.get()
                                    on:click=move |_| submit.run(true)
                                >
                                    "Replace it"
                                </button>
                                <button class="btn ghost sm" on:click=move |_| s.taken.set(None)>
                                    "Pick another name"
                                </button>
                            </div>
                        </div>
                    }
                })}
                {move || {
                    let e = s.error.get();
                    (!e.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {e}")}</div> })
                }}
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fallback_is_named_with_its_reason() {
        let r = VoiceRound {
            alias: "audio/design".into(),
            answered_by: "cloud-tts".into(),
            fallback_reason: Some("hold".into()),
            ..VoiceRound::default()
        };
        assert_eq!(
            answered_line(&r).as_deref(),
            Some("answered by cloud-tts — the GPU hold's fallback")
        );
        // The machine check's two (WP8b, D180).
        let r = VoiceRound {
            fallback_reason: Some("gpu_setup".into()),
            ..r
        };
        assert_eq!(
            answered_line(&r).as_deref(),
            Some("answered by cloud-tts — the graphics card isn't ready for local models")
        );
        let r = VoiceRound {
            fallback_reason: Some("engine_download".into()),
            ..r
        };
        assert_eq!(
            answered_line(&r).as_deref(),
            Some("answered by cloud-tts — the model's engine could not be downloaded")
        );
        let own = VoiceRound {
            alias: "a".into(),
            answered_by: "a".into(),
            ..VoiceRound::default()
        };
        assert_eq!(answered_line(&own), None);
        let cand = |ms| VoiceCandidate {
            duration_ms: Some(ms),
            ..VoiceCandidate::default()
        };
        let r = VoiceRound {
            candidates: vec![cand(17_040), cand(13_200), cand(19_200), cand(17_440)],
            ..VoiceRound::default()
        };
        assert!(looks_short(&r.candidates[1], &r));
        assert!(!looks_short(&r.candidates[0], &r));
        assert_eq!(secs(Some(1250)), "1.2 s");
        assert_eq!(when("2026-10-19T14:02:33Z"), "2026-10-19 14:02");
    }
}
