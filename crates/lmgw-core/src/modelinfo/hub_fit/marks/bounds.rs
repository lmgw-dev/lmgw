//! The bounds that decide which headers the file list reads (find-a-model
//! design D60 to D62, D67): the order the reference is tried in, the most a
//! file's host-kept input embeddings could weigh, and whether a file could
//! be D7's pick at all by its bytes alone.
//!
//! Each bound only ever widens what is read: a file a bound leaves unread
//! is one whose own header could not change its mark or the pick.

use lmgw_api_types::model_find::FitMeasure;

use crate::hf;

use super::super::super::fit::CardNow;
use super::super::super::pick;
use super::super::super::plan_files::hub::Listed;
use super::super::super::plan_files::PlanFiles;

/// The widest a weight is stored in, in bytes per element: F32 (D61).
pub(super) const WIDEST_BYTES: u64 = 4;

/// The width a quant label names, in whole bits, for the reference's order
/// only (D67): `Q4_K_M`, `IQ4_XS`, `UD-Q4_K_XL` and `MXFP4_MOE` are 4,
/// `BF16` and `F16` 16, `F32` 32. `None` for a label that names none.
pub(super) fn label_bits(label: &str) -> Option<u32> {
    let up = label.to_ascii_uppercase();
    let up = up.trim_start_matches("UD-");
    if up.starts_with("BF16") || up.starts_with("F16") {
        return Some(16);
    }
    if up.starts_with("F32") {
        return Some(32);
    }
    let after = |marker: &str| {
        up.find(marker).and_then(|i| {
            let n: String = up[i + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            n.parse().ok()
        })
    };
    after("Q").or_else(|| after("FP"))
}

/// Whether `file`'s quant label names a width D7's pick takes (4 to 8.5
/// bits): the reference is tried among those first (D67).
fn in_pick_range(file: &str) -> bool {
    hf::quant_label(file)
        .as_deref()
        .and_then(label_bits)
        .is_some_and(|b| f64::from(b) >= pick::FLOOR_BITS && f64::from(b) <= pick::CAP_BITS)
}

/// The order the reference is tried in among `indices` of `weights` (D60,
/// D67): the files whose listed bytes fit beside what runs now, then those
/// that fit the whole card, then the rest; within the first two the files
/// whose label names a width the pick takes first, then the largest; within
/// the rest the smallest. Files without bytes aren't tried.
pub(super) fn reference_order(
    card: &CardNow,
    weights: &[String],
    listed: &[Listed],
    indices: &[usize],
) -> Vec<usize> {
    let fits_in = |b: u64, m| b.saturating_add(card.card.headroom) <= card.room(m);
    let tier = |b: u64| match (
        fits_in(b, FitMeasure::Beside),
        fits_in(b, FitMeasure::WholeCard),
    ) {
        (true, _) => 0,
        (false, true) => 1,
        _ => 2,
    };
    let mut known: Vec<(usize, u64)> = indices
        .iter()
        .filter_map(|&i| listed[i].bytes.map(|b| (i, b)))
        .collect();
    known.sort_by(|(ia, a), (ib, b)| {
        let (ta, tb) = (tier(*a), tier(*b));
        ta.cmp(&tb)
            .then_with(|| match ta {
                2 => a.cmp(b),
                _ => (!in_pick_range(&weights[*ia]))
                    .cmp(&!in_pick_range(&weights[*ib]))
                    .then_with(|| b.cmp(a)),
            })
            .then_with(|| weights[*ia].cmp(&weights[*ib]))
    });
    known.into_iter().map(|(i, _)| i).collect()
}

/// The most the host-kept input embeddings of a file of `bytes`, of the
/// model `r` was read from, could weigh (D61): their elements at 32 bits,
/// never more than the file; the file's bytes when their count isn't known.
pub(super) fn host_bound(r: &PlanFiles, bytes: u64) -> u64 {
    r.host_input_elements
        .map_or(bytes, |e| e.saturating_mul(WIDEST_BYTES))
        .min(bytes)
}

/// [`host_bound`] for a file that could be the pick (D67): one of at least
/// D7's 4 bits per weight over the model's quantized layer elements holds
/// at least half a byte for each of them, which its host-kept embeddings
/// aren't part of.
pub(super) fn host_bound_for_pick(r: &PlanFiles, bytes: u64) -> u64 {
    let floor_bytes = r
        .layer_elements
        .map_or(0, |l| (l as f64 * pick::FLOOR_BITS / 8.0) as u64);
    host_bound(r, bytes).min(bytes.saturating_sub(floor_bytes))
}

/// Whether a file of `bytes` can be D7's 4 bits per weight wide over the
/// model's `layers` elements (D62): its layers' bytes are at most the whole
/// file's. Unknown elements can't rule it out.
pub(super) fn may_reach_floor(bytes: u64, layers: Option<u64>) -> bool {
    layers.is_none_or(|l| l == 0 || bytes as f64 * 8.0 / l as f64 >= pick::FLOOR_BITS)
}

/// Whether a file of `bytes` of the model `r` was read from can be at most
/// D7's 8.5 bits per weight wide over its quantized layers (D67). Its
/// layers' bytes are at least its bytes less what else a file of the model
/// holds: every other parameter at 32 bits (F32 being the widest a weight
/// is written in), and the headers and their padding, taken at twice the
/// reference's own (the same model's headers differ only by a few keys).
/// Unknown counts can't rule it out.
pub(super) fn may_be_under_cap(bytes: u64, r: &PlanFiles) -> bool {
    under_cap(bytes, r.parameters, r.layer_elements, r.overhead_bytes)
}

fn under_cap(bytes: u64, params: Option<u64>, layers: Option<u64>, overhead: u64) -> bool {
    let (Some(params), Some(layers)) = (params, layers) else {
        return true;
    };
    if layers == 0 || params < layers {
        return true;
    }
    let others = (params - layers).saturating_mul(WIDEST_BYTES);
    let layer_bytes = bytes
        .saturating_sub(overhead.saturating_mul(2))
        .saturating_sub(others);
    layer_bytes as f64 * 8.0 / layers as f64 <= pick::CAP_BITS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_name_their_width() {
        assert_eq!(label_bits("Q4_K_M"), Some(4));
        assert_eq!(label_bits("UD-Q4_K_XL"), Some(4));
        assert_eq!(label_bits("IQ4_XS"), Some(4));
        assert_eq!(label_bits("MXFP4_MOE"), Some(4));
        assert_eq!(label_bits("Q8_0"), Some(8));
        assert_eq!(label_bits("UD-IQ1_M"), Some(1));
        assert_eq!(label_bits("BF16"), Some(16));
        assert_eq!(label_bits("F32"), Some(32));
        assert_eq!(label_bits("kquant"), None);
        assert!(in_pick_range("x/m-Q8_0.gguf"));
        assert!(!in_pick_range("x/m-Q3_K_M.gguf"));
        assert!(!in_pick_range("x/BF16/m-BF16-00001-of-00002.gguf"));
    }

    #[test]
    fn a_file_under_four_bits_by_its_bytes_alone_is_not_read_for_the_pick() {
        // 16 Mi layer elements: 8 MiB is exactly 4 bits a weight.
        let layers = Some(16 << 20);
        assert!(may_reach_floor(8 << 20, layers));
        assert!(!may_reach_floor((8 << 20) - 1, layers));
        assert!(
            may_reach_floor(1, None),
            "unknown elements rule nothing out"
        );
        assert!(may_reach_floor(1, Some(0)));
    }

    #[test]
    fn a_file_wider_than_the_cap_by_its_bytes_alone_is_not_read_for_the_pick() {
        const GB: u64 = 1_000_000_000;
        // Qwen3-32B's shape: 31.2 G quantized layer elements of 32.8 G; a
        // header of 11 MB.
        let (p, l, h) = (Some(32_800_000_000), Some(31_200_000_000), 11_000_000);
        // A Q8_0 (34.8 GB) and its whole BF16 (65.5 GB).
        assert!(under_cap(34_800_000_000, p, l, h));
        assert!(!under_cap(65_500_000_000, p, l, h));
        // A small Q8_0 whose every other tensor is F32, its header as long
        // as the reference's: never ruled out; 2 MB more is.
        let (p, l, h) = (Some(1_100_000), Some(1_000_000), 700_000);
        let q8 = h + 1_000_000 * 85 / 80 + 100_000 * 4;
        assert!(under_cap(q8, p, l, h));
        assert!(!under_cap(q8 + 2 * GB / 1000, p, l, h));
        // Unknown counts rule nothing out.
        assert!(under_cap(100 * GB, None, l, h));
        assert!(under_cap(100 * GB, p, None, h));
    }
}
