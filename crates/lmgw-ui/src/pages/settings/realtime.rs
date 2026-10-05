//! Settings → Realtime (realtime design §12): `GET /v1/realtime`'s section.
//!
//! The rows are the page's own table ([`REALTIME`]), saved with the rest of
//! the page as `realtime.*` keys of `settings_set_full`. What only this
//! category has lives here: the name maps (one `name = value` per line), the
//! word lists (one comma-separated box), the voice of the chosen TTS model
//! ([`VoiceField`]), the Smart Turn table, and the GPU memory the drafted
//! cascade would hold ([`BudgetPanel`]) — read from the `realtime_budget` op
//! with the draft's aliases, so a choice is sized before it is saved.

use std::collections::BTreeSet;

use leptos::prelude::*;
use serde_json::{json, Map, Value};

use super::{anchor, f, Ctl, Def, Page};
use crate::widgets::{Field, FormState, Kind};

/// The default voice and the TTS model's voice list.
mod voice;
pub(super) use voice::VoiceField;
/// Where the Realtime voice and style fields read their TTS model.
pub(super) const RT_TTS: &[&str] = &["realtime.tts_alias"];
/// What the drafted cascade holds on the GPU.
mod budget;
pub(super) use budget::BudgetPanel;
/// The speech style and the sound-tag hint, told what the TTS does.
mod style;
pub(super) use style::{SpeechStyleField, TagHintField};

const ENGINES: &[(&str, &str)] = &[
    ("smart_turn", "Smart Turn"),
    ("server_vad", "silence windows only (server_vad)"),
];
const CHECKS: &[(&str, &str)] = &[
    ("words", "words — transcribe the interruption"),
    ("duration", "voiced time only"),
];

/// The Smart Turn table's columns: the key in a row, its header, its unit.
pub(super) const VAD_COLS: [(&str, &str, &str); 4] = [
    ("threshold", "Threshold", "0–1"),
    ("floor", "Floor", "0–1"),
    ("max_wait_ms", "Max wait", "ms"),
    ("silence_duration_ms", "No score", "ms"),
];
/// Its rows: the eagerness key and its label.
const VAD_ROWS: [(&str, &str); 3] = [
    ("high", "High"),
    ("medium", "Medium · auto"),
    ("low", "Low"),
];

const fn vad(key: &'static str, label: &'static str, col: usize) -> Def {
    f("realtime", "rt-smart", key, label, Ctl::Vad(col)).terms("semantic_vad eagerness")
}

