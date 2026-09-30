//! The behaviour probes (benchmark design §5): one line per probe with its
//! outcome and the one-line why, folding out the responses it judged; and
//! the same probes side by side for Compare.

use leptos::prelude::*;
use lmgw_api_types::bench::{ProbeEvidence, ProbeKind, ProbeOutcome, ProbeResult};
use lmgw_api_types::bench_ops::BenchRun;

pub fn probe_label(k: ProbeKind) -> &'static str {
    match k {
        ProbeKind::Chat => "Chat",
        ProbeKind::ThinkingOff => "Thinking off",
        ProbeKind::ThinkingOn => "Thinking on",
        ProbeKind::ToolCall => "Tool call",
        ProbeKind::JsonSchema => "JSON schema",
        ProbeKind::Deterministic => "Deterministic",
        ProbeKind::Vision => "Vision",
        ProbeKind::ReasoningHistory => "Reasoning history",
        ProbeKind::Needle => "Needle",
    }
}

/// What the probe asks and what passes (§5's table).
pub fn probe_about(k: ProbeKind) -> &'static str {
    match k {
        ProbeKind::Chat => "a plain chat: 200, non-empty content, finish_reason stop",
        ProbeKind::ThinkingOff => {
            "enable_thinking=false: no reasoning_content (and no <think> block in the content), non-empty content"
        }
        ProbeKind::ThinkingOn => "enable_thinking=true: non-empty reasoning_content",
        ProbeKind::ToolCall => "one get_weather call whose arguments parse as an object with city",
        ProbeKind::JsonSchema => {
            "response_format json_schema: the content parses and has the required typed fields"
        }
        ProbeKind::Deterministic => {
            "the same seeded temperature-0 request twice with cache reuse and twice without: all four identical passes; only the cached pair differing is info; the uncached pair differing fails"
        }
        ProbeKind::Vision => "a solid red 64×64 PNG, \"what colour, one word\": the answer contains red",
        ProbeKind::ReasoningHistory => {
            "info: whether an earlier assistant turn's reasoning_content survives /apply-template"
        }
        ProbeKind::Needle => {
            "a passcode at the start of a context-filling prompt (P_max − 512 tokens): the answer contains it"
        }
    }
}

pub fn outcome_words(o: ProbeOutcome) -> (&'static str, &'static str) {
    match o {
        ProbeOutcome::Pass => ("chip ok", "pass"),
        ProbeOutcome::Fail => ("chip err", "fail"),
        ProbeOutcome::Info => ("chip info", "info"),
        ProbeOutcome::Skipped => ("chip off", "skipped"),
        ProbeOutcome::Error => ("chip err", "error"),
    }
}

pub fn outcome_chip(o: ProbeOutcome, title: String) -> AnyView {
    let (class, words) = outcome_words(o);
    view! {
        <span class=class title=title>
            <span class="dot"></span>
            {words}
        </span>
    }
    .into_any()
}

/// The probes of one run, in suite order.
#[component]
pub fn ProbesList(probes: Vec<ProbeResult>) -> impl IntoView {
    if probes.is_empty() {
        return view! { <div class="empty">"No probes in this run."</div> }.into_any();
    }
    view! {
        <table class="data bk-sub bn-probes-t">
            <tbody>
                {probes.into_iter().map(|p| view! { <ProbeRow p=p/> }).collect_view()}
            </tbody>
        </table>
    }
    .into_any()
}

