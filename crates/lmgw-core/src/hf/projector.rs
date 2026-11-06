//! The projector rule (find-a-model design D121, bug H1): one rule for every
//! place that picks a vision projector for a weights file.
//!
//! A GGUF repo with several quants of a multimodal model often ships several
//! projectors beside them: `unsloth/Qwen3.6-27B-GGUF` has `mmproj-BF16.gguf`
//! and `mmproj-F16.gguf` (0.93 GB each) and `mmproj-F32.gguf` (1.84 GB).
//! llama-server loads one. `hf_add` used to fetch all three and the disk plan
//! took whichever its directory scan listed first. Now both, and the hub plan
//! and Get it after them, ask this rule with the same two facts: each
//! candidate's name and its bytes (from the tree listing or the disk).
//!
//! **The rule:** the smallest projector whose quant label's width is F16 or
//! BF16, else the smallest of them all; equal sizes by name, so the pick is
//! stable. The width is the label's last word ([`width`]): a name like
//! `mmproj-Quux-VL-8B-Instruct-F16.gguf` gives [`super::quant_label`] the
//! whole `Demo-VL-8B-Instruct-F16` (its `-Q…` marker search finds the model
//! name first), and its last word is still `F16` (D41).

/// The width word of a projector's name: the last `-` word of its quant
/// label, uppercased (`F16`, `BF16`, `F32`, `Q8_0`). `None` when the name
/// carries no label.
pub fn width(path: &str) -> Option<String> {
    let label = super::quant_label(path)?;
    let word = label.rsplit('-').next().unwrap_or(&label);
    Some(word.to_ascii_uppercase())
}

/// Whether a projector's name says it is 16-bit (F16 or BF16).
pub fn is_half_width(path: &str) -> bool {
    matches!(width(path).as_deref(), Some("F16" | "BF16"))
}

/// The projector to use among `candidates`, each its path and its bytes
/// (module doc). A candidate whose size isn't known passes `u64::MAX`, so it
/// is never the smaller one. `None` with no candidate.
pub fn pick_projector<'a, I>(candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = (&'a str, u64)>,
{
    candidates
        .into_iter()
        .min_by(|a, b| {
            // Half width first, then the smaller, then the name.
            (!is_half_width(a.0), a.1, a.0).cmp(&(!is_half_width(b.0), b.1, b.0))
        })
        .map(|(path, _)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three projectors of a recorded listing (sizes as the hub listed
    /// them for `unsloth/Qwen3.6-27B-GGUF` on 2026-11-04, names faked): F16
    /// and BF16 differ by 3.5 MB, and the smaller of the two is taken; the
    /// F32 is never, though it is listed first.
    #[test]
    fn the_smallest_of_f16_and_bf16_is_picked_over_a_wider_one() {
        let listing = [
            ("demo/mmproj-F32.gguf", 1_842_940_480),
            ("demo/mmproj-BF16.gguf", 931_146_304),
            ("demo/mmproj-F16.gguf", 927_607_360),
        ];
        assert_eq!(pick_projector(listing), Some("demo/mmproj-F16.gguf"));
        // Equal sizes: the name decides, whichever order they came in.
        let tied = [("b/mmproj-F16.gguf", 10), ("b/mmproj-BF16.gguf", 10)];
        assert_eq!(pick_projector(tied), Some("b/mmproj-BF16.gguf"));
        let tied = [("b/mmproj-BF16.gguf", 10), ("b/mmproj-F16.gguf", 10)];
        assert_eq!(pick_projector(tied), Some("b/mmproj-BF16.gguf"));
    }

    /// Without a 16-bit one, the smallest; one projector is itself; none is
    /// none; an unknown size is never the smaller.
    #[test]
    fn without_a_16_bit_projector_the_smallest_is_picked() {
        let listing = [("mmproj-F32.gguf", 1_800), ("mmproj-Q8_0.gguf", 500)];
        assert_eq!(pick_projector(listing), Some("mmproj-Q8_0.gguf"));
        assert_eq!(
            pick_projector([("mmproj-F32.gguf", 1_800)]),
            Some("mmproj-F32.gguf")
        );
        assert_eq!(pick_projector(std::iter::empty::<(&str, u64)>()), None);
        let unknown = [("mmproj-Q8_0.gguf", u64::MAX), ("mmproj-F32.gguf", 9)];
        assert_eq!(pick_projector(unknown), Some("mmproj-F32.gguf"));
        // A name with no label at all is a candidate like any other.
        let plain = [("mmproj.gguf", 300), ("mmproj-F32.gguf", 900)];
        assert_eq!(pick_projector(plain), Some("mmproj.gguf"));
    }

    /// The width is the label's last word, whatever the label swallowed
    /// before it, and case doesn't matter.
    #[test]
    fn the_width_is_the_labels_last_word() {
        assert_eq!(width("mmproj-F16.gguf").as_deref(), Some("F16"));
        assert_eq!(width("mmproj-BF16.gguf").as_deref(), Some("BF16"));
        assert_eq!(
            width("mmproj-Quux-VL-8B-Instruct-F16.gguf").as_deref(),
            Some("F16")
        );
        assert_eq!(
            width("mmproj-demo_model-12b-it-f16.gguf").as_deref(),
            Some("F16")
        );
        assert_eq!(width("mmproj-demo-4b-Q8_0.gguf").as_deref(), Some("Q8_0"));
        assert_eq!(width("mmproj.gguf"), None);
        assert!(is_half_width("x/mmproj-model-bf16.gguf"));
        assert!(!is_half_width("x/mmproj-F32.gguf"));
    }
}
