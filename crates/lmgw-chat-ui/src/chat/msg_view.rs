//! One message of the transcript: its bubble, actions, tools, citations
//! and attachments, and the stats row under a reply.

use super::*;

#[component]
pub(crate) fn MsgView(
    m: Msg,
    /// Whether the thread's current model is known not to see images — a
    /// history image then carries a note that it will not be resent.
    #[prop(into)]
    vision_no: Signal<bool>,
    on_open: Callback<AttachmentMeta>,
    ops: MsgOps,
) -> impl IntoView {
    // The message opened for editing in place (chat_actions.rs).
    let editing = RwSignal::new(false);
    // Search hits find the message by this (`chat_search::reveal`).
    let db_id = m.db_id;
    let m_actions = m.clone();
    let m_editor = m.clone();
    let msg_key = m.key;
    let content = m.content;
    let reasoning = m.reasoning;
    let tools = m.tools;
    let streaming = m.streaming;
    let tokens = m.tokens;
    let attachments = m.attachments;
    let kb_refs = m.kb_refs;
    let context = m.context;
    let (model, answered_by, unsaved) = (m.model, m.answered_by, m.unsaved);
    let images_note = m.images_note;
    let gpu_setup = m.gpu_setup;
    let voice = m.voice;
    let origin = m.origin;
    let role = m.role.clone();
    // A late MCP task result has a card of its own (`chat_tasks`).
    if role == "tool" {
        return view! { <chat_tasks::TaskResultView m=m ops=ops/> }.into_any();
    }
    let kbui = KbUi::use_ui();
    // A reply's pictures take card actions once it is a stored message.
    let media_settled =
        Signal::derive(move || db_id.get().is_some() && !streaming.get() && !unsaved.get());
    // The rendered markdown is written from an effect rather than through
    // `inner_html`, so highlighting and the toolbar always run on the DOM this
    // pass produced — no ordering race between two effects over one element.
    // Re-runs per streamed token (highlight only) and once more when the
    // message settles, which is when the toolbar and auto-detection happen.
    let md_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    Effect::new(move |_| {
        let Some(el) = md_ref.get() else { return };
        let html = md_to_html(&content.get());
        // `[n]` become badges for the excerpts this answer was given.
        let html = context.with(|c| match c {
            Some(c) if !c.excerpts.is_empty() => cite_html(&html, &c.titles()),
            _ => html,
        });
        let html = voice.with(|v| chat_voice::spoken_html(html, v.as_ref(), &role));
        el.set_inner_html(&html);
        decorate_code(&el, !streaming.get());
    });
    if m.role == "user" {
        let has_images = move || attachments.with(|v| v.iter().any(|a| a.kind == "image"));
        view! {
            <div class="msg-item msg-item-user" data-mid=move || db_id.get().map(|i| i.to_string())>
            <div class="msg-user">
                <Show when=move || !attachments.with(Vec::is_empty)>
                    <div class="chip-row msg-attach-row">
                        <For each=move || attachments.get() key=|a| a.id let:a>
                            <SentChip a=a vision_no=vision_no on_open=on_open/>
                        </For>
                    </div>
                    <Show when=move || has_images() && vision_no.get()>
                        <div class="dim mini-note">
                            "the image(s) above are not sent to this model"
                        </div>
                    </Show>
                </Show>
                <Show when=move || !editing.get() && !kb_refs.with(Vec::is_empty)>
                    <div class="chip-row msg-attach-row kb-chips">
                        <For each=move || kb_refs.get() key=|id| *id let:id>
                            <KbChip id=id/>
                        </For>
                    </div>
                </Show>
                <Show when=move || !editing.get() && !content.with(String::is_empty)>
                    <div class="msg-user-text">{move || content.get()}</div>
                </Show>
                <chat_voice::MicBadge voice=voice/>
                <Show when=move || editing.get()>
                    <MsgEditor m=m_editor.clone() ops=ops editing=editing/>
                </Show>
            </div>
            // Where it was written, as its app said: what the model reads
            // before it (chat-plane design §9.10).
            {move || {
                origin
                    .get()
                    .map(|o| {
                        let title = format!("Where this was written, as the app said; the model reads it before the message: {o}");
                        view! { <div class="mini-note msg-origin" title=title>{o}</div> }
                    })
            }}
            <MsgActions m=m_actions.clone() ops=ops editing=editing/>
            </div>
        }
        .into_any()
    } else {
        view! {
            <div
                class="msg-item msg-assistant"
                class:msg-unsaved=move || unsaved.get()
                data-mid=move || db_id.get().map(|i| i.to_string())
            >
                <RetrievalView ctx=context/>
                <Show when=move || !reasoning.get().is_empty()>
                    <details class="reasoning">
                        <summary class="dim">"thinking"</summary>
                        <div class="reasoning-body">{move || reasoning.get()}</div>
                    </details>
                </Show>
                <For each=move || tools.get() key=|c| c.index let:card>
                    <ToolCardView card=card tools=tools key=msg_key ops=ops/>
                    <chat_media::CallMedia
                        card=card
                        tools=tools
                        files=attachments
                        settled=media_settled
                    />
                </For>
                <chat_media::LooseMedia tools=tools files=attachments settled=media_settled/>
                <ApprovalBatch tools=tools key=msg_key ops=ops/>
                <div
                    class="md"
                    node_ref=md_ref
                    style:display=move || editing.get().then_some("none")
                    on:click=move |ev| open_citation(&ev, context, kbui)
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" || ev.key() == " " {
                            open_citation(&ev, context, kbui);
                        }
                    }
                ></div>
                <crate::chat_machine::GpuSetupCard message=gpu_setup/>
                <Show when=move || editing.get()>
                    <MsgEditor m=m_editor.clone() ops=ops editing=editing/>
                </Show>
                <Show when=move || streaming.get()>
                    <span class="caret"></span>
                </Show>
                <chat_voice::SpokenReply voice=voice/>
                {move || {
                    tokens
                        .get()
                        .map(|(p, c)| {
                            view! {
                                <div class="dim mini-note">{format!("{p} → {c} tokens")}</div>
                            }
                        })
                }}
                {move || {
                    answer_label(model.get().as_deref(), answered_by.get().as_deref())
                        .map(|l| view! { <div class="dim mini-note msg-answered-by">{l}</div> })
                }}
                {move || {
                    images_note
                        .get()
                        .map(|n| view! { <div class="mini-note msg-images-note">{n}</div> })
                }}
                <Show when=move || unsaved.get()>
                    <div class="mini-note msg-unsaved-note">{UNSAVED_NOTE}</div>
                </Show>
                <MsgActions m=m_actions.clone() ops=ops editing=editing/>
            </div>
        }
        .into_any()
    }
}

