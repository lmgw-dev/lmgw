//! A voice turn the Chat refused before it started (`super::start_turn`),
//! as the response's error.
//!
//! A continuation (MCP Tasks design §3.4) is refused as `POST …/answer` is,
//! and its two refusals keep their meaning for the session:
//! - `nothing_to_answer` — a turn that ended since the session last read
//!   its thread answered the results (a dashboard send, another device's
//!   answer) — is the session's own word for a response that answers
//!   nothing, `empty_turn`;
//! - `turn_running` — a turn of the thread runs, which a continuation never
//!   cancels: its end lets the results in, and its reply may answer them.
//!
//! Every other refusal is lmgw's own failure, as before (`internal`, its
//! status named).

use axum::response::Response;
use lmgw_api_types::chat::task_code;
use lmgw_api_types::ApiError;

use crate::error::GatewayError;

/// `resp`, the Chat's refusal of a voice turn; `answer`: the turn was a
/// continuation (module doc).
pub(super) async fn refused(resp: Response, answer: bool) -> GatewayError {
    let status = resp.status();
    // The Chat's own error body, made in this process: read whole.
    let said: Option<ApiError> = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    match (answer, said) {
        (true, Some(e)) if e.code == task_code::NOTHING_TO_ANSWER => GatewayError::InvalidRequest {
            code: "empty_turn",
            message: "this response would answer nothing: the job results it was to answer \
                      were answered by a turn of the thread that ended since, no new words were \
                      said, and no turn is owed an answer"
                .into(),
        },
        (true, Some(e)) if e.code == task_code::TURN_RUNNING => GatewayError::InvalidRequest {
            code: "turn_running",
            message: "a turn of the chat thread is running, and answering the job results \
                      would cancel it: ask again once it ended, if its reply did not answer them"
                .into(),
        },
        _ => GatewayError::Internal(format!(
            "the chat turn was refused before it started ({status})"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    fn refusal(code: &str) -> Response {
        (
            StatusCode::CONFLICT,
            axum::Json(ApiError {
                code: code.into(),
                message: "said".into(),
            }),
        )
            .into_response()
    }

    #[tokio::test]
    async fn a_continuation_s_refusals_keep_their_meaning() {
        let e = refused(refusal(task_code::NOTHING_TO_ANSWER), true).await;
        assert_eq!(e.code(), "empty_turn", "{e}");
        let e = refused(refusal(task_code::TURN_RUNNING), true).await;
        assert_eq!(e.code(), "turn_running", "{e}");
        // Any other turn's refusal is lmgw's own, as before.
        let e = refused(refusal(task_code::NOTHING_TO_ANSWER), false).await;
        assert!(matches!(e, GatewayError::Internal(_)), "{e}");
        assert!(e.to_string().contains("409"), "{e}");
    }
}
