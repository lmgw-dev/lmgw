//! A reasoning item's seal (openai egress design §7.3.1, decision 17): what
//! lets a later turn of the same conversation give the model its earlier
//! reasoning back when the upstream keeps nothing between turns. The OpenAI
//! platform's Responses wire answers with `store: false` (lmgw's own sends,
//! §7.2.2) and puts the reasoning, encrypted for the organization that made
//! it, in each `reasoning` item's `encrypted_content`.
//!
//! A seal rides on its [`ContentPart::Reasoning`](super::ContentPart) part,
//! one part per reasoning item, so a turn that reasoned between tool calls
//! keeps each item where it stood. It goes back only to the upstream row it
//! came from (`upstream_id`, decided 2026-10-24, §7.3.4), and only while
//! that row keeps the configuration it answered with (`config`,
//! [`SealScope`], WP8 review W-R2): another project's key cannot read it.
//! Every egress sends only its own provider's seals.
//!
//! Where a client wire has no field of OpenAI's for it, the seal travels in
//! one the client treats as opaque, as [`Seal::carrier`]:
//! `lmgw-openai:<upstream_id>:<config>:<item_id>:<data>`. That is the
//! `signature` of a `/v1/messages` thinking block (decision 17's prefix, as
//! Gemini's thought signatures ride inside tool-call ids, `ir/call_id.rs`),
//! and the `encrypted_content` of a reasoning item lmgw's own
//! `/v1/responses` loop wrote (§7.3.2). [`Seal::from_carrier`] reads it back; a client's seal is
//! checked against the items its key was given before it is sent anywhere
//! (`proxy::seals`).

use serde::{Deserialize, Serialize};

mod key;
mod scope;
mod unsealed;
pub use key::{install as install_seal_key, SEAL_KEY_LEN};
pub use scope::{config_fingerprint, SealScope};
pub use unsealed::{strip_seals, unsealed_record};

/// Who made a seal, and so the one egress that may send it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SealBy {
    /// The OpenAI platform's `reasoning` item, `encrypted_content`.
    Openai,
}

/// One reasoning item's seal (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seal {
    pub by: SealBy,
    /// The `upstreams.id` of the row that answered with it: the only row it
    /// is sent back to. `0` only on a decoder no send site made
    /// (`Egress::new_decoder` in a test), which matches no row.
    pub upstream_id: i64,
    /// The fingerprint of that row's configuration when it answered
    /// ([`config_fingerprint`]): the seal goes back only while the row
    /// still has it.
    pub config: String,
    /// The reasoning item's own id (`rs_…`), as the upstream named it.
    pub item_id: String,
    /// The item's `encrypted_content`, verbatim.
    pub data: String,
}

/// The prefix of an OpenAI seal in a field the client treats as opaque.
pub const OPENAI_CARRIER_PREFIX: &str = "lmgw-openai:";

