//! What a model is charged on the cards (device-ledger design D4, D23,
//! §3.2, §4.1, WP4): a [`CardCharge`].
//!
//! **Parts.** A charge is a list of parts, each the bytes on a list of
//! cards. A part of one card is what is on that card: the design's
//! `(DeviceId, bytes)`. A part of several cards is a **group** (§5.7): the
//! bytes are on those cards together and how they divide is not known, so
//! no card is charged a share of them — a share would be a guessed limit
//! (ruling 4). A part with no card is a charge whose cards no device read
//! has named: a declared budget without telemetry. A model on the CPU
//! ([`crate::runtime::Placement::Cpu`]) has the empty charge, no part.
//!
//! **lmgw chooses the cards** (WP5, `vram::place`): a model admission
//! decided is charged on the card it was placed on, and started there where
//! that is fewer cards than its run arguments pass ([`CardCharge::handed`],
//! §7.1). A model lmgw cannot hand a card (an AMD card, a native start, a
//! start no decision stood behind) is a group on the cards it sees
//! ([`seen`]): every card where its arguments pass every card
//! (`nvidia.com/gpu=all`, `--gpus all`, a GPU node such as `/dev/dri`), the
//! cards they name where they name NVIDIA cards, none on the CPU. One card
//! seen is a part of one card. [`CardCharge::total`] is what a decision
//! compares on the cards it placed on.
//!
//! **What a group holds on each card** once it runs is the per-process
//! measurement, where the driver gives one: the ledger then charges it
//! that, a part per card ([`CardCharge::per_card`]); without one it is
//! charged its estimate on the group, and says so.
//!
//! **Order.** Cards in the order the device read lists them: NVML's index,
//! which enumerates by PCI bus id; amdgpu's sysfs cards, in theirs.
//!
//! **The label** (§3.3, D12): a container records its charge as
//! [`PLACEMENT_LABEL`]`=<part>[,<part>…]`, a part `<id>[+<id>…]:<bytes>`
//! (an id may hold colons, an AMD card's PCI address: the bytes are after
//! the last one). Boot adoption reads it back ([`CardCharge::parse`]).

use serde::Serialize;

use super::nvml::{DeviceId, GpuMemory};
use crate::runtime::launcher::native::gpu::{selection, Selection};
use crate::runtime::Placement;

/// The label a GPU container records its charge in (§3.3).
pub const PLACEMENT_LABEL: &str = "lmgw.placement";

/// What a model is charged on the cards (module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CardCharge {
    parts: Vec<ChargePart>,
    /// lmgw hands the engine exactly these cards (§7.1, D17): fewer than
    /// its run arguments pass. Set by admission where it placed the model
    /// ([`Self::handed`]); never read back from a label, since an adopted
    /// container is not rendered again.
    #[serde(skip)]
    handed: bool,
    /// The engine is handed its cards for the row's own `--tensor-split`,
    /// which it is started with as written (§7.2, ruling 11,
    /// [`Self::keeping_own_split`]); read back from
    /// [`crate::runtime::argv::OWN_SPLIT_LABEL`] by adoption.
    #[serde(skip)]
    own_split: bool,
}

/// One part of a [`CardCharge`]: `bytes` on `cards` together (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChargePart {
    /// One card: what is on it. Several: a group. None: cards no device
    /// read has named.
    pub cards: Vec<DeviceId>,
    pub bytes: u64,
}

impl CardCharge {
    /// The empty charge: on no card (the CPU, or nothing charged).
    pub fn none() -> Self {
        Self::default()
    }

    /// `bytes` on `cards`, as one part: what is on the card where there is
    /// one, a group where there are several (module doc).
    pub fn over(cards: Vec<DeviceId>, bytes: u64) -> Self {
        Self {
            parts: vec![ChargePart { cards, bytes }],
            handed: false,
            own_split: false,
        }
    }

    /// What is on each card, a part per card: a group charged by what its
    /// processes hold on each card (device-ledger design §3.3, WP5).
    pub fn per_card(cards: Vec<(DeviceId, u64)>) -> Self {
        Self {
            parts: cards
                .into_iter()
                .map(|(id, bytes)| ChargePart {
                    cards: vec![id],
                    bytes,
                })
                .collect(),
            handed: false,
            own_split: false,
        }
    }

    /// The same charge, with its cards the ones lmgw hands the engine
    /// (§7.1).
    pub fn handed(mut self) -> Self {
        self.handed = true;
        self
    }

