use leptos::prelude::*;
use serde_json::Value;

use super::*;
use crate::pages::usage::money;
use crate::widgets::{Facet, FacetSet, FilterBar, GroupRow, Modal, ModalFooter};

/// What a finished run cost (§4.5), as one line — or nothing for a run that
/// made no model call and spent nothing ([`no_model_call`]). A run with no
/// model call can still have a cost: failed or stopped calls whose rows were
/// priced.
pub fn cost_text(result: &Value, currency: &str) -> Option<String> {
    let calls = result["model_calls"].as_u64().unwrap_or(0);
    let tokens_in = result["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let tokens_out = result["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    let cost_micro = result["cost_micro"].as_i64();
    if no_model_call(Some(calls), Some(tokens_in + tokens_out), cost_micro) {
        return None;
    }
    let cost = match cost_micro {
        Some(m) => money(m, currency),
        // NULL is "nobody priced this", which is not zero.
        None => "unpriced".to_string(),
    };
    Some(format!(
        "{calls} model call(s), {} tool call(s), {tokens_in} in / {tokens_out} out tokens · {cost}",
        result["tool_calls"].as_u64().unwrap_or(0),
    ))
}

/// The facet id for "the rows that need a look": not a value any answer can
/// take, so it never collides with a category.
const ATTN_FACET: &str = "\u{1}attention";

/// A review column whose values are all at most this long is shown whole,
/// as wide as its longest value.
const FIT_AT: usize = 20;
/// The width, in characters, of a long column that is not the one being read
/// (a mail's sender beside its subject): it ellipsizes, full value in its
/// tooltip. A layout choice, not a cap: nothing is cut from the data.
pub(super) const CLIP_WIDTH: usize = 28;
/// A date column shows the short local form, "Thu 17 Sep 22:13".
pub(super) const DATE_WIDTH: usize = 18;

/// How one review column is laid out (ux:U-1: the review is the gate before
/// anything is written, so the column a reviewer triages by must be readable
/// in full at every window size, not clipped to leave a date its whole width).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnFit {
    /// As wide as its longest value, in characters.
    Fit(usize),
    /// Every value is a mail date: shown short and local, the header's own
    /// text in the tooltip.
    Date,
    /// Long, but not the column being read: this many characters, ellipsized.
    Clip(usize),
    /// The column holding the most text — a mail's subject. It takes the
    /// width the others leave and wraps, so every value is read whole.
    Wrap,
}

/// Each review column's layout, from the run's rows.
pub fn column_widths(rows: &[ReviewRow], columns: usize) -> Vec<ColumnFit> {
    let texts = |i: usize| {
        rows.iter()
            .filter_map(move |r| r.cells.get(i))
            .map(|(_, t)| t)
    };
    let is_date = |i: usize| {
        let mut any = false;
        let all = texts(i).filter(|t| !t.trim().is_empty()).all(|t| {
            any = true;
            mail_date(t).is_some()
        });
        all && any
    };
    let longest = |i: usize| texts(i).map(|t| t.chars().count()).max().unwrap_or(0);
    let total = |i: usize| texts(i).map(|t| t.chars().count()).sum::<usize>();
    let dates: Vec<bool> = (0..columns).map(is_date).collect();
    // The one that wraps: of the long columns, the one with the most text in
    // it (a later column wins a tie).
    let reader = (0..columns)
        .filter(|&i| !dates[i] && longest(i) > FIT_AT)
        .max_by_key(|&i| (total(i), i));
    (0..columns)
        .map(|i| {
            if dates[i] {
                ColumnFit::Date
            } else if Some(i) == reader {
                ColumnFit::Wrap
            } else if longest(i) <= FIT_AT {
                ColumnFit::Fit(longest(i))
            } else {
                ColumnFit::Clip(longest(i).min(CLIP_WIDTH))
            }
        })
        .collect()
}

/// The column whose cell opens a row's details and carries its attention
/// note: the one being read if there is one, else the first.
pub fn lead_column(fits: &[ColumnFit]) -> usize {
    fits.iter().position(|f| *f == ColumnFit::Wrap).unwrap_or(0)
}

