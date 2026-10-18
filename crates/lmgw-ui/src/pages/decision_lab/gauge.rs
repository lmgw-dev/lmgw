//! The token gauge (labs-store design §2.5): the form's pieces counted on the
//! head model through `/decision-lab/api/count`, one bar per limit of the
//! model's header shape, the limit's rule in its tooltip. Past the limit the
//! bar is amber with the rule written out; it never blocks Ask, because the
//! 500 past it is llama.cpp's, relayed by design.
//!
//! Counting goes through the route, so on a local row that is not running it
//! would start the container: the gauge counts by itself, 800 ms after the
//! last edit, only when the model is ready or in the cloud. Otherwise it says
//! "not counted" with a Count button that loads it.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{DecisionCount, DecisionCounted, DecisionEstimate};

use super::page::Lab;
use crate::api;
use crate::fmt::grouped;

/// How long the gauge waits after the last edit before it counts.
const QUIET_MS: u64 = 800;

/// Where the gauge stands.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Gauge {
    /// Nothing to count, or no model.
    Idle,
    /// The head model is local and not loaded: counting would start it.
    NotLoaded(String),
    Counting,
    Counted(DecisionCounted),
    Failed(String),
}

/// One bar.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Bar {
    /// "7 669 of 8 192"
    pub text: String,
    /// The fill, 0 to 100.
    pub pct: f64,
    pub over: bool,
    /// What the number is and where it comes from.
    pub of: String,
    pub rule: String,
}

pub(super) fn bar(e: &DecisionEstimate) -> Bar {
    let (text, pct) = match e.limit.tokens {
        Some(l) if l > 0 => (
            format!("{} of {}", grouped(e.tokens), grouped(l)),
            (e.tokens as f64 / l as f64 * 100.0).min(100.0),
        ),
        _ => (format!("{} · limit not known", grouped(e.tokens)), 0.0),
    };
    Bar {
        text,
        pct,
        over: e.over,
        of: e.limit.of.clone(),
        rule: e.limit.rule.clone(),
    }
}

/// Why the numbers are approximate, in words.
pub(super) fn approx_line(flags: &[String]) -> String {
    let words: Vec<&str> = flags
        .iter()
        .map(|f| match f.as_str() {
            "flattened" => "the template is not counted",
            "media_omitted" => "images are not counted",
            "tokenizer_guess" => "a tokenizer guess, the model has no counter",
            other => other,
        })
        .collect();
    if words.is_empty() {
        String::new()
    } else {
        format!("approximate: {}", words.join(" · "))
    }
}

/// A question's tokens against the head budget, for its card: (text, over).
pub(super) fn head_line(tokens: u64, budget: Option<u64>) -> (String, bool) {
    match budget {
        Some(b) => (
            format!("{} tokens · head budget {}", grouped(tokens), grouped(b)),
            tokens > b,
        ),
        None => (format!("{} tokens", grouped(tokens)), false),
    }
}

/// Count the form on the head model now; `load` starts its container when
/// it is not running.
pub(super) fn count_now(lab: Lab, load: bool) {
    let model = lab.alias.get_untracked();
    if model.trim().is_empty() {
        lab.count_ticket();
        lab.gauge.set(Gauge::Idle);
        return;
    }
    let body = DecisionCount {
        model,
        form: untrack(|| lab.form.draft()),
        load,
    };
    let ticket = lab.count_ticket();
    // The last count stays up while the next one runs, unless it was for
    // another model.
    let keep = lab
        .gauge
        .with_untracked(|g| matches!(g, Gauge::Counted(c) if c.model == body.model));
    if !keep {
        lab.gauge.set(Gauge::Counting);
    }
    lab.scope.spawn(async move {
        let got = api::post::<DecisionCounted, _>("/decision-lab/api/count", &body).await;
        // A load started the container: its /props fill the facts, whether
        // or not this count is still the newest.
        if load {
            lab.reload_models();
        }
        if !lab.count_is(ticket) {
            return;
        }
        lab.gauge.set(match got {
            Ok(c) => Gauge::Counted(c),
            Err(e) => Gauge::Failed(e.to_string()),
        });
    });
}

