//! The check beyond Settings (machine-check design §2.1, §2.2, §2.5, WP8b):
//! the card on the Overview and above the Chat's empty state
//! ([`MachineCard`]), the Chat's one-line [`MachineBanner`], the same line
//! inside a card under a failure it explains ([`MachineInCard`]), and the
//! slot the dashboard hands its Chat ([`chat_slot`]) with the banner, the
//! card and the fix under a `gpu_setup` card.
//!
//! Each shows the app's one check and run ([`MachineState`], D160); the
//! card and the banner reach out by one rule ([`super::nudge`], D170): a
//! `needs_*` verdict, local models involved ([`super::involved`], D184), no
//! "Not now" for the check as it reads.

use leptos::prelude::*;
use lmgw_api_types::machine::{MachineVerdict, PlanHow};

use super::involved::use_involved;
use super::items::{Mark, MarkView};
use super::nudge::{needs, use_nudge};
use super::plan::on_loopback;
use super::{in_app, use_machine_state, MachineCheck, MachineState};

/// Where every surface links to: Settings → GPU, "This computer" (§2.3).
pub(crate) const SETTINGS_LINK: &str = "/settings/gpu#machine";

/// Is the page on the computer lmgw runs on: the app's window, or a
/// browser on a loopback address (D163)?
pub(crate) fn here(app: bool) -> bool {
    app || on_loopback()
}

/// The banner's words (§2.2, D174, D185): its sentence and its link's
/// label. "Set it up" only where the link lands on that button (`button`:
/// a helper plan in the app's window, no setup out); a restart says so and
/// links to why; everything else is "See what to do". `here`: the page is
/// on the computer lmgw runs on; elsewhere the sentence names that
/// computer, not the one the browser is on (D163).
pub(crate) fn banner_words(
    v: MachineVerdict,
    button: bool,
    here: bool,
) -> (&'static str, &'static str) {
    let restart = v == MachineVerdict::NeedsRestart;
    let sentence = match (restart, here) {
        (true, true) => "Your computer needs a restart before local models can use the graphics card",
        (true, false) => {
            "The computer lmgw runs on needs a restart before local models can use its graphics card"
        }
        (false, true) => "Your graphics card isn't ready for local models yet",
        (false, false) => "The graphics card of the computer lmgw runs on isn't ready for local models yet",
    };
    let link = if button {
        "Set it up"
    } else if restart {
        "See why"
    } else {
        "See what to do"
    };
    (sentence, link)
}

/// The plan has the button: a helper plan with steps in the app's window,
/// and no setup out (tracked).
fn has_button(st: MachineState, app: bool) -> bool {
    app && !st.busy.get()
        && st.check.with(|c| {
            c.as_ref()
                .and_then(|c| c.plan.as_ref())
                .is_some_and(|p| matches!(p.how, PlanHow::Helper) && !p.steps.is_empty())
        })
}

/// The line's sentence, the separator and the link (D185).
#[component]
fn BannerWords(st: MachineState, app: bool) -> impl IntoView {
    let here = here(app);
    let words = Memo::new(move |_| {
        let v = st.check.with(|c| c.as_ref().map(|c| c.verdict));
        banner_words(
            v.unwrap_or(MachineVerdict::NeedsFix),
            has_button(st, app),
            here,
        )
    });
    // One run of text, so it wraps as a sentence beside the mark, never
    // leaving the mark alone on a line in a narrow card.
    view! {
        <span class="mc-banner-words">
            <span class="mc-banner-text">{move || words.get().0}</span>
            <span class="mc-banner-sep" aria-hidden="true">" · "</span>
            <a class="mc-banner-link" href=SETTINGS_LINK>{move || words.get().1}</a>
        </span>
    }
}

/// The one-line banner over the Chat's conversation (§2.2, D174): the
/// sentence, its link to Settings → GPU, and "Not now". Shown while the
/// check reaches out, except while a `gpu_setup` card below holds the fix
/// (`fix_on_page`, D187): the person reads it once, there.
#[component]
pub fn MachineBanner(#[prop(into)] fix_on_page: Signal<bool>) -> impl IntoView {
    let st = use_machine_state();
    let nudge = use_nudge();
    let involved = use_involved();
    // The last result, when none is shown yet: no run of its own.
    st.mount(false);
    let app = in_app();
    view! {
        <Show when=move || !fix_on_page.get() && nudge.reaches(involved.get())>
            <div class="mc-banner" role="status">
                <MarkView mark=Mark::Problem/>
                <BannerWords st=st app=app/>
                <button class="btn ghost sm mc-banner-not-now" on:click=move |_| nudge.not_now()>
                    "Not now"
                </button>
            </div>
        </Show>
    }
}

