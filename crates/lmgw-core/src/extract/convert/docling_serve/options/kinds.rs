//! The kinds of docling's options (docling `main`, 2026-11-18) and the type
//! check a request's values pass, as docling's validation reads them; and
//! the reading of a multipart form's fields into those values.

use serde_json::Value;

/// What a docling option holds, for the type check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Bool,
    Float,
    OptFloat,
    Str,
    OptStr,
    /// A list of strings; one string reads as a list of one, as docling's
    /// form fields send it.
    StrList,
    /// `[first, last]`, 1-based.
    PageRange,
    /// An object (a dict or one of docling's option models), or `null`.
    Object,
}

/// Every option docling's `ConvertDocumentsOptions` has (docling `main`,
/// 2026-11-18), with its kind.
pub(super) const OPTIONS: &[(&str, Kind)] = &[
    ("from_formats", Kind::StrList),
    ("to_formats", Kind::StrList),
    ("image_export_mode", Kind::Str),
    ("do_ocr", Kind::Bool),
    ("force_ocr", Kind::Bool),
    ("ocr_engine", Kind::Str),
    ("ocr_lang", Kind::StrList),
    ("ocr_preset", Kind::Str),
    ("ocr_custom_config", Kind::Object),
    ("pdf_backend", Kind::Str),
    ("table_mode", Kind::Str),
    ("table_cell_matching", Kind::Bool),
    ("pipeline", Kind::Str),
    ("page_range", Kind::PageRange),
    ("document_timeout", Kind::OptFloat),
    ("abort_on_error", Kind::Bool),
    ("do_table_structure", Kind::Bool),
    ("do_pdf_heading_hierarchy", Kind::Bool),
    ("pdf_heading_hierarchy_options", Kind::Object),
    ("include_images", Kind::Bool),
    ("include_page_images", Kind::Bool),
    ("images_scale", Kind::Float),
    ("md_page_break_placeholder", Kind::Str),
    ("md_compact_tables", Kind::Bool),
    ("chunking_options", Kind::Object),
    ("chunking_preset", Kind::OptStr),
    ("do_code_enrichment", Kind::Bool),
    ("do_formula_enrichment", Kind::Bool),
    ("do_picture_classification", Kind::Bool),
    ("do_chart_extraction", Kind::Bool),
    ("chart_extraction_preset", Kind::OptStr),
    ("chart_extraction_custom_config", Kind::Object),
    ("do_picture_description", Kind::Bool),
    ("picture_description_area_threshold", Kind::Float),
    ("picture_description_local", Kind::Object),
    ("picture_description_api", Kind::Object),
    ("vlm_pipeline_model", Kind::OptStr),
    ("vlm_pipeline_model_local", Kind::Object),
    ("vlm_pipeline_model_api", Kind::Object),
    ("vlm_pipeline_preset", Kind::OptStr),
    ("picture_description_preset", Kind::OptStr),
    ("code_formula_preset", Kind::OptStr),
    ("vlm_pipeline_custom_config", Kind::Object),
    ("picture_description_custom_config", Kind::Object),
    ("code_formula_custom_config", Kind::Object),
    ("table_structure_preset", Kind::OptStr),
    ("table_structure_custom_config", Kind::Object),
    ("layout_custom_config", Kind::Object),
    ("layout_preset", Kind::OptStr),
    ("picture_classification_preset", Kind::OptStr),
    ("picture_classification_custom_config", Kind::Object),
];

pub(super) fn kind_of(name: &str) -> Option<Kind> {
    OPTIONS.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
}

pub(super) fn check(name: &str, kind: Kind, v: &Value) -> Result<(), (String, &'static str)> {
    let ok = match kind {
        Kind::Bool => v.is_boolean(),
        Kind::Float => v.is_number(),
        Kind::OptFloat => v.is_number() || v.is_null(),
        Kind::Str => v.is_string(),
        Kind::OptStr => v.is_string() || v.is_null(),
        Kind::StrList => {
            v.is_null()
                || v.is_string()
                || v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
        }
        Kind::PageRange => v
            .as_array()
            .is_some_and(|a| a.len() == 2 && a.iter().all(|x| x.as_u64().is_some_and(|n| n >= 1))),
        Kind::Object => v.is_object() || v.is_null(),
    };
    if ok {
        return Ok(());
    }
    Err(match kind {
        Kind::Bool => (
            format!("{name}: Input should be a valid boolean"),
            "bool_parsing",
        ),
        Kind::Float | Kind::OptFloat => (
            format!("{name}: Input should be a valid number"),
            "float_parsing",
        ),
        Kind::Str | Kind::OptStr => (
            format!("{name}: Input should be a valid string"),
            "string_type",
        ),
        Kind::StrList => (
            format!("{name}: Input should be a valid list of strings"),
            "list_type",
        ),
        Kind::PageRange => (
            format!("{name}: Input should be a list of two page numbers from 1"),
            "tuple_type",
        ),
        Kind::Object => (
            format!("{name}: Input should be a valid dictionary"),
            "dict_type",
        ),
    })
}

/// A multipart form's field `name`, given as `texts` (a field repeated for
/// a list), as the JSON value docling's form reading makes of it: lists
/// from the repeats, `true`/`false` and numbers parsed, objects as JSON
/// text. A field this table does not know stays text (one, or a list).
pub fn form_value(name: &str, texts: Vec<String>) -> Result<Value, (String, &'static str)> {
    let last = texts.last().cloned().unwrap_or_default();
    let Some(kind) = kind_of(name) else {
        return Ok(if texts.len() == 1 {
            Value::String(last)
        } else {
            Value::Array(texts.into_iter().map(Value::String).collect())
        });
    };
    let v = match kind {
        Kind::Bool => match last.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "t" | "y" => Value::Bool(true),
            "false" | "0" | "no" | "off" | "f" | "n" => Value::Bool(false),
            _ => Value::String(last),
        },
        Kind::Float | Kind::OptFloat => match last.trim().parse::<f64>() {
            Ok(n) => serde_json::Number::from_f64(n).map_or(Value::String(last), Value::Number),
            Err(_) if kind == Kind::OptFloat && last.trim().is_empty() => Value::Null,
            Err(_) => Value::String(last),
        },
        Kind::Str => Value::String(last),
        Kind::OptStr => {
            if last.is_empty() {
                Value::Null
            } else {
                Value::String(last)
            }
        }
        Kind::StrList => Value::Array(texts.into_iter().map(Value::String).collect()),
        Kind::PageRange => Value::Array(
            texts
                .iter()
                .map(|t| {
                    t.trim()
                        .parse::<u64>()
                        .map_or_else(|_| Value::String(t.clone()), Value::from)
                })
                .collect(),
        ),
        Kind::Object => {
            if last.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&last).unwrap_or(Value::String(last))
            }
        }
    };
    check(name, kind, &v)?;
    Ok(v)
}
