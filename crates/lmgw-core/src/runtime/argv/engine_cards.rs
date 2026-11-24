//! llama-server's own device flags where lmgw hands the engine its cards
//! (device-ledger design §8.1, WP5): `-dev`/`--device CUDA<n>`, `--main-gpu
//! <n>` and `-sm`/`--split-mode none` in a row's freeform arguments number
//! a card among those the engine sees, and are wrong once lmgw passes it
//! fewer cards than the row's run arguments name — `-dev CUDA1` on an
//! engine that sees one card fails its start. Where the cards are handed
//! ([`crate::vram::CardCharge::handed_cards`]), the start renders the row's
//! arguments without them, and the start's warnings say so.
//!
//! The drafter's device (`-devd`, `--device-draft`, `--spec-draft-device`)
//! numbers a card the same way and goes with them.
//!
//! **A row's own split** (`--tensor-split`, rulings 11 and 16, §7.2, WP11)
//! is the row's, and so are `--split-mode` and `--main-gpu` beside it. A
//! chat row pinned to as many cards as its split has entries is handed
//! those cards and started with all of its arguments as written, these
//! flags included ([`crate::vram::CardCharge::keeps_own_split`]). Any other
//! row whose cards lmgw hands is started without its split, and without
//! `--split-mode` and `--main-gpu` beside it: lmgw places it as if the
//! split were not there.
//!
//! **Adoption** renders the command it compares a running container with
//! from the arguments as written; a container whose cards were handed
//! carries [`HANDED_LABEL`], one that kept its own split also
//! [`OWN_SPLIT_LABEL`], and the comparison renders as its start did, so
//! boot does not find its command changed.

use super::is_opt;

/// The label a container whose cards lmgw handed it carries: the cards, by
/// id, comma-separated (device-ledger design §7.1). Boot adoption reads it
/// to render the command as the start rendered it (module doc).
pub const HANDED_LABEL: &str = "lmgw.handed";

/// The label a container handed its cards for the row's own split carries
/// (`kept`, module doc): boot adoption renders the row's split as written.
pub const OWN_SPLIT_LABEL: &str = "lmgw.own-split";

/// What a flag of the row's freeform arguments is, for this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Not one of these.
    Other,
    /// It names a card among those the engine sees.
    Card,
    /// The row's own split, or a flag that goes with it.
    Split,
}

/// What `key` with `value` is, beside an own split (`own`) or not.
fn kind(key: &str, value: Option<&str>, own: bool) -> Kind {
    let key = key.replace('_', "-");
    let value = value.map(str::trim);
    match key.as_str() {
        "ts" | "tensor-split" => Kind::Split,
        // A device list; `none` keeps the model off every card, which names
        // no card.
        "dev" | "device" | "devd" | "device-draft" | "spec-draft-device" => {
            match value.is_some_and(|v| !v.eq_ignore_ascii_case("none")) {
                true => Kind::Card,
                false => Kind::Other,
            }
        }
        "mg" | "main-gpu" if own => Kind::Split,
        "mg" | "main-gpu" => Kind::Card,
        "sm" | "split-mode" if own => Kind::Split,
        "sm" | "split-mode" if value.is_some_and(|v| v.eq_ignore_ascii_case("none")) => Kind::Card,
        _ => Kind::Other,
    }
}

/// The row's own split (`--tensor-split`, `-ts`), beside which nothing is
/// dropped where it is kept (ruling 11), and which lmgw's own split never
/// replaces.
pub fn own_split(args: &[String]) -> bool {
    args.iter().any(|t| {
        let key = t.trim_start_matches('-');
        let key = key.split_once('=').map_or(key, |(k, _)| k);
        is_opt(t) && matches!(key.replace('_', "-").as_str(), "ts" | "tensor-split")
    })
}

/// A row's own `--tensor-split` as written (module doc).
#[derive(Debug, Clone, PartialEq)]
pub struct OwnSplit {
    /// Its value as written: `1,1`.
    pub written: String,
    /// Its entries, one per card in the cards' order, as llama-server
    /// reads them (numbers separated by commas or slashes); `None` where
    /// lmgw cannot read them: not numbers, a negative one, none, or more
    /// than [`super::MAX_SPLIT`] (llama-server takes at most that many).
    pub entries: Option<Vec<f32>>,
    /// `--split-mode` beside it, as written; `None`: llama-server's
    /// default, `layer`.
    pub mode: Option<String>,
}

impl OwnSplit {
    /// It splits by layers (`--split-mode layer`, the default), the one
    /// split lmgw can say what each card holds of (§4.3).
    pub fn by_layer(&self) -> bool {
        self.mode
            .as_deref()
            .is_none_or(|m| m.trim().eq_ignore_ascii_case("layer"))
    }
}

/// The row's own split (module doc), the last one where several are
/// written as llama-server reads them; `None` where it writes none.
pub fn own_split_of(args: &[String]) -> Option<OwnSplit> {
    if !own_split(args) {
        return None;
    }
    let written = super::flag_value(args, "tensor-split")
        .unwrap_or_default()
        .to_string();
    // Read up to one more than llama-server takes, never more: the count
    // of entries is the row's.
    let tokens: Vec<&str> = written
        .split([',', '/'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .take(super::MAX_SPLIT + 1)
        .collect();
    let entries = (!tokens.is_empty() && tokens.len() <= super::MAX_SPLIT)
        .then(|| {
            tokens
                .iter()
                .map(|t| t.parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0))
                .collect::<Option<Vec<f32>>>()
        })
        .flatten();
    Some(OwnSplit {
        written,
        entries,
        mode: super::flag_value(args, "split-mode").map(str::to_string),
    })
}

/// The row's arguments taken apart (module doc), each flag with its value,
/// as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardFlags {
    /// What stays where lmgw hands the engine its cards.
    pub kept: Vec<String>,
    /// The flags that name a card among those the engine sees.
    pub cards: Vec<String>,
    /// The row's own split and, beside it, `--split-mode` and
    /// `--main-gpu`.
    pub split: Vec<String>,
}

