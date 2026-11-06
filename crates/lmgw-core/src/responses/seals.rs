//! The seals lmgw's own loop wrote, on the native path (openai egress
//! design §7.3.2): a reasoning item the `/v1/responses` loop answered with
//! carries its seal in `encrypted_content` as the carrier
//! `lmgw-openai:<upstream_id>:<config>:<item_id>:<data>`, under an id lmgw
//! minted. A client that sends it back on a request the platform serves
//! natively names the platform's own item: the carrier's item, its row and
//! its encrypted content are what the key check reads ([`named_item`]; WP8
//! review S-R3), and the body the upstream gets has the
//! item as the platform wrote it, its id and its `encrypted_content`, when
//! the seal is that row's as configured now ([`unwrap_for`]). Another row's
//! seal is encrypted for another project, as may be one the row made under
//! another key, and its item is left out, as the loop's body leaves it out
//! (`egress::openai::responses::body`).

use serde_json::Value;

use crate::ir::{encrypted_sha256, Seal, SealScope};
use crate::store::ItemClaim;

/// The seal an input item carries in lmgw's carrier, if it does.
fn carried(item: &Value) -> Option<Seal> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    Seal::from_carrier(item.get("encrypted_content")?.as_str()?)?.ok()
}

/// The stored item an input item names, as it claims it: the platform's
/// item behind a seal lmgw's loop wrote, from the row the seal names and
/// with its data; else the item's own `id`, with the encrypted content a
/// reasoning item carries as OpenAI wrote it.
pub(super) fn named_item(item: &Value) -> Option<ItemClaim> {
    if let Some(seal) = carried(item) {
        return Some(ItemClaim {
            id: seal.item_id,
            upstream_id: Some(seal.upstream_id),
            encrypted_sha256: Some(encrypted_sha256(&seal.data)),
        });
    }
    let id = item.get("id").and_then(Value::as_str)?;
    Some(ItemClaim {
        encrypted_sha256: item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(encrypted_sha256),
        ..ItemClaim::id(id)
    })
}

/// The index of the first input item that is a reasoning item carrying
/// encrypted content as OpenAI wrote it but no `id` (WP8 verify V-N1): no
/// recorded item vouches for it, so its encrypted reasoning could be
/// anyone's, and OpenAI requires the id on a reasoning input item (the create
/// reference's `ResponseReasoningItem`). A client key's is refused
/// (`super::path`); a carrier names its item itself.
pub(super) fn first_without_id(body: &Value) -> Option<usize> {
    body.get("input")?.as_array()?.iter().position(|item| {
        item.get("type").and_then(Value::as_str) == Some("reasoning")
            && carried(item).is_none()
            && item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.is_empty())
            && item
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
    })
}

/// `body`'s input as the upstream row `scope` names takes it, as configured
/// now (module doc).
pub(super) fn unwrap_for(body: &mut Value, scope: &SealScope) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    items.retain_mut(|item| match carried(item) {
        None => true,
        Some(seal) if seal.is_for(scope) => {
            item["id"] = Value::String(seal.item_id);
            item["encrypted_content"] = Value::String(seal.data);
            true
        }
        Some(_) => false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_carried_seal_names_and_becomes_the_platform_s_own_item() {
        let ours = json!({"type": "reasoning", "id": "rs_lmgw1", "summary": [],
                          "encrypted_content": "lmgw-openai:4:cfg4:rs_up:enc"});
        let raw = json!({"type": "reasoning", "id": "rs_raw", "summary": [],
                         "encrypted_content": "gAAAA"});
        assert_eq!(
            named_item(&ours),
            Some(ItemClaim {
                id: "rs_up".into(),
                upstream_id: Some(4),
                encrypted_sha256: Some(encrypted_sha256("enc")),
            })
        );
        assert_eq!(
            named_item(&raw),
            Some(ItemClaim {
                encrypted_sha256: Some(encrypted_sha256("gAAAA")),
                ..ItemClaim::id("rs_raw")
            })
        );
        let mut body = json!({"input": [
            {"role": "user", "content": "hi"},
            ours,
            raw.clone(),
            {"type": "reasoning", "id": "rs_lmgw2", "summary": [],
             "encrypted_content": "lmgw-openai:5:cfg5:rs_other:enc"},
        ]});
        assert_eq!(first_without_id(&body), None);
        unwrap_for(&mut body, &SealScope::new(4, "cfg4"));
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": "hi"},
                {"type": "reasoning", "id": "rs_up", "summary": [], "encrypted_content": "enc"},
                raw,
            ])
        );
    }

    #[test]
    fn a_reasoning_item_with_encrypted_content_and_no_id_is_found() {
        let body = |item: Value| json!({"input": [{"role": "user", "content": "hi"}, item]});
        let bare = json!({"type": "reasoning", "summary": [], "encrypted_content": "gAAA"});
        assert_eq!(first_without_id(&body(bare.clone())), Some(1));
        let mut empty_id = bare.clone();
        empty_id["id"] = json!("");
        assert_eq!(first_without_id(&body(empty_id)), Some(1));
        let carrier = json!({"type": "reasoning", "summary": [],
                             "encrypted_content": "lmgw-openai:4:cfg4:rs_up:enc"});
        assert_eq!(
            first_without_id(&body(carrier)),
            None,
            "a carrier names its item"
        );
        let summary_only = json!({"type": "reasoning", "summary": []});
        assert_eq!(first_without_id(&body(summary_only)), None);
        assert_eq!(first_without_id(&json!({"input": "hi"})), None);
    }
}
