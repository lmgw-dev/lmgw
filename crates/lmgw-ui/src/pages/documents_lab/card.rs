//! A kept conversion (WP12): its files, when and on which route it was sent
//! and how, what came back, and the request as curl. A group with no run is
//! this page's request in flight (with its job live, for an async one), one
//! in flight elsewhere, one this page knows was not kept, or one stopped.
//!
//! The run: the route's notes on the request's options first (a `not_yet`,
//! `tool_missing` or `model_not_chosen` is what the owner came to see), the
//! docling route's headers decoded, then the files: one file's result
//! whole, several as a list on the left (each with its status) and the
//! chosen one's result on the right. A card is folded to its summary until
//! opened, except the newest, so the history does not fetch every text.
//! The menu: Open in form, Run again, Save texts, Delete.

use leptos::prelude::*;
use lmgw_api_types::documents_lab::{DocumentsLabForm, DOCLING_VARIANT};
use lmgw_api_types::lab_store::{LabExportFormat, LabGroupView, LabMultipartSummary, LabRunView};

use super::super::api_ref::curl::{curl_command, Body, MultipartPart, Prepared};
use super::super::api_ref::identity::Identity;
use super::super::lab_history::{runless, ExportScope, Runless};
use super::job::JobView;
use super::result::{read, FileResult, RunResult};
use super::viewer::{FileView, WarningRow};
use super::{words, Lab};
use crate::fmt::{fmt_bytes, local_datetime};
use crate::pages::docs::download;
use crate::widgets::{MenuItem, RowMenu};

fn when(created_at: &str) -> String {
    let ms = js_sys::Date::parse(created_at);
    if ms.is_nan() {
        created_at.to_string()
    } else {
        local_datetime(ms / 1000.0)
    }
}

/// The run's request as curl: the route on this page's origin, the key as
/// `$LMGW_KEY` (never a real one), each file part as `@<its name>`, each
/// field as sent.
pub(super) fn curl_of(run: &LabRunView, origin: &str) -> String {
    let Ok(s) = serde_json::from_str::<LabMultipartSummary>(&run.request) else {
        return run.request.clone();
    };
    let mut parts: Vec<MultipartPart> = s
        .files
        .iter()
        .map(|f| MultipartPart::File(f.name.clone(), f.filename.clone()))
        .collect();
    parts.extend(
        s.fields
            .iter()
            .map(|f| MultipartPart::Field(f.name.clone(), f.value.clone())),
    );
    curl_command(
        &Prepared {
            method: "POST".into(),
            path: run.endpoint.clone(),
            headers: Vec::new(),
            identity: Identity::Key,
            stream: false,
            body: Body::Multipart {
                parts,
                secret_props: Vec::new(),
            },
        },
        origin,
    )
}

fn origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

/// The card of kept group `id`.
#[component]
pub(super) fn DocCard(lab: Lab, id: i64) -> impl IntoView {
    let group = Memo::new(move |_| {
        lab.history
            .groups
            .with(|v| v.iter().find(|g| g.id == id).cloned())
    });
    // The newest group is open; the others until opened show their summary.
    let first = lab
        .history
        .groups
        .with_untracked(|v| v.first().map(|g| g.id))
        == Some(id);
    let open = RwSignal::new(first);
    let menu = Signal::derive(move || {
        let Some(g) = group.get() else {
            return Vec::new();
        };
        let flying = g.in_flight || lab.live.get() == Some(id);
        let files = lab
            .history
            .export_href(LabExportFormat::Files, &ExportScope::Group(id));
        let texts = g.runs.first().map_or(0, |r| r.artifacts.len());
        let g2 = g.clone();
        vec![
            MenuItem::new("Open in form", move || lab.open_in_form(&g)).title(
                "Put this run's options and files back into the form, to change and run again",
            ),
            MenuItem::new("Run again", move || lab.run_again(&g2))
                .title("The same files and options, as a new run")
                .disabled(flying),
            MenuItem::new("Save texts…", move || download(&files, ""))
                .disabled(texts == 0)
                .title(if texts == 0 {
                    "No text came back to save"
                } else {
                    "Save every text this run made, as a zip"
                }),
            MenuItem::new("Delete", move || lab.history.delete(id))
                .danger()
                .disabled(flying)
                .title(if flying {
                    "In flight: stop it, or delete it once its answer is in"
                } else {
                    "Delete this run, its texts and the files nothing else names"
                }),
        ]
    });
    view! {
        {move || group.get().map(|g| view! { <CardBody lab=lab g=g menu=menu open=open/> })}
    }
}

