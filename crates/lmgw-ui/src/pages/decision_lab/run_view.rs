//! One cell's full view (labs-store design §2.7): its answers stacked, one
//! view per question; or, for a failed cell, its status and the envelope's
//! message. The footer has the latency, `input_tokens`, the upstream's own
//! `model`, and an amber chip when a fallback answered. "Raw" shows the
//! request and the response as stored, each with an order-keeping pretty
//! toggle (§1.3) that falls back to the stored text with a note when the
//! text does not pretty-print.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::metrics::is_2xx;
use lmgw_api_types::lab_store::LabRunView;
use lmgw_api_types::ordered_json::pretty;
use lmgw_api_types::systemone::SystemOneResponse;

use super::answers::AnswerView;
use crate::fmt::grouped;

/// The answer of a cell that answered 2xx; `Err` says why there is none.
/// A run that is no 2xx is failed, as the server's metrics read it
/// (`decision_lab::metrics`).
pub(super) fn answer_of(run: &LabRunView) -> Result<SystemOneResponse, String> {
    if !is_2xx(i64::from(run.http_status)) {
        return Err(failure_line(run));
    }
    let text = run
        .response
        .as_deref()
        .ok_or_else(|| "the answer was not text".to_string())?;
    serde_json::from_str::<SystemOneResponse>(text).map_err(|e| format!("not an answer: {e}"))
}

/// A failed cell's head line: "HTTP 503 · gpu_hold". A code that only
/// repeats the status (llama.cpp's envelope carries `"code": 500`) is left
/// out.
pub(super) fn failure_line(run: &LabRunView) -> String {
    match &run.error_code {
        Some(c) if *c != run.http_status.to_string() => {
            format!("HTTP {} · {c}", run.http_status)
        }
        _ => format!("HTTP {}", run.http_status),
    }
}

/// The amber chip's words when a fallback answered: "via jev · hold".
pub(super) fn via(run: &LabRunView) -> Option<String> {
    (run.answered_by != run.model_asked).then(|| match &run.fallback_reason {
        Some(r) => format!("via {} · {r}", run.answered_by),
        None => format!("via {}", run.answered_by),
    })
}

/// A stored text with its Pretty toggle.
#[component]
pub(super) fn RawText(label: &'static str, text: String) -> impl IntoView {
    let on = RwSignal::new(false);
    let shown = {
        let text = text.clone();
        move || {
            if !on.get() {
                return (text.clone(), None);
            }
            match pretty(&text) {
                Ok(p) => (p, None),
                Err(e) => (
                    text.clone(),
                    Some(format!("shown as stored: it does not pretty-print ({e})")),
                ),
            }
        }
    };
    view! {
        <div class="dl-raw">
            <div class="dl-raw-head">
                <span class="lab-label">{label}</span>
                <span class="dim mono-sm">{format!("{} bytes", grouped(text.len() as u64))}</span>
                <span class="spacer"></span>
                <label class="dl-check" title="Indent it, every member where it was">
                    <input
                        type="checkbox"
                        prop:checked=move || on.get()
                        on:change=move |ev| on.set(event_target_checked(&ev))
                    />
                    "Pretty"
                </label>
            </div>
            {move || {
                let (t, note) = shown();
                view! {
                    {note.map(|n| view! { <p class="mini-note dl-bad-line">{n}</p> })}
                    <pre class="preset dl-pre">{t}</pre>
                }
            }}
        </div>
    }
}

/// The cell's footer: latency, tokens, the upstream's model, the fallback
/// chip, and the cell's own note.
#[component]
pub(super) fn RunFoot(run: LabRunView, #[prop(into)] note: Option<String>) -> impl IntoView {
    let mut bits = vec![format!("{} ms", grouped(run.latency_ms))];
    if let Some(t) = run.input_tokens {
        bits.push(format!("{} input tokens", grouped(t)));
    }
    if let Some(m) = &run.upstream_model {
        bits.push(m.clone());
    }
    view! {
        <div class="dl-foot">
            <span class="mono-sm dim">{bits.join(" · ")}</span>
            {via(&run)
                .map(|v| {
                    view! {
                        <span class="chip warn" title="A fallback answered: x-lmgw-fallback and its reason">
                            {v}
                        </span>
                    }
                })}
            {note.map(|n| view! { <span class="mini-note dl-bad-line">{n}</span> })}
        </div>
    }
}

