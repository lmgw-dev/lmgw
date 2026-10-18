//! A kept generation or edit (labs-store design §5): its prompt, when and on
//! which model it was asked, what came back (the pictures, served from the
//! lab store, or the route's refusal as it said it) and the request that
//! made it. A group with no run is this page's request in flight (with
//! Stop), another's in flight, one this page knows was not kept, or one that
//! was stopped. The card's menu: Open in form, Save (the picture under a
//! readable name, several as a zip), Delete (not while it is in flight);
//! each picture can be saved or sent to the edit panel as the kept file it
//! is, without an upload.

use leptos::prelude::*;
use lmgw_api_types::image_lab::EDIT_VARIANT;
use lmgw_api_types::lab_store::{
    LabArtifactRole, LabArtifactView, LabExportFormat, LabFileRef, LabGroupView,
    LabMultipartSummary, LabRunView,
};
use serde_json::Value;

use super::super::lab_history::{runless, ExportScope, Runless};
use super::Lab;
use crate::fmt::{human_bytes, local_datetime};
use crate::pages::docs::download;
use crate::widgets::{MenuItem, RowMenu};

/// One picture of a run: where it is, what to call it, how big.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Picture {
    pub src: String,
    pub name: String,
    pub bytes: u64,
    /// The kept file, which the edit panel can name without an upload.
    pub kept: Option<LabFileRef>,
}

/// `png` | `jpeg` | `webp` → the media type of a `data:` URL. Unknown formats
/// are relayed as given: a build that grows a fourth encoder should show its
/// picture, not a broken one.
pub(super) fn media_type(fmt: &str) -> String {
    match fmt {
        "" => "image/png".to_string(),
        "jpg" => "image/jpeg".to_string(),
        other => format!("image/{other}"),
    }
}

/// A run's pictures: its kept output files, or, for a run the store could
/// not keep, the `b64_json` still inline in its answer.
pub(super) fn pictures(run: &LabRunView) -> Vec<Picture> {
    let kept: Vec<&LabArtifactView> = run
        .artifacts
        .iter()
        .filter(|a| a.role == LabArtifactRole::Output && a.mime.starts_with("image/"))
        .collect();
    if !kept.is_empty() {
        return kept
            .into_iter()
            .map(|a| Picture {
                src: a.href.clone(),
                name: a.name.clone(),
                bytes: a.size,
                kept: Some(LabFileRef {
                    id: a.id,
                    name: a.name.clone(),
                }),
            })
            .collect();
    }
    if run.id.is_some() {
        return Vec::new();
    }
    let Some(v) = run
        .response
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
    else {
        return Vec::new();
    };
    let mime = media_type(v["output_format"].as_str().unwrap_or_default());
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, d)| {
            let b64 = d["b64_json"].as_str().filter(|s| !s.is_empty())?;
            Some(Picture {
                src: format!("data:{mime};base64,{b64}"),
                name: format!("image-{i}.png"),
                // base64 is 4 characters per 3 bytes, padding included.
                bytes: (b64.len() / 4 * 3 - b64.chars().rev().take_while(|c| *c == '=').count())
                    as u64,
                kept: None,
            })
        })
        .collect()
}

/// The request as the card shows it: a generation's body pretty-printed in
/// its own order, an edit's fields and files one to a line.
pub(super) fn request_text(run: &LabRunView) -> String {
    if run.endpoint.ends_with("/edits") {
        if let Ok(s) = serde_json::from_str::<LabMultipartSummary>(&run.request) {
            let fields = s.fields.iter().map(|f| format!("{} = {}", f.name, f.value));
            let files = s.files.iter().map(|f| {
                format!(
                    "{} = {} ({}){}",
                    f.name,
                    f.filename,
                    human_bytes(f.bytes as u64),
                    f.artifact
                        .as_ref()
                        .map(|a| format!(" · {a}"))
                        .unwrap_or_default()
                )
            });
            return fields.chain(files).collect::<Vec<_>>().join("\n");
        }
    }
    lmgw_api_types::ordered_json::pretty(&run.request).unwrap_or_else(|_| run.request.clone())
}

/// A failed run's line: its status, the code when it says more than the
/// status, the message.
pub(super) fn failure_line(run: &LabRunView) -> String {
    let mut s = format!("HTTP {}", run.http_status);
    if let Some(c) = run
        .error_code
        .as_ref()
        .filter(|c| **c != run.http_status.to_string())
    {
        s.push_str(&format!(" · {c}"));
    }
    if let Some(m) = &run.error_message {
        s.push_str(&format!(": {m}"));
    }
    s
}

fn when(created_at: &str) -> String {
    let ms = js_sys::Date::parse(created_at);
    if ms.is_nan() {
        created_at.to_string()
    } else {
        local_datetime(ms / 1000.0)
    }
}

