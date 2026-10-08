//! One streamed turn of a conversation, however it started: a send, an
//! edited or regenerated user message, a regenerated reply, or a continued
//! one (chat-complete §3). The four differ in where the stream is opened and
//! what the bubbles do around it; consuming it — deltas, reasoning, tool
//! cards, usage, the stats row, the settle — is this one function.
//!
//! A thread that reads its replies aloud sends every turn with `speak:
//! true` (chat-voice §6.4): the stream then carries the speech frames
//! beside the text, for the page's read-aloud (`chat_voice::LiveSpeech`).
//! The turn settles at the text's `done` — the composer takes the next
//! send — while the rest of the stream is still read for the speech (§6.5:
//! aborting the fetch at `done` would stop the speech).

use std::cell::Cell;
use std::rc::Rc;

use futures::future::{select, Either, FutureExt};
use leptos::prelude::*;
use serde_json::Value;

use super::chat::{scroll_down, ChatThread, Msg, Stats, ToolCard};
use super::chat_reply::Finished;
use super::chat_stream::{send_stream, ChatEvent};
use super::chat_sync::OwnEdits;
use super::chat_voice::PageVoice;
use crate::scope::Scope;
use crate::widgets::Toasts;

/// Everything of the page a turn reads or writes. All `Copy`: signals and
/// stored values that belong to the `Chat` component.
#[derive(Clone, Copy)]
pub(super) struct TurnEnv {
    pub msgs: RwSignal<Vec<Msg>>,
    pub current: RwSignal<Option<ChatThread>>,
    pub current_id: Memo<Option<i64>>,
    /// The thread whose reply is streaming.
    pub streaming: RwSignal<Option<i64>>,
    /// The streaming reply's own bubble, to put back if the owner returns to
    /// its thread before the server has stored it.
    pub live: StoredValue<Option<(i64, Msg)>>,
    pub stats: RwSignal<Option<(i64, Stats)>>,
    pub aborter: StoredValue<Option<web_sys::AbortController>>,
    pub ctx_max: Memo<Option<i64>>,
    pub toasts: Toasts,
    pub scope: Scope,
    /// Dictation, read-aloud and the voice status line (chat-voice WP7).
    pub voice: PageVoice,
    /// The page's own changes to the transcript (`chat_sync`): a turn's
    /// start and end count.
    pub own: OwnEdits,
}

/// What to stream and where it lands.
pub(super) struct Turn {
    pub tid: i64,
    pub url: String,
    pub body: Value,
    /// The bubble the reply streams into: a new one, or with `continuing`
    /// the existing reply.
    pub target: Msg,
    /// A continuation appends to a stored reply: the stream's deltas are only
    /// the new text, a failure leaves the stored reply as it was (so it is
    /// toasted, not written into the bubble), and there is no "stopped" mark.
    pub continuing: bool,
    /// For the failure toast: "send", "regenerate", …
    pub what: &'static str,
}

/// How the stream ended.
pub(super) struct TurnEnd {
    /// The POST was refused before any frame: nothing happened server-side,
    /// so the caller undoes what it drew for it.
    pub refused: bool,
    /// Stopped by the owner.
    pub aborted: bool,
    /// The stream carried an `error` frame (or the connection died mid-way).
    pub failed: bool,
}

