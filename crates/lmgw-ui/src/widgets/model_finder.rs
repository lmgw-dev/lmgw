//! Find a model by its name, sized to this computer's graphics card
//! (find-a-model design §5, §6, §8, WP6): the box, the one card, "Get it"
//! with its progress from the jobs feed, and "Say hello".
//!
//! [`ModelFinder`] is one component its hosts mount (D33): the Models
//! page's `/models/find` today, part J's Chat empty state later (§8.3). It
//! reads only `model_find`, `hf_repo`, `model_get`, `model_get_status`, the
//! jobs frame and the machine state, and writes only `model_get`, a job's
//! cancel and, after a confirm, the GPU memory budget (D9); what "Say
//! hello" does is the host's ([`ReadyModel`]).
//!
//! The card comes by size from `model_find` in about a second, and the
//! header's marks after it from `hf_repo` with `fit=unless_metered` on the
//! card's upload (D123): on a metered connection nothing is read by itself,
//! and "Check exactly" asks with `fit=true` ([`shown`] merges the two and a
//! file chosen by hand).

use leptos::prelude::*;
use lmgw_api_types::model_find::{FindAnswer, ModelFind, ModelFindArgs};
use lmgw_api_types::model_get::{ModelGetDone, ModelGetStarted};
use lmgw_api_types::RepoFiles;

mod card;
mod lists;
mod no_card;
mod paging;
mod progress;
mod share;
mod shown;
mod words;

use crate::scope::Scope;
use paging::ListState;
use shown::Choice;

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// A model the finder set up, handed to the host's "Say hello" (§5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyModel {
    /// The row's id in its class.
    pub id: String,
    /// The name it answers to on `/v1` (D27): what a Chat thread names.
    pub public_name: String,
    /// `chat` or `aux`.
    pub class: String,
    /// The card's name ("Qwen3.6 27B").
    pub display_name: String,
}

/// Where Get it is (§6.1).
#[derive(Debug, Clone, PartialEq)]
enum Getting {
    Idle,
    /// `model_get` is on its way.
    Starting,
    Running(ModelGetStarted),
    Done(ModelGetDone),
    Failed(String),
    Canceled,
}

/// What the search answered.
#[derive(Debug, Clone, PartialEq)]
enum Found {
    Idle,
    Searching,
    Answer(Box<ModelFind>),
    Failed(String),
}

/// The finder's state, shared by its parts.
#[derive(Clone, Copy)]
struct Finder {
    text: RwSignal<String>,
    /// The query the answer is for.
    asked: RwSignal<String>,
    found: RwSignal<Found>,
    marks: RwSignal<Option<RepoFiles>>,
    marks_busy: RwSignal<bool>,
    marks_err: RwSignal<Option<String>>,
    choice: RwSignal<Choice>,
    get: RwSignal<Getting>,
    /// The two lists, each on its own hub page (D97).
    others: RwSignal<ListState>,
    dym: RwSignal<ListState>,
    lists_busy: RwSignal<bool>,
    /// When the hub's limit resets, in ms since the epoch (§4.5).
    wait_until: RwSignal<Option<f64>>,
    now: RwSignal<f64>,
    /// The file list in its pick-only mode is open (D34).
    picking: RwSignal<bool>,
    /// Get it asked once more because the disk is short (D125).
    disk_ask: RwSignal<bool>,
    /// Answers for a search that was replaced end there.
    generation: StoredValue<u32>,
    scope: Scope,
    toasts: crate::widgets::Toasts,
    on_ready: Callback<ReadyModel>,
    on_search: Option<Callback<String>>,
}

impl Finder {
    /// Seconds until the hub's limit resets, while it must be waited for.
    fn waiting_secs(self) -> Option<u64> {
        let until = self.wait_until.get()?;
        let left = ((until - self.now.get()) / 1000.0).ceil();
        (left > 0.0).then_some(left as u64)
    }

