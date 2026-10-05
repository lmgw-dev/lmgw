//! Message actions (chat-complete §3): Copy, Edit, Delete, Regenerate and
//! Continue, under every message.
//!
//! The conversation stays one linear list, and an action that rewrites
//! history is final: whatever it cuts is deleted server-side, and here. The
//! transcript is cut only once the server has accepted the action (its first
//! stream frame), so a refusal leaves everything as it was.

use std::rc::Rc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use serde_json::{json, Value};

use super::chat::{in_owner, new_msg, scroll_down, Msg, ThreadDetail};
use super::chat_knowledge::KbDraftChips;
use super::chat_turn::{run_turn, Turn, TurnEnv};
use crate::widgets::{use_toasts, ConfirmButton};

/// The thread's `continue` verdict: whether its last reply can be continued
/// on the route the thread resolves to, and if not, why.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ContinueState {
    pub ok: bool,
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers over the message list
// ---------------------------------------------------------------------------

/// How many messages follow the one at `idx` in a list of `len`.
pub(super) fn later_count(len: usize, idx: usize) -> usize {
    len.saturating_sub(idx + 1)
}

/// Cut the list the way the server does for a regenerate at `idx`: a reply
/// goes with everything after it, a user message keeps itself.
pub(super) fn cut_for_regenerate<T>(v: &mut Vec<T>, idx: usize, is_user: bool) {
    v.truncate(if is_user { idx + 1 } else { idx });
}