/// A mail header date (RFC 2822: `Thu, 17 Sep 2026 16:08:29 +0200 (CEST)`)
/// as Unix seconds, or `None` for anything else. The weekday, the seconds and
/// a trailing comment are optional; a named zone is one of the RFC's own.
pub fn mail_date(s: &str) -> Option<i64> {
    const MON: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let s = s.trim();
    let s = match s.split_once(',') {
        Some((dow, rest)) if dow.len() == 3 && dow.chars().all(|c| c.is_ascii_alphabetic()) => rest,
        _ => s,
    };
    let mut it = s.split_whitespace();
    let day: i64 = it.next()?.parse().ok()?;
    let mon = it.next()?.to_ascii_lowercase();
    let mon = MON.iter().position(|m| *m == mon)? as i64 + 1;
    let year = it.next()?;
    let year: i64 = if year.len() == 4 {
        year.parse().ok()?
    } else {
        return None;
    };
    let mut hms = it.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let m: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = match hms.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if hms.next().is_some() || !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let zone = it.next().unwrap_or("+0000");
    let offset_min: i64 = match zone.to_ascii_uppercase().as_str() {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "EDT" => -240,
        "EST" | "CDT" => -300,
        "CST" | "MDT" => -360,
        "MST" | "PDT" => -420,
        "PST" => -480,
        z if z.len() == 5 && (z.starts_with('+') || z.starts_with('-')) => {
            let v: i64 = z[1..].parse().ok()?;
            let mins = (v / 100) * 60 + v % 100;
            if z.starts_with('-') {
                -mins
            } else {
                mins
            }
        }
        _ => return None,
    };
    Some(days_from_civil(year, mon, day) * 86_400 + h * 3600 + m * 60 + sec - offset_min * 60)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A mail date in the reader's own time: "Thu 17 Sep 22:13", or
/// "17 Sep 2025 22:13" outside the current year.
pub(super) fn short_local_date(unix_s: i64) -> String {
    const DOW: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(unix_s as f64 * 1000.0));
    let this_year = js_sys::Date::new_0().get_full_year();
    let mon = MON[(d.get_month() as usize).min(11)];
    let hm = format!("{:02}:{:02}", d.get_hours(), d.get_minutes());
    if d.get_full_year() == this_year {
        format!(
            "{} {} {mon} {hm}",
            DOW[(d.get_day() as usize).min(6)],
            d.get_date()
        )
    } else {
        format!("{} {mon} {} {hm}", d.get_date(), d.get_full_year())
    }
}

