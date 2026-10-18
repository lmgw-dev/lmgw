//! `GET /api/settings-full`: every setting the dashboard edits, built from
//! [`dto::SettingsFull`] so the document and the answer are one type.

use axum::extract::State;
use axum::response::Response;
use lmgw_api_types as dto;

use crate::ops::backends::to_json;
use crate::state::SharedState;
use crate::web::api::ops_result;

/// `GET /api/settings-full`.
///
/// The typed answer goes out as the `Value` the op dispatchers send, not as
/// `Json(T)`: a `Value`'s keys are sorted, and its numbers are the ones
/// `docs_search.rrf_k` and `fts_weights` (`f32`) have always been written
/// with (`0.1` as the `f64` of that `f32`), so the bytes are the same as when
/// the answer was built with `json!`.
pub async fn settings_full(State(st): State<SharedState>) -> Response {
    let full = settings_view(&st).await;
    ops_result(to_json(Ok(full)))
}

/// The settings as the dashboard reads them. Secrets never leave: a token is
/// the flag that says one is stored, a forge token is the `<set>` marker.
async fn settings_view(st: &SharedState) -> dto::SettingsFull {
    let snap = st.snapshot();
    let s = &snap.settings;
    // The effective dir is spelled out because "unset" means a default that
    // differs between prod and a dev instance, and the warning is asked per
    // read: what the dir sits on can change under a setting nobody retyped
    // (container-builds §5, §7, §8).
    let builds_dir = st.builds_dir();
    dto::SettingsFull {
        bind_addr: s.bind_addr.clone(),
        auth_enabled: s.auth_enabled,
        retention_days: s.retention_days,
        retention_max_rows: s.retention_max_rows,
        jobs_retention_days: s.jobs_retention_days,
        jobs_max_rows: s.jobs_max_rows,
        usage_retention_months: s.usage_retention_months,
        currency: s.currency.clone(),
        local_reference_alias: s.local_reference_alias.clone(),
        global_budget_micro: s.global_budget_micro,
        global_budget_period: s.global_budget_period.as_str().to_string(),
        max_body_mb: s.max_body_mb,
        // The default new Chat threads start with, as in force now, and the
        // built-in one beside it: the page's Reset puts that back in the box.
        chat_system_prompt: s.default_chat_prompt().to_string(),
        chat_system_prompt_builtin: crate::config::BUILTIN_CHAT_SYSTEM_PROMPT.to_string(),
        chat_pdf_mode: s.chat_pdf_mode.clone(),
        chat_stt_alias: s.chat_stt_alias.clone(),
        // The Chat's Voice group (chat-voice design §2.1).
        chat_tts_alias: s.chat_tts_alias.clone(),
        chat_voice: s.chat_voice.clone(),
        chat_speech_style: s.chat_speech_style.clone(),
        chat_voice_language: s.chat_voice_language.clone(),
        chat_voice_reply_language: s.chat_voice_reply_language.clone(),
        // Where the saved languages do not reach a speech model as set — the
        // spoken one the ASR, the reply's the TTS — said beside the fields.
        chat_voice_language_notes: super::super::chat_voice::language_notes(st).await,
        chat_read_aloud: s.chat_read_aloud,
        chat_turn_detection: s.chat_turn_detection.clone(),
        chat_voice_audio_input: s.chat_voice_audio_input.clone(),
        chat_kb_budget_tokens: s.chat_kb_budget_tokens,
        // The profile new threads start with (personality-profiles §3.3).
        chat_profile: s.chat_profile,
        chat_archive_days: s.chat_archive_days,
        chat_purge_days: s.chat_purge_days,
        // The Chat change feed's limits (client-apps design §2.3).
        chat_feed_retention_days: s.chat_feed_retention_days,
        chat_feed_keepalive_s: s.chat_feed_keepalive_s,
        chat_feed_keepalive_max_s: s.chat_feed_keepalive_max_s,
        chat_feed_page_size: s.chat_feed_page_size,
        chat_feed_live_buffer: s.chat_feed_live_buffer,
        self_admin: s.self_admin.into(),
        sampling_alias: s.sampling_alias.clone(),
        responses_max_tool_calls: s.responses_max_tool_calls,
        responses_timeout_seconds: s.responses_timeout_seconds,
        responses_store: s.responses_store,
        responses_retention_hours: s.responses_retention_hours,
        responses_max_chains: s.responses_max_chains,
        docs_ingest_reply_tokens: s.docs_ingest_reply_tokens,
        docs_embed_batch: s.docs_embed_batch,
        docs_fetch_delay_ms: s.docs_fetch_delay_ms,
        docs_rerank_model: s.docs_rerank_model.clone(),
        docs_search: (&s.docs_search).into(),
        update_check_enabled: s.update_check_enabled,
        has_update_token: !s.update_token.is_empty(),
        has_hf_token: !s.hf_token.is_empty(),
        builds_dir: s.builds_dir.clone(),
        builds_dir_effective: builds_dir.display().to_string(),
        builds_dir_warning: crate::backends::paths::tmpfs_refusal(&builds_dir),
        forge_tokens: crate::ops::redact_map(&s.forge_tokens),
        build_update_check_hours: s.build_update_check_hours,
        vram: (&s.vram).into(),
        hold: (&s.hold).into(),
        container_prefix: s.container_prefix.clone(),
        agent_origin_suffix: s.agent_origin_suffix.clone(),
        // Asked per read rather than remembered from boot (origins §4.1): the
        // rule is about `bind_addr`, this machine's names and the suffix
        // together, and any of the three can move under a suffix nobody
        // retyped. `null` when it shadows nothing, which is every gateway that
        // has not been moved.
        agent_origin_suffix_warning: crate::agents::service::origin_suffix_warning(s),
        agent_script_image: s.agent_script_image.clone(),
        router: (&s.router).into(),
        aux_router: (&s.aux_router).into(),
        audio: (&s.audio).into(),
        image: (&s.image).into(),
        realtime: crate::ops::realtime_view(&s.realtime),
        mcp: crate::ops::mcp_settings_view(&s.mcp),
        devices: crate::ops::devices_settings_view(&s.devices, &s.bind_addr),
        api_keys: snap
            .api_keys
            .iter()
            .map(|k| dto::ApiKeyRow {
                id: k.id,
                name: k.name.clone(),
                enabled: k.enabled,
            })
            .collect(),
        data_dir: st.data_dir.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // A derived fact, like the built-in prompt above: what an audio row on
        // the CPU runs with when it names no thread count.
        host_cpu: crate::host::cpu().into(),
    }
}
