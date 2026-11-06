//! The GPU memory of the drafted voice cascade (realtime design §12): the
//! `realtime_budget` op's answer as a bar, a sentence and a table.

use leptos::prelude::*;
use lmgw_api_types::realtime::{RealtimeBudget, RealtimeBudgetStage};
use serde_json::{json, Value};

use super::super::{Def, Page};
use crate::fmt::fmt_bytes;
use crate::scope::{Latest, Scope};
use crate::widgets::FormState;

/// The `realtime_budget` arguments for the draft: every alias as typed, so
/// an unsaved pick is sized too.
fn budget_args(form: FormState) -> Value {
    let t = |k: &str| form.text(&format!("realtime.{k}")).trim().to_string();
    json!({
        "default_model": t("default_model"),
        "asr_alias": t("asr_alias"),
        "tts_alias": t("tts_alias"),
        "barge_in_check": t("barge_in_check"),
        "barge_in_check_alias": t("barge_in_check_alias"),
    })
}

/// A stage's place, in a word for its column.
fn place_label(s: &RealtimeBudgetStage) -> &'static str {
    match s.placement.as_str() {
        "local" => "this GPU",
        "cloud" => "cloud",
        "external" => "not lmgw's",
        "cpu" => "CPU",
        "shared" => "shared",
        "unset" => "not set",
        _ => "unresolved",
    }
}

/// The colour a stage's share of the bar is drawn in: one series colour per
/// stage, the same in the bar, the legend and the table.
fn stage_colour(stage: &str) -> &'static str {
    match stage {
        "chat" => "var(--c1)",
        "asr" => "var(--c3)",
        "check" => "var(--c4)",
        "tts" => "var(--c2)",
        _ => "var(--c-other)",
    }
}

/// What the drafted cascade holds on the GPU, stage by stage, against what
/// lmgw may use: the figures admission charges, read from `realtime_budget`
/// (realtime design §12). The LLM and the voice share one card — the bar
/// shows how.
#[component]
pub(in crate::pages::settings) fn BudgetPanel(
    d: &'static Def,
    page: Page,
    id: String,
    hidden: Signal<bool>,
) -> impl IntoView {
    let form = page.form;
    let args = Memo::new(move |_| budget_args(form));
    let budget = RwSignal::new(None::<Result<RealtimeBudget, String>>);
    let loading = RwSignal::new(false);
    let refresh = RwSignal::new(0u32);
    let latest = Latest::new();
    let scope = Scope::new();
    Effect::new(move |_| {
        let a = args.get();
        refresh.track();
        // Asked while the card is on screen, again when an alias moves.
        if !page.shows(d) {
            return;
        }
        let Some(ticket) = latest.next() else { return };
        loading.set(true);
        scope.spawn(async move {
            let res = crate::api::post::<RealtimeBudget, _>("/api/op/realtime_budget", &a).await;
            if !latest.is(ticket) {
                return;
            }
            loading.set(false);
            budget.set(Some(res.map_err(|e| e.to_string())));
        });
    });
    let body = move || match budget.get() {
        None => view! { <div class="dim">"Sizing the cascade…"</div> }.into_any(),
        Some(Err(e)) => view! {
            <div class="notice err">"The budget could not be read: " {e}</div>
        }
        .into_any(),
        Some(Ok(b)) => budget_view(b).into_any(),
    };
    view! {
        <div class="rt-budget" id=id hidden=move || hidden.get()>
            {body}
            <div class="row rt-budget-foot">
                <span class="field-hint">
                    "Each figure is what admission charges: a chat model's weights, KV \
                     cache and compute buffers from its GGUF and settings, an audio model's \
                     learned residency — or its size on disk until it has served a request. \
                     A cloud stage holds nothing here."
                </span>
                <button
                    class="btn ghost sm"
                    disabled=move || loading.get()
                    title="Measure again — what other programs hold changes"
                    on:click=move |_| refresh.update(|n| *n += 1)
                >
                    {move || if loading.get() { "Measuring…" } else { "Refresh" }}
                </button>
            </div>
        </div>
    }
}

