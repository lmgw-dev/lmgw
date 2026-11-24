//! The scheduler's side of [`super::place`]: the candidate cards of a target
//! as one ledger read finds them, and what a decision then works with on
//! the cards it was answered ([`Placement`]).

use crate::config::Snapshot;
use crate::runtime::launcher::native::gpu::{selection, Selection};
use crate::runtime::registry::{Origin, RuntimeState};

use super::super::headroom::CardHeadroom;
use super::super::ledger::Ledger;
use super::super::nvml::DeviceId;
use super::super::scheduler::MIB;
use super::super::{Footprint, Target, VramScheduler};
use super::{place, Ask, Card, Holder, Mode, Placed};

/// What a decision works with on the cards `place` answered: the cards, the
/// answer, and what the start keeps free and costs on them together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct Placement {
    /// The cards; empty where no device read names a card (a declared
    /// budget without telemetry), which is the pooled capacity.
    pub cards: Vec<DeviceId>,
    pub placed: Placed,
    /// lmgw hands the engine exactly these cards (§7.1, D17): fewer than
    /// its run arguments pass, all of them NVIDIA cards a container is
    /// handed through CDI. False where the arguments are used as written.
    pub handed: bool,
    /// What the start keeps free on them while lmgw's own models there
    /// could be waited for or evicted: each card's headroom and its margin
    /// ([`Rules::headroom`]).
    pub headroom: u64,
    /// Once nothing of lmgw's is left there: the floors and its margin
    /// ([`Rules::margin`]).
    pub margin: u64,
    /// Once nothing else fits either: the floors alone, its margin given up
    /// ([`Rules::floor`]).
    pub floor: u64,
    /// Beside it on them with nothing else of lmgw's there (§5.8,
    /// [`Rules::structural`]).
    pub structural: u64,
    /// The engine's own cost on them (§4.2).
    pub engine: u64,
    /// What the start's warnings say of how it was placed (§8.1, §7.2).
    pub note: Option<String>,
    /// Over several cards (§4.3, §7.2): the split, and what llama-server
    /// is told of it — nothing for the row's own split, which it is
    /// started with as written (ruling 11).
    pub split: Option<(super::Split, Option<crate::runtime::argv::TensorSplit>)>,
    /// The engine is handed its cards for the row's own `--tensor-split`,
    /// kept as written (§7.2, ruling 11).
    pub own_kept: bool,
    /// A split's parts on each card by kind, in MiB, for the start's log
    /// beside its note ([`split_kinds`]); `None` for anything else.
    pub kinds: Option<String>,
}

impl Placement {
    /// How many cards its engine sees (D14): the count a fit and a partial
    /// grant take; `None` for the pooled capacity, where none is known.
    pub(in crate::vram) fn count(&self) -> Option<usize> {
        (!self.cards.is_empty()).then_some(self.cards.len())
    }
}

/// What a start keeps free beside it on one card (§5.6, §9.1, WP13), each
/// figure the class's margin included where it says so, in the order they
/// are given up: the learned room first, then the margin with it — both
/// only where nothing else fits (module doc of `vram::headroom`). The
/// floor, or an amount declared for the card, is never given up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::vram) struct Rules {
    /// While it can make room another way: the card's floor and learned
    /// room, and its margin.
    pub headroom: u64,
    /// The learned room given up: the floor and its margin. What a row
    /// that adapts is fitted beside (`partial::llama`), what a model moved
    /// to make room keeps on its new card (§6.2), and what a start keeps
    /// once nothing of lmgw's is left there while anything smaller fits
    /// beside it.
    pub margin: u64,
    /// The learned room and the margin given up: the floor alone, what a
    /// start keeps once nothing of lmgw's is left and nothing else fits.
    pub floor: u64,
    /// What it needs beside it here with nothing else of lmgw's there
    /// (§5.8, "too large"): what other programs hold here at rest and the
    /// floor in `auto`, else the floor — never the learned room nor the
    /// margin, so a model that fits beside the floor is never refused.
    pub structural: u64,
}

/// The candidate cards of a target (§5.1), and whether lmgw can hand its
/// engine a card (D18).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::vram) struct Candidates {
    pub cards: Vec<DeviceId>,
    /// The cards its run arguments pass (`charge::seen`).
    pub seen: Vec<DeviceId>,
    /// An NVIDIA card handed through CDI to a container: it may be placed
    /// on fewer cards than it sees. Otherwise it is placed on what it sees
    /// as a group (§5.7).
    pub hand_out: bool,
    /// The pinned cards this machine does not list (§8): where none of its
    /// pin is left, the start does not go to another card.
    pub missing: Vec<DeviceId>,
    /// The row's own `CUDA_VISIBLE_DEVICES`, which is not passed where lmgw
    /// hands the engine its cards (§8.1), as written.
    pub cuda_visible: Option<String>,
    /// Its pin (§8) as written, and where it is written; `None` unpinned.
    pub pin: Option<(Vec<DeviceId>, PinFrom)>,
    /// llama-server's own flags in the row's arguments that number a card
    /// (§8.1, `argv::card_flags`), which are not passed where lmgw hands
    /// the engine its cards, as written — except beside the row's own split
    /// kept on its pin.
    pub engine_cards: Vec<String>,
    /// What becomes of the row's own `--tensor-split` (§7.2, rulings 11
    /// and 16): where lmgw hands the engine its cards on a machine of
    /// several cards; `None` everywhere else, and for a row without one.
    pub own: Option<super::OwnFate>,
}

impl Candidates {
    /// The row's own split, kept on its pin (§7.2, ruling 11).
    pub(in crate::vram) fn own_kept(&self) -> Option<&super::OwnFate> {
        self.own.as_ref().filter(|f| f.kept())
    }
}

/// Where a model's pin is written (§8, §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::vram) enum PinFrom {
    /// The row's GPU pin field.
    Field,
    /// Its run arguments: `NVIDIA_VISIBLE_DEVICES` or an NVIDIA device node.
    RunArgs,
}

/// What `fp` puts where by layer (§4.3), of the row as written: a footprint
/// fitted to one card before (the preview, `VramScheduler::footprint`) is
/// taken apart whole.
pub(in crate::vram) fn whole_by_layer(
    fp: &Footprint,
) -> Option<std::sync::Arc<super::super::plan::chat::by_layer::ByLayer>> {
    let whole = fp.llama.as_ref().and_then(|l| {
        l.sizer
            .footprint(&crate::runtime::registry::LlamaPlacement::default())
            .by_layer
    });
    whole.or_else(|| fp.by_layer.clone())
}

