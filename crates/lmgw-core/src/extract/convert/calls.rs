//! What conversion callers send (document-conversion design §7.8, D16,
//! D17): per request, the route, the file count, the target and every
//! option it set with its outcome, recorded beside its request-log row
//! (`store::convert_calls`); the docling routes' notes as the
//! `x-lmgw-option-notes` header; and the per-key summary the
//! `doc_adapter_usage` op answers ("Who calls the docling route").
//!
//! Values are kept for options whose values are a small fixed set
//! ([`Value::Enum`]); free text, addresses and numbers a caller types are
//! [`Value::Set`], recorded as `set`. No file names, no content, no URLs,
//! no headers, no prompts.

use std::collections::BTreeMap;

use lmgw_api_types::documents::adapter_usage::{
    AdapterKeyRow, AdapterUsage, HostSeen, OptionOutcome, OptionSeen, RouteSeen,
};
use lmgw_api_types::documents::convert::{ConvertOptions, ConvertWarning, WarningCode};
use serde::{Deserialize, Serialize};

use crate::state::SharedState;
use crate::store;

/// The route prefix of the docling-serve conversions.
pub const DOCLING_ROUTES: &str = "/v1/convert/";

/// An option's value, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// One of a small fixed set (`easyocr`, `md,text`, `true`).
    Enum(String),
    /// Free text, an address, a number: recorded as `set`.
    Set,
}

/// One option a request set, and what lmgw did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Noted {
    pub name: String,
    pub value: Value,
    pub outcome: OptionOutcome,
    /// What the header says beside the outcome (`easyocr->lmgw-ocr`); lmgw's
    /// words and the enum-like value only.
    pub detail: Option<String>,
}