/// A click (or Enter) on a citation badge: open the excerpt it names.
fn open_citation(ev: &web_sys::Event, context: RwSignal<Option<KbContext>>, ui: KbUi) {
    let Some(n) = cite_target(ev) else { return };
    if let Some(target) = context.with_untracked(|c| c.as_ref().and_then(|c| c.source(n))) {
        ev.prevent_default();
        ui.source.set(Some(target));
    }
}

#[component]
pub(crate) fn StatsRow(#[prop(into)] stats: Signal<Option<Stats>>) -> impl IntoView {
    view! {
        {move || {
            stats
                .get()
                .map(|s| {
                    let tilde = if s.server { "" } else { "~" };
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(v) = s.ttft_ms {
                        parts.push(format!("ttft {:.0} ms", v));
                    }
                    if let Some(v) = s.total_ms {
                        parts.push(format!("total {:.1} s", v / 1000.0));
                    }
                    if let Some(v) = s.tps {
                        parts.push(format!("{tilde}{v:.1} tok/s"));
                    }
                    if let Some(v) = s.prefill {
                        parts.push(format!("prefill {tilde}{v:.0} tok/s"));
                    }
                    if let Some(v) = s.cached {
                        parts.push(format!("{v} cached"));
                    }
                    if let (Some(n), Some(a)) = (s.draft_n, s.draft_accepted) {
                        if n > 0 {
                            parts.push(format!(
                                "spec {:.0}%",
                                a as f64 / n as f64 * 100.0
                            ));
                        }
                    }
                    let ctx = match (s.prompt_tokens, s.completion_tokens, s.ctx_max) {
                        (Some(p), Some(c), Some(max)) if max > 0 => {
                            Some(((p + c) as f64 / max as f64, p + c, max))
                        }
                        _ => None,
                    };
                    let ignored = (!s.ignored.is_empty())
                        .then(|| {
                            let names = s
                                .ignored
                                .iter()
                                .map(|k| if k == "enabled" { "on/off" } else { k.as_str() })
                                .collect::<Vec<_>>()
                                .join(", ");
                            let title = match &s.reasoning_note {
                                Some(note) => format!("{note}. Reasoning off was not applied as asked ({names})."),
                                None => format!(
                                    "The route that answered has no field for this thread's reasoning or sampling override ({names}) — the model never saw it",
                                ),
                            };
                            view! {
                                <span
                                    class="type-badge fallback-badge"
                                    title=title
                                >
                                    {format!("not sent: {names}")}
                                </span>
                            }
                        });
                    view! {
                        <div class="stats-row" class:live=s.live>
                            <span class="mono-sm dim">{parts.join(" · ")}</span>
                            {ignored}
                            {ctx
                                .map(|(frac, used, max)| {
                                    view! {
                                        <span
                                            class="ctx-bar"
                                            title=format!("context: {used} / {max}")
                                        >
                                            <i style=format!(
                                                "width:{}%",
                                                (frac * 100.0).min(100.0),
                                            )></i>
                                        </span>
                                    }
                                })}
                        </div>
                    }
                })
        }}
    }
}
