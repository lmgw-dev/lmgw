//! `chat_messages.notices` (migration 0107): a reply's notices, the MCP
//! labels its thread or folder attached that offered the turn no tools
//! ([`LabelNotice`]), as a JSON array; NULL for none.

use lmgw_api_types::chat_frames::LabelNotice;

/// The column's value for `notices`: NULL for none.
pub(super) fn to_column(notices: &[LabelNotice]) -> Option<String> {
    (!notices.is_empty()).then(|| serde_json::to_string(notices).expect("notices serialize"))
}

/// The notices a column holds. A value that does not parse (this process
/// wrote every one) reads as none, with a WARN: the reply still reads as
/// its text.
pub(super) fn from_column(v: Option<String>) -> Vec<LabelNotice> {
    let Some(raw) = v else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        tracing::warn!("a chat reply's notices do not read ({e}); it is read without them");
        Vec::new()
    })
}

/// A reply's notices after a turn that added to it (a continue, a resumed
/// turn): the ones it had, then each of `added` about a label and code it
/// did not say yet; one it did say keeps its place and takes the newer
/// sentence (`LabelNotice::merge_into`).
pub fn merged(had: &[LabelNotice], added: &[LabelNotice]) -> Vec<LabelNotice> {
    let mut out = had.to_vec();
    for n in added {
        LabelNotice::merge_into(&mut out, n.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_frames::LabelCode;

    fn notice(label: &str, code: LabelCode) -> LabelNotice {
        LabelNotice {
            server_label: label.into(),
            code,
            message: format!("MCP server '{label}': {}", code.as_str()),
        }
    }

    #[test]
    fn notices_round_trip_and_none_is_null() {
        assert_eq!(to_column(&[]), None);
        let n = vec![
            notice("kai", LabelCode::Offline),
            notice("mail", LabelCode::Failed),
        ];
        assert_eq!(from_column(to_column(&n)), n);
        assert_eq!(from_column(None), Vec::new());
        assert_eq!(from_column(Some("not json".into())), Vec::new());
    }

    #[test]
    fn a_turn_that_adds_to_a_reply_keeps_its_notices_and_adds_only_new_ones() {
        let (kai, mail) = (
            notice("kai", LabelCode::Offline),
            notice("mail", LabelCode::Failed),
        );
        let both = vec![kai.clone(), mail.clone()];
        assert_eq!(merged(std::slice::from_ref(&kai), &both), both);
        assert_eq!(merged(&[], std::slice::from_ref(&mail)), vec![mail.clone()]);
        assert_eq!(merged(std::slice::from_ref(&kai), &[]), vec![kai.clone()]);
        // The same label and code in other words: one notice, the newer
        // sentence, in the older one's place.
        let reworded = LabelNotice {
            message: "MCP server 'kai': still offline".into(),
            ..kai.clone()
        };
        assert_eq!(
            merged(&both, std::slice::from_ref(&reworded)),
            vec![reworded, mail]
        );
    }

    /// A row a newer gateway wrote, with a code this build does not know:
    /// its notices still read, that one too.
    #[test]
    fn a_column_with_a_code_this_build_does_not_know_still_reads() {
        let raw = r#"[{"server_label":"kai","code":"tool_offline","message":"a"},
                      {"server_label":"mail","code":"tool_quota_spent","message":"b"}]"#;
        let read = from_column(Some(raw.into()));
        assert_eq!(read.len(), 2, "{read:?}");
        assert_eq!(read[1].code, LabelCode::Unrecognized);
        assert_eq!(read[1].message, "b");
    }
}
