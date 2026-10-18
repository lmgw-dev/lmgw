//! A kept audio run (labs-store design §5): which model, which task and
//! route, when; then what came back, drawn as the run in flight is. Speech
//! is its kept file: a WAV or other audio file plays from the lab store as
//! it is, an event stream or raw PCM is fetched and wrapped in a WAV with
//! the PCM format the form held (the gateway's `x-lmgw-sample-rate` wins
//! over it). A transcription's or a task's answer is its text, its audio
//! the files the gateway kept in its place. A run the page stopped says so
//! beside what it got. The card of the group whose request this page has
//! out is the live run, with Stop; it stays live, saying so, when the
//! answer ended without a kept run. Any other group with no run is in
//! flight elsewhere, not kept (when this page knows) or stopped before
//! anything came back. The card's menu: Open in form, Save (the run's audio
//! file, its transcript, or a zip of its several outputs, under a readable
//! name), Delete (not while it is in flight).

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::audio_lab::{AudioLabForm, AudioRoute};
use lmgw_api_types::lab_store::{
    LabArtifactRole, LabArtifactView, LabExportFormat, LabGroupView, LabRunView,
};
use serde_json::Value;

use super::super::lab_history::{runless, ExportScope, Runless};
use super::result::{self, Pcm, Read};
use super::{result_body, track_stem, Lab, RunResult, RunStats, Track};
use crate::fmt::local_datetime;
use crate::pages::docs::download;
use crate::scope::Scope;
use crate::widgets::{MenuItem, RowMenu};

/// How a kept run's answer is read back.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Source {
    /// An audio file a player plays from the store as it is.
    Play(LabArtifactView),
    /// A file to fetch first: an event stream, raw PCM, a JSON envelope.
    Fetch(LabArtifactView),
    /// The answer's text.
    Text(String),
    /// Nothing came back.
    Nothing,
}

/// What `run`'s answer is, by its type and its kept output.
pub(super) fn source_of(run: &LabRunView) -> Source {
    if let Some(t) = &run.response {
        return Source::Text(t.clone());
    }
    let Some(a) = run
        .artifacts
        .iter()
        .find(|a| a.role == LabArtifactRole::Output)
    else {
        return Source::Nothing;
    };
    let ty = run.response_type.as_deref().unwrap_or(&a.mime);
    let plays = [
        "audio/wav",
        "audio/x-wav",
        "audio/mpeg",
        "audio/ogg",
        "audio/flac",
        "audio/aac",
    ]
    .iter()
    .any(|m| ty.starts_with(m) || a.mime.starts_with(m));
    if plays {
        Source::Play(a.clone())
    } else {
        Source::Fetch(a.clone())
    }
}

/// The PCM format `run`'s stream is wrapped with: the rate the gateway
/// stated on the answer, else the form's; the form's channels and depth.
pub(super) fn pcm_of(run: &LabRunView, form: &AudioLabForm) -> Pcm {
    let p = &form.settings.pcm;
    let mut pcm = Pcm::read(&p.rate, &p.channels, &p.bits);
    if let Some(rate) = run
        .headers
        .get("x-lmgw-sample-rate")
        .and_then(|r| r.trim().parse().ok())
    {
        pcm.rate = rate;
    }
    pcm
}

/// The card's Save entry for `run`: its label and whether there is
/// anything to save. Audio when the run kept files (one is saved as it is,
/// several as a zip), else its transcript when it answered text.
pub(super) fn save_entry(run: Option<&LabRunView>) -> (&'static str, bool) {
    let Some(run) = run else {
        return ("Save audio…", false);
    };
    if run
        .artifacts
        .iter()
        .any(|a| a.role == LabArtifactRole::Output)
    {
        return ("Save audio…", true);
    }
    // The export's own rule: the same function decides whether a transcript
    // file is written (an event stream of blanks has none).
    let said = lmgw_api_types::audio_lab::transcript::run_transcript(
        run.http_status,
        run.response.as_deref(),
    )
    .is_some();
    if said {
        ("Save transcript…", true)
    } else {
        ("Save audio…", false)
    }
}

/// The route a plan's variant names, as the page labels it.
fn route_label(variant: &str) -> &'static str {
    match AudioRoute::parse(variant).map(AudioRoute::endpoint) {
        Some(e) => e,
        None => "?",
    }
}