/// The whole of `fp`: the row as written (§5.2), a llama-server row fitted
/// by an earlier reading included.
pub(in crate::vram) fn whole_bytes(fp: &Footprint) -> u64 {
    match &fp.llama {
        Some(l) => l
            .sizer
            .charge(&crate::runtime::registry::LlamaPlacement::default()),
        None => fp.total_bytes,
    }
}

impl VramScheduler {
    /// What `place` is asked for `fp` of `target`.
    pub(in crate::vram) fn ask(&self, snap: &Snapshot, target: &Target, fp: &Footprint) -> Ask {
        Ask {
            bytes: whole_bytes(fp),
            adapts: fp.llama.is_some() || fp.partial.is_some(),
            split: self.splits(snap, target, fp).ok(),
        }
    }

    /// What a split of `target` puts where (§4.3), where lmgw may split it
    /// over several cards (§5.2 steps 3 and 4): a chat row that sets its
    /// context (ruling 15 keeps one that sets none on one card), whose
    /// header says what each layer holds and whose llama-server can be told
    /// a split (`-devd` too with a draft model). A row's own
    /// `--tensor-split` kept on its pin is not asked this (ruling 11,
    /// [`Candidates::own_kept`]); one not kept is placed as if it were not
    /// there, so lmgw may split it. `Err(Some(why))` for such a row that
    /// cannot be split, `Err(None)` for every other model.
    pub(in crate::vram) fn splits(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
    ) -> Result<std::sync::Arc<super::super::plan::chat::by_layer::ByLayer>, Option<String>> {
        if target.class != crate::runtime::Class::Chat {
            return Err(None);
        }
        let row = snap
            .local_models
            .iter()
            .find(|m| m.model_id == target.model_id)
            .ok_or(None)?;
        if !super::super::plan::chat::sets_context(row) {
            return Err(None);
        }
        // The row as written (§5.2).
        let cost = whole_by_layer(fp).ok_or_else(|| {
            Some(
                "its file's header does not say what each layer holds (no layer count, or a \
                 tensor list lmgw could not read whole)"
                    .to_string(),
            )
        })?;
        let vocab = self.split_vocab(snap, &target.model_id).map_err(Some)?;
        if cost.drafter && !vocab.devd {
            return Err(Some(
                "its image's llama-server has no -devd to keep its draft model on the first \
                 card"
                    .into(),
            ));
        }
        Ok(cost)
    }
}

impl VramScheduler {
    /// `target`'s candidate cards of `devices` (§5.1): its pin (§8) where it
    /// has one — the row's field, else the cards its run arguments name by
    /// `NVIDIA_VISIBLE_DEVICES` or an NVIDIA device node (§8.1) — of the
    /// cards this machine lists; else the cards its run arguments pass
    /// ([`Self::seen_cards`]): every card where they pass every card, the
    /// ones they name where they name cards (also a pin, §8.1). lmgw hands
    /// a card out only to a container (no native build, whose device order
    /// is not mapped) whose arguments select NVIDIA cards through CDI or
    /// `--gpus`, on NVIDIA cards (D18).
    pub(in crate::vram) fn candidates(
        &self,
        snap: &Snapshot,
        target: &Target,
        devices: &[super::super::GpuMemory],
    ) -> Candidates {
        let seen = self.seen_cards(snap, target, devices);
        let rt = crate::runtime::descriptor::model_runtime(snap, target.class, &target.model_id);
        let selects = rt.as_ref().is_some_and(|rt| {
            !matches!(
                selection(&rt.extra_run_args, rt.placement()),
                Selection::None
            ) && crate::runtime::argv::passes_cdi(&rt.extra_run_args)
        });
        let hand_out = selects
            && self.engine_build(target.class).is_none()
            && !seen.is_empty()
            && seen.iter().all(|c| c.as_str().starts_with("GPU-"));
        let pinned = snap
            .gpu_pins
            .get(&(target.class, target.model_id.clone()))
            .map(|p| (p.clone(), PinFrom::Field))
            .or_else(|| {
                rt.as_ref()
                    .and_then(|rt| env_pin(&rt.extra_run_args, devices))
                    .map(|p| (p, PinFrom::RunArgs))
            });
        let pin = pinned.as_ref().map(|(p, _)| p.clone());
        let (cards, missing) = match pin {
            Some(pin) if !devices.is_empty() => (
                devices
                    .iter()
                    .filter(|d| pin.contains(&d.id))
                    .map(|d| d.id.clone())
                    .collect(),
                pin.into_iter()
                    .filter(|p| !devices.iter().any(|d| &d.id == p))
                    .collect(),
            ),
            _ => (seen.clone(), Vec::new()),
        };
        let cuda_visible = rt
            .as_ref()
            .and_then(|rt| env_value(&rt.extra_run_args, "CUDA_VISIBLE_DEVICES"));
        let args = match rt.as_ref().and_then(|rt| rt.llama.as_ref()) {
            Some(
                crate::runtime::argv::LlamaArgs::Chat { args, .. }
                | crate::runtime::argv::LlamaArgs::Aux { args, .. },
            ) => args.as_slice(),
            None => &[],
        };
        let engine_cards = crate::runtime::argv::card_flags(args).cards;
        // The pin a row's own split is held to (§7.2): its pin as written,
        // else the cards its run arguments name where they name fewer than
        // the machine's (also a pin, §8.1).
        let own = (devices.len() > 1 && hand_out)
            .then(|| {
                let pin = match &pinned {
                    Some((p, _)) => Some((p.len(), missing.as_slice())),
                    None if seen.len() < devices.len() => Some((seen.len(), &[][..])),
                    None => None,
                };
                super::own_fate(target.class, args, pin)
            })
            .flatten();
        Candidates {
            cards,
            seen,
            hand_out,
            missing,
            cuda_visible,
            pin: pinned,
            engine_cards,
            own,
        }
    }

    /// What a start of `target` with footprint `fp` keeps free on card
    /// `card` (§5.6, headroom per card §9.1, WP13), as [`Rules`]. A card
    /// without a record of its own keeps the fixed value.
    pub(in crate::vram) fn rules_on(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        heads: &[CardHeadroom],
        card: &DeviceId,
    ) -> Rules {
        let margin = super::super::headroom::margin_of(snap, target.class, &target.model_id, fp);
        Self::rules_with(snap, heads, card, margin)
    }