#[component]
fn ProbeRow(p: ProbeResult) -> impl IntoView {
    let open = RwSignal::new(false);
    let has_evidence = !p.evidence.is_empty();
    let evidence = StoredValue::new(p.evidence.clone());
    let n = p.evidence.len();
    view! {
        <tr>
            <td class="bn-probe-name" title=probe_about(p.probe)>{probe_label(p.probe)}</td>
            <td class="bn-probe-out">{outcome_chip(p.outcome, String::new())}</td>
            <td class="clip" title=p.detail.clone()>{p.detail.clone()}</td>
            <td class="actions">
                {has_evidence
                    .then(|| {
                        view! {
                            <button
                                type="button"
                                class="btn ghost sm"
                                aria-expanded=move || open.get().to_string()
                                on:click=move |_| open.update(|o| *o = !*o)
                            >
                                {move || {
                                    if open.get() {
                                        "Hide".to_string()
                                    } else if n == 1 {
                                        "Evidence".to_string()
                                    } else {
                                        format!("Evidence ({n})")
                                    }
                                }}
                            </button>
                        }
                    })}
            </td>
        </tr>
        <Show when=move || open.get()>
            <tr class="detail-row">
                <td colspan="4">
                    <div class="bn-evidence">
                        {evidence
                            .get_value()
                            .into_iter()
                            .enumerate()
                            .map(|(i, e)| view! { <EvidenceView i=i n=n e=e/> })
                            .collect_view()}
                    </div>
                </td>
            </tr>
        </Show>
    }
}

#[component]
fn EvidenceView(i: usize, n: usize, e: ProbeEvidence) -> impl IntoView {
    let mut head = Vec::new();
    if n > 1 {
        head.push(format!("response {} of {n}", i + 1));
    }
    head.push(match e.status {
        Some(s) => format!("HTTP {s}"),
        None => "no HTTP status".to_string(),
    });
    if let Some(ms) = e.ms {
        head.push(super::fmt_ms(ms as f64));
    }
    if let Some(f) = &e.finish_reason {
        head.push(format!("finish {f}"));
    }
    let block = |label: &'static str, text: Option<String>| {
        text.filter(|t| !t.is_empty()).map(|t| {
            view! {
                <div class="bn-ev-k">{label}</div>
                <pre class="preset bn-ev-pre">{t}</pre>
            }
        })
    };
    let tools = e
        .tool_calls
        .as_ref()
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default());
    view! {
        <div class="bn-ev">
            <div class="bn-ev-head dim">{head.join(" · ")}</div>
            {block("content", e.content.clone())}
            {block("reasoning", e.reasoning.clone())}
            {block("tool calls", tools)}
            {block("body", e.body.clone())}
        </div>
    }
}

/// The probes of several runs side by side: a row per probe any of them
/// ran, a column per run, the outcome's why in its tooltip.
#[component]
pub fn ProbesSide(runs: Vec<(String, String, BenchRun)>) -> impl IntoView {
    let kinds: Vec<ProbeKind> = ProbeKind::ALL
        .into_iter()
        .filter(|k| {
            runs.iter()
                .any(|(_, _, r)| r.probes.probes.iter().any(|p| p.probe == *k))
        })
        .collect();
    if kinds.is_empty() {
        return view! { <div class="empty">"None of these runs ran the probes."</div> }.into_any();
    }
    let heads = runs
        .iter()
        .map(|(label, color, _)| {
            view! {
                <th>
                    <i class="slot-swatch" style=format!("background:{color}")></i>
                    {label.clone()}
                </th>
            }
        })
        .collect_view();
    let rows = kinds
        .into_iter()
        .map(|k| {
            let cells = runs
                .iter()
                .map(
                    |(_, _, r)| match r.probes.probes.iter().find(|p| p.probe == k) {
                        Some(p) => view! { <td>{outcome_chip(p.outcome, p.detail.clone())}</td> }
                            .into_any(),
                        None => view! { <td class="dim">"not run"</td> }.into_any(),
                    },
                )
                .collect_view();
            view! {
                <tr>
                    <td title=probe_about(k)>{probe_label(k)}</td>
                    {cells}
                </tr>
            }
        })
        .collect_view();
    view! {
        <div class="table-scroll">
            <table class="data bk-sub bn-probes-side">
                <thead>
                    <tr>
                        <th>"Probe"</th>
                        {heads}
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
    .into_any()
}