/// One cell, in full.
#[component]
pub(super) fn RunView(run: LabRunView, #[prop(into)] note: Option<String>) -> impl IntoView {
    let answer = answer_of(&run);
    let failed = answer.is_err();
    let body = match answer {
        Ok(r) => r
            .answers
            .0
            .into_iter()
            .map(|(qid, a)| {
                let kind = match &a {
                    lmgw_api_types::systemone::Answer::Choice(_) => "choice",
                    lmgw_api_types::systemone::Answer::Score(_) => "score",
                    lmgw_api_types::systemone::Answer::Noul(_) => "noul",
                };
                view! {
                    <div class="dl-answer">
                        <div class="dl-answer-head">
                            <span class="mono-sm">{qid}</span>
                            <span class="dim mono-sm">{kind}</span>
                        </div>
                        <AnswerView a=a/>
                    </div>
                }
            })
            .collect_view()
            .into_any(),
        Err(why) => {
            let message = run.error_message.clone().unwrap_or_default();
            view! {
                <div class="notice err dl-failed">
                    <b>{why}</b>
                    {message}
                </div>
            }
            .into_any()
        }
    };
    let request = run.request.clone();
    let response = run.response.clone();
    view! {
        <div class="dl-run" class:failed=failed>
            {body}
            <RunFoot run=run.clone() note=note/>
            <details class="lab-details">
                <summary>"Raw"</summary>
                <RawText label="request" text=request/>
                {match response {
                    Some(r) => view! { <RawText label="response" text=r/> }.into_any(),
                    None => {
                        view! {
                            <p class="mini-note dim">
                                "The answer was no text: it is kept as an output artifact."
                            </p>
                        }
                            .into_any()
                    }
                }}
            </details>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: u16, response: Option<&str>) -> LabRunView {
        serde_json::from_value(serde_json::json!({
            "id": 1, "group_id": 1, "cell": 0, "variant": "as_written",
            "endpoint": "/v1/systemone", "model_asked": "decide/kev-4b",
            "answered_by": "decide/kev-4b", "fallback_reason": null,
            "upstream_model": null, "http_status": status,
            "error_code": if status == 200 { serde_json::Value::Null } else { "gpu_hold".into() },
            "error_message": null, "request": "{}", "response": response,
            "response_type": null, "headers": {}, "input_tokens": null,
            "output_tokens": null, "latency_ms": 3, "created_at": "", "artifacts": []
        }))
        .unwrap()
    }

    #[test]
    fn an_answer_is_read_in_the_order_sent() {
        let r = run(
            200,
            Some(
                r#"{"model":"laya","answers":{"team":{"type":"choice","choice":"other","probabilities":{"technical":0.1,"billing":0.2,"other":0.7},"confidence":0.6}},"usage":{"input_tokens":10,"output_tokens":0}}"#,
            ),
        );
        let a = answer_of(&r).unwrap();
        let lmgw_api_types::systemone::Answer::Choice(c) = &a.answers.0[0].1 else {
            panic!()
        };
        let keys: Vec<&str> = c.probabilities.keys().collect();
        assert_eq!(keys, ["technical", "billing", "other"]);
    }

    #[test]
    fn a_failure_and_a_fallback_say_so() {
        let r = run(503, Some("{}"));
        assert_eq!(answer_of(&r).unwrap_err(), "HTTP 503 · gpu_hold");
        let mut r = run(500, None);
        r.error_code = Some("500".into());
        assert_eq!(failure_line(&r), "HTTP 500");
        let mut r = run(200, Some("{}"));
        assert!(answer_of(&r).unwrap_err().starts_with("not an answer"));
        assert_eq!(via(&r), None);
        r.answered_by = "jev".into();
        r.fallback_reason = Some("hold".into());
        assert_eq!(via(&r).unwrap(), "via jev · hold");
    }
}
