//! What a `tool_result`'s verbatim block is on a row that is not sent it as
//! a block (gateway design §6.1, changed 2026-11-06): every egress but the
//! Anthropic one. Anthropic's `tool_result.content` takes `text`, `image`,
//! `document` and `search_result` blocks (a browser use member's result
//! adds `browser_state`, a tool search's `tool_reference`); the IR reads
//! `text` and an inline or URL `image`, and every other block is
//! [`super::super::ToolResultBlock::AnthropicBlock`]. Off an Anthropic route
//! the block becomes what of it reads as text, and what has no text form is
//! named `content.<type>` ([`super::ignored_names`]). The one predicate is
//! [`AnthropicBlock::tool_text`]: the egresses' flattening
//! ([`super::super::flatten_tool_result`]), the header's names and lmgw's
//! estimates all read it, so they agree.
//!
//! - A `document` with a plain-text source (`text`) is its text; one with
//!   a custom-content source (`content`) the text of its text blocks, an
//!   image among them named. The `title` and `context`, which Anthropic
//!   passes to the model as well, come first.
//! - A `search_result` is its title, its `source` (the client's own
//!   identifier, a URL or `kb://…`) and the text of its `content`.
//! - A document or search result with nothing but a title to read (an empty
//!   `data` or `content`) is the title; with nothing at all it is left out
//!   and named.
//! - A `document` with a `base64` (PDF), `url` or Files API `file` source
//!   has no text form lmgw reads, and no egress here has a document input
//!   lmgw sends yet: it is left out and named `content.document`, its URL or
//!   file id never inlined. So is an `image` by Files API reference
//!   (`content.image`).
//! - A document or search result that asked for `citations` is also named
//!   `content.citations`: a row that is not sent the block cannot cite.
//! - Any other type (`browser_state`, `tool_reference`, one Anthropic adds
//!   next) is its JSON as text, as a block the ingress does not read always
//!   was off an Anthropic route, without its `cache_control`; one holding
//!   what no local counter reads ([`has_unread`]: base64 `data`, a file id,
//!   encrypted content, a URL or file source) is left out and named
//!   `content.<type>` instead of inlined.
//!
//! The text of the blocks of one tool result: consecutive verbatim blocks'
//! texts are joined by a blank line, so one document's end and the next
//! title tell apart ([`crate::ir::join_tool_texts`]).

use serde_json::{Map, Value};

use super::AnthropicBlock;

/// The keys whose content a model reads but no local counter does (base64
/// `data`, a Files API `file_id`, encrypted content, signatures).
pub(crate) const UNREAD: &[&str] = &[
    "data",
    "file_id",
    "encrypted_content",
    "encrypted_index",
    "signature",
];

/// Whether `o` is a plain-text document source (`{"type": "text", "data":
/// "..."}`), whose `data` is the document's text.
pub(crate) fn text_source(o: &Map<String, Value>) -> bool {
    o.get("type").and_then(Value::as_str) == Some("text")
        && o.get("data").is_some_and(Value::is_string)
}

/// Whether `o` is a by-URL source (`{"type": "url", "url": "..."}`), an
/// image's or a document's: a reference, not content a local counter reads.
pub(crate) fn url_source(o: &Map<String, Value>) -> bool {
    o.get("type").and_then(Value::as_str) == Some("url")
        && o.get("url").is_some_and(Value::is_string)
}

/// Whether `v` holds what no local counter reads, at any depth: content
/// under an [`UNREAD`] key (a plain-text source's `data` is read) or a
/// by-URL source (changed 2026-11-06). The estimates' and
/// [`AnthropicBlock::tool_text`]'s one predicate.
pub(crate) fn has_unread(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.iter().any(has_unread),
        Value::Object(o) => {
            let text = text_source(o);
            url_source(o)
                || o.iter().any(|(k, x)| {
                    (UNREAD.contains(&k.as_str()) && !x.is_null() && !(text && k == "data"))
                        || has_unread(x)
                })
        }
        _ => false,
    }
}

/// A tool result's verbatim block as text (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolText {
    /// What of the block reads as text; `None` when nothing does.
    pub text: Option<String>,
    /// Whether some of the block has no text form, which the request names
    /// `content.<type>` ([`super::ignored_names`]).
    pub lost: bool,
    /// Whether the block asked for citations, which a row that is not sent
    /// the block cannot give: named `content.citations`.
    pub citations: bool,
}

