//! The answer views (labs-store design §2.7), read from the run's response
//! with `SystemOneResponse`, whose `Ordered` keeps the options in the order
//! they were sent:
//! - `choice`: bars sorted by probability, highest first, each with its
//!   percentage and its place as sent (`#3`), so a reversal shows;
//! - `score`: the levels as a strip, lowest first, each cell filled by its
//!   probability, and a marker at the expected level;
//! - `noul`: a 0–1 meter with its midline, P(yes), and the lab's certainty,
//!   max(p, 1−p) (Decision 26: the wire gives `noul` no confidence).
//!
//! Also the compact form a matrix cell shows, and the Δ line a perturbed
//! cell gets against the same model as written.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::Variant;
use lmgw_api_types::systemone::{Answer, ChoiceAnswer, NoulAnswer, ScoreAnswer};

/// One `choice` bar.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct BarRow {
    pub name: String,
    pub p: f64,
    /// Its place among the options as sent, from 1.
    pub pos: usize,
    pub winner: bool,
}

/// The bars, highest first; equal probabilities keep the order sent.
pub(super) fn choice_bars(a: &ChoiceAnswer) -> Vec<BarRow> {
    let mut bars: Vec<BarRow> = a
        .probabilities
        .iter()
        .enumerate()
        .map(|(i, (name, p))| BarRow {
            name: name.to_string(),
            p: *p,
            pos: i + 1,
            winner: name == a.choice,
        })
        .collect();
    bars.sort_by(|x, y| y.p.total_cmp(&x.p));
    bars
}

/// How many levels a score has: its legend, else its probabilities.
fn levels(a: &ScoreAnswer) -> usize {
    a.legend.0.len().max(a.probabilities.0.len())
}

/// Where the expected level sits across the strip, 0 to 100: a whole level
/// on its cell's centre, so `(score + ½) / levels`.
pub(super) fn score_marker(a: &ScoreAnswer) -> f64 {
    let n = levels(a);
    if n == 0 {
        return 0.0;
    }
    ((a.score + 0.5) / n as f64 * 100.0).clamp(0.0, 100.0)
}

/// The level nearest the expected one, and its legend.
pub(super) fn nearest(a: &ScoreAnswer) -> (usize, String) {
    let n = levels(a).max(1);
    let i = (a.score.round().max(0.0) as usize).min(n - 1);
    let legend = a
        .legend
        .get(&i.to_string())
        .cloned()
        .or_else(|| a.legend.0.get(i).map(|(_, v)| v.clone()))
        .unwrap_or_default();
    (i, legend)
}

/// The lab's certainty of a `noul` answer, max(p, 1−p): the server's own
/// rule, the one the metrics cut on.
pub(super) use lmgw_api_types::decision_lab::metrics::certainty;

fn pct(p: f64) -> String {
    format!("{:.0}%", p * 100.0)
}

/// What a matrix cell shows: "billing 81%", "2.37 · today", "P(yes) 0.82".
pub(super) fn compact(a: &Answer) -> String {
    match a {
        Answer::Choice(c) => {
            let p = c.probabilities.get(&c.choice).copied().unwrap_or_default();
            format!("{} {}", c.choice, pct(p))
        }
        Answer::Score(s) => {
            let (_, legend) = nearest(s);
            if legend.is_empty() {
                format!("{:.2}", s.score)
            } else {
                format!("{:.2} · {legend}", s.score)
            }
        }
        Answer::Noul(n) => format!("P(yes) {:.2}", n.noul),
    }
}

/// How a Δ line names a variant.
fn under(v: Variant) -> &'static str {
    match v {
        Variant::AsWritten => "as written",
        Variant::NoDescriptions => "without descriptions",
        Variant::Reversed => "under reversal",
    }
}

