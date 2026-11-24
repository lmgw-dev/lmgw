//! The opened request (`/traffic/<log id>`): a hero with its name, status
//! and time, then its stored fields as boxed groups — the request, the
//! answer, the tokens and cost, the error with its message. A row the list
//! does not hold (a link to an older one, or one the filters leave out) is
//! read by its id; an answer for a row no longer opened is dropped.
//!
//! With nothing opened the detail is the status page: what the list shows
//! and the GPU plane.

use leptos::prelude::*;
use lmgw_api_types::{LogsResponse, RequestRow};

use super::list::{names_no_model, row_name};
use super::{fallback_label, gpu, input_split, row_cost, Log};
use crate::fmt::{grouped, log_time};
use crate::scope::{Latest, Scope};
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{Ico, Icon, InfoRow, MenuItem, PrefGroup, StackRow};

/// One opened row: from the list when it is there, else read by its id.
#[component]
pub(super) fn RowDetail(
    log: Log,
    id: Signal<i64>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let scope = Scope::new();
    let read = Latest::new();
    // The row as read by its id: (id, the row, or why there is none).
    let fetched = RwSignal::new(None::<(i64, Result<RequestRow, String>)>);
    let in_list = Memo::new(move |_| {
        let id = id.get();
        log.rows
            .with(|r| r.iter().find(|x| x.log_id == id).cloned())
    });
    Effect::new(move |_| {
        let want = id.get();
        if in_list.with(Option::is_some)
            || fetched.with_untracked(|f| f.as_ref().is_some_and(|(i, r)| *i == want && r.is_ok()))
        {
            return;
        }
        let Some(ticket) = read.next() else { return };
        // The newest row at or below the id: it is the row when it exists.
        let url = format!("/api/logs?limit=1&before_id={}", want.saturating_add(1));
        scope.spawn(async move {
            let res = crate::api::get::<LogsResponse>(url).await;
            if !read.is(ticket) {
                return;
            }
            let row = match res {
                Ok(r) => r
                    .logs
                    .into_iter()
                    .find(|x| x.log_id == want)
                    .ok_or_else(|| {
                        format!("Request {want} is not in the log (it may have been pruned).")
                    }),
                Err(e) => Err(e.to_string()),
            };
            fetched.set(Some((want, row)));
        });
    });
    let row = Memo::new(move |_| {
        let want = id.get();
        in_list.get().map(Ok).or_else(|| {
            fetched.with(|f| {
                f.as_ref()
                    .filter(|(i, _)| *i == want)
                    .map(|(_, r)| r.clone())
            })
        })
    });

    let scrolled = RwSignal::new(false);
    let title = Signal::derive(move || {
        row.with(|r| match r {
            Some(Ok(r)) => row_name(r),
            _ => format!("Request {}", id.get()),
        })
    });
    let f = log.f;
    let more = Signal::derive(move || {
        let Some(Ok(r)) = row.get() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        let alias = r.requested_alias.clone();
        items.push(
            MenuItem::new("Only this model", move || f.alias.set(alias.clone()))
                .title("List only the requests that asked for this alias"),
        );
        if let Some(up) = r.upstream_name.clone().filter(|u| !u.is_empty()) {
            items.push(
                MenuItem::new("Only this upstream", move || f.upstream.set(up.clone()))
                    .title("List only the requests this upstream answered"),
            );
        }
        if let Some(kind) = r.error_kind.clone() {
            items.push(
                MenuItem::new("Only errors of this kind", move || {
                    f.error_kind.set(kind.clone())
                })
                .title("List only the requests that failed the same way"),
            );
        }
        items
    });

    view! {
        <DetailHead
            narrow=narrow
            back=back
            back_label="Back to the requests"
            title=title
            scrolled=scrolled
            more=more
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            {move || match row.get() {
                None => view! { <div class="mdetail-empty">"Loading…"</div> }.into_any(),
                Some(Err(e)) => view! { <div class="mdetail-empty"><p>{e}</p></div> }.into_any(),
                Some(Ok(r)) => view! { <RowBody r=r/> }.into_any(),
            }}
        </div>
    }
}

fn ms(v: Option<i64>) -> String {
    v.map(|v| format!("{} ms", grouped(v.max(0) as u64)))
        .unwrap_or_else(|| "—".into())
}