    /// The cards lmgw hands the engine, in place of the NVIDIA cards its
    /// run arguments pass (§7.1); `None` where the arguments are used as
    /// written.
    pub fn handed_cards(&self) -> Option<Vec<DeviceId>> {
        self.handed.then(|| self.cards())
    }

    /// The same charge, handed to the engine for the row's own
    /// `--tensor-split` (device-ledger design §7.2, ruling 11): a chat row
    /// pinned to as many cards as its split has entries, started with its
    /// arguments as written.
    pub fn keeping_own_split(mut self) -> Self {
        self.handed = true;
        self.own_split = true;
        self
    }

    /// Whether the engine is handed its cards for the row's own split
    /// ([`Self::keeping_own_split`]).
    pub fn keeps_own_split(&self) -> bool {
        self.handed && self.own_split
    }

    /// A split's parts (device-ledger design §4.3, WP6): several parts of
    /// one card each, as `(card, bytes)`; `None` for any other charge.
    pub fn split_parts(&self) -> Option<Vec<(DeviceId, u64)>> {
        (self.parts.len() > 1 && self.parts.iter().all(|p| p.cards.len() == 1)).then(|| {
            self.parts
                .iter()
                .map(|p| (p.cards[0].clone(), p.bytes))
                .collect()
        })
    }

    /// On several cards: a group, or a split.
    pub fn on_several(&self) -> bool {
        self.split_parts().is_some() || self.parts.iter().any(|p| p.cards.len() > 1)
    }

