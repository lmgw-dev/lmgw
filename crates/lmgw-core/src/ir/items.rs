//! What an upstream calls one answer and its output items (openai egress
//! design §7.2.3): a Responses-wire answer's response id and its `output`
//! items (a reasoning item's `rs_…`, a message's `msg_…`, a call's `fc_…`).
//! A later request may name an item by its id, and only a key that was
//! given it may (§6.1, §7.3), so each is recorded under the key of the
//! request that got it, a reasoning item with a hash of its
//! `encrypted_content` (WP8 review S-R3): a key's own id with another
//! item's encrypted reasoning behind it is not the item it was given.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One answer's response id and its output items.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamItems {
    pub response_id: String,
    pub items: Vec<UpstreamItem>,
}

/// One output item as it is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamItem {
    pub id: String,
    /// [`encrypted_sha256`] of the item's `encrypted_content`, for an item
    /// that carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_sha256: Option<String>,
}

impl UpstreamItem {
    /// An item without encrypted content.
    pub fn plain(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            encrypted_sha256: None,
        }
    }

    /// `item`, an output item as the upstream wrote it; `None` for one
    /// without an id, which names nothing.
    pub fn of(item: &Value) -> Option<Self> {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty())?;
        Some(Self {
            id: id.to_string(),
            encrypted_sha256: item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(encrypted_sha256),
        })
    }
}

/// The SHA-256 (hex) of an item's `encrypted_content`, as recorded and as a
/// returned item is checked against it.
pub fn encrypted_sha256(data: &str) -> String {
    hex::encode(Sha256::digest(data.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_item_is_its_id_and_its_encrypted_content_s_hash() {
        let rs = json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAA"});
        let item = UpstreamItem::of(&rs).unwrap();
        assert_eq!(item.id, "rs_1");
        assert_eq!(
            item.encrypted_sha256.as_deref(),
            Some(encrypted_sha256("gAAA").as_str())
        );
        assert_ne!(encrypted_sha256("gAAA"), encrypted_sha256("gAAB"));
        assert_eq!(
            UpstreamItem::of(&json!({"type": "message", "id": "msg_1"})),
            Some(UpstreamItem::plain("msg_1"))
        );
        assert_eq!(
            UpstreamItem::of(&json!({"type": "message", "id": ""})),
            None
        );
    }
}
