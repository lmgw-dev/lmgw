//! A row's own split (device-ledger design §7.2, rulings 11 and 16, WP11).
//!
//! A chat row that sets `--tensor-split` itself is held to it as written,
//! as a row that sets its context is: pinned to as many cards as its split
//! has entries, every one of them listed, it is placed on those cards, the
//! entries mapped to them in the bus's order, and each card is charged
//! what the row's split puts there ([`ByLayer::by_entries`]) in place of
//! §4.3's filling. It is handed those cards by UUID like any handed start
//! and started with its arguments as written, `--split-mode` and
//! `--main-gpu` beside the split included ([`OwnFate::kept`]).
//!
//! **Not kept** — no pin, a pin of another size, a pinned card this
//! computer does not list, a value lmgw cannot read — the row is placed by
//! §5 like any row (lmgw may split it itself where it sets its context) and
//! started without its split and without `--split-mode` and `--main-gpu`
//! beside it; the start's warnings say so, and so does the line under its
//! command line. An aux row's own split is never kept (ruling 16): aux
//! models run on one card.
//!
//! Only where lmgw hands the engine its cards (§7.1, D18) on a machine of
//! several cards: on one card, and for an engine lmgw cannot hand a card,
//! the arguments are used as written (D17), the split with them.
//!
//! **What each card holds** is known for a split by layers (the default
//! `--split-mode layer`) of a model whose header says what each layer
//! holds. Any other — `--split-mode row` or `tensor`, entries that are all
//! 0 (llama.cpp then splits by free memory), no tensor list — is charged
//! as a group on its cards (§5.7) until it has loaded, then what its
//! process holds on each.
//!
//! **It is never moved** (§6.2): its pin's cards are all its own, so no
//! other card of its pin is left to move it to, and one card would not
//! hold its split.

use crate::runtime::argv::{card_flags, own_split_of, OwnSplit, MAX_SPLIT};
use crate::runtime::Class;

use super::super::nvml::DeviceId;
use super::super::plan::chat::by_layer::ByLayer;
use super::{Card, Holder, Mode, Placed, Split};

/// Why a row's own split is not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// An aux row: aux models run on one card (ruling 16).
    Aux,
    /// Its value is not one lmgw can read.
    Unread,
    /// It has no pin.
    NoPin,
    /// Its pin is of this many cards.
    PinOf(usize),
    /// These cards of its pin this computer does not list.
    Missing(Vec<DeviceId>),
}

/// What becomes of a row's own `--tensor-split` (module doc).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OwnFate {
    pub split: OwnSplit,
    /// The split and what goes with it, as written: what is not passed
    /// where it is not kept.
    pub flags: Vec<String>,
    /// Why it is not kept; `None`: kept.
    pub dropped: Option<Dropped>,
}

/// The fate of the own split in `args` of a `class` row (module doc).
/// `pin`: its pin's cards as written (the GPU pin field, else its run
/// arguments, §8.1) and those this computer does not list; `None`
/// unpinned. `None` where it writes no split, and for a class with none.
pub(crate) fn fate(
    class: Class,
    args: &[String],
    pin: Option<(usize, &[DeviceId])>,
) -> Option<OwnFate> {
    let split = own_split_of(args)?;
    let flags = card_flags(args).split;
    let dropped = match (class, split.entries.as_ref(), pin) {
        (Class::Aux, _, _) => Some(Dropped::Aux),
        (Class::Chat, None, _) => Some(Dropped::Unread),
        (Class::Chat, Some(_), None) => Some(Dropped::NoPin),
        (Class::Chat, Some(e), Some((n, _))) if n != e.len() => Some(Dropped::PinOf(n)),
        (Class::Chat, Some(_), Some((_, missing))) if !missing.is_empty() => {
            Some(Dropped::Missing(missing.to_vec()))
        }
        (Class::Chat, Some(_), Some(_)) => None,
        _ => return None,
    };
    Some(OwnFate {
        split,
        flags,
        dropped,
    })
}

impl OwnFate {
    /// Kept: started with its split as written on its pin's cards.
    pub(crate) fn kept(&self) -> bool {
        self.dropped.is_none()
    }