    /// The bytes of every part together: what the decisions read.
    pub fn total(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| p.bytes)
            .fold(0, u64::saturating_add)
    }

    pub fn parts(&self) -> &[ChargePart] {
        &self.parts
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Every card a part names, each once, in the order the parts name
    /// them.
    pub fn cards(&self) -> Vec<DeviceId> {
        let mut out: Vec<DeviceId> = Vec::new();
        for c in self.parts.iter().flat_map(|p| &p.cards) {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }

    /// The same cards, charged `bytes`: a charge of one part keeps its
    /// cards; one of several parts (a later lmgw's split) becomes a group
    /// over all its cards, which claims nothing per card.
    pub fn with_total(&self, bytes: u64) -> Self {
        let c = match self.parts.as_slice() {
            [] => return Self::none(),
            [one] => Self::over(one.cards.clone(), bytes),
            _ => Self::over(self.cards(), bytes),
        };
        Self {
            handed: self.handed,
            own_split: self.own_split,
            ..c
        }
    }

    /// What is charged on `card` alone: the parts of that card only.
    /// `None` where a group or a part with no card could also be there,
    /// which no figure for the card alone can say.
    pub fn alone_on(&self, card: &DeviceId) -> Option<u64> {
        let mut bytes = 0u64;
        for p in &self.parts {
            match p.cards.as_slice() {
                [one] if one == card => bytes = bytes.saturating_add(p.bytes),
                [_] => {}
                cards if cards.is_empty() || cards.contains(card) => return None,
                _ => {}
            }
        }
        Some(bytes)
    }

    /// Without the cards not in `present`: the parts on those that are,
    /// and the cards that are gone. A part all of whose cards are gone is
    /// charged nowhere; a group that lost some keeps its bytes on the rest.
    /// A part with no card is kept: it names no card that could be gone.
    pub fn on_present(&self, present: &[DeviceId]) -> (Self, Vec<DeviceId>) {
        let mut gone: Vec<DeviceId> = Vec::new();
        let mut parts = Vec::new();
        for p in &self.parts {
            let (here, away): (Vec<DeviceId>, Vec<DeviceId>) =
                p.cards.iter().cloned().partition(|c| present.contains(c));
            for c in away {
                if !gone.contains(&c) {
                    gone.push(c);
                }
            }
            if here.is_empty() && !p.cards.is_empty() {
                continue;
            }
            parts.push(ChargePart {
                cards: here,
                bytes: p.bytes,
            });
        }
        (
            Self {
                parts,
                handed: self.handed,
                own_split: self.own_split,
            },
            gone,
        )
    }

    /// The [`PLACEMENT_LABEL`] value; `None` where there is nothing to
    /// record: no part, or a part whose cards no device read has named.
    pub fn label(&self) -> Option<String> {
        if self.parts.is_empty() || self.parts.iter().any(|p| p.cards.is_empty()) {
            return None;
        }
        Some(
            self.parts
                .iter()
                .map(|p| {
                    let ids: Vec<&str> = p.cards.iter().map(DeviceId::as_str).collect();
                    format!("{}:{}", ids.join("+"), p.bytes)
                })
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    /// A [`PLACEMENT_LABEL`] value read back; `None` for anything else.
    pub fn parse(label: &str) -> Option<Self> {
        let mut parts = Vec::new();
        for part in label.trim().split(',') {
            let (ids, bytes) = part.trim().rsplit_once(':')?;
            let cards: Vec<DeviceId> = ids
                .split('+')
                .map(|id| (!id.is_empty()).then(|| DeviceId::new(id)))
                .collect::<Option<_>>()?;
            parts.push(ChargePart {
                cards,
                bytes: bytes.parse().ok()?,
            });
        }
        Some(Self {
            parts,
            handed: false,
            own_split: false,
        })
    }

    /// The cards in words for a note: `GPU-a`, `GPU-a and GPU-b`.
    pub(super) fn names(cards: &[DeviceId]) -> String {
        let ids: Vec<&str> = cards.iter().map(DeviceId::as_str).collect();
        match ids.as_slice() {
            [] => String::new(),
            [one] => (*one).to_string(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        }
    }
}

/// The cards a model started with `run_args` at `placement` sees, of
/// `devices` (the last device read), in their order (module doc): none on
/// the CPU; the cards an NVIDIA name picks (`nvidia.com/gpu=<index or
/// UUID>`, `--gpus device=<n>`) where each name is one of `devices`; every
/// card otherwise — the arguments pass every card or a GPU node, a name
/// that is no card of `devices` (a MIG device), a native Vulkan `build`
/// whatever the names (it sees every card), and arguments that name no
/// card at all, since an image's own `NVIDIA_VISIBLE_DEVICES=all` hands
/// such a container every card on a host with NVIDIA's legacy hook
/// (`lmgw_api_types::audio_engine::NO_GPU_ENV`).
pub fn seen(
    run_args: &[String],
    placement: Placement,
    build: Option<&str>,
    devices: &[GpuMemory],
) -> Vec<DeviceId> {
    let every = || devices.iter().map(|d| d.id.clone()).collect();
    if !placement.is_gpu() {
        return Vec::new();
    }
    let names = match selection(run_args, placement) {
        Selection::Nvidia(_) if build.is_some_and(|b| b.ends_with("-vulkan")) => return every(),
        Selection::Nvidia(names) => names,
        Selection::All | Selection::None => return every(),
    };
    let mut picked: Vec<&GpuMemory> = Vec::new();
    for name in names.iter().map(|n| n.trim()) {
        let card = devices
            .iter()
            .find(|d| d.id.as_str() == name || name.parse::<u32>().is_ok_and(|i| i == d.index));
        match card {
            Some(d) => picked.push(d),
            None => return every(),
        }
    }
    devices
        .iter()
        .filter(|d| picked.iter().any(|p| p.id == d.id))
        .map(|d| d.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(i: u32) -> GpuMemory {
        GpuMemory {
            id: DeviceId::new(format!("GPU-{i}")),
            index: i,
            name: "card".into(),
            total_bytes: 0,
            used_bytes: 0,
            free_bytes: 0,
        }
    }

    fn id(i: u32) -> DeviceId {
        DeviceId::new(format!("GPU-{i}"))
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// The cards a model sees: every card for `=all`, a GPU node, a name
    /// that is no card, a Vulkan build and arguments naming none; the named
    /// cards in the driver's order by index or UUID; none on the CPU.
    #[test]
    fn the_cards_a_model_sees() {
        let four: Vec<GpuMemory> = (0..4).map(card).collect();
        let every: Vec<DeviceId> = (0..4).map(id).collect();
        let gpu = Placement::Gpu;
        let all = args(&["--device", "nvidia.com/gpu=all"]);
        assert_eq!(seen(&all, gpu, None, &four), every);
        assert_eq!(
            seen(&args(&["--device", "/dev/dri"]), gpu, None, &four),
            every
        );
        assert_eq!(seen(&args(&["--gpus", "all"]), gpu, None, &four), every);
        assert_eq!(seen(&[], gpu, None, &four), every);
        assert_eq!(
            seen(&all, Placement::Cpu, None, &four),
            Vec::<DeviceId>::new()
        );

        let named = args(&[
            "--device",
            "nvidia.com/gpu=3",
            "--device=nvidia.com/gpu=GPU-1",
        ]);
        assert_eq!(seen(&named, gpu, None, &four), vec![id(1), id(3)]);
        assert_eq!(
            seen(&args(&["--gpus", "device=2"]), gpu, None, &four),
            vec![id(2)]
        );
        // A name no card has: nothing narrower than every card is known.
        let mig = args(&["--device", "nvidia.com/gpu=MIG-x"]);
        assert_eq!(seen(&mig, gpu, None, &four), every);
        // A Vulkan build sees every card whatever the names.
        assert_eq!(seen(&named, gpu, Some("b1-vulkan"), &four), every);
        // No device read yet: no card is known.
        assert_eq!(seen(&all, gpu, None, &[]), Vec::<DeviceId>::new());
    }

    /// The label round-trips a card, a group and an AMD card's PCI
    /// address; anything else is no label; a charge with a part whose
    /// cards are unnamed, or none, has no label to write.
    #[test]
    fn the_label_round_trips() {
        let one = CardCharge::over(vec![id(1)], 5 << 30);
        assert_eq!(one.label().unwrap(), format!("GPU-1:{}", 5u64 << 30));
        let group = CardCharge::over(vec![id(0), id(2)], 7);
        assert_eq!(group.label().unwrap(), "GPU-0+GPU-2:7");
        let amd = CardCharge::over(vec![DeviceId::new("0000:17:00.0")], 9);
        assert_eq!(amd.label().unwrap(), "0000:17:00.0:9");
        for c in [&one, &group, &amd] {
            assert_eq!(CardCharge::parse(&c.label().unwrap()).as_ref(), Some(c));
        }
        let split = CardCharge::parse("GPU-0:3,GPU-1:4").unwrap();
        assert_eq!(split.total(), 7);
        assert_eq!(split.cards(), vec![id(0), id(1)]);
        for bad in ["", "GPU-0", "GPU-0:x", ":5", "GPU-0+:5", "GPU-0:5,"] {
            assert_eq!(CardCharge::parse(bad), None, "{bad:?}");
        }
        assert_eq!(CardCharge::none().label(), None);
        assert_eq!(CardCharge::over(vec![], 5).label(), None);
    }

    /// A card's own figure: a part of that card alone; unknown where a
    /// group or a part with no card could be on it.
    #[test]
    fn what_is_on_a_card_alone() {
        let c = CardCharge::parse("GPU-0:3,GPU-1:4").unwrap();
        assert_eq!(c.alone_on(&id(0)), Some(3));
        assert_eq!(c.alone_on(&id(2)), Some(0));
        let g = CardCharge::over(vec![id(0), id(1)], 8);
        assert_eq!(g.alone_on(&id(0)), None);
        assert_eq!(g.alone_on(&id(2)), Some(0));
        assert_eq!(CardCharge::over(vec![], 8).alone_on(&id(0)), None);
        assert_eq!(CardCharge::none().alone_on(&id(0)), Some(0));
    }

    /// A card that is gone: its part is charged nowhere, a group keeps its
    /// bytes on the cards left, and the gone cards are named.
    #[test]
    fn a_card_that_is_gone() {
        let present = vec![id(0), id(1)];
        let (c, gone) = CardCharge::over(vec![id(3)], 5).on_present(&present);
        assert!(c.is_empty() && c.total() == 0);
        assert_eq!(gone, vec![id(3)]);
        let (c, gone) = CardCharge::over(vec![id(1), id(3)], 5).on_present(&present);
        assert_eq!(c, CardCharge::over(vec![id(1)], 5));
        assert_eq!(gone, vec![id(3)]);
        let (c, gone) = CardCharge::over(vec![id(0)], 5).on_present(&present);
        assert_eq!(c, CardCharge::over(vec![id(0)], 5));
        assert!(gone.is_empty());
    }

    /// The same cards with new bytes; a split becomes a group.
    #[test]
    fn with_a_new_total() {
        let one = CardCharge::over(vec![id(1)], 5);
        assert_eq!(one.with_total(9), CardCharge::over(vec![id(1)], 9));
        let split = CardCharge::parse("GPU-0:3,GPU-1:4").unwrap();
        assert_eq!(split.with_total(9), CardCharge::over(vec![id(0), id(1)], 9));
        assert_eq!(CardCharge::none().with_total(9), CardCharge::none());
    }
}
