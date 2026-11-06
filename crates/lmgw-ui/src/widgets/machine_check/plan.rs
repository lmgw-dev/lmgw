//! The plan block (machine-check design §4.5): what lmgw would change, in
//! the plan's own sentences, the exact commands behind "Show the commands",
//! and what can be done here: "Set it up" in the app's window when the plan
//! is the helper's, the commands to type when it is instructions, or §2.7's
//! sentence in a browser, where no button could work.
//!
//! The sentences above the fold are the native confirmation's (the shell's
//! `machine::confirm::of`), so the page and the dialog read the same; the
//! fold has what the dialog's fold has (every command by its absolute path,
//! each file with its mode and a key with its fingerprint, what NVIDIA's
//! package's own scripts change, every file in full), then the same steps as
//! lines to type in a terminal, with a copy button.

use leptos::prelude::*;
use lmgw_api_types::machine::{MachinePlan, PlanHow, PlanStep};
use wasm_bindgen::JsCast;

use super::this_computer;

/// The system task the steps run as (`lmgw_machine::helper::UNIT`).
const UNIT: &str = "lmgw-machine-setup";

/// The plan block's title, the dialog's too.
pub(crate) const TITLE: &str = "Set up your graphics card for local models";

/// Said after the bullets: ruling 4's half that needs no password.
pub(crate) const AFTER: &str = "After that, lmgw keeps the description of your graphics card up \
                                to date by itself, without asking again.";

/// Said when a step installs NVIDIA's package (D144).
pub(crate) const BESIDES: &str = "Besides these, NVIDIA's package runs its own install scripts; \
                                  \"Show the commands\" says what they change.";

/// Said when no step installs a package, where it is true (D144).
pub(crate) const ONLY: &str = "lmgw changes only these things.";

/// The password line, in the app's window.
pub(crate) const PASSWORD: &str = "Your computer will ask for your password once. If you're not \
                                   an administrator, someone who is has to type theirs.";

/// The password line in a browser, which may be on another computer
/// (review M3).
pub(crate) const PASSWORD_ELSEWHERE: &str = "Setting it up asks for a password on the computer \
                                             lmgw runs on, once. If you're not an administrator \
                                             there, someone who is has to type theirs.";

/// §2.7's sentence, in place of the button outside the app's window.
pub(crate) const ELSEWHERE: &str = "Open lmgw on the computer it runs on to set this up.";

/// §2.7's sentence when the browser is on the computer lmgw runs on (a
/// loopback address): lmgw's own window is right there (review M3).
pub(crate) const OWN_WINDOW: &str = "Open lmgw's own window (from your app menu) to set this up.";

/// Does the page come from this computer's own address? Then the browser
/// runs where lmgw does.
pub(crate) fn on_loopback() -> bool {
    let host = window().location().hostname().unwrap_or_default();
    host == "localhost" || host == "[::1]" || host == "::1" || host.starts_with("127.")
}

/// The sentence where the button would be, outside the app's window.
pub(crate) fn elsewhere(loopback: bool) -> &'static str {
    if loopback {
        OWN_WINDOW
    } else {
        ELSEWHERE
    }
}

/// Where the lines to type go: a terminal, on the computer lmgw runs on
/// when the page is not the app's window.
pub(crate) fn type_them(in_app: bool, open: bool) -> String {
    let lead = if open {
        "Type these in a terminal"
    } else {
        "To type them in a terminal yourself"
    };
    if in_app {
        format!("{lead}:")
    } else {
        format!("{lead} on the computer lmgw runs on:")
    }
}

/// Shell-quote one argument as a terminal reads it: bare when no shell
/// reads any of its characters specially, in single quotes otherwise. The
/// same rule as `lmgw_machine::plan::instructions::command_line`, which the
/// dialog's fold uses.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=,+@%-".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// `argv` as one line.
pub(crate) fn command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// One line of what runs as root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootLine {
    pub text: String,
    /// What a package's own scripts change: said, not run by lmgw.
    pub note: bool,
}

