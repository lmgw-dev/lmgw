//! What VRAM admission fitted a llama-server start to in place of llama.cpp's
//! `--fit` (`vram::partial::llama`), as the start renders it and as its
//! container records it ([`FITTED_LABEL`]), so boot adopts the container
//! it renders.

use crate::config::LlamaParams;

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
}

impl LlamaPlacement {
    /// Nothing to render: the row as written.
    pub fn is_whole(&self) -> bool {
        *self == Self::default()
    }

    /// Render `self` over a chat row's `params`: its values and `--fit off`.
    /// The row as written changes nothing.
    pub fn apply(&self, params: &mut LlamaParams) {
        if self.is_whole() {
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
        out.push("--fit off".into());
        out.join(" ")
    }

    /// Only the values `keep` says the row still leaves to fitting: the
    /// context where it sets none, the layers where they are on auto. A
    /// value the row now sets itself is its own (an edit after the start).
    pub fn only(&self, keep: Left) -> Self {
        Self {
            ctx: self.ctx.filter(|_| keep.ctx),
            n_cpu_moe: self.n_cpu_moe.filter(|_| keep.layers),
            n_gpu_layers: self.n_gpu_layers.filter(|_| keep.layers),
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
/// to the row empty. What boot reconciliation renders the expected command
/// from, and what the ledger charges the adopted entry.
pub const FITTED_LABEL: &str = "lmgw.llama-fit";

impl Fitted {
    /// The [`FITTED_LABEL`] value.
    pub fn label(&self) -> String {
        let v = |o: Option<u64>| o.map(|n| n.to_string()).unwrap_or_default();
        let p = &self.placement;
        format!(
            "{}:ctx={},n-cpu-moe={},n-gpu-layers={},fit=off",
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
        let mut fit_off = false;
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
                "fit" => fit_off = v == "off",
                _ => return None,
            }
        }
        fit_off.then_some(())?;
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
}
