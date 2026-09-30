use leptos::prelude::*;
use lmgw_api_types::{AgentBatchShape, AgentField};
use serde_json::Value;

use super::*;
use crate::widgets::schema_form::{is_mount, Draft};
use crate::widgets::{ConfirmButton, Section, Select};

/// The run on screen, in one line: its state, how far it got, what it cost,
/// and the way out (Cancel while it runs, Close after).
#[component]
pub(super) fn RunStrip(ctx: RunCtx) -> impl IntoView {
    let run = ctx.run;
    let currency = ctx.currency;
    let has_run = Memo::new(move |_| run.summary.with(Option::is_some));
    // Whether Cancel or Close is offered changes twice a run, not with every
    // progress frame: the button is rebuilt only then, so a press on it is
    // never lost to a re-render between mousedown and mouseup.
    let running = Memo::new(move |_| run.summary.with(|s| s.as_ref().is_some_and(is_live)));
    // Close drops the reviewer's unticks and changed answers: with any, it
    // asks first (code:A1). Rebuilt when that flips, not per edit.
    let edited = Memo::new(move |_| run.edits() > 0);
    let head = move || {
        run.summary.get().map(|r| {
            let pct = r.percent.unwrap_or(0);
            let has_total = r.total.is_some();
            view! {
                <span class=status_chip(&r.status)>
                    <span class="dot"></span>
                    {r.status.clone()}
                </span>
                <span class="mono-sm">{run_line(&r)}</span>
                <span class="dim mono-sm">{format!("run #{} · {}", r.job_id, r.phase)}</span>
                // No invented denominator: the bar only fills once the source
                // step has said how many there are — and only while it is
                // still moving.
                {(has_total && is_live(&r))
                    .then(|| {
                        view! {
                            <div class="progress run-progress">
                                <i style=format!("width:{pct}%")></i>
                            </div>
                        }
                    })}
            }
        })
    };
    let failure = move || {
        run.summary.with(|r| {
            r.as_ref()
                .filter(|r| r.status == "failed")
                .map(|r| r.error.clone().unwrap_or_else(|| "the run failed".into()))
        })
    };
    view! {
        <Show when=move || has_run.get()>
            <div class="run-strip">
                {head}
                {move || {
                    run.result
                        .with(|r| r.as_ref().and_then(|r| cost_text(r, &currency.get_value())))
                        .map(|c| {
                            let tip = c.clone();
                            view! { <span class="dim mono-sm run-cost" title=tip>{c}</span> }
                        })
                }}
                <span class="spacer"></span>
                {move || {
                    if running.get() {
                        view! {
                            <button class="btn ghost sm" on:click=move |_| ctx.cancel()>
                                "Cancel"
                            </button>
                        }
                            .into_any()
                    } else if edited.get() {
                        view! {
                            <ConfirmButton
                                label="Close"
                                confirm=move || {
                                    format!("Drop {}?", crate::fmt::count_of(run.edits(), "edits"))
                                }
                                title="back to the config; your unticks and changed answers are dropped (the run stays listed under Runs)"
                                on_confirm=Callback::new(move |()| run.clear())
                            />
                        }
                            .into_any()
                    } else {
                        view! {
                            <button
                                class="btn ghost sm"
                                title="back to the config; the run stays listed under Runs"
                                on:click=move |_| run.clear()
                            >
                                "Close"
                            </button>
                        }
                            .into_any()
                    }
                }}
            </div>
            {move || failure().map(|e| view! { <div class="notice err">{e}</div> })}
        </Show>
    }
}

/// Under the review: what Apply did, and the run log — each folded to its
/// heading and count until asked for.
#[component]
pub(super) fn ReviewFoot(run: RunState) -> impl IntoView {
    let applied = Memo::new(move |_| {
        run.result
            .with(|r| r.as_ref().and_then(|r| r.get("applied").cloned()))
    });
    let log_n = Signal::derive(move || run.log.with(Vec::len));
    let any = move || applied.with(Option::is_some) || log_n.get() != 0;
    let has_log = move || log_n.get() != 0;
    view! {
        <Show when=any>
            <div class="review-foot">
                {move || {
                    applied
                        .get()
                        .map(|a| {
                            let calls = a["tool_calls"].as_array().map(Vec::len).unwrap_or(0);
                            let a = StoredValue::new(a);
                            view! {
                                <Section
                                    title="Applied"
                                    count=Signal::derive(move || calls.to_string())
                                    default_open=true
                                >
                                    <AppliedBody a=a.get_value()/>
                                </Section>
                            }
                        })
                }}
                // Where an event that was refused says so (container-runtime
                // §3.2): a row with no id, a `type` this build does not know,
                // an undeclared column, a line of stdout that was not JSON.
                // None of those fail a run, so without this they would be
                // decisions taken about the owner's data with nothing on
                // screen to show for them.
                <Show when=has_log>
                    <Section
                        title="Run log"
                        count=Signal::derive(move || log_n.get().to_string())
                        persist="agents.runlog"
                        default_open=false
                    >
                        <pre class="mono-sm run-log">{move || run.log.get().join("\n")}</pre>
                    </Section>
                </Show>
            </div>
        </Show>
    }
}

