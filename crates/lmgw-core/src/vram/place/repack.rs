//! Repacking (device-ledger design §6.2, ruling 6, D8, D21): an eviction
//! whose victim is started again on another card.
//!
//! A model stopped for room on one card that another card holds now —
//! without evicting anything there — is started again there once it has
//! stopped. The request that needed the room does not wait for it: the
//! start is a task of its own, on the path a warm or boot start takes
//! (`check_background_start`'s decision, which waits for the gate and never
//! evicts or waits for room), and it keeps the model's origin.
//!
//! - **Its age moves with it**: the new container is given the `last_used`
//!   of the one it replaces, and its own start's claim leaves it so
//!   (`registry::Moved`); it stays where it was in the eviction order, and
//!   its idle timeout goes on running.
//! - **One card**: it is placed by step 1 alone ([`super::place`] answering
//!   `Now`), whole, never split and never fitted smaller; a split model
//!   stopped for room comes back on one card where one holds it.
//! - **Not again before it has served**: a moved model on which no claim
//!   has been taken since is an ordinary victim, stopped and not started
//!   again.
//! - **Only idle models move**, a pinned one only among its pin's cards,
//!   and only one lmgw can hand a card (§5.7): the others are placed on
//!   their cards as a group and cannot be moved. A row's own split kept on
//!   its pin (§7.2) is never moved.
//! - **Nothing is left unservable**: a request for the model while it moves
//!   starts it as any request does, and the move then does nothing; a move
//!   that cannot start leaves it stopped, as an eviction always did.

use std::time::{Duration, Instant};

