//! `/v1/convert/file`'s multipart form, as docling-serve reads it: one
//! `files` part per file, `target_type` (`inbody`, `zip`; `presigned_url`
//! is refused), `callbacks` (repeatable, not called) and every other field
//! an option, a repeated field a list.

use std::collections::BTreeMap;

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::StatusCode;
use lmgw_api_types::documents::adapter_usage::OptionOutcome;
use serde_json::Map;

use crate::extract::convert::docling_serve::options::form_value;
use crate::extract::convert::docling_serve::{shape_note, sources, Parsed, Refused, Target};
use crate::extract::convert::file::Input;
use crate::server::documents::body_limit;
use crate::state::SharedState;

fn bad(message: String) -> (Refused, Option<Parsed>) {
    (Refused::new(400, "invalid_multipart", message), None)
}

pub(super) async fn read(
    state: &SharedState,
    route: &'static str,
    req: Request,
    max_mb: u32,
) -> Result<Parsed, (Refused, Option<Parsed>)> {
    let too_large = || {
        (
            Refused::new(413, "body_limit", body_limit(max_mb, false).to_string()),
            None,
        )
    };
    let mut mp = Multipart::from_request(req, state)
        .await
        .map_err(|e| bad(e.body_text()))?;
    let mut inputs = Vec::new();
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut target_type = None;
    let mut callbacks = 0usize;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(too_large()),
            Err(e) => return Err(bad(e.body_text())),
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == "files" {
            let index = inputs.len();
            let file_name = field
                .file_name()
                .filter(|n| !n.is_empty())
                .map(String::from)
                .unwrap_or_else(|| sources::default_name(index));
            let bytes = match field.bytes().await {
                Ok(b) => b,
                Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(too_large()),
                Err(e) => return Err(bad(e.body_text())),
            };
            inputs.push(Input {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                name: file_name,
                bytes: Ok(bytes),
            });
            continue;
        }
        let text = match field.text().await {
            Ok(t) => t,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(too_large()),
            Err(e) => return Err(bad(e.body_text())),
        };
        match name.as_str() {
            "target_type" => target_type = Some(text),
            "callbacks" => callbacks += 1,
            _ => fields.entry(name).or_default().push(text),
        }
    }
    let mut options = Map::new();
    let mut invalid = Vec::new();
    for (name, texts) in fields {
        match form_value(&name, texts) {
            Ok(v) => {
                options.insert(name, v);
            }
            Err((msg, ty)) => invalid.push(serde_json::json!({
                "loc": ["body", name], "msg": msg, "type": ty
            })),
        }
    }
    let mut shape_notes = Vec::new();
    if callbacks > 0 {
        shape_notes.push(shape_note(
            "callbacks",
            OptionOutcome::Mapped,
            "not called so poll the task or use its WebSocket",
        ));
    }
    let mut parsed = Parsed {
        route,
        inputs,
        options,
        target: Target::Inbody,
        shape_notes,
        web: Vec::new(),
        decided: Vec::new(),
    };
    if !invalid.is_empty() {
        let mut r = Refused::new(422, "invalid_option", "");
        r.detail = serde_json::Value::Array(invalid);
        return Err((r, Some(parsed)));
    }
    if let Some(t) = target_type {
        match sources::target(t.trim()) {
            Ok(t) => parsed.target = t,
            Err(r) => return Err((r, Some(parsed))),
        }
    }
    Ok(parsed)
}