/// The card of kept group `id`.
#[component]
pub(super) fn ImageCard(lab: Lab, id: i64) -> impl IntoView {
    let group = Memo::new(move |_| {
        lab.history
            .groups
            .with(|v| v.iter().find(|g| g.id == id).cloned())
    });
    let menu = Signal::derive(move || {
        let Some(g) = group.get() else {
            return Vec::new();
        };
        let flying = g.in_flight || lab.live.get() == Some(id);
        let files = lab
            .history
            .export_href(LabExportFormat::Files, &ExportScope::Group(id));
        let shots = g.runs.first().map_or(0, |r| {
            r.artifacts
                .iter()
                .filter(|a| a.role == LabArtifactRole::Output)
                .count()
        });
        vec![
            MenuItem::new("Open in form", move || lab.open_in_form(&g))
                .title("Put this ask's model, form and pictures back into the form"),
            MenuItem::new(
                if shots == 1 {
                    "Save picture…"
                } else {
                    "Save pictures…"
                },
                move || download(&files, ""),
            )
            .disabled(shots == 0)
            .title(match shots {
                0 => "No picture came back to save",
                1 => "Save the picture under a readable name",
                _ => "Save the pictures under readable names, as a zip",
            }),
            MenuItem::new("Delete", move || lab.history.delete(id))
                .danger()
                .disabled(flying)
                .title(if flying {
                    "In flight: stop it, or delete it once its answer is in"
                } else {
                    "Delete this ask, its run and the files nothing else names"
                }),
        ]
    });
    view! {
        {move || group.get().map(|g| view! { <CardBody lab=lab g=g menu=menu/> })}
    }
}

/// The answer whose group was deleted while it came (a Clear): its card,
/// with why it was not kept, until the next run. Only Open in form is left
/// of its menu: there is nothing in the store to export or delete.
#[component]
pub(super) fn StrayCard(lab: Lab) -> impl IntoView {
    move || {
        lab.stray.get().map(|(g, why)| {
            let menu = {
                let g = g.clone();
                Signal::derive(move || {
                    let g = g.clone();
                    vec![MenuItem::new("Open in form", move || lab.open_in_form(&g))
                        .title("Put this ask's model and form back into the form")]
                })
            };
            view! {
                <div class="notice warn">{why}</div>
                <CardBody lab=lab g=g menu=menu/>
            }
        })
    }
}

/// What a group with no run shows: the page's own request with Stop,
/// another's in flight, not kept and why, or stopped.
fn runless_view(lab: Lab, state: Runless) -> AnyView {
    match state {
        Runless::Live => view! {
            <div class="dl-status">
                <span class="chip live">
                    <i class="dot"></i>
                    "rendering…"
                </span>
                <button class="btn danger sm" on:click=move |_| lab.stop()>
                    "Stop"
                </button>
            </div>
        }
        .into_any(),
        Runless::InFlight => view! {
            <div class="dl-status">
                <span class="chip live">
                    <i class="dot"></i>
                    "in flight — its answer is on its way"
                </span>
            </div>
        }
        .into_any(),
        Runless::NotKept(why) => view! {
            <div class="dl-status">
                <span class="chip warn">{why}</span>
            </div>
        }
        .into_any(),
        Runless::Stopped => view! {
            <div class="dl-status">
                <span class="chip warn">"stopped — nothing came back"</span>
            </div>
        }
        .into_any(),
    }
}

#[component]
fn CardBody(lab: Lab, g: LabGroupView, menu: Signal<Vec<MenuItem>>) -> impl IntoView {
    let cell = g.plan.first().cloned().unwrap_or_default();
    let route = if cell.variant == EDIT_VARIANT {
        "edit"
    } else {
        "generate"
    };
    let run = g.runs.first().cloned();
    let failed = run
        .as_ref()
        .is_some_and(|r| !(200..300).contains(&r.http_status));
    let sub = format!(
        "#{} · {} · {} · {route}",
        g.id,
        when(&g.created_at),
        cell.model
    );
    let inputs: Vec<String> = g
        .artifacts
        .iter()
        .map(|a| format!("{} ({})", a.name, human_bytes(a.size)))
        .collect();
    let id = g.id;
    let in_flight = g.in_flight;
    let body = match run {
        None => (move || {
            let note = lab.history.notes.with(|n| n.get(&id).cloned());
            runless_view(
                lab,
                runless(lab.live.get() == Some(id), in_flight, note.as_deref()),
            )
        })
        .into_any(),
        Some(run) => view! { <RunBody lab=lab run=run/> }.into_any(),
    };
    view! {
        <div class="card result-card il-group" class:failed=failed>
            <div class="run-head">
                <span class="run-model dl-title" title=g.title.clone()>
                    {g.title.clone()}
                </span>
                <span class="spacer"></span>
                <RowMenu items=menu title="Ask actions"/>
            </div>
            <div class="dl-group-sub mono-sm dim">{sub}</div>
            {(!inputs.is_empty())
                .then(|| {
                    view! { <div class="mono-sm dim il-inputs">{format!("edited: {}", inputs.join(" · "))}</div> }
                })}
            {body}
        </div>
    }
}

