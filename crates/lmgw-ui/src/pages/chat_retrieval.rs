//! What a Chat turn retrieved from the thread's knowledge bases (chat-complete
//! §9.3): the summary shown collapsed above the answer, the excerpts it
//! expands to, and the `[n]` citations in the answer that point back at them.
//!
//! The data is one [`KbContext`], from the live SSE `retrieval` event or the
//! stored `context` of the user message the answer follows. A citation `[n]`
//! in that answer is `excerpts[n - 1]`.

use leptos::prelude::*;
use serde::Deserialize;

use super::chat_knowledge::KbUi;
use super::knowledge_source::SourceRef;

/// One retrieved passage.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(super) struct Excerpt {
    pub kb: String,
    pub file_id: i64,
    pub file: String,
    pub page: Option<i64>,
    pub chunk_id: String,
    pub heading_path: String,
    pub text: String,
    pub score: f64,
    /// Where the passage was in the file's text, and the file's sha256 then:
    /// what finds it again after a re-ingest renamed its chunk.
    pub span_start: i64,
    pub span_end: i64,
    pub file_sha: String,
}

/// A turn's retrieval.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(super) struct KbContext {
    pub excerpts: Vec<Excerpt>,
    pub tokens: i64,
    pub dropped: i64,
    pub budget_tokens: i64,
    pub notes: Vec<String>,
    pub searched: Vec<String>,
    pub query: String,
    pub ms: f64,
    /// Live only: the stored retrieval of this message stood, nothing was
    /// searched again.
    pub reused: bool,
}

