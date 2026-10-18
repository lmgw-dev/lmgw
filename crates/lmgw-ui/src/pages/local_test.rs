//! The local model editor's Test: `local_model_test` on the row as saved,
//! with its answer kept on the page — the container log lines of a failed
//! load, a ladder's rungs, the running build's disagreements — rather than a
//! toast that is gone before it is read.
//!
//! The test loads what is saved, not what is in the form, so with unsaved
//! edits it asks first: save them and test, or test the saved version.
//!
//! A test in flight is an app-wide busy key ([`OpsState`]), not the page's:
//! a cold load takes minutes, the owner may leave meanwhile, and coming back
//! must not start a second one. Its answer then arrives as a toast.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{ModelTest, RungTest};
use serde_json::json;

use crate::ops_state::{model_key, OpsState};
use crate::widgets::{Modal, Toasts};

/// One test's answer: when it ran (local time) and what the op said, or why
/// it refused outright (a hold, a disabled row, an unknown id).
pub(super) type Outcome = (String, Result<ModelTest, String>);

/// The busy key of a test of the chat-class row `model_id`.
pub(super) fn test_key(model_id: &str) -> String {
    format!("test:{}", model_key("chat", model_id))
}

/// Run the load test on the saved row `model_id`, unless one is already
/// running for it. The answer lands in `result` while the editor is still
/// open, and in a toast once it is not.
pub(super) fn run_load_test(
    ops: OpsState,
    toasts: Toasts,
    model_id: String,
    result: RwSignal<Option<Outcome>>,
) {
    let key = test_key(&model_id);
    if model_id.is_empty() || !ops.start(&key) {
        return;
    }
    spawn_local(async move {
        let body = json!({ "model_id": model_id, "target": "chat" });
        let res = crate::api::post::<ModelTest, _>("/api/op/local_model_test", &body)
            .await
            .map_err(|e| e.to_string());
        ops.finish(&key);
        if let Some(Some(outcome)) = result.try_set(Some((now(), res))) {
            let (ok, text) = summary(&outcome.1);
            let text = format!("Test of {model_id}: {text}");
            if ok {
                toasts.ok(text);
            } else {
                toasts.err(text);
            }
        }
    });
}

/// A result that is not a test, in the card's place: why none was run.
pub(super) fn not_tested(result: RwSignal<Option<Outcome>>, why: String) {
    result.set(Some((now(), Err(why))));
}

/// The time of day, local.
fn now() -> String {
    js_sys::Date::new_0()
        .to_locale_time_string("default")
        .as_string()
        .unwrap_or_default()
}

/// Passed or not, and one line on it — the toast of an answer that arrived
/// after the editor was left.
fn summary(res: &Result<ModelTest, String>) -> (bool, String) {
    match res {
        Err(e) => (false, e.clone()),
        Ok(ModelTest::Ladder(l)) if l.ok => (true, "every rung loads and generates".into()),
        Ok(ModelTest::Passed(p)) => (true, passed_line(p.latency_ms, &p.probe)),
        Ok(ModelTest::ChatPassed(c)) => (true, passed_line(c.base.latency_ms, &c.base.probe)),
        Ok(ModelTest::ImagePassed(i)) => (true, passed_line(i.latency_ms, &i.probe)),
        Ok(ModelTest::Failed(f)) => (false, failed_line(&f.error)),
        Ok(ModelTest::Ladder(_)) => (false, failed_line("")),
    }
}

/// What a failure says in one line.
fn failed_line(error: &str) -> String {
    if error.is_empty() {
        "failed — open the model to see why".into()
    } else {
        format!("failed — {error}")
    }
}

/// Asked when Test is pressed with unsaved edits: the test would load the
/// saved configuration, not the one on screen.
#[component]
pub(super) fn UnsavedTestModal(
    open: RwSignal<bool>,
    /// What is unsaved, card by card ("Context & batch (2) · Flags (1)").
    #[prop(into)]
    detail: Signal<String>,
    /// Save is not possible as the form stands (fields that do not parse).
    #[prop(into)]
    invalid: Signal<bool>,
    /// A save is already on its way.
    #[prop(into)]
    saving: Signal<bool>,
    /// The form, saved, would leave the model enabled — a disabled one is
    /// not served, so there would be nothing to test.
    #[prop(into)]
    enabled_after: Signal<bool>,
    on_save_and_test: Callback<()>,
    on_test_saved: Callback<()>,
) -> impl IntoView {
    view! {
        <Modal open=open title="Unsaved changes">
            <p>
                "The test loads this model as it is saved. Your unsaved changes are not part of it: "
                <b>{move || detail.get()}</b> "."
            </p>
            <Show when=move || invalid.get()>
                <p class="notice warn">
                    "Some fields do not parse, so they cannot be saved yet — fix them first, or test the saved version."
                </p>
            </Show>
            <Show when=move || !invalid.get() && !enabled_after.get()>
                <p class="notice warn">
                    "Your edits untick Enabled: saved like that, the model is not served, so there would be nothing to test."
                </p>
            </Show>
            <div class="row" style="justify-content:flex-end">
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button
                    class="btn"
                    title="Leave the edits in the form and test what is saved"
                    on:click=move |_| {
                        open.set(false);
                        on_test_saved.run(());
                    }
                >
                    "Test saved version"
                </button>
                <button
                    class="btn primary"
                    disabled=move || invalid.get() || saving.get() || !enabled_after.get()
                    title="Save the edits, then test the model with them"
                    on:click=move |_| {
                        open.set(false);
                        on_save_and_test.run(());
                    }
                >
                    "Save and test"
                </button>
            </div>
        </Modal>
    }
}