fn runless_view(lab: Lab, id: i64, state: Runless) -> AnyView {
    let job = lab.jobs.with(|j| j.get(&id).cloned());
    match (state, job) {
        (Runless::Live | Runless::InFlight, Some(job)) => view! { <JobView id=job/> }.into_any(),
        (Runless::Live, None) => view! {
            <div class="dl-status">
                <span class="chip info">
                    <i class="dot"></i>
                    "converting…"
                </span>
                <button class="btn danger sm" on:click=move |_| lab.stop()>
                    "Stop"
                </button>
            </div>
        }
        .into_any(),
        (Runless::InFlight, None) => view! {
            <div class="dl-status">
                <span class="chip info">
                    <i class="dot"></i>
                    "in flight — its answer is on its way"
                </span>
            </div>
        }
        .into_any(),
        (Runless::NotKept(why), _) => view! {
            <div class="dl-status">
                <span class="chip warn">{why}</span>
            </div>
        }
        .into_any(),
        (Runless::Stopped, _) => view! {
            <div class="dl-status">
                <span class="chip warn">"stopped — nothing came back"</span>
            </div>
        }
        .into_any(),
    }
}

#[component]
fn CardBody(
    lab: Lab,
    g: LabGroupView,
    menu: Signal<Vec<MenuItem>>,
    open: RwSignal<bool>,
) -> impl IntoView {
    let cell = g.plan.first().cloned().unwrap_or_default();
    let form = serde_json::from_value::<DocumentsLabForm>(g.form.clone()).unwrap_or_default();
    let output = serde_json::to_value(form.settings.options.output)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let route = match cell.variant.as_str() {
        DOCLING_VARIANT => "docling route",
        "async" => "background job",
        _ => "lmgw's route",
    };
    let run = g.runs.first().cloned();
    let failed = run
        .as_ref()
        .is_some_and(|r| !(200..300).contains(&r.http_status));
    let sub = format!("#{} · {} · {route} · {output}", g.id, when(&g.created_at));
    let inputs: Vec<String> = g
        .artifacts
        .iter()
        .map(|a| format!("{} ({})", a.name, fmt_bytes(a.size)))
        .collect();
    let id = g.id;
    let in_flight = g.in_flight;
    let docling = cell.variant == DOCLING_VARIANT;
    let markdown = output == "markdown";
    let body = match run {
        None => (move || {
            let note = lab.history.notes.with(|n| n.get(&id).cloned());
            runless_view(
                lab,
                id,
                runless(lab.live.get() == Some(id), in_flight, note.as_deref()),
            )
        })
        .into_any(),
        Some(run) => {
            let res = read(&run, docling, markdown);
            view! { <RunBody lab=lab run=run res=res open=open/> }.into_any()
        }
    };
    view! {
        <div class="card result-card doclab-card" class:failed=failed>
            <div class="run-head">
                <span class="run-model dl-title" title=g.title.clone()>
                    {g.title.clone()}
                </span>
                <span class="spacer"></span>
                <RowMenu items=menu title="Run actions"/>
            </div>
            <div class="dl-group-sub mono-sm dim">{sub}</div>
            {(!inputs.is_empty())
                .then(|| view! { <div class="mono-sm dim doclab-inputs">{format!("files: {}", inputs.join(" · "))}</div> })}
            {body}
        </div>
    }
}

