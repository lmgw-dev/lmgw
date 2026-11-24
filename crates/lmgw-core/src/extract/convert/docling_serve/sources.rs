//! docling-serve's request shapes read into [`Parsed`] (document-conversion
//! design §7.4.1, §7.6): the JSON body's `sources` (`kind: "file"` with
//! `base64_string` and `filename`), the older `file_sources` and
//! `http_sources`, `target`/`targets` and `callbacks`; the multipart form's
//! `files` parts, `target_type`, `callbacks` and options (read by the HTTP
//! half, which hands the fields here).
//!
//! Every source keeps its position in the request (`sources`, then
//! `file_sources`, then `http_sources`) as its file's index. `http` sources
//! are collected with their hosts for [`super::web`], which fetches them or
//! refuses them by name (ruling 9).
//!
//! Refused by name, before any file is converted (D21):
//! - an `s3` source and the other storage kinds;
//! - a `put`, `s3`, `presigned_url` or other storage target.

use base64::Engine as _;
use bytes::Bytes;
use lmgw_api_types::documents::adapter_usage::OptionOutcome;
use lmgw_api_types::documents::docling::{ConvertSourcesRequest, DoclingSource, DoclingTarget};

use super::{shape_note, Parsed, Refused, Target};
use crate::extract::convert::file::Input;

/// base64 as docling's clients send it: padding optional, whitespace left
/// out.
pub fn decode(text: &str) -> Result<Bytes, String> {
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let clean: String;
    let text = if text.bytes().any(|b| b.is_ascii_whitespace()) {
        clean = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        clean.as_str()
    } else {
        text
    };
    engine
        .decode(text)
        .map(Bytes::from)
        .map_err(|e| format!("base64_string is not base64: {e}"))
}

/// A target kind docling has; inbody and zip are served.
pub fn target(kind: &str) -> Result<Target, Refused> {
    match kind {
        "inbody" => Ok(Target::Inbody),
        "zip" => Ok(Target::Zip),
        other => Err(Refused::new(
            400,
            "target_refused",
            format!(
                "the target kind '{}' would make lmgw write the result to a place the \
                 caller chooses, which lmgw does not do; use the target inbody (the answer \
                 in the body) or zip (converted_docs.zip in the body)",
                crate::extract::convert::quoted::quoted(other)
            ),
        )),
    }
}

/// The host of an `http` source, as recorded (never the URL itself): the
/// parsed host in its normal spelling (lowercase, punycode, an address as
/// the address it means), `invalid-url` when there is none.
pub fn host(url: &str) -> String {
    url::Url::parse(url.trim())
        .ok()
        .and_then(|u| {
            u.host()
                .and_then(crate::extract::convert::source_fetch::Host::from_url_host)
        })
        .map_or_else(|| "invalid-url".to_string(), |h| h.to_string())
}

fn file_input(index: usize, s: &DoclingSource) -> Result<Input, Refused> {
    let Some(b64) = &s.base64_string else {
        return Err(Refused::new(
            422,
            "invalid_source",
            format!("sources[{index}]: a file source needs base64_string"),
        ));
    };
    Ok(Input {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        name: s.filename.clone().unwrap_or_else(|| default_name(index)),
        bytes: decode(b64),
    })
}

/// docling-serve's name for an upload without a filename.
pub fn default_name(index: usize) -> String {
    if index == 0 {
        "file.pdf".to_string()
    } else {
        format!("file_{index}.pdf")
    }
}

