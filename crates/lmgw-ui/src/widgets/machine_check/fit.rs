//! The settings-fit confirm (machine-check design §1.4, §4.2, §4.4, D114,
//! D135, D136): the model settings item's action, switching the settings to
//! the ones lmgw ships for this computer's card. No password: an ordinary
//! settings write, after a confirmation that lists every field it changes by
//! the dashboard's own label, as it is and as it would become, what it
//! leaves and why, and the running models it stops.
//!
//! The confirm reads like the rest of the check (review M4, D157): core's
//! sentence for the switch first (the answer to the same switch, not
//! confirmed, which writes nothing and names the running models it stops),
//! then the button, and the fields, old and new, in a "What changes (n)"
//! fold. "Switch" posts `machine_settings_fit` with `confirm` set to the
//! listed changes, exactly as shown. When the settings moved since the list
//! was made, nothing is written and the answer is `outdated`: what differs,
//! and the switch as it is now, which is the next list to confirm. When
//! there is nothing left to switch, the confirm closes into a plain note and
//! the check runs again (review M5).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::machine::{
    MachineAction, MachineSettingsFit, MachineSettingsFitArgs, SettingsFit, SettingsFitChange,
    SettingsFitLeft, SettingsFitOutdated, SettingsFitRunning, SettingsFitStop, SettingsFitValue,
};
use lmgw_api_types::settings_labels as labels;

use super::{FitSlot, MachineState};
use crate::scope::Scope;

/// `machine_settings_fit`: `confirm` `None` only says what it would change.
async fn post(
    fit: SettingsFit,
    confirm: Option<Vec<SettingsFitChange>>,
) -> Result<MachineSettingsFit, String> {
    crate::api::post::<MachineSettingsFit, _>(
        "/api/op/machine_settings_fit",
        &MachineSettingsFitArgs { fit, confirm },
    )
    .await
    .map_err(|e| e.to_string())
}

/// An answer that wrote nothing and has nothing to confirm: the settings
/// already are the switch's (review M5).
pub(crate) fn nothing_to_switch(a: &MachineSettingsFit) -> bool {
    !a.applied && a.outdated.is_none() && a.changes.is_empty()
}

/// The list being confirmed: the item's action, or an `outdated` answer's
/// switch as it is now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Offer {
    pub fit: SettingsFit,
    /// The button's words.
    pub label: String,
    pub changes: Vec<SettingsFitChange>,
    pub left: Vec<SettingsFitLeft>,
    pub running: Vec<SettingsFitRunning>,
}

impl Offer {
    pub(crate) fn of(a: &MachineAction) -> Self {
        match a {
            MachineAction::SettingsFit {
                fit,
                label,
                changes,
                left,
                running,
            } => Self {
                fit: *fit,
                label: label.clone(),
                changes: changes.clone(),
                left: left.clone(),
                running: running.clone(),
            },
        }
    }

    /// An answer's list (`outdated`, or the offer read when the confirm
    /// opened), under the same button.
    fn again(&self, a: &MachineSettingsFit) -> Self {
        Self {
            fit: a.fit,
            label: self.label.clone(),
            changes: a.changes.clone(),
            left: a.left.clone(),
            running: a.running.clone(),
        }
    }
}

/// A class as the Settings page names it.
pub(crate) fn class_label(class: &str) -> &str {
    match class {
        "chat" => labels::CHAT_CLASS,
        "aux" => labels::AUX_CLASS,
        "audio" => labels::AUDIO_CLASS,
        "image" => labels::IMAGE_CLASS,
        other => other,
    }
}

/// Where a field is: the class's Settings path, or a model's own page.
pub(crate) fn place(class: &str, model: Option<&String>, field: &str) -> String {
    match model {
        Some(m) => format!("Model '{m}' ({}) · {field}", class_label(class)),
        None => field.to_string(),
    }
}

/// A value as the confirm shows it: each run argument on a line of its own,
/// as the field holds them.
pub(crate) fn value_lines(v: &SettingsFitValue) -> Vec<String> {
    match v {
        SettingsFitValue::Text(t) if t.is_empty() => vec!["(none)".into()],
        SettingsFitValue::Text(t) => vec![t.clone()],
        SettingsFitValue::Args(a) if a.is_empty() => vec!["(none)".into()],
        SettingsFitValue::Args(a) => a.clone(),
    }
}

/// A side of a change: `None` on a model is its class's setting.
fn side(v: &Option<SettingsFitValue>, from: bool) -> (Vec<String>, bool) {
    match v {
        Some(v) => (value_lines(v), false),
        None if from => (vec!["uses its class's setting".into()], true),
        None => (vec!["cleared, so it uses its class's setting".into()], true),
    }
}

