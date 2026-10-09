//! MCP Tasks in the Chat (MCP Tasks design §6): what a tool the server runs
//! as a job looks like while it runs and once its result is in.
//!
//! - **A result row** (a message of role `tool`) is a card in the
//!   transcript: "Job 7f3a · desktop__run_command · completed", the result
//!   folded like a tool call's output, its time; failed, cancelled and
//!   abandoned in their colours ([`TaskResultView`]). A result no reply has
//!   answered yet says so under its card.
//! - **The thread's tasks** (`GET /chat/api/threads/{id}`'s `tasks`) are a
//!   strip above the composer ([`TaskStrip`]): each job's tool, label,
//!   status and status message, since when it runs, what it waits for, and
//!   Cancel, which asks first naming the tool and then really cancels
//!   (`POST …/tasks/{task}/cancel`). An ended job whose result is on its
//!   way in stays listed until it entered.
//! - **Answer** sits beside Send while a result waits for an answer
//!   ([`needs_answer`]); it streams a continuation (`POST …/answer`) into a
//!   new reply, and a refusal is worded under the strip.
//!
//! **Freshness.** The thread's tasks are read with the thread: when it
//! opens and whenever the feed names it (a job started, a result entered).
//! A status move (`working` ↔ `input_required`, a new status message) is
//! not in the feed (design §4.1), so while a job runs the strip reads the
//! thread again every `mcp.task_poll_interval_s` (Settings → MCP, the
//! interval lmgw itself polls at when the server suggests none) and says so
//! in its head; when that setting cannot be read, it says that instead and
//! reads nothing on its own.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::chat::{task_code, MessageTask, TaskCancelled, ThreadTask};
use lmgw_api_types::SettingsFull;
use serde_json::json;

use super::chat::{parse_utc_ts, scroll_down, Msg};
use super::chat_actions::{ActionEnv, MsgActions, MsgOps};
use super::chat_turn::{run_turn, Turn};
use crate::scope::Scope;
use crate::widgets::{ConfirmButton, Toasts};

/// The open thread's tasks, tagged with the thread they were read for: a
/// read that lands after the owner moved on shows nowhere.
pub(super) type Tasks = RwSignal<Option<(i64, Vec<ThreadTask>)>>;

/// Thread `tid`'s tasks as read; set only where they differ.
pub(super) fn take(tasks: Tasks, tid: i64, list: Vec<ThreadTask>) {
    let differs = tasks.with_untracked(|t| match t {
        Some((at, l)) => *at != tid || *l != list,
        None => true,
    });
    if differs {
        tasks.set(Some((tid, list)));
    }
}

// ---------------------------------------------------------------------------
// Pure reading
// ---------------------------------------------------------------------------

/// A status the server still works on: the job can be cancelled.
pub(super) fn running(status: &str) -> bool {
    matches!(status, "working" | "input_required")
}

/// A status in words.
pub(super) fn status_words(status: &str) -> &str {
    match status {
        "input_required" => "needs input",
        other => other,
    }
}

/// What a result card's summary names.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Facts {
    pub job: String,
    pub tool: String,
    pub status: String,
}

/// The facts of a result row: its `task`, else read from its text's first
/// line, `job <task id> (<tool>) <status>`, which lmgw writes on every
/// result (a row whose `task` does not read still shows what ended).
pub(super) fn facts_of(task: Option<&MessageTask>, content: &str) -> Facts {
    if let Some(t) = task {
        return Facts {
            job: t.task_id.clone(),
            tool: t.tool.clone(),
            status: t.status.clone(),
        };
    }
    let first = content.lines().next().unwrap_or_default();
    let parsed = first.strip_prefix("job ").and_then(|rest| {
        let (job, rest) = rest.split_once(" (")?;
        let (tool, status) = rest.split_once(") ")?;
        Some(Facts {
            job: job.to_string(),
            tool: tool.to_string(),
            status: status.trim().to_string(),
        })
    });
    parsed.unwrap_or(Facts {
        job: "?".into(),
        tool: "?".into(),
        status: "ended".into(),
    })
}

