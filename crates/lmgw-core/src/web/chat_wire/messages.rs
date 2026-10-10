//! A thread's messages as the API carries them (`GET
//! /chat/api/threads/{id}`): the store's message, attachment, retrieval and
//! voice rows turned into `lmgw-api-types::chat_threads`' types.
//!
//! **Every field, by name**, as in the parent module: each conversion
//! destructures the store's struct without `..`, so a column added to a
//! message does not compile until it is on the wire (or deliberately left
//! off it here).

use lmgw_api_types::chat_threads as api;

use crate::store;

/// `m` as the thread lists it, with the files sent with it (`attachments`)
/// and the calls its gated reply still waits on, left out when none wait.
pub(crate) fn message(
    m: &store::ChatMessageRow,
    attachments: &[&store::ChatAttachmentMeta],
) -> api::Message {
    let store::ChatMessageRow {
        id,
        thread_id,
        role,
        content,
        reasoning,
        prompt_tokens,
        completion_tokens,
        ir_messages,
        kb_refs,
        context,
        model,
        answered_by,
        images_note,
        voice,
        pending_approvals,
        task,
        created_at,
    } = m.clone();
    api::Message {
        id,
        thread_id,
        role,
        content,
        reasoning,
        prompt_tokens,
        completion_tokens,
        ir_messages,
        kb_refs,
        context: context.as_ref().map(message_context),
        model,
        answered_by,
        images_note,
        voice: voice.as_ref().map(message_voice),
        task,
        created_at,
        attachments: attachments.iter().map(|a| attachment(a)).collect(),
        // The calls a gated turn's reply still waits on (client-apps design
        // §6.2): what `POST …/approvals` decides.
        pending_approvals: pending_approvals
            .as_ref()
            .map(|p| p.requests())
            .filter(|r| !r.is_empty()),
    }
}

/// An attachment's metadata as the API carries it.
pub(crate) fn attachment(a: &store::ChatAttachmentMeta) -> api::AttachmentMeta {
    let store::ChatAttachmentMeta {
        id,
        // Read to group and validate, not published: the attachment sits in
        // the thread or the message that owns it.
        thread_id: _,
        message_id: _,
        kind,
        name,
        mime,
        size,
        mode,
        meta,
        extracted_tokens,
        blockers,
        hints,
    } = a.clone();
    api::AttachmentMeta {
        id,
        kind,
        name,
        mime,
        size,
        mode,
        meta,
        extracted_tokens,
        blockers,
        hints,
    }
}

pub(crate) fn message_context(c: &store::ChatContext) -> api::MessageContext {
    let store::ChatContext {
        excerpts,
        tokens,
        dropped,
        budget_tokens,
        notes,
        searched,
        kb_ids,
        query,
        ms,
    } = c.clone();
    api::MessageContext {
        excerpts: excerpts.into_iter().map(excerpt).collect(),
        tokens: tokens as u64,
        dropped: dropped as u64,
        budget_tokens: budget_tokens as u64,
        notes,
        searched,
        kb_ids,
        query,
        ms,
    }
}

fn excerpt(e: store::ContextExcerpt) -> api::ContextExcerpt {
    let store::ContextExcerpt {
        kb_id,
        kb,
        file_id,
        file,
        page,
        chunk_id,
        heading_path,
        text,
        score,
        tokens,
        span_start,
        span_end,
        file_sha,
    } = e;
    api::ContextExcerpt {
        kb_id,
        kb,
        file_id,
        file,
        page,
        chunk_id,
        heading_path,
        text,
        score,
        tokens: tokens as u64,
        span_start,
        span_end,
        file_sha,
    }
}

/// A message's voice as the API carries it.
pub(crate) fn message_voice(v: &store::MessageVoice) -> api::MessageVoice {
    let store::MessageVoice {
        via,
        asr,
        asr_answered_by,
        asr_ms,
        audio_ms,
        tts,
        tts_answered_by,
        voice,
        unheard,
        input,
        transcript_error,
        timing,
    } = v.clone();
    api::MessageVoice {
        via,
        asr,
        asr_answered_by,
        asr_ms,
        audio_ms,
        tts,
        tts_answered_by,
        voice,
        unheard,
        input: input.map(input_path),
        transcript_error,
        timing: timing.map(timing_of),
    }
}

fn input_path(i: store::InputPath) -> api::InputPath {
    match i {
        store::InputPath::Audio => api::InputPath::Audio,
        store::InputPath::Transcript => api::InputPath::Transcript,
    }
}

fn timing_of(t: store::VoiceTiming) -> api::VoiceTiming {
    let store::VoiceTiming {
        response_id,
        message_id,
        end_of_turn_ms,
        asr_ms,
        first_token_ms,
        reasoning_ms,
        first_clause_ms,
        first_audio_ms,
        total_ms,
        to_first_audio_ms,
        cold,
        first_clause,
        models,
        input,
        input_why,
        transcript_wait_ms,
    } = t;
    let store::VoiceModels { asr, chat, tts } = models;
    api::VoiceTiming {
        response_id,
        message_id,
        end_of_turn_ms,
        asr_ms,
        first_token_ms,
        reasoning_ms,
        first_clause_ms,
        first_audio_ms,
        total_ms,
        to_first_audio_ms,
        cold,
        first_clause,
        models: api::VoiceModels {
            asr: asr.map(served),
            chat: chat.map(served),
            tts: tts.map(served),
        },
        input: input.map(input_path),
        input_why,
        transcript_wait_ms,
    }
}

fn served(s: store::ServedModel) -> api::ServedModel {
    let store::ServedModel {
        alias,
        answered_by,
        voice,
    } = s;
    api::ServedModel {
        alias,
        answered_by,
        voice,
    }
}
