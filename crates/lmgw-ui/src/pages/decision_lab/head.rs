//! The head line (labs-store design §2.2): the model picker over every model
//! that answers `/v1/systemone`, then what produces the errors a request can
//! meet — `laya · non-causal · ctx 8192 · ubatch 8192 (row) · CPU, 16 threads
//! · text only · stopped · Reload` — each chip's tooltip naming its source.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{DecisionBackend, DecisionLabModel};

use super::page::Lab;
use crate::catalog::CatalogEntry;
use crate::widgets::ModelPicker;

/// How a chip reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Plain,
    Warn,
    Err,
}

/// One fact of the head line.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Fact {
    pub text: String,
    /// Where it comes from.
    pub title: String,
    pub tone: Tone,
}

fn fact(text: impl Into<String>, title: impl Into<String>) -> Fact {
    Fact {
        text: text.into(),
        title: title.into(),
        tone: Tone::Plain,
    }
}

/// The head line's facts for `m`, in reading order.
pub(super) fn facts(m: &DecisionLabModel) -> Vec<Fact> {
    let mut out = Vec::new();
    match &m.facts {
        Some(f) => {
            if let Some(e) = &f.header_error {
                out.push(Fact {
                    text: "header unreadable".into(),
                    title: format!("The GGUF header could not be read: {e}"),
                    tone: Tone::Err,
                });
            }
            if let Some(t) = &f.decision_type {
                out.push(fact(t.clone(), "decision.type, from the GGUF header"));
            }
            if let Some(s) = f.shape {
                out.push(fact(
                    s.label(),
                    "The header's shape: what has to fit in one pass, and so which limit applies",
                ));
            }
            match (f.n_ctx_slot, f.ctx_size) {
                (Some(n), _) => out.push(fact(
                    format!("ctx {n}"),
                    "One slot's context, as the running server's /props says",
                )),
                (None, Some(n)) => out.push(fact(format!("ctx {n}"), "The row's ctx_size")),
                (None, None) => out.push(fact(
                    "ctx default",
                    "The row sets no ctx_size: llama-server's default applies",
                )),
            }
            out.push(fact(
                format!("ubatch {} ({})", f.ubatch, f.ubatch_source.label()),
                format!(
                    "The ubatch in effect: {}. The batch in effect is {} ({}).",
                    f.ubatch_source.label(),
                    f.batch,
                    f.batch_source.label()
                ),
            ));
            if let Some(h) = f.max_head_tokens {
                out.push(fact(
                    format!("head {h}"),
                    "decision.max_head_tokens, from the header: past it llama.cpp truncates a question and its options",
                ));
            }
            out.push(match (f.backend, f.threads_in_effect) {
                (DecisionBackend::Cpu, Some(t)) => fact(
                    format!("CPU, {t} threads"),
                    format!("backend cpu; threads {}", f.threads_source.label()),
                ),
                (DecisionBackend::Cpu, None) => {
                    fact("CPU", "backend cpu; llama-server picks the threads")
                }
                (DecisionBackend::Gpu, _) => fact("GPU", "backend gpu: the GPU hold pauses it"),
            });
        }
        None if !m.local => {
            out.push(fact(
                format!("cloud · {}", m.owner),
                "A cloud alias or catalog entry of task decision",
            ));
            if let Some(c) = m.context_length {
                out.push(fact(format!("ctx {c}"), "The catalog's context_length"));
            }
        }
        None => {}
    }
    out.push(fact(
        if m.reads_images {
            "reads images"
        } else {
            "text only"
        },
        if m.local {
            "The running server's vision, else an image among the input modalities its projector declares"
        } else {
            "Its capabilities' input modalities"
        },
    ));
    if m.held {
        out.push(Fact {
            text: match &m.hold_fallback {
                Some(f) => format!("held — answers through {f}"),
                None => "held — no fallback: 503".into(),
            },
            title: "The GPU hold (or a benchmark's lease) pauses this row now".into(),
            tone: Tone::Warn,
        });
    }
    out
}

/// The container state's chip: (class, words).
pub(super) fn state_chip(local: bool, state: Option<&str>) -> (&'static str, &'static str) {
    match state {
        Some("ready") => ("chip ok", "ready"),
        Some("starting") => ("chip live", "starting…"),
        Some("stopping") => ("chip live", "stopping…"),
        Some(_) => ("chip off", "stopped"),
        None if !local => ("chip off", "remote"),
        None => ("chip off", "stopped"),
    }
}

/// The picker's list: the lab's models, local rows first as the server
/// lists them.
pub(super) fn entries(models: &[DecisionLabModel]) -> Vec<CatalogEntry> {
    models
        .iter()
        .map(|m| CatalogEntry {
            id: m.name.clone(),
            source: if m.local {
                "lmgw".into()
            } else {
                m.owner.clone()
            },
            group_label: if m.local {
                "Local decision rows".into()
            } else {
                m.owner.clone()
            },
            vendor: None,
            task: Some("decision".into()),
            ctx: m.context_length,
            price_in: None,
            price_out: None,
            price_varies: false,
            vision: Some(m.reads_images),
            tools: false,
            reasoning: false,
            reasoning_facts: None,
            local: m.local,
            routable: true,
        })
        .collect()
}

/// The head model a fresh page starts on: one that is running, else a CPU
/// row, else a cloud model, else the first — so opening the lab never
/// suggests starting a GPU row.
pub(super) fn first_pick(models: &[DecisionLabModel]) -> Option<String> {
    let cpu = |m: &&DecisionLabModel| {
        m.facts
            .as_ref()
            .is_some_and(|f| f.backend == DecisionBackend::Cpu)
    };
    models
        .iter()
        .find(|m| m.state.as_deref() == Some("ready"))
        .or_else(|| models.iter().find(cpu))
        .or_else(|| models.iter().find(|m| !m.local))
        .or_else(|| models.first())
        .map(|m| m.name.clone())
}

