//! The machine check in the Chat (machine-check design §2.1, §2.2, §2.5,
//! WP8b): the one-line banner over the conversation, the block above the
//! empty state, and the card of a turn whose model could not start on the
//! graphics card (`gpu_setup`).
//!
//! The Chat does not read the check itself: its host hands it a
//! [`ChatMachine`] in [`ChatHost::machine`](crate::ChatHost::machine), as it
//! hands it its live updates (chat-plane design D31), and the dashboard fills
//! it with its own banner, card and `MachineCheck`
//! (`lmgw-ui`'s `widgets::machine_check::surfaces`), each deciding itself
//! whether it shows, local models' involvement included (D184). A host that
//! hands none (a paired device's sidebar: it cannot read the admin plane's
//! check and cannot fix the owner's computer) shows no banner and no block,
//! and its card is the 503's sentence alone, which already ends "Open lmgw
//! there to see what to do." (D65).
//!
//! What the Chat decides itself: how the card reads the 503's message
//! ([`Parts`]), which card is the newest, and that the banner steps aside
//! while that card holds the fix (D187).

use leptos::prelude::*;

/// What the host shows of the machine check in the Chat. `Copy`.
#[derive(Clone, Copy)]
pub struct ChatMachine {
    /// The banner over the conversation, given whether a `gpu_setup` card
    /// below holds the fix; it decides itself whether it shows.
    banner: Callback<Signal<bool>, AnyView>,
    /// The block above the empty state, the same way.
    empty: Callback<(), AnyView>,
    /// The fix under a `gpu_setup` card: the check, with "Set it up".
    fix: Callback<(), AnyView>,
    /// The page is on the computer lmgw runs on (the app's window, a
    /// loopback address): the card's sentence need not name that computer.
    here: bool,
    /// The newest `gpu_setup` card's number: only that card shows the fix,
    /// so a conversation tried three times carries one check, not three.
    newest: RwSignal<u64>,
    /// The number of the card that shows the fix, while it is on the page.
    holding: RwSignal<Option<u64>>,
}

impl ChatMachine {
    /// The host's slot: its banner (given whether the fix is on the page
    /// below it), its block above the empty state, the fix under a
    /// `gpu_setup` card, and whether the page is on the computer lmgw runs
    /// on.
    pub fn new(
        banner: impl Fn(Signal<bool>) -> AnyView + Send + Sync + 'static,
        empty: impl Fn() -> AnyView + Send + Sync + 'static,
        fix: impl Fn() -> AnyView + Send + Sync + 'static,
        here: bool,
    ) -> Self {
        Self {
            banner: Callback::new(banner),
            empty: Callback::new(move |()| empty()),
            fix: Callback::new(move |()| fix()),
            here,
            newest: RwSignal::new(0),
            holding: RwSignal::new(None),
        }
    }
}

/// The host's slot, `None` for a host that hands none.
fn slot() -> Option<ChatMachine> {
    with_context::<crate::ChatHost, _>(|h| h.machine).flatten()
}

/// The banner over the conversation, when the host has one; it steps aside
/// while a `gpu_setup` card below holds the fix (D187).
#[component]
pub(super) fn BannerSlot() -> impl IntoView {
    slot().map(|s| {
        let holding = s.holding;
        s.banner
            .run(Signal::derive(move || holding.get().is_some()))
    })
}

/// The block above the empty state, when the host has one, in an element of
/// the Chat's own that its layout keys on.
#[component]
pub(super) fn EmptySlot() -> impl IntoView {
    slot().map(|s| view! { <div class="chat-empty-machine">{s.empty.run(())}</div> })
}

/// The card's two parts, from the 503's message (D65, D176): the sentence
/// the person reads, and what follows it in brackets ("(its start said:
/// …)", Podman's words, and any fallback's clause), folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Parts {
    pub sentence: String,
    pub said: Option<String>,
}

