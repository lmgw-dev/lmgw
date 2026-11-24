//! Settings › Documents' "Parallel tool runs" (document-conversion design
//! §7.7): the tool pool's size, what it is derived from, what runs and
//! waits now, and the size set by hand (`settings_set`'s
//! `documents_tool_slots`; 0 goes back to the derived size).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::bounds::CallerFacts;
use lmgw_api_types::documents::{MemorySource, PoolFacts};
use lmgw_api_types::units::fmt_bytes;
use serde_json::json;

use super::use_docs;
use crate::widgets::use_toasts;

/// What the pool's size comes from, in words.
pub(super) fn pool_said(p: &PoolFacts) -> String {
    let memory = match (p.derived_from.memory_bytes, p.derived_from.memory_source) {
        (Some(b), Some(MemorySource::Cgroup)) => format!("{} of memory (its limit)", fmt_bytes(b)),
        (Some(b), _) => format!("{} of memory", fmt_bytes(b)),
        (None, _) => "memory not read on this system yet".to_string(),
    };
    let derived = format!("{} from {} cores", p.derived, p.derived_from.cores);
    if p.by_hand {
        format!(
            "{} at once, set by hand, whatever memory they use; this computer gives {derived} \
             and {memory}.",
            p.slots
        )
    } else {
        format!(
            "{} at once: {derived}, as far as {memory} holds them.",
            p.slots
        )
    }
}

/// Each kind of step's charge and how many fit, in words (`None` when the
/// memory is not counted).
pub(super) fn charges_said(p: &PoolFacts) -> Option<String> {
    if !p.memory_counted {
        return None;
    }
    let c = &p.derived_from.charges;
    let one = |what: &str, s: lmgw_api_types::documents::StepCharge| {
        format!("{what} {} ({} at once)", fmt_bytes(s.bytes), s.fit)
    };
    Some(format!(
        "Each step counts the most memory its tools may use: {}, {}, {}, {}.",
        one("a PDF's text", c.pdf_file),
        one("a page's pictures cut out", c.page_crops),
        one("a page of text recognition", c.ocr_page),
        one(
            "a web page, e-mail or picture read by lmgw's reader",
            c.reader_run
        )
    ))
}

/// One caller's share in words (security round A): which case is in force
/// and why, and what one caller runs at most at once.
pub(super) fn share_said(p: &PoolFacts, c: &CallerFacts) -> String {
    if !c.shared {
        // What would change: with sign-in off a second key changes nothing
        // (review N-c), so the sentence names both conditions.
        let (why, then) = if c.sign_in {
            (
                "only one key may call the conversions",
                "With a second key, each caller keeps room for the others",
            )
        } else {
            (
                "Require API key is off, so calls need no key",
                "Once Require API key is on and more than one key may call the conversions, each \
                 caller keeps room for the others",
            )
        };
        return format!(
            "One caller may use all {} at once: {why}. lmgw's own Chat and knowledge-base reads \
             wait behind its batch. {then}.",
            p.slots
        );
    }
    if p.slots <= 1 {
        return format!(
            "{} callers may call the conversions, but with one run at once no room can be kept \
             for a second caller: each waits for the run before it.",
            c.callers
        );
    }
    let mut text = format!(
        "{} callers may call the conversions, so one caller (an API key, all requests without \
         a key together, or lmgw's own Chat and knowledge-base reads) runs at most {} of them \
         at once and a caller with nothing running goes first, so another caller's next step \
         can always start",
        c.callers, p.per_caller_slots
    );
    if p.memory_counted {
        let ch = &p.derived_from.charges;
        text.push_str(&format!(
            "; with the memory one more step like its own leaves free: a PDF's text {} at \
             once, a page's pictures cut out {}, a page of text recognition {}, a reader run {}",
            ch.pdf_file.per_caller,
            ch.page_crops.per_caller,
            ch.ocr_page.per_caller,
            ch.reader_run.per_caller
        ));
    }
    text.push('.');
    text
}