/// The last test's answer, or that one is running. Nothing at all before the
/// first press.
#[component]
pub(super) fn LoadTestCard(
    #[prop(into)] testing: Signal<bool>,
    result: RwSignal<Option<Outcome>>,
) -> impl IntoView {
    view! {
        <Show when=move || testing.get() || result.with(Option::is_some)>
            <section class="card edit-section load-test">
                <h3>
                    "Test "
                    {move || {
                        if testing.get() {
                            view! {
                                <span class="chip live">
                                    <span class="dot"></span>
                                    "running"
                                </span>
                            }
                                .into_any()
                        } else {
                            let (cls, label) = match result.with(|r| r.as_ref().map(verdict)) {
                                Some(true) => ("chip ok", "passed"),
                                _ => ("chip err", "failed"),
                            };
                            view! { <span class=cls>{label}</span> }.into_any()
                        }
                    }}
                </h3>
                {move || {
                    if testing.get() {
                        return view! {
                            <p class="dim">
                                "Starting the container if it is not running, then asking for one token — a cold load can take minutes."
                            </p>
                        }
                            .into_any();
                    }
                    result
                        .get()
                        .map(|(at, res)| view! { <OutcomeView at=at res=res/> }.into_any())
                        .unwrap_or_else(|| ().into_any())
                }}
            </section>
        </Show>
    }
}

/// Whether a test passed: the op answered and said so.
fn verdict(o: &Outcome) -> bool {
    match &o.1 {
        Ok(ModelTest::Ladder(l)) => l.ok,
        Ok(ModelTest::Failed(_)) | Err(_) => false,
        Ok(ModelTest::ImagePassed(_) | ModelTest::ChatPassed(_) | ModelTest::Passed(_)) => true,
    }
}

/// The one-line summary of a single (non-ladder) test that passed.
fn passed_line(latency_ms: u64, probe: &str) -> String {
    let ms = format!(" in {}", duration(latency_ms));
    match probe {
        "embed" => format!("Loads and embeds{ms}."),
        _ => format!("Loads and generates{ms}."),
    }
}

/// Milliseconds as a person reads them: `850 ms`, `12.4 s`.
fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// One rung of a ladder test, as a line.
fn rung_line(r: &RungTest) -> String {
    let mut parts = vec![format!("rung {}/{}", r.rung, r.of)];
    if let Some(c) = r.ctx_size {
        parts.push(format!("ctx {}", crate::fmt::grouped(c as u64)));
    }
    parts.push(format!("ready after {:.1} s", r.load_seconds));
    parts.join(" · ")
}

/// A single test that passed: its line, where the running build disagrees
/// with what lmgw publishes (a chat model's `/props` against the static
/// derivation) and a note.
fn passed_view(line: String, disagreements: Vec<String>, note: String) -> AnyView {
    view! {
        <p>{line}</p>
        {(!disagreements.is_empty())
            .then(|| {
                view! {
                    <div class="notice warn">
                        <b>"The running build disagrees with what lmgw publishes:"</b>
                        <ul>
                            {disagreements
                                .into_iter()
                                .map(|d| view! { <li>{d}</li> })
                                .collect_view()}
                        </ul>
                    </div>
                }
            })}
        {(!note.is_empty()).then(|| view! { <p class="dim mini-note">{note}</p> })}
    }
    .into_any()
}