impl Noted {
    pub fn new(name: &str, value: Value, outcome: OptionOutcome) -> Self {
        Self {
            name: name.to_string(),
            value,
            outcome,
            detail: None,
        }
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
}

/// One request's record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub route: String,
    pub files: usize,
    pub target: Option<String>,
    pub options: Vec<Noted>,
    /// The host of each `http` source, and `refused` or `fetched`.
    pub hosts: Vec<(String, &'static str)>,
}

#[derive(Serialize, Deserialize)]
struct StoredOption {
    name: String,
    value: String,
    outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredHost {
    host: String,
    outcome: String,
}

impl Record {
    pub fn row(&self) -> store::NewConvertCall {
        let options: Vec<StoredOption> = self
            .options
            .iter()
            .map(|o| StoredOption {
                name: o.name.clone(),
                value: match &o.value {
                    Value::Enum(v) => v.clone(),
                    Value::Set => "set".to_string(),
                },
                outcome: o.outcome.as_str().to_string(),
                detail: o.detail.clone(),
            })
            .collect();
        let hosts: Vec<StoredHost> = self
            .hosts
            .iter()
            .map(|(h, o)| StoredHost {
                host: h.clone(),
                outcome: (*o).to_string(),
            })
            .collect();
        store::NewConvertCall {
            route: self.route.clone(),
            files: i64::try_from(self.files).unwrap_or(i64::MAX),
            target: self.target.clone(),
            options: serde_json::to_string(&options).unwrap_or_else(|_| "[]".into()),
            hosts: serde_json::to_string(&hosts).unwrap_or_else(|_| "[]".into()),
        }
    }
}

/// A header-safe form of `s`: printable ASCII, the header's separators
/// (`;`, `,`, `=`) and anything else replaced by `_`.
pub(crate) fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_graphic() && !matches!(c, ';' | ',' | '=' | '(' | ')') || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The most bytes `x-lmgw-option-notes` takes before it collapses to counts:
/// proxies in front of lmgw refuse headers from about 4 KiB (nginx's
/// `proxy_buffer_size`) up.
pub const NOTES_HEADER_MAX_BYTES: usize = 4096;

fn note_text(o: &Noted) -> String {
    match &o.detail {
        Some(d) => format!("{}={}({})", safe(&o.name), o.outcome.as_str(), safe(d)),
        None => format!("{}={}", safe(&o.name), o.outcome.as_str()),
    }
}

/// `x-lmgw-option-notes`: every outcome other than `honoured`, as
/// `name=outcome(detail)` separated by `; `; `None` when there is none.
/// Past [`NOTES_HEADER_MAX_BYTES`] the unknown options collapse to
/// `unknown_options=not_yet(N names; see the request record)`, and if the
/// rest is still past it, to `option_notes=not_yet(N notes; see the request
/// record)`; the request record keeps each note.
pub fn header(options: &[Noted]) -> Option<String> {
    let noted: Vec<&Noted> = options
        .iter()
        .filter(|o| o.outcome != OptionOutcome::Honoured)
        .collect();
    let join = |parts: &[String]| parts.join("; ");
    let all: Vec<String> = noted.iter().map(|o| note_text(o)).collect();
    if all.is_empty() {
        return None;
    }
    let text = join(&all);
    if text.len() <= NOTES_HEADER_MAX_BYTES {
        return Some(text);
    }
    let is_unknown =
        |o: &Noted| o.detail.as_deref() == Some(super::docling_serve::options::unknown::DETAIL);
    let unknown = noted.iter().filter(|o| is_unknown(o)).count();
    let mut parts: Vec<String> = noted
        .iter()
        .filter(|o| !is_unknown(o))
        .map(|o| note_text(o))
        .collect();
    if unknown > 0 {
        parts.push(format!(
            "unknown_options=not_yet({unknown} names; see the request record)"
        ));
    }
    let text = join(&parts);
    if text.len() <= NOTES_HEADER_MAX_BYTES {
        return Some(text);
    }
    Some(format!(
        "option_notes=not_yet({} notes; see the request record)",
        noted.len()
    ))
}

/// The native route's record: each option that differs from its default,
/// with the outcome its note says (`not_yet`, or `mapped` for a model that
/// is not chosen), `honoured` otherwise.
pub fn native(opts: &ConvertOptions, notes: &[ConvertWarning], files: usize) -> Record {
    let free = [
        "pages",
        "picture_prompt",
        "page_break",
        "picture_min_area",
        "describe_min_area",
    ];
    let set = serde_json::to_value(opts).unwrap_or_default();
    let base = serde_json::to_value(ConvertOptions::default()).unwrap_or_default();
    let mut options = Vec::new();
    if let (Some(set), Some(base)) = (set.as_object(), base.as_object()) {
        for (name, v) in set {
            if base.get(name) == Some(v) || v.is_null() {
                continue;
            }
            // A language list the plan refuses (too many codes, or one
            // that is no code) is recorded as set, never spelled out: only
            // judged codes are values.
            let refused_langs = name == "ocr_languages"
                && opts
                    .ocr_languages
                    .as_deref()
                    .is_some_and(|l| super::ocr::langs::validated(l).is_err());
            let value = if free.contains(&name.as_str()) || refused_langs {
                Value::Set
            } else {
                Value::Enum(match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(a) => a
                        .iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    other => other.to_string(),
                })
            };
            let outcome = match notes
                .iter()
                .find(|n| n.option.as_deref() == Some(name.as_str()))
                .map(|n| n.code)
            {
                Some(WarningCode::NotYet) => OptionOutcome::NotYet,
                Some(WarningCode::OptionNotApplicable) | None => OptionOutcome::Honoured,
                Some(_) => OptionOutcome::Mapped,
            };
            options.push(Noted::new(name, value, outcome));
        }
    }
    Record {
        route: crate::server::documents::PATH.to_string(),
        files,
        target: None,
        options,
        hosts: Vec::new(),
    }
}

#[derive(Default)]
struct Acc {
    key: String,
    last_seen: String,
    requests: u64,
    files: u64,
    routes: BTreeMap<String, u64>,
    options: BTreeMap<(OptionOutcome, String, String, Option<String>), u64>,
    hosts: BTreeMap<(String, String), u64>,
}

/// `doc_adapter_usage`: every key that called a docling route in the kept
/// request log, summed.
pub async fn adapter_usage(state: &SharedState) -> Result<AdapterUsage, String> {
    let rows = store::convert_calls(&state.db, DOCLING_ROUTES)
        .await
        .map_err(|e| e.to_string())?;
    Ok(summarise(rows))
}

/// [`adapter_usage`]'s sums over `rows`.
pub fn summarise(rows: Vec<store::ConvertCallRow>) -> AdapterUsage {
    let mut by_key: BTreeMap<Option<i64>, Acc> = BTreeMap::new();
    for r in rows {
        let a = by_key.entry(r.key_id).or_default();
        a.key = r.client_key.clone().unwrap_or_default();
        if r.ts > a.last_seen {
            a.last_seen = r.ts.clone();
        }
        a.requests += 1;
        a.files += u64::try_from(r.files.max(0)).unwrap_or(0);
        *a.routes.entry(r.route.clone()).or_default() += 1;
        if let Some(t) = &r.target {
            *a.options
                .entry((OptionOutcome::Honoured, "target".into(), t.clone(), None))
                .or_default() += 1;
        }
        let opts: Vec<StoredOption> = serde_json::from_str(&r.options).unwrap_or_default();
        for o in opts {
            *a.options
                .entry((OptionOutcome::parse(&o.outcome), o.name, o.value, o.detail))
                .or_default() += 1;
        }
        let hosts: Vec<StoredHost> = serde_json::from_str(&r.hosts).unwrap_or_default();
        for h in hosts {
            *a.hosts.entry((h.host, h.outcome)).or_default() += 1;
        }
    }
    let mut keys: Vec<AdapterKeyRow> = by_key
        .into_iter()
        .map(|(key_id, a)| {
            let mut routes: Vec<RouteSeen> = a
                .routes
                .into_iter()
                .map(|(route, count)| RouteSeen { route, count })
                .collect();
            routes.sort_by(|x, y| y.count.cmp(&x.count).then(x.route.cmp(&y.route)));
            AdapterKeyRow {
                key_id,
                key: a.key,
                last_seen: a.last_seen,
                requests: a.requests,
                files: a.files,
                routes,
                // The map's order: mapped, not_yet, refused, then honoured.
                options: a
                    .options
                    .into_iter()
                    .map(|((outcome, name, value, detail), count)| OptionSeen {
                        name,
                        value,
                        outcome,
                        detail,
                        count,
                    })
                    .collect(),
                hosts: a
                    .hosts
                    .into_iter()
                    .map(|((host, outcome), count)| HostSeen {
                        host,
                        outcome,
                        count,
                    })
                    .collect(),
            }
        })
        .collect();
    keys.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    AdapterUsage { keys }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_lists_what_was_not_honoured_with_safe_text() {
        let notes = vec![
            Noted::new(
                "do_ocr",
                Value::Enum("true".into()),
                OptionOutcome::Honoured,
            ),
            Noted::new(
                "ocr_engine",
                Value::Enum("easy;ocr".into()),
                OptionOutcome::Mapped,
            )
            .detail("easy;ocr->lmgw-ocr"),
            Noted::new(
                "do_picture_classification",
                Value::Enum("true".into()),
                OptionOutcome::NotYet,
            ),
        ];
        assert_eq!(
            header(&notes).unwrap(),
            "ocr_engine=mapped(easy_ocr->lmgw-ocr); do_picture_classification=not_yet"
        );
        assert_eq!(header(&notes[..1]), None);
    }

    #[test]
    fn a_header_past_the_constant_collapses_to_counts() {
        let unknown =
            |n: String| Noted::new(&n, Value::Set, OptionOutcome::NotYet).detail("unknown option");
        let mut notes: Vec<Noted> = (0..300)
            .map(|i| unknown(format!("example_{i:0>40}")))
            .collect();
        notes.push(Noted::new(
            "do_picture_classification",
            Value::Set,
            OptionOutcome::NotYet,
        ));
        let h = header(&notes).unwrap();
        assert_eq!(
            h,
            "do_picture_classification=not_yet; unknown_options=not_yet(300 names; see the request record)"
        );
        // Many known notes past it too: the whole header is a count.
        let many: Vec<Noted> = (0..400)
            .map(|i| {
                Noted::new(
                    &format!("known_option_{i}"),
                    Value::Set,
                    OptionOutcome::Mapped,
                )
                .detail("a detail that is long enough to add up to a large header")
            })
            .collect();
        assert_eq!(
            header(&many).unwrap(),
            "option_notes=not_yet(400 notes; see the request record)"
        );
        assert!(h.len() <= NOTES_HEADER_MAX_BYTES);
    }

    #[test]
    fn the_native_record_keeps_enums_and_says_set_for_free_text() {
        let o = ConvertOptions {
            output: lmgw_api_types::documents::convert::Output::Text,
            pages: Some("1-3".into()),
            picture_prompt: Some("Describe the example.".into()),
            ocr: lmgw_api_types::documents::convert::Ocr::Force,
            ..Default::default()
        };
        let notes = vec![ConvertWarning {
            code: WarningCode::NotYet,
            message: String::new(),
            tool: None,
            parts: Vec::new(),
            option: Some("ocr".into()),
        }];
        let r = native(&o, &notes, 2);
        let row = r.row();
        assert!(row
            .options
            .contains(r#""name":"output","value":"text","outcome":"honoured""#));
        assert!(row.options.contains(r#""name":"pages","value":"set""#));
        assert!(row
            .options
            .contains(r#""name":"ocr","value":"force","outcome":"not_yet""#));
        assert!(!row.options.contains("1-3") && !row.options.contains("Describe"));
        assert_eq!(row.files, 2);
        // Judged language codes are values; a list the plan refuses (a
        // million codes, a path) is only said to be set.
        let langs = |l: Vec<String>| {
            native(
                &ConvertOptions {
                    ocr_languages: Some(l),
                    document_timeout: Some(30.0),
                    ..Default::default()
                },
                &[],
                1,
            )
            .row()
            .options
        };
        let ok = langs(vec!["eng".into(), "deu".into()]);
        assert!(
            ok.contains(r#""name":"ocr_languages","value":"eng,deu""#),
            "{ok}"
        );
        assert!(
            ok.contains(r#""name":"document_timeout","value":"30.0""#),
            "{ok}"
        );
        let many = langs((0..100_000).map(|i| format!("l{i}")).collect());
        assert!(
            many.contains(r#""name":"ocr_languages","value":"set""#),
            "{many}"
        );
        assert!(many.len() < 500, "{}", many.len());
        let odd = langs(vec!["../../secret/path".into()]);
        assert!(!odd.contains("secret"), "{odd}");
    }

    #[test]
    fn the_summary_sums_per_key_and_puts_what_lmgw_handles_differently_first() {
        let row = |key: Option<i64>, ts: &str, opts: &str| store::ConvertCallRow {
            key_id: key,
            client_key: key.map(|k| format!("example-key-{k}")),
            ts: ts.into(),
            route: "/v1/convert/file".into(),
            files: 2,
            target: Some("inbody".into()),
            options: opts.into(),
            hosts: r#"[{"host":"docs.example","outcome":"refused"}]"#.into(),
        };
        let o = r#"[{"name":"ocr_engine","value":"easyocr","outcome":"mapped"},{"name":"do_ocr","value":"true","outcome":"honoured"}]"#;
        let u = summarise(vec![
            row(Some(1), "2026-11-17T10:00:00Z", o),
            row(Some(2), "2026-11-18T10:00:00Z", "[]"),
            row(Some(1), "2026-11-18T09:00:00Z", o),
        ]);
        assert_eq!(u.keys.len(), 2);
        assert_eq!(u.keys[0].key, "example-key-2");
        let k1 = &u.keys[1];
        assert_eq!((k1.requests, k1.files), (2, 4));
        assert_eq!(k1.last_seen, "2026-11-18T09:00:00Z");
        assert_eq!(k1.options[0].name, "ocr_engine");
        assert_eq!(k1.options[0].count, 2);
        assert_eq!(k1.hosts[0].count, 2);
    }
}
