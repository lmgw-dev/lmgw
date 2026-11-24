//! Settings → GPU — each graphics card's headroom (device-ledger design
//! §9.1, WP13): what is kept free there for other programs, in a sentence;
//! "Keep free for other programs: Automatic | Always N GiB | Nothing, this
//! card is lmgw's", which is the draft's `vram.keep_free` and saved with
//! the page; an amount suggested from what was seen there; and forgetting a
//! card's readings (ruling 10: a game run without the hold, say, taught a
//! most the desktop never reaches), "Forget all" beside. Forgetting acts at
//! once, behind a confirmation.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{HeadroomCard, HeadroomNow, KeepFree, KeepFreeMode};
use serde_json::{json, Value};

use super::Page;
use crate::widgets::use_toasts;

/// The draft key the cards' amounts are held under: the whole map, as the
/// server stores it, by card id.
pub(super) const KEY: &str = "vram.keep_free";

const MIB_PER_GIB: f64 = 1024.0;

/// A card by name and index, as every surface names one; never its id.
fn card_label(c: &HeadroomCard) -> String {
    match c.name.is_empty() {
        false => format!("{} (card {})", c.name, c.index),
        true => format!("card {}", c.index),
    }
}

/// The amount card `id` has in the draft map `draft`: `automatic` where it
/// has none.
pub(super) fn keep_of(draft: &Value, id: &str) -> KeepFree {
    draft
        .get(id)
        .and_then(|v| serde_json::from_value::<KeepFree>(v.clone()).ok())
        .unwrap_or_default()
}

/// The draft map `draft` with card `id` set to `keep`. `automatic` takes
/// the card out where the stored map `stored` has no amount for it, so
/// putting a card back is no change; where it has one, the entry says
/// `automatic`, which the server's merge takes as "take it out".
pub(super) fn with_keep(draft: &Value, stored: &Value, id: &str, keep: KeepFree) -> Value {
    let mut map = draft.as_object().cloned().unwrap_or_default();
    match keep.mode {
        KeepFreeMode::Automatic if stored.get(id).is_none() => {
            map.remove(id);
        }
        _ => {
            map.insert(id.to_string(), json!(keep));
        }
    }
    Value::Object(map)
}

/// An amount in MiB as the GiB box shows it: one decimal.
pub(super) fn gib_text(mb: u64) -> String {
    format!("{:.1}", mb as f64 / MIB_PER_GIB)
}

/// The GiB box's text as MiB: `None` for what is no amount above 0.
pub(super) fn mb_of(text: &str) -> Option<u64> {
    let gib: f64 = text.trim().replace(',', ".").parse().ok()?;
    let mb = (gib * MIB_PER_GIB).round();
    (gib.is_finite() && mb >= 1.0).then_some(mb as u64)
}

const MODES: [(&str, &str); 3] = [
    ("automatic", "Automatic"),
    ("always", "Always"),
    ("nothing", "Nothing, this card is lmgw's"),
];

fn mode_value(m: KeepFreeMode) -> &'static str {
    match m {
        KeepFreeMode::Automatic => "automatic",
        KeepFreeMode::Always => "always",
        KeepFreeMode::Nothing => "nothing",
    }
}

fn mode_of(v: &str) -> KeepFreeMode {
    match v {
        "always" => KeepFreeMode::Always,
        "nothing" => KeepFreeMode::Nothing,
        _ => KeepFreeMode::Automatic,
    }
}