    /// How many cards its split names.
    fn wants(&self) -> usize {
        self.split.entries.as_ref().map_or(0, Vec::len)
    }

    /// The flags not passed, as the start's warning names them: "Its
    /// llama-server arguments' --tensor-split 1,1 --main-gpu 1".
    fn flags_words(&self) -> (String, &'static str) {
        let groups = self
            .flags
            .iter()
            .filter(|t| crate::runtime::argv::is_opt(t))
            .count();
        (
            format!("Its llama-server arguments' {}", self.flags.join(" ")),
            if groups == 1 { "was" } else { "were" },
        )
    }

    /// The start's warning where the split is not kept (module doc);
    /// `None` where it is.
    pub(crate) fn start_note(&self) -> Option<String> {
        let why = self.dropped.as_ref()?;
        let (flags, was) = self.flags_words();
        let n = self.wants();
        let any = "lmgw placed it as it places any model";
        let keep = format!("to keep the split, pin it to {n} cards");
        Some(match why {
            Dropped::Aux => format!("{flags} {was} not passed: aux models run on one card"),
            Dropped::Unread => format!(
                "{flags} {was} not passed: lmgw cannot read it as a split (numbers separated by \
                 commas, at most {MAX_SPLIT} of them), so {any}"
            ),
            Dropped::NoPin => format!(
                "{flags} {was} not passed: a split of its own needs a GPU pin of {n} cards, and it \
                 has none, so {any}; {keep}"
            ),
            Dropped::PinOf(m) => format!(
                "{flags} {was} not passed: a split of its own needs a GPU pin of {n} cards, and it \
                 is pinned to {m}, so {any}; {keep}"
            ),
            Dropped::Missing(gone) => format!(
                "{flags} {was} not passed: a split of its own needs every card of its pin, and \
                 this computer does not list {}, so {any}",
                names(gone)
            ),
        })
    }

    /// The line under the row's command line (§7.2: "the editor says the
    /// split needs a pin of that many cards").
    pub(crate) fn editor_note(&self) -> String {
        let n = self.wants();
        let written = &self.split.written;
        let without = format!("starts it without {}", self.flags.join(" "));
        let any = "and places it as it places any model";
        match &self.dropped {
            None => format!(
                "Its own --tensor-split {written} is kept: lmgw starts it on the {n} cards of its \
                 pin, the entries in the cards' order, and charges each card its share."
            ),
            Some(Dropped::Aux) => format!("Aux models run on one card: lmgw {without}."),
            Some(Dropped::Unread) => format!(
                "lmgw cannot read its own --tensor-split {written} as a split (numbers separated \
                 by commas, at most {MAX_SPLIT} of them): lmgw {without} {any}."
            ),
            Some(Dropped::NoPin) => format!(
                "Its own --tensor-split {written} needs a GPU pin of {n} cards, and it has none: \
                 lmgw {without} {any}. To keep the split, pin it to {n} cards."
            ),
            Some(Dropped::PinOf(m)) => format!(
                "Its own --tensor-split {written} needs a GPU pin of {n} cards, and it is pinned \
                 to {m}: lmgw {without} {any}. To keep the split, pin it to {n} cards."
            ),
            Some(Dropped::Missing(gone)) => format!(
                "Its own --tensor-split {written} needs every card of its pin, and this computer \
                 does not list {}: lmgw {without} {any}.",
                names(gone)
            ),
        }
    }

    /// What the row's split puts on `cards` — its pin's cards as one ledger
    /// read finds them, in the bus's order — where lmgw can say what each
    /// card holds (module doc); `None` where it cannot.
    pub(in crate::vram) fn over(&self, cost: Option<&ByLayer>, cards: &[Card]) -> Option<Split> {
        let entries = self.split.entries.as_ref()?;
        if !self.kept() || !self.split.by_layer() || cards.len() != entries.len() {
            return None;
        }
        let cost = cost?;
        let (counts, bytes, output) = cost.by_entries(entries)?;
        Some(Split {
            cards: cards.iter().map(|c| c.id.clone()).collect(),
            counts,
            bytes,
            // What a split card's engine holds more (`split::SPLIT_ENGINE_EXTRA`).
            engine: cards
                .iter()
                .map(|c| c.engine.saturating_add(super::split::SPLIT_ENGINE_EXTRA))
                .collect(),
            headroom: cards.iter().map(|c| c.headroom).collect(),
            last: cards.iter().map(|c| c.last).collect(),
            drafter: cost.drafter,
            output,
        })
    }
}