/// "discards 1 later message" / "discards 4 later messages".
pub(super) fn discards_text(n: usize) -> String {
    format!(
        "discards {n} later message{}",
        if n == 1 { "" } else { "s" }
    )
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// What the page hands the message actions besides the callbacks.
#[derive(Clone, Copy)]
pub(super) struct ActionEnv {
    pub turn: TurnEnv,
    pub owner: StoredValue<WeakOwner>,
    pub next_key: StoredValue<u64>,
    /// Re-read the thread list (an action bumps the thread's `updated_at`).
    pub refresh: Callback<()>,
}

impl ActionEnv {
    fn alloc_key(&self) -> u64 {
        let k = self.next_key.get_value();
        self.next_key.set_value(k + 1);
        k
    }

    fn busy(&self) -> bool {
        self.turn.streaming.get_untracked().is_some()
    }

    /// The open thread, and the position and stored id of the message `key`.
    fn locate(&self, key: u64) -> Option<(i64, usize, i64, Msg)> {
        let tid = self.turn.current_id.get_untracked()?;
        let (idx, m) = self.turn.msgs.with_untracked(|v| {
            v.iter()
                .position(|m| m.key == key)
                .map(|i| (i, v[i].clone()))
        })?;
        match m.db_id.get_untracked() {
            Some(id) => Some((tid, idx, id, m)),
            None => {
                self.turn
                    .toasts
                    .warn("this message is not stored yet — reload the conversation");
                None
            }
        }
    }

    fn msg_url(tid: i64, mid: i64, action: &str) -> String {
        format!("/chat/api/threads/{tid}/messages/{mid}/{action}")
    }

    /// A fresh assistant bubble under the page's owner.
    fn new_reply(&self) -> Option<Msg> {
        in_owner(self.owner, || {
            new_msg(self.alloc_key(), "assistant", String::new())
        })
    }

    /// Once a turn is over: the thread's `continue` verdict and the stored
    /// ids of bubbles that were only optimistic, read back from the server.
    /// `refresh_last` also re-reads the last message's text (a continuation
    /// that failed leaves the stored reply as it was).
    pub fn resync(&self, tid: i64, refresh_last: bool) {
        let env = *self;
        env.turn.scope.spawn(async move {
            let Ok(d) = crate::api::get::<ThreadDetail>(format!("/chat/api/threads/{tid}")).await
            else {
                return;
            };
            if env.turn.current_id.get_untracked() != Some(tid) {
                return;
            }
            env.turn.current.update(|c| {
                if let Some(t) = c.as_mut().filter(|t| t.id == tid) {
                    t.cont = d.thread.cont.clone();
                }
            });
            // Only when the list is the same shape: a turn in flight (or a
            // thread reopened meanwhile) makes positions meaningless.
            if env.turn.streaming.get_untracked().is_some() {
                return;
            }
            env.turn.msgs.with_untracked(|v| {
                // A reply the server refused to store is a bubble with no row:
                // the stored rows are compared without it.
                let v: Vec<&Msg> = v.iter().filter(|m| !m.unsaved.get_untracked()).collect();
                if v.len() != d.messages.len() {
                    return;
                }
                let last = v.len().saturating_sub(1);
                let mut answering = None;
                for (i, (m, r)) in v.iter().zip(&d.messages).enumerate() {
                    if m.role != r.role {
                        return;
                    }
                    m.db_id.set(Some(r.id));
                    // What the server stored for knowledge wins over what the
                    // live stream showed, once there is any.
                    if r.role == "user" {
                        answering = r.context.as_ref();
                        if m.kb_refs.with_untracked(|k| *k != r.kb_refs) {
                            m.kb_refs.set(r.kb_refs.clone());
                        }
                    } else if let Some(c) = answering {
                        if m.context.with_untracked(Option::is_none) {
                            m.context.set(Some(c.clone()));
                        }
                    }
                    if refresh_last && i == last {
                        m.content.set(r.content.clone());
                        m.reasoning.set(r.reasoning.clone());
                        m.tokens.set(match (r.prompt_tokens, r.completion_tokens) {
                            (Some(p), Some(c)) => Some((p, c)),
                            _ => None,
                        });
                    }
                    if r.role == "assistant" {
                        m.model.set(r.model.clone());
                        m.answered_by.set(r.answered_by.clone());
                    }
                    if m.voice.with_untracked(|v| *v != r.voice) {
                        m.voice.set(r.voice.clone());
                    }
                }
            });
        });
    }

    /// Delete one message.
    pub fn delete(&self, key: u64) {
        if self.busy() {
            return;
        }
        let Some((tid, _, mid, _)) = self.locate(key) else {
            return;
        };
        let env = *self;
        env.turn.scope.spawn(async move {
            match crate::api::post::<Value, _>(Self::msg_url(tid, mid, "delete"), &json!({})).await
            {
                Ok(_) => {
                    if env.turn.current_id.get_untracked() == Some(tid) {
                        env.turn.msgs.update(|v| v.retain(|m| m.key != key));
                    }
                    env.resync(tid, false);
                    env.refresh.run(());
                }
                Err(e) => env
                    .turn
                    .toasts
                    .err(format!("deleting the message failed: {e}")),
            }
        });
    }

    /// Edit an assistant reply in place — no resend. The server clears what
    /// no longer describes the text (thinking, token counts, tool record).
    pub fn edit_assistant(&self, key: u64, text: String, done: Rc<dyn Fn(bool)>) {
        if self.busy() {
            done(false);
            return;
        }
        let Some((tid, _, mid, m)) = self.locate(key) else {
            done(false);
            return;
        };
        let env = *self;
        env.turn.scope.spawn(async move {
            let res = crate::api::post::<Value, _>(
                Self::msg_url(tid, mid, "edit"),
                &json!({ "content": text }),
            )
            .await;
            match res {
                Ok(v) => {
                    let saved = v["message"]["content"]
                        .as_str()
                        .unwrap_or(&text)
                        .to_string();
                    m.content.set(saved);
                    m.reasoning.set(String::new());
                    m.tokens.set(None);
                    m.tools.set(Vec::new());
                    env.resync(tid, false);
                    env.refresh.run(());
                    done(true);
                }
                Err(e) => {
                    env.turn.toasts.err(format!("saving the edit failed: {e}"));
                    done(false);
                }
            }
        });
    }

    /// Edit a user message and send it again: everything after it is deleted
    /// and the reply streams anew. `done(true)` when the server took it.
    pub fn edit_user(&self, key: u64, text: String, kb_refs: Vec<i64>, done: Rc<dyn Fn(bool)>) {
        if self.busy() {
            done(false);
            return;
        }
        let Some((tid, idx, mid, m)) = self.locate(key) else {
            done(false);
            return;
        };
        let Some(reply) = self.new_reply() else {
            done(false);
            return;
        };
        let env = *self;
        let (accept_done, refuse_done) = (done.clone(), done);
        let (accept_text, accept_reply, accept_msg) = (text.clone(), reply.clone(), m.clone());
        let accept_kb = kb_refs.clone();
        let turn = Turn {
            tid,
            url: Self::msg_url(tid, mid, "edit"),
            body: json!({ "content": text, "kb_refs": kb_refs }),
            target: reply,
            continuing: false,
            what: "edit",
        };
        let user_db = m.db_id;
        spawn_local(async move {
            let end = run_turn(
                env.turn,
                turn,
                move |id| user_db.set(Some(id)),
                move || {
                    // Accepted: the text is replaced and the later bubbles go.
                    // A dictated message whose text changed is typed now
                    // (chat-voice §3).
                    if accept_msg.content.with_untracked(|c| *c != accept_text) {
                        accept_msg.voice.set(None);
                    }
                    accept_msg.content.set(accept_text.clone());
                    accept_msg.kb_refs.set(accept_kb.clone());
                    // Runs under `spawn_local`, not the page scope: a task dropped with the page
                    // would cancel the fetch and the server would save a truncated
                    // reply. So the task outlives the page and a left page has no
                    // signals left — hence the `try_` reads and `scope.alive()`.
                    if env.turn.current_id.try_get_untracked().flatten() == Some(tid) {
                        env.turn.msgs.update(|v| {
                            v.truncate(idx + 1);
                            v.push(accept_reply.clone());
                        });
                        scroll_down(true);
                    }
                    accept_done(true);
                },
            )
            .await;
            if end.refused {
                refuse_done(false);
            }
            if env.turn.scope.alive() {
                env.resync(tid, false);
                env.refresh.run(());
            }
        });
    }

    /// Answer again: a reply is deleted with everything after it, a user
    /// message keeps itself and loses everything after.
    pub fn regenerate(&self, key: u64) {
        if self.busy() {
            return;
        }
        let Some((tid, idx, mid, m)) = self.locate(key) else {
            return;
        };
        let is_user = m.role == "user";
        let Some(reply) = self.new_reply() else {
            return;
        };
        let env = *self;
        let accept_reply = reply.clone();
        let turn = Turn {
            tid,
            url: Self::msg_url(tid, mid, "regenerate"),
            body: json!({}),
            target: reply,
            continuing: false,
            what: "regenerate",
        };
        let user_db = m.db_id;
        spawn_local(async move {
            run_turn(
                env.turn,
                turn,
                move |id| {
                    if is_user {
                        user_db.set(Some(id));
                    }
                },
                move || {
                    // Runs under `spawn_local`, not the page scope: a task dropped with the page
                    // would cancel the fetch and the server would save a truncated
                    // reply. So the task outlives the page and a left page has no
                    // signals left — hence the `try_` reads and `scope.alive()`.
                    if env.turn.current_id.try_get_untracked().flatten() == Some(tid) {
                        env.turn.msgs.update(|v| {
                            cut_for_regenerate(v, idx, is_user);
                            v.push(accept_reply.clone());
                        });
                        scroll_down(true);
                    }
                },
            )
            .await;
            if env.turn.scope.alive() {
                env.resync(tid, false);
                env.refresh.run(());
            }
        });
    }

    /// The model continues the last reply, in the same bubble.
    pub fn resume(&self) {
        if self.busy() {
            return;
        }
        let Some(tid) = self.turn.current_id.get_untracked() else {
            return;
        };
        let Some(target) = self
            .turn
            .msgs
            .with_untracked(|v| v.last().filter(|m| m.role == "assistant").cloned())
        else {
            return;
        };
        let env = *self;
        let content = target.content;
        let turn = Turn {
            tid,
            url: format!("/chat/api/threads/{tid}/continue"),
            body: json!({}),
            target,
            continuing: true,
            what: "continue",
        };
        // The server prefills with the reply's trailing whitespace trimmed and
        // stores it that way; the bubble follows before the new text lands, so
        // it shows what is stored.
        content.update(|c| c.truncate(c.trim_end().len()));
        scroll_down(true);
        spawn_local(async move {
            let _ = run_turn(env.turn, turn, |_| {}, || {}).await;
            if env.turn.scope.alive() {
                // Re-read the reply either way: the stored text is the truth
                // (after a failed or refused continuation it is what it was,
                // after a good one it is the trimmed prefill plus the new
                // text).
                env.resync(tid, true);
                env.refresh.run(());
            }
        });
    }
}

// ---------------------------------------------------------------------------
// The action row and the in-place editor
// ---------------------------------------------------------------------------

/// A request to save an edited message: the text, and how to tell the editor
/// whether the server took it (`true` closes it, `false` keeps it open).
pub(super) struct EditReq {
    pub key: u64,
    pub text: String,
    /// A user message's own knowledge bases, as the editor left them.
    pub kb_refs: Vec<i64>,
    pub done: Rc<dyn Fn(bool)>,
}

/// What a message row needs to offer its actions.
#[derive(Clone, Copy)]
pub(super) struct MsgOps {
    pub msgs: RwSignal<Vec<Msg>>,
    /// Any reply is streaming — every action waits.
    pub busy: Signal<bool>,
    pub cont: Signal<Option<ContinueState>>,
    pub regenerate: Callback<u64>,
    pub edit: Callback<EditReq>,
    pub resume: Callback<()>,
    pub delete: Callback<u64>,
}

const ICON_COPY: &str = "M5.5 5.5h7v8h-7z M3.5 10.5v-8h7";
const ICON_EDIT: &str = "M2.5 13.5l.7-3L10.8 3a1.6 1.6 0 0 1 2.2 2.2L5.5 12.8z M9.6 4.2l2.2 2.2";
const ICON_REGEN: &str = "M13.2 8a5.2 5.2 0 1 1-1.6-3.7 M13.4 2.6v3h-3";
const ICON_CONTINUE: &str = "M3 3.5l5 4.5-5 4.5z M9 3.5l5 4.5-5 4.5z";
const ICON_DELETE: &str = "M3 4.5h10 M6.5 4.5V3h3v1.5 M4.5 4.5l.6 8.5h5.8l.6-8.5 M7 7v4 M9 7v4";

fn icon(path: &'static str) -> impl IntoView {
    view! {
        <svg viewBox="0 0 16 16" aria-hidden="true">
            <path d=path></path>
        </svg>
    }
}

/// The row under a message: shown on hover or focus (always on a narrow
/// pane), never while its own reply streams.
#[component]
pub(super) fn MsgActions(m: Msg, ops: MsgOps, editing: RwSignal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let key = m.key;
    let is_user = m.role == "user";
    let content = m.content;
    let db_id = m.db_id;
    let own_streaming = m.streaming;
    let unsaved = m.unsaved;
    let m_voice = m.voice;
    let off = Signal::derive(move || ops.busy.get() || db_id.get().is_none() || editing.get());
    let off_title = move |what: &'static str| {
        move || {
            if ops.busy.get() {
                "a reply is streaming — wait for it or stop it".to_string()
            } else if db_id.get().is_none() {
                "not stored yet — reload the conversation".to_string()
            } else {
                what.to_string()
            }
        }
    };
    let later = Memo::new(move |_| {
        ops.msgs.with(|v| {
            v.iter()
                .position(|x| x.key == key)
                .map(|i| later_count(v.len(), i))
                .unwrap_or(0)
        })
    });
    let is_last = Memo::new(move |_| ops.msgs.with(|v| v.last().is_some_and(|l| l.key == key)));
    let can_continue = Memo::new(move |_| {
        !is_user && is_last.get() && ops.cont.with(|c| c.as_ref().is_some_and(|c| c.ok))
    });

    view! {
        <Show when=move || !own_streaming.get()>
            <div class="msg-actions" role="toolbar" aria-label="Message actions">
                <button
                    type="button"
                    class="msg-act"
                    title="Copy the message (markdown source)"
                    aria-label="Copy"
                    on:click=move |_| {
                        crate::widgets::copy_secret(
                            &content.get_untracked(),
                            toasts,
                            "message copied".to_string(),
                        )
                    }
                >
                    {icon(ICON_COPY)}
                </button>
                // A reply that was never stored has no row to edit, answer again,
                // continue or delete: Copy is all it has.
                <Show when=move || !unsaved.get()>
                {(!is_user).then(|| view! {
                    <super::chat_voice::SpeakerButton key=key db_id=db_id editing=editing/>
                })}
                <button
                    type="button"
                    class="msg-act"
                    title=off_title(
                        if is_user {
                            "Edit and send again — deletes the messages after it"
                        } else {
                            "Edit this reply in place"
                        },
                    )
                    aria-label="Edit"
                    disabled=move || off.get()
                    on:click=move |_| editing.set(true)
                >
                    {icon(ICON_EDIT)}
                </button>
                // On a reply and on a user message: the reply is deleted with
                // everything after it, a user message keeps itself and loses
                // everything after it, and the model answers it again.
                {move || {
                        let n = later.get();
                        // A voice turn the model heard as audio is answered
                        // again from its transcript (voice-audio-input §3.4).
                        let heard = m_voice.with(|v| v.as_ref().is_some_and(|v| v.heard_as_audio()));
                        let tip = off_title(match (is_user, heard) {
                            (true, false) => "Answer this message again — deletes everything after it",
                            (true, true) => {
                                "Answer this message again — answers from the transcript: the audio \
                                 is not kept — deletes everything after it"
                            }
                            (false, false) => {
                                "Answer again — deletes this reply and everything after it"
                            }
                            (false, true) => {
                                "Answer again — answers from the transcript: the audio is not kept \
                                 — deletes this reply and everything after it"
                            }
                        });
                        if n > 0 {
                            view! {
                                <ConfirmButton
                                    label=""
                                    icon=ViewFn::from(move || icon(ICON_REGEN))
                                    confirm=discards_text(n)
                                    class="msg-act"
                                    title=tip
                                    disabled=off
                                    on_confirm=Callback::new(move |()| ops.regenerate.run(key))
                                />
                            }
                                .into_any()
                        } else {
                            view! {
                                <button
                                    type="button"
                                    class="msg-act"
                                    title=tip
                                    aria-label="Regenerate"
                                    disabled=move || off.get()
                                    on:click=move |_| ops.regenerate.run(key)
                                >
                                    {icon(ICON_REGEN)}
                                </button>
                            }
                                .into_any()
                        }
                    }}
                <Show when=move || can_continue.get()>
                    <button
                        type="button"
                        class="msg-act"
                        title=off_title("Let the model carry on from where this reply ends")
                        aria-label="Continue"
                        disabled=move || off.get()
                        on:click=move |_| ops.resume.run(())
                    >
                        {icon(ICON_CONTINUE)}
                    </button>
                </Show>
                <ConfirmButton
                    label=""
                    icon=ViewFn::from(move || icon(ICON_DELETE))
                    confirm="Delete this message?"
                    class="msg-act"
                    title=off_title("Delete this message")
                    disabled=off
                    on_confirm=Callback::new(move |()| ops.delete.run(key))
                />
                </Show>
            </div>
        </Show>
    }
}

