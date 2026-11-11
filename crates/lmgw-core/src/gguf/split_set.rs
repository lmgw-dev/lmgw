//! The tensors whose place in a file decides where llama.cpp keeps them,
//! taken over every part of a split GGUF (find-a-model design D3, owner
//! ruling 2026-11-04).
//!
//! llama.cpp keeps `token_embd.weight` in host memory when the model has a
//! separate `output.weight` (a tied head is copied to the card for the
//! logits), and Gemma 4's `per_layer_token_embd.weight` there always
//! ([`super::ModelSummary::host_input_bytes`]). Which is the case is a fact
//! of the model, not of the file a header was read from: a split set's
//! first part can hold `token_embd.weight` while the untied `output.weight`
//! sits in its last part (`Qwen/Qwen2.5-Coder-32B-Instruct-GGUF`'s `q4_0`:
//! part 1 and part 3). Read from part 1 alone such a set had no output head,
//! so its input embeddings (418 MiB at Q4_0) were charged to the card where
//! the single file of the same quant kept them on the host, and the output
//! head the compute buffer of a CPU row copies over was `token_embd`'s
//! bytes instead of `output.weight`'s.
//!
//! So a summary carries these tensors' bytes ([`HeadTensors`]), and
//! [`super::ModelSummary::of_set`] takes the host-kept bytes and the output
//! head over every part, by the same rule as for one file.

use super::{GgufMeta, ModelSummary};

/// The bytes of the tensors [`super::ModelSummary::host_input_bytes`] and
/// [`super::ModelSummary::output_head_bytes`] are taken from, in one file;
/// `None` where the file doesn't hold the tensor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadTensors {
    /// `token_embd.weight`.
    pub token_embd: Option<u64>,
    /// `per_layer_token_embd.weight` (Gemma 4's E-models).
    pub per_layer: Option<u64>,
    /// `output.weight`: the head isn't tied.
    pub output: Option<u64>,
}

impl HeadTensors {
    pub(super) fn of(meta: &GgufMeta) -> Self {
        Self {
            token_embd: meta.tensor_size("token_embd.weight"),
            per_layer: meta.tensor_size("per_layer_token_embd.weight"),
            output: meta.tensor_size("output.weight"),
        }
    }

    /// The tensors of `parts`, the first part first: each one wherever a
    /// part holds it (every tensor is in exactly one part of a split set).
    fn over(parts: &[&HeadTensors]) -> Self {
        let first = |f: fn(&HeadTensors) -> Option<u64>| parts.iter().find_map(|p| f(p));
        Self {
            token_embd: first(|p| p.token_embd),
            per_layer: first(|p| p.per_layer),
            output: first(|p| p.output),
        }
    }

    /// What llama.cpp keeps in host memory whatever `--n-gpu-layers` says:
    /// `token_embd.weight` beside a separate output head, and the per-layer
    /// embeddings.
    pub(super) fn host_input_bytes(&self) -> u64 {
        self.output
            .and(self.token_embd)
            .unwrap_or(0)
            .saturating_add(self.per_layer.unwrap_or(0))
    }

    /// The output head's bytes: `output.weight`, else the `token_embd.weight`
    /// a tied head reuses.
    pub(super) fn output_head_bytes(&self) -> Option<u64> {
        self.output.or(self.token_embd).filter(|b| *b > 0)
    }
}

impl ModelSummary {
    /// This summary, a split file's first part's, with the facts of the
    /// tensors in `later` (every later part's header, in order) taken over
    /// the whole set: the host-kept input embeddings and the output head
    /// (module doc), and every part's tensors ([`super::TensorList`]).
    /// Everything else in a split set's metadata is part 1's
    /// own, which llama.cpp loads it from. A file that isn't split, with no
    /// later parts, is unchanged.
    pub fn of_set(mut self, later: &[&ModelSummary]) -> Self {
        if later.is_empty() {
            return self;
        }
        let mut parts = vec![&self.head_tensors];
        parts.extend(later.iter().map(|p| &p.head_tensors));
        let set = HeadTensors::over(&parts);
        self.host_input_bytes = set.host_input_bytes();
        self.output_head_bytes = set.output_head_bytes();
        self.head_tensors = set;
        self.take_tensors(later);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gguf::{read_header, synth};

    fn summary(h: &synth::Header, dir: &std::path::Path, name: &str) -> ModelSummary {
        let path = dir.join(name);
        h.write_to(&path);
        ModelSummary::from_meta(&read_header(&path).unwrap())
    }

    /// A set whose untied head is in its last part keeps its input
    /// embeddings on the host and copies the real head, as the single file
    /// of the same tensors does; a tied set charges neither.
    #[test]
    fn a_split_sets_head_is_every_parts() {
        let dir = tempfile::tempdir().unwrap();
        let mut one = synth::chat("qwen2", 4096);
        one.tensor_sized("token_embd.weight", 4000)
            .tensor_sized("blk.0.attn_q.weight", 100)
            .tensor_sized("output.weight", 6000);
        let one = summary(&one, dir.path(), "one.gguf");
        assert_eq!(one.host_input_bytes, 4000);
        assert_eq!(one.output_head_bytes, Some(6000));

        let mut p1 = synth::chat("qwen2", 4096);
        p1.tensor_sized("token_embd.weight", 4000)
            .tensor_sized("blk.0.attn_q.weight", 100);
        let mut p2 = synth::Header::default();
        p2.tensor_sized("output.weight", 6000);
        let p1 = summary(&p1, dir.path(), "p1.gguf");
        let p2 = summary(&p2, dir.path(), "p2.gguf");
        assert_eq!(p1.host_input_bytes, 0, "part 1 alone reads as tied");
        assert_eq!(p1.output_head_bytes, Some(4000));
        let set = p1.clone().of_set(&[&p2]);
        assert_eq!(set.host_input_bytes, one.host_input_bytes);
        assert_eq!(set.output_head_bytes, one.output_head_bytes);
        assert_eq!(p1.clone().of_set(&[]).host_input_bytes, 0);

        // Tied over every part: nothing is kept on the host.
        let mut p2 = synth::Header::default();
        p2.tensor_sized("blk.1.attn_q.weight", 100);
        let p2 = summary(&p2, dir.path(), "tied2.gguf");
        let set = p1.of_set(&[&p2]);
        assert_eq!(set.host_input_bytes, 0);
        assert_eq!(set.output_head_bytes, Some(4000));
    }
}
