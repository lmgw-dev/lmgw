//! Dictation's two requests (chat-voice design §5): the press's warm of the
//! thread's ASR (`POST …/voice/warm`, SSE `state` frames), and the
//! recording's transcription (`POST …/transcribe`, the WAV as the body), with
//! the size check that names `max_body_mb` before anything is uploaded.

use serde::Deserialize;
use serde_json::{json, Value};

use super::super::spoken::Transcript;
use crate::pages::chat_stream::{send_stream, ChatEvent};

/// A transcription that failed: the gateway's code and message, and the
/// fallback that had the audio — named even when it failed, since the audio
/// may have left this machine (§5, WP3 review M1).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Failure {
    pub code: String,
    pub message: String,
    pub answered_by: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ErrorBody {
    code: String,
    message: String,
    asr_answered_by: Option<String>,
}

impl Failure {
    /// Read a refusal's body; `header` is the gate's `x-lmgw-fallback`.
    pub(crate) fn of(status: u16, body: &str, header: Option<String>) -> Self {
        let parsed = serde_json::from_str::<ErrorBody>(body.trim())
            .ok()
            .filter(|e| !e.message.is_empty());
        match parsed {
            Some(e) => Failure {
                code: e.code,
                message: e.message,
                answered_by: e.asr_answered_by.or(header),
            },
            None => {
                let api = crate::api::parse_error_body(status, body);
                Failure {
                    code: api.code,
                    message: api.message,
                    answered_by: header,
                }
            }
        }
    }

    /// The page's sentence for it; `asked` is the thread's ASR. A fallback
    /// that had the audio is named, and that it failed (review n2).
    pub(crate) fn text(&self, asked: Option<&str>) -> String {
        let mut t = format!("transcription failed: {}", self.message);
        if let Some(b) = &self.answered_by {
            match asked.filter(|a| *a != b) {
                Some(a) => t.push_str(&format!(
                    " — the audio went to {b} (in place of {a}), which failed"
                )),
                None => t.push_str(&format!(" — the audio went to {b}, which failed")),
            }
        }
        t
    }
}

/// The recording is larger than the gateway takes: the sentence that says
/// so, naming the setting (§5). `None` within the bound, or with none.
pub(crate) fn over_body_limit(bytes: usize, max_body_mb: u32) -> Option<String> {
    let max = u64::from(max_body_mb) * 1024 * 1024;
    (max_body_mb > 0 && bytes as u64 > max).then(|| {
        format!(
            "{:.1} MiB recorded, over max_body_mb = {max_body_mb} MiB (Settings → Network & \
             access → Max request body): nothing was sent",
            bytes as f64 / (1024.0 * 1024.0)
        )
    })
}

/// `max_body_mb` as the gateway has it now; `None` when it cannot be read
/// (the gateway's own 413 then says it).
pub(crate) async fn max_body_mb() -> Option<u32> {
    let v = crate::api::get::<Value>("/api/settings-full").await.ok()?;
    v["max_body_mb"].as_u64().map(|n| n as u32)
}

/// Transcribe `wav` with thread `tid`'s ASR. `Err` with a code of `aborted`
/// when `signal` stopped it.
pub(crate) async fn transcribe(
    tid: i64,
    wav: &[u8],
    signal: &web_sys::AbortSignal,
) -> Result<Transcript, Failure> {
    let transport = |e: String| Failure {
        code: if signal.aborted() {
            "aborted".into()
        } else {
            "transport".into()
        },
        message: e,
        answered_by: None,
    };
    let body = js_sys::Uint8Array::from(wav);
    let resp = gloo_net::http::Request::post(&format!("/chat/api/threads/{tid}/transcribe"))
        .header("Content-Type", "audio/wav")
        .abort_signal(Some(signal))
        .body(body)
        .map_err(|e| transport(e.to_string()))?
        .send()
        .await
        .map_err(|e| transport(e.to_string()))?;
    let status = resp.status();
    let header = resp.headers().get("x-lmgw-fallback");
    if (200..300).contains(&status) {
        return resp
            .json::<Transcript>()
            .await
            .map_err(|e| transport(format!("bad answer: {e}")));
    }
    let text = resp.text().await.unwrap_or_default();
    let f = Failure::of(status, &text, header);
    if status == 401 && f.code == "session_required" {
        crate::session::lock();
    }
    Err(f)
}

/// Warm thread `tid`'s ASR (§4.1): its `state` frames go to `on_state`
/// while it runs, until `signal` ends it.
pub(crate) async fn warm(
    tid: i64,
    on_state: impl Fn(&Value) + 'static,
    signal: web_sys::AbortSignal,
) {
    let res = send_stream(
        &format!("/chat/api/threads/{tid}/voice/warm"),
        &json!({ "stages": ["asr"] }),
        &signal,
        move |ev| {
            if let ChatEvent::Voice("state", data) = ev {
                on_state(&data);
            }
        },
    )
    .await;
    if let Err(e) = res {
        if !signal.aborted() {
            leptos::logging::warn!("dictation: the speech-to-text warm-up failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_names_the_fallback_that_had_the_audio() {
        let f = Failure::of(
            502,
            r#"{"code":"upstream_error","message":"bad gateway","asr_answered_by":"openai/whisper-1"}"#,
            None,
        );
        assert_eq!(
            f.text(Some("audio/parakeet")),
            "transcription failed: bad gateway — the audio went to openai/whisper-1 (in place \
             of audio/parakeet), which failed"
        );
        assert_eq!(
            f.text(None),
            "transcription failed: bad gateway — the audio went to openai/whisper-1, which failed"
        );
        // The header names it when the body does not.
        let f = Failure::of(
            502,
            r#"{"code":"x","message":"m"}"#,
            Some("openai/whisper-1".into()),
        );
        assert_eq!(f.answered_by.as_deref(), Some("openai/whisper-1"));
        // A hold is its own code.
        let f = Failure::of(
            503,
            r#"{"code":"gpu_hold","message":"GPU hold is on"}"#,
            None,
        );
        assert_eq!((f.code.as_str(), f.answered_by), ("gpu_hold", None));
        // A body that is not the gateway's shape still says something.
        let f = Failure::of(500, "boom", None);
        assert_eq!(f.message, "boom");
    }

    #[test]
    fn the_size_check_names_the_setting() {
        assert_eq!(over_body_limit(10, 0), None, "0 is no bound");
        assert_eq!(over_body_limit(32 * 1024 * 1024, 32), None);
        assert_eq!(
            over_body_limit(41 * 1024 * 1024, 32).as_deref(),
            Some(
                "41.0 MiB recorded, over max_body_mb = 32 MiB (Settings → Network & access → Max \
                 request body): nothing was sent"
            )
        );
    }
}
