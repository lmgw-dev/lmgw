//! One transcription for a conversion (design §7.5): the calling key's
//! per-call check (`key_check.rs`, the page and picture models': expiry, scope,
//! budget, tokens-per-minute window; the requests-per-minute window is
//! neither asked nor counted), then lmgw's in-process speech-to-text call
//! (`proxy::transcribe_document`), so the GPU hold, fallbacks, admission
//! and the usage row are what they are for any client. The row is
//! labelled `documents-asr`, under the calling key; a call dropped before
//! its answer (the client gone, the file's `document_timeout`, a cancelled
//! job) leaves a `499` row.
//!
//! The call is part of the file's conversion time (`pool::clock`): the
//! clock runs from the call's start, the wait for the gate (the model's
//! turn, a local model's start) excepted. Like the page-reading model's
//! stage, the call takes the end of its file's clock over
//! (`FileClock::hand_over`) and ends itself at the deadline
//! (`deadline_exceeded`, the call kept in `stats`): an e-mail whose
//! attachment is transcribed keeps what it read before (its one clock,
//! `file/more/late.rs`), where the batch would have ended the whole
//! message (security round B, a carried bug).
//!
//! The upstream is sent `audio.<extension of the sniffed type>`, never the
//! caller's file name: a cloud model reads the format from the extension,
//! and a file's name is the caller's.

use bytes::Bytes;
use lmgw_api_types::documents::convert::ErrorCode;

use super::Speech;
use crate::erased::Erased;
use crate::error::GatewayError;
use crate::extract::convert::pool::clock;

/// What a transcription came to: the text, who answered, and the alias
/// that answered when a fallback did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heard {
    pub text: String,
    /// The model that answered: the chosen one, or its fallback.
    pub answered: String,
    pub fallback: Option<String>,
}

/// Why nothing was heard, as the file fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unheard {
    pub code: ErrorCode,
    pub message: String,
    /// The call went to a model (it is paid for, and `stats` keeps it):
    /// the model that had it.
    pub called: Option<String>,
}

/// The name the upload has: `audio.<sub>` (module doc).
pub fn upload_name(sub: &str) -> String {
    format!("audio.{sub}")
}

/// Transcribe `bytes` (of the sniffed type `sub`, `mime`) with `speech`'s
/// alias as its key. The future is erased here (`crate::erased`): the
/// gate's and the upload's futures are deep, and a conversion's future is
/// proved `Send` where a job is spawned.
pub fn transcribe<'a>(
    speech: &'a Speech,
    bytes: Bytes,
    sub: &'a str,
    mime: &'a str,
) -> Erased<'a, Result<Heard, Unheard>> {
    Box::pin(async move {
        if speech.alias.is_empty() {
            let (code, message) = super::not_chosen();
            return Err(Unheard {
                code,
                message,
                called: None,
            });
        }
        let (state, ctx, alias) = (&speech.state, &speech.ctx, speech.alias.as_str());
        if let Err(e) = crate::extract::convert::key_check::may_call(state, ctx, alias).await {
            let (code, message) = refused(alias, &e);
            return Err(Unheard {
                code,
                message,
                called: None,
            });
        }
        // The call is conversion time from here (module doc), and ends the
        // file at its deadline itself.
        clock::started();
        let clock = clock::current();
        if let Some(c) = &clock {
            c.hand_over();
        }
        let name = upload_name(sub);
        // The call counts itself where it is sent, under the model that
        // answers (`proxy::transcribe`, the review of round B, N3).
        let call = crate::proxy::transcribe_document(state, ctx, alias, (bytes, &name, mime));
        let t = match &clock {
            None => call.await,
            Some(c) => {
                tokio::pin!(call);
                tokio::select! {
                    biased;
                    t = &mut call => t,
                    () = c.expiry() => return Err(Unheard {
                        code: ErrorCode::DeadlineExceeded,
                        message: format!(
                            "the speech-to-text model {alias} had not answered when the \
                             caller's document_timeout of {} s passed; its call was stopped",
                            c.limit().as_secs_f64()
                        ),
                        called: Some(alias.to_string()),
                    }),
                }
            }
        };
        match t {
            Ok(t) => {
                let fallback = t.answered_by().map(str::to_string);
                Ok(Heard {
                    answered: fallback.clone().unwrap_or_else(|| alias.to_string()),
                    text: t.text,
                    fallback,
                })
            }
            Err(u) => {
                let (code, message) = refused(alias, &u.error);
                let called = reached_a_model(&u.error).then(|| {
                    u.headers
                        .fallback()
                        .map_or_else(|| alias.to_string(), str::to_string)
                });
                Err(Unheard {
                    code,
                    message,
                    called,
                })
            }
        }
    })
}