fn when(created_at: &str) -> String {
    let ms = js_sys::Date::parse(created_at);
    if ms.is_nan() {
        created_at.to_string()
    } else {
        local_datetime(ms / 1000.0)
    }
}

/// The numbers under a kept run.
fn stats_of(run: &LabRunView, read: &Read) -> RunStats {
    RunStats {
        status: run.http_status,
        ttfb_ms: None,
        total_ms: Some(run.latency_ms as f64),
        bytes: read.bytes,
        mime: run.response_type.clone().unwrap_or_default(),
        wall_ms: read.timing["wall_ms"].as_f64(),
        audio_ms: read.timing["audio_duration_ms"].as_f64(),
        rtf: read.timing["rtf"].as_f64().filter(|v| v.is_finite()),
        shaped: run.headers.get("x-lmgw-speech").cloned(),
    }
}

/// The error line a kept run shows: a failure's status and message, or a
/// stop, with what it got.
fn error_of(run: &LabRunView) -> String {
    if run.error_code.as_deref() == Some("stopped") {
        return run
            .error_message
            .clone()
            .unwrap_or_else(|| "stopped before it finished".into());
    }
    if (200..300).contains(&run.http_status) {
        return run.error_message.clone().unwrap_or_default();
    }
    let mut s = format!("HTTP {}", run.http_status);
    if let Some(c) = run
        .error_code
        .as_ref()
        .filter(|c| **c != run.http_status.to_string())
    {
        s.push_str(&format!(" · {c}"));
    }
    if let Some(m) = &run.error_message {
        s.push_str(&format!(": {m}"));
    }
    s
}

/// The card of kept group `id`.
#[component]
pub(super) fn AudioCard(lab: Lab, id: i64) -> impl IntoView {
    let group = Memo::new(move |_| {
        lab.history
            .groups
            .with(|v| v.iter().find(|g| g.id == id).cloned())
    });
    let menu = Signal::derive(move || {
        let Some(g) = group.get() else {
            return Vec::new();
        };
        let flying = g.in_flight || (lab.live.get() == Some(id) && lab.running.get());
        let files = lab
            .history
            .export_href(LabExportFormat::Files, &ExportScope::Group(id));
        let (save, can_save) = save_entry(g.runs.first());
        vec![
            MenuItem::new("Open in form", move || lab.open_in_form(&g))
                .title("Put this run's model, task and form back, its upload included"),
            MenuItem::new(save, move || download(&files, ""))
                .disabled(!can_save)
                .title(if can_save {
                    "Save what this run made under a readable name: one file as it is, \
                     several as a zip"
                } else {
                    "Nothing came back to save"
                }),
            MenuItem::new("Delete", move || lab.history.delete(id))
                .danger()
                .disabled(flying)
                .title(if flying {
                    "In flight: stop it, or delete it once its answer is in"
                } else {
                    "Delete this run and the files nothing else names"
                }),
        ]
    });
    view! {
        {move || group.get().map(|g| view! { <CardBody lab=lab g=g menu=menu/> })}
    }
}

