//! `GET /chat/api/settings` (chat-plane design §2.1, D3): the gateway
//! settings the Chat reads, and only those — the Knowledge section's budget
//! placeholder, an ongoing folder's retention placeholders, the text-to-speech
//! alias a new thread inherits, dictation's body bound, the task strip's poll
//! interval and a realtime session's pre-roll and ping interval. None is a
//! secret, and every caller reads the same values: what a thread or folder
//! does with them is the Chat's own rule, checked where it is written.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_reads::ChatSettings;

use super::super::chat_caller::Caller;
use crate::config::Settings;
use crate::state::SharedState;

/// The handler. The caller is read only to fail closed (its extractor
/// refuses a request without a principal).
pub(in crate::web) async fn settings(State(state): State<SharedState>, _: Caller) -> Response {
    Json(of(&state.snapshot().settings)).into_response()
}

/// The Chat's settings out of the gateway's.
fn of(s: &Settings) -> ChatSettings {
    ChatSettings {
        kb_budget_tokens: s.chat_kb_budget_tokens,
        archive_days: s.chat_archive_days,
        purge_days: s.chat_purge_days,
        tts_alias: s.chat_tts_alias.clone(),
        realtime_tts_alias: s.realtime.tts_alias.clone(),
        max_body_mb: s.max_body_mb,
        task_poll_interval_s: s.mcp.task_poll_interval_s,
        realtime_prefix_padding_ms: s.realtime.prefix_padding_ms,
        realtime_ping_interval_s: s.realtime.ping_interval_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_the_setting_it_names() {
        let s = Settings {
            chat_kb_budget_tokens: 1234,
            chat_archive_days: 9,
            chat_purge_days: 40,
            chat_tts_alias: "speak".into(),
            max_body_mb: 77,
            realtime: crate::config::RealtimeSettings {
                tts_alias: "rt-speak".into(),
                prefix_padding_ms: 310,
                ping_interval_s: 21,
                ..Default::default()
            },
            mcp: crate::config::McpSettings {
                task_poll_interval_s: 13,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            of(&s),
            ChatSettings {
                kb_budget_tokens: 1234,
                archive_days: 9,
                purge_days: 40,
                tts_alias: "speak".into(),
                realtime_tts_alias: "rt-speak".into(),
                max_body_mb: 77,
                task_poll_interval_s: 13,
                realtime_prefix_padding_ms: 310,
                realtime_ping_interval_s: 21,
            }
        );
    }
}