/// The review: what the run is doing, the rows it came back with — filtered,
/// counted, attention first — and Apply pinned under them.
#[component]
pub(super) fn ReviewPane(ctx: RunCtx) -> impl IntoView {
    let run = ctx.run;
    let (has_apply, apply_tools, ceiling) = ctx.shape.with_value(|s| {
        (
            s.has_apply,
            s.apply_tools.join(", "),
            s.apply_tools_are_ceiling,
        )
    });
    let apply_tools = StoredValue::new(apply_tools);
    let apply_verb = if ceiling {
        "Apply may call"
    } else {
        "Apply writes through"
    };

    let query = RwSignal::new(String::new());
    let facet = RwSignal::new(String::new());
    let att_open = RwSignal::new(true);
    let set_open = RwSignal::new(true);

    // The field the facets count: the first answer a reviewer can change (a
    // mail's category), as the reviewer has it now.
    let facet_field = Memo::new(move |_| {
        run.shape.with(|s| {
            s.as_ref()
                .and_then(|s| s.editable.first().map(|e| e.field.clone()))
        })
    });
    let answer = move |r: &ReviewRow| -> String {
        facet_field
            .with(|f| {
                f.as_ref().map(|f| {
                    let server = r
                        .answers
                        .iter()
                        .find(|(k, _)| k == f)
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    run.value_of(&r.id, f, server)
                })
            })
            .unwrap_or_default()
    };
    let facets = Signal::derive(move || {
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        let mut attention = 0;
        run.rows.with(|rs| {
            for r in rs {
                if r.attention {
                    attention += 1;
                }
                let a = answer(r);
                if !a.is_empty() {
                    *counts.entry(a).or_default() += 1;
                }
            }
        });
        let mut v = Vec::new();
        if attention > 0 {
            v.push(Facet {
                id: ATTN_FACET.to_string(),
                label: "Needs attention".to_string(),
                count: attention,
            });
        }
        let mut cats: Vec<(String, usize)> = counts.into_iter().collect();
        cats.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.extend(cats.into_iter().map(|(k, n)| Facet {
            id: k.clone(),
            label: k,
            count: n,
        }));
        v
    });
    let words = Memo::new(move |_| {
        query
            .get()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let keep = move |r: &ReviewRow| -> bool {
        let on = facet.with(|f| {
            f.is_empty() || (f == ATTN_FACET && r.attention) || (f != ATTN_FACET && answer(r) == *f)
        });
        on && words.with(|ws| {
            ws.iter().all(|w| {
                r.cells.iter().any(|(_, t)| t.to_lowercase().contains(w))
                    || answer(r).to_lowercase().contains(w)
            })
        })
    };
    let attention = Memo::new(move |_| {
        run.rows.with(|rs| {
            rs.iter()
                .filter(|r| r.attention && keep(r))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let settled = Memo::new(move |_| {
        run.rows.with(|rs| {
            rs.iter()
                .filter(|r| !r.attention && keep(r))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let total = Signal::derive(move || run.rows.with(Vec::len));
    let shown = Signal::derive(move || attention.with(Vec::len) + settled.with(Vec::len));
    let att_total = Signal::derive(move || {
        run.rows
            .with(|rs| rs.iter().filter(|r| r.attention).count())
    });
    let settled_total = Signal::derive(move || total.get() - att_total.get());
    // "Classified" is a claim about every settled row, not the shown ones.
    let all_answered = Signal::derive(move || {
        run.rows
            .with(|rs| rs.iter().filter(|r| !r.attention).all(|r| r.answered))
    });
    let checked = Signal::derive(move || {
        let out = run.unchecked.get();
        run.rows
            .with(|rs| rs.iter().filter(|r| !out.contains(&r.id)).count())
    });
    let live = Signal::derive(move || run.summary.with(|s| s.as_ref().is_some_and(is_live)));
    // At least one row carries a model answer, which is what Apply writes.
    let classified = Signal::derive(move || run.rows.with(|rs| rs.iter().any(|r| r.answered)));
    let columns = Memo::new(move |_| {
        run.shape.with(|s| {
            s.as_ref()
                .map(|s| 1 + s.columns.len() + s.editable.len())
                .unwrap_or(1)
        })
    });
    let widths = Memo::new(move |_| {
        let n = run
            .shape
            .with(|s| s.as_ref().map(|s| s.columns.len()).unwrap_or(0));
        run.rows.with(|rs| column_widths(rs, n))
    });
    let lead = Memo::new(move |_| widths.with(|w| lead_column(w)));
    let shown_ids = move || {
        attention
            .with(|a| a.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
            .into_iter()
            .chain(settled.with(|s| s.iter().map(|r| r.id.clone()).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    };
    let all_checked = Signal::derive(move || {
        let out = run.unchecked.get();
        shown_ids().iter().all(|id| !out.contains(id))
    });
    let tick_shown = Callback::new(move |on: bool| {
        let ids = shown_ids();
        run.unchecked.update(|set| {
            for id in ids {
                if on {
                    set.remove(&id);
                } else {
                    set.insert(id);
                }
            }
        });
    });

    let open_row = Callback::new(move |r: ReviewRow| {
        run.modal_title
            .set(match r.cells.get(lead.get_untracked()) {
                Some((_, text)) if !text.is_empty() => text.clone(),
                _ => r.id.clone(),
            });
        let asked = match (&r.prompt, &r.error) {
            (Some(p), _) if !p.is_empty() => p.clone(),
            (_, Some(e)) => format!("This row was never sent to the model.\n\n{e}"),
            _ => "This row has not been classified, so nothing was sent to the model yet."
                .to_string(),
        };
        // What it said, in full. The table clips its cell; this is the place
        // that shows the whole reply, which is the question the review table
        // exists to answer.
        run.modal_body
            .set(match r.raw.as_deref().filter(|s| !s.is_empty()) {
                Some(raw) => format!("{asked}\n\n─── the model answered ───\n\n{raw}"),
                None => asked,
            });
        run.modal_open.set(true);
    });

    // What the bar says beside its buttons: the one thing to know before
    // pressing them.
    let note = move || -> (&'static str, String) {
        let (n, all, vis) = (checked.get(), total.get(), shown.get());
        if live.get() {
            (
                "action-note",
                "running — rows fill in as the run reaches them".to_string(),
            )
        } else if all == 0 {
            ("action-note", "this run has no rows to review".to_string())
        } else if let Some(w) = ctx.blocked_by.get() {
            ("action-note needs", w)
        } else if has_apply && !classified.get() {
            // Apply needs rows the model actually answered: a list-only run
            // is a browse, and offering to write from it would be offering to
            // write the fallback (§2.4).
            (
                "action-note needs",
                "this run only listed rows — start a dry run to have the model answer each one"
                    .to_string(),
            )
        } else {
            let filtered = if vis != all {
                format!(" ({vis} shown)")
            } else {
                String::new()
            };
            let tail = if has_apply {
                "nothing is written until Apply"
            } else {
                "this agent has no apply step"
            };
            (
                "action-note",
                format!("{n} of {all} checked{filtered} · {tail}"),
            )
        }
    };
    let has_attention = move || att_total.get() > 0;
    let can_apply = move || has_apply && classified.get() && !live.get();
    let rerun_off = move || ctx.blocked() || run.busy.get() || live.get() || run.job_id().is_none();
    let apply_off = move || ctx.blocked() || run.busy.get() || checked.get() == 0;
    let why = move || ctx.blocked_by.get().unwrap_or_default();

    view! {
        <div class="review-main">
            <RunStrip ctx=ctx/>
            {move || run.error.get().map(|e| view! { <div class="notice err">{e}</div> })}
            <FilterBar
                query=query
                placeholder="Filter rows by any column"
                shown=shown
                total=total
                noun="rows"
                facets=FacetSet {
                    items: facets,
                    active: facet,
                }
            />
            <div class="fill-pane card pad0 review-pane">
                <table class="data review-table">
                    <ReviewHead shape=run.shape widths=widths all_checked=all_checked on_all=tick_shown/>
                    {move || {
                        let span = columns.get() as u32;
                        view! {
                            <tbody>
                                <Show when=move || !attention.with(Vec::is_empty)>
                                    <GroupRow
                                        colspan=span
                                        label="Needs attention"
                                        count=Signal::derive(move || {
                                            crate::fmt::of(attention.with(Vec::len), att_total.get())
                                        })
                                        open=att_open
                                        meta=|| "landed on the fallback or the call failed — widen the config, then re-run just these"
                                    />
                                    <Show when=move || att_open.get()>
                                        <For each=move || attention.get() key=|r| r.view_key() let:r>
                                            <ReviewRowView r=r run=run shape=run.shape open=open_row widths=widths/>
                                        </For>
                                    </Show>
                                </Show>
                                <Show when=move || !settled.with(Vec::is_empty)>
                                    <GroupRow
                                        colspan=span
                                        label=move || settled_heading(all_answered.get())
                                        count=Signal::derive(move || {
                                            crate::fmt::of(settled.with(Vec::len), settled_total.get())
                                        })
                                        open=set_open
                                    />
                                    <Show when=move || set_open.get()>
                                        <For each=move || settled.get() key=|r| r.view_key() let:r>
                                            <ReviewRowView r=r run=run shape=run.shape open=open_row widths=widths/>
                                        </For>
                                    </Show>
                                </Show>
                            </tbody>
                        }
                    }}
                </table>
                <Show when=move || total.get() == 0>
                    <div class="empty">"This run has no rows."</div>
                </Show>
                <Show when=move || total.get() != 0 && shown.get() == 0>
                    <div class="empty">"No row matches the filter."</div>
                </Show>
            </div>
            <div class="action-bar">
                <span class=move || note().0 title=move || note().1>{move || note().1}</span>
                <Show when=has_attention>
                    <button
                        class="btn"
                        title=move || {
                            if ctx.blocked() {
                                why()
                            } else {
                                "classify only the rows that need attention again; every other row keeps its answer"
                                    .to_string()
                            }
                        }
                        disabled=rerun_off
                        on:click=move |_| ctx.rerun()
                    >
                        {move || format!("Re-run attention ({})", att_total.get())}
                    </button>
                </Show>
                <Show when=can_apply>
                    <button
                        class="btn primary"
                        title=why
                        disabled=apply_off
                        on:click=move |_| run.confirm_apply.set(true)
                    >
                        {move || format!("Apply {} checked", checked.get())}
                    </button>
                </Show>
            </div>
            <ReviewFoot run=run/>
        </div>

        <Modal open=run.confirm_apply title="Apply this run">
            <p>
                {move || format!(
                    "This is the only step that writes. {} checked row(s) are handed to the \
                     apply step, which runs with exactly the tools the manifest names.",
                    checked.get(),
                )}
            </p>
            {
                let tools = apply_tools.get_value();
                (!tools.is_empty())
                    .then(|| view! { <p class="dim mini-note">{format!("{apply_verb} {tools}.")}</p> })
            }
            <p class="dim mini-note">
                {match ctx.runtime.get_value() {
                    // Principle 5 covers both: a container's bounds are its
                    // cgroup limits and its deadline, not the Responses pair a
                    // turn runs under.
                    Some(r) => bounds_line(&r).into_any(),
                    None => {
                        let b = ctx.budget.get_value();
                        view! {
                            {format!(
                                "Budget: {} tool calls, {} s (",
                                b.max_tool_calls,
                                b.timeout_seconds,
                            )}
                            <a href=crate::pages::settings_href("responses_max_tool_calls")>
                                "Settings → Agents & tools"
                            </a>
                            ")."
                        }
                            .into_any()
                    }
                }}
            </p>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| run.confirm_apply.set(false)>
                    "Cancel"
                </button>
                <button class="btn primary" on:click=move |_| ctx.apply()>
                    "Apply"
                </button>
            </ModalFooter>
        </Modal>

        // Sized by its content; a long prompt scrolls in the modal's body.
        <Modal open=run.modal_open title="Model input">
            <div class="mini-head">{move || run.modal_title.get()}</div>
            <pre class="mono-sm prompt-text">{move || run.modal_body.get()}</pre>
        </Modal>
    }
}