/// The change a variant made to one question's answer against the same
/// model as written; `None` where the variant does not touch the question's
/// type (a score, or a noul under reversal) or the answers differ in type.
pub(super) fn delta(base: &Answer, other: &Answer, v: Variant) -> Option<String> {
    match (base, other, v) {
        (_, _, Variant::AsWritten) => None,
        (Answer::Choice(a), Answer::Choice(b), _) => {
            let pa = a.probabilities.get(&a.choice).copied().unwrap_or_default();
            if a.choice == b.choice {
                let pb = b.probabilities.get(&b.choice).copied().unwrap_or_default();
                Some(format!(
                    "winner unchanged · {} {pa:.2} → {pb:.2} {}",
                    a.choice,
                    under(v)
                ))
            } else {
                let pb = b.probabilities.get(&b.choice).copied().unwrap_or_default();
                Some(format!(
                    "winner changed {}: {} {pa:.2} → {} {pb:.2}",
                    under(v),
                    a.choice,
                    b.choice
                ))
            }
        }
        (Answer::Noul(a), Answer::Noul(b), Variant::NoDescriptions) => Some(format!(
            "P(yes) {:.2} → {:.2} without criteria",
            a.noul, b.noul
        )),
        _ => None,
    }
}

#[component]
pub(super) fn ChoiceView(a: ChoiceAnswer) -> impl IntoView {
    let bars = choice_bars(&a);
    view! {
        <div class="dl-choice">
            {bars
                .into_iter()
                .map(|b| {
                    view! {
                        <div class="dl-crow" class:win=b.winner>
                            <span class="n mono-sm" title=b.name.clone()>
                                {b.name.clone()}
                            </span>
                            <span class="dl-track">
                                <i style=format!("width:{:.1}%", b.p * 100.0)></i>
                            </span>
                            <span class="p mono-sm">{format!("{:.1}%", b.p * 100.0)}</span>
                            <span class="pos mono-sm dim" title="Its place among the options as sent">
                                {format!("#{}", b.pos)}
                            </span>
                        </div>
                    }
                })
                .collect_view()}
            <p class="mini-note dim">{format!("confidence {:.2}", a.confidence)}</p>
        </div>
    }
}

#[component]
pub(super) fn ScoreView(a: ScoreAnswer) -> impl IntoView {
    let n = levels(&a);
    let marker = score_marker(&a);
    let (near, legend) = nearest(&a);
    let cells: Vec<(String, f64, String)> = (0..n)
        .map(|i| {
            let k = i.to_string();
            let p = a.probabilities.get(&k).copied().unwrap_or_default();
            let l = a.legend.get(&k).cloned().unwrap_or_default();
            (k, p, l)
        })
        .collect();
    let cols = format!(
        "grid-template-columns: repeat({}, minmax(0, 1fr))",
        n.max(1)
    );
    view! {
        <div class="dl-score">
            <div class="dl-strip" style=cols.clone()>
                {cells
                    .iter()
                    .map(|(k, p, _)| {
                        view! {
                            <span class="cell" title=format!("level {k}: {:.1}%", p * 100.0)>
                                <i style=format!("opacity:{:.3}", p)></i>
                            </span>
                        }
                    })
                    .collect_view()}
                <span class="dl-marker" style=format!("left:{marker:.2}%")></span>
            </div>
            <div class="dl-legend" style=cols>
                {cells
                    .into_iter()
                    .map(|(k, p, l)| {
                        view! {
                            <span>
                                <b class="mono-sm">{k}</b>
                                <i class="mono-sm">{format!(" {:.0}%", p * 100.0)}</i>
                                <br/>
                                {l}
                            </span>
                        }
                    })
                    .collect_view()}
            </div>
            <p class="mini-note">
                {format!(
                    "{:.2} of 0–{} · nearest: {}",
                    a.score,
                    n.saturating_sub(1),
                    if legend.is_empty() { near.to_string() } else { legend },
                )}
                <span class="dim">{format!(" · confidence {:.2}", a.confidence)}</span>
            </p>
        </div>
    }
}

#[component]
pub(super) fn NoulView(a: NoulAnswer) -> impl IntoView {
    let p = a.noul;
    view! {
        <div class="dl-noul-view">
            <div class="dl-meter">
                <i style=format!("width:{:.1}%", p * 100.0)></i>
                <span class="mid"></span>
            </div>
            <p class="mini-note">
                <b class="mono-sm">{format!("P(yes) {p:.2}")}</b>
                {format!(" · {}", if p >= 0.5 { "yes" } else { "no" })}
                <span class="dim">
                    {format!(" · certainty {:.2} (the lab's: max(p, 1−p))", certainty(p))}
                </span>
            </p>
        </div>
    }
}