    /// [`Self::rules_on`] for a start whose margin is `margin`.
    fn rules_with(snap: &Snapshot, heads: &[CardHeadroom], card: &DeviceId, margin: u64) -> Rules {
        let fixed = snap.settings.vram.headroom_mb.saturating_mul(MIB);
        let (whole, floor, structural) = match heads.iter().find(|h| h.id.as_ref() == Some(card)) {
            Some(h) => (h.bytes, h.floor, h.structural),
            None => (fixed, fixed, fixed),
        };
        Rules {
            headroom: whole.saturating_add(margin),
            margin: floor.saturating_add(margin),
            floor,
            structural,
        }
    }

    /// Where a start of `target` with footprint `fp` goes on ledger `l`, for
    /// `mode` (§5): its candidates as cards, asked of [`place`], with what
    /// it keeps free and costs on the cards answered. `own`: the start's
    /// claim, whose own bytes are no other start's (§5.5).
    pub(in crate::vram) fn placement(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
        mode: Mode,
        own: Option<u64>,
    ) -> Placement {
        let cand = self.candidates(snap, target, &l.devices);
        if l.devices.is_empty() || cand.cards.is_empty() {
            return self.placement_on(snap, target, fp, l, &cand, Placed::Group(Vec::new()));
        }
        if !cand.hand_out {
            let placed = Placed::Group(cand.cards.clone());
            return self.placement_on(snap, target, fp, l, &cand, placed);
        }
        // A row's own split kept on its pin: on its pin's cards as written
        // (§7.2, ruling 11), never asked of `place`.
        if let Some(own_split) = cand.own_kept() {
            let cards = self.cards_for_place(snap, target, fp, l, &cand.cards, own, false);
            let placed = match own_split.over(whole_by_layer(fp).as_deref(), &cards) {
                Some(split) => super::own_split::fixed(split, &cards, mode),
                None => Placed::Group(cand.cards.clone()),
            };
            return self.placement_on(snap, target, fp, l, &cand, placed);
        }
        // Where a victim could be started again matters to a start that
        // may evict (§5.2 step 2).
        let moves = mode != Mode::Still;
        let cards = self.cards_for_place(snap, target, fp, l, &cand.cards, own, moves);
        let placed = place(&self.ask(snap, target, fp), &cards, mode);
        self.placement_on(snap, target, fp, l, &cand, placed)
    }

    /// What admission keeps beside a llama.cpp start (a chat or an aux row)
    /// on `card` — its [`Rules`] with the llama.cpp margin, and the
    /// engine's cost there (§4.2) — or, with no card (`None`: no device
    /// read names one), the pooled sums every card keeps, as
    /// [`Self::rules_over`] reads them, and no engine cost. What a fit to
    /// the card searches a context against, so it lands on one admission
    /// starts (a bug found 2026-11-18 on a one-card machine: the fit kept
    /// neither a card's "always" amount whole nor the engine's cost).
    pub(in crate::vram) fn llama_rules(
        &self,
        snap: &Snapshot,
        card: Option<&DeviceId>,
    ) -> (Rules, u64) {
        let heads = self.cards_headroom(snap);
        let margin = super::super::headroom::LLAMA_MARGIN;
        match card {
            Some(c) => (
                Self::rules_with(snap, &heads, c, margin),
                self.engine_on(crate::runtime::Class::Chat, c),
            ),
            None => {
                let s = super::super::headroom::sums(&heads);
                (
                    Rules {
                        headroom: s.bytes.saturating_add(margin),
                        margin: s.floor.saturating_add(margin),
                        floor: s.floor,
                        structural: s.structural,
                    },
                    0,
                )
            }
        }
    }

    /// Where `place` answered `p` the roomiest card for a row only a split
    /// holds — one that does not adapt and that lmgw may split, on a
    /// machine of several cards (owner, 2026-11-18) — the cards that split
    /// waits on: those a split would go on once lmgw's models there have
    /// left, busy ones too, else every candidate. Such a start commits to
    /// no card: the roomiest one can never hold it, so it asks `place`
    /// again on every pass and evicts nothing there. `None` for every other
    /// answer.
    pub(in crate::vram) fn split_wait(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
        p: &Placement,
    ) -> Option<Vec<DeviceId>> {
        if !matches!(p.placed, Placed::Roomiest(_)) || l.devices.len() < 2 {
            return None;
        }
        let ask = self.ask(snap, target, fp);
        if ask.adapts || ask.split.is_none() {
            return None;
        }
        let cand = self.candidates(snap, target, &l.devices);
        let cards = self.cards_for_place(snap, target, fp, l, &cand.cards, None, false);
        Some(super::split_once_left(&ask, &cards).unwrap_or(cand.cards))
    }

    /// [`Self::placement`] for cards already chosen: `placed`, as a claim
    /// keeps them (§5.5).
    pub(in crate::vram) fn placement_on(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
        cand: &Candidates,
        placed: Placed,
    ) -> Placement {
        let cards = placed.cards();
        let rules = self.rules_over(snap, target, fp, &cards);
        if cards.is_empty() {
            // The pooled capacity: the sums the decisions read before
            // lmgw placed (no device read names a card).
            return Placement {
                cards,
                placed,
                handed: false,
                note: None,
                headroom: rules.headroom,
                margin: rules.margin,
                floor: rules.floor,
                structural: rules.structural,
                engine: 0,
                split: None,
                own_kept: false,
                kinds: None,
            };
        }
        // A row's own split kept on its pin is started as written (§7.2).
        let own_kept = cand.own_kept().is_some();
        // A split is told its cards in the bus's order, so its engine is
        // handed them whatever its arguments pass (§2.2, §7.2).
        let split = placed.split().and_then(|sp| {
            if own_kept {
                return Some((sp.clone(), None));
            }
            let vocab = self.split_vocab(snap, &target.model_id).ok()?;
            let tensor =
                crate::runtime::argv::TensorSplit::new(&sp.entries(), sp.drafter, !vocab.fit)?;
            Some((sp.clone(), Some(tensor)))
        });
        // Fewer cards than the arguments pass, or others than they name; a
        // split; a row's own split, kept or not passed (§7.2).
        let handed = cand.hand_out
            && (split.is_some()
                || cand.own.is_some()
                || cards.len() != cand.seen.len()
                || cards.iter().any(|c| !cand.seen.contains(c)));
        let own_note = match (&cand.own, own_kept) {
            (Some(f), true) => Some(own_kept_note(l, f, &cards, split.as_ref().map(|(s, _)| s))),
            (Some(f), false) if handed => f.start_note(),
            _ => None,
        };
        let dropped = handed.then(|| dropped_note(cand, cards.len())).flatten();
        let lmgw_split = split
            .as_ref()
            .and_then(|(sp, t)| t.as_ref().map(|t| split_note(l, sp, t)));
        let note = [lmgw_split, own_note, dropped]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let note = (!note.is_empty()).then(|| note.join(". "));
        let engine = cards
            .iter()
            .map(|c| self.engine_on(target.class, c))
            .fold(0, u64::saturating_add);
        let kinds = split.as_ref().and_then(|(sp, _)| {
            let cost = whole_by_layer(fp)?;
            let parts = cost.parts_on(&sp.counts, &sp.bytes, sp.output)?;
            Some(split_kinds(l, sp, &parts))
        });
        Placement {
            engine,
            handed,
            note,
            kinds,
            split,
            own_kept: own_kept && handed,
            headroom: rules.headroom,
            margin: rules.margin,
            floor: rules.floor,
            structural: rules.structural,
            cards,
            placed,
        }
    }

