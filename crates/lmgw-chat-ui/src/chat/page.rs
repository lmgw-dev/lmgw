//! The Chat component: the page's state, its actions, and its view (the
//! thread list and the settings drawer beside the conversation).

use super::drawer::ThreadSettings;
use super::list::{purge_text, thread_items, today_local, ThreadList};
use super::msg_view::{MsgView, StatsRow};
use super::*;

#[component]
pub fn Chat() -> impl IntoView {
    let toasts = use_toasts();
    // The transport the host handed the page (the dashboard: fetch).
    let http = lmgw_ui_kit::http::use_http();
    // What the host hands the Chat besides the transport (`crate::host`).
    let host = crate::host::use_chat_host();
    let place = crate::host::Place::new(host.location.clone());
    // Where the user writes from, asked at each send and edit (§9.10).
    let origin = crate::host::Origin::new(host.origin.clone());
    let opened = host.location.opened_at();
    // The live updates from the host's source: what the follower reads
    // again, and the GPU hold the voice controls show (chat-plane design D1).
    crate::chat_live::provide();

    let threads = RwSignal::new(Vec::<ChatThread>::new());
    let current = RwSignal::new(None::<ChatThread>);
    let msgs = RwSignal::new(Vec::<Msg>::new());
    // The open thread's MCP tasks (`chat_tasks`), and a refused Answer's
    // words.
    let thread_tasks: chat_tasks::Tasks = RwSignal::new(None);
    let answer_said = RwSignal::new(None::<String>);
    let next_key = StoredValue::new(0u64);
    let catalog = lmgw_ui_kit::catalog::use_model_catalog();
    let model_sel = RwSignal::new(String::new());
    let composer = RwSignal::new(String::new());
    // Knowledge (chat-complete §9.3): the bases, the one source viewer, and
    // the `#` picks of the message being written.
    let kbui = KbUi::provide(Scope::new());
    // This window's audio devices and echo mode (chat-voice §2.4), watched
    // only where the host has voice.
    chat_voice::provide_voice_devices(host.voice.is_some());
    // The personality profiles the pickers and the header chip name.
    let profile_dir = crate::chat_profiles::ProfileDir::provide();
    let draft_kbs = RwSignal::new(Vec::<i64>::new());
    // A new chat the composer's draft waits in, its conversation deleted
    // elsewhere: made only when the owner presses Send, and that Send under
    // way (`chat_sync::gone`).
    let rescue = RwSignal::new(Rescue::default());
    let making = Memo::new(move |_| rescue.with(Rescue::making));
    let composer_ta: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let composer_box: NodeRef<leptos::html::Div> = NodeRef::new();
    let kb_pick = KbPick::new(draft_kbs, composer, composer_ta);
    composer::follow(composer, composer_ta, composer_box);
    let scroll_pane: NodeRef<leptos::html::Div> = NodeRef::new();
    let scroll_col: NodeRef<leptos::html::Div> = NodeRef::new();
    tail::install(scroll_pane, scroll_col);
    // Dictation, read-aloud and their status line (chat-voice WP7).
    let page_voice =
        chat_voice::provide_page_voice(current, composer, composer_ta, host.voice.is_some());
    // The thread whose reply is streaming, not just "something is": the
    // owner may open another thread meanwhile, and that one's Stop must not
    // abort this stream, nor its stats row show this stream's numbers.
    let streaming = RwSignal::new(None::<i64>);
    // The streaming reply's own message, to put back under its thread when
    // the owner returns to it before the server has stored it.
    let live = StoredValue::new(None::<(i64, Msg)>);
    // The last reply's numbers, tagged with its thread.
    let stats = RwSignal::new(None::<(i64, Stats)>);
    let aborter = StoredValue::new(None::<web_sys::AbortController>);
    // The list's read: `None` while the first is out, the error of the last
    // one that failed. A failed read is not "no conversations" (code:A3).
    let threads_state = RwSignal::new(None::<Result<(), String>>);
    // The toolbar's "Archived (n)" toggle: which list is fetched and shown.
    // The count is carried by every list response, active or archived, so the
    // toggle's label is right even while looking at the other one.
    let view_archived = RwSignal::new(false);
    // A search hit's message, to scroll to and flash once its thread's
    // transcript is in (`?m=` on a cold load, or a click on a hit).
    let focus = RwSignal::new(None::<i64>);
    install_reveal(focus, msgs);
    let archived_count = RwSignal::new(0i64);
    // The gateway's in-memory temporary chats (chat-complete §7), listed in
    // their own group above the dated ones; and Keep's write in flight.
    let temp_threads = RwSignal::new(Vec::<ChatThread>::new());
    let keep_busy = RwSignal::new(false);
    // The folders (chat-complete §5), read with every thread list; the folder
    // UI's own state and actions are `folders`, made once the callbacks they
    // need exist.
    let folder_list = RwSignal::new(Vec::<FolderInfo>::new());
    // An ongoing folder's draft: the picker is held on its model (review
    // CF-2), as it is while a Send is under way.
    let held = chat_sync::held_model(rescue, folder_list);
    let picker_held = Signal::derive(move || making.get() || held.with(Option::is_some));
    // The open temporary chat's id (0 = none), readable from `on_cleanup`,
    // which must not touch signals the disposal may already have taken.
    let open_temp = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    // The composer's chips: uploaded (or uploading) attachments not yet sent,
    // reseeded from `draft_attachments` whenever a thread opens.
    let draft_attachments = RwSignal::new(Vec::<DraftChip>::new());
    let drag_over = RwSignal::new(false);
    let file_input: NodeRef<leptos::html::Input> = NodeRef::new();
    // A chip's own click: the image full-size, or a text file's content, in a
    // shared Modal rather than one per message.
    let viewer_open = RwSignal::new(false);
    let viewer_is_image = RwSignal::new(false);
    let viewer_name = RwSignal::new(String::new());
    let viewer_src = RwSignal::new(String::new());
    // The open non-image attachment: its header (tokens, Download original).
    let viewer_att = RwSignal::new(None::<AttachmentMeta>);
    let viewer_loading = RwSignal::new(false);
    // Bumped on every open; a text fetch only applies its answer if it is
    // still the current one (review nit: opening a text attachment then an
    // image before the text fetch returns put the text into the image
    // modal, since both share `viewer_src`).
    let viewer_gen = StoredValue::new(0u64);
    // Signals created inside event handlers die with that handler's view
    // scope (the Send button disposes itself when it flips to Stop) — new
    // per-message signals must be owned by the component instead. Held
    // weakly: this StoredValue lives in that owner's own arena, and a strong
    // handle there is a cycle that never let the page be disposed — every
    // visit to Chat used to stay in memory with its listeners, observers and
    // detached transcript (review code:C3).
    let owner = StoredValue::new(Owner::current().expect("component owner").downgrade());
    // Reads that answer after the page was left end there (review code:C4):
    // a late thread load would otherwise rewrite the next page's URL.
    let scope = Scope::new();

    let alloc_key = move || {
        let k = next_key.get_value();
        next_key.set_value(k + 1);
        k
    };

    // The list's one read, shared with the follower (`chat_sync`): the
    // newest read lands, and only what differs is set. A failed re-read
    // keeps the list it had and says so above it.
    let list = ListRead {
        threads,
        temporary: temp_threads,
        folders: folder_list,
        archived_count,
        state: threads_state,
        view_archived,
        latest: Latest::new(),
        out: StoredValue::new(0),
        http,
    };
    let refresh_threads = move || scope.spawn(list.read());
    // A delete of the page's own under way: the follower leaves that one to
    // it.
    let deleting = StoredValue::new(None::<i64>);
    // The page's own changes to the open transcript, counted: a read of it
    // the follower made before one of them is dropped (`chat_sync`).
    let own = OwnEdits::new();

    // Per-message signals are created here rather than in the calling task, so
    // they belong to the component and die with it (see `owner` above).
    let msgs_of_rows = move |rows: Vec<MsgRow>| -> Vec<Msg> {
        let mapped = in_owner(owner, || {
            // The retrieval of the user message an answer follows.
            let mut answering: Option<KbContext> = None;
            rows.into_iter()
                .map(|r| {
                    let m = new_msg(alloc_key(), &r.role, r.content);
                    m.db_id.set(Some(r.id));
                    if r.role == "user" {
                        answering = r.context;
                        m.kb_refs.set(r.kb_refs);
                    } else if answering.is_some() {
                        m.context.set(answering.clone());
                    }
                    m.reasoning.set(r.reasoning);
                    m.voice.set(r.voice);
                    m.origin.set(r.origin);
                    m.model.set(r.model);
                    m.answered_by.set(r.answered_by);
                    m.images_note.set(r.images_note);
                    m.task.set(r.task);
                    m.created_at
                        .set(Some(r.created_at).filter(|a| !a.is_empty()));
                    if !r.attachments.is_empty() {
                        m.attachments.set(r.attachments);
                    }
                    if let (Some(p), Some(c)) = (r.prompt_tokens, r.completion_tokens) {
                        m.tokens.set(Some((p, c)));
                    }
                    // Replay a stored agentic turn's tool calls as full cards,
                    // identical to the live ones. A late result's record is
                    // its synthetic call, which its own card stands for.
                    if let Some(ir) = r.ir_messages.as_ref().filter(|_| r.role != "tool") {
                        let cards = chat_approvals::cards_of(ir, &r.pending_approvals);
                        if !cards.is_empty() {
                            m.tools.set(cards);
                        }
                    }
                    m
                })
                .collect()
        });
        mapped.unwrap_or_default()
    };
    let load_msgs = move |rows: Vec<MsgRow>| msgs.set(msgs_of_rows(rows));

    // What an open read, shown: the open itself and what decides whether it
    // lands are `open::Opener`'s.
    let show = Callback::new(move |l: Landed| {
        let Landed {
            id,
            detail: d,
            left,
            keep_draft,
        } = l;
        // A reply still streaming into the left chat is stopped first.
        if let Some(left) = left {
            if streaming.get_untracked() == Some(left) {
                if let Some(a) = aborter.get_value() {
                    a.abort();
                }
            }
            chat_temp::discard(http, left);
            temp_threads.update(|v| v.retain(|t| t.id != left));
        }
        model_sel.set(d.thread.model_alias.clone());
        chat_tasks::take(thread_tasks, id, d.tasks);
        current.set(Some(d.thread));
        own.bump();
        // A settled reply's numbers belong to the visit; a reply still
        // streaming keeps its own until it is done.
        let keep = streaming.get_untracked();
        stats.update(|s| {
            if s.as_ref().is_some_and(|(t, _)| Some(*t) != keep) {
                *s = None;
            }
        });
        load_msgs(d.messages);
        // Back on the thread whose reply is streaming: the server stores the
        // reply when it is done, so until then the live message is the only
        // copy of it.
        if keep == Some(id) {
            if let Some((_, m)) = live.get_value().filter(|(t, _)| *t == id) {
                msgs.update(|v| v.push(m));
            }
        }
        // Reseed the composer's chips from this thread's own drafts —
        // signals belong to the component (see `owner`).
        if !keep_draft {
            let chips = in_owner(owner, || {
                d.draft_attachments
                    .into_iter()
                    .map(|a| draft_from_attachment(alloc_key(), a))
                    .collect()
            });
            draft_attachments.set(chips.unwrap_or_default());
            draft_kbs.set(Vec::new());
        }
        place.show_thread(Some(id));
    });
    // Deep link: the host's address (the dashboard's `/chat?t=<id>`) opens
    // that thread on a cold load, otherwise the first one in the list; a
    // message in it (`?m=`) is revealed. Read once: the address is rewritten
    // on every switch.
    let seed = opened.thread;
    focus.set(opened.message);
    let opener = Opener::new(OpenEnv {
        current,
        msgs,
        stats,
        threads,
        model_sel,
        focus,
        rescue,
        own,
        list,
        seed,
        scope,
        toasts,
        show,
        http,
        place,
        refresh: Callback::new(move |()| refresh_threads()),
    });
    opener.mount();
    let retry_threads = Callback::new(move |()| opener.retry());

    // The open thread deleted elsewhere: a draft in the composer waits for
    // a new chat made by its Send, never going into another conversation
    // (`chat_sync::Gone`).
    let gone = chat_sync::Gone {
        current,
        msgs,
        stats,
        composer,
        chips: draft_attachments,
        kbs: draft_kbs,
        folders: folder_list,
        model_sel,
        rescue,
        own,
        toasts,
        place,
        fall_through: Callback::new(move |id| opener.fall_through(id, opener.now())),
    };
    let delete_thread = move |id: i64| {
        // The fall-through after it yields to an open started since (review
        // CF-4).
        let since = opener.now();
        deleting.set_value(Some(id));
        scope.spawn(async move {
            if let Err(e) = http
                .post::<Ack, _>(format!("/chat/api/threads/{id}/delete"), &json!({}))
                .await
            {
                deleting.try_set_value(None);
                toasts.err(format!("deleting the conversation failed: {e}"));
                return;
            }
            let Some((ticket, read)) = list.fetch().await else {
                return;
            };
            match read {
                Ok(r) => {
                    if list.latest.is(ticket) {
                        list.take(r);
                    }
                }
                Err(e) => {
                    // A failed re-read keeps the list as it was, less the
                    // deleted row, rather than emptying it (code:A3).
                    threads_state.set(Some(Err(e.to_string())));
                    threads.update(|v| v.retain(|t| t.id != id));
                    temp_threads.update(|v| v.retain(|t| t.id != id));
                }
            }
            deleting.set_value(None);
            // Whether it is still open is read now: the follower may have
            // moved on from it already.
            opener.fall_through(id, since);
        });
    };

    // Pin/Unpin and Archive/Restore from a row's menu, or the open archived
    // thread's Restore. The server's answer is the updated thread: applied
    // to `current` when that is the one touched, then the list is re-read —
    // a pin or an archive both move where (or whether) a thread is listed.
    let pin_thread = move |id: i64, pinned: bool| {
        scope.spawn(async move {
            match post_thread(
                http,
                format!("/chat/api/threads/{id}/pin"),
                &json!({ "pinned": pinned }),
            )
            .await
            {
                Ok(updated) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            *t = updated;
                        }
                    });
                    refresh_threads();
                }
                Err(e) => toasts.err(format!(
                    "{} failed: {e}",
                    if pinned { "pin" } else { "unpin" }
                )),
            }
        });
    };
    // Rename from a row's menu or F2: the server's answer is the thread, as
    // for pin; its refusal (an empty title) is shown as the other actions'.
    let rename_thread = move |id: i64, title: String| {
        scope.spawn(async move {
            match post_thread(
                http,
                format!("/chat/api/threads/{id}/title"),
                &TitleRequest { title },
            )
            .await
            {
                Ok(updated) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            *t = updated;
                        }
                    });
                    refresh_threads();
                }
                Err(e) => toasts.err(format!("rename failed: {e}")),
            }
        });
    };
    let archive_thread = move |id: i64, archived: bool| {
        scope.spawn(async move {
            match post_thread(
                http,
                format!("/chat/api/threads/{id}/archive"),
                &json!({ "archived": archived }),
            )
            .await
            {
                Ok(updated) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            *t = updated;
                        }
                    });
                    refresh_threads();
                }
                Err(e) => toasts.err(format!(
                    "{} failed: {e}",
                    if archived { "archive" } else { "restore" }
                )),
            }
        });
    };

    // A settings patch for thread `id` (`patch_thread`: the open one). The
    // thread is updated when the server has taken it, not before, and only
    // while it is the open one; the list is re-read: a settings write moves
    // the thread to the top and may change its model label and what the
    // model filter finds (code:A4). A refusal is the caller's to say.
    let save_settings = move |id: i64, body: Value| async move {
        let answer = http
            .post::<SettingsAck, _>(format!("/chat/api/threads/{id}/settings"), &body)
            .await?;
        if body.get("model_alias").is_none() {
            toasts.ok("thread settings saved");
        }
        // `try_`: a no-op once the page is gone.
        current.try_update(|c| {
            if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                apply_settings(t, &body);
                // The new model or reasoning may change whether the last
                // reply can be continued.
                t.cont = Some(answer.continue_state.clone());
                // The voice as the server took it, and what it resolves to
                // now.
                crate::chat_voice::apply_answer(t, &answer);
            }
        });
        if scope.alive() {
            refresh_threads();
        }
        // The drafts' blockers depend on the model: re-read them.
        if body.get("model_alias").is_some() && scope.alive() {
            refresh_drafts(http, draft_attachments, scope, id, move |t| {
                current.with_untracked(|c| c.as_ref().map(|x| x.id) == Some(t))
            });
        }
        Ok::<(), lmgw_ui_kit::http::Error>(())
    };
    // The same, saying a refusal. Whether the server took it.
    let post_settings = move |id: i64, body: Value| async move {
        let saved = save_settings(id, body).await;
        if let Err(e) = &saved {
            toasts.err(format!("saving the thread settings failed: {e}"));
        }
        saved.is_ok()
    };
    let patch_thread = move |body: Value| {
        if let Some(id) = current_id_now(current) {
            spawn_local(async move {
                post_settings(id, body).await;
            });
        }
    };
    // The picker's picks, saved one at a time in the order made, so the last
    // one is stored (`ModelPicks`, review CL-5).
    let picks = StoredValue::new(ModelPicks::default());
    let pick_model = move |tid: i64, model: String| {
        let Some(first) = picks.try_update_value(|p| p.picked(tid, &model)).flatten() else {
            return;
        };
        spawn_local(async move {
            let mut next = Some(first);
            while let Some((tid, model)) = next {
                post_settings(tid, json!({ "model_alias": model })).await;
                next = picks.try_update_value(ModelPicks::answered).flatten();
            }
        });
    };
    // A model picked for the open thread → persist immediately. Opening a
    // thread sets the picker to that thread's model too, and that is not a
    // pick: the thread is compared as well, so browsing writes nothing (a
    // settings write also bumps the thread to the top of the list). Nor is
    // the picker following the stored model another writer set
    // (`chat_sync`), which `ModelPicks` is told of.
    let current_id = Memo::new(move |_| current.with(|c| c.as_ref().map(|t| t.id)));
    // A profile edited elsewhere shows when another thread opens (the feed
    // carries no profile change; the directory also reads on focus).
    Effect::new(move |prev: Option<()>| {
        current_id.track();
        if prev.is_some() {
            profile_dir.refresh();
        }
    });
    let settings = SettingsDraft::new();
    Effect::new(move |prev: Option<Option<i64>>| {
        let id = current_id.get();
        if prev != Some(id) {
            settings.seed(&current.get_untracked().unwrap_or_default());
        }
        id
    });
    // A seed "New voice" drew is pending until the thread holds it.
    let held_seed = Memo::new(move |_| current.with(|c| c.as_ref().and_then(|t| t.voice.seed)));
    Effect::new(move |_| settings.voice.settle_seed(held_seed.get()));
    let settings_unsaved =
        Memo::new(move |_| current.with(|c| c.as_ref().is_some_and(|t| settings.differs_from(t))));
    use_dirty_guard().watch_page("the thread settings", settings_unsaved.into());
    Effect::new(move |prev: Option<(Option<i64>, String)>| {
        let sel = model_sel.get();
        let id = current_id.get();
        if let Some((prev_id, prev_sel)) = prev {
            if prev_id != id {
                picks.update_value(ModelPicks::left);
            } else if let Some(tid) = id.filter(|_| prev_sel != sel && !sel.is_empty()) {
                pick_model(tid, sel.clone());
            }
        }
        (id, sel)
    });

    let ctx_max = Memo::new(move |_| {
        let sel = model_sel.get();
        catalog.entries.with(|e| {
            e.iter()
                .find(|m| m.id == sel)
                .and_then(|m| m.ctx)
                .map(|c| c as i64)
        })
    });
    // `Some(true)`/unknown both send images; only `Some(false)` gates them —
    // the same reading the gateway's own send-time rule takes (design §2).
    let model_vision = Memo::new(move |_| {
        let sel = model_sel.get();
        catalog
            .entries
            .with(|e| e.iter().find(|m| m.id == sel).and_then(|m| m.vision))
    });
    let vision_no = Signal::derive(move || model_vision.get() == Some(false));
    // The one Send gate: the server's blockers on the drafts (images, PDF
    // modes, audio), with the local vision check as the fallback for a draft
    // it has not answered for yet (chat_attach::select_blockers).
    // What the drafts become on the way, sent all the same (a GPU block's
    // fallback that cannot see): said, never blocking.
    let attach_hints = Signal::derive(move || draft_attachments.with(|v| draft_hints(v)));
    let attach_blockers = Signal::derive(move || {
        draft_attachments.with(|v| draft_blockers(v, vision_no.get(), &model_sel.get()))
    });
    let on_refresh_drafts = Callback::new(move |()| {
        if let Some(tid) = current_id.get_untracked() {
            refresh_drafts(http, draft_attachments, scope, tid, move |t| {
                current_id.get_untracked() == Some(t)
            });
        }
    });

    // Switching the toolbar's Archived toggle re-fetches that list — a
    // stale-list flash of the other view is worse than a beat of "Loading…".
    Effect::new(move |prev: Option<bool>| {
        let va = view_archived.get();
        if prev.is_some() && prev != Some(va) {
            threads_state.set(None);
            refresh_threads();
        }
        va
    });

    // Upload a picked/dropped/pasted file into the open thread's draft chips
    // at once — what is on screen is always what a Send would bind, never a
    // local-only file the server has not seen. The chip's signals are made
    // under the page's owner (see `owner` above), not this closure's.
    // A draft's chip in the composer at once, for the open thread: the
    // thread it goes to, and the chip; none (said why) with no thread open.
    let start_draft = move |name: String, size: Option<i64>| -> Option<(i64, DraftChip)> {
        let Some(t) = current.get_untracked() else {
            if rescue.with_untracked(|r| r.waiting.is_some()) {
                toasts.warn(chat_sync::NO_FILES_YET);
            } else if rescue.with_untracked(Rescue::opening) {
                toasts.warn(chat_sync::OPENING);
            } else {
                toasts.err("open or create a chat first");
            }
            return None;
        };
        let chip = in_owner(owner, || new_draft(alloc_key(), name, size))?;
        draft_attachments.update(|v| v.push(chip.clone()));
        Some((t.id, chip))
    };
    // The chip's failure, kept on it; the toast is where a long one (the
    // accepted-types list) reads in full.
    let draft_failed = move |chip: &DraftChip, e: String| {
        chip.uploading.try_set(false);
        toasts.err(e.clone());
        chip.error.try_set(Some(e));
    };
    // `file` uploaded as a draft of thread `tid`, its chip told how it went.
    let post_draft = move |tid: i64, chip: DraftChip, file: web_sys::File| async move {
        let encoded = js_sys::encode_uri_component(&file.name())
            .as_string()
            .unwrap_or_default();
        let url = format!("/chat/api/threads/{tid}/attachments?name={encoded}");
        match http
            .post_body::<AttachmentMeta>(url, lmgw_ui_kit::http::Body::Blob(file.into()))
            .await
        {
            Ok(a) => chip.uploaded(a),
            Err(e) => draft_failed(&chip, e.to_string()),
        }
    };
    let upload_file = move |file: web_sys::File| {
        if let Some((tid, chip)) = start_draft(file.name(), Some(file.size() as i64)) {
            scope.spawn(post_draft(tid, chip, file));
        }
    };
    let upload_files = move |files: web_sys::FileList| {
        for i in 0..files.length() {
            if let Some(f) = files.get(i) {
                upload_file(f);
            }
        }
    };

    // ✕ on a chip: an uploaded draft is deleted server-side (fire and
    // forget — it is gone from the composer either way); an error chip has
    // nothing on the server to delete.
    let remove_draft = move |chip: DraftChip| {
        draft_attachments.update(|v| v.retain(|c| c.key != chip.key));
        if let Some(id) = chip.id.get_untracked() {
            spawn_local(async move {
                if let Err(e) = http
                    .post::<lmgw_api_types::chat::Ack, _>(
                        format!("/chat/api/attachments/{id}/delete"),
                        &json!({}),
                    )
                    .await
                {
                    toasts.err(format!("removing the attachment failed: {e}"));
                }
            });
        }
    };

    // A sent chip's own click: an image opens full-size, a text file's
    // content is fetched (it is served as plain bytes, not JSON) and shown
    // in a `<pre>`.
    let open_attachment = Callback::new(move |a: AttachmentMeta| {
        let gen = viewer_gen.get_value() + 1;
        viewer_gen.set_value(gen);
        viewer_name.set(a.name.clone());
        viewer_is_image.set(a.kind == "image");
        viewer_att.set(Some(a.clone()));
        viewer_open.set(true);
        if a.kind == "image" {
            viewer_src.set(crate::chat_bytes::attachment_path(a.id));
            return;
        }
        viewer_src.set(String::new());
        viewer_loading.set(true);
        scope.spawn(async move {
            let text = match http
                .get_text(format!("/chat/api/attachments/{}/text", a.id))
                .await
            {
                Ok(t) => t,
                Err(e) => format!("failed to load: {e}"),
            };
            // A later open (of this attachment or another) may have already
            // moved the viewer on — only the fetch that is still current
            // gets to write `viewer_src`.
            if viewer_gen.get_value() == gen {
                viewer_src.set(text);
                viewer_loading.set(false);
            }
        });
    });

    // What a produced picture's or recording's card reaches of the page
    // (`chat_media`): the open thread (the cancel route names it), the
    // viewer, and Attach — the file fetched back and uploaded as a draft of
    // the composer, so the next message carries it to the model.
    // The chip shows at once and the copy goes to the thread open at the
    // click, however long the fetch takes; a second click while it is on
    // its way is ignored (the button is held).
    let attaching = RwSignal::new(Vec::<i64>::new());
    let attach_media = Callback::new(move |a: AttachmentMeta| {
        if attaching.with_untracked(|v| v.contains(&a.id)) {
            return;
        }
        let Some((tid, chip)) = start_draft(a.name.clone(), Some(a.size)) else {
            return;
        };
        attaching.update(|v| v.push(a.id));
        if let Some(el) = composer_ta.get_untracked() {
            let _ = el.focus();
        }
        scope.spawn(async move {
            let path = format!("/chat/api/attachments/{}", a.id);
            match lmgw_ui_kit::http::file_of(http, &path, &a.name, &a.mime).await {
                Ok(file) => post_draft(tid, chip, file).await,
                Err(e) => draft_failed(&chip, format!("attaching {} failed: {e}", a.name)),
            }
            attaching.update(|v| v.retain(|&i| i != a.id));
        });
    });
    let media = chat_media::MediaCtx::new(
        current_id,
        open_attachment,
        attach_media,
        attaching,
        scope,
        toasts,
    );
    chat_media::provide(media);
    on_cleanup(move || media.close_streams());

    // A code block's Preview button (assets/codeblocks.js) fires a window
    // CustomEvent; the modal it opens is ours. The listener lives outside the
    // reactive tree, so it only touches these signals — never context.
    let preview_open = RwSignal::new(false);
    let preview_code = RwSignal::new(String::new());
    let preview_lang = RwSignal::new(String::new());
    let listener = window_event_listener_untyped("lmgw-preview", move |ev| {
        let Ok(detail) = js_sys::Reflect::get(&ev, &JsValue::from_str("detail")) else {
            return;
        };
        let field = |k: &str| {
            js_sys::Reflect::get(&detail, &JsValue::from_str(k))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default()
        };
        preview_code.set(field("code"));
        preview_lang.set(field("lang"));
        preview_open.set(true);
    });
    on_cleanup(move || listener.remove());

    // Every streamed turn — send, regenerate, edit, continue — reads and
    // writes the page through this (chat_turn.rs); the message actions
    // (chat_actions.rs) are built on it.
    let turn_env = TurnEnv {
        msgs,
        current,
        current_id,
        streaming,
        live,
        stats,
        aborter,
        ctx_max,
        toasts,
        scope,
        voice: page_voice,
        own,
        http,
        origin,
    };
    let actions = ActionEnv {
        turn: turn_env,
        owner,
        next_key,
        refresh: Callback::new(move |()| refresh_threads()),
    };
    let msg_ops = MsgOps {
        msgs,
        busy: Signal::derive(move || streaming.get().is_some()),
        cont: Signal::derive(move || current.with(|c| c.as_ref().and_then(|t| t.cont.clone()))),
        regenerate: Callback::new(move |key| actions.regenerate(key)),
        edit: Callback::new(move |r: EditReq| {
            let is_user = msgs.with_untracked(|v| {
                v.iter()
                    .find(|m| m.key == r.key)
                    .is_some_and(|m| m.role == "user")
            });
            if is_user {
                actions.edit_user(r.key, r.text, r.kb_refs, r.done);
            } else {
                actions.edit_assistant(r.key, r.text, r.done);
            }
        }),
        resume: Callback::new(move |()| actions.resume()),
        delete: Callback::new(move |key| actions.delete(key)),
        decide: Callback::new(move |v| actions.decide(v)),
    };

    // Leaving the Chat page discards the open temporary chat too.
    {
        use std::sync::atomic::Ordering::Relaxed;
        let track = open_temp.clone();
        Effect::new(move |_| {
            let id = current
                .with(|c| c.as_ref().filter(|t| t.temporary).map(|t| t.id))
                .unwrap_or(0);
            track.store(id, Relaxed);
        });
        on_cleanup(move || {
            let id = open_temp.load(Relaxed);
            if id < 0 {
                if let Some(a) = aborter.try_get_value().flatten() {
                    a.abort();
                }
                chat_temp::discard(http, id);
            }
        });
    }

    let stop = move |_| {
        if let Some(a) = aborter.get_value() {
            a.abort();
        }
    };

    let folders = FolderEnv::new(
        folder_list,
        scope,
        model_sel,
        current,
        opener,
        Callback::new(move |()| refresh_threads()),
        Callback::new(delete_thread),
        deleting,
    );
    // The Send of a draft's new chat (`chat_sync::gone`): it makes the chat
    // as the folder's own New chat does, opens it with the draft, sets the
    // model picked, then runs the ordinary send below, reached through
    // `send_cb`.
    let send_cb = StoredValue::new(None::<Callback<()>>);
    let send_now = Callback::new(move |()| {
        if let Some(s) = send_cb.get_value() {
            s.run(());
        }
    });
    let send_new = chat_sync::SendNew {
        rescue,
        folders,
        model_sel,
        scope,
        toasts,
        refresh: Callback::new(move |()| refresh_threads()),
        open: Callback::new(move |(id, ticket)| opener.open_rescued(id, ticket)),
    };
    let landing = chat_sync::Landing {
        rescue,
        current,
        model_sel,
        picks,
        scope,
        toasts,
        send: send_now,
    };
    opener.rescued_lands(Callback::new(move |(ticket, o)| {
        chat_sync::rescued_open(landing, save_settings, ticket, o);
    }));
    let send = move |_| {
        // The rescued draft's Send is under way: this one waits for it.
        if making.get_untracked() {
            return;
        }
        if let Some(busy) = streaming.get_untracked() {
            if current_id.get_untracked() != Some(busy) {
                toasts.warn("a reply is still streaming in another conversation");
            }
            return;
        }
        // A dictation is finished or discarded first (chat-voice §5).
        if !page_voice.dictation.before_send() {
            return;
        }
        let text = composer.get_untracked().trim().to_string();
        if draft_attachments.with_untracked(|v| v.iter().any(|c| c.uploading.get_untracked())) {
            toasts.warn("attachments are still uploading");
            return;
        }
        let ready: Vec<DraftChip> = draft_attachments
            .get_untracked()
            .into_iter()
            .filter(|c| c.id.get_untracked().is_some() && c.error.get_untracked().is_none())
            .collect();
        if text.is_empty() && ready.is_empty() {
            return;
        }
        if let Some(why) = attach_blockers.get_untracked().first() {
            // Send is disabled for this already; Enter bypasses that button,
            // so it must say why nothing happened instead of doing nothing
            // (review nit).
            toasts.warn(why.clone());
            return;
        }
        let Some(t) = current.get_untracked() else {
            if rescue.with_untracked(|r| r.waiting.is_some()) {
                chat_sync::send_new(send_new);
            } else if rescue.with_untracked(Rescue::opening) {
                toasts.warn(chat_sync::OPENING);
            } else {
                toasts.err("open or create a chat first");
            }
            return;
        };
        let ids: Vec<i64> = ready.iter().filter_map(|c| c.id.get_untracked()).collect();
        let att_meta: Vec<AttachmentMeta> = ready.iter().map(DraftChip::as_attachment).collect();
        // Kept to restore the composer if the send is refused outright
        // (review finding: a refusal used to silently lose the typed text
        // and the draft chips).
        let restore_text = text.clone();
        let restore_chips = draft_attachments.get_untracked();
        let kb_refs = draft_kbs.get_untracked();
        let restore_kbs = kb_refs.clone();
        // Dictated text goes as a spoken turn (chat-voice §5).
        let spoken = page_voice.dictation.take_mark();
        // Where the host says the user writes from, now (§9.10).
        let written_from = origin.now();
        composer.set(String::new());
        draft_attachments.set(Vec::new());
        draft_kbs.set(Vec::new());
        let Some((user, assistant)) = in_owner(owner, || {
            let u = new_msg(alloc_key(), "user", text.clone());
            u.attachments.set(att_meta);
            u.kb_refs.set(kb_refs.clone());
            u.voice.set(spoken.as_ref().map(|m| m.as_msg_voice()));
            u.origin.set(written_from.clone());
            (u, new_msg(alloc_key(), "assistant", String::new()))
        }) else {
            return;
        };
        let user_key = user.key;
        let assistant_key = assistant.key;
        let tid = t.id;
        let user_db = user.db_id;
        let reply = assistant.clone();
        own.bump();
        msgs.update(|m| {
            m.push(user);
            m.push(assistant);
        });
        scroll_down(true);
        let mut body = if kb_refs.is_empty() {
            json!({ "content": text, "attachments": ids })
        } else {
            json!({ "content": text, "attachments": ids, "kb_refs": kb_refs })
        };
        if let Some(o) = &written_from {
            body["origin"] = json!(o);
        }
        page_voice
            .dictation
            .voice_for_send(spoken.as_ref(), &mut body);
        let turn = Turn {
            tid,
            url: format!("/chat/api/threads/{tid}/send"),
            body,
            target: reply,
            continuing: false,
            what: "send",
            inline: false,
        };
        spawn_local(async move {
            let end = run_turn(turn_env, turn, move |id| user_db.set(Some(id)), || {}).await;
            if end.refused {
                // Refused before any SSE frame arrived: nothing went
                // anywhere, so give back exactly what Send took — the
                // typed text, the chips, and the optimistic bubbles it
                // had provisionally drawn (review finding: a refusal
                // silently lost all three, and left empty user/assistant
                // bubbles in the transcript). A failure once the stream
                // had already started keeps today's behaviour.
                composer.set(restore_text.clone());
                draft_attachments.set(restore_chips.clone());
                draft_kbs.set(restore_kbs.clone());
                page_voice.dictation.restore_mark(spoken.clone());
                msgs.update(|m| m.retain(|msg| msg.key != user_key && msg.key != assistant_key));
            }
            refresh_threads();
            if scope.alive() {
                actions.resync(tid, false);
            }
        });
    };

    send_cb.set_value(Some(Callback::new(move |()| send(()))));
    chat_sync::watch_new_chat(rescue, folder_list, current_id, model_sel, held);

    // Stop and Send follow the thread on screen: rebuilt when that flips,
    // not with every delta.
    let streaming_here = Memo::new(move |_| {
        let s = streaming.get();
        s.is_some() && s == current_id.get()
    });
    let streaming_elsewhere = Memo::new(move |_| {
        let s = streaming.get();
        s.is_some() && s != current_id.get()
    });
    // Voice mode (chat-voice WP9): the realtime panel in the composer's
    // place. It reads the thread back when a session ends (`load`).
    let realtime = chat_voice::provide_realtime(
        page_voice,
        chat_voice::RealtimeParts {
            msgs,
            current,
            owner,
            next_key,
            scope,
            http,
            streaming,
            model_sel,
            draft: settings.voice,
            profile: settings.profile,
            refresh: Callback::new(move |()| refresh_threads()),
            load: Callback::new(load_msgs),
            make: Callback::new(msgs_of_rows),
            realtime_url: host
                .voice
                .clone()
                .map(|v| Callback::new(move |tid| (v.realtime_url)(tid))),
        },
    );
    let voice_mode = page_voice.voice_mode;
    let task_env = TaskEnv {
        tasks: thread_tasks,
        current_id,
        msgs,
        busy: Signal::derive(move || streaming.get().is_some()),
        scope,
        toasts,
        actions,
    };
    // What other writers change shows here without leaving the page.
    chat_sync::follow(Follow {
        list,
        scope,
        current,
        current_id,
        msgs,
        streaming,
        voice_busy: Signal::derive(move || realtime.holds_transcript()),
        own,
        settings,
        settings_unsaved,
        model_sel,
        picks,
        drafts: draft_attachments,
        tasks: thread_tasks,
        make: Callback::new(msgs_of_rows),
        load: Callback::new(load_msgs),
        gone: Callback::new(move |id| gone.run(id)),
        deleting,
        toasts,
    });
    let is_temporary =
        Memo::new(move |_| current.with(|c| c.as_ref().is_some_and(|t| t.temporary)));
    let is_admin =
        Memo::new(move |_| current.with(|c| c.as_ref().is_some_and(|t| t.kind == "admin")));
    let mcp_names = Memo::new(move |_| {
        current.with(|c| {
            c.as_ref()
                .map(|t| {
                    t.mcp_tools
                        .iter()
                        .map(|m| m.server_label.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    // Deep link: the host's address (the dashboard's
    // `/chat?folder=<id>&settings=1`) opens that folder's settings form once
    // the list is loaded — where a client sends the owner to mark its folder
    // as an ongoing conversation, give it a model or its own retention
    // (client-apps design §3.7).
    if let Some(id) = opened.folder_settings {
        let pending = StoredValue::new(true);
        Effect::new(move |_| {
            if !matches!(threads_state.get(), Some(Ok(()))) || !pending.get_value() {
                return;
            }
            pending.set_value(false);
            // Handled once (review F-17): the link's query leaves the
            // address bar, so a reload does not open the form again.
            place.show_thread(current_id.get_untracked());
            if folders.folder(id).is_some() {
                folders.open_settings(id);
            } else {
                toasts.err(format!(
                    "folder {id} from the link is not in the list: it was deleted, or the \
                     link is another gateway's"
                ));
            }
        });
    }
    let query = RwSignal::new(String::new());
    let listed = Memo::new(move |_| {
        let today = today_local();
        let q = query.get();
        let archived = view_archived.get();
        // The temporary group leads the active view only.
        let mut out = if archived {
            Vec::new()
        } else {
            temp_threads
                .with(|t| chat_temp::temp_items(t, &lmgw_ui_kit::widgets::filter_words(&q), &today))
        };
        let collapsed = folders.collapsed();
        out.extend(
            folder_list.with(|f| thread_items(&threads.get(), f, &collapsed, &q, &today, archived)),
        );
        out
    });
    let badge = Signal::derive(move || {
        let shown = listed.with(|l| {
            l.iter()
                .filter(|i| matches!(i, ListItem::Row { .. }))
                .count()
        });
        let temps = if view_archived.get() {
            0
        } else {
            temp_threads.with(Vec::len)
        };
        lmgw_ui_kit::fmt::of(shown, threads.with(Vec::len) + temps)
    });
    // New chat follows the open thread's folder (chat-plane design §9.10):
    // into it as its menu's "New chat here", or an ongoing folder's new
    // current conversation; otherwise unfiled.
    let new_goes = Memo::new(move |_| {
        current.with(|c| folder_list.with(|f| chat_folders::new_chat_goes("chat", c.as_ref(), f)))
    });
    let new_label = Signal::derive(move || {
        folder_list.with(|f| chat_folders::new_chat_label(new_goes.get(), f))
    });
    let on_new = Callback::new(move |kind: &'static str| {
        let goes = current.with_untracked(|c| {
            folder_list.with_untracked(|f| chat_folders::new_chat_goes(kind, c.as_ref(), f))
        });
        match goes {
            chat_folders::NewChatIn::Unfiled => opener.new_thread(kind),
            chat_folders::NewChatIn::Folder(id) => folders.new_chat_in(id),
            chat_folders::NewChatIn::Ongoing(id) => folders.new_conversation(id),
        }
    });
    // The way back to an ongoing folder's current thread, while another
    // thread is open (§9.10).
    let back = Memo::new(move |_| {
        folder_list.with(|f| threads.with(|t| chat_folders::back_to(f, t, current_id.get())))
    });
    let go_back = move |_| {
        if let Some(b) = back.get_untracked() {
            folders.expand(b.folder);
            opener.open(b.thread);
        }
    };
    let on_open = Callback::new(move |id| opener.open(id));
    let on_pick = Callback::new(move |(tid, mid): (i64, Option<i64>)| opener.reveal(tid, mid));
    let on_delete = Callback::new(delete_thread);
    let on_pin = Callback::new(move |(id, pinned): (i64, bool)| pin_thread(id, pinned));
    let on_archive = Callback::new(move |(id, archived): (i64, bool)| archive_thread(id, archived));
    let on_rename = Callback::new(move |(id, title): (i64, String)| rename_thread(id, title));
    let renaming: RwSignal<Option<i64>> = RwSignal::new(None);

    // Whether attachments can go out at all: any still uploading blocks a
    // send outright, and so does any blocker (`attach_blockers`, design §3/§8).
    let uploading_now =
        Signal::derive(move || draft_attachments.with(|v| v.iter().any(|c| c.uploading.get())));
    let can_send = Signal::derive(move || {
        let ready = draft_attachments.with(|v| {
            v.iter()
                .any(|c| c.id.get().is_some() && c.error.get().is_none())
        });
        (!composer.get().trim().is_empty() || ready)
            && !uploading_now.get()
            && attach_blockers.with(Vec::is_empty)
    });
    let send_disabled_reason = Signal::derive(move || {
        if streaming_elsewhere.get() {
            "a reply is still streaming in another conversation — wait for it, or stop it there"
                .to_string()
        } else if let Some(why) = attach_blockers.get().first() {
            why.clone()
        } else if uploading_now.get() {
            "attachments are still uploading".to_string()
        } else {
            String::new()
        }
    });

    // The narrow layout (`narrow`): below its width the thread list and
    // the settings are drawers over the conversation, opened from the head.
    let shell: NodeRef<leptos::html::Div> = NodeRef::new();
    let shell_size = lmgw_ui_kit::element_size::use_element_size(shell);
    let narrow = Memo::new(move |_| narrow::narrow_at(shell_size.get().0));
    let threads_drawer = Drawer {
        active: narrow.into(),
        open: RwSignal::new(false),
    };
    let settings_drawer = Drawer {
        active: narrow.into(),
        open: RwSignal::new(false),
    };
    // A thread opened (a pick, New chat, a search hit) puts the list away.
    Effect::new(move |prev: Option<Option<i64>>| {
        let id = current_id.get();
        if prev.is_some_and(|p| p != id) {
            threads_drawer.open.set(false);
        }
        id
    });

    // The open thread's archive strip, when it is archived: "Archived <date>
    // · deleted <relative>" plus Restore.
    let archive_info = Signal::derive(move || {
        current.with(|c| {
            c.as_ref().and_then(|t| {
                t.archived_at.as_ref().map(|a| {
                    let when = lmgw_ui_kit::fmt::log_time(a).day_label;
                    let purge = t
                        .purge_at
                        .as_deref()
                        .map(purge_text)
                        .unwrap_or_else(|| "kept indefinitely".to_string());
                    (when, purge)
                })
            })
        })
    });

    view! {
        <div class="chat-shell" class:narrow=move || narrow.get() node_ref=shell>
            <SplitPane
                side=Side::Left
                persist="chat.threads"
                label="Conversations"
                badge=badge
                drawer=threads_drawer
                side_view=move || {
                    view! {
                        <ThreadList
                            items=listed
                            state=threads_state
                            retry=retry_threads
                            total=Signal::derive(move || {
                                threads.with(Vec::len) + temp_threads.with(Vec::len)
                            })
                            query=query
                            current=current_id
                            on_open=on_open
                            on_new=on_new
                            new_label=new_label
                            on_delete=on_delete
                            on_pin=on_pin
                            on_archive=on_archive
                            renaming=renaming
                            on_rename=on_rename
                            view_archived=view_archived
                            archived_count=archived_count
                            folders=folders
                            on_pick=on_pick
                        />
                    }
                }
            >
                <Show
                    when=move || current_id.get().is_some() || rescue.with(Rescue::holds_page)
                    fallback=move || {
                        view! {
                            <div class="chat-empty density-airy">
                                <crate::chat_machine::EmptySlot/>
                                <div class="empty">
                                    "Pick a conversation or start a new one."
                                    <button class="btn primary" on:click=move |_| on_new.run("chat")>
                                        "New chat"
                                    </button>
                                    <Show when=move || narrow.get()>
                                        <narrow::DrawerButton
                                            drawer=threads_drawer
                                            other=settings_drawer
                                            label="Conversations"
                                            icon=narrow::ICON_LIST
                                            badge=badge
                                            class="chat-empty-list"
                                        />
                                    </Show>
                                </div>
                            </div>
                        }
                    }
                >
                    // Collapsed by default: the settings take the transcript's
                    // width, never its height, so the composer stays the last
                    // row whatever the panel holds.
                    <SplitPane
                        side=Side::Right
                        persist="chat.settings"
                        label="Thread settings"
                        default_open=false
                        width=(280, 30, 420)
                        auto_collapse_below=480
                        drawer=settings_drawer
                        side_view=move || {
                            // One form per thread: switching threads starts
                            // from that thread's own settings.
                            move || {
                                current_id
                                    .get()
                                    .map(|_| {
                                        view! {
                                            <ThreadSettings
                                                thread=current
                                                draft=settings
                                                unsaved=settings_unsaved
                                                on_save=patch_thread
                                                is_admin=is_admin.get_untracked()
                                                current_of=Signal::derive(move || {
                                                    let id = current
                                                        .with(|t| t.as_ref().map(|t| t.id))?;
                                                    folders
                                                        .folders
                                                        .with(|f| chat_folders::current_of(f, id))
                                                })
                                            />
                                        }
                                    })
                            }
                        }
                    >
                        <div
                            class="chat-main density-airy"
                            class:drag-over=move || drag_over.get()
                            on:dragover=move |ev| {
                                // Only claim the drop when it is files: text
                                // (a selection, a link) must fall through to
                                // the browser's normal drop-into-a-field
                                // handling instead of being swallowed.
                                if ev.data_transfer().is_some_and(|dt| drag_has_files(&dt)) {
                                    ev.prevent_default();
                                    drag_over.set(true);
                                }
                            }
                            on:dragleave=move |ev| {
                                ev.prevent_default();
                                drag_over.set(false);
                            }
                            on:drop=move |ev| {
                                let Some(dt) = ev.data_transfer() else { return };
                                if !drag_has_files(&dt) {
                                    return;
                                }
                                ev.prevent_default();
                                drag_over.set(false);
                                if let Some(files) = dt.files() {
                                    upload_files(files);
                                }
                            }
                        >
                            <div class="chat-head">
                                <Show when=move || narrow.get()>
                                    <narrow::DrawerButton
                                        drawer=threads_drawer
                                        other=settings_drawer
                                        label="Conversations"
                                        icon=narrow::ICON_LIST
                                        badge=badge
                                    />
                                    <narrow::NewChatButton
                                        label=new_label
                                        on_new=Callback::new(move |()| on_new.run("chat"))
                                    />
                                </Show>
                                <ModelPicker
                                    value=model_sel
                                    tasks=&["chat"]
                                    recent_key="chat"
                                    disabled=picker_held
                                    empty_hint=move || {
                                        // The dashboard's pages, where the host has them.
                                        let (models, providers) = (place.href("/models"), place.href("/upstreams"));
                                        let both = models.is_some() && providers.is_some();
                                        view! {
                                            {models.map(|h| view! { <a href=h>"Add a model"</a> })}
                                            {both.then_some(" · ")}
                                            {providers.map(|h| view! { <a href=h>"Add a cloud provider"</a> })}
                                        }
                                    }
                                />
                                <Show when=move || held.with(Option::is_some)>
                                    <span class="type-badge held-badge" title=chat_sync::HELD>
                                        "folder's model"
                                    </span>
                                </Show>
                                <Show when=move || is_admin.get()>
                                    <span class="type-badge" title="self-admin tools attached">
                                        "admin"
                                    </span>
                                </Show>
                                {move || {
                                    current
                                        .with(|c| c.as_ref().and_then(reasoning_badge))
                                        .map(|b| {
                                            view! {
                                                <span
                                                    class="type-badge"
                                                    title="This thread's reasoning overrides — Thread settings → Reasoning"
                                                >
                                                    {b}
                                                </span>
                                            }
                                        })
                                }}
                                {move || {
                                    let names = mcp_names.get();
                                    (!names.is_empty())
                                        .then(|| {
                                            view! {
                                                <span
                                                    class="type-badge"
                                                    title=format!(
                                                        "MCP servers attached: {}",
                                                        names.join(", "),
                                                    )
                                                >
                                                    {format!("{} mcp", names.len())}
                                                </span>
                                            }
                                        })
                                }}
                                {move || {
                                    current
                                        .with(|c| c.as_ref().and_then(kb_badge))
                                        .map(|b| {
                                            view! {
                                                <span
                                                    class="type-badge"
                                                    title="Knowledge bases on this thread — Thread settings → Knowledge"
                                                >
                                                    {b}
                                                </span>
                                            }
                                        })
                                }}
                                <crate::chat_profiles::ProfileBadge current=current/>
                                <crate::chat_live::FeedStopped/>
                                {move || {
                                    back.get()
                                        .map(|b| {
                                            view! {
                                                <narrow::BackButton name=b.name on_back=go_back/>
                                            }
                                        })
                                }}
                                <Show when=move || current_id.get().is_some()>
                                    <crate::chat_export::ExportMenu thread=current_id/>
                                </Show>
                                <Show when=move || narrow.get()>
                                    <narrow::DrawerButton
                                        drawer=settings_drawer
                                        other=threads_drawer
                                        label="Thread settings"
                                        icon=narrow::ICON_SETTINGS
                                        class="chat-settings-btn"
                                    />
                                </Show>
                            </div>
                            <crate::chat_machine::BannerSlot/>
                            <div class="chat-scroll" id="chat-scroll" node_ref=scroll_pane>
                                <div class="chat-col" node_ref=scroll_col>
                                    <For each=move || msgs.get() key=|m| m.key let:m>
                                        <MsgView m=m vision_no=vision_no on_open=open_attachment ops=msg_ops/>
                                    </For>
                                </div>
                            </div>
                            <StatsRow stats=Signal::derive(move || {
                                let here = current_id.get();
                                stats
                                    .get()
                                    .filter(|(t, _)| Some(*t) == here)
                                    .map(|(_, s)| s)
                            })/>
                            <Show when=move || is_temporary.get()>
                                <TempBanner
                                    thread=current_id
                                    streaming=streaming_here
                                    voice_mode=voice_mode
                                    voice_closing=realtime.closing
                                    busy=keep_busy
                                    on_keep=Callback::new(move |()| {
                                        if let Some(id) = current_id.get_untracked() {
                                            // The kept chat opens unless the
                                            // owner did something meanwhile
                                            // (review CF-4).
                                            let since = opener.choose();
                                            chat_temp::keep(
                                                http,
                                                id,
                                                current,
                                                keep_busy,
                                                scope,
                                                toasts,
                                                Callback::new(move |kept| {
                                                    opener.follow(since, kept);
                                                }),
                                                Callback::new(move |()| refresh_threads()),
                                            );
                                        }
                                    })
                                />
                            </Show>
                            {move || {
                                archive_info
                                    .get()
                                    .map(|(when, purge)| {
                                        view! {
                                            <div class="archive-strip">
                                                <span>{format!("Archived {when} · {purge}")}</span>
                                                <button
                                                    class="btn ghost sm"
                                                    on:click=move |_| {
                                                        if let Some(id) = current_id.get_untracked() {
                                                            archive_thread(id, false);
                                                        }
                                                    }
                                                >
                                                    "Restore"
                                                </button>
                                            </div>
                                        }
                                    })
                            }}
                            <chat_sync::NewChatStrip rescue=rescue folders=folder_list held=held/>
                            <chat_tasks::TaskStrip env=task_env said=answer_said/>
                            // The composer and what belongs to it, hidden (never
                            // dropped: its text, chips and dictation mark wait)
                            // while voice mode has its place.
                            <div class="composer-area" style:display=move || if voice_mode.get() { "none" } else { "contents" }>
                            // Notes about the draft sit above the bubble: they
                            // are messages, not something to type into.
                            <BlockerNote blockers=attach_blockers/>
                            <HintNote hints=attach_hints/>
                            <chat_voice::VoiceStatusLine pv=page_voice/>
                            <div class="composer-wrap">
                            // One bubble holds what the message is made of:
                            // its chips, the text, and a toolbar row under it,
                            // so the text always has the bubble's full width.
                            <div class="composer" node_ref=composer_box>
                                <KbPopover pick=kb_pick anchor=composer_box/>
                                <input
                                    type="file"
                                    multiple
                                    style="display:none"
                                    node_ref=file_input
                                    on:change=move |ev| {
                                        let el: web_sys::HtmlInputElement = event_target(&ev);
                                        if let Some(files) = el.files() {
                                            upload_files(files);
                                        }
                                        el.set_value("");
                                    }
                                />
                                <DraftChips
                                    chips=draft_attachments
                                    vision_no=vision_no
                                    on_remove=Callback::new(remove_draft)
                                    on_open=open_attachment
                                    on_refresh=on_refresh_drafts
                                />
                                <KbDraftChips chips=draft_kbs/>
                                // The empty box's hint is an element over it
                                // (composer.rs), not a placeholder.
                                <div class="composer-field">
                                <textarea
                                    class="input ta composer-input"
                                    rows="1"
                                    node_ref=composer_ta
                                    aria-label="Message"
                                    title=move || {
                                        chat_voice::composer_title(Some(page_voice))()
                                            .unwrap_or_else(|| composer::KEYS.to_owned())
                                    }
                                    prop:value=move || composer.get()
                                    on:input=move |ev| {
                                        kb_pick.on_input(&ev);
                                        // Now rather than a frame later: a new
                                        // line shows without a scroll jump.
                                        composer::fit(&event_target(&ev));
                                    }
                                    on:click=move |ev| kb_pick.on_caret(&ev)
                                    on:keyup=move |ev| kb_pick.on_caret(&ev)
                                    on:keydown=move |ev| {
                                        if kb_pick.on_key(&ev) {
                                            return;
                                        }
                                        if ev.key() == "Enter" && !ev.shift_key() {
                                            ev.prevent_default();
                                            send(());
                                        }
                                    }
                                    on:paste=move |ev| {
                                        if let Some(files) = ev.clipboard_data().and_then(|dt| dt.files()) {
                                            if files.length() > 0 {
                                                ev.prevent_default();
                                                upload_files(files);
                                            }
                                        }
                                    }
                                ></textarea>
                                <composer::ComposerHint text=composer/>
                                </div>
                                <div class="composer-bar">
                                <div class="composer-tools">
                                <button
                                    type="button"
                                    class="btn ghost composer-attach"
                                    title="Attach files"
                                    on:click=move |_| {
                                        if let Some(el) = file_input.get_untracked() {
                                            el.click();
                                        }
                                    }
                                >
                                    // Monochrome line icon, like every other
                                    // icon in the app — the paperclip emoji
                                    // rendered in colour otherwise (review nit).
                                    <svg viewBox="0 0 16 16">
                                        <path d="M14.29 7.37l-6.13 6.13a4 4 0 0 1-5.66-5.66l6.13-6.13a2.67 2.67 0 0 1 3.77 3.77l-6.13 6.13a1.33 1.33 0 0 1-1.89-1.89l5.65-5.65"/>
                                    </svg>
                                </button>
                                <KbButton pick=kb_pick/>
                                </div>
                                <div class="composer-actions">
                                <chat_tasks::AnswerButton env=task_env said=answer_said/>
                                <chat_voice::VoiceControls
                                    draft=settings.voice
                                    on_saved=Callback::new(move |()| refresh_threads())
                                />
                                // Send and Stop are one round icon button in one
                                // place, so the hand that sent can stop; their
                                // names are the aria-labels.
                                {move || {
                                    if streaming_here.get() {
                                        view! {
                                            <button
                                                class="btn danger composer-send"
                                                aria-label="Stop"
                                                on:click=stop
                                            >
                                                <svg viewBox="0 0 16 16" aria-hidden="true">
                                                    <rect x="4" y="4" width="8" height="8" rx="1.5"/>
                                                </svg>
                                            </button>
                                        }
                                            .into_any()
                                    } else {
                                        view! {
                                            <button
                                                class="btn primary composer-send"
                                                aria-label="Send"
                                                disabled=move || streaming_elsewhere.get() || !can_send.get() || making.get()
                                                title=move || send_disabled_reason.get()
                                                on:click=move |_| send(())
                                            >
                                                <svg viewBox="0 0 16 16" aria-hidden="true">
                                                    <path d="M8 13V3.5 M3.75 7.5L8 3.25l4.25 4.25"/>
                                                </svg>
                                            </button>
                                        }
                                            .into_any()
                                    }
                                }}
                                </div>
                                </div>
                            </div>
                            </div>
                            </div>
                            <Show when=move || voice_mode.get()>
                                <chat_voice::RealtimePanel/>
                            </Show>
                        </div>
                    </SplitPane>
                </Show>
            </SplitPane>

            // HTML/SVG preview — the fenced block rendered in the frame the
            // host hands the page (the dashboard's: a sandboxed `srcdoc`,
            // `chat_preview`), closed by Esc, the backdrop, or ✕ (the Modal
            // widget).
            <Modal open=preview_open title="Preview" size=ModalSize::Full fill=true>
                <span class="preview-lang">
                    {move || {
                        let l = preview_lang.get();
                        if l.is_empty() { "html".to_string() } else { l }
                    }}
                </span>
                // Mounted only while the modal is open: an <iframe> created
                // inside a closed <dialog> never gets a browsing context, and
                // writing srcdoc later does not revive it (Chromium/WebKit) —
                // it would render a permanently blank frame.
                <Show when=move || preview_open.get()>
                    <crate::chat_preview::PreviewPane code=preview_code lang=preview_lang/>
                </Show>
            </Modal>

            // A sent chip's own click: an image full-size, or a text file's
            // fetched content in a `<pre>`.
            <FolderDialogs env=folders/>
            <SourceModal target=kbui.source plane=crate::knowledge_source::SourcePlane::Chat/>
            // `fill`: the body holds one `.fill-pane`, the only scroller (a
            // `<pre>` with its own max-height inside a scrolling body was two).
            <Modal open=viewer_open title="Attachment" size=ModalSize::Wide fill=true>
                <div class="dim mono-sm" style="margin-bottom:8px">{move || viewer_name.get()}</div>
                <ViewerMeta att=viewer_att/>
                {move || {
                    if viewer_is_image.get() {
                        view! {
                            <div class="fill-pane">
                                <crate::chat_bytes::ChatImg
                                    class="attach-modal-img"
                                    path=Signal::derive(move || viewer_src.get())
                                />
                            </div>
                        }
                            .into_any()
                    } else if viewer_loading.get() {
                        view! { <div class="dim">"Loading…"</div> }.into_any()
                    } else {
                        view! { <pre class="preset attach-text fill-pane">{move || viewer_src.get()}</pre> }.into_any()
                    }
                }}
            </Modal>
        </div>
    }
}