/// What a confirmed switch did to a running model, in plain words.
pub(crate) fn stop_words(s: &SettingsFitStop) -> String {
    match s {
        SettingsFitStop::Stopped => "stopped; it starts again with the new settings".into(),
        SettingsFitStop::NotRunning => "had stopped already".into(),
        SettingsFitStop::Busy { in_flight } => format!(
            "answering {in_flight} request{}, so it keeps the old settings until it stops",
            if *in_flight == 1 { "" } else { "s" }
        ),
        SettingsFitStop::Failed { error } => format!("couldn't be stopped: {error}"),
    }
}

#[component]
fn Values(lines: Vec<String>, plain: bool, class: &'static str) -> impl IntoView {
    let class = if plain {
        format!("{class} plain")
    } else {
        class.to_string()
    };
    view! {
        <span class=class>
            {lines.into_iter().map(|l| view! { <span class="mc-val-line">{l}</span> }).collect_view()}
        </span>
    }
}

#[component]
fn Changes(changes: Vec<SettingsFitChange>) -> impl IntoView {
    changes
        .into_iter()
        .map(|c| {
            let (from, from_plain) = side(&c.from, true);
            let (to, to_plain) = side(&c.to, false);
            view! {
                <li class="mc-change">
                    <div class="mc-field">{place(&c.class, c.model.as_ref(), &c.field)}</div>
                    <div class="mc-vals">
                        <Values lines=from plain=from_plain class="mc-val old"/>
                        <span class="mc-arrow" aria-label="becomes">"→"</span>
                        <Values lines=to plain=to_plain class="mc-val new"/>
                    </div>
                </li>
            }
        })
        .collect_view()
}

