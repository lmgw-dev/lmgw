//! A file's fit over the graphics cards as badges (device-ledger design
//! §13 WP12): **Min. cards** ("2 of 4"), **Free VRAM** ("fits now",
//! "after unloading chat/q", "needs 21.0 GiB, card 2 has 18.0") and, for a
//! mixture-of-experts model no card holds whole, **with experts in RAM**,
//! each with a tick, a "!" or a cross, and the context they are judged at.
//! The finder's card opens the figures per card below them; the file list
//! keeps a row to one line and puts them in the tooltip.

use leptos::prelude::*;
use lmgw_api_types::model_find::{
    CardPart, CardsFit, ExpertsInRam, FreeState, FreeVram, MinCards, MinCardsState, Sign,
};

use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::{Ico, Icon};

/// The chip's tone for a sign, as `.chip` draws it.
pub(crate) fn tone(sign: Sign) -> &'static str {
    match sign {
        Sign::Ok => "ok",
        Sign::Warn => "warn",
        Sign::No => "err",
        Sign::Unknown => "off",
    }
}

/// The sign's icon: a tick, a cross, "!" or "?".
fn sign_icon(sign: Sign) -> AnyView {
    match sign {
        Sign::Ok => view! { <Icon ico=Ico::Check class="fs-ico"/> }.into_any(),
        Sign::No => view! { <Icon ico=Ico::Close class="fs-ico"/> }.into_any(),
        Sign::Warn => {
            view! { <span class="fs-ico fs-glyph" aria-hidden="true">"!"</span> }.into_any()
        }
        Sign::Unknown => {
            view! { <span class="fs-ico fs-glyph" aria-hidden="true">"?"</span> }.into_any()
        }
    }
}

/// One card's figures in a line: "card 2 · NVIDIA RTX 6000 · 21.0 GiB of
/// 48.0 GiB · 30.1 GiB free · 18 layers".
pub(crate) fn part_line(p: &CardPart) -> String {
    let mut s = format!("card {}", p.index);
    if !p.name.is_empty() {
        s.push_str(&format!(" · {}", p.name));
    }
    s.push_str(&format!(
        " · {} of {} · {} free",
        fmt_bytes(p.bytes),
        fmt_bytes(p.total_bytes),
        fmt_bytes(p.free_bytes)
    ));
    if let Some(l) = p.layers {
        s.push_str(&format!(" · {l} layers"));
    }
    s
}

/// A signal's tooltip: its sentence and each card's figures.
fn tip(sentence: &str, parts: &[CardPart]) -> String {
    let mut lines = vec![sentence.to_string()];
    lines.extend(parts.iter().map(part_line));
    lines.join("\n")
}

/// Whether the computer's card count is worth a badge (not on one card).
fn shows_min(m: &MinCards) -> bool {
    m.state != MinCardsState::OneCard
}

/// The whole tooltip of a row's signals, for the file list.
pub(crate) fn row_tip(c: &CardsFit) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(ctx) = c.ctx {
        lines.push(format!("Judged at {} tokens.", grouped(ctx)));
    }
    if shows_min(&c.min_cards) {
        lines.push(format!(
            "Min. cards: {}",
            tip(&c.min_cards.sentence, &c.min_cards.parts)
        ));
    }
    lines.push(format!(
        "Free VRAM: {}",
        tip(&c.free.sentence, &c.free.cards)
    ));
    if let Some(e) = &c.experts_in_ram {
        lines.push(tip(&e.sentence, &e.parts));
    }
    lines.extend(c.notes.iter().cloned());
    lines.join("\n")
}

#[component]
fn MinBadge(m: MinCards, compact: bool) -> impl IntoView {
    // "Min. cards 2 of 4" on the card, "2 of 4 cards" in a row; a budget
    // says "budget" with its tick or cross.
    let (key, val) = match (m.state, compact) {
        (MinCardsState::Budget, _) => (None, "budget".to_string()),
        // "?" is the icon already.
        (MinCardsState::Unknown, true) => (None, "cards".to_string()),
        (MinCardsState::Unknown, false) => (Some("Min. cards"), String::new()),
        (MinCardsState::TooLarge, true) => (None, format!("cards: {}", m.label)),
        (_, true) => (None, format!("{} cards", m.label)),
        (_, false) => (Some("Min. cards"), m.label.clone()),
    };
    let title = tip(&m.sentence, &m.parts);
    view! {
        <span class=format!("chip {} fs-badge", tone(m.sign)) title=title>
            {sign_icon(m.sign)}
            {key.map(|k| view! { <span class="fs-key">{k}</span> })}
            <span class="fs-val">{val}</span>
        </span>
    }
}