    /// What a start of `target` with footprint `fp` keeps free on `cards`
    /// together (§5.6, WP13): each card's [`Rules`] by [`Self::rules_on`],
    /// summed, its margin on each — what admission reads of a placement —
    /// or a fitted llama-server row's own `--fit-target` where that is
    /// more. No cards (no device read names one): the pooled sums of every
    /// card, as before lmgw placed. A ladder climb reads its new rung's
    /// cards so too (owner, 2026-11-18).
    pub(in crate::vram) fn rules_over(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        cards: &[DeviceId],
    ) -> Rules {
        if cards.is_empty() {
            return Rules {
                headroom: self.start_headroom(snap, target.class, &target.model_id, fp),
                margin: self.start_margin(snap, target.class, &target.model_id, fp),
                floor: self.start_floor(snap, fp),
                structural: self.card_headroom(snap, fp),
            };
        }
        let heads = self.cards_headroom(snap);
        let (mut headroom, mut margin, mut floor, mut structural) = (0u64, 0u64, 0u64, 0u64);
        for c in cards {
            let r = self.rules_on(snap, target, fp, &heads, c);
            headroom = headroom.saturating_add(r.headroom);
            margin = margin.saturating_add(r.margin);
            floor = floor.saturating_add(r.floor);
            structural = structural.saturating_add(r.structural);
        }
        let own = fp.llama.as_ref().and_then(|l| l.sizer.fit_target);
        let at_least = |b: u64| own.map_or(b, |t| t.max(b));
        Rules {
            headroom: at_least(headroom),
            margin: at_least(margin),
            floor: at_least(floor),
            structural: at_least(structural),
        }
    }

    /// `ids` as [`Card`]s for `place`, as ledger `l` reads them; `moves`:
    /// with where each idle model on them could be started again were it
    /// stopped for room there (§6.2, [`Holder::elsewhere`]).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::vram) fn cards_for_place(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
        ids: &[DeviceId],
        own: Option<u64>,
        moves: bool,
    ) -> Vec<Card> {
        let heads = self.cards_headroom(snap);
        let ownt = fp.llama.as_ref().and_then(|l| l.sizer.fit_target);
        let at_least = |b: u64| ownt.map_or(b, |t| t.max(b));
        // A row that adapts is fitted smaller before it gives its margin
        // up (`partial::decide`), so `place` keeps its margin on every card
        // (`Card::last`).
        let adapts = fp.llama.is_some() || fp.partial.is_some();
        l.devices
            .iter()
            .filter(|d| ids.contains(&d.id))
            .filter_map(|d| {
                let cap = l.capacity_on(std::slice::from_ref(&d.id), own)?;
                let r = self.rules_on(snap, target, fp, &heads, &d.id);
                let holders = l
                    .residents
                    .iter()
                    .filter(|r| r.state == RuntimeState::Ready.as_str())
                    .filter(|r| !(r.container == target.class && r.model == target.model_id))
                    .filter_map(|r| {
                        let bytes = Ledger::held_on(r, &d.id);
                        let generation = r.generation?;
                        let e = l.entries.iter().find(|e| e.generation == generation)?;
                        let idle = r.in_flight == 0;
                        (bytes > 0).then(|| Holder {
                            class: r.container,
                            model: r.model.clone(),
                            generation,
                            bytes,
                            idle,
                            guest: e.owner == Origin::Background,
                            idle_seconds: r.idle_seconds.unwrap_or(0),
                            elsewhere: match moves && idle {
                                true => self.elsewhere(snap, l, e, std::slice::from_ref(&d.id)),
                                false => Vec::new(),
                            },
                        })
                    })
                    .collect();
                Some(Card {
                    id: d.id.clone(),
                    index: d.index,
                    total: cap.total,
                    free: cap.free,
                    headroom: at_least(r.headroom),
                    margin: at_least(r.margin),
                    last: at_least(match adapts {
                        true => r.margin,
                        false => r.floor,
                    }),
                    structural: at_least(r.structural),
                    engine: self.engine_on(target.class, &d.id),
                    holders,
                    pool: l.budget_left(own),
                })
            })
            .collect()
    }
}

/// What a split start's warnings say (§4.3, §7.2, §10): its cards by name
/// and index, the layers and bytes on each, what llama-server is told, and
/// that each card's part carries a split's larger compute buffers, as WP0
/// measured them, until the model has loaded and is charged by measurement.
fn split_note(l: &Ledger, sp: &super::Split, t: &crate::runtime::argv::TensorSplit) -> String {
    let last = sp.cards.len().saturating_sub(1);
    let parts: Vec<String> = sp
        .cards
        .iter()
        .enumerate()
        .map(|(j, c)| {
            format!(
                "{} layers{} on {} ({})",
                sp.counts[j],
                if j == last {
                    " and the output layer"
                } else {
                    ""
                },
                super::card_label(&l.devices, c),
                crate::hf::fmt_bytes(sp.bytes[j].saturating_add(sp.engine[j]))
            )
        })
        .collect();
    format!(
        "No card holds it whole: lmgw splits it over {} cards, the fewest that hold it — {}{}; \
         llama-server is told {}. Each card's part counts the larger compute buffers llama.cpp \
         reserves for a split, by factors measured on one model: each card is charged that \
         estimate until the model has loaded, then what its process holds there",
        sp.cards.len(),
        parts.join(", "),
        if t.draft_first {
            ", its draft model on the first card"
        } else {
            ""
        },
        crate::runtime::argv::LlamaPlacement {
            split: Some(*t),
            ..Default::default()
        }
        .flags()
    )
}

