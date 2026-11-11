//! Every tensor's name and bytes on disk, every part's of a split GGUF: what
//! a chat row's footprint matches llama.cpp's tensor overrides against
//! (`--n-cpu-moe`, `--cpu-moe`, `--override-tensor`), so the tensors they
//! keep in host memory are off the card ([`crate::vram::plan::chat`]).
//!
//! A tensor's bytes are its data offsets' ([`super::tensors::sizes`]), the
//! padding after it included, at most `general.alignment` bytes: the same
//! figure the file's size, which the footprint starts from, counts it at.

use super::{GgufMeta, ModelSummary};

/// The tensors of one file, or of every part of a split set once
/// [`ModelSummary::of_set`] took the later parts in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TensorList {
    /// `(name, bytes on disk)`, in file order, part by part.
    pub tensors: Vec<(String, u64)>,
    /// How many files' tensors are listed: 1 for a header read alone, the
    /// set's count once every later part was taken in.
    pub parts: u64,
}

impl TensorList {
    pub(super) fn of(meta: &GgufMeta) -> Self {
        Self {
            tensors: meta
                .tensor_names
                .iter()
                .cloned()
                .zip(meta.tensor_bytes.iter().copied())
                .collect(),
            parts: 1,
        }
    }

    /// Every part's tensors are listed: as many parts as `split_count`
    /// declares (1 for a file that isn't split).
    pub fn complete(&self, split_count: Option<u64>) -> bool {
        self.parts >= split_count.unwrap_or(1).max(1)
    }
}

impl ModelSummary {
    /// [`Self::tensor_list`] with the tensors of `later`, every later part's
    /// summary in order, taken in ([`Self::of_set`]).
    pub(super) fn take_tensors(&mut self, later: &[&ModelSummary]) {
        for p in later {
            self.tensor_list
                .tensors
                .extend(p.tensor_list.tensors.iter().cloned());
            self.tensor_list.parts += p.tensor_list.parts;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::gguf::{read_header, synth, ModelSummary};

    #[test]
    fn a_split_sets_list_is_every_parts() {
        let dir = tempfile::tempdir().unwrap();
        let read = |h: &synth::Header, name: &str| {
            let path = dir.path().join(name);
            h.write_to(&path);
            ModelSummary::from_meta(&read_header(&path).unwrap())
        };
        let mut p1 = synth::chat("qwen3moe", 4096);
        p1.u32("split.count", 2)
            .tensor_sized("token_embd.weight", 64)
            .tensor_sized("blk.0.ffn_up_exps.weight", 640);
        let mut p2 = synth::Header::default();
        p2.tensor_sized("blk.1.ffn_up_exps.weight", 320);
        let (p1, p2) = (read(&p1, "p1.gguf"), read(&p2, "p2.gguf"));
        assert!(!p1.tensor_list.complete(p1.split_count));
        assert_eq!(p1.tensor_list.tensors.len(), 2);
        let set = p1.of_set(&[&p2]);
        assert!(set.tensor_list.complete(set.split_count));
        assert_eq!(
            set.tensor_list.tensors,
            vec![
                ("token_embd.weight".to_string(), 64),
                ("blk.0.ffn_up_exps.weight".to_string(), 640),
                ("blk.1.ffn_up_exps.weight".to_string(), 320),
            ]
        );
    }
}
