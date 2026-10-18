//! `POST /decision-lab/api/count` (labs-store design §2.5): the token gauge.
//! Each piece of the form — the state, each question, each `choice`
//! question's options alone — is counted with one `count_texts` call on the
//! model's resolved route, in process, as `/v1/count_tokens` counts, and the
//! estimates are set against the model's limits.
//!
//! Counting goes through the route, so on a local row that is not loaded it
//! would start the container. The rule — ready or in the cloud, else counted
//! only when the request asks (`load`) — is applied to the route the count
//! would hit, after the hold's swap: a held row answering through its cloud
//! fallback is counted there, and whatever local model the gate settles on
//! that is not ready is not started without `load`. A count that starts a
//! model writes a log row, a cheap one none.

use std::collections::BTreeSet;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::{
    DecisionCount, DecisionCounted, DecisionEstimate, DecisionMeasure, QuestionTokens,
};

use super::super::chat::err_json;
use super::super::chat_extract::ChatJson;
use super::models;
use crate::ingress::ClientProto;
use crate::proxy::{IfCold, RequestCtx};
use crate::state::SharedState;

pub async fn count(
    State(state): State<SharedState>,
    ChatJson(req): ChatJson<DecisionCount>,
) -> Response {
    let Some(model) = models::model(&state, &req.model).await else {
        return err_json(
            StatusCode::NOT_FOUND,
            "not_found",
            format!(
                "'{}' answers no /v1/systemone: pick a model of GET /decision-lab/api/models",
                req.model
            ),
        );
    };
    let pieces = req.form.count_pieces();
    let mut texts: Vec<&str> = vec![&pieces.state];
    for (_, text, span) in &pieces.questions {
        texts.push(text);
        if let Some(span) = span {
            texts.push(span);
        }
    }
    let counted = crate::proxy::count_texts_if(
        &state,
        ClientProto::SystemOne,
        &RequestCtx::default(),
        &model.name,
        &texts,
        if req.load {
            IfCold::Start
        } else {
            IfCold::Skip
        },
    )
    .await;
    let (counts, headers) = match counted {
        Ok((Some(counts), headers)) => (counts, headers),
        Ok((None, headers)) => {
            let fallback = headers.fallback();
            return Json(DecisionCounted {
                not_counted: Some(match fallback {
                    Some(fb) => format!("{} answers through {fb}, which is not loaded", model.name),
                    None => format!("{} is not loaded", model.name),
                }),
                model: model.name,
                answered_by: fallback.map(String::from),
                ..Default::default()
            })
            .into_response();
        }
        Err((headers, e)) => {
            return Json(DecisionCounted {
                model: model.name.clone(),
                not_counted: Some(format!("the count failed: {e}")),
                answered_by: headers.fallback().map(String::from),
                ..Default::default()
            })
            .into_response()
        }
    };

    // Always `flattened`: llama.cpp's template wraps the pieces and is not
    // counted. `media_omitted` with images.
    let mut approx: BTreeSet<&'static str> = BTreeSet::from(["flattened"]);
    if !req.form.images.is_empty() {
        approx.insert("media_omitted");
    }
    for c in &counts {
        approx.extend(c.approx.iter().map(|a| a.as_str()));
    }
    let mut it = counts.iter().map(|c| c.tokens);
    let state_tokens = it.next().unwrap_or_default();
    let mut questions = Vec::with_capacity(pieces.questions.len());
    for (id, _, span) in &pieces.questions {
        let tokens = it.next().unwrap_or_default();
        let option_span = span.as_ref().map(|_| it.next().unwrap_or_default());
        questions.push(QuestionTokens {
            id: id.clone(),
            tokens,
            option_span,
        });
    }
    let largest_question = questions.iter().map(|q| q.tokens).max().unwrap_or_default();
    let largest_span = questions
        .iter()
        .filter_map(|q| q.option_span)
        .max()
        .unwrap_or_default();
    let estimates = model
        .limits
        .iter()
        .map(|l| {
            let tokens = match l.applies_to {
                DecisionMeasure::StateAndQuestion => state_tokens + largest_question,
                DecisionMeasure::OptionSpan => largest_span,
            };
            DecisionEstimate {
                over: l.tokens.is_some_and(|max| tokens > max),
                limit: l.clone(),
                tokens,
            }
        })
        .collect();
    Json(DecisionCounted {
        model: model.name,
        not_counted: None,
        state: Some(state_tokens),
        questions,
        approx: approx.into_iter().map(String::from).collect(),
        estimates,
        answered_by: headers.fallback().map(String::from),
    })
    .into_response()
}
