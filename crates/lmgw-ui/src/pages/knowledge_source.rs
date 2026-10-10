//! The source viewer: a file's extracted text in a dialog — what the chunks
//! point into — optionally scrolled to and highlighting one chunk. The same
//! dialog the Chat's citations will open (design §9.3).

use leptos::prelude::*;

use crate::widgets::{Modal, ModalFooter, ModalSize};

/// What to show: a file, and the chunk to highlight when there is one. A
/// citation also carries where the passage was in the file's text and which
/// version of the file that was, so it still resolves after a re-ingest gave
/// its chunk a new id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceRef {
    pub file_id: i64,
    pub chunk: Option<String>,
    /// The cited span (byte offsets), when the citation stored one.
    pub span: Option<(i64, i64)>,
    /// The file's sha256 when it was cited.
    pub sha: Option<String>,
}

impl SourceRef {
    /// The viewer's request path.
    fn path(&self) -> String {
        let mut path = format!("/api/knowledge/files/{}/text", self.file_id);
        let Some(c) = &self.chunk else {
            return path;
        };
        path.push_str(&format!("?chunk={c}"));
        if let Some((a, b)) = self.span {
            path.push_str(&format!("&span_start={a}&span_end={b}"));
        }
        if let Some(sha) = self.sha.as_ref().filter(|s| !s.is_empty()) {
            path.push_str(&format!("&sha={sha}"));
        }
        path
    }
}

use lmgw_api_types::knowledge::{ChunkSpan as Span, KnowledgeSource as Source};

/// The dialog. Open by setting `target`; closing (Esc, ✕, backdrop) clears it.
#[component]
pub fn SourceModal(target: RwSignal<Option<SourceRef>>) -> impl IntoView {
    let open = RwSignal::new(false);
    // Guarded both ways: an unconditional set would notify even when
    // unchanged and the two effects would ping-pong.
    Effect::new(move |_| {
        let want = target.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && target.with_untracked(Option::is_some) {
            target.set(None);
        }
    });
    view! {
        <Modal open=open title="Extracted text" size=ModalSize::Wide>
            {move || target.get().map(|t| view! { <SourceBody target=t open=open/> })}
        </Modal>
    }
}

/// The text with the highlighted chunk wrapped; offsets are bytes into the
/// text, and one that is not a character boundary means the text moved under
/// the chunk — the highlight is dropped rather than cutting a character.
fn split_at_span(text: &str, span: &Span) -> Option<(String, String, String)> {
    let (a, b) = (
        span.span_start.max(0) as usize,
        span.span_end.max(0) as usize,
    );
    if a > b || b > text.len() || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
        return None;
    }
    Some((
        text[..a].to_string(),
        text[a..b].to_string(),
        text[b..].to_string(),
    ))
}

#[component]
fn SourceBody(target: SourceRef, open: RwSignal<bool>) -> impl IntoView {
    let path = target.path();
    let res = LocalResource::new(move || crate::api::get::<Source>(path.clone()));
    view! {
        {move || match res.get() {
            None => view! { <div class="empty">"Loading…"</div> }.into_any(),
            Some(Err(e)) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
            Some(Ok(s)) => view! { <SourceView s=s open=open/> }.into_any(),
        }}
    }
}

#[component]
fn SourceView(s: Source, open: RwSignal<bool>) -> impl IntoView {
    let file = s.file.clone();
    let name = file.name.clone();
    let id = file.id;
    let download = {
        let name = name.clone();
        Callback::new(move |()| {
            super::docs::download(&format!("/api/knowledge/files/{id}/original"), &name);
        })
    };
    let total = s.chunks.len();
    let where_ = s.highlight.as_ref().map(|h| {
        let mut bits = vec![format!("chunk {} of {total}", h.seq + 1)];
        if let Some(p) = h.page {
            bits.push(format!("page {p}"));
        }
        if !h.heading_path.is_empty() {
            bits.push(h.heading_path.clone());
        }
        bits.push(format!("{} tokens", h.tokens));
        bits.join(" · ")
    });
    let parts = match (&s.text, &s.highlight) {
        (Some(t), Some(h)) => split_at_span(t, h),
        _ => None,
    };
    let highlighted = parts.is_some();
    if highlighted {
        // After the mark is in the DOM.
        request_animation_frame(|| {
            if let Some(el) = document().get_element_by_id("kb-hl") {
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Center);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        });
    }
    let missing_highlight = s.highlight.is_some() && !highlighted;
    view! {
        <div class="kb-src-head">
            <span class="mono-sm">{name}</span>
            <span class="dim mono-sm">
                {s.kb_name.clone()} " · " {file.kind.clone()} " · " {file.status.clone()}
                {file.pages.map(|p| format!(" · {p} pages"))}
            </span>
        </div>
        {s.notice.clone().map(|n| view! { <div class="notice warn">{n}</div> })}
        {where_.map(|w| view! { <div class="dim mono-sm kb-src-where">{w}</div> })}
        {missing_highlight
            .then(|| {
                view! {
                    <div class="notice warn">
                        "The cited region no longer lines up with the extracted text — the file was changed since. The text is shown without a highlight."
                    </div>
                }
            })}
        {match (s.text.clone(), parts) {
            (None, _) => {
                view! { <div class="empty">"This file has not been ingested yet, so there is no text to show."</div> }
                    .into_any()
            }
            (Some(_), Some((before, mid, after))) => {
                view! {
                    <pre class="kb-src">
                        {before} <mark id="kb-hl" class="kb-hl">{mid}</mark> {after}
                    </pre>
                }
                    .into_any()
            }
            (Some(t), None) => view! { <pre class="kb-src">{t}</pre> }.into_any(),
        }}
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| download.run(())>
                "Download original"
            </button>
            <button class="btn" on:click=move |_| open.set(false)>
                "Close"
            </button>
        </ModalFooter>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_carries_a_citations_position_and_version() {
        let plain = SourceRef {
            file_id: 3,
            ..Default::default()
        };
        assert_eq!(plain.path(), "/api/knowledge/files/3/text");
        let cited = SourceRef {
            file_id: 3,
            chunk: Some("abc".into()),
            span: Some((10, 42)),
            sha: Some("f00".into()),
        };
        assert_eq!(
            cited.path(),
            "/api/knowledge/files/3/text?chunk=abc&span_start=10&span_end=42&sha=f00"
        );
        // An old citation has only its id.
        let old = SourceRef {
            file_id: 3,
            chunk: Some("abc".into()),
            ..Default::default()
        };
        assert_eq!(old.path(), "/api/knowledge/files/3/text?chunk=abc");
    }
}