/// One answer by its type.
#[component]
pub(super) fn AnswerView(a: Answer) -> impl IntoView {
    match a {
        Answer::Choice(c) => view! { <ChoiceView a=c/> }.into_any(),
        Answer::Score(s) => view! { <ScoreView a=s/> }.into_any(),
        Answer::Noul(n) => view! { <NoulView a=n/> }.into_any(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(pairs: &[(&str, f64)], winner: &str) -> ChoiceAnswer {
        ChoiceAnswer {
            choice: winner.into(),
            probabilities: pairs.iter().map(|(k, v)| (*k, *v)).collect(),
            confidence: 0.7,
        }
    }

    fn score(score: f64, n: usize) -> ScoreAnswer {
        ScoreAnswer {
            score,
            legend: (0..n)
                .map(|i| (i.to_string(), format!("level {i}")))
                .collect(),
            probabilities: (0..n).map(|i| (i.to_string(), 1.0 / n as f64)).collect(),
            confidence: 0.5,
        }
    }

    #[test]
    fn bars_are_highest_first_and_keep_their_place_as_sent() {
        let a = choice(
            &[("billing", 0.2), ("technical", 0.7), ("account", 0.1)],
            "technical",
        );
        let bars = choice_bars(&a);
        let order: Vec<(&str, usize, bool)> = bars
            .iter()
            .map(|b| (b.name.as_str(), b.pos, b.winner))
            .collect();
        assert_eq!(
            order,
            [
                ("technical", 2, true),
                ("billing", 1, false),
                ("account", 3, false)
            ]
        );
        // a tie keeps the order sent
        let a = choice(&[("x", 0.5), ("y", 0.5)], "x");
        let names: Vec<String> = choice_bars(&a).into_iter().map(|b| b.name).collect();
        assert_eq!(names, ["x", "y"]);
    }

    #[test]
    fn the_score_marker_sits_on_its_levels_centre() {
        assert_eq!(score_marker(&score(0.0, 4)), 12.5);
        assert_eq!(score_marker(&score(3.0, 4)), 87.5);
        assert!((score_marker(&score(2.37, 5)) - 57.4).abs() < 1e-9);
        assert_eq!(score_marker(&score(9.0, 4)), 100.0);
        assert_eq!(nearest(&score(2.37, 5)), (2, "level 2".to_string()));
        assert_eq!(nearest(&score(7.0, 4)).0, 3);
    }

    #[test]
    fn noul_certainty_is_the_far_side() {
        assert_eq!(certainty(0.82), 0.82);
        assert!((certainty(0.1) - 0.9).abs() < 1e-12);
        assert_eq!(certainty(0.5), 0.5);
    }

    #[test]
    fn compact_and_delta_lines() {
        let a = Answer::Choice(choice(&[("billing", 0.81), ("technical", 0.19)], "billing"));
        assert_eq!(compact(&a), "billing 81%");
        let b = Answer::Choice(choice(
            &[("billing", 0.42), ("technical", 0.58)],
            "technical",
        ));
        let same = Answer::Choice(choice(&[("billing", 0.42), ("technical", 0.38)], "billing"));
        assert_eq!(
            delta(&a, &same, Variant::NoDescriptions).unwrap(),
            "winner unchanged · billing 0.81 → 0.42 without descriptions"
        );
        assert_eq!(
            delta(&a, &b, Variant::Reversed).unwrap(),
            "winner changed under reversal: billing 0.81 → technical 0.58"
        );
        let s = Answer::Score(score(1.0, 3));
        assert_eq!(delta(&s, &s, Variant::NoDescriptions), None);
        let n1 = Answer::Noul(NoulAnswer { noul: 0.82 });
        let n2 = Answer::Noul(NoulAnswer { noul: 0.4 });
        assert_eq!(
            delta(&n1, &n2, Variant::NoDescriptions).unwrap(),
            "P(yes) 0.82 → 0.40 without criteria"
        );
        assert_eq!(delta(&n1, &n2, Variant::Reversed), None);
        assert_eq!(compact(&n1), "P(yes) 0.82");
    }
}
