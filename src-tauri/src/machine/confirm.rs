//! The confirmation (machine-check design §4.5, D25): a dialog the shell
//! draws itself, outside the webview, from the plan it computed itself, so
//! a script at the gateway's origin can call the command but cannot press
//! its button.
//!
//! It is a GTK message dialog (gtk 0.18, the version tao builds the window
//! with; the shell already depends on it), modal over the app's window: the
//! plan's steps as plain bullets, the password line, and the exact commands
//! with every file they write behind a "Show the commands" fold.
//! tauri-plugin-dialog's message dialog (rfd's GTK one) can neither make
//! Cancel the default nor hold a fold, so it is not used here. "Not now" is
//! the default button and has the focus, so Enter or Escape changes
//! nothing; only a click on "Set it up" proceeds. A dialog closed any other
//! way (the window manager, the app's window destroyed with it) is "Not
//! now".
//!
//! The page decides when the command is called, so three things keep it
//! from deciding when the dialog appears (the review's M1):
//!
//! - **Only over the app's window in front** ([`Asked::may_show`], D133):
//!   checked on the GTK main thread right before the dialog is built, in the
//!   same main-loop turn, the window must be shown and the active one. A
//!   hidden window's webview keeps running, and its script could otherwise
//!   pop the dialog over another program, under the person's cursor.
//! - **"Set it up" waits** ([`Arming`], [`ARM_DELAY`], D134): it stays
//!   insensitive until the dialog has been mapped and active for a second,
//!   and goes insensitive again (and waits again) whenever it loses the
//!   focus, as Firefox's security dialogs do. A click aimed at what the page
//!   showed under that spot lands on nothing.
//! - **"Not now" holds for the document** ([`Asked::answered`], D140): a
//!   call from the document that said "Not now" is answered without a
//!   dialog until the window's document is replaced
//!   (`crate::media::document`), so the page cannot ask again and again.

use std::sync::Mutex;
use std::time::Duration;

use lmgw_api_types::machine::MachinePlan;
use lmgw_machine::helper::UNIT;
use lmgw_machine::plan::instructions::command_line;

/// What the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Confirmation {
    /// The dialog's primary text.
    pub title: String,
    /// The bullets and the password line.
    pub text: String,
    /// The fold: every command in order, then every file in full.
    pub commands: String,
}

/// The fold's label.
pub(crate) const SHOW_COMMANDS: &str = "Show the commands";
/// The default button: nothing changes.
pub(crate) const NOT_NOW: &str = "Not now";
/// The one button that proceeds.
pub(crate) const SET_IT_UP: &str = "Set it up";

/// How long "Set it up" waits after the dialog is shown and has the focus,
/// and again after every time it got the focus back (D134): Firefox's
/// `security.dialog_enable_delay` default, 1000 ms. The person sees it as
/// the button becoming available.
pub(crate) const ARM_DELAY: Duration = Duration::from_millis(1000);

/// The confirmation for `plan` (§4.5): the plan block's bullets and its
/// password line, and the commands fold.
pub(crate) fn of(plan: &MachinePlan) -> Confirmation {
    let bullets: Vec<String> = plan
        .steps
        .iter()
        .map(|s| format!("• {}", s.sentence))
        .collect();
    // "Only these things" is true only of a plan whose steps install no
    // package: a package's own scripts change more (D144).
    let scripts = plan.steps.iter().any(|s| s.package_scripts.is_some());
    let (besides, only) = if scripts {
        (
            "Besides these, NVIDIA's package runs its own install scripts; \"Show the commands\" \
             says what they change.\n\n",
            "",
        )
    } else {
        ("", " lmgw changes only these things.")
    };
    let text = format!(
        "lmgw will make these changes to this computer:\n\n{}\n\n{besides}Your computer will ask \
         for your password once. If you're not an administrator, someone who is has to type \
         theirs.{only}",
        bullets.join("\n")
    );
    let mut commands = vec![format!(
        "These run as the administrator (root), in this order, as the system task {UNIT}:"
    )];
    commands.push(String::new());
    for step in &plan.steps {
        if let Some(f) = &step.writes_file {
            let verb = if f.replaces { "replace" } else { "create" };
            let what = match &f.key_fingerprint {
                Some(fp) => format!("the file below: an OpenPGP key, fingerprint {fp}"),
                None => "the file below".into(),
            };
            commands.push(format!(
                "{verb} {} (mode {:04o})   ({what})",
                f.path, f.mode
            ));
        }
        for argv in &step.commands {
            commands.push(command_line(argv));
        }
        if let Some(note) = &step.package_scripts {
            commands.push(note.clone());
        }
    }
    for f in plan.steps.iter().filter_map(|s| s.writes_file.as_ref()) {
        commands.push(String::new());
        commands.push(format!("{}, in full (mode {:04o}):", f.path, f.mode));
        commands.extend(f.content.lines().map(str::to_string));
    }
    Confirmation {
        title: "Set up your graphics card for local models".into(),
        text,
        commands: commands.join("\n"),
    }
}

