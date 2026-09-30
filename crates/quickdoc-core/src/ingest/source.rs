//! Source kinds, the cheapest-first sniffing order, and the domain fence (§8).

use serde::{Deserialize, Serialize};

use super::{html, url};
use crate::error::{QuickdocError, Result};

/// The four shapes a documentation source arrives in, cheapest first — which is
/// also the order [`sniff`] tries them and the order the ingestion prompt tells
/// the model to prefer (§8). Spelled exactly as the `source.kind` CHECK
/// constraint stores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// `llms.txt` / `llms-full.txt`: markdown written for models, often an index
    /// of further markdown pages.
    LlmsTxt,
    Markdown,
    RustdocJson,
    /// Last resort: rendered HTML, reduced to headings + prose + fenced code.
    Html,
}

impl SourceKind {
    pub const ALL: [SourceKind; 4] = [Self::LlmsTxt, Self::Markdown, Self::RustdocJson, Self::Html];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlmsTxt => "llms_txt",
            Self::Markdown => "markdown",
            Self::RustdocJson => "rustdoc_json",
            Self::Html => "html",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a fetched document. The URL and `Content-Type` decide when they can;
/// the body breaks the ties, because documentation hosts routinely serve
/// markdown as `text/plain` and rustdoc JSON as `application/octet-stream`.
pub fn sniff(url_str: &str, content_type: Option<&str>, body: &str) -> SourceKind {
    let path = url::split(url_str)
        .map(|(_, _, p)| p.split(['?', '#']).next().unwrap_or("").to_string())
        .unwrap_or_else(|| url_str.to_string())
        .to_ascii_lowercase();
    let file = path.rsplit('/').next().unwrap_or("");
    if file == "llms.txt" || file == "llms-full.txt" {
        return SourceKind::LlmsTxt;
    }
    if path.ends_with(".md") || path.ends_with(".markdown") {
        return SourceKind::Markdown;
    }
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let trimmed = body.trim_start();
    let looks_json = path.ends_with(".json") || ct.contains("json") || trimmed.starts_with('{');
    if looks_json && is_rustdoc_json(trimmed) {
        return SourceKind::RustdocJson;
    }
    if ct.contains("markdown") {
        return SourceKind::Markdown;
    }
    if ct.contains("html") || trimmed.starts_with("<!") || trimmed.starts_with("<html") {
        return SourceKind::Html;
    }
    // `text/plain` with no HTML tags in sight: markdown is the useful reading,
    // and it is lossless (the text is kept as-is either way).
    if trimmed.starts_with('<') {
        SourceKind::Html
    } else {
        SourceKind::Markdown
    }
}

/// rustdoc's JSON output is identified by its `format_version` + `index` pair —
/// present since the format existed and absent from every other JSON document.
fn is_rustdoc_json(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    v.get("format_version").is_some() && v.get("index").is_some()
}

/// Reduce a fetched body to the text ingestion works on.
///
/// This is the text the model is shown, the text spans index, and the text a
/// payload is verbatim with respect to. For markdown that is the body itself;
/// for the two derived kinds it is a deterministic rendering, so a re-ingest of
/// unchanged bytes produces byte-identical text and therefore identical chunk
/// ids.
pub fn to_document_text(kind: SourceKind, body: &str) -> Result<String> {
    Ok(match kind {
        SourceKind::LlmsTxt | SourceKind::Markdown => body.to_string(),
        SourceKind::Html => html::to_text(body),
        SourceKind::RustdocJson => rustdoc_to_text(body)?,
    })
}

/// rustdoc JSON → markdown: one section per documented item, keyed by its full
/// path. Ordered by path so the rendering is stable across runs.
fn rustdoc_to_text(body: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| QuickdocError::Invalid(format!("rustdoc json: {e}")))?;
    let index = v
        .get("index")
        .and_then(|i| i.as_object())
        .ok_or_else(|| QuickdocError::Invalid("rustdoc json without an 'index'".into()))?;
    let paths = v.get("paths").and_then(|p| p.as_object());

