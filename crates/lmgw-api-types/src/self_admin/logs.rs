//! `lmgw__logs`

use serde::{Deserialize, Serialize};

/// `lmgw__logs`: the newest request-log rows, newest first. The same as `GET
/// /api/logs`'s `LogsResponse`, with each row cut to the columns that diagnose
/// a request (no cost, key, class or cache columns), under the same names and
/// types.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LogTail {
    pub logs: Vec<LogEntry>,
}

/// One request-log row. Every key is always there; a column the request never
/// had is `null`, never left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LogEntry {
    /// The row's id; `before_id` pages by it.
    pub log_id: i64,
    /// When the request arrived, as the log stores it.
    pub ts: String,
    /// The ingress protocol as logged (`openai`, `anthropic`, `mcp`, ...).
    pub ingress_proto: String,
    /// The name the client asked for.
    pub requested_alias: String,
    /// The upstream that answered; `null` when none did.
    pub upstream_name: Option<String>,
    /// The model name that upstream was sent.
    pub upstream_model: Option<String>,
    /// The tool's name, on a row whose `ingress_proto` is `mcp`.
    pub mcp_tool: Option<String>,
    /// The HTTP status the client got.
    pub status: u16,
    /// Milliseconds to the first byte.
    pub ttfb_ms: Option<i64>,
    /// Milliseconds in all.
    pub total_ms: Option<i64>,
    /// Total input tokens, cache included.
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// Whether the answer was streamed.
    pub streamed: bool,
    pub error_kind: Option<String>,
    pub error_msg: Option<String>,
    /// Why a fallback answered instead of the requested local model: `hold`,
    /// `external_vram`, `background` or `unavailable`; `null` when none did.
    pub fallback_reason: Option<String>,
    /// The ladder rung the row answered from, counted from 1; `null` without a
    /// ladder.
    pub rung: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// All seventeen keys of a row are always sent, `null` where the request
    /// had no value; the tail is `{logs}` and an empty one says so.
    #[test]
    fn a_row_sends_every_key_and_an_empty_tail_is_one_key() {
        let wire = serde_json::to_value(LogEntry::default()).unwrap();
        let keys = wire.as_object().unwrap();
        assert_eq!(keys.len(), 17, "{wire}");
        for nullable in [
            "upstream_name",
            "upstream_model",
            "mcp_tool",
            "ttfb_ms",
            "total_ms",
            "prompt_tokens",
            "completion_tokens",
            "error_kind",
            "error_msg",
            "fallback_reason",
            "rung",
        ] {
            assert!(keys[nullable].is_null(), "{nullable}: {wire}");
        }
        assert_eq!(
            serde_json::to_value(LogTail::default()).unwrap(),
            json!({"logs": []})
        );
    }

    #[test]
    fn a_row_reads_back_as_it_was_written() {
        let row = LogEntry {
            log_id: 7,
            ts: "2026-10-01 12:00:00".into(),
            ingress_proto: "openai".into(),
            requested_alias: "chat".into(),
            upstream_name: Some("acme".into()),
            status: 200,
            ttfb_ms: Some(120),
            rung: Some(2),
            streamed: true,
            ..Default::default()
        };
        let tail = LogTail { logs: vec![row] };
        let wire = serde_json::to_value(&tail).unwrap();
        assert_eq!(wire["logs"][0]["rung"], json!(2));
        assert_eq!(serde_json::from_value::<LogTail>(wire).unwrap(), tail);
    }
}
