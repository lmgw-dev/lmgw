//! Billable units on the Prices page (billable-units design §8.1): which
//! unit a new row starts in, how a row's unit and rate read in the table,
//! and the body the editor posts for each shape.
//!
//! The units themselves — their spellings, labels and scales — are
//! `lmgw_api_types::PriceUnit`'s one table, shared with the gateway.

use lmgw_api_types::PriceUnit;
use serde_json::{json, Value};

/// The unit a new price row starts in: the one its model's task is billed
/// in where the task is known (`asr` per minute of input audio, `tts` per
/// input character, the image tasks per generated image), tokens otherwise.
pub(super) fn default_unit(task: Option<&str>) -> PriceUnit {
    match task {
        Some("asr") => PriceUnit::PerAudioMinute,
        Some("tts") => PriceUnit::PerMchar,
        Some("image_generation" | "image_edit") => PriceUnit::PerImage,
        _ => PriceUnit::PerMtok,
    }
}

/// `unit` as a row carries it, or `None` for a spelling this build does not
/// know (a newer gateway's unit), which the table shows verbatim.
pub(super) fn parse(unit: &str) -> Option<PriceUnit> {
    PriceUnit::parse(unit)
}

/// What a rate of `unit` is per, short enough for a table cell; the full
/// scale ([`PriceUnit::scale`]) goes in its tooltip.
pub(super) fn rate_suffix(unit: PriceUnit) -> &'static str {
    match unit {
        PriceUnit::PerMtok => "per 1M tokens",
        PriceUnit::PerAudioMinute => "per min",
        PriceUnit::PerMchar => "per 1M chars",
        PriceUnit::PerImage => "per image",
        PriceUnit::PerRequest => "per request",
    }
}

/// The unit picker's options, tokens first.
pub(super) fn unit_options() -> Vec<(String, String)> {
    PriceUnit::ALL
        .iter()
        .map(|u| (u.as_str().to_string(), u.label().to_string()))
        .collect()
}

/// Where a unit sorts among one scope's rows: the table's order, tokens
/// first, so a scope's parts read in the same order everywhere.
pub(super) fn rank(unit: &str) -> usize {
    PriceUnit::ALL
        .iter()
        .position(|u| u.as_str() == unit)
        .unwrap_or(PriceUnit::ALL.len())
}

/// The editor's boxes as `price_set` takes them: the four token rates for
/// `per_mtok`, the one `price` for any other unit — never both, which the
/// op refuses. `id` rides along for the record; the op keys a row on its
/// scope and unit.
pub(super) fn price_body(
    id: i64,
    scope_kind: &str,
    scope_key: &str,
    unit: PriceUnit,
    tokens: [Option<f64>; 4],
    price: Option<f64>,
) -> Value {
    let mut body = json!({
        "id": id,
        "scope_kind": scope_kind,
        "scope_key": scope_key,
        "unit": unit.as_str(),
        "source": "manual",
    });
    if unit.is_tokens() {
        let [p_in, p_out, p_cr, p_cw] = tokens;
        body["price_in"] = json!(p_in);
        body["price_out"] = json!(p_out);
        body["price_cache_read"] = json!(p_cr);
        body["price_cache_write"] = json!(p_cw);
    } else {
        body["price"] = json!(price);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_row_starts_in_the_unit_its_task_is_billed_in() {
        assert_eq!(default_unit(Some("asr")), PriceUnit::PerAudioMinute);
        assert_eq!(default_unit(Some("tts")), PriceUnit::PerMchar);
        assert_eq!(default_unit(Some("image_generation")), PriceUnit::PerImage);
        assert_eq!(default_unit(Some("image_edit")), PriceUnit::PerImage);
        assert_eq!(default_unit(Some("chat")), PriceUnit::PerMtok);
        assert_eq!(default_unit(Some("embedding")), PriceUnit::PerMtok);
        assert_eq!(default_unit(None), PriceUnit::PerMtok);
    }

    #[test]
    fn the_body_carries_one_shape_never_both() {
        let t = price_body(
            7,
            "alias",
            "a",
            PriceUnit::PerMtok,
            [Some(3.0), Some(15.0), None, None],
            Some(1.0),
        );
        assert_eq!(t["unit"], "per_mtok");
        assert_eq!(t["price_in"], 3.0);
        assert!(t.get("price").is_none(), "{t}");

        let u = price_body(
            0,
            "alias",
            "whisper",
            PriceUnit::PerAudioMinute,
            [Some(3.0), None, None, None],
            Some(0.006),
        );
        assert_eq!(u["unit"], "per_audio_minute");
        assert_eq!(u["price"], 0.006);
        assert!(u.get("price_in").is_none(), "{u}");
    }

    #[test]
    fn a_scope_s_rows_sort_tokens_first_and_an_unknown_unit_last() {
        assert!(rank("per_mtok") < rank("per_request"));
        assert_eq!(rank("per_video_second"), PriceUnit::ALL.len());
    }
}