/// One transcription call sent to `answered` (a fallback `answered_for`
/// the alias asked for), as `convert::sent` counts it.
pub(crate) fn one_call(
    answered: &str,
    answered_for: Option<&str>,
) -> lmgw_api_types::documents::model_help::ModelCalls {
    lmgw_api_types::documents::model_help::ModelCalls {
        alias: answered.to_string(),
        answered_for: answered_for.map(str::to_string),
        calls: 1,
        parts_read: 0,
        pictures_described: 0,
        files_transcribed: 0,
        in_flight_bound: 1,
        bound_from: lmgw_api_types::documents::model_help::BoundFrom::OneCall,
    }
}

/// Whether a failed call had reached a model (it was sent): every failure
/// but a refusal before the send.
fn reached_a_model(e: &GatewayError) -> bool {
    !matches!(
        e,
        GatewayError::GpuHold { .. }
            | GatewayError::GpuBenchmark { .. }
            | GatewayError::KeyScope { .. }
            | GatewayError::KeyBudget { .. }
            | GatewayError::BudgetUnpriced { .. }
            | GatewayError::KeyRate { .. }
            | GatewayError::KeyExpired { .. }
            | GatewayError::Unauthorized(_)
            | GatewayError::UnknownAlias(_)
    )
}

/// How a refused or failed call fails the file.
pub fn refused(alias: &str, e: &GatewayError) -> (ErrorCode, String) {
    match e {
        GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. } => (
            ErrorCode::ModelHeld,
            format!(
                "the speech-to-text model {alias} is paused by the GPU hold and has no \
                 fallback, so nothing was transcribed: {e}"
            ),
        ),
        GatewayError::KeyScope { .. }
        | GatewayError::KeyBudget { .. }
        | GatewayError::BudgetUnpriced { .. }
        | GatewayError::KeyRate { .. }
        | GatewayError::KeyExpired { .. }
        | GatewayError::Unauthorized(_) => (
            ErrorCode::ModelNotAllowed,
            format!("this key may not use the speech-to-text model {alias}: {e}"),
        ),
        GatewayError::UnknownAlias(_) => (
            ErrorCode::ModelFailed,
            format!(
                "the speech-to-text model chosen under Settings → Documents, {alias}, does \
                 not exist any more: choose another"
            ),
        ),
        other => (
            ErrorCode::ModelFailed,
            format!("the speech-to-text model {alias} failed: {other}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upload_is_named_by_its_type_alone() {
        assert_eq!(upload_name("mp3"), "audio.mp3");
        assert_eq!(upload_name("wav"), "audio.wav");
    }

    #[test]
    fn refusals_are_named_and_only_a_sent_call_counts() {
        let held = GatewayError::GpuHold {
            model: "example-stt".into(),
            detail: "the GPU hold is on".into(),
        };
        assert_eq!(refused("example-stt", &held).0, ErrorCode::ModelHeld);
        assert!(!reached_a_model(&held));
        let gone = GatewayError::UnknownAlias("example-stt".into());
        let (code, m) = refused("example-stt", &gone);
        assert_eq!(code, ErrorCode::ModelFailed);
        assert!(m.contains("Settings → Documents"), "{m}");
        let up = GatewayError::Transport("connection reset".into());
        assert_eq!(refused("example-stt", &up).0, ErrorCode::ModelFailed);
        assert!(reached_a_model(&up));
    }
}