#[component]
fn CardBody(lab: Lab, g: LabGroupView, menu: Signal<Vec<MenuItem>>) -> impl IntoView {
    let cell = g.plan.first().cloned().unwrap_or_default();
    let form = serde_json::from_value::<AudioLabForm>(g.form.clone()).unwrap_or_default();
    let task = if form.task.is_empty() {
        cell.variant.replace('_', " ")
    } else {
        super::task_label(&form.task)
    };
    let id = g.id;
    let in_flight = g.in_flight;
    let live = Memo::new(move |_| lab.live.get() == Some(id));
    let note = Memo::new(move |_| lab.history.notes.with(|n| n.get(&id).cloned()));
    let run = g.runs.first().cloned();
    let failed = run
        .as_ref()
        .is_some_and(|r| !(200..300).contains(&r.http_status));
    // The uploads the title does not name already.
    let inputs: Vec<String> = g
        .artifacts
        .iter()
        .map(|a| a.name.clone())
        .filter(|n| *n != g.title)
        .collect();
    view! {
        <div class="card result-card al-group" class:failed=failed>
            <div class="run-head">
                <span class="run-model mono-sm" title=cell.model.clone()>
                    {cell.model.clone()}
                </span>
                <span class="type-badge">{task}</span>
                <span class="mono-sm dim">{route_label(&cell.variant)}</span>
                <span class="spacer"></span>
                <span class="mono-sm dim">{format!("#{} · {}", g.id, when(&g.created_at))}</span>
                <RowMenu items=menu title="Run actions"/>
            </div>
            <div class="dl-group-sub mono-sm dim al-title" title=g.title.clone()>
                {g.title.clone()}
                {(!inputs.is_empty()).then(|| format!(" · sent {}", inputs.join(" · ")))}
            </div>
            {move || {
                // Only what this card is reads here: the live card's own
                // flag and its own note. A run starting elsewhere, or a
                // note of another group, leaves a kept run (and its player)
                // as it is; its reads are not made again.
                if live.get() {
                    return view! { <LiveRun lab=lab note=note/> }.into_any();
                }
                match run.clone() {
                    Some(run) => view! { <KeptRun lab=lab run=run form=form.clone()/> }.into_any(),
                    None => runless_view(runless(false, in_flight, note.get().as_deref())),
                }
            }}
        </div>
    }
}

/// The live run inside its group's card: running with Stop while the
/// request is out, what came so far as the page reads it, and why it was
/// not kept once the page knows.
#[component]
fn LiveRun(lab: Lab, note: Memo<Option<String>>) -> impl IntoView {
    let rate_field = lab.form.timeline_rate;
    view! {
        {move || {
            lab.running
                .get()
                .then(|| {
                    view! {
                        <div class="dl-status">
                            <span class="chip live">
                                <i class="dot"></i>
                                "running…"
                            </span>
                            <button class="btn danger sm" on:click=move |_| lab.stop()>
                                "Stop"
                            </button>
                        </div>
                    }
                })
        }}
        {move || {
            result_body(
                lab.result.get().unwrap_or_default(),
                lab.stats.get(),
                lab.error.get(),
                rate_field,
            )
        }}
        {move || note.get().map(|n| view! { <div class="notice warn">{n}</div> })}
    }
}

/// What a group with no run that is not this page's live one shows.
fn runless_view(state: Runless) -> AnyView {
    let (chip, text) = match state {
        Runless::Live | Runless::InFlight => (
            "chip live",
            "in flight — its answer is on its way".to_string(),
        ),
        Runless::NotKept(why) => ("chip warn", why),
        Runless::Stopped => ("chip warn", "stopped — nothing came back".to_string()),
    };
    let live = chip == "chip live";
    view! {
        <div class="dl-status">
            <span class=chip>{live.then(|| view! { <i class="dot"></i> })} {text}</span>
        </div>
    }
    .into_any()
}