/// The result below the summary: the row's text without its first line
/// when that line is the summary's own (`job <id> (…) …`); the whole text
/// otherwise. A result with nothing after that line says so.
pub(super) fn body_of(content: &str, job: &str) -> String {
    let head = format!("job {job} (");
    match content.split_once('\n') {
        Some((first, rest)) if first.starts_with(&head) => {
            if rest.trim().is_empty() {
                "(no output)".into()
            } else {
                rest.to_string()
            }
        }
        None if content.starts_with(&head) => "(no output)".into(),
        _ => content.to_string(),
    }
}

/// Whether a result waits for an answer, as `POST …/answer` judges it: a
/// result row with no reply after the last one (`roles`: the stored
/// messages' roles, in order), or a job that ended and whose result enters
/// the thread the moment a turn starts or a message is sent (one nothing
/// holds off).
pub(super) fn needs_answer(roles: &[&str], tasks: &[ThreadTask]) -> bool {
    let row_waits = roles
        .iter()
        .rposition(|r| *r == "tool")
        .is_some_and(|i| !roles[i..].contains(&"assistant"));
    row_waits
        || tasks
            .iter()
            .any(|t| !running(&t.status) && t.waiting_for.is_none())
}

/// A refused Answer, in words, by the route's code.
pub(super) fn answer_refusal(code: Option<&str>, message: &str) -> String {
    match code {
        Some(task_code::NOTHING_TO_ANSWER) => "Nothing to answer: every job result already has \
            a reply after it (another window or a device may have answered it)."
            .into(),
        Some(task_code::TURN_RUNNING) => "A reply in this conversation is being written \
            elsewhere (another window, a device or a voice session); it may answer the results. \
            Answer once it is done, if it did not."
            .into(),
        _ => format!("Answer did not start: {message}"),
    }
}

/// A refused cancel, in words, by the route's code, and whether the job
/// goes on (said as a warning rather than an error).
pub(super) fn cancel_refusal(code: &str, message: &str, tool: &str) -> (String, bool) {
    match code {
        task_code::CANCEL_UNSUPPORTED => (
            format!("{tool} cannot be cancelled: {message}. The job goes on."),
            true,
        ),
        task_code::CANCEL_REFUSED => (format!("Cancelling {tool} was refused: {message}"), true),
        task_code::TASK_ENDED => (
            format!("{tool} already ended; its result enters the conversation."),
            true,
        ),
        task_code::TASK_NOT_FOUND => (
            format!("{tool} is no longer running here: its result is in the conversation."),
            true,
        ),
        _ => (format!("Cancelling {tool} failed: {message}"), false),
    }
}

/// The strip's head: how many jobs run, and how many results are on their
/// way in.
pub(super) fn strip_head(tasks: &[ThreadTask]) -> String {
    let run = tasks.iter().filter(|t| running(&t.status)).count();
    let ended = tasks.len() - run;
    let jobs = |n: usize| if n == 1 { "job" } else { "jobs" };
    match (run, ended) {
        (r, 0) => format!("{r} {} running", jobs(r)),
        (0, e) => format!(
            "{e} {} ended, {} on {} way in",
            jobs(e),
            if e == 1 {
                "its result"
            } else {
                "their results"
            },
            if e == 1 { "its" } else { "their" }
        ),
        (r, e) => format!("{r} {} running · {e} ended, on the way in", jobs(r)),
    }
}

/// "14:02:11 · 3m" for a job that started at `started_at` (UTC as stored),
/// as of `now_ms`.
fn since_text(started_at: &str, now_ms: f64) -> String {
    let at = crate::fmt::log_time(started_at).time;
    match parse_utc_ts(started_at) {
        Some(s) => {
            let secs = (now_ms / 1000.0 - s).max(0.0) as u64;
            format!("since {at} · {}", crate::fmt::age(secs))
        }
        None => format!("since {at}"),
    }
}

/// A stated ttl in words ("the server keeps it 1h 0m").
fn ttl_text(ttl_ms: Option<i64>) -> String {
    match ttl_ms {
        Some(ms) if ms >= 0 => format!(
            "the server keeps it {} after it ends",
            crate::fmt::age((ms / 1000) as u64)
        ),
        _ => "the server stated no ttl".into(),
    }
}

// ---------------------------------------------------------------------------
// The strip's state and its reads
// ---------------------------------------------------------------------------

