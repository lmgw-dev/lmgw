//! What the live server says about itself (benchmark design §2.1, §3.4):
//! `/props` and `/slots`, read in both engines' shapes.
//!
//! Verified 2026-09-29 with Qwen3.5-0.8B at `-c 8192 --parallel 2`:
//!
//! | | official (`b11226`) | ik_llama (`7ff619c`) |
//! |---|---|---|
//! | `/props` `total_slots` | 2 | 2 |
//! | `/props` `default_generation_settings.n_ctx` | 4096 (per slot) | 4096 (per slot) |
//! | `/props` top-level `n_ctx` | absent | 8192 (the whole context) |
//! | `/props` `build_info`, `model_ftype` | present | absent |
//! | `/props` `chat_template_caps` | booleans | `{}` |
//! | `/slots` | `id`, `n_ctx`, `speculative`, `is_processing` | `id`, `n_ctx`, `state`, sampling |
//!
//! *S* is read from `/slots` first (the per-slot figure every engine
//! reports there, and the smallest one if slots differ), then from
//! `default_generation_settings.n_ctx`, and only then from the whole context
//! divided by the slot count.

use std::collections::BTreeMap;

use lmgw_api_types::bench::ServerFacts;
use serde_json::Value;

/// Read [`ServerFacts`] out of `/props` and (when the server serves it)
/// `/slots`. `Err` when neither says how many slots or how much context
/// there is — every point depends on both.
pub fn server_facts(props: &Value, slots: Option<&Value>) -> Result<ServerFacts, String> {
    let slot_list = slots.and_then(Value::as_array).filter(|s| !s.is_empty());

    let n_slots = props
        .get("total_slots")
        .and_then(Value::as_u64)
        .or_else(|| slot_list.map(|s| s.len() as u64))
        .filter(|n| *n > 0)
        .ok_or("neither /props total_slots nor /slots says how many slots the server has")?;
    let n_slots = u32::try_from(n_slots).map_err(|_| format!("{n_slots} slots"))?;

    let total_ctx = props.get("n_ctx").and_then(Value::as_u64);
    let from_slots = slot_list.and_then(|s| {
        s.iter()
            .map(|slot| slot.get("n_ctx").and_then(Value::as_u64))
            .collect::<Option<Vec<u64>>>()
            .and_then(|v| v.into_iter().min())
    });
    let from_dgs = props
        .pointer("/default_generation_settings/n_ctx")
        .and_then(Value::as_u64);
    let (per_slot_ctx, source) = match (from_slots, from_dgs, total_ctx) {
        (Some(s), ..) => (s, "slots"),
        (None, Some(s), _) => (s, "props.default_generation_settings.n_ctx"),
        (None, None, Some(t)) => (t / n_slots as u64, "props.n_ctx / total_slots"),
        _ => return Err("neither /slots nor /props says how large a slot's context is".into()),
    };
    if per_slot_ctx == 0 {
        return Err(format!(
            "the server reports a per-slot context of 0 ({source})"
        ));
    }

    let str_field = |k: &str| {
        props
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let chat_template_caps: BTreeMap<String, bool> = props
        .get("chat_template_caps")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();
    let speculative = slot_list.and_then(|s| {
        let flags: Vec<bool> = s
            .iter()
            .filter_map(|slot| slot.get("speculative").and_then(Value::as_bool))
            .collect();
        (!flags.is_empty()).then(|| flags.contains(&true))
    });

    // ik reports the whole context beside the per-slot one: slots that
    // together claim more than the whole share it. Official llama.cpp says
    // nothing either way; the run then takes the row's settings.
    let kv_unified = total_ctx
        .filter(|_| n_slots > 1 && source != "props.n_ctx / total_slots")
        .map(|t| per_slot_ctx.saturating_mul(u64::from(n_slots)) > t);

    Ok(ServerFacts {
        n_slots,
        per_slot_ctx,
        per_slot_ctx_source: source.to_string(),
        total_ctx,
        kv_unified,
        kv_unified_source: kv_unified.map(|_| "props.n_ctx".to_string()),
        build_info: str_field("build_info"),
        model_ftype: str_field("model_ftype"),
        speculative,
        chat_template_caps,
        vision: props.pointer("/modalities/vision").and_then(Value::as_bool),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Trimmed from the official build's real answers (§2.1).
    fn official() -> (Value, Value) {
        (
            json!({
                "default_generation_settings": {"params": {}, "n_ctx": 4096},
                "total_slots": 2,
                "model_ftype": "Q4_K - Medium",
                "modalities": {"vision": true, "video": true, "audio": false},
                "chat_template_caps": {"supports_tools": true, "supports_preserve_reasoning": false},
                "build_info": "b11226-0c6a6a7"
            }),
            json!([
                {"id": 0, "n_ctx": 4096, "speculative": false, "is_processing": false},
                {"id": 1, "n_ctx": 4096, "speculative": false, "is_processing": false}
            ]),
        )
    }

    /// Trimmed from ik_llama.cpp's real answers (§2.1).
    fn ik() -> (Value, Value) {
        (
            json!({
                "default_generation_settings": {"n_ctx": 4096, "n_predict": -1},
                "total_slots": 2,
                "chat_template_caps": {},
                "modalities": {"vision": false, "audio": false},
                "n_ctx": 8192
            }),
            json!([
                {"n_ctx": 4096, "id": 0, "state": 0},
                {"n_ctx": 4096, "id": 1, "state": 0}
            ]),
        )
    }

    #[test]
    fn official_shape() {
        let (p, s) = official();
        let f = server_facts(&p, Some(&s)).unwrap();
        assert_eq!((f.n_slots, f.per_slot_ctx), (2, 4096));
        assert_eq!(f.per_slot_ctx_source, "slots");
        assert_eq!(f.total_ctx, None);
        assert_eq!(f.build_info.as_deref(), Some("b11226-0c6a6a7"));
        assert_eq!(f.model_ftype.as_deref(), Some("Q4_K - Medium"));
        assert_eq!(f.speculative, Some(false));
        assert_eq!(f.kv_unified, None, "official does not say");
        assert_eq!(f.chat_template_caps.get("supports_tools"), Some(&true));
        assert_eq!(f.vision, Some(true));
        // Without /slots the per-slot figure comes from default_generation_settings.
        let f = server_facts(&p, None).unwrap();
        assert_eq!(f.per_slot_ctx, 4096);
        assert_eq!(
            f.per_slot_ctx_source,
            "props.default_generation_settings.n_ctx"
        );
    }

    #[test]
    fn ik_shape() {
        let (p, s) = ik();
        let f = server_facts(&p, Some(&s)).unwrap();
        assert_eq!(
            (f.n_slots, f.per_slot_ctx, f.total_ctx),
            (2, 4096, Some(8192))
        );
        assert_eq!(f.build_info, None);
        assert_eq!(f.model_ftype, None);
        assert_eq!(f.speculative, None);
        // 2 × 4096 per slot within 8192: split.
        assert_eq!(f.kv_unified, Some(false));
        assert_eq!(f.kv_unified_source.as_deref(), Some("props.n_ctx"));
        // Every slot reporting the whole context: one shared pool.
        let shared = json!([{"n_ctx": 8192}, {"n_ctx": 8192}]);
        let f = server_facts(&p, Some(&shared)).unwrap();
        assert_eq!(f.kv_unified, Some(true));
        assert!(f.chat_template_caps.is_empty());
        assert_eq!(f.vision, Some(false));
        // Only the top-level whole context: divided by the slot count.
        let f = server_facts(&json!({"total_slots": 4, "n_ctx": 8192}), None).unwrap();
        assert_eq!(f.per_slot_ctx, 2048);
        assert_eq!(f.per_slot_ctx_source, "props.n_ctx / total_slots");
    }

    #[test]
    fn the_smallest_slot_wins_and_missing_facts_are_errors() {
        let s = json!([{"n_ctx": 4096}, {"n_ctx": 2048}]);
        let f = server_facts(&json!({}), Some(&s)).unwrap();
        assert_eq!((f.n_slots, f.per_slot_ctx), (2, 2048));
        assert!(server_facts(&json!({}), None).is_err());
        assert!(server_facts(&json!({"total_slots": 1}), None).is_err());
        assert!(server_facts(&json!({"total_slots": 1, "n_ctx": 0}), None).is_err());
    }
}