/// The banner's line inside a card, under a failure it explains: a model
/// test or a lab run whose start failed with `gpu_setup` (D179). It shows
/// while the check says the card is not ready, whatever "Not now" holds,
/// and has none of its own (D188): that failure is the person's own, and
/// waving its explanation away is not waving the check away app-wide.
#[component]
pub fn MachineInCard() -> impl IntoView {
    let st = use_machine_state();
    st.mount(false);
    let app = in_app();
    let shown = move || {
        st.check
            .with(|c| c.as_ref().is_some_and(|c| needs(c.verdict)))
    };
    view! {
        <Show when=shown>
            <div class="mc-banner in-card" role="status">
                <MarkView mark=Mark::Problem/>
                <BannerWords st=st app=app/>
            </div>
        </Show>
    }
}

/// The check as a card of its own: at the top of the Overview's state
/// column (§2.1, D172) and above the Chat's empty state (§2.1, D173).
/// Shown while the check reaches out, and for the rest of the visit once a
/// setup ran while it was shown, so its answer is read. "Not now" is in the
/// card's head, beside "This computer", except while a setup runs, whose
/// progress it would hide (D175); the component is compact (D186).
#[component]
pub fn MachineCard(
    /// The surface's own class, for its place (`ov-machine`, `chat-machine`).
    class: &'static str,
) -> impl IntoView {
    let st = use_machine_state();
    let nudge = use_nudge();
    let involved = use_involved();
    st.mount(false);
    // "Not now" said on this card: gone at once, whatever else holds it.
    let gone = RwSignal::new(false);
    // A setup ran while this card was shown: it stays for its answer.
    let ran_here = RwSignal::new(false);
    let reaches = Memo::new(move |_| nudge.reaches(involved.get()));
    let shown = Memo::new(move |_| !gone.get() && (reaches.get() || ran_here.get()));
    Effect::new(move |_| {
        if shown.get() && st.busy.get() {
            ran_here.set(true);
        }
    });
    let not_now = move |_| {
        nudge.not_now();
        gone.set(true);
    };
    view! {
        <Show when=move || shown.get()>
            <section class=format!("card mc-card {class}") aria-label="This computer">
                <div class="mc-card-head">
                    <h3>"This computer"</h3>
                    <Show when=move || !st.busy.get()>
                        <button class="btn ghost sm mc-not-now" on:click=not_now>
                            "Not now"
                        </button>
                    </Show>
                </div>
                <MachineCheck fresh=true compact=true/>
            </section>
        </Show>
    }
}

/// What the dashboard hands its Chat (D177): the banner, the card above the
/// empty state, the check under a `gpu_setup` card, and whether the page is
/// on the computer lmgw runs on (the card's words, D187). The `/chat` route
/// (`crate::chat_host`) calls it as it mounts the Chat, under `App`'s
/// [`super::provide_machine_state`], and puts it in `ChatHost::machine`; the
/// debug build's test host (`crate::test_host`) hands none, as a paired
/// device's sidebar (D178).
pub fn chat_slot() -> lmgw_chat_ui::ChatMachine {
    lmgw_chat_ui::ChatMachine::new(
        |fix_on_page| view! { <MachineBanner fix_on_page=fix_on_page/> }.into_any(),
        || view! { <MachineCard class="chat-machine"/> }.into_any(),
        // A start just failed: core checked again (§2.6), so a new run
        // shows what it found (and joins the one in progress, D61).
        || view! { <MachineCheck fresh=true compact=true/> }.into_any(),
        here(in_app()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [MachineVerdict; 6] = [
        MachineVerdict::Ready,
        MachineVerdict::NeedsFix,
        MachineVerdict::NeedsSteps,
        MachineVerdict::NeedsRestart,
        MachineVerdict::NeedsDriver,
        MachineVerdict::CloudOnly,
    ];

    /// M3: "Set it up" only where the link lands on the button; a restart
    /// says so and asks nothing else.
    #[test]
    fn the_banner_says_set_it_up_only_where_the_button_is() {
        for v in ALL {
            let (_, link) = banner_words(v, true, true);
            assert_eq!(link, "Set it up", "{v:?}");
            let (sentence, link) = banner_words(v, false, true);
            if v == MachineVerdict::NeedsRestart {
                assert_eq!(link, "See why");
                assert!(sentence.contains("needs a restart"), "{sentence}");
            } else {
                assert_eq!(link, "See what to do", "{v:?}");
                assert_eq!(
                    sentence,
                    "Your graphics card isn't ready for local models yet"
                );
            }
        }
    }

    /// L2: a browser on another computer is told whose graphics card.
    #[test]
    fn a_browser_elsewhere_is_told_whose_graphics_card() {
        let (s, _) = banner_words(MachineVerdict::NeedsFix, false, false);
        assert_eq!(
            s,
            "The graphics card of the computer lmgw runs on isn't ready for local models yet"
        );
        let (s, _) = banner_words(MachineVerdict::NeedsRestart, false, false);
        assert!(
            s.starts_with("The computer lmgw runs on needs a restart"),
            "{s}"
        );
        for v in ALL {
            for button in [false, true] {
                let (s, _) = banner_words(v, button, false);
                assert!(!s.starts_with("Your"), "{v:?}: {s}");
            }
        }
    }
}