/// What the strip and Answer need of the page. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct TaskEnv {
    pub tasks: Tasks,
    pub current_id: Memo<Option<i64>>,
    pub msgs: RwSignal<Vec<Msg>>,
    /// A reply streams on this page (anywhere): Answer waits (Cancel does
    /// not: it is a request to the job's server, not a turn).
    pub busy: Signal<bool>,
    pub scope: Scope,
    pub toasts: Toasts,
    /// The page's message actions, for Answer's stream.
    pub actions: ActionEnv,
}

/// The strip's own state: the cancels on their way, the last read's time,
/// the poll interval, and a refused Answer's words.
#[derive(Clone, Copy)]
struct StripState {
    cancelling: RwSignal<Vec<i64>>,
    /// When the tasks were last read (ms), for the ages and the head.
    read_at: RwSignal<f64>,
    /// Settings → MCP's `task_poll_interval_s`; `Err` when it could not be
    /// read (said in the head).
    poll_s: RwSignal<Option<Result<u32, String>>>,
    said: RwSignal<Option<String>>,
    /// The interval's read is on its way.
    poll_busy: RwSignal<bool>,
}

/// Read Settings → MCP's `task_poll_interval_s` into `st.poll_s`, unless a
/// read is on its way.
fn read_interval(env: TaskEnv, st: StripState) {
    if st.poll_busy.get_untracked() {
        return;
    }
    st.poll_busy.set(true);
    st.poll_s.set(Some(Err("reading…".into())));
    env.scope.spawn(async move {
        let got = crate::api::get::<SettingsFull>("/api/settings-full")
            .await
            .map(|s| s.mcp.task_poll_interval_s)
            .map_err(|e| e.to_string());
        st.poll_s.try_set(Some(match got {
            Ok(0) => Err("Settings → MCP holds no interval".into()),
            other => other,
        }));
        st.poll_busy.try_set(false);
    });
}

impl TaskEnv {
    /// Read thread `tid`'s tasks again (`GET …/tasks`, not the history) and
    /// take them.
    fn read(self, tid: i64, read_at: RwSignal<f64>) {
        self.scope.spawn(async move {
            let Ok(tasks) =
                crate::api::get::<Vec<ThreadTask>>(format!("/chat/api/threads/{tid}/tasks")).await
            else {
                // The follower says a failed read of the open thread; the
                // strip keeps what it shows until the next read.
                return;
            };
            if self.current_id.get_untracked() == Some(tid) {
                take(self.tasks, tid, tasks);
                read_at.set(js_sys::Date::now());
            }
        });
    }

    /// The open thread's roles, as stored (a reply the server refused to
    /// store is no row).
    fn needs_answer_now(self) -> bool {
        let tid = self.current_id.get();
        let roles: Vec<String> = self.msgs.with(|v| {
            v.iter()
                .filter(|m| !m.unsaved.get())
                .map(|m| m.role.clone())
                .collect()
        });
        let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
        self.tasks.with(|t| {
            let list = t
                .as_ref()
                .filter(|(at, _)| Some(*at) == tid)
                .map(|(_, l)| l.as_slice())
                .unwrap_or_default();
            needs_answer(&roles, list)
        })
    }
}

impl ActionEnv {
    /// `POST …/answer`: a continuation streams into a new reply at the end
    /// of the transcript. A refusal before any frame takes the reply back
    /// and is worded in `said`.
    pub(super) fn answer(&self, said: RwSignal<Option<String>>) {
        if self.busy() {
            return;
        }
        let Some(tid) = self.turn.current_id.get_untracked() else {
            return;
        };
        let Some(reply) = self.new_reply() else {
            return;
        };
        said.set(None);
        let env = *self;
        let key = reply.key;
        self.turn.msgs.update(|v| v.push(reply.clone()));
        scroll_down(true);
        let turn = Turn {
            tid,
            url: format!("/chat/api/threads/{tid}/answer"),
            body: json!({}),
            target: reply,
            continuing: false,
            what: "answer",
            inline: true,
        };
        spawn_local(async move {
            let end = run_turn(env.turn, turn, |_| {}, || {}).await;
            if let Some(r) = &end.refusal {
                env.turn.msgs.try_update(|v| v.retain(|m| m.key != key));
                said.try_set(Some(answer_refusal(r.code.as_deref(), &r.message)));
            }
            if env.turn.scope.alive() {
                env.resync(tid, false);
                env.refresh.run(());
            }
        });
    }
}