/// Each card's headroom, its amount and its Forget, and Forget all.
#[component]
pub(super) fn HeadroomCards(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let form = page.form;
    // The card being confirmed: `Some(None)` every card's, `Some(Some(id))`
    // one card's.
    let confirming = RwSignal::new(None::<Option<String>>);
    let busy = RwSignal::new(false);
    let now = move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| v.get("headroom_now"))
                .and_then(|v| serde_json::from_value::<HeadroomNow>(v.clone()).ok())
        })
    };
    let cards = move || now().map(|n| n.cards).unwrap_or_default();
    let several = move || cards().len() > 1;
    let any_readings = move || cards().iter().any(|c| c.most_bytes.is_some());
    let stored = move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| v.pointer("/vram/keep_free"))
                .cloned()
                .unwrap_or(Value::Null)
        })
    };
    let set_keep = move |card: String, keep: KeepFree| {
        let next = with_keep(&form.value(KEY), &stored(), &card, keep);
        form.set_value(KEY, next);
    };
    let forget = move |card: Option<String>| {
        busy.set(true);
        spawn_local(async move {
            let args = match &card {
                Some(c) => json!({ "card": c }),
                None => json!({}),
            };
            let res = crate::api::post::<HeadroomNow, _>("/api/op/headroom_forget", &args).await;
            busy.set(false);
            confirming.set(None);
            match res {
                Ok(now) => {
                    toasts.ok(match card {
                        Some(_) => "That card's readings forgotten".to_string(),
                        None => "Readings forgotten".to_string(),
                    });
                    page.data.update(|d| {
                        if let Some(d) = d.as_mut() {
                            d["headroom_now"] = json!(now);
                        }
                    });
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let warning = move |card: &Option<String>| -> String {
        match card {
            Some(c) => {
                let label = cards()
                    .iter()
                    .find(|x| x.id.as_str() == c)
                    .map(card_label)
                    .unwrap_or_else(|| "this card".into());
                format!(
                    "The readings of other programs' GPU memory on {label} are deleted. Until \
                     its next reading that card keeps the whole fixed floor free and no learned \
                     room; the other cards keep theirs."
                )
            }
            None => "Every card's readings of other programs' GPU memory are deleted. Until \
                 their next readings the cards keep the whole fixed floor free and no learned \
                 room."
                .to_string(),
        }
    };
    let line = move |c: HeadroomCard| {
        let card_id = StoredValue::new(c.id.as_str().to_string());
        let none = c.most_bytes.is_none();
        let keep = Memo::new(move |_| card_id.with_value(|id| keep_of(&form.value(KEY), id)));
        let mode = RwSignal::new(mode_value(keep.get_untracked().mode).to_string());
        Effect::new(move |_| {
            let m = mode_value(keep.get().mode);
            if mode.get_untracked() != m {
                mode.set(m.to_string());
            }
        });
        Effect::new(move |_| {
            let m = mode_of(&mode.get());
            let k = keep.get_untracked();
            if m != k.mode {
                let mb = match m {
                    // An amount to start from: the one set before, else
                    // 1 GiB.
                    KeepFreeMode::Always if k.mb > 0 => k.mb,
                    KeepFreeMode::Always => 1024,
                    _ => 0,
                };
                set_keep(card_id.get_value(), KeepFree { mode: m, mb });
            }
        });
        let amount = RwSignal::new(None::<String>);
        let options = Signal::derive(|| {
            MODES
                .iter()
                .map(|(v, l)| (v.to_string(), l.to_string()))
                .collect::<Vec<_>>()
        });
        let suggest = c.suggest.clone();
        let label = card_label(&c);
        let aria = format!("Amount kept free on {label}, in GiB");
        view! {
            <div class="set-blocks">
                <div class="set-line">
                    <span class="set-line-label">{label}</span>
                    <span>{c.sentence.clone()}</span>
                </div>
                <div class="set-line">
                    <span>"Keep free for other programs"</span>
                    <crate::widgets::Select value=mode options=options/>
                    {move || {
                        (keep.get().mode == KeepFreeMode::Always)
                            .then(|| {
                                view! {
                                    <div class="spin">
                                        <input
                                            class="spin-field"
                                            class:invalid=move || {
                                                amount.get().is_some_and(|t| mb_of(&t).is_none())
                                            }
                                            inputmode="decimal"
                                            spellcheck="false"
                                            aria-label=aria.clone()
                                            prop:value=move || {
                                                amount.get().unwrap_or_else(|| gib_text(keep.get().mb))
                                            }
                                            on:input=move |ev| {
                                                let t = event_target_value(&ev);
                                                if let Some(mb) = mb_of(&t) {
                                                    set_keep(
                                                        card_id.get_value(),
                                                        KeepFree { mode: KeepFreeMode::Always, mb },
                                                    );
                                                }
                                                amount.set(Some(t));
                                            }
                                            on:change=move |_| amount.set(None)
                                        />
                                        <span class="spin-unit">"GiB"</span>
                                    </div>
                                }
                            })
                    }}
                    <button
                        class="btn"
                        disabled=move || none || busy.get()
                        on:click=move |_| confirming.set(Some(Some(card_id.get_value())))
                    >
                        "Forget…"
                    </button>
                </div>
                {suggest
                    .map(|s| {
                        let mb = s.mb;
                        view! {
                            <div class="set-line">
                                <span class="dim">{s.sentence.clone()}</span>
                                <button
                                    class="btn"
                                    disabled=move || {
                                        keep.get() == KeepFree { mode: KeepFreeMode::Always, mb }
                                    }
                                    on:click=move |_| {
                                        amount.set(None);
                                        set_keep(
                                            card_id.get_value(),
                                            KeepFree { mode: KeepFreeMode::Always, mb },
                                        );
                                    }
                                >
                                    {format!("Always keep {} GiB free", gib_text(mb))}
                                </button>
                            </div>
                        }
                    })}
            </div>
        }
    };
    view! {
        <div class="prow set-row stack" id=id hidden=move || hidden.get()>
            <div class="prow-text">
                <span class="prow-title">"Each card"</span>
                <span class="prow-sub">
                    "What is kept free for other programs on each graphics card, and what it \
                     rests on: Automatic follows the headroom above, Always is kept without \
                     exception, Nothing gives the card to lmgw. Saved with the page"
                </span>
            </div>
            <div class="prow-end">
                <Show
                    when=move || confirming.get().is_some()
                    fallback=move || {
                        view! {
                            <For
                                each=cards
                                key=|c| format!("{}|{}|{:?}", c.id, c.sentence, c.suggest)
                                children=line
                            />
                            <button
                                class="btn destructive"
                                disabled=move || !any_readings() || busy.get()
                                on:click=move |_| confirming.set(Some(None))
                            >
                                {move || match several() {
                                    true => "Forget all readings…",
                                    false => "Forget the readings…",
                                }}
                            </button>
                        }
                    }
                >
                    <div class="set-confirm">
                        <div class="notice warn">
                            {move || confirming.get().map(|c| warning(&c)).unwrap_or_default()}
                        </div>
                        <div class="row set-confirm-acts">
                            <button class="btn" on:click=move |_| confirming.set(None)>
                                "Cancel"
                            </button>
                            <button
                                class="btn destructive"
                                disabled=move || busy.get()
                                on:click=move |_| {
                                    if let Some(card) = confirming.get_untracked() {
                                        forget(card);
                                    }
                                }
                            >
                                "Forget"
                            </button>
                        </div>
                    </div>
                </Show>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card set to an amount, back to automatic: out of the map where
    /// nothing was stored for it, an `automatic` entry where something was
    /// (the server's merge takes that out); the other cards untouched.
    #[test]
    fn a_cards_amount_in_the_draft() {
        let always = KeepFree {
            mode: KeepFreeMode::Always,
            mb: 4198,
        };
        let draft = with_keep(&Value::Null, &Value::Null, "GPU-1", always);
        assert_eq!(draft, json!({"GPU-1": {"mode": "always", "mb": 4198}}));
        assert_eq!(keep_of(&draft, "GPU-1"), always);
        assert_eq!(keep_of(&draft, "GPU-2"), KeepFree::default());
        let back = with_keep(&draft, &Value::Null, "GPU-1", KeepFree::default());
        assert_eq!(back, json!({}));
        let stored = json!({"GPU-1": {"mode": "nothing", "mb": 0}});
        let back = with_keep(&stored, &stored, "GPU-1", KeepFree::default());
        assert_eq!(back, json!({"GPU-1": {"mode": "automatic", "mb": 0}}));
    }

    #[test]
    fn the_amount_box_reads_gib() {
        assert_eq!(gib_text(4198), "4.1");
        assert_eq!(mb_of("4.1"), Some(4198));
        assert_eq!(mb_of(" 2,5 "), Some(2560));
        assert_eq!(mb_of("0"), None);
        assert_eq!(mb_of("lots"), None);
    }
}
