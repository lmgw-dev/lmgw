//! What VRAM admission fitted a llama-server start to in place of llama.cpp's
//! `--fit` (`vram::partial::llama`), as the start renders it and as its
//! container records it ([`FITTED_LABEL`]), so boot adopts the container
//! it renders.
//!
//! **A split** (device-ledger design §4.3, §7.2, WP6): a chat model lmgw
//! placed over several cards is told how, as [`TensorSplit`]: `--split-mode
//! layer`, `--tensor-split` with the layers on each card in the cards'
//! order (the engine numbers them by PCI bus, `CUDA_DEVICE_ORDER`), the
//! drafter confined to the first card (`-devd CUDA0`) and `--fit off`
//! where the image has `--fit`. The label records it (`split=15/22`); an
//! lmgw older than WP6 cannot read such a label and renders the command
//! without it, so it starts the container again (§3.3, roll-back).

use crate::config::LlamaParams;

/// The most cards a [`TensorSplit`] names: more than any machine lmgw ran
/// on; a split over more is not made (`vram::place`).
pub const MAX_SPLIT: usize = 16;

/// How lmgw split a chat model over its cards (module doc): the entries of
/// `--tensor-split`, layer counts in the cards' order that sum to the
/// layers on the cards with the output layer (`n_layer + 1` where every
/// layer is on a card), the last card's count holding the output layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TensorSplit {
    len: u8,
    entries: [u32; MAX_SPLIT],
    /// The row runs a draft model: confined to the first card (`-devd
    /// CUDA0`), where it is charged (§4.3; llama.cpp would spread it over
    /// every card it sees).
    pub draft_first: bool,
    /// The image's llama-server has no `--fit`: `--fit off` is not
    /// rendered beside the split (there is nothing to turn off).
    pub no_fit: bool,
}

impl TensorSplit {
    /// A split of `entries` (two to [`MAX_SPLIT`] cards, each entry at
    /// least one layer); `None` for anything else.
    pub fn new(entries: &[u64], draft_first: bool, no_fit: bool) -> Option<Self> {
        if !(2..=MAX_SPLIT).contains(&entries.len()) || entries.contains(&0) {
            return None;
        }
        let mut e = [0u32; MAX_SPLIT];
        for (to, from) in e.iter_mut().zip(entries) {
            *to = u32::try_from(*from).ok()?;
        }
        Some(Self {
            len: entries.len() as u8,
            entries: e,
            draft_first,
            no_fit,
        })
    }

    /// The entries, one per card in the cards' order.
    pub fn entries(&self) -> &[u32] {
        &self.entries[..usize::from(self.len)]
    }

    /// `15,22`: the value of `--tensor-split`.
    pub fn value(&self) -> String {
        self.entries()
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The flags it renders, beside the row's own (module doc).
    pub fn args(&self) -> Vec<String> {
        let mut out = vec![
            "--split-mode".to_string(),
            "layer".into(),
            "--tensor-split".into(),
            self.value(),
        ];
        if self.draft_first {
            out.extend(["-devd".to_string(), "CUDA0".into()]);
        }
        out
    }

    /// Its part of a [`FITTED_LABEL`]: `split=15/22`, `,devd=1` with a
    /// drafter.
    fn label(&self) -> String {
        let e: Vec<String> = self.entries().iter().map(u32::to_string).collect();
        format!(
            "split={}{}",
            e.join("/"),
            if self.draft_first { ",devd=1" } else { "" }
        )
    }
}

/// What lmgw tells llama-server in place of `--fit` (`vram::plan::chat::fit`):
/// each value `Some` is rendered over the row's, and `--fit off` with them.
/// All `None` is the row unshrunk, which renders nothing extra.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LlamaPlacement {
    /// `--ctx-size`: the context, lowered from the trained one.
    pub ctx: Option<u64>,
    /// `--n-cpu-moe`: the first layers whose routed experts stay in host
    /// memory.
    pub n_cpu_moe: Option<u64>,
    /// `--n-gpu-layers`: the layers on the card, the output layer counted.
    pub n_gpu_layers: Option<u64>,
    /// Over several cards: how (module doc). Never fitted smaller: a split
    /// is the row's whole (device-ledger design "Not yet").
    pub split: Option<TensorSplit>,
}

impl LlamaPlacement {
    /// Nothing to render: the row as written.
    pub fn is_whole(&self) -> bool {
        *self == Self::default()
    }