    let mut sections: Vec<(String, String, String)> = Vec::new();
    for (id, item) in index {
        let docs = item.get("docs").and_then(|d| d.as_str()).unwrap_or("");
        if docs.trim().is_empty() {
            continue;
        }
        let entry = paths.and_then(|p| p.get(id));
        let path = entry
            .and_then(|e| e.get("path"))
            .and_then(|p| p.as_array())
            .map(|segs| {
                segs.iter()
                    .filter_map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("::")
            })
            .or_else(|| item.get("name").and_then(|n| n.as_str()).map(String::from))
            .unwrap_or_else(|| id.clone());
        let kind = entry
            .and_then(|e| e.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("item")
            .to_string();
        sections.push((path, kind, docs.to_string()));
    }
    sections.sort();
    let mut out = String::new();
    if let Some(name) = v.get("root").and_then(|r| r.as_str()) {
        let _ = name; // the root id is opaque; the crate name lives in `paths`.
    }
    for (path, kind, docs) in sections {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## {path} ({kind})\n\n{}", docs.trim_end()));
    }
    Ok(out)
}

/// Markdown links a document points at, resolved against its own URL. This is
/// how an `llms.txt` index turns into a document list — the *code* follows the
/// links, never the model (§8).
pub fn linked_urls(kind: SourceKind, body: &str, base: &str) -> Vec<String> {
    if !matches!(kind, SourceKind::LlmsTxt | SourceKind::Markdown) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(open) = body[i..].find("](").map(|o| i + o) else {
            break;
        };
        let rest = &body[open + 2..];
        let end = rest.find(')').unwrap_or(rest.len());
        let target = rest[..end].split_whitespace().next().unwrap_or("");
        if let Some(abs) = url::join(base, target) {
            if !out.contains(&abs) {
                out.push(abs);
            }
        }
        i = open + 2 + end;
    }
    out
}

/// Domains a source's fetches may touch (§8). The model never free-crawls, and
/// even the code's own link-following is bounded by this.
#[derive(Debug, Clone)]
pub struct Fence {
    hosts: Vec<String>,
}

impl Fence {
    /// An empty `extra` fences the source to its own root host — the safe
    /// reading of "no fence configured", and stated rather than assumed.
    pub fn new(root: &str, extra: &[String]) -> Self {
        let mut hosts: Vec<String> = Vec::new();
        if let Some(h) = url::host(root) {
            hosts.push(h);
        }
        for e in extra {
            let h = url::host(e).unwrap_or_else(|| e.trim().to_ascii_lowercase());
            if !h.is_empty() && !hosts.contains(&h) {
                hosts.push(h);
            }
        }
        Self { hosts }
    }

    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// A host matches its fence entry exactly or as a subdomain of it.
    pub fn allows(&self, url_str: &str) -> bool {
        let Some(host) = url::host(url_str) else {
            return false;
        };
        self.hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    }

    /// The refusal message, naming what was asked for and what is allowed.
    pub fn refusal(&self, url_str: &str) -> QuickdocError {
        QuickdocError::Invalid(format!(
            "{url_str} is outside this source's fence ({})",
            self.hosts.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing_prefers_the_cheapest_kind_that_fits() {
        assert_eq!(
            sniff("https://x.dev/llms-full.txt", None, "# Docs"),
            SourceKind::LlmsTxt
        );
        assert_eq!(
            sniff("https://x.dev/guide.md", Some("text/plain"), "# Guide"),
            SourceKind::Markdown
        );
        assert_eq!(
            sniff(
                "https://x.dev/doc.json",
                Some("application/octet-stream"),
                r#"{"format_version":45,"index":{},"paths":{}}"#
            ),
            SourceKind::RustdocJson
        );
        // JSON that is not rustdoc's must not claim the rustdoc path.
        assert_eq!(
            sniff("https://x.dev/data.json", None, r#"{"a":1}"#),
            SourceKind::Markdown
        );
        assert_eq!(
            sniff("https://x.dev/page", Some("text/html"), "<html>"),
            SourceKind::Html
        );
    }

    #[test]
    fn rustdoc_json_renders_documented_items_only() {
        let body = r#"{
            "format_version": 45, "root": "0:0",
            "index": {
                "0:1": {"name":"Router","docs":"The router.\n"},
                "0:2": {"name":"Private","docs":""}
            },
            "paths": {"0:1": {"path":["axum","Router"],"kind":"struct"}}
        }"#;
        let text = to_document_text(SourceKind::RustdocJson, body).unwrap();
        assert_eq!(text, "## axum::Router (struct)\n\nThe router.");
    }

    #[test]
    fn links_are_absolutised_and_deduplicated() {
        let body = "- [Routing](guide/routing.md)\n- [Same](guide/routing.md)\n- [Ext](https://other.dev/x)\n";
        let links = linked_urls(SourceKind::LlmsTxt, body, "https://x.dev/llms.txt");
        assert_eq!(
            links,
            vec![
                "https://x.dev/guide/routing.md".to_string(),
                "https://other.dev/x".to_string()
            ]
        );
    }

    #[test]
    fn the_fence_defaults_to_the_root_host_and_allows_subdomains() {
        let f = Fence::new("https://docs.rs/axum/llms.txt", &[]);
        assert!(f.allows("https://docs.rs/axum/routing.md"));
        assert!(!f.allows("https://evil.dev/x"));
        let f = Fence::new("https://docs.rs/x", &["example.com".into()]);
        assert!(f.allows("https://api.example.com/y"));
        assert!(!f.allows("https://notexample.com/y"));
    }
}