/// What runs as root, in order, as the dialog's fold lists it (one line per
/// file written, command and package note), without the files' text.
pub(crate) fn root_lines(plan: &MachinePlan) -> Vec<RootLine> {
    let mut out = Vec::new();
    let mut push = |text: String, note: bool| out.push(RootLine { text, note });
    for step in &plan.steps {
        if let Some(f) = &step.writes_file {
            let verb = if f.replaces { "replace" } else { "create" };
            let what = match &f.key_fingerprint {
                Some(fp) => format!("the file below: an OpenPGP key, fingerprint {fp}"),
                None => "the file below".into(),
            };
            push(
                format!("{verb} {} (mode {:04o})   ({what})", f.path, f.mode),
                false,
            );
        }
        for argv in &step.commands {
            push(command_line(argv), false);
        }
        if let Some(note) = &step.package_scripts {
            push(note.clone(), true);
        }
    }
    out
}

/// Every step's terminal lines, in order: what the copy button copies.
pub(crate) fn terminal_text(steps: &[PlanStep]) -> String {
    steps
        .iter()
        .flat_map(|s| s.terminal.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a step installs a package whose own scripts change more.
fn has_scripts(plan: &MachinePlan) -> bool {
    plan.steps.iter().any(|s| s.package_scripts.is_some())
}

/// Select `el`'s text, for Ctrl+C when the page may not copy.
fn select_text(el: &web_sys::Element) {
    let w = window();
    let sel = js_sys::Reflect::get(&w, &"getSelection".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .and_then(|f| f.call0(&w).ok());
    let Some(sel) = sel.filter(|s| !s.is_null()) else {
        return;
    };
    if let Some(f) = js_sys::Reflect::get(&sel, &"selectAllChildren".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    {
        let _ = f.call1(&sel, el);
    }
}

/// The lines to type, under what they are for, with "Copy the commands" in
/// words beside it (the kit's icon alone is easy to miss for someone who
/// has never copied a command). A copy that cannot happen says so and
/// selects the lines instead (review L1), as the kit's `copy_secret` says a
/// refusal.
#[component]
fn TerminalLines(text: String, label: String) -> impl IntoView {
    let pre: NodeRef<leptos::html::Pre> = NodeRef::new();
    // `Some(Ok)`: copied; `Some(Err)`: not, and why.
    let said = RwSignal::new(None::<Result<(), String>>);
    let lines = text.clone();
    let copy = move |_| {
        let failed = move |why: String| {
            said.try_set(Some(Err(why)));
            if let Some(el) = pre.get_untracked() {
                select_text(&el);
            }
        };
        let lines = lines.clone();
        leptos::task::spawn_local(async move {
            match lmgw_ui_kit::clipboard::write_text(&lines).await {
                Ok(()) => {
                    said.try_set(Some(Ok(())));
                    set_timeout(
                        move || {
                            if said.try_get_untracked() == Some(Some(Ok(()))) {
                                said.try_set(None);
                            }
                        },
                        std::time::Duration::from_secs(2),
                    );
                }
                Err(why) => failed(why),
            }
        });
    };
    view! {
        <div class="mc-copy-head">
            <span class="mc-sub">{label}</span>
            <button class="btn sm mc-copy" on:click=copy>
                {move || if said.get() == Some(Ok(())) { "Copied" } else { "Copy the commands" }}
            </button>
        </div>
        {move || {
            said.get()
                .and_then(Result::err)
                .map(|why| {
                    view! {
                        <p class="mc-sub mc-copy-failed" role="alert">
                            {format!(
                                "Not copied: {why}. The commands below are selected: press Ctrl+C to copy them."
                            )}
                        </p>
                    }
                })
        }}
        <pre class="mc-code" node_ref=pre>{text}</pre>
    }
}

/// The commands fold: what root runs, the files in full, and the lines to
/// type in a terminal.
#[component]
fn CommandsFold(plan: MachinePlan, in_app: bool) -> impl IntoView {
    let lines = root_lines(&plan)
        .into_iter()
        .map(|l| {
            let class = if l.note { "mc-cmd note" } else { "mc-cmd" };
            view! { <div class=class>{l.text}</div> }
        })
        .collect_view();
    let files: Vec<_> = plan
        .steps
        .iter()
        .filter_map(|s| s.writes_file.clone())
        .collect();
    let terminal = terminal_text(&plan.steps);
    let has_terminal = !terminal.is_empty();
    view! {
        <details class="mc-fold mc-commands">
            <summary>"Show the commands"</summary>
            <p class="mc-sub">
                {format!(
                    "These run as the administrator (root), in this order, as the system task {UNIT}:"
                )}
            </p>
            <div class="mc-code">{lines}</div>
            {files
                .into_iter()
                .map(|f| {
                    view! {
                        <p class="mc-sub">{format!("{}, in full (mode {:04o}):", f.path, f.mode)}</p>
                        <pre class="mc-code">{f.content}</pre>
                    }
                })
                .collect_view()}
            {has_terminal
                .then(|| view! { <TerminalLines text=terminal.clone() label=type_them(in_app, false)/> })}
        </details>
    }
}

/// The commands to type, shown open: the plan is instructions, or the
/// password window could not be shown (`no_agent`).
#[component]
fn Terminal(steps: Vec<PlanStep>, in_app: bool) -> impl IntoView {
    let text = terminal_text(&steps);
    (!text.is_empty()).then(|| {
        view! {
            <div class="mc-terminal">
                <TerminalLines text=text.clone() label=type_them(in_app, true)/>
            </div>
        }
    })
}

/// The plan block.
#[component]
pub(crate) fn PlanBlock(
    plan: MachinePlan,
    /// Inside the app's window: the button can work.
    in_app: bool,
    /// The commands to type are shown open even for a helper plan: the
    /// desktop has no password window (`no_agent`).
    terminal: bool,
    /// A setup is out: the button waits.
    #[prop(into)]
    busy: Signal<bool>,
    on_set_up: Callback<String>,
    /// A surface with little room (D186): the button (or §2.7's sentence)
    /// right after the bullets and the password line, and the notes on what
    /// lmgw keeps doing after it, so "Set it up" is in view first. The
    /// shell's own dialog says all of it again before anything runs.
    #[prop(optional)]
    compact: bool,
) -> impl IntoView {
    let bullets = plan
        .steps
        .iter()
        .map(|s| view! { <li>{s.sentence.clone()}</li> })
        .collect_view();
    let has_steps = !plan.steps.is_empty();
    let scripts = has_scripts(&plan);
    match plan.how.clone() {
        PlanHow::Instructions { why } => view! {
            <div class="mc-plan">
                <h4>"How to set it up"</h4>
                <p>{why}</p>
                {has_steps.then(|| view! { <ul class="mc-bullets">{bullets}</ul> })}
                {has_steps.then(|| view! { <p class="mc-sub">{AFTER}</p> })}
                <Terminal steps=plan.steps.clone() in_app=in_app/>
            </div>
        }
        .into_any(),
        PlanHow::Helper => {
            let hash = plan.hash.clone();
            let act = if in_app {
                view! {
                    <div class="mc-acts">
                        <button
                            class="btn primary"
                            disabled=move || busy.get()
                            on:click=move |_| on_set_up.run(hash.clone())
                        >
                            "Set it up"
                        </button>
                    </div>
                }
                .into_any()
            } else {
                view! { <p class="mc-elsewhere">{elsewhere(on_loopback())}</p> }.into_any()
            };
            // In the app the button ends the block; elsewhere §2.7's sentence
            // stands where the button would, before the commands fold. On a
            // compact surface either comes right after the password line.
            let (early, before, after) = if compact {
                (Some(act), None, None)
            } else if in_app {
                (None, None, Some(act))
            } else {
                (None, Some(act), None)
            };
            let notes = move || {
                view! {
                    <p class="mc-sub">{AFTER}</p>
                    {scripts.then(|| view! { <p class="mc-sub">{BESIDES}</p> })}
                }
            };
            view! {
                <div class="mc-plan">
                    <h4>{TITLE}</h4>
                    <p>{format!("lmgw will make these changes to {}:", this_computer(in_app))}</p>
                    <ul class="mc-bullets">{bullets}</ul>
                    {(!compact).then(notes)}
                    <p class="mc-sub">
                        {if in_app { PASSWORD } else { PASSWORD_ELSEWHERE }}
                        {(!scripts).then(|| format!(" {ONLY}"))}
                    </p>
                    {early}
                    {compact.then(notes)}
                    {terminal.then(|| view! { <Terminal steps=plan.steps.clone() in_app=in_app/> })}
                    {before}
                    <CommandsFold plan=plan.clone() in_app=in_app/>
                    {after}
                </div>
            }
            .into_any()
        }
    }
}