#[component]
fn RunBody(lab: Lab, run: LabRunView) -> impl IntoView {
    let ok = (200..300).contains(&run.http_status);
    let shots = pictures(&run);
    let request = request_text(&run);
    let headers = run
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join(" · ");
    let can_edit = lab.can_edit();
    let failure = (!ok).then(|| failure_line(&run));
    let no_shots = shots.is_empty();
    let empty = ok && no_shots;
    let raw = run.response.clone();
    view! {
        {failure.map(|f| view! { <div class="notice err">{format!("⚠ {f}")}</div> })}
        {empty.then(|| view! { <div class="notice err">"⚠ the answer carried no picture"</div> })}
        <div class="gallery" hidden=no_shots>
            {shots
                .into_iter()
                .map(|s| {
                    let kept = s.kept.clone();
                    view! {
                        <figure class="shot">
                            <img src=s.src.clone() alt=s.name.clone()/>
                            <figcaption class="meta">
                                <a class="btn ghost" href=s.src.clone() download=s.name.clone()>
                                    "Save…"
                                </a>
                                {(can_edit && kept.is_some())
                                    .then(|| {
                                        let kept = kept.clone();
                                        view! {
                                            <button
                                                class="btn ghost il-send-edit"
                                                title="Edit this picture: the kept file, no upload"
                                                on:click=move |_| {
                                                    if let Some(k) = kept.clone() {
                                                        lab.send_to_edit(k);
                                                    }
                                                }
                                            >
                                                "Send to edit"
                                            </button>
                                        }
                                    })}
                                <span class="spacer" style="flex:1"></span>
                                <span class="dim mono-sm">{human_bytes(s.bytes)}</span>
                            </figcaption>
                        </figure>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
        <div class="lab-stats mono-sm dim">
            <span>{format!("{} ms in the gateway · HTTP {}", run.latency_ms, run.http_status)}</span>
            {(!headers.is_empty()).then(|| view! { <span>{headers.clone()}</span> })}
        </div>
        {run.id.is_none().then(|| view! { <div class="notice warn">"not kept: this answer is the page's only copy"</div> })}
        <details class="lab-details">
            <summary>"The request that made it"</summary>
            <pre class="preset">{request}</pre>
        </details>
        {(!ok)
            .then(|| {
                raw.map(|t| {
                    view! {
                        <details class="lab-details">
                            <summary>"The answer as the route sent it"</summary>
                            <pre class="preset">{t}</pre>
                        </details>
                    }
                })
            })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(response: Option<&str>, id: Option<i64>) -> LabRunView {
        serde_json::from_value(serde_json::json!({
            "id": id, "group_id": 1, "cell": 0, "variant": "generate",
            "endpoint": "/v1/images/generations", "model_asked": "image/z",
            "answered_by": "image/z", "fallback_reason": null, "upstream_model": null,
            "http_status": 200, "error_code": null, "error_message": null,
            "request": "{\"prompt\":\"a cat\",\"model\":\"image/z\"}",
            "response": response, "response_type": "application/json", "headers": {},
            "input_tokens": null, "output_tokens": null, "latency_ms": 12,
            "created_at": "", "artifacts": []
        }))
        .unwrap()
    }

    #[test]
    fn an_unkept_run_shows_its_inline_pictures_and_a_kept_one_its_files() {
        let unkept = run(
            Some(r#"{"output_format":"webp","data":[{"b64_json":"AAAA"}]}"#),
            None,
        );
        let p = pictures(&unkept);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].src, "data:image/webp;base64,AAAA");
        assert_eq!(p[0].bytes, 3);
        assert!(p[0].kept.is_none());
        let kept = run(Some(r#"{"data":[{"b64_json":"lab-artifact:9"}]}"#), Some(4));
        assert!(pictures(&kept).is_empty(), "the refs are no pictures");
        assert_eq!(
            request_text(&kept),
            "{\n  \"prompt\": \"a cat\",\n  \"model\": \"image/z\"\n}"
        );
    }

    #[test]
    fn a_failure_line_leaves_out_a_code_that_repeats_the_status() {
        let mut r = run(None, Some(1));
        r.http_status = 500;
        r.error_code = Some("500".into());
        r.error_message = Some("boom".into());
        assert_eq!(failure_line(&r), "HTTP 500: boom");
        r.error_code = Some("upstream".into());
        assert_eq!(failure_line(&r), "HTTP 500 · upstream: boom");
        assert_eq!(media_type("jpg"), "image/jpeg");
    }
}