/// Cancel job `t` of thread `tid`, the owner having confirmed.
fn cancel(env: TaskEnv, st: StripState, tid: i64, t: ThreadTask) {
    if st.cancelling.with_untracked(|c| c.contains(&t.id)) {
        return;
    }
    st.cancelling.update(|c| c.push(t.id));
    env.scope.spawn(async move {
        let url = format!("/chat/api/threads/{tid}/tasks/{}/cancel", t.id);
        match crate::api::post::<TaskCancelled, _>(url, &json!({})).await {
            Ok(c) => env.toasts.ok(format!("{}: {}", t.tool, c.note)),
            Err(crate::api::Error::Api(e)) => {
                let (words, goes_on) = cancel_refusal(&e.code, &e.message, &t.tool);
                if goes_on {
                    env.toasts.warn(words);
                } else {
                    env.toasts.err(words);
                }
            }
            Err(e) => env.toasts.err(format!("Cancelling {} failed: {e}", t.tool)),
        }
        st.cancelling.update(|c| c.retain(|id| *id != t.id));
        env.read(tid, st.read_at);
    });
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// A late result in the transcript: the card, folded, and under it whether
/// a reply answered it yet.
#[component]
pub(super) fn TaskResultView(m: Msg, ops: MsgOps) -> impl IntoView {
    let key = m.key;
    let (content, task, at, db_id) = (m.content, m.task, m.created_at, m.db_id);
    let facts = Memo::new(move |_| content.with(|c| task.with(|t| facts_of(t.as_ref(), c))));
    let body = Memo::new(move |_| content.with(|c| body_of(c, &facts.with(|f| f.job.clone()))));
    let label = Memo::new(move |_| task.with(|t| t.as_ref().map(|t| t.server_label.clone())));
    let when = Memo::new(move |_| {
        at.with(|a| {
            a.as_deref().filter(|a| !a.is_empty()).map(|a| {
                let t = crate::fmt::log_time(a);
                let today = crate::fmt::log_time(&today_utc()).day;
                let shown = if t.day == today {
                    t.time.clone()
                } else {
                    format!("{} {}", t.day_label, t.time)
                };
                (shown, t.utc)
            })
        })
    });
    // No stored reply after it: the model has not answered it yet.
    let unanswered = Memo::new(move |_| {
        ops.msgs.with(|v| {
            let Some(i) = v.iter().position(|x| x.key == key) else {
                return false;
            };
            !v[i + 1..]
                .iter()
                .any(|x| x.role == "assistant" && !x.unsaved.get())
        })
    });
    let editing = RwSignal::new(false);
    let status = move || facts.with(|f| f.status.clone());
    view! {
        <div class="msg-item msg-job" data-mid=move || db_id.get().map(|i| i.to_string())>
            <details class=move || format!("tool-card job-card {}", status())>
                <summary>
                    <span class="job-sum">
                    <span class="job-head">
                        "Job "
                        <span class="mono-sm">{move || facts.with(|f| f.job.clone())}</span>
                        " · "
                        <span class="mono-sm">{move || facts.with(|f| f.tool.clone())}</span>
                        " · "
                        <span class=move || format!("tool-state {}", status())>{status}</span>
                    </span>
                    {move || label.get().map(|l| view! { <span class="type-badge">{l}</span> })}
                    {move || {
                        when.get()
                            .map(|(shown, utc)| {
                                view! { <span class="job-when" title=utc>{shown}</span> }
                            })
                    }}
                    </span>
                </summary>
                <div class="tool-io">
                    <div class="dim mini-note">"result"</div>
                    <pre class="preset">{move || body.get()}</pre>
                </div>
            </details>
            <Show when=move || unanswered.get()>
                <div class="tool-note job-unanswered">
                    "No reply has answered this result yet — Answer, beside Send, asks the model."
                </div>
            </Show>
            <MsgActions m=m.clone() ops=ops editing=editing/>
        </div>
    }
}

/// Today's date as a stored UTC timestamp would carry it, for "is this
/// today" in the reader's own time.
fn today_utc() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.get_utc_full_year(),
        d.get_utc_month() + 1,
        d.get_utc_date(),
        d.get_utc_hours(),
        d.get_utc_minutes(),
        d.get_utc_seconds()
    )
}