/// Where the gauge goes the moment its input changes, before any count:
/// `None` keeps it as it is. A count of the same model stays up while the
/// next one waits and runs; another model's is cleared at once, so its
/// bars and per-question numbers never stand beside the new model's name.
pub(super) fn on_change(
    now: &Gauge,
    model: Option<(&str, bool, bool)>,
    empty: bool,
) -> Option<Gauge> {
    let Some((name, local, ready)) = model else {
        return Some(Gauge::Idle);
    };
    if empty {
        return Some(Gauge::Idle);
    }
    if local && !ready {
        return Some(Gauge::NotLoaded(name.to_string()));
    }
    match now {
        Gauge::Counted(c) if c.model == name => None,
        Gauge::Counting => None,
        _ => Some(Gauge::Counting),
    }
}

/// Count by itself after each edit, when that starts nothing.
pub(super) fn auto_count(lab: Lab) {
    let timer = StoredValue::new(None::<TimeoutHandle>);
    // What a count depends on, so a runtime frame or a model list read that
    // changes none of it does not count again: the form's text, which
    // images it holds (by row, not by their bytes), and the head model's
    // name, whether it is local, whether it is ready.
    let input = Memo::new(move |_| {
        let m = lab.model().map(|m| {
            let ready = lab.state_of(&m).as_deref() == Some("ready");
            (m.name, m.local, ready)
        });
        (lab.form.text_draft(), lab.form.image_keys(), m)
    });
    Effect::new(move |_| {
        let (form, _, model) = input.get();
        if let Some(h) = timer.get_value() {
            h.clear();
        }
        // Whatever comes next, a count on its way is for an older input.
        lab.count_ticket();
        let empty = form.state.trim().is_empty() && form.questions.is_empty();
        let m = model.as_ref().map(|(n, l, r)| (n.as_str(), *l, *r));
        if let Some(g) = lab.gauge.with_untracked(|now| on_change(now, m, empty)) {
            lab.gauge.set(g);
        }
        let countable = matches!(m, Some((_, local, ready)) if !local || ready);
        if empty || !countable {
            return;
        }
        let h = set_timeout_with_handle(
            move || {
                if lab.scope.alive() {
                    count_now(lab, false);
                }
            },
            std::time::Duration::from_millis(QUIET_MS),
        )
        .ok();
        timer.set_value(h);
    });
    on_cleanup(move || {
        if let Some(Some(h)) = timer.try_get_value() {
            h.clear();
        }
    });
}

#[component]
pub(super) fn GaugeCard(lab: Lab) -> impl IntoView {
    let count_btn = move |label: &'static str| {
        view! {
            <button
                class="btn ghost sm"
                title="Count now, loading the model when it is not running: cheap on a CPU row, your call on a GPU row"
                on:click=move |_| count_now(lab, true)
            >
                {label}
            </button>
        }
    };
    view! {
        <div class="card dl-gauge">
            <div class="dl-card-head">
                <h3 class="lab-h3">"Tokens"</h3>
                <span class="spacer"></span>
                {move || {
                    matches!(lab.gauge.get(), Gauge::Counted(_) | Gauge::Failed(_))
                        .then(|| count_btn("Count again"))
                }}
            </div>
            {move || match lab.gauge.get() {
                Gauge::Idle => {
                    view! { <p class="mini-note dim">"Write a state and a question to count them."</p> }
                        .into_any()
                }
                Gauge::Counting => view! { <p class="mini-note dim">"counting…"</p> }.into_any(),
                Gauge::NotLoaded(m) => {
                    view! {
                        <div class="dl-not-counted">
                            <span class="mini-note">{format!("not counted: {m} is not loaded")}</span>
                            {count_btn("Count")}
                        </div>
                    }
                        .into_any()
                }
                Gauge::Failed(e) => {
                    view! { <p class="mini-note dl-bad-line">{format!("not counted: {e}")}</p> }
                        .into_any()
                }
                Gauge::Counted(c) => counted_view(lab, c).into_any(),
            }}
            {move || {
                let m = lab.alias.get();
                lab.measured
                    .get()
                    .filter(|(asked, _)| *asked == m)
                    .map(|(_, t)| {
                        view! {
                            <p class="mini-note dim" title="input_tokens of the last answer on this model; types that do not share the state count it once per question">
                                {format!("measured on the last answer: {} input tokens", grouped(t))}
                            </p>
                        }
                    })
            }}
        </div>
    }
}