/// The message opened for editing in place: a textarea with Save and Cancel.
/// Ctrl+Enter saves, Esc cancels.
#[component]
pub(super) fn MsgEditor(m: Msg, ops: MsgOps, editing: RwSignal<bool>) -> impl IntoView {
    let key = m.key;
    let is_user = m.role == "user";
    let draft = RwSignal::new(m.content.get_untracked());
    let kbs = RwSignal::new(m.kb_refs.get_untracked());
    let saving = RwSignal::new(false);
    let ta: NodeRef<leptos::html::Textarea> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(el) = ta.get() {
            let _ = el.focus();
        }
    });
    let later = Memo::new(move |_| {
        ops.msgs.with(|v| {
            v.iter()
                .position(|x| x.key == key)
                .map(|i| later_count(v.len(), i))
                .unwrap_or(0)
        })
    });
    let tools = m.tools;
    let reasoning = m.reasoning;
    let unchanged =
        move || draft.with(|d| *d == m.content.get()) && kbs.with(|k| *k == m.kb_refs.get());
    let can_save = move || {
        !saving.get()
            && !ops.busy.get()
            && !unchanged()
            && (!is_user
                || !draft.with(|d| d.trim().is_empty())
                || !m.attachments.with(Vec::is_empty))
    };
    let save = move || {
        if !can_save() {
            return;
        }
        saving.set(true);
        let done: Rc<dyn Fn(bool)> = Rc::new(move |ok| {
            saving.try_set(false);
            if ok {
                editing.try_set(false);
            }
        });
        ops.edit.run(EditReq {
            key,
            text: draft.get_untracked(),
            kb_refs: if is_user {
                kbs.get_untracked()
            } else {
                Vec::new()
            },
            done,
        });
    };
    view! {
        <div class="msg-editor">
            <textarea
                class="input ta msg-edit-ta"
                node_ref=ta
                aria-label="Edit message"
                prop:value=move || draft.get()
                on:input=move |ev| draft.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Escape" {
                        ev.stop_propagation();
                        editing.set(false);
                    } else if ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
                        ev.prevent_default();
                        save();
                    }
                }
            ></textarea>
            <Show when=move || is_user && !kbs.with(Vec::is_empty)>
                <KbDraftChips chips=kbs/>
            </Show>
            <Show when=move || is_user && (later.get() > 0)>
                <div class="dim mini-note msg-edit-note">
                    {move || format!("Saving {}.", super::chat_actions::discards_text(later.get()))}
                </div>
            </Show>
            <Show when=move || !is_user && !tools.with(Vec::is_empty)>
                <div class="dim mini-note msg-edit-note">
                    "This reply ran tools: saving drops its tool record from the history."
                </div>
            </Show>
            <Show when=move || !is_user && !reasoning.with(String::is_empty)>
                <div class="dim mini-note msg-edit-note">
                    "Saving clears this reply's thinking and token counts."
                </div>
            </Show>
            <div class="msg-edit-btns">
                <button type="button" class="btn ghost sm" on:click=move |_| editing.set(false)>
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="btn primary sm"
                    disabled=move || !can_save()
                    title="Ctrl+Enter"
                    on:click=move |_| save()
                >
                    {if is_user { "Save & send" } else { "Save" }}
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_messages_are_the_ones_after_it() {
        assert_eq!(later_count(6, 1), 4);
        assert_eq!(later_count(6, 5), 0);
        assert_eq!(later_count(0, 0), 0);
    }

    #[test]
    fn a_regenerate_cuts_a_reply_but_keeps_a_user_message() {
        let mut v = vec!["u1", "a1", "u2", "a2"];
        cut_for_regenerate(&mut v, 1, false);
        assert_eq!(v, ["u1"]);
        let mut v = vec!["u1", "a1", "u2", "a2"];
        cut_for_regenerate(&mut v, 2, true);
        assert_eq!(v, ["u1", "a1", "u2"]);
        // the last reply: nothing after it
        let mut v = vec!["u1", "a1"];
        cut_for_regenerate(&mut v, 1, false);
        assert_eq!(v, ["u1"]);
    }

    #[test]
    fn the_truncation_warning_counts_and_pluralises() {
        assert_eq!(discards_text(1), "discards 1 later message");
        assert_eq!(discards_text(4), "discards 4 later messages");
    }

    #[test]
    fn the_continue_verdict_reads_the_thread_json() {
        let c: ContinueState =
            serde_json::from_value(json!({"ok": false, "reason": "no prefill"})).unwrap();
        assert_eq!((c.ok, c.reason.as_deref()), (false, Some("no prefill")));
        let c: ContinueState = serde_json::from_value(json!({"ok": true, "reason": null})).unwrap();
        assert!(c.ok && c.reason.is_none());
    }
}