/// The app's window as the dialog would open over it, read on the GTK main
/// thread just before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    /// Shown, not hidden to the tray.
    pub visible: bool,
    /// The active window: the one the person is working in.
    pub active: bool,
    /// The number of the document it runs (`crate::media::document`).
    pub document: Option<u64>,
}

/// What came of asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// "Set it up" was pressed.
    SetItUp,
    /// "Not now", or the dialog closed any other way.
    NotNow,
    /// No dialog: this document said "Not now" already (D140).
    DeclinedBefore,
    /// No dialog: the app's window is not in front, or could not be asked;
    /// why, for the detail (D133).
    NotShown(&'static str),
}

/// What the confirmation keeps across calls: the document that said "Not
/// now" last.
#[derive(Debug, Default)]
pub(crate) struct Asked {
    declined: Mutex<Option<u64>>,
}

impl Asked {
    pub(crate) const fn new() -> Self {
        Self {
            declined: Mutex::new(None),
        }
    }

    /// Whether the dialog may show over `w`, asked on the main thread right
    /// before it is built: the document it is for, or the answer without a
    /// dialog.
    pub(crate) fn may_show(&self, w: Window) -> Result<u64, Answer> {
        let Some(document) = w.document else {
            return Err(Answer::NotShown("the app's window has no document"));
        };
        if !w.visible {
            return Err(Answer::NotShown("the app's window is hidden"));
        }
        if !w.active {
            return Err(Answer::NotShown(
                "the app's window is not the active one (another window has the focus)",
            ));
        }
        if *self.declined.lock().unwrap_or_else(|e| e.into_inner()) == Some(document) {
            return Err(Answer::DeclinedBefore);
        }
        Ok(document)
    }

    /// The person's answer in `document`; a "Not now" is kept until the
    /// window runs another document.
    pub(crate) fn answered(&self, document: u64, set_it_up: bool) -> Answer {
        let mut declined = self.declined.lock().unwrap_or_else(|e| e.into_inner());
        if set_it_up {
            *declined = None;
            Answer::SetItUp
        } else {
            *declined = Some(document);
            Answer::NotNow
        }
    }
}

/// When "Set it up" may be pressed (D134): after [`ARM_DELAY`] with the
/// dialog mapped and active, the whole time; any change of either starts
/// the wait again. The GTK glue feeds it the dialog's `map`, `unmap` and
/// `is-active` changes, starts a timer for each number it hands back, and
/// sets the button's sensitivity from [`Self::armed`].
#[derive(Debug, Default)]
pub(crate) struct Arming {
    mapped: bool,
    active: bool,
    /// The newest timer's number: an older one that runs out arms nothing.
    timer: u64,
    armed: bool,
}

impl Arming {
    /// The dialog was mapped or unmapped. A number: start a timer for it.
    pub(crate) fn mapped(&mut self, mapped: bool) -> Option<u64> {
        if self.mapped == mapped {
            return None;
        }
        self.mapped = mapped;
        self.again()
    }

    /// The dialog got or lost the focus (its `is-active`).
    pub(crate) fn active(&mut self, active: bool) -> Option<u64> {
        if self.active == active {
            return None;
        }
        self.active = active;
        self.again()
    }

    /// Timer `timer` ran out: whether the button is sensitive now.
    pub(crate) fn elapsed(&mut self, timer: u64) -> bool {
        if timer == self.timer && self.mapped && self.active {
            self.armed = true;
        }
        self.armed
    }

    pub(crate) fn armed(&self) -> bool {
        self.armed
    }

    fn again(&mut self) -> Option<u64> {
        self.armed = false;
        self.timer += 1;
        (self.mapped && self.active).then_some(self.timer)
    }
}

