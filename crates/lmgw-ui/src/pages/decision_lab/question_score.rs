//! A `score` question's levels (labs-store design §2.3): lowest first, a row
//! each with its index, its description and ↑ ↓ ✕; the count against the
//! wire's 2 to 10. The order is the scale, so Perturb never touches it.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::form::SCORE_LEVELS;

use super::form_state::{shift, Level};
use super::page::Lab;
use super::questions::Moves;

/// The line under the levels: (text, outside 2 to 10).
pub(super) fn levels_line(n: usize) -> (String, bool) {
    (
        format!(
            "{n} {} · a score takes {} to {}, lowest first",
            if n == 1 { "level" } else { "levels" },
            SCORE_LEVELS.start(),
            SCORE_LEVELS.end()
        ),
        !SCORE_LEVELS.contains(&n),
    )
}

#[component]
pub(super) fn ScoreEditor(lab: Lab, uid: u32) -> impl IntoView {
    let f = lab.form;
    let levels = move || {
        f.questions
            .with(|v| v.iter().find(|q| q.uid == uid).map(|q| q.levels.clone()))
            .unwrap_or_default()
    };
    let n = Memo::new(move |_| {
        f.questions
            .with(|v| v.iter().find(|q| q.uid == uid).map(|q| q.levels.len()))
            .unwrap_or_default()
    });
    let add = move |_| {
        let luid = f.uids.next();
        f.edit(uid, |q| {
            q.levels.push(Level {
                uid: luid,
                text: String::new(),
            })
        });
    };
    view! {
        <div class="dl-rows">
            <For each=levels key=|l| l.uid let:l>
                {
                    let luid = l.uid;
                    let at = Memo::new(move |_| {
                        f.questions
                            .with(|v| {
                                v.iter()
                                    .find(|q| q.uid == uid)
                                    .and_then(|q| q.levels.iter().position(|x| x.uid == luid))
                            })
                    });
                    view! {
                        <div class="dl-level">
                            <span class="dl-li mono-sm">{move || at.get().unwrap_or_default()}</span>
                            <input
                                class="input"
                                placeholder="what this level means"
                                prop:value=l.text.clone()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    f.edit(
                                        uid,
                                        |q| {
                                            if let Some(x) = q.levels.iter_mut().find(|x| x.uid == luid) {
                                                x.text = v;
                                            }
                                        },
                                    );
                                }
                            />
                            <Moves
                                at=at
                                len=n
                                what="level"
                                on_move=Callback::new(move |by: isize| {
                                    f.edit(
                                        uid,
                                        |q| {
                                            if let Some(i) = q.levels.iter().position(|x| x.uid == luid) {
                                                shift(&mut q.levels, i, by);
                                            }
                                        },
                                    )
                                })
                                on_remove=Callback::new(move |()| {
                                    f.edit(uid, |q| q.levels.retain(|x| x.uid != luid))
                                })
                            />
                        </div>
                    }
                }
            </For>
        </div>
        <div class="dl-rows-foot">
            <button type="button" class="btn ghost sm" on:click=add>
                "Add level"
            </button>
            {move || {
                let (text, off) = levels_line(n.get());
                view! {
                    <span class="mini-note dim" class:dl-over=off>
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
    fn the_count_is_against_two_to_ten() {
        assert_eq!(
            levels_line(4),
            (
                "4 levels · a score takes 2 to 10, lowest first".into(),
                false
            )
        );
        assert!(levels_line(1).1);
        assert!(levels_line(11).1);
    }
}