    /// The limit's wait from an answer (§4.5): the hub said none are left,
    /// or fewer than the search took.
    fn note_budget(self, a: &ModelFind) {
        let limited = a.answer == FindAnswer::RateLimited;
        let b = a.hub_budget.clone().unwrap_or_default();
        if words::must_wait(limited, b.remaining, b.calls) {
            let secs = b.reset_s.unwrap_or(if limited { 60 } else { 0 });
            if secs > 0 {
                self.now.set(now_ms());
                self.wait_until.set(Some(now_ms() + secs as f64 * 1000.0));
                return;
            }
        }
        self.wait_until.set(None);
    }

    /// Search for `q` (§4): the card by size, then its header's marks.
    fn search(self, q: String) {
        let q = q.trim().to_string();
        if q.is_empty() || self.waiting_secs_untracked().is_some() {
            return;
        }
        self.generation.update_value(|g| *g += 1);
        let gen = self.generation.get_value();
        self.text.set(q.clone());
        self.asked.set(q.clone());
        self.found.set(Found::Searching);
        self.marks.set(None);
        self.marks_busy.set(false);
        self.marks_err.set(None);
        self.choice.set(Choice::Pick);
        self.get.set(Getting::Idle);
        self.disk_ask.set(false);
        self.others.set(ListState::default());
        self.dym.set(ListState::default());
        self.lists_busy.set(false);
        if let Some(cb) = self.on_search {
            cb.run(q.clone());
        }
        self.scope.spawn(async move {
            let args = ModelFindArgs {
                query: q,
                ..ModelFindArgs::default()
            };
            let res = crate::api::post::<ModelFind, _>("/api/op/model_find", &args).await;
            if self.generation.get_value() != gen {
                return;
            }
            match res {
                Ok(a) => {
                    self.note_budget(&a);
                    self.others.set(ListState::first(
                        a.others.clone(),
                        a.others_more,
                        a.others_total,
                        a.next_cursor.clone(),
                    ));
                    self.dym.set(ListState::first(
                        a.did_you_mean.clone(),
                        a.did_you_mean_more,
                        a.did_you_mean_total,
                        a.next_cursor.clone(),
                    ));
                    let ask = a.card.as_ref().filter(|c| {
                        !c.upload.is_empty()
                            && !c.file.is_empty()
                            && c.fit.mark != lmgw_api_types::model_find::FitMark::NoCard
                    });
                    let ask = ask.map(|c| (c.upload.clone(), words::target_of(c.kind)));
                    self.found.set(Found::Answer(Box::new(a)));
                    if let Some((upload, target)) = ask {
                        self.ask_marks(upload, target, "unless_metered");
                    }
                }
                Err(e) => self.found.set(Found::Failed(e.to_string())),
            }
        });
    }

    /// The card the finder shows, which neither list repeats.
    fn card_origin(self) -> Option<String> {
        self.found.with_untracked(|f| match f {
            Found::Answer(a) => a.card.as_ref().map(|c| c.origin.clone()),
            _ => None,
        })
    }

    fn waiting_secs_untracked(self) -> Option<u64> {
        let until = self.wait_until.get_untracked()?;
        let left = ((until - now_ms()) / 1000.0).ceil();
        (left > 0.0).then_some(left as u64)
    }

    /// The header's marks on the card's upload (D123): `unless_metered` by
    /// itself, `true` for "Check exactly".
    fn ask_marks(self, upload: String, target: &'static str, mode: &'static str) {
        let gen = self.generation.get_value();
        self.marks_busy.set(true);
        self.marks_err.set(None);
        self.scope.spawn(async move {
            let res = crate::api::get::<RepoFiles>(format!(
                "/api/hf/repo?repo={}&target={target}&fit={mode}",
                urlenc(&upload),
            ))
            .await;
            if self.generation.get_value() != gen {
                return;
            }
            self.marks_busy.set(false);
            match res {
                Ok(a) => self.marks.set(Some(a)),
                Err(e) => self.marks_err.set(Some(e.to_string())),
            }
        });
    }