/// Show `c` over the app's window, if [`Asked::may_show`] lets it, and
/// answer how it went. The window is read and the dialog built in one
/// closure on the GTK main thread, so nothing of the window changes
/// between the two; the answer comes when the person chose.
#[cfg(target_os = "linux")]
pub(crate) async fn ask(app: &tauri::AppHandle, c: Confirmation, asked: &'static Asked) -> Answer {
    use gtk::prelude::*;
    use tauri::Manager;

    let (tx, rx) = tokio::sync::oneshot::channel::<Answer>();
    let handle = app.clone();
    let shown = app.run_on_main_thread(move || {
        let parent = handle
            .get_webview_window("main")
            .and_then(|w| w.gtk_window().ok());
        let window = Window {
            visible: parent.as_ref().is_some_and(|p| p.is_visible()),
            active: parent.as_ref().is_some_and(|p| p.is_active()),
            document: crate::media::document(),
        };
        match asked.may_show(window) {
            Err(answer) => {
                let _ = tx.send(answer);
            }
            Ok(document) => show(parent.as_ref(), &c, move |set_it_up| {
                let _ = tx.send(asked.answered(document, set_it_up));
            }),
        }
    });
    if let Err(e) = shown {
        tracing::error!("the setup's confirmation could not be shown: {e}");
        return Answer::NotShown("the app's main thread could not be reached");
    }
    // A dialog destroyed without an answer drops the sender: "Not now".
    rx.await.unwrap_or(Answer::NotNow)
}

/// Build and show the dialog (GTK main thread). `answer` gets whether "Set
/// it up" was pressed, once.
#[cfg(target_os = "linux")]
fn show(
    parent: Option<&gtk::ApplicationWindow>,
    c: &Confirmation,
    answer: impl FnOnce(bool) + 'static,
) {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk::prelude::*;
    use gtk::{
        ButtonsType, DialogFlags, Expander, MessageDialog, MessageType, ResponseType,
        ScrolledWindow, TextView, WrapMode,
    };

    let dialog = MessageDialog::new(
        parent,
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        MessageType::Question,
        ButtonsType::None,
        &c.title,
    );
    dialog.set_title("lmgw");
    dialog.set_secondary_text(Some(&c.text));
    dialog.add_button(NOT_NOW, ResponseType::Cancel);
    dialog.add_button(SET_IT_UP, ResponseType::Accept);
    dialog.set_default_response(ResponseType::Cancel);
    dialog.set_response_sensitive(ResponseType::Accept, false);

    let view = TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(WrapMode::WordChar);
    if let Some(buffer) = view.buffer() {
        buffer.set_text(&c.commands);
    }
    let scroll = ScrolledWindow::builder()
        .min_content_height(240)
        .min_content_width(560)
        .build();
    scroll.add(&view);
    let fold = Expander::new(Some(SHOW_COMMANDS));
    fold.add(&scroll);
    if let Ok(area) = dialog.message_area().downcast::<gtk::Box>() {
        area.pack_start(&fold, true, true, 0);
    }

    // "Set it up" waits (D134): the arming decides, a timer per wait.
    let arming = Rc::new(RefCell::new(Arming::default()));
    let weak = dialog.downgrade();
    let set = {
        let arming = arming.clone();
        let weak = weak.clone();
        Rc::new(move |timer: Option<u64>| {
            if let Some(d) = weak.upgrade() {
                d.set_response_sensitive(ResponseType::Accept, arming.borrow().armed());
            }
            let Some(timer) = timer else { return };
            let arming = arming.clone();
            let weak = weak.clone();
            gtk::glib::timeout_add_local_once(ARM_DELAY, move || {
                let armed = arming.borrow_mut().elapsed(timer);
                if let Some(d) = weak.upgrade() {
                    d.set_response_sensitive(ResponseType::Accept, armed);
                }
            });
        })
    };
    {
        let (arming, set) = (arming.clone(), set.clone());
        dialog.connect_map(move |_| {
            let timer = arming.borrow_mut().mapped(true);
            set(timer);
        });
    }
    {
        let (arming, set) = (arming.clone(), set.clone());
        dialog.connect_unmap(move |_| {
            let timer = arming.borrow_mut().mapped(false);
            set(timer);
        });
    }
    dialog.connect_is_active_notify(move |d| {
        let timer = arming.borrow_mut().active(d.is_active());
        set(timer);
    });

    let answer = RefCell::new(Some(answer));
    dialog.connect_response(move |d, response| {
        if let Some(answer) = answer.borrow_mut().take() {
            answer(response == ResponseType::Accept);
        }
        // Not `close`: a dialog answers its own delete-event with another
        // response. SAFETY: nothing else holds the dialog; it is gone once
        // the person answered, and this handler is its last use.
        unsafe { d.destroy() };
    });
    dialog.show_all();
    if let Some(button) = dialog.widget_for_response(ResponseType::Cancel) {
        button.grab_focus();
    }
}
