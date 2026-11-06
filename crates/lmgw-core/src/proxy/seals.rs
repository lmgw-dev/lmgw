//! A client's seals are checked like a native item id (openai egress design
//! §7.3.2, §6.1): a reasoning seal a client sends back (`/v1/messages`'
//! thinking `signature`, the `/v1/responses` loop's `encrypted_content`)
//! goes upstream only when it is an item the client's key was given, as it
//! was given it.
//!
//! The owner key passes. For a client key the seal's item must be among the
//! key's own (`native_response_items`, which every platform answer to that
//! key filled, `proxy::answer_items`), recorded from the very row the seal
//! names (`native_responses.upstream_id`), and with the encrypted content
//! the seal carries (one of the item's recorded hashes, migrations 0096 and
//! 0100; WP8 review S-R3), otherwise the request is refused with
//! `no_stored_response` exactly as the native path refuses a foreign item:
//! a leaked `rs_…` id
//! with forged content, or another turn's encrypted reasoning behind the
//! key's own id, must not pull the owner's items into a client's prompt,
//! and dropping the seal instead would hide that the request carried
//! something it may not use (decided 2026-10-24, §7.3.4). A request without
//! a key (auth off) reads the items recorded without one, as on the native
//! path.

use crate::error::GatewayError;
use crate::ir::Message;
use crate::store::{ItemClaim, ResponseReader};

/// What every seal in `messages` claims, in order.
fn sealed_items(messages: &[Message]) -> Vec<ItemClaim> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| p.seal())
        .map(|s| ItemClaim {
            id: s.item_id.clone(),
            upstream_id: Some(s.upstream_id),
            encrypted_sha256: Some(crate::ir::encrypted_sha256(&s.data)),
        })
        .collect()
}

/// `Err(no_stored_response)` naming the first seal in `messages` that
/// `reader` was not given as it is (module doc); `Ok` for none.
pub(crate) async fn check(
    db: &sqlx::SqlitePool,
    reader: ResponseReader,
    messages: &[Message],
) -> Result<(), GatewayError> {
    let ResponseReader::Key(key_id) = reader else {
        return Ok(());
    };
    let items = sealed_items(messages);
    match crate::store::first_item_not_of(db, &items, key_id)
        .await
        .map_err(|e| GatewayError::Internal(format!("looking up sealed items: {e}")))?
    {
        Some(id) => Err(crate::responses::no_stored_response(&id)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ContentPart, Role, Seal, SealScope};

    #[test]
    fn the_sealed_items_are_read_in_order_with_their_row_and_content() {
        let scope = SealScope::new(1, "cfg");
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::sealed_reasoning("a", Seal::openai(&scope, "rs_1", "e")),
                ContentPart::reasoning("plain"),
                ContentPart::text("t"),
                ContentPart::sealed_reasoning("", Seal::openai(&scope, "rs_2", "f")),
            ],
        };
        let claims = sealed_items(&[m]);
        assert_eq!(
            claims.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["rs_1", "rs_2"]
        );
        assert_eq!(claims[0].upstream_id, Some(1));
        assert_eq!(
            claims[1].encrypted_sha256,
            Some(crate::ir::encrypted_sha256("f"))
        );
    }
}
