//! The context field's notice (device-ledger design §8.2, ruling 15): on a
//! machine of several cards, a chat row that sets no context is kept on
//! one card at its own context where one holds it, else the largest that
//! fits one card; the editor says so where the context is set.

use crate::config::Snapshot;
use crate::runtime::Class;

use super::super::nvml::DeviceId;
use super::super::{Target, VramScheduler};

impl VramScheduler {
    /// The context field's notice of chat row `model_id` (device-ledger
    /// design §8.2, ruling 15): while the row sets no context and the
    /// machine has more than one card it may use, what its own context is,
    /// that lmgw keeps it on one card, and what that card holds of it now.
    /// `None` on one card, with a set context, and where lmgw cannot hand
    /// it a card.
    pub async fn context_notice(&self, snap: &Snapshot, model_id: &str) -> Option<String> {
        let row = snap.local_models.iter().find(|m| m.model_id == model_id)?;
        if row.params.ctx_size.is_some() {
            return None;
        }
        let target = Target {
            class: Class::Chat,
            model_id: model_id.to_string(),
        };
        let devices = self.last_devices.lock().unwrap().clone();
        let cand = self.candidates(snap, &target, &devices);
        // Its own split kept on its pin goes on every card of it (§7.2).
        if !cand.hand_out || cand.cards.len() < 2 || cand.own_kept().is_some() {
            return None;
        }
        let fp = self.footprint(snap, Class::Chat, model_id).await?;
        let l = fp.llama.as_ref()?;
        let trained = l.sizer.trained?;
        let (card, _) = self.roomiest_now(snap, &target, &fp)?;
        let on = format!("{} (card {})", card.name, card.index);
        Some(match l.target.ctx {
            None if l.target.is_whole() => one_card_words(trained, None, &on),
            Some(ctx) => one_card_words(trained, Some(ctx), &on),
            None => format!(
                "{} Where that context does not fit one card, the largest that fits one card is \
                 used; now not all of its layers fit on {on} even at its least context. To \
                 spread the model over several cards for more, set a context.",
                one_card_lead(trained)
            ),
        })
    }

    /// `grant` of a start of `target` on the cards of `p`, with the warning
    /// of a context lowered to fit one card of several in the words of the
    /// context field's notice (device-ledger design §8.2, ruling 15): a
    /// chat row that sets no context, on one card of several it may use.
    /// Any other grant as it is.
    pub(in crate::vram) fn one_card_grant(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &super::super::Footprint,
        p: &super::Placement,
        l: &super::super::ledger::Ledger,
        grant: super::super::partial::Grant,
    ) -> super::super::partial::Grant {
        use super::super::partial::llama::CONTEXT_FITTED;
        let super::super::partial::Grant::Partial(mut g) = grant else {
            return grant;
        };
        let lowered = g
            .llama
            .filter(|pl| pl.n_gpu_layers.is_none() && pl.n_cpu_moe.is_none())
            .and_then(|pl| pl.ctx);
        let rest = g
            .warning
            .strip_prefix(CONTEXT_FITTED)
            .and_then(|r| r.strip_prefix(": "))
            .map(str::to_string);
        let trained = fp.llama.as_ref().and_then(|f| f.sizer.trained);
        let sets_none = snap
            .local_models
            .iter()
            .find(|m| m.model_id == target.model_id)
            .is_some_and(|m| m.params.ctx_size.is_none());
        let (Some(ctx), Some(rest), Some(trained), [card], true, Class::Chat) = (
            lowered,
            rest,
            trained,
            p.cards.as_slice(),
            sets_none,
            target.class,
        ) else {
            return super::super::partial::Grant::Partial(g);
        };
        let cand = self.candidates(snap, target, &l.devices);
        if cand.hand_out && cand.cards.len() > 1 {
            let on = card_label(&l.devices, card);
            g.warning = format!(
                "{CONTEXT_FITTED}: {} {rest}",
                one_card_words(trained, Some(ctx), &on)
            );
        }
        super::super::partial::Grant::Partial(g)
    }
}

/// The first two sentences of the context field's notice (§8.2).
fn one_card_lead(trained: u64) -> String {
    format!(
        "No context set: the model's own is used ({} tokens). lmgw keeps this model on one \
         card.",
        thousands(trained)
    )
}

