//! An admission that found nothing of lmgw's to evict (§4): one look for a
//! container the registry lost track of before it settles into waiting, and a
//! stage that says what the wait is for.
//!
//! Seen 2026-10-06 (`docs/design/2026-10-06-registry-owns-start.md`): a
//! model's container was running with no registry entry — a start abandoned
//! mid-load — so its memory read as outside use. The next request for that
//! very model never saw it resident, eviction found nothing, and the request
//! held the admission gate for the whole `vram.queue_timeout_seconds` under
//! the stage "waiting for a busy model to finish" — with no busy model
//! anywhere — before it was refused.

use std::time::{Duration, Instant};

use crate::hf::fmt_bytes;

use super::ledger::Ledger;
use super::nvml::DeviceId;
use super::{Target, VramScheduler};
use crate::runtime::lifecycle::REAP_INTERVAL;
use crate::runtime::registry::{PassWait, RuntimeState};
use crate::state::SharedState;
use lmgw_api_types::WaitCard;

impl VramScheduler {
    /// `evict_one` came back empty for the first time in this wait: run a
    /// reconciliation pass ([`crate::runtime::lifecycle::readopt`]) — or
    /// wait for the one running — before settling into waiting. `true`: it
    /// changed what the registry holds; the target itself may be resident
    /// now, or a model the wait may evict.
    ///
    /// The caller has let the admission gate go: a pass asks podman, and the
    /// admissions queued behind this one have no reason to wait for that.
    /// Bounded by what is left of this wait's `vram.queue_timeout_seconds`,
    /// and by one reaper tick — a pass that takes longer is stuck on podman,
    /// and the tick's own pass carries on with it. The reaper tick's pass
    /// covers the rest of the wait.
    pub(super) async fn look_for_unheld(
        &self,
        state: &SharedState,
        waiter_id: u64,
        started: Instant,
        budget: Option<Duration>,
    ) -> bool {
        let within = budget.map_or(REAP_INTERVAL, |d| {
            d.saturating_sub(started.elapsed()).min(REAP_INTERVAL)
        });
        self.set_stage(waiter_id, "looking for containers lmgw lost track of");
        crate::runtime::lifecycle::readopt(state, PassWait::Join, within).await
    }
}

/// What a wait with nothing to evict is waiting for, from the ledger it
/// decided on: lmgw's own models still busy (or still starting), or — when
/// none of them is on the card at all — memory lmgw has no say over.
///
/// `on`: the cards the wait is on, and `cards` the same named, on a
/// computer with several (device-ledger design §5.5, §10): the words then
/// say which card is short, and only lmgw's models there count. Empty on
/// one card: the words are as they always were.
pub(super) fn waiting_stage(
    l: &Ledger,
    target: &Target,
    needs: u64,
    free: u64,
    on: &[DeviceId],
    cards: &[WaitCard],
) -> String {
    let others: Vec<_> = l
        .entries
        .iter()
        .filter(|e| !(e.class == target.class && e.model_id == target.model_id))
        .filter(|e| {
            cards.is_empty()
                || l.residents
                    .iter()
                    .any(|r| r.generation == Some(e.generation) && Ledger::is_on(r, on))
        })
        .collect();
    let there = match cards {
        [] => String::new(),
        [_] => " on that card".to_string(),
        _ => " on those cards".to_string(),
    };
    if others.is_empty() {
        return format!(
            "needs {}{}, {} free{} — nothing of lmgw's is on {} to evict; waiting for memory \
             held outside lmgw",
            fmt_bytes(needs),
            on_words(cards),
            fmt_bytes(free),
            if cards.is_empty() { "" } else { " there" },
            if cards.is_empty() {
                "the GPU"
            } else {
                there.trim_start_matches(" on ")
            },
        );
    }
    if others.iter().all(|e| e.state == RuntimeState::Starting) {
        return format!(
            "waiting for a model that is still starting{}",
            on_words(cards)
        );
    }
    match cards {
        [] => "waiting for a busy model to finish".into(),
        _ => format!(
            "needs {}{}, {} free there — waiting for a busy model there to finish",
            fmt_bytes(needs),
            on_words(cards),
            fmt_bytes(free)
        ),
    }
}

/// The cards `on` as a wait names them, on a computer with several cards
/// (device-ledger design §10); empty on one card, so the words and the
/// phases there are as they always were.
pub(super) fn wait_cards(l: &Ledger, on: &[DeviceId]) -> Vec<WaitCard> {
    if l.devices.len() < 2 {
        return Vec::new();
    }
    on.iter()
        .filter_map(|id| l.devices.iter().find(|d| &d.id == id))
        .map(|d| WaitCard {
            id: d.id.clone(),
            index: d.index,
            name: d.name.clone(),
        })
        .collect()
}

/// " on NVIDIA RTX 6000 Ada Generation (card 2)", or nothing for no cards.
fn on_words(cards: &[WaitCard]) -> String {
    match cards {
        [] => String::new(),
        cards => format!(" on {}", WaitCard::list(cards)),
    }
}

/// What a wait that evicts says (chat-media-tools design §5.3): what it
/// needs and what is free, and the model it stops once one is chosen — the
/// figures stay beside the victim, so the queue's view keeps them while the
/// victim goes. On a computer with several cards it names the card it
/// makes room on (`cards`), and says so where the victim starts again on
/// another card (`moves`, device-ledger design §6.2).
pub(super) fn evicting_stage(
    needs: u64,
    free: Option<u64>,
    victim: Option<&str>,
    cards: &[WaitCard],
    moves: bool,
) -> String {
    let free = free
        .map(|f| {
            format!(
                ", {} free{}",
                fmt_bytes(f),
                if cards.is_empty() { "" } else { " there" }
            )
        })
        .unwrap_or_default();
    let victim = victim.map(|v| format!(" {v}")).unwrap_or_default();
    let moves = if moves && !victim.is_empty() {
        ", which starts again on another card that has room"
    } else {
        ""
    };
    format!(
        "needs {}{}{free} — evicting{victim}{moves}",
        fmt_bytes(needs),
        on_words(cards)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_eviction_keeps_its_figures_beside_the_victim() {
        let gib = 1024 * 1024 * 1024;
        let needs = 7 * gib;
        let free = 2 * gib;
        let text = evicting_stage(needs, Some(free), Some("chat/qwen"), &[], false);
        assert_eq!(
            text,
            format!(
                "needs {}, {} free — evicting chat/qwen",
                fmt_bytes(needs),
                fmt_bytes(free)
            )
        );
        assert_eq!(
            evicting_stage(needs, Some(free), None, &[], false),
            format!(
                "needs {}, {} free — evicting",
                fmt_bytes(needs),
                fmt_bytes(free)
            )
        );
        assert_eq!(
            evicting_stage(needs, None, Some("chat/qwen"), &[], false),
            format!("needs {} — evicting chat/qwen", fmt_bytes(needs))
        );
    }

    #[test]
    fn on_several_cards_the_words_name_the_card_and_a_move() {
        let gib = 1024 * 1024 * 1024;
        let card = WaitCard {
            id: DeviceId::new("GPU-0"),
            index: 2,
            name: "NVIDIA RTX 6000 Ada Generation".into(),
        };
        assert_eq!(
            evicting_stage(7 * gib, Some(2 * gib), Some("chat/qwen"), &[card], true),
            format!(
                "needs {} on NVIDIA RTX 6000 Ada Generation (card 2), {} free there — \
                 evicting chat/qwen, which starts again on another card that has room",
                fmt_bytes(7 * gib),
                fmt_bytes(2 * gib)
            )
        );
    }
}