/// The 503's code (`GatewayError::GpuSetup`), on a turn's `error` frame.
pub(super) const GPU_SETUP: &str = "gpu_setup";

/// Where the sentences end and core's clauses begin: the start's own
/// words (`vram::start_failed`) or a fallback's clause without them
/// (`gate::chain`'s refusal of a hop with no route), each in brackets
/// after a sentence's full stop.
const CLAUSE: &str = ". (";

/// What the sentence ends with when there is something to see in lmgw's
/// window (`machine::blame`); dropped where the fix is on the page.
const SEE_THERE: &str = " Open lmgw there to see what to do.";

/// How the card is read: by a host with no fix (the sentence whole), by a
/// card that does not hold the fix while another below does ("Open lmgw
/// there…" goes), or by the card that holds it (its first sentence alone,
/// D187: the check under it says the rest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reading {
    NoFix,
    FixBelow,
    HoldsFix {
        /// The page is on the computer lmgw runs on.
        here: bool,
    },
}

impl Parts {
    /// `message` read for the card as `reading` says.
    pub(super) fn of(message: &str, reading: Reading) -> Self {
        let (head, said) = match message.find(CLAUSE) {
            Some(i) => (&message[..=i], Some(message[i + 2..].trim().to_string())),
            None => (message, None),
        };
        let head = head.trim();
        let sentence = match reading {
            Reading::NoFix => head.to_string(),
            Reading::FixBelow => head
                .strip_suffix(SEE_THERE.trim_start())
                .unwrap_or(head)
                .trim_end()
                .to_string(),
            Reading::HoldsFix { here } => {
                let first = head.find(". ").map_or(head, |i| &head[..=i]);
                if here {
                    first
                        .replace(" of the computer lmgw runs on", "")
                        .replace(" on the computer lmgw runs on", "")
                } else {
                    first.to_string()
                }
            }
        };
        Self { sentence, said }
    }
}