/// "Parallel tool runs": the size, what it is derived from, what runs and
/// waits now, and the size by hand.
#[component]
pub(super) fn PoolRow(pool: PoolFacts, callers: CallerFacts) -> impl IntoView {
    let d = use_docs();
    let toasts = use_toasts();
    let typed = RwSignal::new(if pool.by_hand {
        pool.slots.to_string()
    } else {
        String::new()
    });
    let saving = RwSignal::new(false);
    let save = move |slots: u32| {
        saving.set(true);
        spawn_local(async move {
            let body = json!({ "documents_tool_slots": slots });
            match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
                Ok(_) => {
                    toasts.ok(if slots == 0 {
                        "Parallel tool runs follow this computer again".to_string()
                    } else {
                        format!("Parallel tool runs set to {slots}")
                    });
                    d.load(false);
                }
                Err(e) => toasts.err(e.to_string()),
            }
            saving.try_set(false);
        });
    };
    let set = move |_| match typed.get_untracked().trim().parse::<u32>() {
        Ok(n) if n > 0 => save(n),
        _ => toasts.err("Type a whole number above 0, or use the computer's size".to_string()),
    };
    let now = format!(
        "Running now: {}, waiting: {}, counting {}. Each file's tool runs and reads take one; \
         files and callers take turns.",
        pool.running,
        pool.waiting,
        fmt_bytes(pool.charged_bytes)
    );
    let charges = charges_said(&pool);
    let share = share_said(&pool, &callers);
    view! {
        <div class="prow doc-row" data-pool="">
            <div class="prow-text">
                <span class="prow-title">"Parallel tool runs"</span>
                <span class="prow-sub">{pool_said(&pool)}</span>
                {charges.map(|c| view! { <span class="prow-sub">{c}</span> })}
                <span class="prow-sub" data-share="">{share}</span>
                <span class="prow-sub">{now}</span>
            </div>
            <div class="prow-end">
                <input
                    class="input mono-sm w-num"
                    type="number"
                    min="1"
                    aria-label="Parallel tool runs"
                    placeholder=pool.derived.to_string()
                    prop:value=move || typed.get()
                    on:input=move |ev| typed.set(event_target_value(&ev))
                />
                <button type="button" class="btn sm" disabled=move || saving.get() on:click=set>
                    "Set"
                </button>
                {pool.by_hand.then(|| view! {
                    <button
                        type="button"
                        class="btn sm flat"
                        disabled=move || saving.get()
                        on:click=move |_| save(0)
                    >
                        "Use this computer's size"
                    </button>
                })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pool_says_its_size_and_what_it_comes_from() {
        use lmgw_api_types::documents::{PoolCharges, StepCharge};
        let step = |gib: u64, fit| StepCharge {
            bytes: gib << 30,
            fit,
            per_caller: fit - 1,
        };
        let mut p = PoolFacts {
            slots: 8,
            derived: 8,
            derived_from: lmgw_api_types::documents::PoolInputs {
                cores: 8,
                memory_bytes: Some(16 << 30),
                memory_source: Some(MemorySource::Physical),
                charges: PoolCharges {
                    pdf_file: StepCharge {
                        bytes: 5 << 29,
                        fit: 6,
                        per_caller: 5,
                    },
                    page_crops: step(6, 2),
                    ocr_page: step(2, 7),
                    reader_run: step(5, 3),
                },
            },
            by_hand: false,
            memory_counted: true,
            charged_bytes: 0,
            running: 0,
            waiting: 0,
            per_caller_slots: 7,
        };
        assert_eq!(
            pool_said(&p),
            "8 at once: 8 from 8 cores, as far as 16.0 GiB of memory holds them."
        );
        assert_eq!(
            charges_said(&p).unwrap(),
            "Each step counts the most memory its tools may use: a PDF's text 2.5 GiB (6 at \
             once), a page's pictures cut out 6.0 GiB (2 at once), a page of text recognition \
             2.0 GiB (7 at once), a web page, e-mail or picture read by lmgw's reader 5.0 GiB (3 \
             at once)."
        );
        p.slots = 2;
        p.by_hand = true;
        p.memory_counted = false;
        p.derived_from.memory_bytes = None;
        assert_eq!(
            pool_said(&p),
            "2 at once, set by hand, whatever memory they use; this computer gives 8 from 8 \
             cores and memory not read on this system yet."
        );
        assert_eq!(charges_said(&p), None);
        let mut c = CallerFacts {
            sign_in: true,
            callers: 3,
            shared: true,
        };
        p.per_caller_slots = 1;
        assert!(
            share_said(&p, &c).starts_with("3 callers may call the conversions, so one caller"),
            "{}",
            share_said(&p, &c)
        );
        p.memory_counted = true;
        assert!(
            share_said(&p, &c).ends_with(
                "a PDF's text 5 at once, a page's pictures cut out 1, a page of text \
                 recognition 6, a reader run 2."
            ),
            "{}",
            share_said(&p, &c)
        );
        p.slots = 1;
        assert!(share_said(&p, &c).contains("no room can be kept"));
        c.shared = false;
        c.callers = 1;
        assert!(share_said(&p, &c).contains("only one key may call the conversions"));
        assert!(share_said(&p, &c)
            .ends_with("With a second key, each caller keeps room for the others."));
        c.sign_in = false;
        let off = share_said(&p, &c);
        assert!(off.contains("Require API key is off"), "{off}");
        // Review N-c: with sign-in off a second key alone changes nothing.
        assert!(!off.contains("With a second key"), "{off}");
        assert!(
            off.ends_with(
                "Once Require API key is on and more than one key may call the conversions, \
                 each caller keeps room for the others."
            ),
            "{off}"
        );
    }
}
