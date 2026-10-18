//! When a catalog price is no price lmgw can apply (decision-models design
//! §6.2): the two cases where writing the catalog's numbers as a `per_mtok`
//! row would price requests wrong downward, which usage-analytics §2.3 calls
//! the worst failure. Such a model is left unpriced: no row, counted in the
//! sync's `unpriced_models`, priced by a manual row if the owner sets one.

use crate::pricing::Prices;

/// The tasks whose catalog `prompt` and `completion` are not prices per
/// token, so a `per_mtok` row made of them would multiply the wrong count.
/// OpenRouter's full list (`?output_modalities=all`) brings all of them:
/// - `tts`: its `prompt` is per input character;
/// - `asr`: its `prompt` is per second of audio (Scribe v2's 0.0000306 is
///   $0.11 an hour), and its answer reports `seconds`;
/// - `image_generation`, `image_edit`, `video_generation`: billed per image,
///   per second or per output image token in fields price sync does not
///   write (`image_output`, `image_token`), most with a `prompt` and
///   `completion` of 0;
/// - `rerank`: billed per search unit (its answer's `usage.search_units`),
///   with a `prompt` and `completion` of 0 in the catalog.
pub(super) const NOT_PER_TOKEN: [&str; 6] = [
    "tts",
    "asr",
    "image_generation",
    "image_edit",
    "video_generation",
    "rerank",
];

/// Whether a model of `task` is priced in a unit its catalog's token fields
/// do not hold ([`NOT_PER_TOKEN`]).
pub(super) fn not_per_token(task: Option<&str>) -> bool {
    task.is_some_and(|t| NOT_PER_TOKEN.contains(&t))
}

/// Whether a free token price hides a charge: `prompt` and `completion` are
/// both 0 while the pricing object publishes a non-zero field that goes into
/// no row (`unsynced`, the fields a sync would count in `not_synced`). A
/// 0/0 row would price such a model's requests at $0 from the catalog, when
/// what they cost sits in the field lmgw does not apply. A model whose every
/// field is 0 is really free and keeps its 0 row.
pub(super) fn free_tokens_hide_a_charge(prices: &Prices, unsynced: &[&str]) -> bool {
    prices.price_in == Some(0.0) && prices.price_out == Some(0.0) && !unsynced.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(price_in: f64, price_out: f64) -> Prices {
        Prices {
            price_in: Some(price_in),
            price_out: Some(price_out),
            ..Default::default()
        }
    }

    #[test]
    fn only_the_tasks_billed_in_other_units_are_not_per_token() {
        for t in NOT_PER_TOKEN {
            assert!(not_per_token(Some(t)), "{t}");
        }
        for t in ["chat", "embedding", "decision"] {
            assert!(!not_per_token(Some(t)), "{t}");
        }
        assert!(!not_per_token(None), "an unstated task is a chat model's");
    }

    #[test]
    fn a_free_token_price_hides_a_charge_only_beside_an_unsynced_field() {
        assert!(free_tokens_hide_a_charge(
            &sheet(0.0, 0.0),
            &["image_output"]
        ));
        assert!(
            !free_tokens_hide_a_charge(&sheet(0.0, 0.0), &[]),
            "really free"
        );
        assert!(
            !free_tokens_hide_a_charge(&sheet(0.042, 0.0), &["web_search"]),
            "a paid token price stays, the field is counted in not_synced"
        );
    }
}