#[component]
fn OutcomeView(at: String, res: Result<ModelTest, String>) -> impl IntoView {
    let test = match res {
        Err(e) => {
            return view! {
                <div class="problem">{e}</div>
                <p class="dim mini-note">{format!("at {at}")}</p>
            }
            .into_any()
        }
        Ok(test) => test,
    };
    let body = match test {
        ModelTest::Ladder(ladder) => {
            let reset = !ladder.reset_to_base;
            view! {
                <ul class="load-test-rungs">
                    {ladder
                        .rungs
                        .into_iter()
                        .map(|r| {
                            let err = r.error.clone().unwrap_or_default();
                            view! {
                                <li>
                                    <span class=if r.ok { "chip ok" } else { "chip err" }>
                                        {if r.ok { "ok" } else { "failed" }}
                                    </span>
                                    " "
                                    <span class="mono-sm">{rung_line(&r)}</span>
                                    {(!err.is_empty()).then(|| view! { <div class="problem">{err}</div> })}
                                </li>
                            }
                        })
                        .collect_view()}
                </ul>
                {reset
                    .then(|| {
                        view! {
                            <p class="dim mini-note">
                                "The container could not be put back on the base rung after the test; the next request starts from where it is."
                            </p>
                        }
                    })}
            }
            .into_any()
        }
        ModelTest::Passed(p) => passed_view(
            passed_line(p.latency_ms, &p.probe),
            Vec::new(),
            String::new(),
        ),
        ModelTest::ChatPassed(c) => passed_view(
            passed_line(c.base.latency_ms, &c.base.probe),
            c.disagreements,
            c.note.unwrap_or_default(),
        ),
        ModelTest::ImagePassed(i) => {
            passed_view(passed_line(i.latency_ms, &i.probe), Vec::new(), i.note)
        }
        ModelTest::Failed(f) => view! {
            <div class="problem">{f.error}</div>
            {(!f.hint.is_empty()).then(|| view! { <p class="mini-note">{f.hint}</p> })}
            {(!f.container_log.is_empty())
                .then(|| {
                    view! {
                        <pre class="preset load-test-log">{f.container_log.join("\n")}</pre>
                    }
                })}
        }
        .into_any(),
    };
    view! {
        {body}
        <p class="dim mini-note">{format!("tested at {at}")}</p>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The op's answers, as the gateway writes them.
    fn answer(v: serde_json::Value) -> ModelTest {
        serde_json::from_value(v).unwrap()
    }

    fn passed(probe: &str, latency_ms: u64) -> ModelTest {
        answer(json!({
            "ok": true, "loaded": true, "model_id": "m", "class": "aux",
            "probe": probe, "latency_ms": latency_ms,
        }))
    }

    fn failed(error: &str) -> ModelTest {
        answer(json!({
            "ok": false, "loaded": false, "model_id": "m", "class": "chat",
            "probe": "generate", "latency_ms": 7, "error": error,
            "container_log": [], "hint": "",
        }))
    }

    fn ladder(ok: bool) -> ModelTest {
        answer(json!({
            "ok": ok, "model_id": "m", "class": "chat", "ladder": true, "top_rung": 2,
            "rungs": [], "was_running_rung": null, "reset_to_base": true,
        }))
    }

    #[test]
    fn a_test_passes_only_when_the_op_says_ok() {
        let at = String::new();
        assert!(verdict(&(at.clone(), Ok(passed("embed", 5)))));
        assert!(verdict(&(at.clone(), Ok(ladder(true)))));
        assert!(!verdict(&(at.clone(), Ok(ladder(false)))));
        assert!(!verdict(&(at.clone(), Ok(failed("x")))));
        assert!(!verdict(&(at, Err("held".into()))));
    }

    #[test]
    fn a_late_answer_is_one_line() {
        assert_eq!(
            summary(&Ok(failed("HTTP 500: boom"))),
            (false, "failed — HTTP 500: boom".to_string())
        );
        assert_eq!(
            summary(&Ok(failed(""))),
            (false, "failed — open the model to see why".to_string())
        );
        assert_eq!(
            summary(&Ok(ladder(true))),
            (true, "every rung loads and generates".to_string())
        );
        assert_eq!(
            summary(&Ok(ladder(false))),
            (false, "failed — open the model to see why".to_string())
        );
        assert_eq!(summary(&Err("held".into())), (false, "held".to_string()));
    }

    #[test]
    fn the_summary_names_the_probe_and_the_time() {
        assert_eq!(
            passed_line(1500, "generate"),
            "Loads and generates in 1.5 s."
        );
        assert_eq!(passed_line(850, "embed"), "Loads and embeds in 850 ms.");
        assert_eq!(
            summary(&Ok(passed("embed", 850))),
            (true, "Loads and embeds in 850 ms.".to_string())
        );
    }

    #[test]
    fn a_rung_line_carries_its_context_and_load_time() {
        let rung: RungTest = serde_json::from_value(json!({
            "rung": 2, "of": 3, "gguf_path": null, "ctx_size": 65536, "per_slot_ctx": null,
            "switchover": null, "load_seconds": 4.25, "ok": true,
        }))
        .unwrap();
        assert_eq!(
            rung_line(&rung),
            format!(
                "rung 2/3 · ctx {} · ready after 4.2 s",
                crate::fmt::grouped(65536)
            )
        );
    }
}
