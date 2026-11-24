//! A file's fit over the graphics cards (device-ledger design §13 WP12, the
//! owner's decisions of 2026-11-17): two signals judged at the context Get
//! it would create — **Min. cards**, the fewest cards whose total memory
//! holds it, and **Free VRAM**, whether the cards it would use have room
//! now — a third for a mixture-of-experts model kept with its experts in
//! RAM, and the choices Get it asks for where lmgw cannot decide: a file
//! that fits only split, or only with its experts in RAM.
//!
//! Each signal carries its words (`label`, `sentence`) and the figures per
//! card behind them, so a page draws badges and an agent reads fields.

use serde::{Deserialize, Serialize};

/// How a signal reads: the badge's colour and icon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Sign {
    /// It holds: a tick, green.
    Ok,
    /// It holds once something happens, or with a condition: "!", amber.
    Warn,
    /// It does not hold: a cross, red.
    No,
    /// Not known: "?", grey; `sentence` says why.
    #[default]
    Unknown,
}

/// One card's part, as a signal or a choice counts it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardPart {
    /// The driver's index (`nvidia-smi`'s).
    pub index: u32,
    pub name: String,
    /// What the model and its engine would take on this card, without what
    /// is kept free beside it.
    pub bytes: u64,
    /// The repeating layers it would hold, for a split; `None` on one card.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub layers: Option<u64>,
    /// The card's total memory.
    pub total_bytes: u64,
    /// Its free memory now, for this start (what other starts keep there
    /// taken off).
    pub free_bytes: u64,
    /// What is kept free beside the model there (the headroom, or the
    /// margin where nothing of lmgw's is left).
    pub keep_bytes: u64,
}

/// What Min. cards says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MinCardsState {
    /// `cards` of the computer's cards hold it, one or a split.
    Holds,
    /// One card holds only a split of it, and lmgw cannot split it here:
    /// its kind runs on one card (aux), or its engine cannot split
    /// (`sentence` says why).
    SplitNotHere,
    /// Larger than all the cards together.
    TooLarge,
    /// A declared GPU memory budget is set: `sign` says whether it fits the
    /// budget, in place of a count of cards.
    Budget,
    /// The computer has one card: the signal is not shown.
    OneCard,
    /// Its size is not known (`sentence` says why).
    #[default]
    Unknown,
}

/// **Min. cards**: the fewest cards whose total memory holds the file at
/// the signal's context, each keeping free beside its part what admission
/// keeps free there with nothing else of lmgw's on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MinCards {
    pub state: MinCardsState,
    pub sign: Sign,
    /// The count, where one is known (also for `split_not_here`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub cards: Option<u32>,
    /// The badge: "2 of 4", "budget", "too large".
    pub label: String,
    /// What it means, in a sentence.
    pub sentence: String,
    /// What it puts on each card when every card is empty.
    pub parts: Vec<CardPart>,
}

/// What Free VRAM says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FreeState {
    /// The cards it would use have room now.
    Now,
    /// They have room once lmgw's idle models there unload (`unload`).
    AfterUnloading,
    /// They have room once lmgw's busy models there finish and unload.
    AfterWaiting,
    /// They have not: `needs_bytes` against `free_bytes`.
    Short,
    /// Not known (`sentence` says why).
    #[default]
    Unknown,
}

/// **Free VRAM**: whether the cards the file would use have room for it
/// now, as a start would decide it (device-ledger design §5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FreeVram {
    pub state: FreeState,
    pub sign: Sign,
    /// The badge: "fits now", "after unloading 2 models", "needs 21 GiB,
    /// card 2 has 18".
    pub label: String,
    pub sentence: String,
    /// The cards it would use, with what it takes on each.
    pub cards: Vec<CardPart>,
    /// lmgw's models that unload (or are waited for) first, by name.
    pub unload: Vec<String>,
    /// What it needs where it is short, what it keeps free included, and
    /// what is there for it; `None` unless `short`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub needs_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub free_bytes: Option<u64>,
}

/// The third signal of a mixture-of-experts model whose context no card
/// holds whole: kept with every layer's routed experts in RAM
/// (`--n-cpu-moe`), how many cards hold the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExpertsInRam {
    pub sign: Sign,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub cards: Option<u32>,
    /// "with experts in RAM: 1 card".
    pub label: String,
    pub sentence: String,
    /// The experts' bytes in the computer's memory.
    pub ram_bytes: u64,
    /// What it puts on each card when every card is empty.
    pub parts: Vec<CardPart>,
    /// Whether the cards it would use have room now.
    pub free: FreeVram,
}

/// How Get it sets the model up, where it asks (`model_get`'s `layout`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum GetLayout {
    /// On one card, at the largest context that fits there: what Get it
    /// does for a file that fits one card.
    #[default]
    OneCard,
    /// At the full context, split over the fewest cards that hold it.
    Split,
    /// At the full context, with every layer's routed experts in RAM, on
    /// as few cards as hold the rest.
    ExpertsInRam,
}

impl GetLayout {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneCard => "one_card",
            Self::Split => "split",
            Self::ExpertsInRam => "experts_in_ram",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "one_card" => Some(Self::OneCard),
            "split" => Some(Self::Split),
            "experts_in_ram" => Some(Self::ExpertsInRam),
            _ => None,
        }
    }
}

/// One way Get it can set the file up, with its numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GetChoice {
    pub layout: GetLayout,
    /// The context the row gets.
    pub ctx: u64,
    /// The cards it goes on, with what it takes on each: the ones it would
    /// use now where they have room, else the fewest that hold it.
    pub cards: Vec<CardPart>,
    /// What stays in the computer's memory (the experts), for
    /// `experts_in_ram`.
    pub ram_bytes: u64,
    /// Whether it has room now (`free` says what has to happen first).
    pub free: FreeVram,
    /// "Split over cards 2 and 3 at 262 144 tokens: 21.0 GiB on card 2,
    /// 5.6 GiB on card 3."
    pub sentence: String,
}

/// A file's fit over the cards (module doc), judged at `ctx`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardsFit {
    /// The context the signals are judged at: the one Get it would create,
    /// and the full one where Get it asks.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub ctx: Option<u64>,
    /// How many graphics cards the computer has (1 under a budget without a
    /// device reading).
    pub of: u32,
    pub min_cards: MinCards,
    pub free: FreeVram,
    /// A mixture-of-experts model that no single card holds at the full
    /// context, with its experts in RAM.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub experts_in_ram: Option<ExpertsInRam>,
    /// Where Get it asks: the ways it can set the file up, none chosen.
    /// Empty where it does not ask (the file fits one card at its full
    /// context, or nothing else holds more of it).
    pub choices: Vec<GetChoice>,
    /// Judged by the file's size alone (its header not read): no
    /// split is worked out, and Free VRAM is one card's.
    pub by_size: bool,
    /// What the reader should also know, each a sentence.
    pub notes: Vec<String>,
}

impl CardsFit {
    /// Get it asks before it downloads anything.
    pub fn asks(&self) -> bool {
        !self.choices.is_empty()
    }
}