/// A turn that failed with `gpu_setup`: its sentence, what the start said
/// in "Details", and under the newest such card the host's fix. While that
/// card is on the page the banner above steps aside (D187). A "Not now"
/// said before the failure ends by core's stamp on the check (D182), not
/// by this card.
#[component]
pub(super) fn GpuSetupCard(message: RwSignal<Option<String>>) -> impl IntoView {
    let host = slot();
    // No number until the message is there: nothing is the newest before.
    let mine = StoredValue::new(u64::MAX);
    Effect::new(move |numbered: Option<bool>| {
        let has = message.with(Option::is_some);
        if has && numbered != Some(true) {
            if let Some(h) = host {
                h.newest.update(|n| *n += 1);
                let n = h.newest.get_untracked();
                mine.set_value(n);
                h.holding.set(Some(n));
            }
        }
        has || numbered == Some(true)
    });
    on_cleanup(move || {
        if let Some(h) = host {
            let n = mine.try_get_value();
            if n.is_some() && h.holding.try_get_untracked().flatten() == n {
                h.holding.try_set(None);
            }
        }
    });
    move || {
        message.get().map(|m| {
            let holds = move || host.is_some_and(|h| h.newest.get() == mine.get_value());
            let reading = move || match host {
                None => Reading::NoFix,
                Some(h) if holds() => Reading::HoldsFix { here: h.here },
                Some(_) => Reading::FixBelow,
            };
            let said = Parts::of(&m, Reading::NoFix).said;
            let sentence = move || Parts::of(&m, reading()).sentence;
            // Only the newest card holds the fix (and its frame).
            let fix = host.map(|h| {
                move || {
                    holds().then(|| view! { <div class="chat-gpu-setup-fix">{h.fix.run(())}</div> })
                }
            });
            view! {
                <div class="chat-gpu-setup" role="alert">
                    <div class="chat-gpu-setup-head">
                        <span class="chat-gpu-setup-mark" aria-hidden="true">"!"</span>
                        <span>{sentence}</span>
                    </div>
                    {said.map(|s| {
                        view! {
                            <details class="chat-gpu-setup-said">
                                <summary>"Details"</summary>
                                <div>{s}</div>
                            </details>
                        }
                    })}
                    {fix}
                </div>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &str = "'qwen-demo-8b' could not start on the graphics card of the computer lmgw \
                       runs on. One piece is missing so models can use the graphics card. lmgw \
                       can set it up. Open lmgw there to see what to do. (its start said: Error: \
                       setting up CDI devices: unresolvable CDI devices nvidia.com/gpu=all) \
                       (fallback 'cloud-demo' is disabled)";

    #[test]
    fn the_card_keeps_the_sentence_and_folds_what_the_start_said() {
        let p = Parts::of(MSG, Reading::NoFix);
        assert!(p.sentence.starts_with("'qwen-demo-8b' could not start"));
        assert!(p.sentence.ends_with("Open lmgw there to see what to do."));
        assert_eq!(
            p.said.as_deref(),
            Some(
                "(its start said: Error: setting up CDI devices: unresolvable CDI devices \
                 nvidia.com/gpu=all) (fallback 'cloud-demo' is disabled)"
            )
        );
    }

    #[test]
    fn where_the_fix_is_below_the_sentence_does_not_send_the_person_away() {
        let p = Parts::of(MSG, Reading::FixBelow);
        assert!(
            p.sentence.ends_with("lmgw can set it up."),
            "{}",
            p.sentence
        );
        assert!(p.said.is_some());
    }

    /// M4: the card that holds the fix says what failed, once; the check
    /// under it says why and what to do.
    #[test]
    fn the_card_that_holds_the_fix_says_only_what_failed() {
        let p = Parts::of(MSG, Reading::HoldsFix { here: true });
        assert_eq!(
            p.sentence,
            "'qwen-demo-8b' could not start on the graphics card."
        );
        assert!(p.said.is_some());
        // A browser on another computer still says whose card.
        let p = Parts::of(MSG, Reading::HoldsFix { here: false });
        assert_eq!(
            p.sentence,
            "'qwen-demo-8b' could not start on the graphics card of the computer lmgw runs on."
        );
        // No card at all has words of its own (D65).
        let m = "'qwen-demo-8b' could not start: no graphics card was found on the computer \
                 lmgw runs on. Cloud models still work.";
        assert_eq!(
            Parts::of(m, Reading::HoldsFix { here: true }).sentence,
            "'qwen-demo-8b' could not start: no graphics card was found."
        );
    }

    /// L3: a refused hop with no route has a fallback's clause after
    /// "Open lmgw there…" and no "(its start said:" (`gate::chain`): the
    /// clause still folds, and the sentence still drops what the page shows.
    #[test]
    fn a_fallback_clause_after_the_sentence_folds_without_the_start_s_words() {
        let m = "'qwen-demo-8b' could not start on the graphics card of the computer lmgw runs \
                 on. One piece is missing so models can use the graphics card. lmgw can set it \
                 up. Open lmgw there to see what to do. (fallback 'cloud-demo' is disabled) \
                 (tried: qwen-demo-8b)";
        let p = Parts::of(m, Reading::FixBelow);
        assert!(
            p.sentence.ends_with("lmgw can set it up."),
            "{}",
            p.sentence
        );
        assert_eq!(
            p.said.as_deref(),
            Some("(fallback 'cloud-demo' is disabled) (tried: qwen-demo-8b)")
        );
        let p = Parts::of(m, Reading::HoldsFix { here: true });
        assert_eq!(
            p.sentence,
            "'qwen-demo-8b' could not start on the graphics card."
        );
    }

    #[test]
    fn a_message_without_the_start_s_words_is_the_sentence_alone() {
        let m = "'qwen-demo-8b' could not start: no graphics card was found on the computer \
                 lmgw runs on. Cloud models still work.";
        assert_eq!(
            Parts::of(m, Reading::FixBelow),
            Parts {
                sentence: m.into(),
                said: None
            }
        );
    }
}
