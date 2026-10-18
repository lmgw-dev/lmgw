//! The question builder (labs-store design §2.3): a card per question with
//! its id, its type, its instructions and ↑ ↓ ✕, then the type's own editor.
//! Once the gauge has counted, a question's tokens show on its card against
//! the header's head budget, past which llama.cpp truncates it.

use leptos::prelude::*;

use super::form_state::{new_question, shift, Kind, Q};
use super::gauge::{head_line, Gauge};
use super::page::Lab;
use super::question_choice::ChoiceEditor;
use super::question_noul::NoulEditor;
use super::question_score::ScoreEditor;
use super::set_card::ThresholdLine;
use crate::widgets::Select;

/// The ↑ ↓ ✕ of a row. `at` is its place, `len` how many rows there are.
#[component]
pub(super) fn Moves(
    #[prop(into)] at: Signal<Option<usize>>,
    #[prop(into)] len: Signal<usize>,
    what: &'static str,
    on_move: Callback<isize>,
    on_remove: Callback<()>,
) -> impl IntoView {
    let first = move || at.get() == Some(0);
    let last = move || at.get().is_some_and(|i| i + 1 >= len.get());
    view! {
        <span class="dl-moves">
            <button
                type="button"
                class="btn ghost sm"
                title=format!("Move this {what} up")
                disabled=first
                on:click=move |_| on_move.run(-1)
            >
                "↑"
            </button>
            <button
                type="button"
                class="btn ghost sm"
                title=format!("Move this {what} down")
                disabled=last
                on:click=move |_| on_move.run(1)
            >
                "↓"
            </button>
            <button
                type="button"
                class="btn ghost sm del"
                title=format!("Remove this {what}")
                on:click=move |_| on_remove.run(())
            >
                "✕"
            </button>
        </span>
    }
}

#[component]
pub(super) fn QuestionsCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let n = Memo::new(move |_| f.questions.with(Vec::len));
    let add = move |_| {
        let uids = f.uids;
        f.questions.update(|v| {
            let q = new_question(v, || uids.next());
            v.push(q);
        });
    };
    view! {
        <div class="card dl-questions">
            <div class="dl-card-head">
                <h3 class="lab-h3">"Questions"</h3>
                <span class="count">{move || n.get()}</span>
                <span class="spacer"></span>
                <span class="mini-note dim">"answers come back in this order"</span>
            </div>
            <For each=move || f.questions.get() key=|q| q.uid let:q>
                <QuestionCard lab=lab q=q/>
            </For>
            <div class="row">
                <button type="button" class="btn ghost" on:click=add>
                    "Add question"
                </button>
            </div>
        </div>
    }
}

#[component]
fn QuestionCard(lab: Lab, q: Q) -> impl IntoView {
    let f = lab.form;
    let uid = q.uid;
    let at = Memo::new(move |_| f.questions.with(|v| v.iter().position(|x| x.uid == uid)));
    let len = Memo::new(move |_| f.questions.with(Vec::len));
    let kind = Memo::new(move |_| {
        f.questions
            .with(|v| v.iter().find(|x| x.uid == uid).map(|x| x.kind))
            .unwrap_or(Kind::Choice)
    });
    let id = Memo::new(move |_| {
        f.questions
            .with(|v| v.iter().find(|x| x.uid == uid).map(|x| x.id.clone()))
            .unwrap_or_default()
    });
    let pick = RwSignal::new(q.kind.wire().to_string());
    Effect::new(move |_| {
        let k = Kind::parse(&pick.get());
        if untrack(|| kind.get()) != k {
            f.edit(uid, |x| x.kind = k);
        }
    });
    let kinds = Signal::derive(|| {
        Kind::ALL
            .iter()
            .map(|k| (k.wire().to_string(), k.label().to_string()))
            .collect::<Vec<_>>()
    });
    let tokens = move || {
        let Gauge::Counted(c) = lab.gauge.get() else {
            return None;
        };
        // Only the head model's count stands beside the head model's budget.
        let m = lab.model().filter(|m| m.name == c.model)?;
        let qid = id.get();
        let t = c.questions.iter().find(|t| t.id == qid)?.tokens;
        let budget = m.facts.and_then(|f| f.max_head_tokens);
        let (text, over) = head_line(t, budget);
        Some(view! {
            <span
                class="mini-note dl-qtokens"
                class:over=over
                title="Its instructions and its options, levels or criteria, counted on the head model. Past the header's head budget llama.cpp truncates the question and its options."
            >
                {text}
            </span>
        })
    };
    view! {
        <div class="dl-q">
            <div class="dl-q-head">
                <span class="dl-qn">{move || at.get().map(|i| i + 1).unwrap_or_default()}</span>
                <input
                    class="input mono dl-qid"
                    placeholder="id"
                    title="The key the answer comes back under"
                    prop:value=q.id.clone()
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        f.edit(uid, |x| x.id = v);
                    }
                />
                <span class="dl-qkind">
                    <Select value=pick options=kinds/>
                </span>
                <Moves
                    at=at
                    len=len
                    what="question"
                    on_move=Callback::new(move |by: isize| {
                        f.questions
                            .update(|v| {
                                if let Some(i) = v.iter().position(|x| x.uid == uid) {
                                    shift(v, i, by);
                                }
                            })
                    })
                    on_remove=Callback::new(move |()| {
                        f.questions.update(|v| v.retain(|x| x.uid != uid))
                    })
                />
            </div>
            <textarea
                class="input ta dl-instr"
                placeholder="Instructions: what to decide"
                prop:value=q.instructions.clone()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    f.edit(uid, |x| x.instructions = v);
                }
            ></textarea>
            {move || match kind.get() {
                Kind::Choice => view! { <ChoiceEditor lab=lab uid=uid/> }.into_any(),
                Kind::Score => view! { <ScoreEditor lab=lab uid=uid/> }.into_any(),
                Kind::Noul => view! { <NoulEditor lab=lab uid=uid/> }.into_any(),
            }}
            {tokens}
            <ThresholdLine lab=lab uid=uid/>
        </div>
    }
}
