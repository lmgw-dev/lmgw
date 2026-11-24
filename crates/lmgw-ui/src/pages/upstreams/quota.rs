//! An upstream remembered out of quota, a group of its detail (openai egress design
//! §14.6): since when, the provider's code, what happens meanwhile, when it
//! is asked again, and **Check now**, which forgets it so the next request
//! goes to it (`upstream_quota_clear`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{RowSaved, UpstreamQuota};
use serde_json::json;

use crate::fmt::local_datetime;
use crate::widgets::{ActionRow, PrefGroup};

/// When `unix_s` is, in the reader's local time: the clock time ("14:02")
/// on the day it is read, with the date on any other ("2026-10-26 14:07"),
/// so a check due tomorrow does not read as one due today. `None` past what
/// a date can say (a `Retry-After` of millennia).
fn when(unix_s: i64, now_s: i64) -> Option<String> {
    // The JavaScript `Date` range: 8.64e15 ms either side of the epoch.
    if unix_s.unsigned_abs() > 8_640_000_000_000 {
        return None;
    }
    let t = local_datetime(unix_s as f64);
    let same_day = t.get(..10) == local_datetime(now_s as f64).get(..10);
    t.get(if same_day { 11..16 } else { 0..16 })
        .map(String::from)
}

/// The line under the row: since when, what happens meanwhile, and when the
/// upstream is asked again.
fn quota_line(since: Option<String>, code: &str, next: Option<String>) -> String {
    let since = since.unwrap_or_else(|| "a while".into());
    let next = match next {
        Some(at) => format!("the next request goes to it again at {at}"),
        None => "it is asked again only after Check now (the wait it asked for runs past any \
                 date)"
            .to_string(),
    };
    format!(
        "Out of quota since {since}{code}. Models and aliases on it that have a fallback are \
         answered by their fallback until it answers again; {next}."
    )
}

/// The group in the upstream's detail; `cleared` runs after Check now.
#[component]
pub(super) fn QuotaGroup(
    id: i64,
    quota: UpstreamQuota,
    toasts: crate::widgets::Toasts,
    cleared: Callback<()>,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let code = quota
        .code
        .clone()
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();
    let now = (js_sys::Date::now() / 1000.0) as i64;
    let line = quota_line(when(quota.since, now), &code, when(quota.next_check, now));
    let check = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<RowSaved, _>(
                "/api/op/upstream_quota_clear",
                &json!({ "id": id }),
            )
            .await;
            let _ = busy.try_set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    cleared.run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let message = quota.message.clone();
    view! {
        <PrefGroup title="Out of quota">
            <div class="prow notice-row warn up-quota-line" title=message>
                {line}
            </div>
            <ActionRow
                title="Forget it now"
                sub="The next request goes to this upstream again"
            >
                <button
                    type="button"
                    class="btn"
                    disabled=move || busy.get()
                    on:click=move |_| check()
                >
                    {move || if busy.get() { "Checking…" } else { "Check now" }}
                </button>
            </ActionRow>
        </PrefGroup>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_says_what_happens_and_when_it_is_asked_again() {
        let line = quota_line(
            Some("14:02".into()),
            " (insufficient_quota)",
            Some("2026-10-26 14:07".into()),
        );
        assert_eq!(
            line,
            "Out of quota since 14:02 (insufficient_quota). Models and aliases on it that have \
             a fallback are answered by their fallback until it answers again; the next \
             request goes to it again at 2026-10-26 14:07."
        );
        assert!(quota_line(None, "", None).contains("only after Check now"));
    }
}
