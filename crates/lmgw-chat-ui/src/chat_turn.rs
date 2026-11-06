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
use lmgw_api_types::chat_frames::ToolFrame;
use lmgw_api_types::chat_threads::AttachmentMeta;
use serde_json::Value;

use super::chat::{scroll_down, ChatThread, Msg, Stats, ToolCard};
use super::chat_approvals;
use super::chat_media;
use super::chat_reply::Finished;
use super::chat_stream::{send_stream_coded, ChatEvent, Refusal, ToolEvent};
use super::chat_sync::OwnEdits;
use super::chat_voice::PageVoice;
use lmgw_ui_kit::http::Http;
use lmgw_ui_kit::scope::Scope;
use lmgw_ui_kit::widgets::Toasts;

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
    /// The transport the host handed the page.
    pub http: Http,
    /// Where the host says the user writes from (chat-plane design §9.10),
    /// asked at each send and each edit of a user message.
    pub origin: crate::host::Origin,
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
    /// A refusal before any frame is handed back in [`TurnEnd::refusal`] for
    /// the caller to word where it happened, instead of a toast.
    pub inline: bool,
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
    /// The refusal that ended a [`Turn::inline`] turn before its first frame.
    pub refusal: Option<Refusal>,
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
        inline,
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
    let a_files = target.attachments;
    let a_streaming = target.streaming;
    let a_tokens = target.tokens;
    let a_db_id = target.db_id;
    let a_context = target.context;
    let (a_model, a_answered_by, a_unsaved) = (target.model, target.answered_by, target.unsaved);
    let a_images_note = target.images_note;
    let a_gpu_setup = target.gpu_setup;

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
            ChatEvent::Tool(step) => {
                apply_tool_frame(a_tools, a_files, &step);
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
                } else if code.as_deref() == Some(super::chat_machine::GPU_SETUP) {
                    // The model could not start on the graphics card: a
                    // card of its own, with the host's fix (WP8b).
                    a_gpu_setup.set(Some(msg));
                    if here() {
                        scroll_down(false);
                    }
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
                chat_approvals::settle_done(a_tools, &done);
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
                let saved = done.saved.as_ref();
                let total_ms = saved.map_or(js_sys::Date::now() - t0, |s| s.total_ms as f64);
                let ttft = saved
                    .and_then(|s| s.ttfb_ms)
                    .map(|ms| ms as f64)
                    .or_else(|| first_at2.get().map(|f| f - t0));
                let s = if let Some(t) = saved.and_then(|s| s.timings.as_ref()) {
                    let mut s = Stats::from_timings(t, cm);
                    s.tps = t.predicted_per_second;
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
                let ignored = saved
                    .map(|s| s.reasoning_ignored.clone())
                    .unwrap_or_default();
                let reasoning_note = saved.and_then(|s| s.reasoning_note.clone());
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
                if done.aborted && !continuing && !superseded2.get() {
                    a_content.update(|c| c.push_str(" ⏹"));
                }
                if let Some(s) = &speech2 {
                    s.text_done();
                    if let Some(tx) = text_done_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
            ChatEvent::Voice(frame) => match chat_media::regain_alias(&frame, a_tools) {
                Some(alias) if speech2.is_none() => voice.status.reloading(&alias),
                _ => voice.turn_frame(speech2.as_deref(), &frame),
            },
        }
    };
    let http = env.http;
    let sig = signal.clone();
    let reading = async move { send_stream_coded(http, &url, &body, sig.as_ref(), handle).await }
        .boxed_local();
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
    // A call the stream left running (a Stop mid-picture, a failure) has no
    // result coming: its live pane goes.
    chat_media::settle(a_tools);
    // Nor is its model still downloading or loading for it.
    voice.turn_ended();
    env.own.bump();
    live.try_set_value(None);
    aborter.try_set_value(None);
    let stopped = signal.as_ref().is_some_and(|s| s.aborted());
    let ok = res.is_ok();
    let (end, said) = turn_end(res, stopped, failed.get(), any_event.get(), inline);
    if let Some(why) = said {
        toasts.err(format!("{what} failed: {why}"));
    }
    if ok {
        // A turn into an archived thread restores it server-side; drop the
        // strip above the composer to match.
        current.try_update(|c| {
            if let Some(t) = c.as_mut().filter(|t| t.id == tid) {
                t.archived_at = None;
                t.purge_at = None;
            }
        });
    }
    end
}