/// `3100` → `3.1k`, `900` → `900`.
fn tokens_text(n: i64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

impl KbContext {
    /// The bases the excerpts came from, first appearance first.
    fn origins(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for e in &self.excerpts {
            if !out.contains(&e.kb.as_str()) {
                out.push(&e.kb);
            }
        }
        out
    }

    /// "4 excerpts from Taxes · 3.1k tokens", or what came of the search when
    /// it found nothing.
    pub(super) fn summary(&self) -> String {
        if self.excerpts.is_empty() {
            let mut s = "No excerpts found".to_string();
            if !self.searched.is_empty() {
                s.push_str(&format!(" in {}", self.searched.join(", ")));
            }
            if !self.notes.is_empty() {
                s.push_str(" — ");
                s.push_str(&self.notes.join(" · "));
            }
            return s;
        }
        let mut s = format!(
            "{} from {} · {} tokens",
            crate::fmt::count_of(self.excerpts.len(), "excerpts"),
            self.origins().join(", "),
            tokens_text(self.tokens)
        );
        if self.dropped > 0 {
            s.push_str(&format!(" · {} over the budget left out", self.dropped));
        }
        s
    }

    /// The source viewer's target for citation `n` (1-based).
    pub(super) fn source(&self, n: usize) -> Option<SourceRef> {
        let e = self.excerpts.get(n.checked_sub(1)?)?;
        Some(SourceRef {
            file_id: e.file_id,
            chunk: Some(e.chunk_id.clone()).filter(|c| !c.is_empty()),
            // A context stored before the position was kept has none.
            span: (e.span_end > e.span_start).then_some((e.span_start, e.span_end)),
            sha: Some(e.file_sha.clone()).filter(|s| !s.is_empty()),
        })
    }

    /// Hover text of a citation: "file · page 3".
    pub(super) fn titles(&self) -> Vec<String> {
        self.excerpts
            .iter()
            .map(|e| match e.page {
                Some(p) => format!("{} · page {p}", e.file),
                None => e.file.clone(),
            })
            .collect()
    }
}

/// The collapsed summary above an answer, expanding to the numbered excerpts.
#[component]
pub(super) fn RetrievalView(#[prop(into)] ctx: Signal<Option<KbContext>>) -> impl IntoView {
    let ui = KbUi::use_ui();
    move || {
        ctx.get().map(|c| {
            let notes = c.notes.clone();
            let has_notes = !notes.is_empty();
            let empty = c.excerpts.is_empty();
            let summary = c.summary();
            let meta = format!(
                "query: {} · {} ms · budget {} tokens",
                c.query,
                c.ms.round() as i64,
                c.budget_tokens
            );
            let rows = c
                .excerpts
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let n = i + 1;
                    let target = c.source(n);
                    let place = match e.page {
                        Some(p) => format!("{} · page {p}", e.file),
                        None => e.file.clone(),
                    };
                    view! {
                        <li>
                            <button
                                type="button"
                                class="kb-ex"
                                title="Open the source"
                                on:click=move |_| ui.source.set(target.clone())
                            >
                                <span class="kb-ex-n">{n}</span>
                                <span class="kb-ex-file">{place}</span>
                                <span class="dim kb-ex-head">{e.heading_path.clone()}</span>
                                <span class="dim mono-sm">{format!("{:.2}", e.score)}</span>
                                <span class="kb-ex-text">{e.text.clone()}</span>
                            </button>
                        </li>
                    }
                })
                .collect_view();
            view! {
                <details class="kb-ctx" class:empty=empty>
                    <summary class="dim">
                        <span class="kb-ctx-sum">{summary}</span>
                        {c.reused.then(|| view! { <span class="type-badge" title="This message's stored retrieval was reused: nothing was searched again">"reused"</span> })}
                        {(has_notes && !empty).then(|| view! { <span class="chip-warn-mark" title="see the notes inside">"⚠"</span> })}
                    </summary>
                    {has_notes
                        .then(|| {
                            view! {
                                <ul class="kb-notes">
                                    {notes.into_iter().map(|n| view! { <li>{n}</li> }).collect_view()}
                                </ul>
                            }
                        })}
                    <ol class="kb-excerpts">{rows}</ol>
                    <div class="dim mini-note">{meta}</div>
                </details>
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Citations
// ---------------------------------------------------------------------------

/// A `[n]`, `[n, m]` group at the start of `s`: its numbers and byte length.
/// Adjacent groups (`[1][3]`) are read one at a time by the caller.
fn cite_group(s: &str) -> Option<(Vec<usize>, usize)> {
    let inner = s.strip_prefix('[')?;
    let end = inner.find(']')?;
    let body = &inner[..end];
    if body.is_empty() || body.len() > 40 {
        return None;
    }
    let mut nums = Vec::new();
    for part in body.split(',') {
        let p = part.trim();
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nums.push(p.parse().ok()?);
    }
    Some((nums, end + 2))
}

fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Turn the citations in one run of text into badges. A number with no
/// excerpt stays as the literal `[n]`.
fn cite_text(out: &mut String, text: &str, titles: &[String]) {
    let mut rest = text;
    while let Some(at) = rest.find('[') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match cite_group(rest) {
            Some((nums, len)) if nums.iter().any(|n| (1..=titles.len()).contains(n)) => {
                for n in nums {
                    if (1..=titles.len()).contains(&n) {
                        out.push_str(&format!(
                            "<span class=\"cite\" role=\"button\" tabindex=\"0\" data-cite=\"{n}\" title=\"{}\">{n}</span>",
                            esc_attr(&titles[n - 1])
                        ));
                    } else {
                        out.push_str(&format!("[{n}]"));
                    }
                }
                rest = &rest[len..];
            }
            _ => {
                out.push('[');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

/// Rewrite `[n]` citations in rendered markdown into clickable badges, for
/// the excerpts `titles` names (1-based). Only text is touched: never inside
/// code (`pre`, `code`), links, buttons or math spans, and never a number that
/// has no excerpt. The markdown itself is not changed, so the same renderer
/// serves the docs playground.
pub(super) fn cite_html(html: &str, titles: &[String]) -> String {
    if titles.is_empty() || !html.contains('[') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + 64);
    let mut skip = 0usize;
    let mut math = false;
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, rest, skip == 0 && !math, titles);
            break;
        };
        push_text(&mut out, &rest[..lt], skip == 0 && !math, titles);
        let after = &rest[lt..];
        let end = after.find('>').map_or(after.len(), |e| e + 1);
        let tag = &after[..end];
        let closing = tag.starts_with("</");
        let name: String = tag
            .trim_start_matches('<')
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        match name.as_str() {
            "pre" | "code" | "a" | "button" | "script" | "style" => {
                if closing {
                    skip = skip.saturating_sub(1);
                } else if !tag.ends_with("/>") {
                    skip += 1;
                }
            }
            "span" if closing => math = false,
            "span" if tag.contains("math") => math = true,
            _ => {}
        }
        out.push_str(tag);
        rest = &after[end..];
    }
    out
}

fn push_text(out: &mut String, text: &str, live: bool, titles: &[String]) {
    if live {
        cite_text(out, text, titles);
    } else {
        out.push_str(text);
    }
}

/// The citation badge under a click, if any: its number.
pub(super) fn cite_target(ev: &web_sys::Event) -> Option<usize> {
    use wasm_bindgen::JsCast;
    let el = ev.target()?.dyn_into::<web_sys::Element>().ok()?;
    let badge = el.closest("[data-cite]").ok()??;
    badge.get_attribute("data-cite")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("f{i}.md · page {i}")).collect()
    }

    fn badges(html: &str) -> Vec<usize> {
        html.match_indices("data-cite=\"")
            .map(|(i, m)| {
                let s = &html[i + m.len()..];
                s[..s.find('"').unwrap()].parse().unwrap()
            })
            .collect()
    }

    #[test]
    fn a_single_citation_becomes_a_badge_with_its_file_and_page() {
        let h = cite_html("<p>Rent is due [1].</p>", &titles(2));
        assert_eq!(badges(&h), vec![1]);
        assert!(h.contains("title=\"f1.md · page 1\""), "{h}");
        assert!(h.starts_with("<p>Rent is due <span"), "{h}");
        assert!(h.ends_with("</span>.</p>"), "{h}");
    }

    #[test]
    fn lists_and_adjacent_groups_cite_every_number() {
        assert_eq!(
            badges(&cite_html("<p>a [1, 2] b</p>", &titles(3))),
            vec![1, 2]
        );
        assert_eq!(
            badges(&cite_html("<p>a [1][3] b</p>", &titles(3))),
            vec![1, 3]
        );
    }

    #[test]
    fn a_number_without_an_excerpt_stays_text() {
        let h = cite_html("<p>x [4] y [1, 9]</p>", &titles(2));
        assert_eq!(badges(&h), vec![1]);
        assert!(h.contains("[4]") && h.contains("[9]"), "{h}");
        assert_eq!(cite_html("<p>[0]</p>", &titles(2)), "<p>[0]</p>");
    }

    #[test]
    fn code_links_and_math_are_left_alone() {
        let src = "<p><code>a[1]</code> <a href=\"x\">see [1]</a> <span class=\"math math-inline\">x_[1]</span></p><pre><code class=\"language-rust\">v[1]\n</code></pre><p>[2]</p>";
        let h = cite_html(src, &titles(2));
        assert_eq!(badges(&h), vec![2], "{h}");
    }

    #[test]
    fn other_brackets_are_not_citations() {
        let src = "<p>[a] [1x] [] [1,] [x, 1]</p>";
        assert_eq!(cite_html(src, &titles(3)), src);
    }

    #[test]
    fn no_excerpts_leaves_the_html_untouched() {
        assert_eq!(cite_html("<p>[1]</p>", &[]), "<p>[1]</p>");
    }

    #[test]
    fn the_summary_names_the_bases_and_the_tokens() {
        let c = KbContext {
            excerpts: vec![
                Excerpt {
                    kb: "Taxes".into(),
                    ..Default::default()
                };
                4
            ],
            tokens: 3100,
            ..Default::default()
        };
        assert_eq!(c.summary(), "4 excerpts from Taxes · 3.1k tokens");
    }

    #[test]
    fn an_empty_retrieval_says_why() {
        let c = KbContext {
            searched: vec!["Taxes".into()],
            notes: vec!["GPU hold is on — keyword search only".into()],
            ..Default::default()
        };
        assert_eq!(
            c.summary(),
            "No excerpts found in Taxes — GPU hold is on — keyword search only"
        );
    }
}