/// Answer, beside Send: shown while a result waits for an answer.
#[component]
pub(super) fn AnswerButton(env: TaskEnv, said: RwSignal<Option<String>>) -> impl IntoView {
    let needed = Memo::new(move |_| env.needs_answer_now());
    view! {
        <Show when=move || needed.get()>
            <button
                type="button"
                class="btn answer-btn"
                data-answer=""
                disabled=move || env.busy.get()
                title=move || {
                    if env.busy.get() {
                        "a reply is streaming — wait for it or stop it".to_string()
                    } else {
                        "Ask the model to answer the job results no reply has answered yet"
                            .to_string()
                    }
                }
                on:click=move |_| env.actions.answer(said)
            >
                "Answer"
            </button>
        </Show>
    }
}

/// The open thread's jobs, above the composer, and a refused Answer's
/// words.
#[component]
pub(super) fn TaskStrip(env: TaskEnv, said: RwSignal<Option<String>>) -> impl IntoView {
    let st = StripState {
        cancelling: RwSignal::new(Vec::new()),
        read_at: RwSignal::new(js_sys::Date::now()),
        poll_s: RwSignal::new(None),
        said,
        poll_busy: RwSignal::new(false),
    };
    let list = Memo::new(move |_| {
        let tid = env.current_id.get();
        env.tasks.with(|t| {
            t.as_ref()
                .filter(|(at, _)| Some(*at) == tid)
                .map(|(_, l)| l.clone())
                .unwrap_or_default()
        })
    });
    // A refused Answer is about the thread it was refused on.
    Effect::new(move |prev: Option<Option<i64>>| {
        let id = env.current_id.get();
        if prev.is_some_and(|p| p != id) {
            st.said.set(None);
        }
        id
    });
    let any_running = Memo::new(move |_| list.with(|l| l.iter().any(|t| running(&t.status))));
    // Every read of the tasks moves the ages on, the follower's too. While a
    // job runs, the interval is read, and again with the next read of the
    // tasks or the next job that runs once a read of it failed.
    Effect::new(move |_| {
        env.tasks.track();
        st.read_at.set(js_sys::Date::now());
        if any_running.get() && !matches!(st.poll_s.get_untracked(), Some(Ok(_))) {
            read_interval(env, st);
        }
    });
    // A refusal that no longer matters goes when no result waits for an
    // answer ("nothing to answer" stays: it says why nothing happened).
    Effect::new(move |prev: Option<bool>| {
        let needed = env.needs_answer_now();
        if prev == Some(true) && !needed {
            st.said.update(|w| {
                if w.as_ref()
                    .is_some_and(|w| !w.starts_with("Nothing to answer"))
                {
                    *w = None;
                }
            });
        }
        needed
    });
    let poll = StoredValue::new(None::<IntervalHandle>);
    Effect::new(move |_| {
        if let Some(h) = poll.get_value() {
            h.clear();
            poll.set_value(None);
        }
        let (Some(tid), true, Some(Ok(secs))) =
            (env.current_id.get(), any_running.get(), st.poll_s.get())
        else {
            return;
        };
        let tick = move || {
            if !document().hidden() {
                env.read(tid, st.read_at);
            }
        };
        if let Ok(h) = set_interval_with_handle(tick, Duration::from_secs(u64::from(secs))) {
            poll.set_value(Some(h));
        }
    });
    on_cleanup(move || {
        if let Some(h) = poll.try_get_value().flatten() {
            h.clear();
        }
    });
    let freshness = move || {
        let at = crate::fmt::local_datetime(st.read_at.get() / 1000.0);
        let at = at.get(11..).unwrap_or(&at).to_string();
        if !any_running.get() {
            return format!("as of {at}");
        }
        match st.poll_s.get() {
            Some(Ok(s)) => {
                format!("as of {at} · read again every {s} s (Settings → MCP → Task poll interval)")
            }
            Some(Err(e)) => format!(
                "as of {at} · not read again on its own: the poll interval could not be read \
                 ({e}); it is read again when a job starts or ends"
            ),
            None => format!("as of {at}"),
        }
    };
    view! {
        <Show when=move || !list.with(Vec::is_empty)>
            <div class="job-strip" role="region" aria-label="Jobs of this conversation">
                <div class="job-strip-head">
                    <span class="job-count">{move || list.with(|l| strip_head(l))}</span>
                    <span class="dim job-fresh">{freshness}</span>
                </div>
                <For each=move || list.with(|l| l.iter().map(|t| t.id).collect::<Vec<_>>()) key=|id| *id let:id>
                    <TaskRow id=id list=list env=env st=st/>
                </For>
            </div>
        </Show>
        {move || {
            st.said
                .get()
                .map(|w| {
                    view! {
                        <div class="job-said" role="alert">
                            <span>{w}</span>
                            <button
                                type="button"
                                class="btn ghost sm"
                                title="Dismiss"
                                aria-label="Dismiss"
                                on:click=move |_| st.said.set(None)
                            >
                                "✕"
                            </button>
                        </div>
                    }
                })
        }}
    }
}