/// What of a block reads as text, by reference: no text is built.
struct Form<'a> {
    title: Option<&'a str>,
    /// The lines before the body: the title, then the context or source.
    head: Vec<&'a str>,
    body: Vec<&'a str>,
    lost: bool,
    citations: bool,
    /// Another type: its JSON, unless `lost`.
    json: bool,
}

impl AnthropicBlock {
    /// The block as a row that is not sent it as a block gets it (module
    /// doc).
    pub fn tool_text(&self) -> ToolText {
        let f = self.form();
        let text = if f.json {
            (!f.lost).then(|| {
                let mut v = self.block.clone();
                strip_cache_control(&mut v);
                v.to_string()
            })
        } else if f.body.is_empty() {
            // Nothing to read but a title: the title. Where something was
            // lost, nothing is sent.
            f.title.filter(|_| !f.lost).map(str::to_string)
        } else if f.head.is_empty() {
            Some(f.body.join("\n"))
        } else {
            Some(format!("{}\n\n{}", f.head.join("\n"), f.body.join("\n")))
        };
        ToolText {
            text,
            lost: f.lost,
            citations: f.citations,
        }
    }

    /// [`ToolText::lost`] without building the text.
    pub fn lost(&self) -> bool {
        self.form().lost
    }

    /// [`ToolText::citations`] without building the text.
    pub fn wants_citations(&self) -> bool {
        self.form().citations
    }

    fn form(&self) -> Form<'_> {
        let b = &self.block;
        let field = |k: &str| b.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        let citations = b
            .get("citations")
            .and_then(|c| c.get("enabled"))
            .and_then(Value::as_bool)
            == Some(true);
        let mut f = Form {
            title: field("title"),
            head: Vec::new(),
            body: Vec::new(),
            lost: false,
            citations,
            json: false,
        };
        match self.kind() {
            "document" => {
                let source = b.get("source");
                f.head = [field("title"), field("context")]
                    .into_iter()
                    .flatten()
                    .collect();
                match source.and_then(|s| s.get("type")).and_then(Value::as_str) {
                    Some("text") => {
                        f.body = source
                            .and_then(|s| s.get("data"))
                            .and_then(Value::as_str)
                            .into_iter()
                            .collect();
                    }
                    Some("content") => {
                        f.lost = texts(source.and_then(|s| s.get("content")), &mut f.body)
                    }
                    _ => f.lost = true,
                }
            }
            "search_result" => {
                f.head = [field("title"), field("source")]
                    .into_iter()
                    .flatten()
                    .collect();
                f.lost = texts(b.get("content"), &mut f.body);
            }
            "image" => {
                f.lost = true;
                f.citations = false;
            }
            _ => {
                f.json = true;
                f.citations = false;
                f.lost = has_unread(b);
            }
        }
        f.body.retain(|t| !t.is_empty());
        // Nothing at all to read (not even a title) is left out and named.
        if !f.json && f.body.is_empty() && f.title.is_none() {
            f.lost = true;
        }
        f
    }
}

/// The text of a content field into `out`: a string, or an array's `text`
/// blocks; `true` when the array holds a block of another type (an image).
fn texts<'a>(v: Option<&'a Value>, out: &mut Vec<&'a str>) -> bool {
    match v {
        Some(Value::String(s)) => out.push(s),
        Some(Value::Array(items)) => {
            let mut lost = false;
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => out.extend(item.get("text").and_then(Value::as_str)),
                    _ => lost = true,
                }
            }
            return lost;
        }
        _ => {}
    }
    false
}

