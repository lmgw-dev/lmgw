//! Settings › Documents' "How much conversions may hold" (security round
//! A): the memory answers in flight hold gateway-wide, and the two bounds
//! of the async jobs, "Storage for conversion jobs" (`settings_set`'s
//! `documents_job_storage_mib`) and "Jobs per key"
//! (`documents_jobs_per_key`), each derived until set by hand (0 goes back
//! to the derived figure).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::bounds::{AnswerFacts, JobBounds};
use lmgw_api_types::units::{fmt_bytes, gib_figure};
use serde_json::json;

use super::use_docs;
use crate::widgets::use_toasts;

/// The answers row's sentences.
pub(super) fn answers_said(a: &AnswerFacts) -> (String, String) {
    let bound = match a.limit_bytes {
        Some(l) => format!(
            "Answers being made and sent hold at most {} of memory together, the memory lmgw \
             may use; each conversion holds its upload and up to {} of output from before its \
             files are read until its answer has been sent, then only its answer's size.",
            fmt_bytes(l),
            fmt_bytes(a.output_bytes)
        ),
        None => format!(
            "Answers being made and sent are not bounded yet: this system's memory is not read. \
             A request's files may produce up to {} together.",
            fmt_bytes(a.output_bytes)
        ),
    };
    // Who may hold all of it, and why (review N-d).
    let callers = if a.callers.shared {
        "; one caller leaves room for one more conversion like its own, and a caller with none \
         goes first"
    } else if a.callers.sign_in {
        "; one caller may hold all of it, as only one key may call the conversions"
    } else {
        "; one caller may hold all of it, as Require API key is off; with it on and more than \
         one key, each caller leaves room for another's conversion"
    };
    let now = format!(
        "Now {} holding {}, {} waiting{callers}. A client gets {} s plus 1 s for every {} of an \
         answer to read it, and {} s plus 1 s for every {} of a request to send it; one that is \
         slower is cut off.",
        a.holding,
        fmt_bytes(a.held_bytes),
        a.waiting,
        a.write_grace_s,
        fmt_bytes(a.min_rate_bytes_per_s),
        a.read_grace_s,
        fmt_bytes(a.read_min_rate_bytes_per_s)
    );
    (bound, now)
}

/// The storage row's sentence.
pub(super) fn storage_said(b: &JobBounds) -> String {
    let stored = format!("the jobs store {} now", fmt_bytes(b.stored_bytes));
    if b.storage_by_hand {
        return format!(
            "{} at most, set by hand; {stored}.",
            fmt_bytes(b.storage_bytes)
        );
    }
    match b.free_bytes {
        Some(free) => format!(
            "{} at most now: half of what the jobs store and the {} free on the data folder's \
             disk together, read at each new job; {stored}.",
            fmt_bytes(b.storage_bytes),
            fmt_bytes(free)
        ),
        None => format!(
            "Not bounded: the free space of the data folder's disk could not be read; {stored}."
        ),
    }
}

/// The jobs-per-key row's sentence.
pub(super) fn per_key_said(b: &JobBounds) -> String {
    if b.per_key_by_hand {
        format!(
            "{} background conversions running or waiting per API key at most, set by hand.",
            b.per_key
        )
    } else {
        format!(
            "{} background conversions running or waiting per API key at most (all requests \
             without a key count as one): as many as the {} parallel tool runs.",
            b.per_key, b.pool_slots
        )
    }
}

/// "Memory for answers".
#[component]
pub(super) fn AnswersRow(answers: AnswerFacts) -> impl IntoView {
    let (bound, now) = answers_said(&answers);
    view! {
        <div class="prow doc-row" data-answers="">
            <div class="prow-text">
                <span class="prow-title">"Memory for answers"</span>
                <span class="prow-sub">{bound}</span>
                <span class="prow-sub">{now}</span>
            </div>
        </div>
    }
}

/// Save one of the two job settings; `0` goes back to the derived figure.
fn save(key: &'static str, value: u64, done: String, saving: RwSignal<bool>) {
    let d = use_docs();
    let toasts = use_toasts();
    saving.set(true);
    spawn_local(async move {
        let body = json!({ key: value });
        match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
            Ok(_) => {
                toasts.ok(done);
                d.load(false);
            }
            Err(e) => toasts.err(e.to_string()),
        }
        saving.try_set(false);
    });
}