use crate::config::Snapshot;
use crate::runtime::descriptor::model_runtime;
use crate::runtime::registry::{Origin, RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::ledger::Ledger;
use super::super::nvml::DeviceId;
use super::super::{Fit, Footprint, Target, VramScheduler};
use super::choose::{others_hold_on, whole_bytes};
use super::{card_label, in_words, place, Ask, Card, Mode, Placed, Placement, Spare};

/// A model stopped for room, to be started again on another card.
#[derive(Debug, Clone)]
pub(in crate::vram) struct Moving {
    pub class: Class,
    pub model_id: String,
    /// The cards it was on.
    pub from: Vec<DeviceId>,
    /// The cards it may go on now: its candidates but the ones it was
    /// stopped to make room on.
    pub to: Vec<DeviceId>,
    /// When it was last used, as it was stopped.
    pub last_used: Instant,
    /// Whom it ran for: kept (§6.2).
    pub origin: Origin,
    /// The model it made room for, for its warnings.
    pub for_model: String,
}

/// The card of `cards` a model moved to make room goes on (§6.2): by step 1
/// alone, now, each card keeping its margin. A model moved to make room
/// keeps its margin on its new card (WP13): the pass that gives it up
/// ([`super::Keep::Last`]) keeps it here, and a move that needs it given up
/// is no move.
pub(super) fn move_to(ask: &Ask, mut cards: Vec<Card>) -> Option<DeviceId> {
    for c in &mut cards {
        c.last = c.margin;
    }
    match place(ask, &cards, Mode::Owner) {
        Placed::Now(card) => Some(card),
        _ => None,
    }
}

impl VramScheduler {
    /// The cards other than `from` that `e`, stopped for room there, could
    /// be started again on now (§6.2, §5.2 step 1), as ledger `l` reads them:
    /// of its candidates lmgw can hand it, each card whose free memory —
    /// with what `e` itself holds there, which its stop frees — holds its
    /// whole footprint, the engine's cost there and what it keeps free
    /// there (its headroom where something else of lmgw's is on that card,
    /// else its margin). Empty on a machine of one card, under a declared
    /// budget (a bound on the whole, §9.2, which a move's second start is
    /// not judged against), for a model that is not ready, a moved one no
    /// claim has been taken on since (D21), one that cannot be handed a
    /// card, a row's own split kept on its pin, and one no other card holds
    /// now.
    pub(in crate::vram) fn elsewhere(
        &self,
        snap: &Snapshot,
        l: &Ledger,
        e: &RuntimeView,
        from: &[DeviceId],
    ) -> Vec<Spare> {
        if l.devices.len() < 2 || l.budget > 0 || e.moved || e.state != RuntimeState::Ready {
            return Vec::new();
        }
        let Some(fp) = l
            .footprints
            .iter()
            .find(|(g, _)| *g == e.generation)
            .map(|(_, f)| f)
        else {
            return Vec::new();
        };
        let target = Target {
            class: e.class,
            model_id: e.model_id.clone(),
        };
        let cand = self.candidates(snap, &target, &l.devices);
        // A row's own split kept on its pin is never moved: its pin's cards
        // are all its own, and one card would not hold its split (§7.2).
        if !cand.hand_out || cand.own_kept().is_some() {
            return Vec::new();
        }
        let heads = self.cards_headroom(snap);
        let own_target = fp.llama.as_ref().and_then(|l| l.sizer.fit_target);
        let at_least = |b: u64| own_target.map_or(b, |t| t.max(b));
        let resident = l
            .residents
            .iter()
            .find(|r| r.generation == Some(e.generation));
        let bytes = whole_bytes(fp);
        l.devices
            .iter()
            .filter(|d| cand.cards.contains(&d.id) && !from.contains(&d.id))
            .filter_map(|d| {
                let one = std::slice::from_ref(&d.id);
                let cap = l.capacity_on(one, None)?;
                let held = resident.map_or(0, |r| Ledger::held_on(r, &d.id));
                // A model moved to make room keeps its margin on its new
                // card: a move that needs it given up is no move (WP13).
                let r = self.rules_on(snap, &target, fp, &heads, &d.id);
                let keep = match others_hold_on(l, &target, one) {
                    true => at_least(r.headroom),
                    false => at_least(r.margin),
                };
                let takes = bytes.saturating_add(self.engine_on(target.class, &d.id));
                let needs = takes.saturating_add(keep);
                let free = cap.free.saturating_add(held);
                (free >= needs).then(|| Spare {
                    id: d.id.clone(),
                    index: d.index,
                    spare: free - needs,
                    takes,
                })
            })
            .collect()
    }

    /// Where `target`, moved, goes among `to` on ledger `l` (§6.2): its
    /// candidates of `to`, by step 1 alone — whole, on one card, now.
    /// `None` where no such card holds it now, or lmgw cannot hand it one.
    fn placement_for_move(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
        to: &[DeviceId],
    ) -> Option<Placement> {
        if l.devices.len() < 2 || l.budget > 0 {
            return None;
        }
        let cand = self.candidates(snap, target, &l.devices);
        if !cand.hand_out {
            return None;
        }
        let ids: Vec<DeviceId> = cand
            .cards
            .iter()
            .filter(|c| to.contains(c))
            .cloned()
            .collect();
        let cards = self.cards_for_place(snap, target, fp, l, &ids, None, false);
        let ask = Ask {
            bytes: whole_bytes(fp),
            adapts: false,
            split: None,
        };
        let card = move_to(&ask, cards)?;
        Some(self.placement_on(snap, target, fp, l, &cand, Placed::Now(card)))
    }

    /// After `e` was stopped to make room on `on` for `target` (§6.1):
    /// where another card holds it now, as ledger `l` read the cards before
    /// the stop, it is started again there by a task of its own
    /// ([`Self::start_moved`]); the caller does not wait for it. Nothing
    /// where `on` names no card (the pooled capacity).
    pub(in crate::vram) fn repack(
        &self,
        state: &SharedState,
        l: &Ledger,
        e: &RuntimeView,
        on: &[DeviceId],
        target: &Target,
    ) {
        if on.is_empty() {
            return;
        }
        let snap = state.snapshot();
        if self.elsewhere(&snap, l, e, on).is_empty() {
            return;
        }
        let moved = Target {
            class: e.class,
            model_id: e.model_id.clone(),
        };
        let to: Vec<DeviceId> = self
            .candidates(&snap, &moved, &l.devices)
            .cards
            .into_iter()
            .filter(|c| !on.contains(c))
            .collect();
        let from = l
            .residents
            .iter()
            .find(|r| r.generation == Some(e.generation))
            .map(|r| r.charge.cards())
            .unwrap_or_default();
        let now = Instant::now();
        let m = Moving {
            class: e.class,
            model_id: e.model_id.clone(),
            from,
            to,
            last_used: now
                .checked_sub(Duration::from_secs(e.last_used_age_seconds))
                .unwrap_or(now),
            origin: e.owner,
            for_model: target.model_id.clone(),
        };
        let st = state.clone();
        tokio::spawn(async move { st.vram.start_moved(&st, m).await });
    }

    /// Start `m` again on another card (§6.2): the decision a warm or boot
    /// start gets, under the gate, on `m.to` by step 1 alone; a reservation
    /// there, the entry given the age it had and a sentence that says where
    /// it came from, and the model's origin kept. Nothing where its row is
    /// gone or disabled, the hold or a benchmark's lease is on, a request
    /// started it meanwhile, or no card of `m.to` holds it now: it stays
    /// stopped, as an eviction leaves it, and its next request starts it.
    pub(in crate::vram) async fn start_moved(&self, state: &SharedState, m: Moving) {
        let snap = state.snapshot();
        let what = format!("{}/{}", m.class.as_str(), m.model_id);
        let stays = |why: &str| {
            tracing::info!(
                "{what} stays stopped after making room for {}: {why}",
                m.for_model
            );
        };
        let Some(runtime) = model_runtime(&snap, m.class, &m.model_id).filter(|r| r.enabled) else {
            return stays("its row is gone or disabled");
        };
        if snap.gpu_block_for(m.class, &m.model_id).is_some() {
            return stays("the GPU is held");
        }
        if !snap.settings.vram.enabled || !runtime.placement().is_gpu() {
            return stays("it is not placed on a card");
        }
        let target = Target {
            class: m.class,
            model_id: m.model_id.clone(),
        };
        let Some(fp) = self.footprint(&snap, m.class, &m.model_id).await else {
            return stays("its footprint could not be read");
        };
        let (permit, card, devices) = {
            let _gate = self.ask_gate().await;
            // A request for it started it meanwhile: it is where that
            // request put it.
            if self.is_up(state, &target) {
                return;
            }
            let l = self.ledger(state, &snap).await;
            if state.snapshot().gpu_block().is_some() {
                return stays("the GPU is held");
            }
            if l.capacity.is_none() {
                return stays("the cards cannot be read");
            }
            let Some(p) = self.placement_for_move(&snap, &target, &fp, &l, &m.to) else {
                return stays("no other card holds it now");
            };
            let card = p.cards.clone();
            match self.background_on(state, &snap, &target, fp, &l, p) {
                Fit::Go(permit) => (permit, card, l.devices.clone()),
                Fit::Full(why) | Fit::Held(why) | Fit::Failed(why) => return stays(&why),
                Fit::Unchecked | Fit::Downloaded => {
                    return stays("no decision placed it on another card")
                }
            }
        };
        let names = |cards: &[DeviceId]| {
            in_words(
                &cards
                    .iter()
                    .map(|c| card_label(&devices, c))
                    .collect::<Vec<_>>(),
            )
        };
        let note = format!(
            "Moved from {} to {} to make room for {}",
            names(&m.from),
            names(&card),
            m.for_model
        );
        let registry = state.runtime();
        registry.stage_moved(
            m.class,
            &m.model_id,
            crate::runtime::registry::Moved {
                last_used: m.last_used,
                note: note.clone(),
            },
        );
        let spec = super::super::queue::lifecycle_spec(state, &snap, &runtime);
        let started = registry.acquire_as(&spec, m.origin).await;
        drop(permit);
        match started {
            Ok(guard) => {
                drop(guard);
                crate::machine::after_start(state, &runtime);
                self.cache_pids(state);
                tracing::info!("{what}: {note}");
            }
            Err(e) => {
                let e = super::super::start_error(state, &runtime, e).await;
                tracing::warn!("{what} was not started again after making room: {e}");
            }
        }
    }
}