/// One job of the strip. The row is keyed by the task's id and reads its
/// task from the list by that id, so a poll that changes the status, the
/// message or what it waits for changes the text and keeps the row (and an
/// armed Cancel in it).
#[component]
fn TaskRow(id: i64, list: Memo<Vec<ThreadTask>>, env: TaskEnv, st: StripState) -> impl IntoView {
    // The last task seen under this id stays while the list drops it.
    let task = Memo::new(move |prev: Option<&ThreadTask>| {
        list.with(|l| l.iter().find(|t| t.id == id).cloned())
            .or_else(|| prev.cloned())
            .unwrap_or_default()
    });
    let is_running = Memo::new(move |_| task.with(|t| running(&t.status)));
    let status = move || task.with(|t| t.status.clone());
    let tool = task.with_untracked(|t| t.tool.clone());
    let label = task.with_untracked(|t| t.server_label.clone());
    let started = task.with_untracked(|t| t.started_at.clone());
    let title = move || {
        task.with(|t| {
            format!(
                "job {} on '{}', started {} by {}; {}",
                t.task_id,
                t.server_label,
                crate::fmt::log_time(&t.started_at).utc,
                t.by.clone().unwrap_or_else(|| "the gateway".into()),
                ttl_text(t.ttl_ms),
            )
        })
    };
    let cancel_now = Callback::new(move |()| {
        if let Some(tid) = env.current_id.get_untracked() {
            cancel(env, st, tid, task.get_untracked());
        }
    });
    let off = Signal::derive(move || st.cancelling.with(|c| c.contains(&id)));
    let confirm = format!("Cancel {tool}?");
    view! {
        <div
            class=move || format!("job-row {}", status())
            data-task-id=id.to_string()
            title=title
        >
            <span class="job-dot" class:live=move || is_running.get()></span>
            <span class="mono-sm job-tool">{tool}</span>
            <span class="type-badge">{label}</span>
            <span class=move || format!("tool-state {}", status())>
                {move || status_words(&status()).to_string()}
            </span>
            {move || {
                task.with(|t| t.status_message.clone())
                    .filter(|m| !m.trim().is_empty())
                    .map(|m| view! { <span class="job-msg">{m}</span> })
            }}
            <span class="dim job-since">{move || since_text(&started, st.read_at.get())}</span>
            {move || {
                match (is_running.get(), task.with(|t| t.waiting_for.clone())) {
                    (true, Some(w)) => {
                        view! { <span class="job-wait">{format!("waiting for {w}")}</span> }
                            .into_any()
                    }
                    (false, Some(w)) => {
                        view! {
                            <span class="job-wait">
                                {format!("enters the conversation after {w}")}
                            </span>
                        }
                            .into_any()
                    }
                    (false, None) => {
                        view! { <span class="dim">"its result is entering the conversation"</span> }
                            .into_any()
                    }
                    (true, None) => ().into_any(),
                }
            }}
            <Show when=move || is_running.get()>
                <span class="job-cancel">
                    <ConfirmButton
                        label=move || if off.get() { "Cancelling…" } else { "Cancel" }
                        confirm=confirm.clone()
                        class="btn sm"
                        title="Ask the server to stop this job; its result (cancelled) then enters the conversation"
                        disabled=off
                        on_confirm=cancel_now
                    />
                </span>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str, waiting_for: Option<&str>) -> ThreadTask {
        ThreadTask {
            id: 1,
            task_id: "t1".into(),
            server_label: "desk".into(),
            tool: "desk__build".into(),
            status: status.into(),
            waiting_for: waiting_for.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn a_row_s_facts_come_from_its_task_else_from_its_first_line() {
        let t = MessageTask {
            task_id: "7f3a".into(),
            server_label: "desktop".into(),
            tool: "desktop__run_command".into(),
            status: "failed".into(),
            ended_by: None,
            structured_content: None,
        };
        let f = facts_of(Some(&t), "anything");
        assert_eq!(
            (f.job.as_str(), f.tool.as_str(), f.status.as_str()),
            ("7f3a", "desktop__run_command", "failed")
        );
        let f = facts_of(
            None,
            "job t9 (notes__build) cancelled\ncancelled by the dashboard",
        );
        assert_eq!(
            f,
            Facts {
                job: "t9".into(),
                tool: "notes__build".into(),
                status: "cancelled".into()
            }
        );
        assert_eq!(facts_of(None, "hand-edited").status, "ended");
    }

    #[test]
    fn the_body_leaves_out_the_summary_s_own_line() {
        assert_eq!(
            body_of("job t1 (a__b) completed\n42 files", "t1"),
            "42 files"
        );
        assert_eq!(body_of("job t1 (a__b) completed", "t1"), "(no output)");
        assert_eq!(body_of("job t1 (a__b) completed\n  ", "t1"), "(no output)");
        assert_eq!(
            body_of("something else\nmore", "t1"),
            "something else\nmore"
        );
    }

    /// As `POST …/answer` judges it: a result with no reply after the last
    /// one, or a job's result that enters as a turn starts or a message is
    /// sent.
    #[test]
    fn a_result_waits_for_an_answer_until_a_reply_follows_it() {
        assert!(!needs_answer(&["user", "assistant"], &[]));
        assert!(needs_answer(&["user", "assistant", "tool"], &[]));
        assert!(needs_answer(&["user", "assistant", "tool", "tool"], &[]));
        assert!(needs_answer(&["user", "assistant", "tool", "user"], &[]));
        assert!(!needs_answer(
            &["user", "assistant", "tool", "assistant"],
            &[]
        ));
        assert!(!needs_answer(
            &["user", "assistant", "tool", "user", "assistant"],
            &[]
        ));
        // A job still running owes nothing yet.
        assert!(!needs_answer(
            &["user", "assistant"],
            &[task("working", None)]
        ));
        // An ended one nothing holds enters at the answer's start.
        assert!(needs_answer(
            &["user", "assistant"],
            &[task("completed", None)]
        ));
        // One a running turn holds off is that turn's to answer.
        assert!(!needs_answer(
            &["user", "assistant"],
            &[task("completed", Some("the turn that is running"))]
        ));
    }

    #[test]
    fn refusals_are_worded_by_code() {
        assert!(answer_refusal(Some("nothing_to_answer"), "x").starts_with("Nothing to answer"));
        assert!(answer_refusal(Some("turn_running"), "x").contains("being written elsewhere"));
        assert_eq!(answer_refusal(None, "boom"), "Answer did not start: boom");
        let (w, on) = cancel_refusal("task_cancel_unsupported", "no tasks.cancel", "a__b");
        assert!(
            on && w.contains("cannot be cancelled") && w.contains("goes on"),
            "{w}"
        );
        let (w, on) = cancel_refusal("task_cancel_refused", "busy", "a__b");
        assert!(on && w.contains("refused: busy"), "{w}");
        assert!(cancel_refusal("task_ended", "", "a__b")
            .0
            .contains("already ended"));
        assert!(cancel_refusal("task_not_found", "", "a__b")
            .0
            .contains("in the conversation"));
        let (w, on) = cancel_refusal("internal", "db", "a__b");
        assert!(!on && w.ends_with("failed: db"), "{w}");
    }

    #[test]
    fn the_strip_head_counts_running_and_ended_jobs() {
        assert_eq!(strip_head(&[task("working", None)]), "1 job running");
        assert_eq!(
            strip_head(&[task("working", None), task("input_required", None)]),
            "2 jobs running"
        );
        assert_eq!(
            strip_head(&[task("completed", None)]),
            "1 job ended, its result on its way in"
        );
        assert_eq!(
            strip_head(&[task("working", None), task("failed", None)]),
            "1 job running · 1 ended, on the way in"
        );
        assert_eq!(status_words("input_required"), "needs input");
        assert!(running("input_required") && !running("abandoned"));
    }
}