impl Seal {
    /// An OpenAI seal from the row `scope` names, as it was configured.
    pub fn openai(scope: &SealScope, item_id: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            by: SealBy::Openai,
            upstream_id: scope.upstream_id,
            config: scope.config.clone(),
            item_id: item_id.into(),
            data: data.into(),
        }
    }

    /// Whether the platform egress sends this seal to the row `scope`: an
    /// OpenAI seal of that row, made while it had the configuration it has
    /// now (module doc).
    pub fn is_for(&self, scope: &SealScope) -> bool {
        self.by == SealBy::Openai
            && self.upstream_id == scope.upstream_id
            && self.config == scope.config
    }

    /// The seal as one opaque string (module doc):
    /// `lmgw-openai:<upstream_id>:<config>:<item_id>:<data>`.
    pub fn carrier(&self) -> String {
        match self.by {
            SealBy::Openai => format!(
                "{OPENAI_CARRIER_PREFIX}{}:{}:{}:{}",
                self.upstream_id, self.config, self.item_id, self.data
            ),
        }
    }

    /// A [`carrier`](Self::carrier) string read back. `None` for a string
    /// without the prefix (Anthropic's own signature, OpenAI's own
    /// `encrypted_content`), `Some(Err)` for one with the prefix that does
    /// not read: a carrier that was cut or edited, which is refused rather
    /// than sent on as something it is not.
    pub fn from_carrier(s: &str) -> Option<Result<Self, String>> {
        let rest = s.strip_prefix(OPENAI_CARRIER_PREFIX)?;
        let read = || {
            let (upstream, rest) = rest.split_once(':')?;
            let (config, rest) = rest.split_once(':')?;
            let (item_id, data) = rest.split_once(':')?;
            let upstream_id = upstream.parse::<i64>().ok().filter(|id| *id > 0)?;
            (!config.is_empty() && !item_id.is_empty() && !data.is_empty())
                .then(|| Self::openai(&SealScope::new(upstream_id, config), item_id, data))
        };
        Some(read().ok_or_else(|| {
            format!(
                "a reasoning seal that does not read: it starts with '{OPENAI_CARRIER_PREFIX}' \
                 but is not '{OPENAI_CARRIER_PREFIX}<upstream id>:<config>:<item id>:<data>' \
                 as this gateway wrote it — send the block back exactly as it came"
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::ContentPart;

    fn sc(upstream_id: i64) -> SealScope {
        SealScope::new(upstream_id, "c0ffee")
    }

    #[test]
    fn a_seal_reads_back_from_its_carrier() {
        // `encrypted_content` is base64 with `=` padding and no `:`; the
        // item id has none either.
        let s = Seal::openai(&sc(5), "rs_0a1b", "gAAAAABo+x/y==");
        let c = s.carrier();
        assert_eq!(c, "lmgw-openai:5:c0ffee:rs_0a1b:gAAAAABo+x/y==");
        assert_eq!(Seal::from_carrier(&c), Some(Ok(s.clone())));
        // Whatever follows the item id is the data, verbatim.
        assert_eq!(
            Seal::from_carrier("lmgw-openai:5:c0ffee:rs_1:a:b"),
            Some(Ok(Seal::openai(&sc(5), "rs_1", "a:b")))
        );
        // It goes back to its row only while the row keeps its configuration.
        assert!(s.is_for(&sc(5)));
        assert!(!s.is_for(&sc(6)));
        assert!(!s.is_for(&SealScope::new(5, "another")));
    }

    #[test]
    fn a_string_without_the_prefix_is_no_carrier_and_a_broken_one_is_refused() {
        assert_eq!(Seal::from_carrier("EqQBCkgIARABGAIiQL"), None);
        for broken in [
            "lmgw-openai:",
            "lmgw-openai:5",
            "lmgw-openai:5:c0ffee:rs_1",
            "lmgw-openai:5:rs_1:data",
            "lmgw-openai:x:c0ffee:rs_1:data",
            "lmgw-openai:0:c0ffee:rs_1:data",
            "lmgw-openai:5::rs_1:data",
            "lmgw-openai:5:c0ffee::data",
            "lmgw-openai:5:c0ffee:rs_1:",
        ] {
            assert!(
                matches!(Seal::from_carrier(broken), Some(Err(_))),
                "{broken}"
            );
        }
    }

    /// A replayed plain reply's later sealed parts carry no text (its whole
    /// trace is on the first): the trace a `reasoning_content` upstream gets
    /// has no stray line break for them (WP8 review W-N2).
    #[test]
    fn a_sealed_part_without_text_adds_nothing_to_the_trace() {
        let m = crate::ir::Message {
            role: crate::ir::Role::Assistant,
            content: vec![
                ContentPart::sealed_reasoning("Whole trace.", Seal::openai(&sc(1), "rs_1", "e")),
                ContentPart::sealed_reasoning("", Seal::openai(&sc(1), "rs_2", "e")),
                ContentPart::text("Answer."),
            ],
        };
        assert_eq!(m.reasoning_text(), "Whole trace.");
    }

    /// The part round-trips through serde with its seal, and a part without
    /// one serializes as it did before seals existed.
    #[test]
    fn a_sealed_reasoning_part_round_trips_and_an_unsealed_one_is_unchanged() {
        let sealed = ContentPart::Reasoning {
            text: "thinking".into(),
            signature: None,
            sealed: Some(Seal::openai(&sc(3), "rs_x", "enc")),
            signature_scope: None,
        };
        let v = serde_json::to_value(&sealed).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "reasoning", "text": "thinking", "sealed": {
                "by": "openai", "upstream_id": 3, "config": "c0ffee", "item_id": "rs_x",
                "data": "enc"}})
        );
        assert_eq!(serde_json::from_value::<ContentPart>(v).unwrap(), sealed);
        let plain = ContentPart::reasoning("t");
        let v = serde_json::to_value(&plain).unwrap();
        assert_eq!(v, serde_json::json!({"type": "reasoning", "text": "t"}));
        assert_eq!(
            serde_json::from_value::<ContentPart>(serde_json::json!(
                {"type": "reasoning", "text": "t"}
            ))
            .unwrap(),
            plain
        );
    }
}