/// The context field's notice (§8.2) for a model whose own context is
/// `trained`: on card `on` it fits (`fitted` `None`), or the largest that
/// fits there is `fitted`. The start that lowers the context says the same
/// ([`VramScheduler::one_card_grant`]).
fn one_card_words(trained: u64, fitted: Option<u64>, on: &str) -> String {
    let lead = one_card_lead(trained);
    match fitted {
        None => format!("{lead} It fits on {on}."),
        Some(ctx) => format!(
            "{lead} Where that context does not fit one card, the largest that fits one card is \
             used. Now that would be {} tokens on {on}. To spread the model over several cards \
             for more, set a context.",
            thousands(ctx)
        ),
    }
}

impl VramScheduler {
    /// The line under the copyable command line (device-ledger design
    /// §7.1, §8): the line shows the arguments as written. A pinned row
    /// (its GPU pin, or run arguments that pin it, §8.1) says which cards
    /// it is pinned to and what happens where they are gone; where lmgw
    /// chooses the card it says so, with the card it would choose now. A
    /// row with its own `--tensor-split` hears whether it is kept and, where
    /// it is not, the pin it needs (§7.2). `None` for an unpinned row on one
    /// card, and where the row names its card or lmgw cannot hand it one.
    pub async fn card_note(
        &self,
        state: &crate::state::SharedState,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
    ) -> Option<String> {
        let target = Target {
            class,
            model_id: model_id.to_string(),
        };
        let devices = self.last_devices.lock().unwrap().clone();
        let cand = self.candidates(snap, &target, &devices);
        let note = self
            .card_note_of(state, snap, &target, &devices, &cand)
            .await;
        // What becomes of the row's own --tensor-split (§7.2, rulings 11
        // and 16): kept on its pin, or the pin it needs.
        let own = cand.own.as_ref().map(|f| f.editor_note());
        let note = match (note, own) {
            (Some(n), Some(o)) => format!("{n} {o}"),
            (n, o) => n.or(o)?,
        };
        // Where it runs now (§10): the cards its container is charged on.
        let on: Vec<String> = state
            .runtime()
            .list()
            .into_iter()
            .find(|e| e.class == class && e.model_id == model_id)
            .and_then(|e| e.card_charge)
            .map(|c| c.cards())
            .unwrap_or_default()
            .iter()
            .filter(|_| devices.len() > 1)
            .map(|c| card_label(&devices, c))
            .collect();
        Some(match on.is_empty() {
            true => note,
            false => format!("It runs on {} now. {note}", in_words(&on)),
        })
    }

    /// [`Self::card_note`] without where the model runs now.
    async fn card_note_of(
        &self,
        state: &crate::state::SharedState,
        snap: &Snapshot,
        target: &Target,
        devices: &[super::super::GpuMemory],
        cand: &super::choose::Candidates,
    ) -> Option<String> {
        let (class, model_id) = (target.class, target.model_id.as_str());
        if let Some((pin, from)) = &cand.pin {
            let now = match cand.cards.len() > 1 && cand.hand_out {
                true => self.card_now(state, snap, target).await,
                false => None,
            };
            // With no device read lmgw hands a pin's cards all the same
            // where it can (§5.7): to a container whose arguments pass
            // NVIDIA cards through CDI, pinned by UUID.
            let handable = match devices.is_empty() {
                false => cand.hand_out,
                true => crate::runtime::descriptor::model_runtime(snap, class, model_id)
                    .is_some_and(|rt| {
                        rt.placement().is_gpu()
                            && crate::runtime::argv::passes_cdi(&rt.extra_run_args)
                            && pin.iter().all(|c| c.as_str().starts_with("GPU-"))
                    }),
            };
            return Some(pin_note(devices, cand, pin, *from, handable, now));
        }
        if !cand.hand_out || cand.cards.len() < 2 {
            return None;
        }
        let now = self
            .card_now(state, snap, target)
            .await
            .map(|c| format!("; now it would be {c}"))
            .unwrap_or_default();
        Some(format!(
            "lmgw picks the card when it starts this model and passes it in place of the \
             graphics cards these arguments name{now}."
        ))
    }