/// How a turn's stream ended: `res` is what [`send_stream_coded`] answered,
/// `stopped` whether the request's signal was aborted (the Stop button, a
/// page that goes), `failed` whether an `error` frame came, `any_event`
/// whether any frame came. A stop is told by the signal, never by the
/// error's words, which are each transport's own ("AbortError: …" from the
/// browser's fetch, "cancelled" from a relay). The second value is the
/// failure to say, when it is not handed back as the inline refusal.
pub(super) fn turn_end(
    res: Result<(), Refusal>,
    stopped: bool,
    failed: bool,
    any_event: bool,
    inline: bool,
) -> (TurnEnd, Option<String>) {
    let mut end = TurnEnd {
        refused: false,
        aborted: false,
        failed,
        refusal: None,
    };
    let Err(e) = res else {
        return (end, None);
    };
    if stopped {
        end.aborted = true;
        return (end, None);
    }
    end.failed = true;
    end.refused = !any_event;
    if inline && end.refused {
        end.refusal = Some(e);
        (end, None)
    } else {
        (end, Some(e.message))
    }
}

/// A `tool` frame: find or open the card at its index and apply the step.
/// An event this build does not know still opens its card, and applies
/// nothing to it. A result's stored files join the message's `files`, where
/// the card's pictures and players read them (`chat_media`).
pub(super) fn apply_tool_frame(
    tools: RwSignal<Vec<ToolCard>>,
    files: RwSignal<Vec<AttachmentMeta>>,
    step: &ToolEvent,
) {
    // A call that waits for a decision names no index: it is one of the
    // cards already shown; a media call's progress names its call by id.
    let Some(index) = step.index() else {
        match step {
            ToolEvent::Frame(ToolFrame::Approval(req)) => {
                chat_approvals::hold_for_frame(tools, req);
            }
            ToolEvent::Frame(ToolFrame::Progress(p)) => chat_media::progress(tools, p),
            _ => {}
        }
        return;
    };
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
                    approval: chat_approvals::Approval::new(),
                    call_id: RwSignal::new(None),
                    media: RwSignal::new(None),
                };
                tools.push(c);
                c
            }
        };
        let ToolEvent::Frame(frame) = step else {
            return;
        };
        match frame {
            ToolFrame::Start(f) => card.name.set(f.name.clone()),
            ToolFrame::Args(f) => card.args.update(|a| a.push_str(&f.fragment)),
            ToolFrame::Ready(f) => {
                if f.call_id.is_some() {
                    card.call_id.set(f.call_id.clone());
                }
                card.name.set(f.name.clone());
                card.args
                    .set(serde_json::to_string_pretty(&f.arguments).unwrap_or_default());
            }
            ToolFrame::Result(f) => {
                if card.call_id.with_untracked(Option::is_none) && f.call_id.is_some() {
                    card.call_id.set(f.call_id.clone());
                }
                chat_media::take_files(files, &f.attachments);
                card.output.set(f.output.clone());
                card.is_error.set(f.is_error);
                card.ms.set(i64::try_from(f.ms).ok());
                // The preview and the stages go with the live pane.
                card.media.set(None);
                card.done.set(true);
            }
            // Neither names a call by its index (handled above).
            ToolFrame::Approval(_) | ToolFrame::Progress(_) => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::chat_stream::{frame, ChatEvent};
    use super::*;
    use serde_json::json;

    /// A relaying host words a stop "cancelled", the browser's fetch
    /// "AbortError: …": the turn tells a stop by its signal either way.
    #[test]
    fn a_stop_is_the_signal_s_whatever_the_transport_says() {
        use futures::executor::block_on;
        use lmgw_ui_kit::http::fake::Fake;

        let http = Http::new(Fake::new(|_| Err("cancelled".into())));
        let res = block_on(send_stream_coded(
            http,
            "/chat/api/threads/1/send",
            &json!({}),
            None,
            |_| {},
        ));
        let err = res.clone().unwrap_err();
        assert!(!err.message.to_lowercase().contains("abort"), "{err:?}");

        // Stopped: no failure, nothing said, nothing to undo.
        let (end, said) = turn_end(res.clone(), true, false, false, true);
        assert!(end.aborted && !end.failed && !end.refused && end.refusal.is_none());
        assert_eq!(said, None);

        // Not stopped, the same words are a failure: before any frame a
        // refusal (inline: handed back; else said) ...
        let (end, said) = turn_end(res.clone(), false, false, false, true);
        assert!(!end.aborted && end.failed && end.refused);
        assert_eq!(end.refusal, Some(err.clone()));
        assert_eq!(said, None);
        let (end, said) = turn_end(res.clone(), false, false, false, false);
        assert!(end.refused && end.refusal.is_none());
        assert_eq!(said.as_deref(), Some("connection failed: cancelled"));
        // ... after one, a failure mid-way, said.
        let (end, said) = turn_end(res, false, false, true, true);
        assert!(end.failed && !end.refused && end.refusal.is_none());
        assert_eq!(said.as_deref(), Some("connection failed: cancelled"));

        // A stream that ended cleanly keeps its error frame's verdict.
        let (end, said) = turn_end(Ok(()), false, true, true, false);
        assert!(end.failed && !end.aborted && said.is_none());
    }

    /// What a card shows.
    #[derive(Debug, PartialEq)]
    struct Shown {
        index: i64,
        name: String,
        args: String,
        output: String,
        is_error: bool,
        ms: Option<i64>,
        done: bool,
    }

    impl Shown {
        /// The card of a call that was only opened.
        fn opened(index: i64) -> Self {
            Self {
                index,
                name: String::new(),
                args: String::new(),
                output: String::new(),
                is_error: false,
                ms: None,
                done: false,
            }
        }
    }

    fn apply(tools: RwSignal<Vec<ToolCard>>, f: Value) {
        match frame("tool", f) {
            Some(ChatEvent::Tool(step)) => {
                apply_tool_frame(tools, RwSignal::new(Vec::new()), &step)
            }
            other => panic!("{other:?}"),
        }
    }

    /// The cards a stream's `tool` frames build, in an owner of their own.
    fn run(frames: Vec<Value>) -> Vec<Shown> {
        let owner = Owner::new();
        owner.with(|| {
            let tools = RwSignal::new(Vec::<ToolCard>::new());
            for f in frames {
                apply(tools, f);
            }
            tools.with_untracked(|t| {
                t.iter()
                    .map(|c| Shown {
                        index: c.index,
                        name: c.name.get_untracked(),
                        args: c.args.get_untracked(),
                        output: c.output.get_untracked(),
                        is_error: c.is_error.get_untracked(),
                        ms: c.ms.get_untracked(),
                        done: c.done.get_untracked(),
                    })
                    .collect()
            })
        })
    }

    #[test]
    fn a_call_builds_its_card_from_start_to_result() {
        let cards = run(vec![
            json!({"event": "start", "index": 0, "name": "docs__read"}),
            json!({"event": "args", "index": 0, "fragment": "{\"a\":"}),
            json!({"event": "args", "index": 0, "fragment": "1}"}),
            json!({"event": "ready", "index": 0, "name": "docs__read", "arguments": {"a": 1}}),
            json!({"event": "result", "index": 0, "name": "docs__read", "output": "text",
                   "is_error": true, "ms": 41}),
            json!({"event": "start", "index": 1, "name": "docs__list"}),
        ]);
        assert_eq!(
            cards,
            [
                Shown {
                    index: 0,
                    name: "docs__read".into(),
                    args: serde_json::to_string_pretty(&json!({"a": 1})).unwrap(),
                    output: "text".into(),
                    is_error: true,
                    ms: Some(41),
                    done: true,
                },
                Shown {
                    name: "docs__list".into(),
                    ..Shown::opened(1)
                },
            ]
        );
    }

    #[test]
    fn an_unknown_tool_event_opens_its_card_and_changes_nothing() {
        // A newer gateway's event: the card at its index, once.
        let cards = run(vec![
            json!({"event": "checkpoint", "index": 2, "percent": 40}),
            json!({"event": "checkpoint", "index": 2, "percent": 80}),
        ]);
        assert_eq!(cards, [Shown::opened(2)]);
        // On a card already open it applies nothing.
        let cards = run(vec![
            json!({"event": "start", "index": 0, "name": "docs__read"}),
            json!({"event": "checkpoint", "index": 0}),
        ]);
        assert_eq!(
            cards,
            [Shown {
                name: "docs__read".into(),
                ..Shown::opened(0)
            }]
        );
        // No event and no index: the first card.
        assert_eq!(run(vec![json!({})]), [Shown::opened(0)]);
    }

    #[test]
    fn an_approval_holds_the_card_of_its_call_and_opens_none() {
        let owner = Owner::new();
        owner.with(|| {
            let tools = RwSignal::new(Vec::<ToolCard>::new());
            apply(
                tools,
                json!({"event": "start", "index": 0, "name": "mail__send"}),
            );
            apply(
                tools,
                json!({"event": "ready", "index": 0, "name": "mail__send",
                       "arguments": {"to": "x"}, "needs_approval": true}),
            );
            apply(
                tools,
                json!({"event": "approval", "approval_request_id": "mcpr_1",
                       "server_label": "mail", "name": "send", "arguments": "{\"to\":\"x\"}"}),
            );
            tools.with_untracked(|t| {
                assert_eq!(t.len(), 1);
                assert_eq!(
                    t[0].approval.hold.get_untracked(),
                    Some(chat_approvals::Hold::Gated {
                        id: "mcpr_1".into(),
                        label: "mail".into()
                    })
                );
                assert!(t[0].done.get_untracked());
            });
        });
    }

    /// A media call (chat-media-tools design §6.2): its `progress` frames
    /// reach its card by the id its `ready` frame gave it, a frame for
    /// another call is dropped, and its result's files join the message.
    /// The chat model's `loading` after it reads as the reload it is.
    #[test]
    fn a_media_call_takes_its_progress_by_id_and_hands_its_files_to_the_message() {
        let owner = Owner::new();
        owner.with(|| {
            let tools = RwSignal::new(Vec::<ToolCard>::new());
            let files = RwSignal::new(Vec::<AttachmentMeta>::new());
            let step = |f: Value| match frame("tool", f) {
                Some(ChatEvent::Tool(s)) => apply_tool_frame(tools, files, &s),
                other => panic!("{other:?}"),
            };
            step(
                json!({"event": "ready", "index": 0, "name": "image__generate",
                        "arguments": {"size": "512x512"}, "call_id": "call_1"}),
            );
            step(
                json!({"event": "progress", "call_id": "call_1", "phase": "generating",
                        "model": "image/z", "pass": 1, "step": 3, "total_steps": 8,
                        "elapsed_ms": 900, "cancel": "restart", "preview": "AAA"}),
            );
            step(
                json!({"event": "progress", "call_id": "call_9", "phase": "loading",
                        "elapsed_ms": 1}),
            );
            let card = tools.with_untracked(|t| t[0]);
            assert_eq!(tools.with_untracked(Vec::len), 1, "progress opens no card");
            let live = card.media.get_untracked().expect("the call's progress");
            assert_eq!(live.step().as_deref(), Some("Step 3 / 8"));
            assert_eq!(live.preview.as_deref(), Some("AAA"));
            assert_eq!(
                live.last.cancel,
                Some(lmgw_api_types::chat_media::CancelKind::Restart)
            );
            assert_eq!(live.stages, ["Generating"]);
            let state = super::super::chat_stream::frame(
                "state",
                json!({"stage": "chat", "alias": "chat/q", "state": "loading"}),
            );
            let Some(ChatEvent::Voice(state)) = state else {
                panic!("a state frame")
            };
            assert_eq!(
                chat_media::regain_alias(&state, tools),
                None,
                "not while the picture is drawn"
            );
            step(
                json!({"event": "result", "index": 0, "name": "image__generate",
                        "output": "Made image att:5", "is_error": false, "ms": 1200,
                        "call_id": "call_1", "attachments": [{"id": 5, "kind": "image",
                        "name": "z-7.png", "mime": "image/png", "size": 10, "meta": {},
                        "source": "tool", "call_id": "call_1"}]}),
            );
            assert_eq!(
                files.with_untracked(|f| f.iter().map(|a| a.id).collect::<Vec<_>>()),
                [5]
            );
            assert!(card.done.get_untracked());
            assert!(
                card.media.get_untracked().is_none(),
                "the result drops the preview and the stages"
            );
            assert_eq!(
                chat_media::regain_alias(&state, tools).as_deref(),
                Some("chat/q")
            );
        });
    }
}