    /// "Show more" on a list, or the hub's next page once its page is shown
    /// whole (D142, D97): the same search again with that list's window,
    /// and the other list as it is.
    fn more(self, others: bool) {
        if self.lists_busy.get_untracked() || self.waiting_secs_untracked().is_some() {
            return;
        }
        let list = if others { self.others } else { self.dym };
        let Some(ask) = list.with_untracked(ListState::ask) else {
            return;
        };
        let args = paging::args(&self.asked.get_untracked(), &ask, others);
        let shown = self.card_origin();
        let gen = self.generation.get_value();
        self.lists_busy.set(true);
        self.scope.spawn(async move {
            let res = crate::api::post::<ModelFind, _>("/api/op/model_find", &args).await;
            if self.generation.get_value() != gen {
                return;
            }
            self.lists_busy.set(false);
            match res {
                Ok(a) => {
                    self.note_budget(&a);
                    if a.answer == FindAnswer::RateLimited {
                        return;
                    }
                    let (items, more, total, lead) = if others {
                        let lead = a.card.as_ref().map(paging::card_choice);
                        (a.others, a.others_more, a.others_total, lead)
                    } else {
                        let d = a.did_you_mean;
                        (d, a.did_you_mean_more, a.did_you_mean_total, None)
                    };
                    let next = a.next_cursor;
                    list.update(|l| {
                        l.apply(&ask, items, more, total, next, lead, shown.as_deref())
                    });
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }
}

#[component]
pub fn ModelFinder(
    /// A query the host fills the box with and searches (a link's `?q=`).
    #[prop(into, default = Signal::stored(None))]
    query: Signal<Option<String>>,
    /// The narrow layout, for a host with little room (§8.3).
    #[prop(optional)]
    compact: bool,
    /// "Say hello" on a chat model it set up: the host opens a chat.
    on_ready: Callback<ReadyModel>,
    /// Every search's query, for a host that keeps it in its address.
    #[prop(optional)]
    on_search: Option<Callback<String>>,
) -> impl IntoView {
    let f = Finder {
        text: RwSignal::new(String::new()),
        asked: RwSignal::new(String::new()),
        found: RwSignal::new(Found::Idle),
        marks: RwSignal::new(None),
        marks_busy: RwSignal::new(false),
        marks_err: RwSignal::new(None),
        choice: RwSignal::new(Choice::Pick),
        get: RwSignal::new(Getting::Idle),
        others: RwSignal::new(ListState::default()),
        dym: RwSignal::new(ListState::default()),
        lists_busy: RwSignal::new(false),
        wait_until: RwSignal::new(None),
        now: RwSignal::new(now_ms()),
        picking: RwSignal::new(false),
        disk_ask: RwSignal::new(false),
        generation: StoredValue::new(0),
        scope: Scope::new(),
        toasts: crate::widgets::use_toasts(),
        on_ready,
        on_search,
    };

    // The host's query: searched when it names something else than the
    // answer shown.
    Effect::new(move |_| {
        if let Some(q) = query.get().filter(|q| !q.trim().is_empty()) {
            if q.trim() != f.asked.get_untracked() {
                f.search(q);
            }
        }
    });

    // The limit's countdown: a tick a second while it runs, none after.
    let ticker = StoredValue::new(None::<IntervalHandle>);
    let stop_tick = move || {
        if let Some(h) = ticker.try_update_value(Option::take).flatten() {
            h.clear();
        }
    };
    on_cleanup(stop_tick);
    Effect::new(move |_| {
        let until = f.wait_until.get();
        stop_tick();
        if until.is_none() {
            return;
        }
        f.now.set(now_ms());
        if let Ok(h) = set_interval_with_handle(
            move || {
                let n = now_ms();
                f.now.set(n);
                if f.wait_until.get_untracked().is_none_or(|u| n >= u) {
                    f.wait_until.set(None);
                }
            },
            std::time::Duration::from_secs(1),
        ) {
            ticker.set_value(Some(h));
        }
    });

    let wait = Memo::new(move |_| f.waiting_secs());
    let submit = move || f.search(f.text.get_untracked());
    // The limit's words for the button's tooltip: what the hub allows, with
    // or without the token, as the answer said it.
    let allowance = move || {
        f.found.with(|x| match x {
            Found::Answer(a) => a.hub_budget.as_ref().and_then(|b| b.allowance.clone()),
            _ => None,
        })
    };

    view! {
        <div class="mf" class:compact=compact>
            <div class="mf-box">
                <input
                    class="input mf-input"
                    type="search"
                    placeholder="A model's name, like qwen 3.6, or a Hugging Face address"
                    aria-label="Model name or Hugging Face address"
                    prop:value=move || f.text.get()
                    on:input=move |ev| f.text.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            submit();
                        }
                    }
                />
                <button
                    class="btn primary mf-find"
                    disabled=move || {
                        wait.get().is_some() || matches!(f.found.get(), Found::Searching)
                            || f.text.with(|t| t.trim().is_empty())
                    }
                    title=move || wait.get().map(|_| words::limit_tip(allowance().as_deref()))
                    on:click=move |_| submit()
                >
                    {move || match (wait.get(), f.found.with(|x| matches!(x, Found::Searching))) {
                        (Some(s), _) => words::limit_wait(s),
                        (None, true) => "Finding…".to_string(),
                        (None, false) => "Find".to_string(),
                    }}
                </button>
            </div>
            {move || match f.found.get() {
                Found::Idle => view! {
                    <p class="mf-hint dim">
                        "Type the name of a model someone told you about. lmgw looks it up on Hugging Face, picks the file that fits your graphics card, and sets it up. Names are searched as written, with numbers as digits."
                    </p>
                }
                    .into_any(),
                Found::Searching => view! {
                    <p class="mf-hint dim">"Asking Hugging Face…"</p>
                }
                    .into_any(),
                Found::Failed(e) => view! {
                    <div class="notice err mf-notice">
                        "lmgw couldn't search just now. Find asks again."
                        <Raw raw=e/>
                    </div>
                }
                    .into_any(),
                Found::Answer(a) => view! { <Answered f=f a=*a/> }.into_any(),
            }}
        </div>
    }
}

/// What went wrong, in the words it came in, folded under a plain sentence.
#[component]
fn Raw(raw: String) -> impl IntoView {
    view! {
        <details class="mf-fold mf-raw">
            <summary>"What went wrong"</summary>
            <p class="mono dim">{raw}</p>
        </details>
    }
}

/// What the search answered: its sentence when it isn't a plain card, the
/// card, and the two lists.
#[component]
fn Answered(f: Finder, a: ModelFind) -> impl IntoView {
    let plain_card = a.answer == FindAnswer::Card;
    let tone = match a.answer {
        FindAnswer::RateLimited | FindAnswer::Unreachable | FindAnswer::AllGated => "notice warn",
        _ => "notice",
    };
    // The limit's wait says the number the Find button counts down (§4.5).
    let limited = a.answer == FindAnswer::RateLimited;
    let allowance = a.hub_budget.as_ref().and_then(|b| b.allowance.clone());
    let said = a.sentence.clone();
    let text = move || match (limited, f.waiting_secs()) {
        (true, Some(s)) => words::limit_sentence(Some(s), allowance.as_deref()),
        (true, None) => words::limit_sentence(None, allowance.as_deref()),
        (false, _) => said.clone(),
    };
    let sentence = (!plain_card && !a.sentence.is_empty()).then(|| {
        let other_kind = a.answer == FindAnswer::OtherKind;
        view! {
            <div class=format!("{tone} mf-notice")>
                {text}
                {other_kind.then(|| view! {
                    " " <a href="/models">"Open Models"</a>
                })}
            </div>
        }
    });
    let near = a
        .hub_budget
        .as_ref()
        .filter(|b| b.near && a.answer != FindAnswer::RateLimited)
        .and_then(|b| b.sentence.clone())
        .map(|s| view! { <p class="mf-line dim">{s}</p> });
    let notes = (!a.notes.is_empty()).then(|| {
        let notes = a.notes.clone();
        view! {
            <details class="mf-fold mf-notes">
                <summary>"What lmgw checked on Hugging Face"</summary>
                <ul>{notes.into_iter().map(|n| view! { <li>{n}</li> }).collect_view()}</ul>
            </details>
        }
    });
    let card = a
        .card
        .clone()
        .map(|c| view! { <card::FinderCard f=f card=c answer=a.answer/> });
    view! {
        {sentence}
        {near}
        {card}
        <lists::ChoiceList f=f others=true/>
        <lists::ChoiceList f=f others=false/>
        {notes}
    }
}