    /// The card `target` would start on now, by name and index, or the
    /// cards lmgw would split it over ("split over … and …", §7.2); `None`
    /// where it has no figure or would go over several cards as a group.
    async fn card_now(
        &self,
        state: &crate::state::SharedState,
        snap: &Snapshot,
        target: &Target,
    ) -> Option<String> {
        let fp = self.footprint(snap, target.class, &target.model_id).await?;
        let l = self.ledger(state, snap).await;
        let p = self.placement(snap, target, &fp, &l, super::Mode::Owner, None);
        match (p.cards.as_slice(), &p.split) {
            ([one], _) => Some(card_label(&l.devices, one)),
            (cards, Some(_)) => {
                let names: Vec<String> = cards.iter().map(|c| card_label(&l.devices, c)).collect();
                Some(format!("split over {}", in_words(&names)))
            }
            _ => None,
        }
    }
}

/// A card by name and index ("RTX PRO 6000 Blackwell (card 2)", §2.1), or
/// one this computer does not list, by its id.
pub(crate) fn card_label(devices: &[super::super::GpuMemory], id: &DeviceId) -> String {
    match devices.iter().find(|d| &d.id == id) {
        Some(d) => format!("{} (card {})", d.name, d.index),
        None if devices.is_empty() => id.as_str().to_string(),
        None => format!("a card this computer does not list ({})", id.as_str()),
    }
}

/// `items` joined as a list in words: "a", "a and b", "a, b and c".
pub(crate) fn in_words(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// [`VramScheduler::card_note`] for a pinned row (§8): the cards it is
/// pinned to and where that is written, where it starts, and what happens
/// where its cards are gone. `handable`: lmgw hands the engine its cards
/// (§7.1); `now`: the card it would start on now, for a pin of several.
fn pin_note(
    devices: &[super::super::GpuMemory],
    cand: &super::choose::Candidates,
    pin: &[DeviceId],
    from: super::choose::PinFrom,
    handable: bool,
    now: Option<String>,
) -> String {
    let names: Vec<String> = pin.iter().map(|id| card_label(devices, id)).collect();
    let lead = match from {
        super::choose::PinFrom::Field => format!("Pinned to {}", in_words(&names)),
        super::choose::PinFrom::RunArgs => format!(
            "Its run arguments pin it to {} (the GPU pin field can hold this instead)",
            in_words(&names)
        ),
    };
    if !devices.is_empty() && cand.cards.is_empty() {
        return format!(
            "{lead}: this computer lists none of them, so it does not start on another card; \
             its fallback answers where it has one. Set its GPU pin to a card this computer \
             has, or clear it."
        );
    }
    // Where lmgw hands the engine its cards (§7.1), they replace what the
    // arguments name; elsewhere the arguments pass the cards as written.
    // With no device read the cards are the pin's as written.
    let cards: Vec<DeviceId> = match devices.is_empty() {
        true => pin.to_vec(),
        false => cand.cards.clone(),
    };
    let differs = devices.is_empty()
        || cards.len() != cand.seen.len()
        || cards.iter().any(|c| !cand.seen.contains(c));
    let passes = match (handable, differs) {
        (true, true) => ", in place of the graphics cards these arguments name",
        (true, false) => "",
        (false, _) => ", with these arguments as written",
    };
    let gone = match pin.len() {
        1 => {
            "Where that card is gone, it does not start on another card, and its fallback \
              answers where it has one."
        }
        _ => {
            "Where all of them are gone, it does not start on another card, and its fallback \
              answers where it has one."
        }
    };
    let missing = match cand.missing.as_slice() {
        [] => String::new(),
        m => format!(
            " This computer does not list {} now.",
            in_words(
                &m.iter()
                    .map(|id| id.as_str().to_string())
                    .collect::<Vec<_>>()
            )
        ),
    };
    match cards.as_slice() {
        [one] => format!(
            "{lead}: lmgw starts it on {} alone{passes}.{missing} {gone}",
            card_label(devices, one)
        ),
        // Its own split kept: on every card of it (§7.2).
        _ if cand.own_kept().is_some() => {
            format!("{lead}: lmgw starts it on all of them{passes}.{missing} {gone}")
        }
        _ => format!(
            "{lead}: lmgw picks among them when it starts this model{passes}{}.{missing} {gone}",
            now.map(|c| format!("; now it would be {c}"))
                .unwrap_or_default()
        ),
    }
}

/// `n` with a comma every three digits: 262,144.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands_are_grouped() {
        assert_eq!(super::thousands(262144), "262,144");
        assert_eq!(super::thousands(4096), "4,096");
        assert_eq!(super::thousands(512), "512");
        assert_eq!(super::thousands(1000000), "1,000,000");
    }
}