/// Open the stream and feed the bubble until it ends. `on_turn` learns the
/// user message's stored id; `on_accept` runs once, at the first frame — the
/// moment the server has taken the turn, and what an action that rewrites
/// history waits for before it cuts the transcript.
pub(super) async fn run_turn(
    env: TurnEnv,
    turn: Turn,
    on_turn: impl Fn(i64) + 'static,
    on_accept: impl Fn() + 'static,
) -> TurnEnd {
    let Turn {
        tid,
        url,
        mut body,
        target,
        continuing,
        what,
    } = turn;
    let TurnEnv {
        streaming,
        live,
        stats,
        aborter,
        ctx_max,
        current_id,
        toasts,
        current,
        ..
    } = env;
    env.own.bump();
    target.streaming.set(true);
    live.set_value(Some((tid, target.clone())));
    streaming.set(Some(tid));
    stats.set(Some((
        tid,
        Stats {
            live: true,
            ctx_max: ctx_max.get_untracked(),
            ..Default::default()
        },
    )));
    let ctrl = web_sys::AbortController::new().ok();
    let signal = ctrl.as_ref().map(|c| c.signal());
    // Read aloud as it streams, when the thread says so: `speak` goes on
    // the body, and the speech frames to the page's read-aloud.
    let speech = env
        .voice
        .for_turn(tid, target.key, &mut body, ctrl.as_ref())
        .map(Rc::new);
    let (text_done_tx, text_done) = futures::channel::oneshot::channel::<()>();
    let text_done_tx = Rc::new(Cell::new(Some(text_done_tx)));
    aborter.set_value(ctrl);

    // Only the thread on screen scrolls: the owner may be reading another.
    let here = move || current_id.try_get_untracked().flatten() == Some(tid);
    let a_content = target.content;
    let a_reasoning = target.reasoning;
    let a_tools = target.tools;
    let a_streaming = target.streaming;
    let a_tokens = target.tokens;
    let a_db_id = target.db_id;
    let a_context = target.context;
    let (a_model, a_answered_by, a_unsaved) = (target.model, target.answered_by, target.unsaved);
    let a_images_note = target.images_note;

    let t0 = js_sys::Date::now();
    let first_at = Rc::new(Cell::new(None::<f64>));
    let delta_count = Rc::new(Cell::new(0i64));
    let usage_seen = Rc::new(Cell::new((None::<i64>, None::<i64>)));
    // Set the instant any SSE frame arrives — an Err from `send_stream` with
    // this still false is a refusal before the stream ever started (a
    // 400/409/413/415 body, or a transport failure on the initial POST).
    let any_event = Rc::new(Cell::new(false));
    let failed = Rc::new(Cell::new(false));
    let superseded = Rc::new(Cell::new(false));
    let superseded2 = superseded.clone();
    let (speech2, voice) = (speech.clone(), env.voice);
    let cm = ctx_max.get_untracked();
    let (first_at2, delta_count2, usage_seen2, any_event2, failed2) = (
        first_at.clone(),
        delta_count.clone(),
        usage_seen.clone(),
        any_event.clone(),
        failed.clone(),
    );
    let handle = move |ev: ChatEvent| {
        if !any_event2.replace(true) {
            on_accept();
        }
        match ev {
            ChatEvent::Turn(id) => on_turn(id),
            ChatEvent::Delta(text) => {
                if first_at2.get().is_none() {
                    first_at2.set(Some(js_sys::Date::now()));
                }
                delta_count2.set(delta_count2.get() + 1);
                a_content.update(|c| c.push_str(&text));
                if here() {
                    scroll_down(false);
                }
            }
            ChatEvent::Reasoning(text) => {
                if first_at2.get().is_none() {
                    first_at2.set(Some(js_sys::Date::now()));
                }
                a_reasoning.update(|r| r.push_str(&text));
                if here() {
                    scroll_down(false);
                }
            }
            ChatEvent::Tool(v) => {
                apply_tool_frame(a_tools, &v);
                if here() {
                    scroll_down(false);
                }
            }
            ChatEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                usage_seen2.set((prompt_tokens, completion_tokens));
                if let (Some(p), Some(c)) = (prompt_tokens, completion_tokens) {
                    a_tokens.set(Some((p, c)));
                }
            }
            ChatEvent::Stats(t) => {
                let mut s = Stats::from_timings(&t, cm);
                s.live = true;
                let prev = stats
                    .try_get_untracked()
                    .flatten()
                    .filter(|(t, _)| *t == tid)
                    .map(|(_, s)| s)
                    .unwrap_or_default();
                s.ttft_ms = prev.ttft_ms;
                stats.set(Some((tid, s)));
            }
            ChatEvent::Retrieval(c) => a_context.set(Some(c)),
            ChatEvent::Stop => {}
            ChatEvent::Error { message: msg, code } => {
                failed2.set(true);
                // A gateway refusal is worded on the composer's line too, by
                // its code: the hold as the amber chip, in place of the chat
                // stage's `held` note (WP11 UI review m6).
                voice.turn_error(code.as_deref(), &msg);
                if continuing {
                    toasts.err(format!("{what} failed: {msg}"));
                } else {
                    a_content.update(|c| {
                        if !c.is_empty() {
                            c.push_str("\n\n");
                        }
                        c.push_str(&format!("⚠ {msg}"));
                    });
                    if here() {
                        scroll_down(false);
                    }
                }
            }
            ChatEvent::Superseded => {
                superseded2.set(true);
                // Not a failure: a newer turn or a rewrite of the history
                // took the thread over. The reply stops with a note.
                if !continuing {
                    a_content.update(|c| {
                        if !c.is_empty() {
                            c.push_str("\n\n");
                        }
                        c.push_str("⏹ stopped — the conversation moved on");
                    });
                }
            }
            ChatEvent::NotSaved(msg) => {
                // A continuation that was dropped leaves the stored reply as
                // it was (re-read after the turn); a new reply is no row.
                if continuing {
                    toasts.warn(msg);
                } else {
                    a_unsaved.set(true);
                }
            }
            ChatEvent::Done(done) => {
                let fin = Finished::from_done(&done);
                match fin.id {
                    Some(id) => a_db_id.set(Some(id)),
                    // A refused save: the bubble is not a row, and must not
                    // look like one (its actions would answer 404).
                    None if Finished::refused(&done) && !continuing => a_unsaved.set(true),
                    None => {}
                }
                if fin.model.is_some() {
                    a_model.set(fin.model);
                    a_answered_by.set(fin.answered_by);
                    a_images_note.set(fin.images_note);
                }
                let (up, uc) = usage_seen2.get();
                let total_ms = done["total_ms"]
                    .as_f64()
                    .unwrap_or(js_sys::Date::now() - t0);
                let ttft = done["ttfb_ms"]
                    .as_f64()
                    .or_else(|| first_at2.get().map(|f| f - t0));
                let s = if !done["timings"].is_null() {
                    let mut s = Stats::from_timings(&done["timings"], cm);
                    s.tps = done["timings"]["predicted_per_second"]
                        .as_f64()
                        .filter(|v| v.is_finite());
                    if up.is_some() {
                        s.prompt_tokens = up;
                    }
                    if uc.is_some() {
                        s.completion_tokens = uc;
                    }
                    Stats {
                        live: false,
                        ttft_ms: ttft,
                        total_ms: Some(total_ms),
                        ..s
                    }
                } else {
                    let decode_ms = ttft.map(|f| total_ms - f);
                    let comp = uc.unwrap_or(delta_count2.get());
                    Stats {
                        server: false,
                        live: false,
                        ttft_ms: ttft,
                        total_ms: Some(total_ms),
                        tps: decode_ms
                            .filter(|d| *d > 0.0 && comp > 0)
                            .map(|d| comp as f64 / d * 1000.0),
                        prefill: ttft
                            .filter(|f| *f > 0.0)
                            .and_then(|f| up.map(|p| p as f64 / f * 1000.0)),
                        prompt_tokens: up,
                        completion_tokens: Some(comp),
                        ctx_max: cm,
                        ..Default::default()
                    }
                };
                let ignored = done["reasoning_ignored"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let reasoning_note = done["reasoning_note"].as_str().map(str::to_string);
                // The composer's line says it once: a model that reasoned
                // although off was asked.
                voice.status.reasoning(&done);
                stats.set(Some((
                    tid,
                    Stats {
                        ignored,
                        reasoning_note,
                        ..s
                    },
                )));
                if done["aborted"].as_bool().unwrap_or(false) && !continuing && !superseded2.get() {
                    a_content.update(|c| c.push_str(" ⏹"));
                }
                if let Some(s) = &speech2 {
                    s.text_done();
                    if let Some(tx) = text_done_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
            ChatEvent::Voice(name, data) => voice.turn_frame(speech2.as_deref(), name, &data),
        }
    };
    let sig = signal.unwrap_or_else(|| web_sys::AbortController::new().unwrap().signal());
    let reading = async move { send_stream(&url, &body, &sig, handle).await }.boxed_local();
    let res = match &speech {
        None => reading.await,
        // The text's `done` settles the turn; the stream is read on for the
        // speech, to its end.
        Some(s) => match select(reading, text_done).await {
            Either::Left((res, _)) => {
                s.finish();
                res
            }
            Either::Right((_, rest)) => {
                let s = s.clone();
                leptos::task::spawn_local(async move {
                    let _ = rest.await;
                    s.finish();
                });
                Ok(())
            }
        },
    };
    a_streaming.set(false);
    streaming.set(None);
    env.own.bump();
    live.try_set_value(None);
    aborter.try_set_value(None);
    let mut end = TurnEnd {
        refused: false,
        aborted: false,
        failed: failed.get(),
    };
    match res {
        Err(e) => {
            end.aborted = e.contains("abort");
            if !end.aborted {
                toasts.err(format!("{what} failed: {e}"));
                end.failed = true;
                end.refused = !any_event.get();
            }
        }
        Ok(()) => {
            // A turn into an archived thread restores it server-side; drop
            // the strip above the composer to match.
            current.try_update(|c| {
                if let Some(t) = c.as_mut().filter(|t| t.id == tid) {
                    t.archived_at = None;
                    t.purge_at = None;
                }
            });
        }
    }
    end
}

/// A `tool` frame: find or open the card at its index and apply the step.
pub(super) fn apply_tool_frame(tools: RwSignal<Vec<ToolCard>>, v: &Value) {
    let index = v["index"].as_i64().unwrap_or(0);
    tools.update(|tools| {
        let card = match tools.iter().find(|c| c.index == index) {
            Some(c) => *c,
            None => {
                let c = ToolCard {
                    index,
                    name: RwSignal::new(String::new()),
                    args: RwSignal::new(String::new()),
                    output: RwSignal::new(String::new()),
                    is_error: RwSignal::new(false),
                    ms: RwSignal::new(None),
                    done: RwSignal::new(false),
                };
                tools.push(c);
                c
            }
        };
        match v["event"].as_str().unwrap_or("") {
            "start" => card
                .name
                .set(v["name"].as_str().unwrap_or("tool").to_string()),
            "args" => card
                .args
                .update(|a| a.push_str(v["fragment"].as_str().unwrap_or(""))),
            "ready" => {
                if let Some(n) = v["name"].as_str() {
                    card.name.set(n.to_string());
                }
                if let Some(args) = v.get("arguments") {
                    card.args
                        .set(serde_json::to_string_pretty(args).unwrap_or_default());
                }
            }
            "result" => {
                card.output
                    .set(v["output"].as_str().unwrap_or("").to_string());
                card.is_error.set(v["is_error"].as_bool().unwrap_or(false));
                card.ms.set(v["ms"].as_i64());
                card.done.set(true);
            }
            _ => {}
        }
    });
}
