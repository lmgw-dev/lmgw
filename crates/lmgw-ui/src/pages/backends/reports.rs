//! What the build ops report, drawn the same way wherever they appear: the
//! Resolve preview (editor), the Check merge report (editor and the Builds
//! row), and a run's verify result (the after-run panel).

use leptos::prelude::*;
use lmgw_api_types::builds::{
    CheckMergeReport, EditApplied, EditRole, MergeOutcome, ResolvedPreview, VerifyReport,
};
use serde_json::Value;

use super::sha7;
use crate::widgets::CopyBtn;

fn role_label(r: EditRole) -> &'static str {
    match r {
        EditRole::Ccache => "ccache",
        EditRole::Cache => "cache",
        EditRole::BuildInfo => "build info",
        EditRole::Qualify => "qualify",
        EditRole::BaseImage => "base image",
        EditRole::Other => "other",
    }
}

/// One line of a find/replace, for a table cell: the first line, with a
/// marker when there is more (the full text is in the tooltip).
fn first_line(s: &str) -> String {
    let mut lines = s.lines();
    let first = lines.next().unwrap_or("").to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

/// How one edit fared against the resolved Dockerfile, as a chip:
/// `(class, label, tooltip)`. `None` when the preview did not say (an older
/// server).
pub fn edit_outcome_chip(o: Option<&EditApplied>) -> Option<(&'static str, String, &'static str)> {
    let o = o?;
    Some(match (o.applied, o.required) {
        (0, true) => (
            "chip err",
            "not matched".to_string(),
            "required, and its find text is not in the Dockerfile: a run fails here",
        ),
        (0, false) => (
            "chip warn",
            "not matched".to_string(),
            "its find text is not in the Dockerfile: a run skips it",
        ),
        (1, _) => ("chip ok", "applied".to_string(), "matched once"),
        (n, _) => (
            "chip ok",
            format!("applied {n}\u{d7}"),
            "matched more than once: every occurrence is replaced",
        ),
    })
}

/// `build_resolve`'s answer: what the build would be if it ran now.
#[component]
pub fn ResolvedView(p: ResolvedPreview) -> impl IntoView {
    let edits = p.edits.clone();
    let has_edits = !edits.is_empty();
    let outcomes = p.edit_outcomes.clone();
    let has_outcomes = !outcomes.is_empty();
    let unmatched_required = outcomes
        .iter()
        .filter(|o| o.required && o.applied == 0)
        .count();
    let args = p.build_args.clone();
    view! {
        <div class="bk-report">
            <For each=move || p.warnings.clone() key=|w| w.clone() let:w>
                <div class="notice warn">{w}</div>
            </For>
            <div class="bk-facts">
                <span title=p.base_sha.clone()>
                    "base " <code class="mono-sm">{sha7(&p.base_sha)}</code>
                </span>
                {p.build_number.map(|n| view! { <span>"build " <b>{format!("b{n}")}</b></span> })}
                <span>"Dockerfile " <code class="mono-sm">{p.dockerfile.clone()}</code></span>
                <span>"target " <code class="mono-sm">{p.target.clone()}</code></span>
                {(!p.profile.is_empty())
                    .then(|| view! { <span>"profile " <code class="mono-sm">{p.profile.clone()}</code></span> })}
            </div>
            <div class="bk-tags">
                <div class="copy-line" title="Follows the build: every successful run moves it">
                    <span class="dim">"moving"</span>
                    <code class="mono-sm">{p.moving_tag.clone()}</code>
                    <CopyBtn text=p.moving_tag.clone()/>
                </div>
                <div class="copy-line" title="This exact image: base, extras and config">
                    <span class="dim">"this run"</span>
                    <code class="mono-sm">{p.immutable_tag.clone()}</code>
                    <CopyBtn text=p.immutable_tag.clone()/>
                </div>
            </div>
            {(!args.is_empty())
                .then(|| {
                    let line = args
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("  ");
                    view! {
                        <div class="bk-facts">
                            <span class="dim">"build args"</span>
                            <code class="mono-sm">{line}</code>
                        </div>
                    }
                })}
            {has_edits
                .then(|| {
                    view! {
                        <table class="data bk-sub bk-edits-t">
                            <thead>
                                <tr>
                                    <th>"Edit"</th>
                                    <th>"Role"</th>
                                    <th>"Find"</th>
                                    <th>"Replace"</th>
                                    <th title="Against the base's Dockerfile: a preview merges no extra, so one that changes the Dockerfile can still move a run's result">
                                        "Matched"
                                    </th>
                                </tr>
                            </thead>
                            <tbody>
                                {edits
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, e)| {
                                        let o = outcomes
                                            .iter()
                                            .find(|o| o.index as usize == i)
                                            .or_else(|| outcomes.get(i));
                                        let chip = edit_outcome_chip(o);
                                        let req = e.required;
                                        view! {
                                            <tr>
                                                <td class="mono-sm">
                                                    {if e.name.is_empty() { "—".to_string() } else { e.name.clone() }}
                                                </td>
                                                <td><span class="type-badge">{role_label(e.role)}</span></td>
                                                <td class="mono-sm bk-cut" title=e.find.clone()>{first_line(&e.find)}</td>
                                                <td class="mono-sm bk-cut" title=e.replace.clone()>
                                                    {first_line(&e.replace)}
                                                </td>
                                                <td>
                                                    {match chip {
                                                        Some((c, l, t)) => {
                                                            view! { <span class=c title=t><span class="dot"></span>{l}</span> }
                                                                .into_any()
                                                        }
                                                        None => {
                                                            view! { <span class="dim">{if req { "required" } else { "" }}</span> }
                                                                .into_any()
                                                        }
                                                    }}
                                                </td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </tbody>
                        </table>
                    }
                })}
            {(has_outcomes && unmatched_required > 0)
                .then(|| {
                    view! {
                        <div class="notice err">
                            {format!(
                                "{} required edit{} did not match the base's Dockerfile: a run would stop at it. Customize the edits under Advanced, or pick another Dockerfile.",
                                unmatched_required,
                                if unmatched_required == 1 { "" } else { "s" },
                            )}
                        </div>
                    }
                })}
        </div>
    }
}

fn outcome_chip(o: MergeOutcome) -> impl IntoView {
    let (class, label, title) = match o {
        MergeOutcome::Merged => ("chip ok", "merged", "merges cleanly"),
        MergeOutcome::AlreadyInBase => (
            "chip off",
            "already in base",
            "its changes are already in the base — it adds nothing",
        ),
        MergeOutcome::MergedUpstream => (
            "chip off",
            "merged upstream",
            "the forge says it was merged — skipped; drop it from the extras?",
        ),
        MergeOutcome::SquashApplied => (
            "chip ok",
            "squash-applied",
            "no shared history with the base: its changes since its fork point are applied as one commit",
        ),
        MergeOutcome::Conflict => ("chip err", "conflict", "does not merge — see the files"),
    };
    view! { <span class=class title=title><span class="dot"></span>{label}</span> }
}

/// `build_check_merge`'s answer: each extra in order, merged onto the base.
#[component]
pub fn MergeReportView(r: CheckMergeReport) -> impl IntoView {
    let head = if r.steps.is_empty() {
        "No extras: the build is the base ref as it is.".to_string()
    } else if r.ok {
        "Every extra merges cleanly.".to_string()
    } else {
        "A conflict stops the run at that extra.".to_string()
    };
    view! {
        <div class="bk-report">
            <div class=if r.ok { "wiz-ok" } else { "wiz-err" } style="margin-top:0">
                {head} " Base " <code class="mono-sm" title=r.base_sha.clone()>{sha7(&r.base_sha)}</code> "."
            </div>
            {(!r.steps.is_empty())
                .then(|| {
                    view! {
                        <table class="data bk-sub bk-merge-t">
                            <thead>
                                <tr>
                                    <th>"Extra"</th>
                                    <th>"Outcome"</th>
                                    <th>"Commit"</th>
                                    <th>"Note"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {r
                                    .steps
                                    .into_iter()
                                    .map(|s| {
                                        let files = s.files.join("\n");
                                        let has_files = !s.files.is_empty();
                                        view! {
                                            <tr>
                                                <td class="mono-sm" title=s.label.clone()>{s.label.clone()}</td>
                                                <td>{outcome_chip(s.outcome)}</td>
                                                <td class="mono-sm" title=s.sha.clone()>{sha7(&s.sha)}</td>
                                                <td class="wrap">
                                                    {s.note.clone()}
                                                    {has_files
                                                        .then(|| {
                                                            view! { <pre class="preset bk-files">{files.clone()}</pre> }
                                                        })}
                                                </td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </tbody>
                        </table>
                    }
                })}
        </div>
    }
}

/// A run's verify result. The run row stores it as JSON "shaped by the
/// executor" (`BuildRun.verify`); when it has [`VerifyReport`]'s shape it is
/// drawn as one, otherwise shown as it is.
#[component]
pub fn VerifyView(v: Value) -> impl IntoView {
    match serde_json::from_value::<VerifyReport>(v.clone()) {
        Ok(r) if v.get("gpu_verified").is_some() || v.get("help_ok").is_some() => {
            verify_report(r).into_any()
        }
        _ => {
            let text = serde_json::to_string_pretty(&v).unwrap_or_default();
            view! { <pre class="preset">{text}</pre> }.into_any()
        }
    }
}

pub fn verify_report(r: VerifyReport) -> impl IntoView {
    let devices = if r.devices.is_empty() {
        "no device listed".to_string()
    } else {
        r.devices.join(", ")
    };
    view! {
        <div class="bk-facts">
            <span class=if r.help_ok { "status-ok" } else { "status-err" }>
                {if r.help_ok { "--help answers" } else { "--help failed" }}
            </span>
            <span class=if r.gpu_verified { "status-ok" } else { "status-warn" }>
                {if r.gpu_verified { "GPU-verified" } else { "not GPU-verified" }}
            </span>
            <span class="dim">{devices}</span>
        </div>
        {r
            .notes
            .into_iter()
            .map(|n| view! { <div class="dim mini-note">{n}</div> })
            .collect_view()}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_outcomes_read_as_applied_or_not_matched() {
        let o = |applied, required| EditApplied {
            applied,
            required,
            ..Default::default()
        };
        assert_eq!(edit_outcome_chip(None), None);
        assert_eq!(edit_outcome_chip(Some(&o(1, true))).unwrap().1, "applied");
        assert_eq!(
            edit_outcome_chip(Some(&o(3, false))).unwrap().1,
            "applied 3\u{d7}"
        );
        let (c, l, _) = edit_outcome_chip(Some(&o(0, true))).unwrap();
        assert_eq!((c, l.as_str()), ("chip err", "not matched"));
        assert_eq!(
            edit_outcome_chip(Some(&o(0, false))).unwrap().0,
            "chip warn"
        );
    }

    #[test]
    fn a_multiline_edit_shows_its_first_line_and_says_there_is_more() {
        assert_eq!(first_line("RUN a\nRUN b"), "RUN a …");
        assert_eq!(first_line("RUN a"), "RUN a");
        assert_eq!(first_line(""), "");
    }
}