fn counted_view(lab: Lab, c: DecisionCounted) -> impl IntoView {
    if let Some(why) = c.not_counted.clone() {
        return view! {
            <div class="dl-not-counted">
                <span class="mini-note">{format!("not counted: {why}")}</span>
                <button class="btn ghost sm" on:click=move |_| count_now(lab, true)>
                    "Count"
                </button>
            </div>
        }
        .into_any();
    }
    let bars: Vec<Bar> = c.estimates.iter().map(bar).collect();
    let approx = approx_line(&c.approx);
    let state = c.state.map(|s| format!("state {} tokens", grouped(s)));
    let by = c
        .answered_by
        .clone()
        .map(|f| format!("counted by {f}, the fallback"));
    view! {
        {bars
            .into_iter()
            .map(|b| {
                let over = b.over;
                let rule = b.rule.clone();
                view! {
                    <div class="dl-bar" class:over=over title=b.rule.clone()>
                        <div class="dl-bar-label">
                            <span class="of">{b.of.clone()}</span>
                            <span class="mono-sm">{b.text.clone()}</span>
                        </div>
                        <div class="dl-track">
                            <i style=format!("width:{:.1}%", b.pct)></i>
                        </div>
                        {over.then(|| view! { <p class="mini-note dl-rule">{rule}</p> })}
                    </div>
                }
            })
            .collect_view()}
        <p class="mini-note dim">
            {[state, Some(approx).filter(|s| !s.is_empty()), by]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ")}
        </p>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::{DecisionLimit, DecisionMeasure};

    fn est(tokens: u64, limit: Option<u64>, over: bool) -> DecisionEstimate {
        DecisionEstimate {
            limit: DecisionLimit {
                applies_to: DecisionMeasure::StateAndQuestion,
                tokens: limit,
                of: "the ubatch in effect".into(),
                rule: "laya evaluates the whole prompt in one ubatch (8192)".into(),
            },
            tokens,
            over,
        }
    }

    #[test]
    fn a_bar_fills_to_its_share_and_stops_full() {
        let b = bar(&est(4096, Some(8192), false));
        assert_eq!(b.pct, 50.0);
        assert_eq!(b.text, "4\u{202F}096 of 8\u{202F}192");
        let b = bar(&est(9000, Some(8192), true));
        assert_eq!(b.pct, 100.0);
        assert!(b.over);
        let b = bar(&est(10, None, false));
        assert_eq!(b.text, "10 · limit not known");
    }

    fn counted(model: &str) -> Gauge {
        Gauge::Counted(
            serde_json::from_value(serde_json::json!({
                "model": model, "state": 3, "questions": [],
                "estimates": [], "approx": [], "not_counted": null, "answered_by": null
            }))
            .unwrap(),
        )
    }

    #[test]
    fn another_models_count_is_cleared_at_once() {
        let laya = counted("decide/laya");
        // the same model: the count stays up while the next one waits
        assert_eq!(
            on_change(&laya, Some(("decide/laya", true, true)), false),
            None
        );
        // another model: cleared before its count
        assert_eq!(
            on_change(&laya, Some(("decide/jev", false, false)), false),
            Some(Gauge::Counting)
        );
        assert_eq!(
            on_change(&laya, Some(("decide/kev-4b", true, false)), false),
            Some(Gauge::NotLoaded("decide/kev-4b".into()))
        );
        assert_eq!(on_change(&laya, None, false), Some(Gauge::Idle));
        assert_eq!(
            on_change(&laya, Some(("decide/laya", true, true)), true),
            Some(Gauge::Idle)
        );
    }

    #[test]
    fn approximations_and_the_head_budget_read_in_words() {
        assert_eq!(
            approx_line(&["flattened".into(), "media_omitted".into()]),
            "approximate: the template is not counted · images are not counted"
        );
        assert_eq!(approx_line(&[]), "");
        assert_eq!(
            head_line(200, Some(192)),
            ("200 tokens · head budget 192".into(), true)
        );
        assert_eq!(head_line(20, None), ("20 tokens".into(), false));
    }
}