/// A split's parts on each card by kind (owner, 2026-11-18), in MiB as
/// llama.cpp's buffer lines give them, so a live check compares the two
/// directly: per card its layers, weights, KV cache, recurrent state,
/// compute buffer (the projector's own apart), the first card's own parts
/// (projector weights, a draft model, the expert cache), the output layer
/// and the engine's cost.
fn split_kinds(
    l: &Ledger,
    sp: &super::Split,
    parts: &[crate::vram::plan::chat::by_layer::CardKinds],
) -> String {
    let mib = |b: u64| format!("{:.2}", b as f64 / MIB as f64);
    sp.cards
        .iter()
        .zip(parts)
        .enumerate()
        .map(|(j, (c, k))| {
            let mut s = format!(
                "{}: {} layers, weights {}, KV {}, recurrent state {}, compute {}",
                super::card_label(&l.devices, c),
                k.layers,
                mib(k.weights),
                mib(k.kv),
                mib(k.states),
                mib(k.compute)
            );
            if k.projector_compute > 0 {
                s.push_str(&format!(", projector compute {}", mib(k.projector_compute)));
            }
            if k.first > 0 {
                s.push_str(&format!(
                    ", projector weights and other first-card parts {}",
                    mib(k.first)
                ));
            }
            if k.output > 0 {
                s.push_str(&format!(", output layer {}", mib(k.output)));
            }
            let engine = sp.engine.get(j).copied().unwrap_or(0);
            let extra = super::split::SPLIT_ENGINE_EXTRA.min(engine);
            let total = sp.bytes.get(j).copied().unwrap_or(0).saturating_add(engine);
            s.push_str(&format!(
                ", engine {}, split engine extra {}; {} MiB in all",
                mib(engine - extra),
                mib(extra),
                mib(total)
            ));
            s
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a start with the row's own split kept on its pin says (§7.2,
/// ruling 11): its cards by name and index, and, for a split by layers, the
/// layers and bytes on each; the split as written; and that what a split
/// adds to the compute buffers is estimated and then charged by measurement.
fn own_kept_note(
    l: &Ledger,
    f: &super::OwnFate,
    cards: &[DeviceId],
    split: Option<&super::Split>,
) -> String {
    let lead = format!(
        "Its own --tensor-split {} is kept: lmgw starts it on the {} cards of its pin with its \
         arguments as written",
        f.split.written,
        cards.len()
    );
    let Some(sp) = split else {
        let names: Vec<String> = cards
            .iter()
            .map(|c| super::card_label(&l.devices, c))
            .collect();
        return format!(
            "{lead}, {}. lmgw cannot say beforehand what each card holds of this split: they are \
             charged lmgw's estimate together until the model has loaded, then what its process \
             holds on each",
            super::in_words(&names)
        );
    };
    let parts: Vec<String> = sp
        .cards
        .iter()
        .enumerate()
        .map(|(j, c)| {
            format!(
                "{} layers{} on {} ({})",
                sp.counts[j],
                if j == sp.output {
                    " and the output layer"
                } else {
                    ""
                },
                super::card_label(&l.devices, c),
                crate::hf::fmt_bytes(sp.bytes[j].saturating_add(sp.engine[j]))
            )
        })
        .collect();
    format!(
        "{lead} — {}. Each card's part counts the larger compute buffers llama.cpp reserves \
         for a split, by factors measured on one model: each card is charged that estimate \
         until the model has loaded, then what its process holds there",
        parts.join(", ")
    )
}

/// What the start's warnings say where lmgw hands the engine `n` cards
/// (§8.1): the row's own `CUDA_VISIBLE_DEVICES` and llama-server's own
/// flags that number a card were not passed. `None` where the row has
/// neither.
fn dropped_note(cand: &Candidates, n: usize) -> Option<String> {
    let mut gone: Vec<String> = Vec::new();
    if let Some(v) = &cand.cuda_visible {
        gone.push(format!("its run arguments' CUDA_VISIBLE_DEVICES={v}"));
    }
    // Beside the row's own split kept on its pin they are passed as
    // written (§8.1, ruling 11).
    if !cand.engine_cards.is_empty() && cand.own_kept().is_none() {
        gone.push(format!(
            "its llama-server arguments' {}",
            cand.engine_cards.join(" ")
        ));
    }
    if gone.is_empty() {
        return None;
    }
    let mut text = super::notice::in_words(&gone);
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    Some(format!(
        "{text} {} not passed: lmgw starts it on the {} it placed it on and numbers them for \
         it; to choose its card, set its GPU pin",
        if gone.len() == 1 { "was" } else { "were" },
        if n == 1 { "card" } else { "cards" }
    ))
}

impl VramScheduler {
    /// Why `target` does not start where every card of its pin is gone
    /// from this machine (§8): it does not go to another card, and its
    /// fallback answers where it has one. `None` where it has no pin, or a
    /// card of it is here, or no device read named a card.
    pub(in crate::vram) fn pin_gone(
        &self,
        snap: &Snapshot,
        target: &Target,
        devices: &[super::super::GpuMemory],
    ) -> Option<String> {
        let cand = self.candidates(snap, target, devices);
        if cand.missing.is_empty() || !cand.cards.is_empty() {
            return None;
        }
        let ids: Vec<&str> = cand.missing.iter().map(DeviceId::as_str).collect();
        Some(format!(
            "is pinned to {} this computer does not list ({}): it does not start on another \
             card; set its GPU pin to a card this computer has, or clear it",
            if ids.len() == 1 {
                "a graphics card"
            } else {
                "graphics cards"
            },
            ids.join(", ")
        ))
    }
}

/// Whether `target` with footprint `fp` is larger than its candidates
/// could hold with nothing else of lmgw's on them (§5.8), and if so the
/// figures for the refusal: what it needs beside it there and the capacity
/// it is judged against. A chat model that may go over its candidates as a
/// group is judged against their sum; any other model, which goes on one
/// card (ruling 12), against each candidate, and the largest is named; a
/// model that cannot be handed a card against the cards it sees together,
/// as before lmgw placed. One that adapts is never too large.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct TooLarge {
    /// What it needs there without what it keeps free beside it: its
    /// footprint, or for a row's own split the part its split puts on the
    /// card that cannot hold it (§7.2).
    pub bytes: u64,
    pub structural: u64,
    pub capacity: u64,
    /// What holds `capacity`, with its verb, for the refusal (device-ledger
    /// design §10): "the GPU holds" on one card, else the card or cards
    /// it was judged against by name and index.
    pub holder: String,
}

impl VramScheduler {
    /// [`TooLarge`] for `target` on ledger `l`; `None` where it fits, adapts
    /// or there is no capacity.
    pub(in crate::vram) fn too_large(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
    ) -> Option<TooLarge> {
        if super::super::partial::adapts(fp) {
            return None;
        }
        let cand = self.candidates(snap, target, &l.devices);
        let group = !cand.hand_out || l.devices.is_empty();
        // A row's own split kept on its pin (§7.2): each card of its pin
        // against what the split puts there, or the cards together where
        // lmgw cannot say what each holds of it.
        if let (Some(own_split), false) = (cand.own_kept(), group) {
            return self.own_too_large(snap, target, fp, l, &cand, own_split);
        }
        if group {
            let p = self.placement_on(
                snap,
                target,
                fp,
                l,
                &cand,
                Placed::Group(cand.cards.clone()),
            );
            let cap = l.capacity_on(&p.cards, None)?;
            let needs = fp
                .total_bytes
                .saturating_add(p.engine)
                .saturating_add(p.structural);
            return (needs > cap.total).then(|| TooLarge {
                bytes: fp.total_bytes,
                structural: p.structural.saturating_add(p.engine),
                capacity: cap.total,
                holder: match p.cards.len() {
                    _ if l.devices.len() < 2 => "the GPU holds".to_string(),
                    1 => format!("{} holds", super::card_label(&l.devices, &p.cards[0])),
                    n => format!("the {n} cards its engine sees hold"),
                },
            });
        }
        let mut largest: Option<TooLarge> = None;
        let mut empty: Vec<Card> = Vec::new();
        for c in &cand.cards {
            let p = self.placement_on(snap, target, fp, l, &cand, Placed::Now(c.clone()));
            let cap = l.capacity_on(&p.cards, None)?;
            let structural = p.structural.saturating_add(p.engine);
            if fp.total_bytes.saturating_add(structural) <= cap.total {
                return None;
            }
            if largest.as_ref().is_none_or(|t| cap.total > t.capacity) {
                largest = Some(TooLarge {
                    bytes: fp.total_bytes,
                    structural,
                    capacity: cap.total,
                    holder: match cand.cards.len() {
                        _ if l.devices.len() < 2 => "the GPU holds".to_string(),
                        1 => format!("{} holds", super::card_label(&l.devices, c)),
                        _ => format!(
                            "{}, the largest card it may use, holds",
                            super::card_label(&l.devices, c)
                        ),
                    },
                });
            }
            let index = l.devices.iter().find(|d| &d.id == c).map_or(0, |d| d.index);
            empty.push(Card {
                id: c.clone(),
                index,
                total: cap.total,
                free: cap.total,
                headroom: p.structural,
                margin: p.structural,
                last: p.structural,
                structural: p.structural,
                engine: p.engine,
                holders: Vec::new(),
                pool: l.budget_total(),
            });
        }
        // A chat model lmgw may split: too large where no split over its
        // candidates holds it with nothing else of lmgw's on them (§5.8),
        // judged against their sum — and under a declared budget its parts
        // together against the budget (owner, 2026-11-18).
        if let Ok(cost) = self.splits(snap, target, fp) {
            let rooms: Vec<(&Card, u64)> = empty
                .iter()
                .map(|c| {
                    (
                        c,
                        c.total
                            .saturating_sub(c.structural)
                            .saturating_sub(c.engine),
                    )
                })
                .collect();
            if super::split::fewest(&cost, &rooms).is_some() {
                return None;
            }
            // The cards hold it, but not within the budget: say so.
            if let Some(budget) = l.budget_total() {
                let free: Vec<Card> = empty
                    .iter()
                    .map(|c| Card {
                        pool: None,
                        ..c.clone()
                    })
                    .collect();
                let rooms: Vec<(&Card, u64)> = free
                    .iter()
                    .map(|c| {
                        (
                            c,
                            c.total
                                .saturating_sub(c.structural)
                                .saturating_sub(c.engine),
                        )
                    })
                    .collect();
                if super::split::fewest(&cost, &rooms).is_some() {
                    return Some(TooLarge {
                        bytes: fp.total_bytes,
                        structural: empty
                            .iter()
                            .map(|c| c.engine.saturating_add(super::split::SPLIT_ENGINE_EXTRA))
                            .fold(0, u64::saturating_add),
                        capacity: budget,
                        holder: "the GPU memory budget, which a split's parts on its cards share \
                                 together with their engine's cost, holds"
                            .to_string(),
                    });
                }
            }
            return Some(TooLarge {
                bytes: fp.total_bytes,
                structural: empty
                    .iter()
                    .map(|c| c.structural.saturating_add(c.engine))
                    .fold(0, u64::saturating_add),
                capacity: empty.iter().map(|c| c.total).fold(0, u64::saturating_add),
                holder: match empty.len() {
                    _ if l.devices.len() < 2 => "the GPU holds".to_string(),
                    n => format!("the {n} cards it may use hold"),
                },
            });
        }
        largest
    }

    /// [`Self::too_large`] for a row's own split kept on its pin (§7.2):
    /// the first card of its pin that cannot hold what the split puts
    /// there with nothing else of lmgw's on it (§5.8); where lmgw cannot
    /// say what each card holds of it, the pin's cards together.
    fn own_too_large(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
        l: &Ledger,
        cand: &Candidates,
        own_split: &super::OwnFate,
    ) -> Option<TooLarge> {
        let mut empty: Vec<Card> = Vec::new();
        for c in &cand.cards {
            let p = self.placement_on(snap, target, fp, l, cand, Placed::Now(c.clone()));
            let cap = l.capacity_on(&p.cards, None)?;
            let index = l.devices.iter().find(|d| &d.id == c).map_or(0, |d| d.index);
            empty.push(Card {
                id: c.clone(),
                index,
                total: cap.total,
                free: cap.total,
                headroom: p.structural,
                margin: p.structural,
                last: p.structural,
                structural: p.structural,
                engine: p.engine,
                holders: Vec::new(),
                pool: None,
            });
        }
        let Some(sp) = own_split.over(whole_by_layer(fp).as_deref(), &empty) else {
            let keep = empty
                .iter()
                .map(|c| c.structural.saturating_add(c.engine))
                .fold(0, u64::saturating_add);
            let total = empty.iter().map(|c| c.total).fold(0, u64::saturating_add);
            return (fp.total_bytes.saturating_add(keep) > total).then(|| TooLarge {
                bytes: fp.total_bytes,
                structural: keep,
                capacity: total,
                holder: format!("the {} cards of its pin hold", empty.len()),
            });
        };
        // Each card its part, its engine as a split card's (`sp.engine`,
        // `split::SPLIT_ENGINE_EXTRA` included) …
        let card_short = empty.iter().enumerate().find_map(|(j, c)| {
            let keep = c.structural.saturating_add(sp.engine[j]);
            (sp.bytes[j].saturating_add(keep) > c.total).then(|| TooLarge {
                bytes: sp.bytes[j],
                structural: keep,
                capacity: c.total,
                holder: format!(
                    "its own --tensor-split puts that on {}, which holds",
                    super::card_label(&l.devices, &c.id)
                ),
            })
        });
        // … and under a declared budget the parts together, the engine's
        // cost included, within the budget (owner, 2026-11-18), as an
        // lmgw split's (`split::over`).
        card_short.or_else(|| {
            let budget = l.budget_total()?;
            let engines = sp.engine.iter().fold(0u64, |a, e| a.saturating_add(*e));
            (sp.total().saturating_add(engines) > budget).then(|| TooLarge {
                bytes: sp.total(),
                structural: engines,
                capacity: budget,
                holder: "the GPU memory budget, which its own --tensor-split's parts on its \
                         cards share together with their engine's cost, holds"
                    .to_string(),
            })
        })
    }

    /// `fp` fitted to the cards of `p` as ledger `l` reads them, where
    /// llama.cpp's `--fit` would shrink it (`partial::llama`): their total
    /// and what they can give this start — what is free there and what
    /// lmgw's other models there are charged, which a wait or an eviction
    /// frees — less the engine's cost there, with the count of cards the
    /// engine sees from the placement (D14). `own`: the start's claim.
    pub(in crate::vram) fn fit_in_on(
        &self,
        target: &Target,
        fp: Footprint,
        l: &Ledger,
        p: &Placement,
        own: Option<u64>,
    ) -> Footprint {
        // A split is the row as written over its cards (§4.3): every card
        // reserves a compute buffer of its own.
        if let Some((sp, _)) = &p.split {
            // From the row as written, where a preview fitted it to one card.
            let fp = match &fp.llama {
                Some(l) if !l.target.is_whole() => {
                    let mut whole = l
                        .sizer
                        .footprint(&crate::runtime::registry::LlamaPlacement::default());
                    whole.llama = Some(super::super::plan::chat::fit::LlamaFit {
                        target: Default::default(),
                        ..l.clone()
                    });
                    whole
                }
                _ => fp,
            };
            let more = sp.total().saturating_sub(fp.total_bytes);
            return Footprint {
                compute_bytes: fp.compute_bytes.saturating_add(more),
                total_bytes: sp.total(),
                ..fp
            };
        }
        if fp.llama.is_none() {
            return fp;
        }
        let Some(cap) = l.capacity_on(&p.cards, own) else {
            return fp;
        };
        let avail = cap
            .free
            .saturating_add(freeable_on(l, target, &p.cards))
            .min(cap.total);
        let card = super::super::partial::llama::Card {
            total: cap.total.saturating_sub(p.engine),
            avail: avail.saturating_sub(p.engine),
            margin: p.margin,
            structural: p.structural,
        };
        super::super::partial::llama::fit_to_card(fp, card, p.count())
    }
}

impl VramScheduler {
    /// The card a fit that reads no ledger takes for `target` with
    /// footprint `fp` (device-ledger design §5.3, D14): on a machine of
    /// several cards that can be handed out, the candidate with the most
    /// room for lmgw — its total less what other programs held there at the
    /// newest reading — as the devices the last ledger read listed it, with
    /// what the start keeps free and costs there. `None` on one card, where
    /// the pooled preview is that card's, and where lmgw cannot hand out a
    /// card (the engine sees the cards its arguments pass).
    pub(in crate::vram) fn roomiest_now(
        &self,
        snap: &Snapshot,
        target: &Target,
        fp: &Footprint,
    ) -> Option<(super::super::GpuMemory, super::super::partial::llama::Card)> {
        if snap.settings.vram.budget_mb > 0 {
            return None;
        }
        let devices = self.last_devices.lock().unwrap().clone();
        if devices.len() < 2 {
            return None;
        }
        let cand = self.candidates(snap, target, &devices);
        if !cand.hand_out {
            return None;
        }
        let heads = self.cards_headroom(snap);
        let recent = |id: &DeviceId| {
            heads
                .iter()
                .find(|h| h.id.as_ref() == Some(id))
                .and_then(|h| h.recent)
                .unwrap_or(0)
        };
        let card = devices
            .iter()
            .filter(|d| cand.cards.contains(&d.id))
            .max_by_key(|d| {
                (
                    d.total_bytes.saturating_sub(recent(&d.id)),
                    std::cmp::Reverse(d.index),
                )
            })?;
        let r = self.rules_on(snap, target, fp, &heads, &card.id);
        let own = fp.llama.as_ref().and_then(|l| l.sizer.fit_target);
        let at_least = |b: u64| own.map_or(b, |t| t.max(b));
        let engine = self.engine_on(target.class, &card.id);
        Some((
            card.clone(),
            super::super::partial::llama::Card {
                total: card.total_bytes.saturating_sub(engine),
                avail: card
                    .total_bytes
                    .saturating_sub(recent(&card.id))
                    .saturating_sub(engine),
                margin: at_least(r.margin),
                structural: at_least(r.structural),
            },
        ))
    }
}

/// What lmgw's own models other than `target` on `cards` are charged there,
/// which a wait or an eviction frees (`queue::freeable` on those cards):
/// on one card what each holds there at most ([`Ledger::held_on`]); on
/// several, each model's whole charge once. A reservation is no victim.
pub(in crate::vram) fn freeable_on(l: &Ledger, target: &Target, cards: &[DeviceId]) -> u64 {
    l.residents
        .iter()
        .filter(|r| !(r.container == target.class && r.model == target.model_id))
        .filter(|r| r.state != "reserved")
        .filter(|r| Ledger::is_on(r, cards))
        .map(|r| match cards {
            [one] => Ledger::held_on(r, one),
            _ => r
                .estimated_bytes
                .saturating_add(r.peak_extra_bytes.unwrap_or(0))
                .saturating_add(r.kept_bytes),
        })
        .fold(0u64, u64::saturating_add)
}

/// Something of lmgw's other than `target` is on `cards` or reserved for
/// them: a wait could free it (`partial::Room::others_hold`, per card).
pub(in crate::vram) fn others_hold_on(l: &Ledger, target: &Target, cards: &[DeviceId]) -> bool {
    l.residents
        .iter()
        .filter(|r| !(r.container == target.class && r.model == target.model_id))
        .any(|r| Ledger::is_on(r, cards))
}

/// The generations a claim on `p`'s cards waits for (§5.5): the victims or
/// busy models `place` named, else every model of lmgw's on those cards.
pub(in crate::vram) fn waits_on(l: &Ledger, target: &Target, p: &Placement) -> Vec<u64> {
    match &p.placed {
        Placed::Evict { victims, .. } => victims.iter().map(|h| h.generation).collect(),
        Placed::Split { victims, .. } if !victims.is_empty() => {
            victims.iter().map(|h| h.generation).collect()
        }
        Placed::Wait { on, .. } => on.iter().map(|h| h.generation).collect(),
        _ => l
            .residents
            .iter()
            .filter(|r| !(r.container == target.class && r.model == target.model_id))
            .filter(|r| r.state != "reserved" && Ledger::is_on(r, &p.cards))
            .filter_map(|r| r.generation)
            .collect(),
    }
}

/// The cards a row's run arguments name in forms `charge::seen` does not
/// read (§8.1): `-e NVIDIA_VISIBLE_DEVICES=<n or UUID,…>` and `--device
/// /dev/nvidia<n>`, each a card of `devices` by index or id. `None` where
/// they name none (`all`, `void`, `none` and an empty value name none); a
/// name that is no card of `devices` is left out.
pub(in crate::vram) fn env_pin(
    run_args: &[String],
    devices: &[super::super::GpuMemory],
) -> Option<Vec<DeviceId>> {
    let mut names: Vec<String> = Vec::new();
    if let Some(list) = env_value(run_args, "NVIDIA_VISIBLE_DEVICES") {
        if !matches!(list.trim(), "" | "all" | "void" | "none") {
            names.extend(list.split(',').map(|n| n.trim().to_string()));
        }
    }
    for (i, a) in run_args.iter().enumerate() {
        let value = match a.split_once('=') {
            Some(("--device", v)) => Some(v.to_string()),
            _ if a == "--device" => run_args.get(i + 1).cloned(),
            _ => None,
        };
        if let Some(v) = value {
            let node = v.split(':').next().unwrap_or(&v).to_string();
            if let Some(n) = node.strip_prefix("/dev/nvidia") {
                if n.parse::<u32>().is_ok() {
                    names.push(n.to_string());
                }
            }
        }
    }
    if names.is_empty() {
        return None;
    }
    Some(
        devices
            .iter()
            .filter(|d| {
                names
                    .iter()
                    .any(|n| n == d.id.as_str() || n.parse::<u32>().is_ok_and(|i| i == d.index))
            })
            .map(|d| d.id.clone())
            .collect(),
    )
}

/// The value `run_args` set variable `name` to (`-e NAME=V`, `--env
/// NAME=V`, `--env=NAME=V`), the last where several do.
fn env_value(run_args: &[String], name: &str) -> Option<String> {
    let mut out = None;
    for (i, a) in run_args.iter().enumerate() {
        let set = match a.as_str() {
            "-e" | "--env" => run_args.get(i + 1).cloned(),
            _ => a.strip_prefix("--env=").map(str::to_string),
        };
        if let Some(v) = set.and_then(|s| s.strip_prefix(&format!("{name}=")).map(str::to_string)) {
            out = Some(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn card(i: u32) -> super::super::super::GpuMemory {
        super::super::super::GpuMemory {
            id: DeviceId::new(format!("GPU-{i}")),
            index: i,
            name: "card".into(),
            total_bytes: 0,
            used_bytes: 0,
            free_bytes: 0,
        }
    }

    /// The forms that name a card besides CDI's (§8.1): the variable by
    /// index or UUID, an NVIDIA device node; `all` and `void` name none.
    #[test]
    fn env_and_node_forms_are_read_as_a_pin() {
        let four: Vec<_> = (0..4).map(card).collect();
        let id = |i: u32| DeviceId::new(format!("GPU-{i}"));
        assert_eq!(
            env_pin(&args(&["-e", "NVIDIA_VISIBLE_DEVICES=2"]), &four),
            Some(vec![id(2)])
        );
        assert_eq!(
            env_pin(&args(&["--env=NVIDIA_VISIBLE_DEVICES=GPU-3,1"]), &four),
            Some(vec![id(1), id(3)])
        );
        assert_eq!(
            env_pin(&args(&["--device", "/dev/nvidia0"]), &four),
            Some(vec![id(0)])
        );
        assert_eq!(
            env_pin(&args(&["-e", "NVIDIA_VISIBLE_DEVICES=all"]), &four),
            None
        );
        assert_eq!(
            env_pin(&args(&["-e", "NVIDIA_VISIBLE_DEVICES=void"]), &four),
            None
        );
        assert_eq!(env_pin(&args(&["--device", "/dev/nvidiactl"]), &four), None);
        assert_eq!(
            env_value(
                &args(&["-e", "CUDA_VISIBLE_DEVICES=1"]),
                "CUDA_VISIBLE_DEVICES"
            ),
            Some("1".into())
        );
    }
}
