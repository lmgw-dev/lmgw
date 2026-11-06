//! The items (machine-check design §1, §4.4): one row each, a mark, the
//! plain sentence, and the technical lines behind a "Details" fold. A fix
//! shows under the item it is for: the settings switch's confirm under the
//! model settings item, the plan block and the progress view under the item
//! for using the card for models, whose plan it is (§1.2).

use leptos::prelude::*;
use lmgw_api_types::machine::{ItemState, MachineItem, MachineItemId, MachineVerdict};

use super::fit::{FitConfirm, Offer};
use super::{FitSlot, MachineState};

/// How a mark looks: its class, and the check drawn or a letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    Ok,
    Problem,
    Info,
    NotChecked,
}

impl Mark {
    pub(crate) fn of(s: ItemState) -> Self {
        match s {
            ItemState::Ok => Self::Ok,
            ItemState::Problem => Self::Problem,
            ItemState::Info => Self::Info,
            ItemState::NotChecked => Self::NotChecked,
        }
    }

    /// An item's mark under the headline's verdict: with `cloud_only` there
    /// is nothing to fix, so the item that decided it (no card, a problem
    /// to core, which blames a local start on it) reads as the headline
    /// does, worth knowing (review L5).
    pub(crate) fn item(s: ItemState, cloud_only: bool) -> Self {
        match Self::of(s) {
            Self::Problem if cloud_only => Self::Info,
            m => m,
        }
    }

    /// The headline's: ready is a check, cloud only is worth knowing, every
    /// `needs_*` wants something done.
    pub(crate) fn verdict(v: MachineVerdict) -> Self {
        match v {
            MachineVerdict::Ready => Self::Ok,
            MachineVerdict::CloudOnly => Self::Info,
            MachineVerdict::NeedsFix
            | MachineVerdict::NeedsSteps
            | MachineVerdict::NeedsRestart
            | MachineVerdict::NeedsDriver => Self::Problem,
        }
    }

    /// Prefixed: app.css styles a bare `.problem` (red, smaller) for other
    /// pages.
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Ok => "mc-ok",
            Self::Problem => "mc-problem",
            Self::Info => "mc-info",
            Self::NotChecked => "mc-not-checked",
        }
    }
}

/// The check, drawn: a "✓" glyph is missing from the dashboard's own font
/// and falls back to whatever the browser finds (a "V" in some).
pub(crate) fn tick() -> impl IntoView {
    view! {
        <svg class="mc-tick" viewBox="0 0 16 16" aria-hidden="true">
            <path d="M3.5 8.5l3 3 6-7"/>
        </svg>
    }
}

/// A mark in a circle; `big` for the headline.
#[component]
pub(crate) fn MarkView(mark: Mark, #[prop(optional)] big: bool) -> impl IntoView {
    let class = format!("mc-mark {}{}", mark.class(), if big { " big" } else { "" });
    view! {
        <span class=class aria-hidden="true">
            {match mark {
                Mark::Ok => tick().into_any(),
                Mark::Problem => "!".into_any(),
                Mark::Info => "i".into_any(),
                Mark::NotChecked => "–".into_any(),
            }}
        </span>
    }
}

/// What an item is about, in front of a "Not checked yet" sentence, which
/// on its own does not say what was not checked.
pub(crate) fn name(id: MachineItemId) -> &'static str {
    match id {
        MachineItemId::GraphicsCard => "Graphics card",
        MachineItemId::CardForModels => "Using the card for models",
        MachineItemId::RunningModels => "Running models",
        MachineItemId::SettingsFit => "Model settings",
        MachineItemId::Room => "Room for models",
        MachineItemId::Download => "The download for your graphics card",
    }
}

/// An item id as a list key (the type has no `Hash`).
pub(crate) fn key(id: MachineItemId) -> u8 {
    match id {
        MachineItemId::GraphicsCard => 0,
        MachineItemId::CardForModels => 1,
        MachineItemId::RunningModels => 2,
        MachineItemId::SettingsFit => 3,
        MachineItemId::Room => 4,
        MachineItemId::Download => 5,
    }
}

/// The technical lines, folded.
#[component]
pub(crate) fn DetailFold(
    lines: Vec<String>,
    #[prop(default = "Details")] label: &'static str,
) -> impl IntoView {
    (!lines.is_empty()).then(|| {
        view! {
            <details class="mc-fold">
                <summary>{label}</summary>
                <ul class="mc-lines">
                    {lines.into_iter().map(|l| view! { <li>{l}</li> }).collect_view()}
                </ul>
            </details>
        }
    })
}

/// One item, by its id: the row stays while the check is replaced, so a
/// confirm opened under it keeps its state. `fix` is what shows under it
/// (the plan block and the progress view, for the item they are for).
#[component]
pub(crate) fn ItemRow(
    id: MachineItemId,
    st: MachineState,
    /// The settings switch's confirm, its write and its note.
    fit_slot: FitSlot,
    /// The verdict is `cloud_only`: a problem item reads as information.
    cloud_only: Memo<bool>,
    /// The switch wrote the settings.
    settings_written: Option<Callback<()>>,
    fix: Option<ViewFn>,
) -> impl IntoView {
    let slot = fit_slot;
    let check = st.check;
    let item = Memo::new(move |_| {
        check.with(|c| {
            c.as_ref()
                .and_then(|c| c.items.iter().find(|i| i.id == id).cloned())
        })
    });
    let mark_of = move |s: ItemState| Mark::item(s, cloud_only.get());
    let body = move || {
        item.get().map(|it: MachineItem| {
            let mark = mark_of(it.state);
            let action = it.action.as_ref().map(Offer::of);
            let button = action.map(|offer| {
                let label = offer.label.clone();
                view! {
                    <button
                        class="btn sm"
                        disabled=move || slot.offer.with(Option::is_some) || st.checking.get()
                        on:click=move |_| {
                            slot.note.set(None);
                            slot.offer.set(Some(offer.clone()));
                        }
                    >
                        {label}
                    </button>
                }
            });
            let named = (mark == Mark::NotChecked)
                .then(|| view! { <span class="mc-name">{name(id)}</span> " · " });
            view! {
                <MarkView mark=mark/>
                <div class="mc-item-body">
                    <div class="mc-sentence">{named} {it.sentence.clone()}</div>
                    {button.map(|b| view! { <div class="mc-item-act">{b}</div> })}
                    <DetailFold lines=it.detail.clone()/>
                </div>
            }
        })
    };
    let class = move || {
        item.with(|i| {
            i.as_ref()
                .map(|i| format!("mc-item {}", mark_of(i.state).class()))
                .unwrap_or_else(|| "mc-item".into())
        })
    };
    let fit_here = id == MachineItemId::SettingsFit;
    view! {
        <li class=class hidden=move || item.with(Option::is_none)>
            {body}
            {move || {
                fit_here
                    .then(|| slot.offer.get())
                    .flatten()
                    .map(|offer| {
                        view! { <FitConfirm offer=offer st=st fit_slot=slot settings_written=settings_written/> }
                    })
            }}
            {move || {
                fit_here
                    .then(|| slot.note.get())
                    .flatten()
                    .map(|n| view! { <div class="mc-note plain" role="status">{n}</div> })
            }}
            {fix.map(|f| view! { <div class="mc-fix">{f.run()}</div> })}
        </li>
    }
}
