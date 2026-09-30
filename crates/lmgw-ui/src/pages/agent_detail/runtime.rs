use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AgentProvenance, AgentRuntime};
use serde_json::{json, Value};

use crate::pages::agents::short_ts;
use crate::widgets::{use_toasts, Explain, Section};

// ---------------------------------------------------------------------------
// The batch run surface (§6.2)
// ---------------------------------------------------------------------------

/// One limit, printed the way §4.1 asks for: the value, or the word for what
/// `0` means there. Never a number lmgw invented — an owner who set `0` gets
/// "unlimited", not a default printed as though they had chosen it.
pub fn limit_text(value: u64, unit: &str, zero: &str) -> String {
    if value == 0 {
        zero.to_string()
    } else {
        format!("{value} {unit}")
    }
}

/// Which phases have their `output` event checked against a declared schema,
/// and which do not (§4.1, per-phase `run.output`).
///
/// "unvalidated" is printed rather than omitted: a phase whose output nothing
/// checks is a fact the owner should have to read, not one they have to infer
/// from an absent row.
pub fn output_line(r: &AgentRuntime) -> String {
    if r.phases.is_empty() {
        return "no phases declared".to_string();
    }
    r.phases
        .iter()
        .map(|p| {
            let state = if r.output_validated.iter().any(|v| v == p) {
                "validated"
            } else {
                "unvalidated"
            };
            format!("{p}: {state}")
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The bounds line under Apply for a container agent (§7): memory, CPU and the
/// deadline, the three the owner can actually feel.
pub fn bounds_line(r: &AgentRuntime) -> String {
    let cpus = if r.cpus <= 0.0 {
        "no CPU quota".to_string()
    } else {
        format!("{} CPU", r.cpus)
    };
    format!(
        "Bounds: {}, {cpus}, {}.",
        limit_text(r.memory_mb, "MB", "no memory limit"),
        limit_text(r.deadline_seconds, "s", "no deadline"),
    )
}

/// The provenance lines, in the order they answer "where did this come from?"
/// — and empty when the row was never installed from an image.
pub fn provenance_rows(p: &AgentProvenance) -> Vec<(String, String)> {
    // "installed from" is a claim about where this row's *document* came from,
    // and only an install can make it. A row whose digest was filled in by
    // Pull image alone (an authored manifest naming an image) says the honest,
    // smaller thing instead.
    let installed = !p.installed_at.is_empty();
    let mut out = vec![(
        if installed { "installed from" } else { "image" }.to_string(),
        p.image.clone(),
    )];
    out.push((
        "digest".to_string(),
        if p.digest.is_empty() {
            "podman could not report one".to_string()
        } else {
            p.digest.clone()
        },
    ));
    if !p.manifest_path.is_empty() {
        out.push(("manifest".to_string(), p.manifest_path.clone()));
    }
    if installed {
        out.push(("installed".to_string(), short_ts(&p.installed_at)));
    }
    // Only when it is a *different* fact: an install sets both, and printing
    // the same timestamp twice would read as two events.
    if !p.pulled_at.is_empty() && p.pulled_at != p.installed_at {
        out.push(("last pulled".to_string(), short_ts(&p.pulled_at)));
    }
    out
}

/// The Runtime block folded to one line: the image, then the bounds a run is
/// held to, in the table's order — or, for a row that only has a package,
/// where it was installed from.
pub fn runtime_summary(r: Option<&AgentRuntime>, p: Option<&AgentProvenance>) -> String {
    match r {
        Some(r) => {
            let cpus = if r.cpus <= 0.0 {
                "no CPU quota".to_string()
            } else {
                format!("{} CPU", r.cpus)
            };
            let mut parts = vec![
                r.image.clone(),
                limit_text(r.memory_mb, "MB", "no memory limit"),
                cpus,
                limit_text(r.deadline_seconds, "s", "no deadline"),
                if r.read_only { "read-only" } else { "writable" }.to_string(),
            ];
            if !r.podman {
                parts.push("podman missing".to_string());
            }
            parts.join(" · ")
        }
        None => p
            .map(|p| format!("installed from {}", p.image))
            .unwrap_or_default(),
    }
}

/// The Runtime block on the Run tab (§7): the image the owner is about to
/// start, the pull policy that is never implicit, every limit with its value
/// printed rather than applied behind their back — and, for a row installed
/// from an image (§3.4), where the document came from plus the two acts that
/// keep it current: **Pull image** and **Re-import from image**.
///
/// Folded to its one-line summary: it is read before a first run and looked
/// up after, never scanned on every visit.
#[component]
pub(super) fn RuntimeBlock(
    id: String,
    runtime: Option<AgentRuntime>,
    /// Where this row was installed from. `None` for a pasted manifest — the
    /// package half of the block is simply absent then.
    provenance: Option<AgentProvenance>,
    reload: Callback<()>,
) -> impl IntoView {
    if runtime.is_none() && provenance.is_none() {
        return None;
    }
    let agent_id = StoredValue::new(id);
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    // The pull's own answer, kept on the page rather than in a toast that
    // disappears: "the digest moved and the manifest differs" is the one thing
    // this button exists to say, and it comes with a decision to make.
    let pulled = RwSignal::new(None::<String>);
    let offer_reimport = RwSignal::new(false);

    let press = move |reimport: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        pulled.set(None);
        spawn_local(async move {
            let op = if reimport {
                "agent_reimport"
            } else {
                "agent_pull"
            };
            let res = crate::api::post::<Value, _>(
                &format!("/api/op/{op}"),
                &json!({ "id": agent_id.get_value() }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    let message = v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("done")
                        .to_string();
                    let note = v
                        .get("note")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    offer_reimport.set(v["manifest_differs"] == json!(true));
                    pulled.set(Some(if note.is_empty() {
                        message.clone()
                    } else {
                        format!("{message}. {note}")
                    }));
                    toasts.ok(message);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let summary = runtime_summary(runtime.as_ref(), provenance.as_ref());
    let title = if runtime.is_some() {
        "Runtime"
    } else {
        "Package"
    };
    let prov_rows = StoredValue::new(provenance.as_ref().map(provenance_rows).unwrap_or_default());
    let has_package = provenance.is_some();
    let from_package = provenance
        .as_ref()
        .is_some_and(|p| !p.installed_at.is_empty());
    let podman = runtime.as_ref().map(|r| {
        if r.podman {
            (true, "podman is available".to_string())
        } else {
            (false, format!("podman is NOT available: {}", r.podman_note))
        }
    });
    let limits = StoredValue::new(
        runtime
            .map(|r| {
                vec![
                    ("image".to_string(), r.image.clone()),
                    ("pull".into(), r.pull.clone()),
                    ("phases".into(), r.phases.join(", ")),
                    (
                        "memory".into(),
                        limit_text(r.memory_mb, "MB", "unlimited (0)"),
                    ),
                    (
                        "cpus".into(),
                        if r.cpus <= 0.0 {
                            "unlimited (0)".to_string()
                        } else {
                            r.cpus.to_string()
                        },
                    ),
                    ("pids".into(), limit_text(r.pids, "", "unlimited (0)")),
                    (
                        "deadline".into(),
                        limit_text(r.deadline_seconds, "s", "unbounded (0)"),
                    ),
                    (
                        "stop grace".into(),
                        // `0` here is *stricter*, not looser, and the page
                        // says so rather than calling it unlimited (§4.1).
                        limit_text(
                            r.stop_grace_seconds,
                            "s",
                            "0 — SIGKILL at once, no chance to flush",
                        ),
                    ),
                    (
                        "filesystem".into(),
                        if r.read_only {
                            "read-only, with a tmpfs on /tmp".to_string()
                        } else {
                            "writable".to_string()
                        },
                    ),
                    ("output".into(), output_line(&r)),
                ]
            })
            .unwrap_or_default(),
    );
    let podman = StoredValue::new(podman);

    Some(view! {
        <div class="card run-rt">
            <Section
                title=title
                summary=Signal::derive(move || summary.clone())
                persist="agents.runtime"
                default_open=false
            >
                <Show when=move || limits.with_value(|l| !l.is_empty())>
                    <table class="data kv-table">
                        <tbody>
                            <For each=move || limits.get_value() key=|(k, _)| k.clone() let:row>
                                <tr>
                                    <td class="dim mono-sm">{row.0}</td>
                                    <td class="mono-sm clip" title=row.1.clone()>{row.1.clone()}</td>
                                </tr>
                            </For>
                        </tbody>
                    </table>
                    {podman
                        .get_value()
                        .map(|(ok, line)| {
                            view! { <p class=if ok { "dim mono-sm" } else { "notice err" }>{line}</p> }
                        })}
                    <Explain summary="0 means no limit — except stop grace." persist="agents.explain.bounds">
                        "Every bound is a manifest field with its value printed here. 0 means no limit "
                        "everywhere except stop grace, where it means SIGKILL at once."
                    </Explain>
                </Show>
                <Show when=move || has_package>
                    <div class="mini-head">"Package"</div>
                    <Explain
                        summary=if from_package {
                            "Installed from an image that carries its manifest."
                        } else {
                            "Written here; only the image below has been pulled."
                        }
                        persist="agents.explain.package"
                    >
                        "Pull asks podman for that image again and says whether it moved; "
                        "re-import reads the manifest out of it and keeps this row's config. "
                        "Nothing polls in the background."
                    </Explain>
                    <table class="data kv-table">
                        <tbody>
                            <For each=move || prov_rows.get_value() key=|(k, _)| k.clone() let:row>
                                <tr>
                                    <td class="dim mono-sm">{row.0}</td>
                                    <td class="mono-sm clip" title=row.1.clone()>{row.1.clone()}</td>
                                </tr>
                            </For>
                        </tbody>
                    </table>
                    {move || pulled.get().map(|m| view! { <div class="notice">{m}</div> })}
                    <div class="run-acts">
                        <button
                            class="btn ghost sm"
                            disabled=move || busy.get()
                            title="podman pull, and a digest comparison afterwards"
                            on:click=move |_| press(false)
                        >
                            {move || if busy.get() { "Working…" } else { "Pull image" }}
                        </button>
                        <button
                            class=move || {
                                if offer_reimport.get() { "btn primary sm" } else { "btn ghost sm" }
                            }
                            disabled=move || busy.get()
                            title="read the manifest out of the image again, keeping the config"
                            on:click=move |_| press(true)
                        >
                            "Re-import from image"
                        </button>
                    </div>
                </Show>
            </Section>
        </div>
    })
}