#[component]
fn FreeBadge(f: FreeVram, compact: bool) -> impl IntoView {
    let title = tip(&f.sentence, &f.cards);
    view! {
        <span class=format!("chip {} fs-badge", tone(f.sign)) title=title>
            {sign_icon(f.sign)}
            {(!compact || f.state == FreeState::Unknown).then(|| view! { <span class="fs-key">"Free VRAM"</span> })}
            {(f.state != FreeState::Unknown).then(|| view! { <span class="fs-val">{f.label.clone()}</span> })}
        </span>
    }
}

/// Free VRAM's badge alone, without its name: what a choice of Get it's
/// says of its room (WP12).
#[component]
pub fn FreeChip(f: FreeVram) -> impl IntoView {
    view! { <span class="fs-signals compact"><FreeBadge f=f compact=true/></span> }
}

#[component]
fn ExpertsBadge(e: ExpertsInRam) -> impl IntoView {
    let title = tip(&e.sentence, &e.parts);
    view! {
        <span class=format!("chip {} fs-badge", tone(e.sign)) title=title>
            {sign_icon(e.sign)}
            <span class="fs-val">{e.label.clone()}</span>
        </span>
    }
}

/// The badges of `c`: the context, Min. cards (not on one card), Free VRAM
/// and the experts in RAM. `compact`: the file list's row, without the
/// signals' names and the figures below them.
#[component]
pub fn FitSignals(c: CardsFit, #[prop(optional)] compact: bool) -> impl IntoView {
    let ctx = c.ctx.map(|n| format!("at {} tokens", grouped(n)));
    let min = shows_min(&c.min_cards).then(|| c.min_cards.clone());
    let experts = c.experts_in_ram.clone();
    // Each card once, with what each signal puts there.
    let mut rows: Vec<(String, Vec<CardPart>)> = Vec::new();
    if let Some(m) = &min {
        if !m.parts.is_empty() {
            rows.push(("Min. cards: every card empty".into(), m.parts.clone()));
        }
    }
    if !c.free.cards.is_empty() {
        rows.push((
            "Free VRAM: the cards it would use now".into(),
            c.free.cards.clone(),
        ));
    }
    if let Some(e) = &experts {
        if !e.parts.is_empty() {
            rows.push((
                format!("With experts in RAM ({} in RAM)", fmt_bytes(e.ram_bytes)),
                e.parts.clone(),
            ));
        }
    }
    let sentences: Vec<String> = [
        min.as_ref().map(|m| m.sentence.clone()),
        Some(c.free.sentence.clone()),
        experts.as_ref().map(|e| e.sentence.clone()),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .chain(c.notes.iter().cloned())
    .collect();
    let title = compact.then(|| row_tip(&c));
    view! {
        <span class=if compact { "fs-signals compact" } else { "fs-signals" } title=title>
            {min.map(|m| view! { <MinBadge m=m compact=compact/> })}
            <FreeBadge f=c.free.clone() compact=compact/>
            {experts.map(|e| view! { <ExpertsBadge e=e/> })}
            {ctx.map(|t| view! { <span class="fs-ctx dim">{t}</span> })}
        </span>
        {(!compact).then(|| view! {
            <details class="fs-fold">
                <summary>"Per card"</summary>
                <div class="fs-fold-body">
                    {sentences.into_iter().map(|s| view! { <p>{s}</p> }).collect_view()}
                    {rows
                        .into_iter()
                        .map(|(head, parts)| view! {
                            <div class="fs-cards">
                                <span class="dim">{head}</span>
                                <ul>
                                    {parts
                                        .iter()
                                        .map(|p| view! { <li class="mono-sm">{part_line(p)}</li> })
                                        .collect_view()}
                                </ul>
                            </div>
                        })
                        .collect_view()}
                </div>
            </details>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_s_line_says_its_figures() {
        let p = CardPart {
            index: 2,
            name: "NVIDIA RTX".into(),
            bytes: 3 << 30,
            layers: Some(18),
            total_bytes: 48 << 30,
            free_bytes: 30 << 30,
            keep_bytes: 0,
        };
        assert_eq!(
            part_line(&p),
            "card 2 · NVIDIA RTX · 3.0 GiB of 48.0 GiB · 30.0 GiB free · 18 layers"
        );
    }

    #[test]
    fn one_card_hides_min_cards_from_the_row_s_tip() {
        let c = CardsFit {
            ctx: Some(8192),
            of: 1,
            min_cards: MinCards {
                state: MinCardsState::OneCard,
                sentence: "lmgw measures one graphics card here.".into(),
                ..MinCards::default()
            },
            free: FreeVram {
                sign: Sign::Ok,
                label: "fits now".into(),
                sentence: "There is room for it now.".into(),
                ..FreeVram::default()
            },
            ..CardsFit::default()
        };
        let t = row_tip(&c);
        assert!(!t.contains("Min. cards"), "{t}");
        assert!(
            t.starts_with(&format!("Judged at {} tokens.", grouped(8192))),
            "{t}"
        );
        assert_eq!(tone(Sign::No), "err");
    }
}