/// One kept run, read back: a file to play, one to fetch and assemble, or
/// the answer's text.
#[component]
fn KeptRun(lab: Lab, run: LabRunView, form: AudioLabForm) -> impl IntoView {
    let model = run.model_asked.clone();
    // The card's own lifetime: a fetch that lands after the card went makes
    // no object URL nobody would revoke.
    let scope = Scope::new();
    let read = RwSignal::new(None::<Read>);
    let fetch_error = RwSignal::new(String::new());
    let urls = StoredValue::new_local(Vec::<String>::new());
    // The card going (its run deleted, the page left) ends a fetch still out.
    let ctrl = web_sys::AbortController::new().ok();
    let signal = ctrl.as_ref().map(|c| c.signal());
    let ctrl = StoredValue::new_local(ctrl);
    on_cleanup(move || {
        if let Some(Some(c)) = ctrl.try_get_value() {
            c.abort();
        }
        if let Some(v) = urls.try_get_value() {
            for u in v {
                let _ = web_sys::Url::revoke_object_url(&u);
            }
        }
    });
    let keep = move |r: Read| {
        urls.update_value(|v| v.extend(r.urls.iter().cloned()));
        read.set(Some(r));
    };
    match source_of(&run) {
        Source::Text(t) => {
            let streamed = run
                .response_type
                .as_deref()
                .is_some_and(|ty| ty.starts_with("text/event-stream"));
            if streamed {
                keep(result::from_stream(
                    &result::fold_sse(&t),
                    pcm_of(&run, &form),
                    &model,
                ));
            } else {
                match serde_json::from_str::<Value>(&t) {
                    Ok(j) if (200..300).contains(&run.http_status) => {
                        keep(result::from_json(&j, &model, &run.artifacts))
                    }
                    _ => keep(Read {
                        result: RunResult {
                            json: serde_json::from_str(&t).ok(),
                            text: if serde_json::from_str::<Value>(&t).is_ok() {
                                String::new()
                            } else {
                                t.clone()
                            },
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                }
            }
        }
        Source::Play(a) => keep(Read {
            result: RunResult {
                tracks: vec![Track {
                    id: String::new(),
                    url: a.href.clone(),
                    name: a.name.clone(),
                    size: a.size as usize,
                    sample_rate: None,
                }],
                ..Default::default()
            },
            bytes: Some(a.size as usize),
            ..Default::default()
        }),
        Source::Fetch(a) => {
            let ty = run.response_type.clone().unwrap_or_else(|| a.mime.clone());
            let pcm = pcm_of(&run, &form);
            let model = model.clone();
            let kept = run.artifacts.clone();
            scope.spawn(async move {
                // A refusal as every call reads it: a `401
                // session_required` raises the login gate.
                let got = match Request::get(&a.href)
                    .abort_signal(signal.as_ref())
                    .send()
                    .await
                {
                    Ok(resp) => match crate::api::checked(resp).await {
                        Ok(resp) => resp.binary().await.map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    },
                    Err(e) => Err(e.to_string()),
                };
                match got {
                    Err(e) => fetch_error.set(format!("the kept answer did not load: {e}")),
                    Ok(bytes) if ty.starts_with("text/event-stream") => {
                        let text = String::from_utf8_lossy(&bytes);
                        keep(result::from_stream(&result::fold_sse(&text), pcm, &model));
                    }
                    Ok(bytes) if ty.starts_with("application/json") => {
                        match serde_json::from_slice::<Value>(&bytes) {
                            Ok(j) => keep(result::from_json(&j, &model, &kept)),
                            Err(e) => fetch_error.set(format!("the kept answer is no JSON: {e}")),
                        }
                    }
                    // Raw PCM, chunked (`stream_format: audio`).
                    Ok(bytes) => keep(result::from_stream(
                        &super::super::audio_stream::StreamAcc {
                            pcm: bytes,
                            ..Default::default()
                        },
                        pcm,
                        &model,
                    )),
                }
            });
        }
        Source::Nothing => keep(Read::default()),
    }
    let rate_field = lab.form.timeline_rate;
    let error = error_of(&run);
    let stem = track_stem(&model);
    let unkept = run.id.is_none();
    let run_for_stats = run.clone();
    view! {
        {move || {
            let e = fetch_error.get();
            (!e.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {e}")}</div> })
        }}
        {move || {
            read.with(|r| {
                r.as_ref()
                    .map(|r| {
                        result_body(
                            r.result.clone(),
                            Some(stats_of(&run_for_stats, r)),
                            error.clone(),
                            rate_field,
                        )
                            .into_any()
                    })
                    .unwrap_or_else(|| {
                        view! { <p class="dim mini-note">{format!("reading {stem}…")}</p> }
                            .into_any()
                    })
            })
        }}
        {unkept.then(|| view! { <div class="notice warn">"not kept"</div> })}
        <details class="lab-details">
            <summary>"The request as sent"</summary>
            <pre class="preset">
                {lmgw_api_types::ordered_json::pretty(&run.request)
                    .unwrap_or_else(|_| run.request.clone())}
            </pre>
        </details>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(response: Option<&str>, ty: &str, artifact_mime: Option<&str>) -> LabRunView {
        let artifacts: Vec<Value> = artifact_mime
            .map(|m| {
                serde_json::json!({
                    "id": 5, "run_id": 2, "role": "output", "ord": 0, "name": "audio-tts.wav",
                    "mime": m, "size": 44, "sha256": "ab", "data_url_head": null,
                    "href": "/labs/api/artifacts/5", "created_at": ""
                })
            })
            .into_iter()
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": 2, "group_id": 1, "cell": 0, "variant": "speech",
            "endpoint": "/v1/audio/speech", "model_asked": "audio/tts",
            "answered_by": "audio/tts", "fallback_reason": null, "upstream_model": null,
            "http_status": 200, "error_code": null, "error_message": null,
            "request": "{}", "response": response, "response_type": ty,
            "headers": {"x-lmgw-sample-rate": "44100"},
            "input_tokens": null, "output_tokens": null, "latency_ms": 12,
            "created_at": "", "artifacts": artifacts
        }))
        .unwrap()
    }

    #[test]
    fn a_kept_answer_is_played_fetched_or_read_by_its_type() {
        assert!(matches!(
            source_of(&run(None, "audio/wav", Some("audio/wav"))),
            Source::Play(_)
        ));
        assert!(matches!(
            source_of(&run(
                None,
                "text/event-stream",
                Some("text/plain; charset=utf-8")
            )),
            Source::Fetch(_)
        ));
        assert!(matches!(
            source_of(&run(
                None,
                "application/octet-stream",
                Some("application/octet-stream")
            )),
            Source::Fetch(_)
        ));
        assert_eq!(
            source_of(&run(Some("{\"text\":\"hi\"}"), "application/json", None)),
            Source::Text("{\"text\":\"hi\"}".into())
        );
        assert_eq!(source_of(&run(None, "audio/wav", None)), Source::Nothing);
    }

    #[test]
    fn save_offers_the_audio_else_the_transcript_else_nothing() {
        assert_eq!(
            save_entry(Some(&run(None, "audio/wav", Some("audio/wav")))),
            ("Save audio…", true)
        );
        assert_eq!(
            save_entry(Some(&run(
                Some(r#"{"text": "hi"}"#),
                "application/json",
                None
            ))),
            ("Save transcript…", true)
        );
        let streamed = "data: {\"type\": \"transcript.text.delta\", \"delta\": \"hi\"}\n\n";
        assert_eq!(
            save_entry(Some(&run(Some(streamed), "text/event-stream", None))),
            ("Save transcript…", true)
        );
        assert_eq!(
            save_entry(Some(&run(
                Some(r#"{"words": []}"#),
                "application/json",
                None
            ))),
            ("Save audio…", false),
            "an answer with no text and no file"
        );
        let blanks = "data: {\"type\": \"transcript.text.delta\", \"delta\": \" \"}\n\n";
        assert_eq!(
            save_entry(Some(&run(Some(blanks), "text/event-stream", None))),
            ("Save audio…", false),
            "a stream of blanks says nothing"
        );
        // A separation that answered no files: no output artifact, only the
        // input it was given (an input is not something to save).
        let none = run(
            Some(r#"{"named_audio_outputs": []}"#),
            "application/json",
            None,
        );
        assert_eq!(save_entry(Some(&none)), ("Save audio…", false));
        let mut input_only = run(None, "audio/wav", Some("audio/wav"));
        input_only.artifacts[0].role = LabArtifactRole::Input;
        assert_eq!(save_entry(Some(&input_only)), ("Save audio…", false));
        let mut failed = run(Some(r#"{"text": "error text"}"#), "application/json", None);
        failed.http_status = 500;
        assert_eq!(save_entry(Some(&failed)), ("Save audio…", false));
        assert_eq!(save_entry(None), ("Save audio…", false), "in flight");
    }

    #[test]
    fn the_gateways_rate_wins_over_the_forms() {
        let mut form = AudioLabForm::default();
        form.settings.pcm.channels = "2".into();
        let p = pcm_of(&run(None, "text/event-stream", None), &form);
        assert_eq!((p.rate, p.channels, p.bits), (44100, 2, 16));
    }

    #[test]
    fn a_stop_says_what_it_got_and_a_failure_its_status() {
        let mut r = run(None, "audio/wav", None);
        r.error_code = Some("stopped".into());
        r.error_message = Some("the page stopped the answer after 512 bytes".into());
        assert_eq!(error_of(&r), "the page stopped the answer after 512 bytes");
        r.error_code = Some("gpu_hold".into());
        r.http_status = 503;
        r.error_message = Some("held".into());
        assert_eq!(error_of(&r), "HTTP 503 · gpu_hold: held");
        assert_eq!(
            route_label("transcription_details"),
            "/v1/audio/transcriptions/details"
        );
    }
}
