//! The Documents lab's plain words (WP12): a warning's, an error's, a job's
//! and a block's code as the page says it, beside the code itself. The codes
//! are read as the strings the route writes, so a code a later work package
//! adds shows by its own name (underscores as spaces) until it has words here.

/// `n` and `one` or `many`, by `n`: "1 part", "3 parts".
pub(super) fn count(n: impl Into<u64>, one: &str, many: &str) -> String {
    let n = n.into();
    format!(
        "{} {}",
        crate::fmt::grouped(n),
        if n == 1 { one } else { many }
    )
}

/// A warning code (design §7.2) in plain words.
pub(super) fn warning(code: &str) -> String {
    match code {
        "tool_missing" => "a program is missing",
        "no_reader" => "nothing reads these parts yet",
        "model_not_chosen" => "no model is chosen for this",
        "model_held" => "the GPU hold paused the model",
        "model_not_allowed" => "the key may not use the model",
        "part_failed" => "a part could not be read",
        "header_removed" => "a repeated header or footer was left out",
        "equations_dropped" => "equations were left out",
        "pictures_partial" => "some pictures were not found",
        "formulas_partial" => "formulas were kept as text",
        "ocr_language_missing" => "a language pack is missing",
        "not_yet" => "not built yet",
        "pages_out_of_range" => "pages the file does not have",
        "pages_not_applicable" => "the file has no pages",
        "option_not_applicable" => "the option changed nothing here",
        "memory_limit_widened" => "a program needed more memory and was run again",
        "image_not_decoded" => "an image could not be decoded and is blank",
        other => return other.replace('_', " "),
    }
    .into()
}

/// A failed file's code in plain words.
pub(super) fn error(code: &str) -> String {
    match code {
        "tool_missing" => "the program that reads it is missing",
        "no_reader" => "lmgw has no reader for it yet",
        "unsupported" => "not a kind of file lmgw reads",
        "unreadable" => "damaged, or its program could not read it",
        "invalid_base64" => "the file's base64 does not decode",
        "pages_out_of_range" => "none of the pages asked for is in it",
        "output_too_large" => "its text passed the request's output bound",
        "deadline_exceeded" => "not converted within the deadline",
        "interrupted" => "the gateway stopped before it was done",
        "model_not_chosen" => "it needs a model, and none is chosen",
        "model_not_allowed" => "the key may not use the model it needs",
        "model_held" => "the GPU hold paused the model it needs",
        "model_failed" => "the model it needs failed",
        "memory_limit" => "its program needed more memory than its limit",
        other => return other.replace('_', " "),
    }
    .into()
}

/// How a part or a block was read (`source`).
pub(super) fn source(code: &str) -> String {
    match code {
        "text_layer" => "its own text",
        "native" => "lmgw's reader",
        "ocr" => "text recognition",
        "vision" => "a model",
        "onnx" => "the layout model",
        "asr" => "a speech-to-text model",
        other => return other.replace('_', " "),
    }
    .into()
}

/// A file's or a job's state.
pub(super) fn state(code: &str) -> String {
    match code {
        "pending" => "waiting or being read",
        "running" => "running",
        "succeeded" => "done",
        "converted" => "converted",
        "partial" => "partly converted",
        "failed" => "failed",
        "skipped" => "skipped",
        "cancelled" => "cancelled",
        "interrupted" => "interrupted",
        other => return other.replace('_', " "),
    }
    .into()
}

/// The chip class a file's status takes.
pub(super) fn status_chip(code: &str) -> &'static str {
    match code {
        "converted" | "succeeded" => "chip ok",
        "partial" | "skipped" => "chip warn",
        "pending" | "running" => "chip info",
        "failed" | "cancelled" | "interrupted" => "chip err",
        _ => "chip off",
    }
}

/// A docling option's outcome in `x-lmgw-option-notes` in plain words.
pub(super) fn outcome(o: &str) -> String {
    match o {
        "mapped" => "served by lmgw's own equivalent",
        "not_yet" => "not built yet",
        "refused" => "refused",
        "honoured" => "done as docling does it",
        other => return other.replace('_', " "),
    }
    .into()
}

/// `x-lmgw-option-notes` (`name=outcome(detail); …`) as rows: the option,
/// its outcome and the detail in its brackets.
pub(super) fn option_notes(header: &str) -> Vec<(String, String, String)> {
    header
        .split("; ")
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let (name, rest) = s.split_once('=').unwrap_or((s, ""));
            let (outcome, detail) = match rest.split_once('(') {
                Some((o, d)) => (o, d.strip_suffix(')').unwrap_or(d)),
                None => (rest, ""),
            };
            (name.trim().into(), outcome.trim().into(), detail.into())
        })
        .collect()
}

/// `x-lmgw-documents` (`converted=2, partial=0, …`) as pairs.
pub(super) fn counts(header: &str) -> Vec<(String, String)> {
    header
        .split(',')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.trim().replace('_', " "), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_without_words_shows_by_its_own_name() {
        assert_eq!(warning("not_yet"), "not built yet");
        assert_eq!(warning("some_new_code"), "some new code");
        assert_eq!(
            error("unreadable"),
            "damaged, or its program could not read it"
        );
        assert_eq!(source("text_layer"), "its own text");
        assert_eq!(status_chip("partial"), "chip warn");
    }

    #[test]
    fn the_docling_headers_read_as_rows() {
        let n =
            option_notes("ocr_engine=mapped(easyocr->lmgw-ocr); do_picture_classification=not_yet");
        assert_eq!(
            n,
            vec![
                (
                    "ocr_engine".into(),
                    "mapped".into(),
                    "easyocr->lmgw-ocr".into()
                ),
                (
                    "do_picture_classification".into(),
                    "not_yet".into(),
                    String::new()
                ),
            ]
        );
        assert_eq!(
            counts("converted=2, partial=0, entry_names_renamed=1"),
            vec![
                ("converted".into(), "2".into()),
                ("partial".into(), "0".into()),
                ("entry names renamed".into(), "1".into()),
            ]
        );
    }
}