/// What the apply step called, and what it said.
#[component]
fn AppliedBody(a: Value) -> impl IntoView {
    let calls = a["tool_calls"].as_array().cloned().unwrap_or_default();
    let output = serde_json::to_string_pretty(&a["output"]).unwrap_or_default();
    view! {
        // Only when the apply step was a turn that actually called something:
        // an empty table with three headings is noise, not a report.
        {(!calls.is_empty())
            .then(|| {
                view! {
                    <table class="data">
                        <thead>
                            <tr>
                                <th>"Tool"</th>
                                <th>"Result"</th>
                                <th class="num-h">"ms"</th>
                            </tr>
                        </thead>
                        <tbody>
                            {calls
                                .into_iter()
                                .map(|c| {
                                    let ok = c["ok"].as_bool().unwrap_or(false);
                                    view! {
                                        <tr>
                                            <td class="mono-sm clip">
                                                {c["name"].as_str().unwrap_or("?").to_string()}
                                            </td>
                                            <td>
                                                <span class=if ok { "chip ok" } else { "chip err" }>
                                                    <span class="dot"></span>
                                                    {if ok { "ok" } else { "failed" }}
                                                </span>
                                            </td>
                                            <td class="num">{c["ms"].as_u64().unwrap_or(0).to_string()}</td>
                                        </tr>
                                    }
                                })
                                .collect_view()}
                        </tbody>
                    </table>
                }
            })}
        <pre class="mono-sm run-log">{output}</pre>
    }
}

/// The start summary's mount lines (mounts §5.8), under the Start buttons.
///
/// From the **form**, not from the stored config, for the same reason the
/// buttons read it: a start uses the values as they stand at the click, so the
/// folder picked a second ago is the one that would be bound. Unbound slots
/// print nothing — there is no mount to name — while the `keep-id` note prints
/// whenever a mount field is *declared*, because that is what decides how the
/// container runs (§5.5).
#[component]
pub(super) fn MountSummary(
    #[prop(into)] fields: Signal<Vec<AgentField>>,
    draft: Draft,
) -> impl IntoView {
    let declared = Signal::derive(move || fields.get().iter().any(is_mount));
    let lines = Signal::derive(move || {
        fields
            .get()
            .iter()
            .filter(|f| is_mount(f))
            .filter_map(|f| {
                let host = draft.text(&f.name);
                let host = host.trim();
                (!host.is_empty()).then(|| mount_line(f, host))
            })
            .collect::<Vec<_>>()
    });
    view! {
        <Show when=move || declared.get()>
            <div class="dim mono-sm" style="margin-top:10px">
                <For each=move || lines.get() key=|l: &String| l.clone() let:line>
                    <div>{line}</div>
                </For>
                <div>{KEEP_ID_NOTE}</div>
            </div>
        </Show>
    }
}

#[component]
pub(super) fn ReviewHead(
    shape: RwSignal<Option<AgentBatchShape>>,
    /// See [`column_widths`]: the table is laid out from this row.
    widths: Memo<Vec<ColumnFit>>,
    /// Every row the filter shows is ticked.
    #[prop(into)]
    all_checked: Signal<bool>,
    /// Tick (`true`) or untick every row the filter shows.
    on_all: Callback<bool>,
) -> impl IntoView {
    let columns = Signal::derive(move || {
        let w = widths.get();
        shape
            .get()
            .map(|s| s.columns)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                // The wrapping column takes what the others leave. (No
                // percentage here: a fixed table reads a `calc()` mixing one
                // as `auto`.)
                let style = match w.get(i).copied().unwrap_or(ColumnFit::Wrap) {
                    ColumnFit::Fit(n) | ColumnFit::Clip(n) => {
                        Some(format!("width:calc({n}ch + 22px)"))
                    }
                    ColumnFit::Date => Some(format!("width:calc({DATE_WIDTH}ch + 22px)")),
                    ColumnFit::Wrap => None,
                };
                (c, style)
            })
            .collect::<Vec<_>>()
    });
    let editable = Signal::derive(move || {
        shape
            .get()
            .map(|s| s.editable.into_iter().map(|e| e.field).collect::<Vec<_>>())
            .unwrap_or_default()
    });
    view! {
        <thead>
            <tr>
                <th class="pick">
                    <input
                        type="checkbox"
                        title="tick or untick every row the filter shows"
                        prop:checked=move || all_checked.get()
                        on:change=move |ev| on_all.run(event_target_checked(&ev))
                    />
                </th>
                // Keyed on the width too: a run that brings longer values
                // re-lays the column out.
                <For each=move || columns.get() key=|c| c.clone() let:c>
                    <th style=c.1>{c.0}</th>
                </For>
                <For each=move || editable.get() key=|f| f.clone() let:f>
                    <th class="review-edit">{f}</th>
                </For>
            </tr>
        </thead>
    }
}