/// The ids in words.
fn names(ids: &[DeviceId]) -> String {
    let ids: Vec<String> = ids.iter().map(|i| i.as_str().to_string()).collect();
    super::in_words(&ids)
}

/// A row's own split `split` on its cards (module doc), for `mode`: on
/// them as it is, once the idle models each card needs gone have been
/// stopped (within the mode, least recently used first). Where the idle
/// ones are not enough on a card, none are named: the start evicts or
/// waits on the card that is short (`split_short`), as any split's.
pub(in crate::vram) fn fixed(split: Split, cards: &[Card], mode: Mode) -> Placed {
    let card = |id: &DeviceId| cards.iter().find(|c| &c.id == id);
    let mut chosen: Vec<Holder> = Vec::new();
    let mut enough = true;
    for (j, id) in split.cards.iter().enumerate() {
        let Some(c) = card(id) else {
            enough = false;
            continue;
        };
        let order = c.freeable(mode, false);
        loop {
            let gone: Vec<&Holder> = c
                .holders
                .iter()
                .filter(|h| chosen.iter().any(|x| x.generation == h.generation))
                .collect();
            let freed = gone.iter().fold(0u64, |a, h| a.saturating_add(h.bytes));
            let keep = c.keep(c.holders.len() > gone.len(), super::Keep::Left);
            let needs = split.bytes[j]
                .saturating_add(split.engine[j])
                .saturating_add(keep);
            if c.free.saturating_add(freed) >= needs {
                break;
            }
            match order
                .iter()
                .find(|h| !chosen.iter().any(|x| x.generation == h.generation))
            {
                Some(next) => chosen.push((*next).clone()),
                None => {
                    enough = false;
                    break;
                }
            }
        }
    }
    Placed::Split {
        split,
        victims: match enough {
            true => chosen,
            false => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn id(i: u32) -> DeviceId {
        DeviceId::new(format!("GPU-{i}"))
    }

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    /// A card of 16 GiB with `free`, 1 GiB headroom, no margin, 640 MiB of
    /// engine.
    fn card(i: u32, free: u64, holders: Vec<Holder>) -> Card {
        Card {
            id: id(i),
            index: i,
            total: 16 * GIB,
            free,
            headroom: GIB,
            margin: 0,
            last: 0,
            structural: GIB,
            engine: 640 * MIB,
            holders,
            pool: None,
        }
    }

    fn holder(model: &str, bytes: u64, idle: bool) -> Holder {
        Holder {
            class: Class::Chat,
            model: model.into(),
            generation: bytes,
            bytes,
            idle,
            guest: false,
            idle_seconds: 10,
            elsewhere: Vec::new(),
        }
    }

    fn victims(p: &Placed) -> Vec<&str> {
        match p {
            Placed::Split { victims, .. } => victims.iter().map(|h| h.model.as_str()).collect(),
            other => panic!("not a split: {other:?}"),
        }
    }

    /// The verification of WP11 at the place (§7.2): `--tensor-split 1,1`
    /// on a pin of two cards puts the layers whose place is below half on
    /// the first card and the rest with the output layer on the second,
    /// and goes on them as it is; a card short by an idle model of lmgw's
    /// names it; one short by a busy model names none, so the start waits
    /// on that card.
    #[test]
    fn a_kept_split_goes_on_its_pin_as_written() {
        let cost = ByLayer {
            first_gpu: 0,
            layers: vec![GIB; 8],
            last: GIB / 2,
            first: 0,
            each: 100 * MIB,
            copy: 0,
            weights: Vec::new(),
            states: Vec::new(),
            drafter: false,
            pipeline: true,
            projector: false,
        };
        let f = fate(Class::Chat, &args(&["-ts", "1,1"]), Some((2, &[]))).unwrap();
        let free = || vec![card(0, 8 * GIB, vec![]), card(1, 8 * GIB, vec![])];
        let sp = f.over(Some(&cost), &free()).unwrap();
        assert_eq!(sp.cards, vec![id(0), id(1)]);
        assert_eq!(sp.counts, vec![5, 3]);
        assert_eq!(sp.output, 1);
        // A split's compute buffers (`by_layer`): 222 and 174 MiB for the
        // one-card 100.
        assert_eq!(
            sp.bytes,
            vec![5 * GIB + 222 * MIB, 3 * GIB + 174 * MIB + GIB / 2]
        );
        // Each card's engine cost and what a split card's engine holds more.
        assert_eq!(sp.engine, vec![(640 + 64) * MIB, (640 + 64) * MIB]);
        assert!(victims(&fixed(sp.clone(), &free(), Mode::Owner)).is_empty());

        let idle = vec![
            card(0, 4 * GIB, vec![holder("idle", 4 * GIB, true)]),
            card(1, 8 * GIB, vec![]),
        ];
        assert_eq!(
            victims(&fixed(sp.clone(), &idle, Mode::Owner)),
            vec!["idle"]
        );
        let busy = vec![
            card(0, 4 * GIB, vec![holder("busy", 4 * GIB, false)]),
            card(1, 8 * GIB, vec![]),
        ];
        assert!(victims(&fixed(sp, &busy, Mode::Owner)).is_empty());

        // Another split mode, a card count not its own, no layer figures:
        // lmgw cannot say what each card holds of it.
        let row = fate(
            Class::Chat,
            &args(&["-ts", "1,1", "-sm", "row"]),
            Some((2, &[])),
        );
        assert!(row.unwrap().over(Some(&cost), &free()).is_none());
        assert!(f.over(Some(&cost), &free()[..1]).is_none());
        assert!(f.over(None, &free()).is_none());
    }

    /// Ruling 11 and 16: a chat row's split is kept on a pin of as many
    /// cards as it has entries, all listed; any other pin, none, a value
    /// lmgw cannot read and an aux row's split are dropped, each with its
    /// reason; a row without one has no fate.
    #[test]
    fn the_fate_of_a_row_s_own_split() {
        let row = args(&["--tensor-split", "1,1", "--main-gpu", "1"]);
        let two = [id(0), id(1)];
        let f = fate(Class::Chat, &row, Some((2, &[]))).unwrap();
        assert!(f.kept());
        assert_eq!(f.flags, row);
        assert_eq!(f.start_note(), None);
        let f = fate(Class::Chat, &row, None).unwrap();
        assert_eq!(f.dropped, Some(Dropped::NoPin));
        assert_eq!(
            f.start_note().unwrap(),
            "Its llama-server arguments' --tensor-split 1,1 --main-gpu 1 were not passed: a \
             split of its own needs a GPU pin of 2 cards, and it has none, so lmgw placed it as \
             it places any model; to keep the split, pin it to 2 cards"
        );
        assert!(f
            .editor_note()
            .starts_with("Its own --tensor-split 1,1 needs a GPU pin of 2 cards, and it has none"));
        let f = fate(Class::Chat, &row, Some((3, &[]))).unwrap();
        assert_eq!(f.dropped, Some(Dropped::PinOf(3)));
        let f = fate(Class::Chat, &row, Some((2, &two[1..]))).unwrap();
        assert_eq!(f.dropped, Some(Dropped::Missing(vec![id(1)])));
        let f = fate(Class::Chat, &args(&["-ts", "a,b"]), Some((2, &[]))).unwrap();
        assert_eq!(f.dropped, Some(Dropped::Unread));
        let f = fate(Class::Aux, &args(&["-ts", "1,1"]), Some((2, &[]))).unwrap();
        assert_eq!(f.dropped, Some(Dropped::Aux));
        assert_eq!(
            f.start_note().unwrap(),
            "Its llama-server arguments' -ts 1,1 was not passed: aux models run on one card"
        );
        assert_eq!(fate(Class::Chat, &args(&["--no-warmup"]), None), None);
        assert_eq!(fate(Class::Audio, &row, None), None);
    }
}