/// The JSON body of `route`.
pub fn from_json(
    route: &'static str,
    req: ConvertSourcesRequest,
) -> Result<Parsed, (Refused, Parsed)> {
    let mut shape_notes = Vec::new();
    let mut all: Vec<(DoclingSource, &str)> = Vec::new();
    for s in req.sources {
        let k = s.kind.clone().unwrap_or_else(|| "file".into());
        all.push((
            s,
            if k == "file" {
                "file"
            } else if k == "http" {
                "http"
            } else {
                "other"
            },
        ));
    }
    if !req.file_sources.is_empty() {
        shape_notes.push(shape_note("file_sources", OptionOutcome::Honoured, ""));
    }
    for s in req.file_sources {
        all.push((s, "file"));
    }
    if !req.http_sources.is_empty() {
        shape_notes.push(shape_note("http_sources", OptionOutcome::Honoured, ""));
    }
    for s in req.http_sources {
        all.push((s, "http"));
    }
    let kinds_refused: Vec<String> = all
        .iter()
        .filter(|(_, k)| *k == "other")
        .map(|(s, _)| s.kind.clone().unwrap_or_default())
        .collect();
    let web: Vec<super::web::WebSource> = all
        .iter_mut()
        .enumerate()
        .filter(|(_, (_, k))| *k == "http")
        .map(|(i, (s, _))| {
            let url = s.url.take().unwrap_or_default();
            super::web::WebSource {
                index: u32::try_from(i).unwrap_or(u32::MAX),
                host: host(&url),
                outcome: "refused",
                tried: false,
                url,
                headers: s.headers.take(),
            }
        })
        .collect();
    let mut targets: Vec<DoclingTarget> = req.targets.unwrap_or_default();
    if let Some(t) = req.target {
        targets.insert(0, t);
    }
    if !req.callbacks.is_empty() {
        shape_notes.push(shape_note(
            "callbacks",
            OptionOutcome::Mapped,
            "not called so poll the task or use its WebSocket",
        ));
    }
    let mut parsed = Parsed {
        route,
        inputs: Vec::new(),
        options: req.options,
        target: Target::Inbody,
        shape_notes,
        web,
        decided: Vec::new(),
    };
    let mut target_kind = Target::Inbody;
    for t in &targets {
        match target(&t.kind) {
            Ok(Target::Zip) => target_kind = Target::Zip,
            Ok(Target::Inbody) => {}
            Err(r) => return Err((r, parsed)),
        }
    }
    parsed.target = target_kind;
    if !kinds_refused.is_empty() {
        let r = Refused::new(
            400,
            "source_kind_refused",
            format!(
                "lmgw does not fetch documents from storage a caller names (source kind(s): \
                 {}); send the file itself, as a source of kind \"file\" with base64_string",
                kinds_refused.join(", ")
            ),
        );
        return Err((r, parsed));
    }
    for (i, (s, _)) in all.iter().enumerate().filter(|(_, (_, k))| *k == "file") {
        match file_input(i, s) {
            Ok(input) => parsed.inputs.push(input),
            Err(r) => return Err((r, parsed)),
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(v: serde_json::Value) -> ConvertSourcesRequest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn file_sources_in_both_spellings_become_inputs_in_order() {
        let p = from_json(
            "/v1/convert/source",
            req(json!({
                "sources": [{"kind": "file", "base64_string": "aGk=", "filename": "example-a.txt"}],
                "file_sources": [{"base64_string": "aG8", "filename": "example-b.txt"}],
                "target": {"kind": "zip"}
            })),
        )
        .unwrap_or_else(|_| panic!("refused"));
        assert_eq!(p.inputs.len(), 2);
        assert_eq!(p.inputs[1].name, "example-b.txt");
        assert_eq!(p.inputs[1].index, 1);
        assert_eq!(
            p.inputs[1].bytes.as_ref().unwrap(),
            &Bytes::from_static(b"ho")
        );
        assert_eq!(p.target, Target::Zip);
    }

    #[test]
    fn http_sources_keep_their_place_and_their_hosts() {
        let p = from_json(
            "/v1/convert/source",
            req(json!({
                "sources": [
                    {"kind": "http", "url": "https://Docs.Example./a.pdf?token=x"},
                    {"kind": "file", "base64_string": "aGk=", "filename": "example-a.txt"}
                ],
                "http_sources": [{"url": "http://0x7f.1:8897/b.pdf"}]
            })),
        )
        .unwrap_or_else(|_| panic!("refused"));
        let hosts: Vec<(&str, &str)> = p.web.iter().map(|w| (w.host.as_str(), w.outcome)).collect();
        assert_eq!(
            hosts,
            [("docs.example", "refused"), ("127.0.0.1", "refused")]
        );
        let idx: Vec<u32> = p.web.iter().map(|w| w.index).collect();
        assert_eq!(idx, [0, 2]);
        assert_eq!(p.inputs[0].index, 1);
    }

    #[test]
    fn storage_sources_and_targets_are_refused() {
        let Err((r, _)) = from_json(
            "/v1/convert/source",
            req(json!({"sources": [{"kind": "s3", "bucket": "example"}]})),
        ) else {
            panic!("served");
        };
        assert_eq!(r.code, "source_kind_refused");
        let Err((r, _)) = from_json(
            "/v1/convert/source",
            req(json!({"sources": [], "target": {"kind": "put", "url": "https://example.com/"}})),
        ) else {
            panic!("served");
        };
        assert_eq!(r.code, "target_refused");
    }
}
