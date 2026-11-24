//! Settings › Retention's "Stored conversions", beside the retention of
//! conversion jobs (document-conversion design §7.3): how many background
//! conversions have files on this computer, how much space those take, and
//! how many files still wait. A queued job keeps its uploads on disk, and
//! the retention alone bounds the converted text, so this is where a queue
//! or a disk that fills shows. Read from `doc_jobs_stored` each time the
//! category is shown.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::jobs::JobsStored;
use lmgw_api_types::units::fmt_bytes;
use serde_json::json;

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The row's sentence.
pub(super) fn said(s: &JobsStored) -> String {
    if s.jobs == 0 && s.running == 0 {
        return "No background conversion has files stored.".to_string();
    }
    let mut text = format!(
        "{} on disk, {}.",
        plural(s.jobs, "conversion", "conversions"),
        fmt_bytes(s.bytes)
    );
    if s.running > 0 {
        text.push_str(&format!(
            " {} running; {} waiting or being converted.",
            s.running,
            plural(s.queued_files, "file is", "files are")
        ));
    }
    text
}

#[component]
pub(super) fn Stored(id: String, hidden: Signal<bool>) -> impl IntoView {
    let stored: RwSignal<Option<Result<JobsStored, String>>> = RwSignal::new(None);
    Effect::new(move |_| {
        if hidden.get() {
            return;
        }
        spawn_local(async move {
            let r = crate::api::post::<JobsStored, _>("/api/op/doc_jobs_stored", &json!({}))
                .await
                .map_err(|e| e.to_string());
            stored.try_set(Some(r));
        });
    });
    let body = move || match stored.get() {
        None => view! { <span class="prow-sub">"Reading…"</span> }.into_any(),
        Some(Err(e)) => view! {
            <span class="prow-sub">{format!("Reading the stored conversions failed: {e}")}</span>
        }
        .into_any(),
        Some(Ok(s)) => view! { <span class="prow-sub">{said(&s)}</span> }.into_any(),
    };
    view! {
        <div class="prow doc-row" id=id hidden=hidden data-stored="">
            <div class="prow-text">
                <span class="prow-title">"Stored conversions"</span>
                {body}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_says_how_many_jobs_how_many_bytes_and_what_waits() {
        let s = JobsStored {
            jobs: 3,
            bytes: 5 * 1024 * 1024,
            running: 1,
            queued_files: 40,
        };
        assert_eq!(
            said(&s),
            "3 conversions on disk, 5.0 MiB. 1 running; 40 files are waiting or being converted."
        );
        assert_eq!(
            said(&JobsStored::default()),
            "No background conversion has files stored."
        );
        let one = JobsStored {
            jobs: 1,
            bytes: 1024,
            ..Default::default()
        };
        assert_eq!(said(&one), "1 conversion on disk, 1.0 KiB.");
    }
}