/// The docling route's headers, decoded.
fn docling_headers(run: &LabRunView) -> Option<AnyView> {
    let notes = run
        .headers
        .get("x-lmgw-option-notes")
        .map(|h| words::option_notes(h));
    let counts = run
        .headers
        .get("x-lmgw-documents")
        .map(|h| words::counts(h));
    if notes.is_none() && counts.is_none() {
        return None;
    }
    Some(
        view! {
            <div class="doclab-headers">
                {counts
                    .map(|c| {
                        let line = c
                            .into_iter()
                            .map(|(k, v)| format!("{k} {v}"))
                            .collect::<Vec<_>>()
                            .join(" · ");
                        view! {
                            <div>
                                <b>"What was converted: "</b>
                                {line}
                                <span class="mono-sm dim">" x-lmgw-documents"</span>
                            </div>
                        }
                    })}
                {notes
                    .map(|n| {
                        view! {
                            <div>
                                <b>"How the options were handled"</b>
                                <span class="mono-sm dim">" x-lmgw-option-notes"</span>
                            </div>
                            <table class="doclab-notes">
                                {n
                                    .into_iter()
                                    .map(|(name, outcome, detail)| {
                                        view! {
                                            <tr>
                                                <td class="mono-sm">{name}</td>
                                                <td>{words::outcome(&outcome)}</td>
                                                <td class="mono-sm dim">{detail}</td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </table>
                        }
                    })}
            </div>
        }
        .into_any(),
    )
}

/// A file's line in the list: its name and status.
fn file_line(f: &FileResult) -> (String, &'static str, String) {
    (
        f.name.clone(),
        words::status_chip(&f.status),
        words::state(&f.status),
    )
}

#[component]
fn RunBody(lab: Lab, run: LabRunView, res: RunResult, open: RwSignal<bool>) -> impl IntoView {
    let ok = (200..300).contains(&run.http_status);
    let failure = (!ok).then(|| {
        let mut s = format!("HTTP {}", run.http_status);
        if let Some(c) = &run.error_code {
            s.push_str(&format!(" · {c}"));
        }
        if let Some(m) = &run.error_message {
            s.push_str(&format!(": {m}"));
        }
        s
    });
    let curl = curl_of(&run, &origin());
    let raw = run.response.clone();
    let summary = res.summary.map(|s| {
        format!(
            "{}: {} converted, {} partly, {} failed, {} skipped",
            words::count(s.files, "file", "files"),
            s.converted,
            s.partial,
            s.failed,
            s.skipped
        )
    });
    let tools = (!res.tools.is_empty()).then(|| {
        format!(
            "programs used: {}",
            res.tools
                .iter()
                .map(|t| format!("{} {}", t.id, t.version))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let docling = docling_headers(&run);
    let notes = res.notes.clone();
    let zip = res.zip.clone();
    let files = res.files.clone();
    let n = files.len();
    let chosen = RwSignal::new(0usize);
    let lines: Vec<(String, &'static str, String)> = files.iter().map(file_line).collect();
    let files = StoredValue::new(files);
    let expert = lab.expert;
    let copy_curl = {
        let curl = curl.clone();
        move |_| {
            let c = curl.clone();
            let toasts = lab.toasts;
            leptos::task::spawn_local(async move {
                match lmgw_ui_kit::clipboard::write_text(&c).await {
                    Ok(()) => toasts.ok("curl copied"),
                    Err(e) => toasts.err(format!("Not copied: {e}.")),
                }
            });
        }
    };
    let chips = lines.clone();
    let files_class = if n > 1 {
        "doclab-files several"
    } else {
        "doclab-files"
    };
    view! {
        {failure.map(|f| view! { <div class="notice err">{f}</div> })}
        {(!notes.is_empty())
            .then(|| {
                view! {
                    <div class="doclab-notes-box">
                        <div class="doclab-notes-title">"What the route said about the options"</div>
                        {notes.into_iter().map(|w| view! { <WarningRow w=w/> }).collect_view()}
                    </div>
                }
            })}
        {docling}
        <div class="doclab-summary mono-sm dim">
            {summary.map(|s| view! { <span>{s}</span> })}
            {res
                .processing_time
                .map(|t| {
                    view! {
                        <span>
                            {format!("processing time {t:.2} s ")}
                            <span class="dim">"processing_time"</span>
                        </span>
                    }
                })}
            {tools.map(|t| view! { <span>{t}</span> })}
        </div>
        {zip
            .map(|z| {
                let (href, name, size) = match z {
                    super::result::Text::Kept { href, name, size } => (href, name, size),
                    super::result::Text::Inline(_) => (String::new(), String::new(), 0),
                };
                view! {
                    <div class="doclab-zip">
                        <span>{format!("{name} · {} · as a docling client gets it", fmt_bytes(size))}</span>
                        <button class="btn ghost sm" on:click=move |_| download(&href, &name)>
                            "Save zip…"
                        </button>
                    </div>
                }
            })}
        <Show
            when=move || open.get()
            fallback=move || {
                let chips = chips.clone();
                view! {
                    <div class="doclab-chips">
                        {chips
                            .into_iter()
                            .enumerate()
                            .map(|(i, (name, cls, state))| {
                                let tip = format!("{name}: {state} — show its result");
                                view! {
                                    <button
                                        class="doclab-chip"
                                        title=tip
                                        on:click=move |_| {
                                            chosen.set(i);
                                            open.set(true);
                                        }
                                    >
                                        <span class=cls>{state}</span>
                                        <span class="doclab-file-name">{name.clone()}</span>
                                    </button>
                                }
                            })
                            .collect_view()}
                    </div>
                }
            }
        >
            {
                let lines = lines.clone();
                view! {
                    <div class=files_class>
                        {(n > 1)
                            .then(|| {
                                let lines = lines.clone();
                                view! {
                                    <div class="doclab-file-list" role="listbox">
                                        {lines
                                            .into_iter()
                                            .enumerate()
                                            .map(|(i, (name, cls, state))| {
                                                view! {
                                                    <button
                                                        class="doclab-file-pick"
                                                        class:active=move || chosen.get() == i
                                                        role="option"
                                                        aria-selected=move || (chosen.get() == i).to_string()
                                                        on:click=move |_| chosen.set(i)
                                                    >
                                                        <span class="doclab-file-name" title=name.clone()>{name.clone()}</span>
                                                        <span class=cls>{state}</span>
                                                    </button>
                                                }
                                            })
                                            .collect_view()}
                                    </div>
                                }
                            })}
                        <div class="doclab-file-pane">
                            {move || {
                                let i = chosen.get();
                                files
                                    .with_value(|f| f.get(i).cloned())
                                    .map(|f| view! { <FileView f=f expert=expert/> })
                            }}
                        </div>
                    </div>
                    <button class="btn ghost sm doclab-fold" on:click=move |_| open.set(false)>
                        "Fold"
                    </button>
                }
            }
        </Show>
        <div class="lab-stats mono-sm dim">
            <span>{format!("{} ms in the gateway · HTTP {}", run.latency_ms, run.http_status)}</span>
        </div>
        {run.id.is_none().then(|| view! { <div class="notice warn">"not kept: this answer is the page's only copy"</div> })}
        <details class="lab-details">
            <summary>"The request as curl"</summary>
            <div class="doclab-curl">
                <pre class="preset">{curl}</pre>
                <button class="btn ghost sm" on:click=copy_curl>"Copy"</button>
            </div>
        </details>
        {move || {
            (expert.get() || !ok)
                .then(|| {
                    raw.clone()
                        .map(|t| {
                            view! {
                                <details class="lab-details">
                                    <summary>"The answer as kept (texts as their files)"</summary>
                                    <pre class="preset">{t}</pre>
                                </details>
                            }
                        })
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents_lab::CONVERT_ENDPOINT;

    #[test]
    fn the_curl_names_the_files_and_fields_and_never_a_key() {
        let run: LabRunView = serde_json::from_value(serde_json::json!({
            "id": 3, "group_id": 1, "cell": 0, "variant": "native",
            "endpoint": CONVERT_ENDPOINT, "model_asked": "documents/convert",
            "answered_by": "documents/convert", "fallback_reason": null, "upstream_model": null,
            "http_status": 200, "error_code": null, "error_message": null,
            "request": r#"{"fields":[{"name":"output","value":"json"}],"files":[{"name":"file","filename":"example report.pdf","type":"application/pdf","bytes":10,"artifact":"lab-artifact:2"}]}"#,
            "response": null, "response_type": null, "headers": {}, "input_tokens": null,
            "output_tokens": null, "latency_ms": 12, "created_at": "", "artifacts": []
        }))
        .unwrap();
        let c = curl_of(&run, "http://127.0.0.1:8787");
        assert!(
            c.contains("\"http://127.0.0.1:8787/v1/documents/convert\""),
            "{c}"
        );
        assert!(c.contains("-F 'file=@\"example report.pdf\"'"), "{c}");
        assert!(c.contains("--form-string 'output=json'"), "{c}");
        assert!(c.contains("Bearer $LMGW_KEY"), "{c}");
    }
}
