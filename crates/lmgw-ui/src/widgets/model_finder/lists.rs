//! "Other models with this name" and "Did you mean" (§4.5): every entry
//! the answer's window holds, each opening its own card, the totals, and
//! "Show more" for the next window or the hub's next page (D142, D70).

use leptos::prelude::*;
use lmgw_api_types::model_find::ModelChoice;

use super::paging::Further;
use super::{words, Finder};
use crate::fmt::grouped;

/// One entry's line after its name: "by Qwen · 27 billion parameters ·
/// 1 204 likes", and the walk's note.
fn detail(c: &ModelChoice) -> String {
    let mut parts = vec![format!("by {}", c.author)];
    if let Some(p) = c.parameters {
        parts.push(words::parameters(p));
    }
    parts.push(match c.likes {
        1 => "1 like".to_string(),
        n => format!("{} likes", grouped(n)),
    });
    if c.gated {
        parts.push("asks for an account".into());
    }
    if c.private {
        parts.push("private to your account".into());
    }
    parts.join(" · ")
}

#[component]
pub(super) fn ChoiceList(f: Finder, others: bool) -> impl IntoView {
    let list = if others { f.others } else { f.dym };
    let title = if others {
        "Other models with this name"
    } else {
        "Did you mean one of these?"
    };
    let shown = move || list.with(|l| !l.items.is_empty());
    let more_label = move || match list.with(|l| l.further()) {
        Further::ShowMore(n) => Some(format!("Show more ({} not shown)", grouped(u64::from(n)))),
        Further::NextPage => Some("Look further on Hugging Face".to_string()),
        Further::None => None,
    };
    // While the hub's limit runs, an entry's card would wait too.
    let waiting = move || f.waiting_secs().is_some();
    view! {
        <Show when=shown>
            <section class="mf-list" aria-label=title>
                <h3 class="mini-head">
                    {title}
                    <span class="count">
                        {move || list.with(|l| {
                            if l.total as usize > l.items.len() {
                                format!("{} of {}", l.items.len(), grouped(u64::from(l.total)))
                            } else {
                                l.items.len().to_string()
                            }
                        })}
                    </span>
                </h3>
                <ul class="mf-choices">
                    <For
                        each=move || list.with(|l| l.items.clone())
                        key=|c| c.origin.clone()
                        let:c
                    >
                        {
                            let origin = c.origin.clone();
                            let note = c.note.clone();
                            view! {
                                <li>
                                    <button
                                        class="mf-choice"
                                        title=format!("Show the card of {}", c.origin)
                                        disabled=waiting
                                        on:click=move |_| f.search(origin.clone())
                                    >
                                        <span class="mf-choice-name">{c.name.clone()}</span>
                                        <span class="mf-choice-detail dim">{detail(&c)}</span>
                                        {note.map(|n| view! { <span class="mf-choice-note">{n}</span> })}
                                    </button>
                                </li>
                            }
                        }
                    </For>
                </ul>
                {move || more_label().map(|l| view! {
                    <button
                        class="btn ghost sm mf-more"
                        disabled=move || f.lists_busy.get() || f.waiting_secs().is_some()
                        title="Asks Hugging Face the same search again for the next entries"
                        on:click=move |_| f.more(others)
                    >
                        {move || if f.lists_busy.get() { "Asking…".to_string() } else { l.clone() }}
                    </button>
                })}
            </section>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_says_who_its_size_and_likes() {
        let c = ModelChoice {
            author: "demo-org".into(),
            parameters: Some(8_000_000_000),
            likes: 1_204,
            gated: true,
            ..ModelChoice::default()
        };
        assert_eq!(
            detail(&c),
            "by demo-org · 8 billion parameters · 1\u{202F}204 likes · asks for an account"
        );
    }
}