#[component]
fn RowBody(r: RequestRow) -> impl IntoView {
    let t = log_time(&r.ts);
    let ok = r.status < 400;
    let name = row_name(&r);
    let no_model = names_no_model(&r);
    let route = format!(
        "{} → {}",
        r.ingress_proto,
        r.egress_proto.clone().unwrap_or_else(|| "—".into())
    );
    let upstream = r.upstream_name.clone().unwrap_or_else(|| "—".into());
    let mut sub = vec![format!("{} {}", t.day_label, t.time), upstream.clone()];
    if !r.class.is_empty() {
        sub.push(r.class.clone());
    }
    let tokens = match (r.prompt_tokens, r.completion_tokens) {
        (Some(p), Some(c)) => format!(
            "{} in → {} out",
            grouped(p.max(0) as u64),
            grouped(c.max(0) as u64)
        ),
        (Some(p), None) => format!("{} in", grouped(p.max(0) as u64)),
        _ => "—".into(),
    };
    let split = input_split(&r);
    let measured = row_cost::measured(&r);
    let cost_line = row_cost::cost_line(&r);
    let parts = row_cost::parts(&r);
    let glyph = if r.ingress_proto == "mcp" {
        Ico::Wrench
    } else {
        Ico::Shuffle
    };
    view! {
        <div class="detail-flow tr-detail">
            <div class="detail-hero">
                <div class="glyph"><Icon ico=glyph/></div>
                <div class="hero-text">
                    <h1 class:dim=no_model>{name.clone()}</h1>
                    <div class="sub">{sub.join(" · ")}</div>
                    <div class="chips">
                        <span class=if ok { "chip ok" } else { "chip err" }>
                            <span class="dot"></span>
                            {r.status.to_string()}
                        </span>
                        {r.streamed.then(|| view! { <span class="chip">"Streamed"</span> })}
                        {r.fallback_reason.clone().map(|reason| view! {
                            <span class="chip warn" title=reason.clone()>{fallback_label(&reason)}</span>
                        })}
                        {r.degraded.clone().map(|marker| view! {
                            <span class="chip warn" title=marker>"degraded"</span>
                        })}
                        {r.rung.map(|k| view! {
                            <span class="chip" title="ladder rung this request was judged on">{format!("rung {k}")}</span>
                        })}
                    </div>
                </div>
            </div>
            {r.error_kind.clone().or(r.error_msg.clone()).map(|_| {
                let kind = r.error_kind.clone();
                let msg = r.error_msg.clone();
                view! {
                    <PrefGroup title="Error">
                        {kind.map(|k| view! { <InfoRow title="Kind" value=k mono=true/> })}
                        {msg.map(|m| view! {
                            <StackRow class="tr-err">
                                <pre class="code-block err">{m}</pre>
                            </StackRow>
                        })}
                    </PrefGroup>
                }
            })}
            <PrefGroup title="Request">
                <InfoRow title="Requested" value=r.requested_alias.clone() mono=true/>
                {(r.ingress_proto == "mcp").then(|| r.mcp_tool.clone()).flatten().map(|tool| view! {
                    <InfoRow title="Tool" value=tool mono=true/>
                })}
                <InfoRow title="Route" value=route mono=true/>
                {(!r.class.is_empty()).then(|| view! { <InfoRow title="Class" value=r.class.clone()/> })}
                {r.client_key.clone().filter(|k| !k.is_empty()).map(|k| view! {
                    <InfoRow title="Client key" value=k mono=true/>
                })}
                <InfoRow title="Time" value=format!("{} {} local", t.day, t.time)/>
                <InfoRow title="Time in UTC" value=t.utc.clone()/>
                <InfoRow title="Log id" value=r.log_id.to_string() mono=true/>
            </PrefGroup>
            <PrefGroup title="Answer">
                <InfoRow
                    title="Status"
                    value=if r.streamed { format!("{} · streamed", r.status) } else { r.status.to_string() }
                />
                <InfoRow title="Upstream" value=upstream/>
                {
                    let model = r.upstream_model.clone().filter(|m| !m.is_empty());
                    view! {
                        <InfoRow
                            title="Upstream model"
                            mono=model.is_some()
                            value=model.unwrap_or_else(|| "—".into())
                        />
                    }
                }
                <InfoRow title="Time to first byte" value=ms(r.ttfb_ms)/>
                <InfoRow title="Total time" value=ms(r.total_ms)/>
                {r.fallback_reason.clone().map(|reason| view! {
                    <InfoRow title="Fallback" value=fallback_label(&reason)/>
                })}
                {r.degraded.clone().map(|marker| view! { <InfoRow title="Degraded" value=marker/> })}
                {r.rung.map(|k| view! { <InfoRow title="Rung" value=k.to_string()/> })}
                {r.max_tokens_clamped.map(|n| view! {
                    <InfoRow title="Max tokens" value=format!("clamped to {n}")/>
                })}
            </PrefGroup>
            <PrefGroup title="Tokens and cost">
                <InfoRow title="Tokens" value=tokens/>
                {split.map(|(fresh, read, write)| view! {
                    <InfoRow title="Input split" value=format!("{fresh} fresh · {read} cached · {write} written")/>
                })}
                {measured.map(|m| view! { <InfoRow title="Measured" value=m/> })}
                {cost_line.map(|c| view! { <InfoRow title="Cost" value=c/> })}
                {r.service_tier.clone().map(|tier| view! { <InfoRow title="Service tier" value=tier/> })}
                {r.price_note.clone().map(|note| view! { <InfoRow title="Price note" value=note/> })}
                {parts
                    .into_iter()
                    .map(|(what, how)| {
                        let title = if what == "Tokens" { "Priced as" } else { what };
                        view! { <InfoRow title=title value=how/> }
                    })
                    .collect_view()}
            </PrefGroup>
        </div>
    }
}

/// Nothing opened: what the list shows, and the GPU plane.
#[component]
pub(super) fn StatusPage(log: Log, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead
            narrow=narrow
            back=back
            back_label="Back to the requests"
            title=Signal::derive(|| "Traffic".to_string())
            scrolled=scrolled
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="detail-flow tr-status">
                <div class="detail-hero">
                    <div class="glyph"><Icon ico=Ico::Shuffle/></div>
                    <div class="hero-text">
                        <h1>"Requests"</h1>
                        <div class="sub">{move || log.status.get()}</div>
                        <div class="sub">
                            "Every request the gateway served, newest first. Pick one to read every field it stored."
                        </div>
                    </div>
                </div>
                <Show when=move || log.vram.with(Option::is_some)>
                    <PrefGroup title="GPU memory">
                        <StackRow class="tr-gpu">
                            <gpu::GpuPanel vram=log.vram/>
                        </StackRow>
                    </PrefGroup>
                </Show>
            </div>
        </div>
    }
}