#[component]
pub(super) fn ReviewRowView(
    r: ReviewRow,
    run: RunState,
    shape: RwSignal<Option<AgentBatchShape>>,
    open: Callback<ReviewRow>,
    /// How each review column is laid out (see [`column_widths`]).
    widths: Memo<Vec<ColumnFit>>,
) -> impl IntoView {
    // Only an attention row explains itself under its first cell; a settled
    // row's reply is its answer column, and the whole of it is in the details.
    let note = r
        .attention
        .then(|| attention_note(r.error.as_deref(), r.raw.as_deref()))
        .flatten();
    let attention = r.attention;
    let cells = r.cells.clone();
    let answers = r.answers.clone();
    // The checkbox and the pickers read and write `run`, never a signal of
    // their own: this view is rebuilt whenever the row's content changes, and
    // the reviewer's decisions have to outlive that.
    let row_id = StoredValue::new(r.id.clone());
    let row = StoredValue::new(r);
    let fit = move |i: usize| widths.with(|w| w.get(i).copied().unwrap_or(ColumnFit::Wrap));
    view! {
        <tr class:attn=attention>
            <td class="pick">
                <input
                    type="checkbox"
                    prop:checked=move || run.is_checked(&row_id.get_value())
                    on:change=move |ev| {
                        run.set_checked(&row_id.get_value(), event_target_checked(&ev))
                    }
                />
            </td>
            {cells
                .into_iter()
                .enumerate()
                .map(|(i, (_, text))| {
                    let title = text.clone();
                    // A date column shows the short local form; the header's
                    // own text is the tooltip.
                    let shown = match (fit(i), mail_date(&text)) {
                        (ColumnFit::Date, Some(t)) => short_local_date(t),
                        _ => text.clone(),
                    };
                    let class = move || match fit(i) {
                        ColumnFit::Wrap => "wrap",
                        ColumnFit::Clip(_) => "clip dim",
                        ColumnFit::Fit(_) | ColumnFit::Date => "dim",
                    };
                    // The lead cell — the column being read, else the first —
                    // opens the details modal, which shows the exact rendered
                    // `user` the model received (§6.2).
                    if i == widths.with(|w| lead_column(w)) {
                        let has_note = note.is_some();
                        view! {
                            <td
                                class=move || match (has_note, fit(i)) {
                                    (_, ColumnFit::Wrap) | (true, _) => "wrap review-lead",
                                    (false, ColumnFit::Clip(_)) => "clip review-lead",
                                    (false, _) => "review-lead",
                                }
                                title=title
                            >
                                <button
                                    class="link-btn"
                                    on:click=move |_| open.run(row.get_value())
                                >
                                    {if shown.is_empty() { "(no value)".to_string() } else { shown.clone() }}
                                </button>
                                {note
                                    .clone()
                                    .map(|n| {
                                        // Clipped by CSS, never by a character
                                        // count: the whole reply is one click
                                        // away in the details modal. The one
                                        // row kind allowed a second line.
                                        let full = n.clone();
                                        view! { <div class="dim mono-sm clip-line" title=full>{n}</div> }
                                    })}
                            </td>
                        }
                            .into_any()
                    } else {
                        view! { <td class=class title=title>{shown}</td> }.into_any()
                    }
                })
                .collect_view()}
            {answers
                .into_iter()
                .map(|(field, server)| {
                    let id = row_id.get_value();
                    let name = StoredValue::new(field.clone());
                    let options = Signal::derive(move || {
                        shape
                            .get()
                            .into_iter()
                            .flat_map(|s| s.editable)
                            .find(|e| e.field == field)
                            .map(|e| e.options.into_iter().map(|o| (o.clone(), o)).collect())
                            .unwrap_or_else(Vec::new)
                    });
                    // `Select` writes into an `RwSignal`, so this row keeps one
                    // — seeded from what the reviewer already chose, and
                    // mirrored straight back into `run.overrides`, which is the
                    // copy Apply reads. The effect skips its first run so
                    // merely drawing a row is not an override.
                    let sig = RwSignal::new(run.value_of(&id, &name.get_value(), &server));
                    let key = id.clone();
                    Effect::new(move |seen: Option<String>| {
                        let v = sig.get();
                        if seen.is_some() {
                            run.set_override(&key, &name.get_value(), v.clone());
                        }
                        v
                    });
                    view! {
                        <td class="review-edit">
                            <Show
                                when=move || !options.get().is_empty()
                                fallback=move || {
                                    view! {
                                        <input
                                            class="input"
                                            prop:value=move || sig.get()
                                            on:input=move |ev| sig.set(event_target_value(&ev))
                                        />
                                    }
                                }
                            >
                                <Select value=sig options=options/>
                            </Show>
                        </td>
                    }
                })
                .collect_view()}
        </tr>
    }
}