fn budget_view(b: RealtimeBudget) -> impl IntoView {
    let (chip, word) = match b.verdict.as_str() {
        "fits" => ("chip ok", "fits"),
        "tight" => ("chip warn", "tight now"),
        "too_large" => ("chip err", "does not fit"),
        _ => ("chip off", "unknown"),
    };
    // The track is what lmgw may use; a cascade larger than that widens the
    // scale, and a mark shows where the card ends.
    let capacity = b.capacity_bytes;
    let scale = capacity
        .unwrap_or(b.needed_bytes)
        .max(b.needed_bytes)
        .max(1);
    let pct = |bytes: u64| bytes as f64 * 100.0 / scale as f64;
    let mut segments: Vec<(String, String, u64)> = b
        .stages
        .iter()
        .filter(|s| s.bytes.unwrap_or(0) > 0)
        .map(|s| {
            (
                s.label.clone(),
                stage_colour(&s.stage).to_string(),
                s.bytes.unwrap_or(0),
            )
        })
        .collect();
    if b.headroom_bytes > 0 {
        segments.push(("Headroom".into(), "var(--c-other)".into(), b.headroom_bytes));
    }
    let outside = b.outside_bytes.filter(|o| *o > 0);
    let bar = segments
        .iter()
        .map(|(label, colour, bytes)| {
            view! {
                <i
                    style=format!("width:{:.2}%;background:{colour}", pct(*bytes))
                    title=format!("{label}: {}", fmt_bytes(*bytes))
                ></i>
            }
        })
        .collect_view();
    // What other programs hold sits at the card's far end: where it meets
    // the cascade's share, the two do not fit side by side.
    let outside_seg = outside.zip(capacity).map(|(o, cap)| {
        view! {
            <b
                class="rt-outside"
                style=format!(
                    "right:{:.2}%;width:{:.2}%",
                    pct(scale - cap),
                    pct(o.min(cap)),
                )
                title=format!("other programs: {}", fmt_bytes(o))
            ></b>
        }
    });
    let cap_mark = capacity.filter(|c| *c < scale).map(|c| {
        view! {
            <b
                class="rt-cap"
                style=format!("left:{:.2}%", pct(c))
                title=format!("lmgw may use {}", fmt_bytes(c))
            ></b>
        }
    });
    let legend = segments
        .iter()
        .map(|(label, colour, bytes)| {
            view! {
                <span>
                    <i style=format!("background:{colour}")></i>
                    {format!("{label} {}", fmt_bytes(*bytes))}
                </span>
            }
        })
        .collect_view();
    let head = match capacity {
        Some(c) => format!(
            "{} of the {} lmgw may use",
            fmt_bytes(b.needed_bytes),
            fmt_bytes(c)
        ),
        None => format!("{} needed", fmt_bytes(b.needed_bytes)),
    };
    let models = format!(
        "{} for the models and {} headroom",
        fmt_bytes(b.total_bytes),
        fmt_bytes(b.headroom_bytes)
    );
    let detail = match (b.verdict.as_str(), capacity, outside) {
        (_, None, _) => format!(
            "{models}; there is nothing to compare it with: {}",
            b.capacity_source
        ),
        ("too_large", Some(c), _) => format!(
            "{models} — {} more than lmgw may use, so the cascade's models evict each other \
             and a turn waits for a load",
            fmt_bytes(b.needed_bytes.saturating_sub(c))
        ),
        ("unknown", Some(_), _) => format!(
            "{models} so far — {} has no figure, so the total is a lower bound",
            b.unknown.join(", ")
        ),
        ("tight", Some(_), Some(o)) => format!(
            "{models}; it fits what lmgw may use, but other programs hold {} now — beside them \
             a start waits for room or falls back",
            fmt_bytes(o)
        ),
        (_, Some(c), Some(o)) => format!(
            "{models}; other programs hold {} now, and {} stays free",
            fmt_bytes(o),
            fmt_bytes(c.saturating_sub(o).saturating_sub(b.needed_bytes))
        ),
        (_, Some(c), None) => format!(
            "{models}; {} of the card stays free for anything else",
            fmt_bytes(c.saturating_sub(b.needed_bytes))
        ),
    };
    let unmeasured = b
        .outside_bytes
        .is_none()
        .then(|| b.outside_note.clone())
        .flatten()
        .map(|why| format!("What other programs hold is not measured: {why}."));
    let rows = b
        .stages
        .iter()
        .map(|s| {
            let bytes = match s.bytes {
                Some(0) if s.placement != "local" => "—".to_string(),
                Some(n) => fmt_bytes(n),
                None => "unknown".to_string(),
            };
            let unknown = s.bytes.is_none();
            view! {
                <tr>
                    <td class="wrap">
                        <span class="rt-stage">
                            <span
                                class="rt-swatch"
                                style=format!("background:{}", stage_colour(&s.stage))
                            ></span>
                            <span>{s.label.clone()}</span>
                        </span>
                    </td>
                    <td class="mono-sm wrap rt-alias">
                        // Breakable after each `/` too (a hyphen already
                        // is): `audio/parakeet-tdt-q8-0` wraps at its parts.
                        {s
                            .alias
                            .clone()
                            .unwrap_or_default()
                            .split_inclusive('/')
                            .map(|p| view! { {p.to_string()} <wbr/> })
                            .collect_view()}
                        {s
                            .running
                            .then(|| {
                                view! {
                                    <span class="chip ok rt-up" title="its container is up">
                                        "up"
                                    </span>
                                }
                            })}
                    </td>
                    <td>{place_label(s)}</td>
                    <td class="num" class:t-warn=unknown>{bytes}</td>
                    <td class="wrap dim rt-note">{s.note.clone()}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <div class="row rt-budget-head">
            <span class=chip title=b.summary.clone()>
                <span class="dot"></span>
                {word}
            </span>
            <span class="rt-budget-line" title=b.capacity_source.clone()>{head}</span>
        </div>
        <div class="rt-bar" role="img" aria-label=b.summary.clone()>
            {bar}
            {outside_seg}
            {cap_mark}
        </div>
        <div class="legend">
            {legend}
            {outside
                .map(|o| {
                    view! {
                        <span>
                            <i class="rt-outside"></i>
                            {format!("Other programs {}", fmt_bytes(o))}
                        </span>
                    }
                })}
        </div>
        <p class="field-hint rt-summary">{detail}</p>
        {unmeasured.map(|u| view! { <p class="field-hint rt-summary">{u}</p> })}
        // Its own sideways scroller for a pane narrower than even the
        // wrapped table, which then scrolls here rather than the page.
        <div class="table-scroll">
        <table class="data rt-stages">
            <thead>
                <tr>
                    <th>"Stage"</th>
                    <th>"Model"</th>
                    <th>"Runs on"</th>
                    <th class="num-h">"Holds"</th>
                    <th>"Where the figure comes from"</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn the_panel_s_figures_are_the_server_s_units() {
        assert_eq!(fmt_bytes(9 * 1024 * 1024 * 1024), "9.0 GiB");
        assert_eq!(fmt_bytes(900 * 1024 * 1024), "900 MiB");
        assert_eq!(fmt_bytes(512), "512 B");
    }

    #[test]
    fn the_budget_is_asked_for_the_draft_s_aliases() {
        let owner = Owner::new();
        owner.with(|| {
            let mut flat = Map::new();
            flat.insert("realtime.default_model".into(), json!("brain"));
            flat.insert("realtime.asr_alias".into(), json!(""));
            flat.insert("realtime.tts_alias".into(), json!("audio/voice"));
            flat.insert("realtime.barge_in_check".into(), json!("words"));
            flat.insert("realtime.barge_in_check_alias".into(), json!(""));
            let form = FormState::new(flat);
            form.set_text("realtime.tts_alias", " audio/other ");
            assert_eq!(
                budget_args(form),
                json!({
                    "default_model": "brain",
                    "asr_alias": "",
                    "tts_alias": "audio/other",
                    "barge_in_check": "words",
                    "barge_in_check_alias": "",
                })
            );
        });
    }
}