    /// Render `self` over a chat row's `params`: its values and `--fit off`
    /// (not for a split on an image without `--fit`). The row as written
    /// changes nothing. A split's own flags are [`TensorSplit::args`],
    /// rendered beside the params.
    pub fn apply(&self, params: &mut LlamaParams) {
        if self.is_whole() || self.split.is_some_and(|s| s.no_fit) {
            return;
        }
        let int = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        if let Some(c) = self.ctx {
            params.ctx_size = Some(int(c));
        }
        if let Some(k) = self.n_cpu_moe {
            params.n_cpu_moe = Some(int(k));
        }
        if let Some(n) = self.n_gpu_layers {
            params.n_gpu_layers = Some(int(n));
        }
        params.fit = Some("off".into());
    }

    /// The values as llama-server flags, for sentences.
    pub fn flags(&self) -> String {
        let mut out = Vec::new();
        if let Some(c) = self.ctx {
            out.push(format!("--ctx-size {c}"));
        }
        if let Some(k) = self.n_cpu_moe {
            out.push(format!("--n-cpu-moe {k}"));
        }
        if let Some(n) = self.n_gpu_layers {
            out.push(format!("--n-gpu-layers {n}"));
        }
        if let Some(s) = &self.split {
            out.push(s.args().join(" "));
        }
        if !self.split.is_some_and(|s| s.no_fit) {
            out.push("--fit off".into());
        }
        out.join(" ")
    }

    /// Only the values `keep` says the row still leaves to fitting: the
    /// context where it sets none, the layers where they are on auto. A
    /// value the row now sets itself is its own (an edit after the start).
    /// A split is lmgw's placement, not a value left to fitting, and stays.
    pub fn only(&self, keep: Left) -> Self {
        Self {
            ctx: self.ctx.filter(|_| keep.ctx),
            n_cpu_moe: self.n_cpu_moe.filter(|_| keep.layers),
            n_gpu_layers: self.n_gpu_layers.filter(|_| keep.layers),
            split: self.split,
        }
    }
}

/// What a row leaves to llama.cpp's `--fit` (`vram::plan::chat::fit`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Left {
    /// `--fit` on and no context set.
    pub ctx: bool,
    /// `--fit` on, `--n-gpu-layers` on auto and no tensor override.
    pub layers: bool,
}

/// A fitted start as its container carries it: what admission charged it,
/// and the placement rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fitted {
    /// What admission reserved on the card for it, in bytes.
    pub bytes: u64,
    pub placement: LlamaPlacement,
}

/// The label that records a fitted start's [`Fitted`] on its container:
/// `<bytes>:ctx=<n>,n-cpu-moe=<n>,n-gpu-layers=<n>,fit=off`, a value left
/// to the row empty; a split adds `split=<n>/<n>…` (and `devd=1` with a
/// drafter) before `fit`, which is `fit=none` where the image has no
/// `--fit` (module doc). What boot reconciliation renders the expected
/// command from, and what the ledger charges the adopted entry.
pub const FITTED_LABEL: &str = "lmgw.llama-fit";

impl Fitted {
    /// The [`FITTED_LABEL`] value.
    pub fn label(&self) -> String {
        let v = |o: Option<u64>| o.map(|n| n.to_string()).unwrap_or_default();
        let p = &self.placement;
        let split = p
            .split
            .map(|s| format!("{},", s.label()))
            .unwrap_or_default();
        let fit = match p.split.is_some_and(|s| s.no_fit) {
            true => "none",
            false => "off",
        };
        format!(
            "{}:ctx={},n-cpu-moe={},n-gpu-layers={},{split}fit={fit}",
            self.bytes,
            v(p.ctx),
            v(p.n_cpu_moe),
            v(p.n_gpu_layers)
        )
    }