fn strip_cache_control(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(strip_cache_control),
        Value::Object(o) => {
            o.remove("cache_control");
            o.values_mut().for_each(strip_cache_control);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn of(v: Value) -> ToolText {
        AnthropicBlock::from_client(v).tool_text()
    }

    fn text(t: &str, lost: bool) -> ToolText {
        ToolText {
            text: Some(t.into()),
            lost,
            citations: false,
        }
    }

    const NOTHING: ToolText = ToolText {
        text: None,
        lost: true,
        citations: false,
    };

    #[test]
    fn a_plain_text_document_is_its_text_after_its_title_and_context() {
        assert_eq!(
            of(
                json!({"type": "document", "title": "Weather", "context": "station 4",
                      "source": {"type": "text", "media_type": "text/plain", "data": "15 degrees"},
                      "citations": {"enabled": true}})
            ),
            ToolText {
                citations: true,
                ..text("Weather\nstation 4\n\n15 degrees", false)
            }
        );
        assert_eq!(
            of(json!({"type": "document",
                      "source": {"type": "text", "media_type": "text/plain", "data": "15"}})),
            text("15", false)
        );
    }

    #[test]
    fn a_custom_content_document_is_its_text_blocks_an_image_named() {
        assert_eq!(
            of(
                json!({"type": "document", "source": {"type": "content", "content": [
                    {"type": "text", "text": "First chunk"},
                    {"type": "text", "text": "Second chunk"},
                ]}})
            ),
            text("First chunk\nSecond chunk", false)
        );
        assert_eq!(
            of(
                json!({"type": "document", "source": {"type": "content", "content": [
                    {"type": "text", "text": "Chart:"},
                    {"type": "image", "source": {"type": "file", "file_id": "file_1"}},
                ]}})
            ),
            text("Chart:", true)
        );
    }

    #[test]
    fn a_search_result_is_its_title_source_and_content() {
        assert_eq!(
            of(
                json!({"type": "search_result", "source": "kb://article-1234",
                      "title": "Timeouts", "content": [
                          {"type": "text", "text": "Set timeout_ms."},
                          {"type": "text", "text": "The default is 30 s."},
                      ], "citations": {"enabled": true}})
            ),
            ToolText {
                citations: true,
                ..text(
                    "Timeouts\nkb://article-1234\n\nSet timeout_ms.\nThe default is 30 s.",
                    false
                )
            }
        );
    }

    #[test]
    fn a_pdf_url_or_file_document_has_no_text_form() {
        for source in [
            json!({"type": "base64", "media_type": "application/pdf", "data": "JVBERi0x"}),
            json!({"type": "url", "url": "https://example.com/a.pdf"}),
            json!({"type": "file", "file_id": "file_1"}),
        ] {
            assert_eq!(
                of(json!({"type": "document", "title": "A", "source": source})),
                NOTHING
            );
        }
        assert_eq!(
            of(json!({"type": "image", "source": {"type": "file", "file_id": "file_1"}})),
            NOTHING
        );
    }

    #[test]
    fn another_type_is_its_json() {
        let state = json!({"type": "browser_state", "tabs": [
            {"tab_id": "tab-1", "title": "Docs", "url": "https://example.com/docs", "active": true},
        ]});
        assert_eq!(of(state.clone()), text(&state.to_string(), false));
    }

    #[test]
    fn an_unknown_block_with_unreadable_content_is_left_out() {
        for block in [
            json!({"type": "future", "data": "AAAA"}),
            json!({"type": "future", "file_id": "file_1"}),
            json!({"type": "future", "encrypted_content": "x"}),
            json!({"type": "future", "source": {"type": "url", "url": "https://e.example/a"}}),
        ] {
            assert_eq!(of(block), NOTHING);
        }
        let state =
            of(json!({"type": "future", "note": "a", "cache_control": {"type": "ephemeral"}}));
        assert_eq!(
            state,
            text(&json!({"type": "future", "note": "a"}).to_string(), false)
        );
    }

    #[test]
    fn a_title_alone_is_sent_and_nothing_at_all_is_named() {
        assert_eq!(
            of(json!({"type": "document", "title": "T",
                      "source": {"type": "text", "media_type": "text/plain", "data": ""}})),
            text("T", false)
        );
        assert_eq!(
            of(json!({"type": "search_result", "source": "s", "title": "T", "content": []})),
            text("T", false)
        );
        assert_eq!(
            of(json!({"type": "document",
                      "source": {"type": "text", "media_type": "text/plain", "data": ""}})),
            NOTHING
        );
        assert_eq!(
            of(json!({"type": "search_result", "source": "s", "title": "", "content": []})),
            NOTHING
        );
    }

    #[test]
    fn lost_and_citations_agree_with_the_text() {
        for block in [
            json!({"type": "document", "source": {"type": "file", "file_id": "f"}}),
            json!({"type": "document", "citations": {"enabled": true},
                   "source": {"type": "text", "data": "x"}}),
            json!({"type": "future", "data": "AAAA"}),
            json!({"type": "search_result", "source": "s", "content": []}),
        ] {
            let b = AnthropicBlock::from_client(block);
            let t = b.tool_text();
            assert_eq!((b.lost(), b.wants_citations()), (t.lost, t.citations));
        }
    }
}