/// "Storage for conversion jobs", typed in GiB.
#[component]
pub(super) fn StorageRow(bounds: JobBounds) -> impl IntoView {
    let toasts = use_toasts();
    let typed = RwSignal::new(if bounds.storage_by_hand {
        gib_figure(bounds.storage_bytes)
    } else {
        String::new()
    });
    let saving = RwSignal::new(false);
    let set = move |_| match typed
        .get_untracked()
        .trim()
        .replace(',', ".")
        .parse::<f64>()
    {
        Ok(g) if g > 0.0 && g.is_finite() => {
            let mib = (g * 1024.0).round().max(1.0) as u64;
            save(
                "documents_job_storage_mib",
                mib,
                format!(
                    "Storage for conversion jobs set to {}",
                    fmt_bytes(mib << 20)
                ),
                saving,
            );
        }
        _ => toasts.err("Type a size in GiB above 0, or use the derived size".to_string()),
    };
    let reset = move |_| {
        save(
            "documents_job_storage_mib",
            0,
            "Storage for conversion jobs follows the disk again".to_string(),
            saving,
        )
    };
    // The figure the sentence says (review N-e), in the field's unit.
    let placeholder = gib_figure(bounds.storage_bytes);
    view! {
        <div class="prow doc-row" data-job-storage="">
            <div class="prow-text">
                <span class="prow-title">"Storage for conversion jobs"</span>
                <span class="prow-sub">{storage_said(&bounds)}</span>
                <span class="prow-sub">
                    "A background conversion whose files do not fit is refused; a result that \
                     does not fit is not stored, and its file says so."
                </span>
            </div>
            <div class="prow-end">
                <input
                    class="input mono-sm w-num"
                    type="number"
                    min="0.1"
                    step="0.1"
                    aria-label="Storage for conversion jobs in GiB"
                    placeholder=placeholder
                    prop:value=move || typed.get()
                    on:input=move |ev| typed.set(event_target_value(&ev))
                />
                <span class="dim">"GiB"</span>
                <button type="button" class="btn sm" disabled=move || saving.get() on:click=set>
                    "Set"
                </button>
                {bounds.storage_by_hand.then(|| view! {
                    <button
                        type="button"
                        class="btn sm flat"
                        disabled=move || saving.get()
                        on:click=reset
                    >
                        "Use the derived size"
                    </button>
                })}
            </div>
        </div>
    }
}

/// "Jobs per key".
#[component]
pub(super) fn PerKeyRow(bounds: JobBounds) -> impl IntoView {
    let toasts = use_toasts();
    let typed = RwSignal::new(if bounds.per_key_by_hand {
        bounds.per_key.to_string()
    } else {
        String::new()
    });
    let saving = RwSignal::new(false);
    let set = move |_| match typed.get_untracked().trim().parse::<u32>() {
        Ok(n) if n > 0 => save(
            "documents_jobs_per_key",
            u64::from(n),
            format!("Jobs per key set to {n}"),
            saving,
        ),
        _ => toasts.err("Type a whole number above 0, or use the derived number".to_string()),
    };
    let reset = move |_| {
        save(
            "documents_jobs_per_key",
            0,
            "Jobs per key follow the parallel tool runs again".to_string(),
            saving,
        )
    };
    view! {
        <div class="prow doc-row" data-jobs-per-key="">
            <div class="prow-text">
                <span class="prow-title">"Jobs per key"</span>
                <span class="prow-sub">{per_key_said(&bounds)}</span>
                <span class="prow-sub">"One more is refused until one of them has ended."</span>
            </div>
            <div class="prow-end">
                <input
                    class="input mono-sm w-num"
                    type="number"
                    min="1"
                    aria-label="Jobs per key"
                    placeholder=bounds.per_key.to_string()
                    prop:value=move || typed.get()
                    on:input=move |ev| typed.set(event_target_value(&ev))
                />
                <button type="button" class="btn sm" disabled=move || saving.get() on:click=set>
                    "Set"
                </button>
                {bounds.per_key_by_hand.then(|| view! {
                    <button
                        type="button"
                        class="btn sm flat"
                        disabled=move || saving.get()
                        on:click=reset
                    >
                        "Use the derived number"
                    </button>
                })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_rows_say_their_bounds_and_where_they_come_from() {
        let a = AnswerFacts {
            limit_bytes: Some(16 * GIB),
            output_bytes: GIB,
            held_bytes: 4 * GIB,
            holding: 2,
            waiting: 1,
            write_grace_s: 60,
            min_rate_bytes_per_s: 1 << 20,
            read_grace_s: 60,
            read_min_rate_bytes_per_s: 256 << 10,
            callers: Default::default(),
        };
        let (bound, now) = answers_said(&a);
        assert!(
            bound.contains("16.0 GiB") && bound.contains("1.0 GiB"),
            "{bound}"
        );
        assert!(
            now.starts_with(
                "Now 2 holding 4.0 GiB, 1 waiting; one caller may hold all of it, as Require API \
                 key is off;"
            ) && now.contains("60 s")
                && now.contains("256 KiB"),
            "{now}"
        );
        let mut one = a;
        one.callers.sign_in = true;
        one.callers.callers = 1;
        assert!(
            answers_said(&one)
                .1
                .contains("one caller may hold all of it, as only one key may call"),
            "{}",
            answers_said(&one).1
        );
        let mut b = JobBounds {
            storage_bytes: 400 * GIB,
            storage_by_hand: false,
            free_bytes: Some(800 * GIB),
            stored_bytes: GIB,
            per_key: 32,
            per_key_by_hand: false,
            pool_slots: 32,
        };
        assert_eq!(
            storage_said(&b),
            "400 GiB at most now: half of what the jobs store and the 800 GiB free on the data \
             folder's disk together, read at each new job; the jobs store 1.0 GiB now."
        );
        assert!(per_key_said(&b).contains("as many as the 32 parallel tool runs"));
        // The field shows the figure the sentence says (review N-e).
        b.storage_bytes = 217 * GIB + GIB / 2 - 1;
        assert!(storage_said(&b).starts_with("217 GiB at most now"));
        assert_eq!(gib_figure(b.storage_bytes), "217");
        b.storage_bytes = 400 * GIB;
        b.storage_by_hand = true;
        b.per_key_by_hand = true;
        b.per_key = 4;
        assert!(storage_said(&b).starts_with("400 GiB at most, set by hand"));
        assert!(per_key_said(&b).starts_with("4 background conversions"));
        b.storage_by_hand = false;
        b.free_bytes = None;
        assert!(storage_said(&b).starts_with("Not bounded"));
    }
}