/// `args` taken apart (module doc).
pub fn card_flags(args: &[String]) -> CardFlags {
    let own = own_split(args);
    let mut out = CardFlags {
        kept: Vec::with_capacity(args.len()),
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let tok = &args[i];
        if !is_opt(tok) {
            out.kept.push(tok.clone());
            i += 1;
            continue;
        }
        let key_part = tok.trim_start_matches('-');
        let (key, inline) = match key_part.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (key_part, None),
        };
        // llama-server options take at most one argument.
        let next = (inline.is_none())
            .then(|| args.get(i + 1).filter(|v| !is_opt(v)))
            .flatten();
        let value = inline.or(next.map(String::as_str));
        let width = if next.is_some() { 2 } else { 1 };
        let group = args[i..i + width].iter().cloned();
        match kind(key, value, own) {
            Kind::Other => out.kept.extend(group),
            Kind::Card => out.cards.extend(group),
            Kind::Split => out.split.extend(group),
        }
        i += width;
    }
    out
}

/// `args` split into what stays where lmgw hands the engine its cards and
/// what is not passed there — the flags that name a card, and the row's
/// own split with what goes with it (module doc) — each with its value, as
/// written. Where the row's own split is kept the caller passes `args` as
/// written instead.
pub fn split_card_flags(args: &[String]) -> (Vec<String>, Vec<String>) {
    let f = card_flags(args);
    let mut taken = f.split;
    taken.extend(f.cards);
    (f.kept, taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// The three forms of §8.1 and the drafter's device go; everything else,
    /// `-dev none` and another split mode included, stays.
    #[test]
    fn the_flags_that_name_a_card_are_taken() {
        let (kept, taken) = split_card_flags(&args(&[
            "-dev",
            "CUDA1",
            "--no-warmup",
            "--main-gpu",
            "1",
            "-sm",
            "none",
            "--device-draft=CUDA2",
            "--threads",
            "8",
        ]));
        assert_eq!(kept, args(&["--no-warmup", "--threads", "8"]));
        assert_eq!(
            taken,
            args(&[
                "-dev",
                "CUDA1",
                "--main-gpu",
                "1",
                "-sm",
                "none",
                "--device-draft=CUDA2"
            ])
        );
        let (kept, taken) = split_card_flags(&args(&[
            "--device",
            "none",
            "--split-mode",
            "layer",
            "-mg=0",
        ]));
        assert_eq!(kept, args(&["--device", "none", "--split-mode", "layer"]));
        assert_eq!(taken, args(&["-mg=0"]));
    }

    /// Where lmgw hands the cards and the row's own split is not kept, the
    /// split goes, and `--split-mode` and `--main-gpu` with it whatever
    /// their values (§7.2); the flags that name a card go as before.
    #[test]
    fn a_row_s_own_split_goes_with_what_is_beside_it() {
        let row = args(&[
            "--tensor-split",
            "15,22",
            "--main-gpu",
            "1",
            "-sm",
            "layer",
            "-dev",
            "CUDA0,CUDA1",
            "--no-warmup",
        ]);
        let f = card_flags(&row);
        assert_eq!(f.kept, args(&["--no-warmup"]));
        assert_eq!(
            f.split,
            args(&["--tensor-split", "15,22", "--main-gpu", "1", "-sm", "layer"])
        );
        assert_eq!(f.cards, args(&["-dev", "CUDA0,CUDA1"]));
        let (kept, taken) = split_card_flags(&row);
        assert_eq!(kept, args(&["--no-warmup"]));
        assert_eq!(taken.len(), 8);
        let (kept, taken) = split_card_flags(&args(&["-ts=1,1", "--split_mode", "row"]));
        assert!(kept.is_empty());
        assert_eq!(taken, args(&["-ts=1,1", "--split_mode", "row"]));
    }

    /// The row's own split as llama-server reads it: commas or slashes, the
    /// last one written, at most 16 entries, each a number not below 0;
    /// what lmgw cannot read has no entries.
    #[test]
    fn the_own_split_is_read_as_written() {
        assert_eq!(own_split_of(&args(&["--threads", "8"])), None);
        let s = own_split_of(&args(&["-ts", "3,1", "--tensor-split=1/1/2"])).unwrap();
        assert_eq!(s.written, "1/1/2");
        assert_eq!(s.entries, Some(vec![1.0, 1.0, 2.0]));
        assert!(s.by_layer());
        let s = own_split_of(&args(&["-ts", "0.6,0.4", "-sm", "row"])).unwrap();
        assert_eq!(s.entries, Some(vec![0.6, 0.4]));
        assert!(!s.by_layer());
        for bad in ["a,b", "1,-1", "", "1,inf"] {
            let s = own_split_of(&args(&[&format!("--tensor-split={bad}")])).unwrap();
            assert_eq!(s.entries, None, "{bad}");
        }
        let many = vec!["1"; 40].join(",");
        let s = own_split_of(&args(&["-ts", &many])).unwrap();
        assert_eq!(s.entries, None, "more entries than llama-server takes");
        let sixteen = vec!["1"; 16].join(",");
        let s = own_split_of(&args(&["-ts", &sixteen])).unwrap();
        assert_eq!(s.entries.map(|e| e.len()), Some(16));
    }
}