/// Every Realtime setting, in page order.
pub(super) const REALTIME: &[Def] = &[
    // The cascade
    f(
        "realtime",
        "rt-cascade",
        "realtime.default_model",
        "Chat model",
        Ctl::Model(&["chat"], None, "none — a session must name its model"),
    )
    .hint("answers a session that names no model, or an OpenAI realtime one (gpt-realtime…)")
    .terms("llm voice default model brain"),
    f(
        "realtime",
        "rt-cascade",
        "realtime.asr_alias",
        "Speech to text",
        Ctl::Model(&["asr"], None, "the Chat's transcription model"),
    )
    .hint("transcribes each turn; the Chat uses it too while Settings → Chat names none")
    .terms("asr stt transcription whisper"),
    f(
        "realtime",
        "rt-cascade",
        "realtime.tts_alias",
        "Text to speech",
        Ctl::Model(
            lmgw_api_types::realtime::SPEECH_TASKS,
            None,
            "none — audio answers fail",
        ),
    )
    .hint("speaks each answer; the Chat uses it too while Settings → Chat names none")
    .terms("tts speech synthesis"),
    f(
        "realtime",
        "rt-cascade",
        "realtime.default_voice",
        "Voice",
        Ctl::Voice(RT_TTS),
    )
    .hint(
        "for a session that names none, or an OpenAI voice, and for the Chat when it names \
         none (checked against the Chat's model); empty = the model's default",
    )
    .terms("voice speaker alloy marin preset"),
    f(
        "realtime",
        "rt-cascade",
        "realtime.speech_instructions",
        "Speech style",
        Ctl::SpeechStyle(RT_TTS),
    )
    .ph("calm, warm, unhurried")
    .hint(
        "what the voice is told while a session sends none (session.lmgw.speech_instructions), \
         and the Chat's while Settings → Chat names none; empty = its own neutral delivery",
    )
    .terms("speech instructions style tone voice design description expressive emotion"),
    f(
        "realtime",
        "rt-budget",
        "realtime.budget",
        "GPU memory of the cascade",
        Ctl::Budget,
    )
    .terms("vram gpu memory budget fits footprint residency card sliding scale"),
    // The prompt
    f(
        "realtime",
        "rt-prompt",
        "realtime.default_instructions",
        "Instructions",
        Ctl::Prompt("/realtime/default_instructions_builtin"),
    )
    .hint(
        "what a session with audio output tells the model while the client sends no \
         instructions of its own; a text session gets none",
    )
    .terms("system prompt voice assistant instructions"),
    f(
        "realtime",
        "rt-prompt",
        "realtime.tag_hint",
        "Sound tags and delivery cues in the prompt",
        Ctl::TagHint,
    )
    .hint(
        "tell the model what square brackets do in audio answers: the sounds the voice can \
         make, or how to ask it for a delivery, such as [laughing]",
    )
    .terms(
        "tag hint laughter sigh inline tags nonverbal sounds expressive delivery cues laughing \
         whispering instructions",
    ),
    // Names clients send
    f(
        "realtime",
        "rt-names",
        "realtime.model_map",
        "Model names",
        Ctl::Map,
    )
    .unit("client name = alias, one per line")
    .ph("gpt-realtime = my-chat")
    .hint("checked first: a session's model to a chat alias, a transcription model to an ASR alias")
    .terms("model_map mapping gpt-realtime whisper-1 alias"),
    f(
        "realtime",
        "rt-names",
        "realtime.voice_map",
        "Voice names",
        Ctl::Map,
    )
    .unit("client voice = voice, one per line")
    .ph("alloy = alba")
    .hint("after the model's own voices, before OpenAI's built-in names")
    .terms("voice_map mapping alloy marin"),
    // Turn detection (server_vad)
    f(
        "realtime",
        "rt-vad",
        "realtime.threshold",
        "Speech threshold",
        Ctl::Float(0.0, 1.0),
    )
    .s()
    .unit("0–1")
    .hint("speech probability that counts as voice")
    .terms("server_vad silero vad"),
    f(
        "realtime",
        "rt-vad",
        "realtime.prefix_padding_ms",
        "Pre-roll",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("kept from before the onset")
    .terms("server_vad prefix padding"),
    f(
        "realtime",
        "rt-vad",
        "realtime.silence_duration_ms",
        "Silence ends a turn",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .terms("server_vad silence duration"),
    // Smart Turn
    f(
        "realtime",
        "rt-smart",
        "realtime.semantic_vad_engine",
        "semantic_vad runs on",
        Ctl::Choice(ENGINES),
    )
    .terms("semantic_vad smart turn escape hatch"),
    f(
        "realtime",
        "rt-smart",
        "realtime.semantic_floor_window_ms",
        "Floor window",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("an unsure score commits here")
    .terms("semantic_vad floor"),
    vad(
        "realtime.semantic_vad.high.threshold",
        "High · threshold",
        0,
    ),
    vad("realtime.semantic_vad.high.floor", "High · floor", 1),
    vad(
        "realtime.semantic_vad.high.max_wait_ms",
        "High · max wait",
        2,
    ),
    vad(
        "realtime.semantic_vad.high.silence_duration_ms",
        "High · no-score window",
        3,
    ),
    vad(
        "realtime.semantic_vad.medium.threshold",
        "Medium · threshold",
        0,
    ),
    vad("realtime.semantic_vad.medium.floor", "Medium · floor", 1),
    vad(
        "realtime.semantic_vad.medium.max_wait_ms",
        "Medium · max wait",
        2,
    ),
    vad(
        "realtime.semantic_vad.medium.silence_duration_ms",
        "Medium · no-score window",
        3,
    ),
    vad("realtime.semantic_vad.low.threshold", "Low · threshold", 0),
    vad("realtime.semantic_vad.low.floor", "Low · floor", 1),
    vad("realtime.semantic_vad.low.max_wait_ms", "Low · max wait", 2),
    vad(
        "realtime.semantic_vad.low.silence_duration_ms",
        "Low · no-score window",
        3,
    ),
    // Barge-in
    f(
        "realtime",
        "rt-barge",
        "realtime.barge_in_min_ms",
        "Voice to interrupt",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("less is a backchannel")
    .terms("barge-in interruption"),
    f(
        "realtime",
        "rt-barge",
        "realtime.barge_in_guard_ms",
        "Guard",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("at the start of each answer")
    .terms("barge-in"),
    f(
        "realtime",
        "rt-barge",
        "realtime.post_interrupt_silence_ms",
        "Silence after one",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("ends the turn after a barge-in")
    .terms("barge-in rephrase"),
    f(
        "realtime",
        "rt-barge",
        "realtime.echo_tail_ms",
        "Echo tail",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("on top of the ping round trip")
    .terms("barge-in bluetooth echo"),
    f(
        "realtime",
        "rt-barge",
        "realtime.barge_in_check_timeout_ms",
        "Word check timeout",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("0 = no bound")
    .terms("barge-in"),
    f(
        "realtime",
        "rt-barge",
        "realtime.barge_in_check",
        "Interruption check",
        Ctl::Choice(CHECKS),
    )
    .terms("barge-in word check backchannel"),
    f(
        "realtime",
        "rt-barge",
        "realtime.barge_in_check_alias",
        "Word check model",
        Ctl::Model(&["asr"], None, "the session's speech to text"),
    )
    .hint("an ASR model that hears 200 ms of voice")
    .terms("barge-in asr qwen3"),
    f(
        "realtime",
        "rt-barge",
        "realtime.half_duplex",
        "Half duplex",
        Ctl::Bool,
    )
    .hint("don't listen while an answer plays — for clients without echo cancellation")
    .terms("barge-in echo"),
    f(
        "realtime",
        "rt-barge",
        "realtime.backchannel_words",
        "Backchannel words",
        Ctl::Words,
    )
    .unit("comma-separated")
    .hint("a transcript of only these keeps the answer playing")
    .terms("barge-in mhm okay"),
    f(
        "realtime",
        "rt-barge",
        "realtime.barge_in_check_scripts",
        "Scripts that count as words",
        Ctl::Words,
    )
    .unit("comma-separated")
    .hint(
        "Latin, Greek, Cyrillic, Armenian, Hebrew, Arabic, Devanagari, Bengali, Thai, \
         Georgian, Hangul, Hiragana, Katakana, Han; empty = every script",
    )
    .terms("barge-in script language"),
    // Output
    f(
        "realtime",
        "rt-output",
        "realtime.output_lead_ms",
        "Output lead",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("audio sent ahead of real time")
    .terms("pacing"),
    f(
        "realtime",
        "rt-output",
        "realtime.synthesis_ahead_s",
        "Synthesis ahead",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("0 = no bound")
    .terms("tts flow control"),
    f(
        "realtime",
        "rt-output",
        "realtime.longest_pause_ms",
        "Longest pause",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint(
        "longest silence in speech, at a join or inside; a [pause] tag too. Below ~150 ms it \
         makes speech choppy. 0 keeps the engine's own silences",
    )
    .terms("tts silence gap sentence"),
    f(
        "realtime",
        "rt-output",
        "realtime.warm_on_connect",
        "Warm the models when a session connects",
        Ctl::Bool,
    )
    .hint(
        "API sessions; off: their first turn pays any cold start. The Chat's voice mode always \
         loads its models, and may evict idle ones",
    )
    .terms("warm start cold voice mode evict"),
    // Connection
    f(
        "realtime",
        "rt-limits",
        "realtime.max_message_mb",
        "Max message",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = no bound")
    .terms("websocket limit size"),
    f(
        "realtime",
        "rt-limits",
        "realtime.max_frame_mb",
        "Max frame",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = the message limit")
    .terms("websocket limit size"),
    f(
        "realtime",
        "rt-limits",
        "realtime.ping_interval_s",
        "Ping interval",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("0 = no pings and no liveness bound")
    .terms("websocket ping pong liveness keepalive"),
];

// ---------------------------------------------------------------------------
// The maps
// ---------------------------------------------------------------------------

/// A stored map as the box shows it: `name = value` per line.
pub(super) fn map_text(v: &Value) -> String {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("{k} = {}", v.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The box's lines as the patch's object. Lines [`map_error`] refuses never
/// get here: Save waits for them.
pub(super) fn map_object(text: &str) -> Value {
    let mut m = Map::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Some((k, v)) = line.split_once('=') {
            m.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
        }
    }
    Value::Object(m)
}

/// What the server would refuse in a map, said at the line.
pub(super) fn map_error(text: &str) -> Option<String> {
    let mut seen = BTreeSet::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Some(format!("line {} has no “=”: name = value", n + 1));
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            return Some(format!("line {} has no name before “=”", n + 1));
        }
        if v.is_empty() {
            return Some(format!("“{k}” maps to nothing"));
        }
        if !seen.insert(k.to_string()) {
            return Some(format!("“{k}” is named twice"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The word lists
// ---------------------------------------------------------------------------

/// A stored list as the box shows it: comma-separated.
pub(super) fn words_text(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The box's words as the patch's list, empty ones dropped.
pub(super) fn words_list(text: &str) -> Value {
    json!(text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>())
}

/// A list of short words as one comma-separated box that wraps and grows
/// with its text — the backchannel list is ~50 words, one per line a column
/// longer than the rest of the page. Re-measured when the text changes and
/// when the box changes width, as the agents' list control is.
#[component]
pub(super) fn WordsField(
    d: &'static Def,
    page: Page,
    dirty: Signal<bool>,
    error: Signal<Option<String>>,
    id: String,
    hidden: Signal<bool>,
) -> impl IntoView {
    let form = page.form;
    let k = d.key;
    let boxed: NodeRef<leptos::html::Div> = NodeRef::new();
    let area: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let size = crate::charts::use_element_size(boxed);
    Effect::new(move |_| {
        size.track();
        form.text(k);
        hidden.track();
        let Some(el) = area.get() else { return };
        let style = web_sys::HtmlElement::style(&el);
        let _ = style.set_property("height", "auto");
        // The borders on top of the content height: `.input` is border-box.
        let _ = style.set_property("height", &format!("{}px", el.scroll_height() + 2));
    });
    view! {
        <Field
            label=d.label
            unit=d.unit
            hint=d.hint
            dirty=dirty
            error=error
            id=id
            hidden=hidden
        >
            <div class="list-box" node_ref=boxed>
                <textarea
                    class="input ta list-ta"
                    rows="1"
                    spellcheck="false"
                    node_ref=area
                    prop:value=move || form.text(k)
                    on:input=move |ev| form.set_text(k, event_target_value(&ev))
                ></textarea>
            </div>
        </Field>
    }
}

// ---------------------------------------------------------------------------
// The Smart Turn table
// ---------------------------------------------------------------------------

pub(super) fn vad_kind(col: usize) -> Kind {
    if col < 2 {
        Kind::Float
    } else {
        Kind::Int
    }
}

pub(super) fn vad_range_error(col: usize, t: &str) -> Option<String> {
    if col < 2 {
        match t.parse::<f64>() {
            Ok(v) if !(0.0..=1.0).contains(&v) => Some("must be between 0 and 1".into()),
            _ => None,
        }
    } else {
        match t.parse::<i64>() {
            Ok(v) if v < 0 => Some("must be 0 or more".into()),
            _ => None,
        }
    }
}

/// The settings a save judges together, as the server does (realtime
/// design §12, "Judged when touched"): the whole Smart Turn table with its
/// floor window once any of them moves, and the two WebSocket limits once
/// either moves. `None`: a key judged alone, only when it moves.
pub(super) fn judged_with(key: &str) -> Option<&'static str> {
    if key.starts_with("realtime.semantic_vad.") || key == "realtime.semantic_floor_window_ms" {
        Some("semantic_vad")
    } else if matches!(key, "realtime.max_message_mb" | "realtime.max_frame_mb") {
        Some("limits")
    } else {
        None
    }
}

/// What the server refuses across two fields, said at the one the owner
/// can fix: a floor above its row's threshold, a floor window past a row's
/// maximum wait, and both WebSocket limits at 0.
pub(super) fn cross_error(form: FormState, key: &str) -> Option<String> {
    let num = |k: &str| form.text(k).trim().parse::<f64>().ok();
    if let Some(row) = key
        .strip_prefix("realtime.semantic_vad.")
        .and_then(|r| r.strip_suffix(".floor"))
    {
        let floor = num(key)?;
        let threshold = num(&format!("realtime.semantic_vad.{row}.threshold"))?;
        return (floor > threshold)
            .then(|| format!("above the threshold {threshold} (equal turns the floor off)"));
    }
    match key {
        "realtime.semantic_floor_window_ms" => {
            let window = num(key)?;
            VAD_ROWS.iter().find_map(|(row, label)| {
                let wait = num(&format!("realtime.semantic_vad.{row}.max_wait_ms"))?;
                (window > wait).then(|| {
                    format!(
                        "past the {} row's max wait of {wait} ms",
                        label.to_lowercase()
                    )
                })
            })
        }
        "realtime.max_frame_mb" => (num("realtime.max_message_mb") == Some(0.0)
            && num(key) == Some(0.0))
        .then(|| "max message and max frame cannot both be 0 — then no bound holds a frame".into()),
        _ => None,
    }
}

/// The run of [`Ctl::Vad`] cells as one table: eagerness down, the four
/// knobs across. Each cell is still its own form key, dirty and in error on
/// its own, and its own deep link. A cell's problem is an error while a save
/// judges the table, else a warning about the stored row (`judged_keys`).
pub(super) fn vad_table(defs: Vec<&'static Def>, page: Page) -> AnyView {
    let form = page.form;
    // `(error, warning)` of a cell.
    let said = move |d: &'static Def| {
        let (error, warn) = super::messages(form, d, page.judged.with(|j| j.contains(d.key)));
        (form.error(d.key).or(error), warn)
    };
    let any_shown = {
        let defs = defs.clone();
        move || defs.iter().any(|d| page.shows(d))
    };
    let cell = move |key: String| {
        let Some(d) = defs.iter().copied().find(|d| d.key == key) else {
            return ().into_any();
        };
        let k = d.key;
        let shown = Memo::new(move |_| said(d));
        let tip = move || shown.with(|(e, w)| e.clone().or_else(|| w.clone()).unwrap_or_default());
        view! {
            <td>
                <input
                    class="input mono vad-cell"
                    class:dirty=move || form.is_dirty(k)
                    class:invalid=move || shown.with(|(e, _)| e.is_some())
                    class:warned=move || shown.with(|(e, w)| e.is_none() && w.is_some())
                    id=anchor(k)
                    inputmode="decimal"
                    spellcheck="false"
                    aria-label=d.label
                    title=tip
                    prop:value=move || form.text(k)
                    on:input=move |ev| form.set_text(k, event_target_value(&ev))
                />
            </td>
        }
        .into_any()
    };
    // Every cell's message under the table, by row and column: `(text,
    // warning)`.
    let errors = move || {
        let mut out = Vec::new();
        for (row, row_label) in VAD_ROWS {
            for (col, col_label, _) in VAD_COLS {
                let k = format!("realtime.semantic_vad.{row}.{col}");
                let Some(d) = REALTIME.iter().find(|d| d.key == k) else {
                    continue;
                };
                let at = format!("{row_label} · {}", col_label.to_lowercase());
                match said(d) {
                    (Some(e), _) => out.push((format!("{at}: {e}"), false)),
                    (None, Some(w)) => out.push((format!("{at}: {w}"), true)),
                    (None, None) => {}
                }
            }
        }
        out
    };
    view! {
        <div class="set-blocks" hidden=move || !any_shown()>
            <table class="data vad-table">
                <thead>
                    <tr>
                        <th>"Eagerness"</th>
                        {VAD_COLS
                            .iter()
                            .map(|(_, h, unit)| {
                                view! {
                                    <th class="num-h">
                                        {*h} " " <span class="field-unit">{*unit}</span>
                                    </th>
                                }
                            })
                            .collect_view()}
                    </tr>
                </thead>
                <tbody>
                    {VAD_ROWS
                        .iter()
                        .map(|(row, label)| {
                            view! {
                                <tr>
                                    <td>{*label}</td>
                                    {VAD_COLS
                                        .iter()
                                        .map(|(col, ..)| cell(format!("realtime.semantic_vad.{row}.{col}")))
                                        .collect_view()}
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
            {move || {
                errors()
                    .into_iter()
                    .map(|(m, warn)| {
                        if warn {
                            view! { <div class="field-warn" role="note">{m}</div> }.into_any()
                        } else {
                            view! { <div class="field-err" role="alert">{m}</div> }.into_any()
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// The prose
// ---------------------------------------------------------------------------

/// The category's explain blocks: `(persist key, summary, body)`.
pub(super) fn explain(group: &str) -> Option<(&'static str, String, AnyView)> {
    Some(match group {
        "rt-cascade" => (
            "settings.explain.realtime",
            "GET /v1/realtime speaks OpenAI's Realtime protocol with a cascade of this gateway's \
             own models."
                .into(),
            view! {
                "lmgw detects the turns itself (Silero VAD, and Smart Turn for semantic_vad), "
                "transcribes each with the speech-to-text model, answers with the chat model and "
                "speaks the answer with the text-to-speech model — local models first; any stage "
                "can be a cloud alias. A client may name its own chat model; an OpenAI realtime "
                "name (gpt-realtime…) is answered by the chat model above. Chat models list "
                "/v1/realtime on GET /v1/models once both a speech-to-text and a text-to-speech "
                "model are set."
            }
            .into_any(),
        ),
        "rt-budget" => (
            "settings.explain.realtime-budget",
            "A voice session keeps all three models resident at once, so on one card the chat \
             model and the voice share the memory."
                .into(),
            view! {
                "A larger chat model leaves less room for the voice and the other way round. "
                "The sum adds the headroom admission keeps free above each estimate "
                <a href="/settings/gpu">"(GPU → Headroom)"</a>
                ". When it does not fit, a session still works — its models evict each other, "
                "and every turn pays a load."
            }
            .into_any(),
        ),
        "rt-smart" => (
            "settings.explain.smart-turn",
            "Smart Turn scores a pause once 200 ms of it has passed.".into(),
            view! {
                "A score at or above the threshold ends the turn at once; one between the floor "
                "and the threshold ends it at the floor window; below the floor the turn waits "
                "for more speech, or for the maximum wait. A pause that could not be scored ends "
                "at its row's no-score window, which is also what the server_vad engine runs on. "
                "The defaults were measured on real recordings, not taken from OpenAI's waits."
            }
            .into_any(),
        ),
        "rt-barge" => (
            "settings.explain.barge-in",
            "While the client plays an answer, speech has to last before it interrupts it.".into(),
            view! {
                "With the word check, the evidence is transcribed first, and a transcript that is "
                "empty or only backchannel words (\"mhm\", \"okay\") keeps the answer playing; "
                "each check is a call of its own. Barge-in needs the client's echo cancellation — "
                "a browser's, PipeWire's echo-cancel module or a headset; half duplex is for "
                "clients without it."
            }
            .into_any(),
        ),
        "rt-limits" => (
            "settings.explain.realtime-limits",
            "A message or frame over its limit closes the socket with a reason naming the \
             setting."
                .into(),
            view! {
                "A frame is always bounded: with max frame at 0 the message limit bounds it, so "
                "the two cannot both be 0. With pings off nothing ends a session whose client "
                "stopped reading, and the key's concurrency slot stays taken."
            }
            .into_any(),
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_reads_and_writes_as_name_equals_value_lines() {
        let text = map_text(&json!({"gpt-realtime": "my-chat", "whisper-1": "my-asr"}));
        assert_eq!(text, "gpt-realtime = my-chat\nwhisper-1 = my-asr");
        assert_eq!(
            map_object(" alloy =alba \n\n whisper-1 = audio/qwen3 "),
            json!({"alloy": "alba", "whisper-1": "audio/qwen3"})
        );
        assert_eq!(map_object(""), json!({}));
        assert_eq!(map_text(&Value::Null), "");
    }

    #[test]
    fn a_word_list_is_one_comma_separated_box() {
        assert_eq!(
            words_text(&json!(["mhm", "ach so", "o.k."])),
            "mhm, ach so, o.k."
        );
        assert_eq!(
            words_list(" mhm,, ach so ,\no.k. "),
            json!(["mhm", "ach so", "o.k."])
        );
        assert_eq!(
            words_list(""),
            json!([]),
            "empty is every script, sent as []"
        );
    }

    #[test]
    fn a_map_line_the_server_would_refuse_is_caught() {
        assert_eq!(map_error("a = b\n\nc = d"), None);
        assert!(map_error("a b").unwrap().contains("line 1"));
        assert!(map_error("a = b\n= d").unwrap().contains("line 2"));
        assert!(map_error("a =  ").unwrap().contains("maps to nothing"));
        assert!(map_error("a = b\na = c").unwrap().contains("twice"));
    }

    #[test]
    fn every_realtime_row_has_its_own_key_and_the_table_is_whole() {
        for d in REALTIME {
            assert!(d.key.starts_with("realtime."), "{}", d.key);
            assert_eq!(d.cat, "realtime");
        }
        for (row, _) in VAD_ROWS {
            for (col, ..) in VAD_COLS {
                let k = format!("realtime.semantic_vad.{row}.{col}");
                assert!(REALTIME.iter().any(|d| d.key == k), "{k}");
            }
        }
        // The table's cells sit together, so they are drawn as one run.
        let cells: Vec<usize> = REALTIME
            .iter()
            .enumerate()
            .filter(|(_, d)| matches!(d.ctl, Ctl::Vad(_)))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(cells.len(), 12);
        assert_eq!(cells[11] - cells[0], 11);
    }

    #[test]
    fn the_vad_cells_take_the_server_s_ranges() {
        assert!(vad_range_error(0, "1.2").is_some());
        assert!(vad_range_error(1, "0.5").is_none());
        assert!(vad_range_error(2, "-5").is_some());
        assert!(vad_range_error(3, "800").is_none());
        assert_eq!(vad_kind(0), Kind::Float);
        assert_eq!(vad_kind(3), Kind::Int);
    }
}