#[component]
pub(super) fn Head(lab: Lab) -> impl IntoView {
    let list = Signal::derive(move || entries(&lab.models.get()));
    let status = Signal::derive(move || lab.list.get());
    view! {
        <span class="lab-label">"Model"</span>
        <ModelPicker value=lab.alias entries=list status=status/>
        <span class="dl-facts">
            {move || {
                lab.model()
                    .map(|m| {
                        facts(&m)
                            .into_iter()
                            .map(|f| {
                                let cls = match f.tone {
                                    Tone::Plain => "dl-fact",
                                    Tone::Warn => "dl-fact warn",
                                    Tone::Err => "dl-fact err",
                                };
                                view! {
                                    <span class=cls title=f.title>
                                        {f.text}
                                    </span>
                                }
                            })
                            .collect_view()
                    })
            }}
        </span>
        <span class="spacer"></span>
        {move || {
            lab.model()
                .map(|m| {
                    let state = lab.state_of(&m);
                    let (cls, label) = state_chip(m.local, state.as_deref());
                    let title = if m.local {
                        "The row's container. A count or an ask on a stopped CPU row starts it; on a GPU row that is your call."
                    } else {
                        "A cloud model: nothing of lmgw's to start"
                    };
                    view! {
                        <span class=cls title=title>
                            <i class="dot"></i>
                            {label}
                        </span>
                    }
                })
        }}
        <button
            class="btn ghost"
            title="Read the model list and its facts again"
            on:click=move |_| lab.reload_models()
        >
            "Reload"
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::aux_engine::AuxThreadsSource;
    use lmgw_api_types::decision_lab::{CountSource, DecisionFacts, DecisionShape};

    fn laya() -> DecisionLabModel {
        DecisionLabModel {
            name: "decide/laya".into(),
            owner: "lmgw".into(),
            local: true,
            model_id: Some("laya".into()),
            facts: Some(DecisionFacts {
                header_error: None,
                decision_type: Some("laya".into()),
                max_head_tokens: Some(192),
                shape: Some(DecisionShape::NonCausal),
                ctx_size: Some(8192),
                n_ctx_slot: None,
                ubatch: 8192,
                ubatch_source: CountSource::Row,
                batch: 8192,
                batch_source: CountSource::Ubatch,
                backend: DecisionBackend::Cpu,
                threads_in_effect: Some(16),
                threads_source: AuxThreadsSource::Cores,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_head_line_reads_like_the_design() {
        let line: Vec<String> = facts(&laya()).into_iter().map(|f| f.text).collect();
        assert_eq!(
            line,
            [
                "laya",
                "non-causal",
                "ctx 8192",
                "ubatch 8192 (row)",
                "head 192",
                "CPU, 16 threads",
                "text only"
            ]
        );
    }

    #[test]
    fn a_running_slot_and_a_hold_say_so() {
        let mut m = laya();
        if let Some(f) = m.facts.as_mut() {
            f.n_ctx_slot = Some(4096);
            f.backend = DecisionBackend::Gpu;
        }
        m.held = true;
        m.hold_fallback = Some("jev".into());
        let f = facts(&m);
        assert!(f
            .iter()
            .any(|f| f.text == "ctx 4096" && f.title.contains("/props")));
        assert!(f.iter().any(|f| f.text == "GPU"));
        let held = f.last().unwrap();
        assert_eq!(held.text, "held — answers through jev");
        assert_eq!(held.tone, Tone::Warn);
        m.hold_fallback = None;
        assert_eq!(facts(&m).last().unwrap().text, "held — no fallback: 503");
    }

    #[test]
    fn a_cloud_model_shows_its_catalog() {
        let m = DecisionLabModel {
            name: "openrouter/jev".into(),
            owner: "openrouter".into(),
            context_length: Some(32768),
            ..Default::default()
        };
        let line: Vec<String> = facts(&m).into_iter().map(|f| f.text).collect();
        assert_eq!(line, ["cloud · openrouter", "ctx 32768", "text only"]);
        assert_eq!(state_chip(false, None).1, "remote");
        assert_eq!(state_chip(true, None).1, "stopped");
        assert_eq!(state_chip(true, Some("ready")), ("chip ok", "ready"));
    }

    #[test]
    fn a_fresh_page_starts_on_a_running_or_cpu_row_never_a_cold_gpu_row() {
        let mut kev = laya();
        kev.name = "decide/kev-4b".into();
        if let Some(f) = kev.facts.as_mut() {
            f.backend = DecisionBackend::Gpu;
        }
        let cloud = DecisionLabModel {
            name: "openrouter/jev".into(),
            ..Default::default()
        };
        let list = vec![kev.clone(), cloud.clone(), laya()];
        assert_eq!(first_pick(&list).as_deref(), Some("decide/laya"));
        assert_eq!(
            first_pick(&[kev.clone(), cloud]).as_deref(),
            Some("openrouter/jev")
        );
        let mut ready = kev.clone();
        ready.state = Some("ready".into());
        assert_eq!(
            first_pick(&[laya(), ready]).as_deref(),
            Some("decide/kev-4b")
        );
        assert_eq!(first_pick(&[kev]).as_deref(), Some("decide/kev-4b"));
        assert_eq!(first_pick(&[]), None);
    }
}