    /// A [`FITTED_LABEL`] value read back; `None` for anything else.
    pub fn parse(label: &str) -> Option<Self> {
        let (bytes, rest) = label.trim().split_once(':')?;
        let mut p = LlamaPlacement::default();
        let mut fit: Option<&str> = None;
        let mut split: Option<Vec<u64>> = None;
        let mut devd = false;
        for kv in rest.split(',') {
            let (k, v) = kv.split_once('=')?;
            let n = || -> Option<Option<u64>> {
                match v {
                    "" => Some(None),
                    v => v.parse().ok().map(Some),
                }
            };
            match k {
                "ctx" => p.ctx = n()?,
                "n-cpu-moe" => p.n_cpu_moe = n()?,
                "n-gpu-layers" => p.n_gpu_layers = n()?,
                "split" => {
                    split = Some(
                        v.split('/')
                            .map(|e| e.parse().ok())
                            .collect::<Option<Vec<u64>>>()?,
                    )
                }
                "devd" => devd = v == "1",
                "fit" => fit = Some(v),
                _ => return None,
            }
        }
        match (fit, &split) {
            (Some("off"), _) => {}
            // Only a split says the image has no `--fit`.
            (Some("none"), Some(_)) => {}
            _ => return None,
        }
        if let Some(e) = split {
            p.split = Some(TensorSplit::new(&e, devd, fit == Some("none"))?);
        }
        Some(Self {
            bytes: bytes.parse().ok()?,
            placement: p,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_reads_back() {
        let f = Fitted {
            bytes: 3 << 30,
            placement: LlamaPlacement {
                ctx: Some(4096),
                n_cpu_moe: None,
                n_gpu_layers: Some(33),
                split: None,
            },
        };
        assert_eq!(
            f.label(),
            "3221225472:ctx=4096,n-cpu-moe=,n-gpu-layers=33,fit=off"
        );
        assert_eq!(Fitted::parse(&f.label()), Some(f));
        assert_eq!(
            Fitted::parse("3221225472:2147483648"),
            None,
            "an image grant"
        );
        assert_eq!(Fitted::parse("12:ctx=1,fit=on"), None);
    }

    /// A value the row now sets itself is dropped; the row as written
    /// renders nothing.
    #[test]
    fn only_what_is_still_left_to_fitting() {
        let p = LlamaPlacement {
            ctx: Some(4096),
            n_cpu_moe: Some(3),
            n_gpu_layers: Some(10),
            split: None,
        };
        let ctx_only = Left {
            ctx: true,
            layers: false,
        };
        assert_eq!(
            p.only(ctx_only),
            LlamaPlacement {
                ctx: Some(4096),
                ..Default::default()
            }
        );
        let mut params = LlamaParams::default();
        p.only(Left::default()).apply(&mut params);
        assert_eq!(params, LlamaParams::default(), "nothing extra");
        p.apply(&mut params);
        assert_eq!(params.fit.as_deref(), Some("off"));
        assert_eq!(params.ctx_size, Some(4096));
    }

    /// A split's label reads back with its drafter and an image without
    /// `--fit`; one-card labels keep their form, and `fit=none` without a
    /// split is not a label of lmgw's (device-ledger design §3.3, §7.2).
    #[test]
    fn a_split_label_reads_back() {
        let split = TensorSplit::new(&[15, 22], true, false).unwrap();
        let f = Fitted {
            bytes: 7 << 30,
            placement: LlamaPlacement {
                split: Some(split),
                ..Default::default()
            },
        };
        assert_eq!(
            f.label(),
            "7516192768:ctx=,n-cpu-moe=,n-gpu-layers=,split=15/22,devd=1,fit=off"
        );
        assert_eq!(Fitted::parse(&f.label()), Some(f));
        let bare = TensorSplit::new(&[9, 13, 15], false, true).unwrap();
        let g = Fitted {
            bytes: 1,
            placement: LlamaPlacement {
                split: Some(bare),
                ..Default::default()
            },
        };
        assert_eq!(
            g.label(),
            "1:ctx=,n-cpu-moe=,n-gpu-layers=,split=9/13/15,fit=none"
        );
        assert_eq!(Fitted::parse(&g.label()), Some(g));
        assert_eq!(Fitted::parse("1:ctx=4,fit=none"), None);
        assert_eq!(TensorSplit::new(&[37], false, false), None, "one card");
        assert_eq!(TensorSplit::new(&[0, 37], false, false), None);
    }

    /// The split renders its flags, `--fit off` only where the image has
    /// `--fit`, and survives what the row now sets itself.
    #[test]
    fn a_split_renders_and_stays() {
        let s = TensorSplit::new(&[15, 22], true, false).unwrap();
        assert_eq!(
            s.args(),
            vec![
                "--split-mode",
                "layer",
                "--tensor-split",
                "15,22",
                "-devd",
                "CUDA0"
            ]
        );
        let p = LlamaPlacement {
            split: Some(s),
            ..Default::default()
        };
        assert!(!p.is_whole());
        assert_eq!(p.only(Left::default()), p);
        let mut params = LlamaParams::default();
        p.apply(&mut params);
        assert_eq!(params.fit.as_deref(), Some("off"));
        let none = LlamaPlacement {
            split: TensorSplit::new(&[15, 22], false, true),
            ..Default::default()
        };
        let mut params = LlamaParams::default();
        none.apply(&mut params);
        assert_eq!(params, LlamaParams::default(), "no --fit to turn off");
        assert_eq!(none.flags(), "--split-mode layer --tensor-split 15,22");
    }
}
