//! A `choice` question's options (labs-store design §2.2, §2.3): a row each
//! with its name, an optional description and ↑ ↓ ✕, in the order sent.
//! The count line says what limits them: the wire allows 255, some types
//! fewer, and llama.cpp's 400 names the limit — no table of llama.cpp's
//! types is kept here (Decision 24).

use leptos::prelude::*;
use lmgw_api_types::decision_lab::form::WIRE_MAX_OPTIONS;

use super::form_state::{shift, Opt};
use super::page::Lab;
use super::questions::Moves;

/// The line under the options: (text, over the wire's limit).
pub(super) fn options_line(n: usize) -> (String, bool) {
    (
        format!(
            "{n} {} · the wire allows {WIRE_MAX_OPTIONS}; some types allow fewer, and llama.cpp's 400 names the limit",
            if n == 1 { "option" } else { "options" }
        ),
        n > WIRE_MAX_OPTIONS,
    )
}

#[component]
pub(super) fn ChoiceEditor(lab: Lab, uid: u32) -> impl IntoView {
    let f = lab.form;
    let options = move || {
        f.questions
            .with(|v| v.iter().find(|q| q.uid == uid).map(|q| q.options.clone()))
            .unwrap_or_default()
    };
    let n = Memo::new(move |_| {
        f.questions
            .with(|v| v.iter().find(|q| q.uid == uid).map(|q| q.options.len()))
            .unwrap_or_default()
    });
    let edit = move |ouid: u32, g: Box<dyn FnOnce(&mut Opt)>| {
        f.edit(uid, |q| {
            if let Some(o) = q.options.iter_mut().find(|o| o.uid == ouid) {
                g(o);
            }
        });
    };
    let add = move |_| {
        let ouid = f.uids.next();
        f.edit(uid, |q| {
            q.options.push(Opt {
                uid: ouid,
                name: String::new(),
                description: String::new(),
            })
        });
    };
    view! {
        <div class="dl-rows">
            <For each=options key=|o| o.uid let:o>
                {
                    let ouid = o.uid;
                    let at = Memo::new(move |_| {
                        f.questions
                            .with(|v| {
                                v.iter()
                                    .find(|q| q.uid == uid)
                                    .and_then(|q| q.options.iter().position(|x| x.uid == ouid))
                            })
                    });
                    view! {
                        <div class="dl-opt">
                            <input
                                class="input mono"
                                placeholder="option"
                                prop:value=o.name.clone()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    edit(ouid, Box::new(move |o| o.name = v));
                                }
                            />
                            <input
                                class="input"
                                placeholder="what it means (blank sends null)"
                                prop:value=o.description.clone()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    edit(ouid, Box::new(move |o| o.description = v));
                                }
                            />
                            <Moves
                                at=at
                                len=n
                                what="option"
                                on_move=Callback::new(move |by: isize| {
                                    f.edit(
                                        uid,
                                        |q| {
                                            if let Some(i) = q.options.iter().position(|x| x.uid == ouid) {
                                                shift(&mut q.options, i, by);
                                            }
                                        },
                                    )
                                })
                                on_remove=Callback::new(move |()| {
                                    f.edit(uid, |q| q.options.retain(|x| x.uid != ouid))
                                })
                            />
                        </div>
                    }
                }
            </For>
        </div>
        <div class="dl-rows-foot">
            <button type="button" class="btn ghost sm" on:click=add>
                "Add option"
            </button>
            {move || {
                let (text, over) = options_line(n.get());
                view! {
                    <span class="mini-note dim" class:dl-over=over>
                        {text}
                    </span>
                }
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_is_against_the_wires_255() {
        let (t, over) = options_line(4);
        assert!(t.starts_with("4 options · the wire allows 255"), "{t}");
        assert!(!over);
        assert!(options_line(256).1);
        assert!(options_line(1).0.starts_with("1 option ·"));
    }
}