#[component]
fn Left(left: Vec<SettingsFitLeft>) -> impl IntoView {
    (!left.is_empty()).then(|| {
        view! {
            <p class="mc-sub">"Left as they are:"</p>
            <ul class="mc-changes">
                {left
                    .into_iter()
                    .map(|l| {
                        view! {
                            <li class="mc-change">
                                <div class="mc-field">{place(&l.class, l.model.as_ref(), &l.field)}</div>
                                <div class="mc-vals">
                                    <Values lines=value_lines(&l.value) plain=false class="mc-val"/>
                                </div>
                                <div class="mc-why">{l.why}</div>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        }
    })
}

/// The running models the switch stops, said when core's sentence (which
/// names them) could not be read.
pub(crate) fn running_line(names: &str, one: bool) -> String {
    if one {
        format!(
            "Running now: {names}. lmgw stops it after the switch, so it starts again with the \
             new settings at the next request. If it is answering a request then, it keeps \
             running until it is done."
        )
    } else {
        format!(
            "Running now: {names}. lmgw stops them after the switch, so they start again with \
             the new settings at the next request. One that is answering a request then keeps \
             running until it is done."
        )
    }
}

#[component]
fn Running(running: Vec<SettingsFitRunning>) -> impl IntoView {
    (!running.is_empty()).then(|| {
        let names = running
            .iter()
            .map(|r| format!("'{}' ({})", r.model, class_label(&r.class)))
            .collect::<Vec<_>>()
            .join(", ");
        view! { <p class="mc-sub">{running_line(&names, running.len() == 1)}</p> }
    })
}

#[component]
fn Outdated(o: SettingsFitOutdated) -> impl IntoView {
    let withdrawn = (!o.withdrawn.is_empty()).then(|| {
        view! {
            <p class="mc-sub">"No longer part of the switch:"</p>
            <ul class="mc-changes was"><Changes changes=o.withdrawn.clone()/></ul>
        }
    });
    let added = (!o.added.is_empty()).then(|| {
        view! {
            <p class="mc-sub">"New in the switch:"</p>
            <ul class="mc-changes"><Changes changes=o.added.clone()/></ul>
        }
    });
    let models = (!o.models.is_empty()).then(|| {
        view! {
            <p class="mc-sub">
                {format!(
                    "Changed on their own pages meanwhile: {}.",
                    o.models.iter().map(|m| format!("'{m}'")).collect::<Vec<_>>().join(", ")
                )}
            </p>
        }
    });
    view! { <div class="mc-outdated">{withdrawn} {added} {models}</div> }
}

/// What the switch did, after it was written.
#[component]
fn Applied(a: MachineSettingsFit) -> impl IntoView {
    let stops = a
        .running
        .iter()
        .filter_map(|r| {
            r.stop.as_ref().map(|s| {
                view! { <li>{format!("'{}' ({}): {}", r.model, class_label(&r.class), stop_words(s))}</li> }
            })
        })
        .collect::<Vec<_>>();
    let n = a.changes.len();
    // The sentence says what happened to the running models; the fold has
    // each one, beside every field.
    view! {
        <div class="mc-note ok" role="status">
            <div>{a.sentence.clone()}</div>
            {a.notes.iter().map(|n| view! { <div class="mc-sub">{n.clone()}</div> }).collect_view()}
            <details class="mc-fold">
                <summary>{format!("What changed ({n})")}</summary>
                <ul class="mc-changes"><Changes changes=a.changes.clone()/></ul>
                <Left left=a.left.clone()/>
                {(!stops.is_empty())
                    .then(|| {
                        view! {
                            <p class="mc-sub">"The running models:"</p>
                            <ul class="mc-lines">{stops}</ul>
                        }
                    })}
            </details>
        </div>
    }
}

/// The fields a switch changes, folded: "What changes (n)" (D157).
#[component]
fn ChangesFold(offer: Offer) -> impl IntoView {
    let n = offer.changes.len();
    view! {
        <details class="mc-fold">
            <summary>{format!("What changes ({n})")}</summary>
            <ul class="mc-changes"><Changes changes=offer.changes.clone()/></ul>
            <Left left=offer.left.clone()/>
        </details>
    }
}

/// The confirm, under the item whose action it is. The write's answer is
/// taken even when the confirm is gone by then (its check is the app's).
#[component]
pub(crate) fn FitConfirm(
    offer: Offer,
    st: MachineState,
    fit_slot: FitSlot,
    /// The settings were written: the surface that shows them reads them
    /// again.
    settings_written: Option<Callback<()>>,
) -> impl IntoView {
    let slot = fit_slot;
    let scope = Scope::new();
    let shown = RwSignal::new(offer);
    // Core's sentence for the switch: `None` while it is read, then the
    // offer's or an `outdated` answer's; `Some(None)` when it could not be
    // read, and the confirm says it in its own words.
    let sentence = RwSignal::new(None::<Option<String>>);
    let busy = slot.busy;
    let error = RwSignal::new(None::<String>);
    let outdated = RwSignal::new(None::<SettingsFitOutdated>);
    let applied = RwSignal::new(None::<MachineSettingsFit>);
    // Nothing to switch: a plain note under the item, the confirm closed,
    // and a new check, which no longer offers it.
    let already = move |said: String| {
        slot.note.set(Some(said));
        slot.offer.set(None);
        st.refresh();
    };

    // The switch as it is now, with core's sentence. Not when the person
    // confirmed first: the list they confirmed stays what they read.
    let fit = shown.get_untracked().fit;
    scope.spawn(async move {
        let answer = post(fit, None).await;
        if busy.get_untracked() || applied.with_untracked(Option::is_some) {
            return;
        }
        match answer {
            Ok(a) if nothing_to_switch(&a) => already(a.sentence),
            Ok(a) => {
                let now = shown.with_untracked(|o| o.again(&a));
                if shown.with_untracked(|o| *o != now) {
                    shown.set(now);
                }
                sentence.set(Some(Some(a.sentence)));
            }
            Err(_) => sentence.set(Some(None)),
        }
    });

    let confirm = move |_| {
        if busy.get_untracked() {
            return;
        }
        let o = shown.get_untracked();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let answer = post(o.fit, Some(o.changes.clone())).await;
            busy.try_set(false);
            match answer {
                Ok(a) if a.applied => {
                    if let Some(c) = a.check.clone() {
                        st.show(c);
                    }
                    if let Some(cb) = settings_written {
                        cb.try_run(());
                    }
                    outdated.try_set(None);
                    applied.try_set(Some(a));
                }
                // Nothing was written. With `outdated`, the list is the
                // switch as it is now; without, there is nothing to switch.
                Ok(a) => match a.outdated.clone() {
                    Some(diff) => {
                        if scope.alive() {
                            sentence.set(Some(Some(a.sentence.clone())));
                            outdated.set(Some(diff));
                            shown.set(o.again(&a));
                        }
                    }
                    None => {
                        if scope.alive() {
                            already(a.sentence);
                        }
                    }
                },
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };
    let said = move || {
        let o = shown.get();
        match sentence.get() {
            // An `outdated` answer's sentence is in its note.
            Some(Some(_)) if outdated.with(Option::is_some) => None,
            Some(Some(s)) => Some(view! { <p>{s}</p> }.into_any()),
            None => {
                Some(view! { <p class="mc-sub">"Reading what the switch changes…"</p> }.into_any())
            }
            Some(None) => {
                let n = o.changes.len();
                let say = if n == 1 {
                    "lmgw changes one setting. Nothing is changed until you confirm.".to_string()
                } else {
                    format!("lmgw changes {n} settings. Nothing is changed until you confirm.")
                };
                Some(
                    view! {
                        <p>{say}</p>
                        <Running running=o.running.clone()/>
                    }
                    .into_any(),
                )
            }
        }
    };
    let diff = move || {
        outdated.get().map(|d| {
            let k = d.withdrawn.len() + d.added.len() + d.models.len();
            let said = sentence.get().flatten().unwrap_or_default();
            view! {
                <div class="mc-note warn" role="status">
                    <div>{said}</div>
                    <details class="mc-fold">
                        <summary>{format!("What is different now ({k})")}</summary>
                        <Outdated o=d/>
                    </details>
                </div>
            }
        })
    };
    view! {
        // Once written, the answer stands alone, without the confirm's frame.
        <div class=move || if applied.with(Option::is_some) { "mc-fit done" } else { "mc-fit" }>
            {move || {
                applied
                    .get()
                    .map(|a| view! { <Applied a=a/> }.into_any())
                    .unwrap_or_else(|| {
                        view! {
                            {diff}
                            {said}
                            {move || error.get().map(|e| view! { <div class="mc-note err" role="alert">{e}</div> })}
                            <div class="mc-acts">
                                <button
                                    class="btn ghost"
                                    disabled=move || busy.get()
                                    on:click=move |_| slot.offer.set(None)
                                >
                                    "Cancel"
                                </button>
                                <button
                                    class="btn primary"
                                    disabled=move || busy.get()
                                    on:click=confirm
                                >
                                    {move || shown.with(|o| o.label.clone())}
                                </button>
                            </div>
                            {move || view! { <ChangesFold offer=shown.get()/> }}
                        }
                            .into_any()
                    })
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_as_the_fields_hold_them() {
        assert_eq!(
            value_lines(&SettingsFitValue::Args(vec![
                "--device".into(),
                "nvidia.com/gpu=all".into()
            ])),
            vec!["--device", "nvidia.com/gpu=all"]
        );
        assert_eq!(
            value_lines(&SettingsFitValue::Text(String::new())),
            vec!["(none)"]
        );
        assert_eq!(side(&None, true).0, vec!["uses its class's setting"]);
        assert!(side(&None, false).0[0].starts_with("cleared"));
    }

    #[test]
    fn a_models_own_field_is_named_with_the_model() {
        assert_eq!(
            place(
                "chat",
                Some(&"qwen-demo".to_string()),
                labels::IMAGE_OVERRIDE
            ),
            "Model 'qwen-demo' (Chat) · Image override"
        );
        assert_eq!(
            place("chat", None, "Settings → Runtimes → Chat → Image"),
            "Settings → Runtimes → Chat → Image"
        );
    }

    #[test]
    fn an_answer_without_a_switch_is_nothing_to_switch() {
        let already = MachineSettingsFit {
            fit: SettingsFit::AmdVulkan,
            applied: false,
            sentence: "The model settings already are the ones for AMD cards.".into(),
            changes: Vec::new(),
            left: Vec::new(),
            running: Vec::new(),
            outdated: None,
            notes: Vec::new(),
            check: None,
        };
        assert!(nothing_to_switch(&already));
        let mut offer = already.clone();
        offer.changes.push(SettingsFitChange {
            class: "chat".into(),
            model: None,
            field: "Settings → Runtimes → Chat → Image".into(),
            from: Some(SettingsFitValue::Text("a".into())),
            to: Some(SettingsFitValue::Text("b".into())),
        });
        assert!(!nothing_to_switch(&offer), "an offer to confirm");
        let mut written = offer.clone();
        written.applied = true;
        assert!(!nothing_to_switch(&written));
    }

    #[test]
    fn one_running_model_is_said_as_one() {
        let one = running_line("'qwen-demo' (Chat)", true);
        assert!(one.contains("If it is answering a request then, it keeps running"));
        let two = running_line("'a' (Chat), 'b' (Aux)", false);
        assert!(two.contains("One that is answering a request then keeps running"));
    }

    #[test]
    fn stops_are_said_plainly() {
        assert!(stop_words(&SettingsFitStop::Busy { in_flight: 1 }).contains("1 request,"));
        assert!(stop_words(&SettingsFitStop::Busy { in_flight: 2 }).contains("2 requests"));
    }
}
