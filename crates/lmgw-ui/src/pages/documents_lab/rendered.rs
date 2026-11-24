//! A converted document's markdown, rendered (WP12). The text comes from an
//! untrusted document and the dashboard holds the owner's session, so it
//! goes through the Chat's own sanitising renderer
//! ([`lmgw_chat_ui::md_to_html`]): raw HTML is written as text, a link keeps
//! only an http(s), mailto or relative target (anything else is `#`), and a
//! remote image becomes a link to it; only inline `data:image/…` pictures
//! (the `pictures: embedded` mode) show. The dashboard's CSP (`img-src 'self'
//! data: blob:`) stays as it is and stops any remote load the renderer let
//! through.
//!
//! A click on a link never navigates the dashboard: an http(s) or mailto
//! link opens in a new window, which the desktop shell hands to the system
//! browser, and every other link (an in-document anchor, a `#` the renderer
//! wrote) does nothing.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// What a click on a link in a rendered document does with `href`: open it
/// outside the dashboard, or nothing.
pub(super) fn opens_outside(href: &str) -> bool {
    let h = href.trim().to_ascii_lowercase();
    h.starts_with("http://") || h.starts_with("https://") || h.starts_with("mailto:")
}

/// One page of a document's markdown, rendered.
#[component]
pub(super) fn Rendered(#[prop(into)] markdown: Signal<String>) -> impl IntoView {
    let el: NodeRef<html::Div> = NodeRef::new();
    Effect::new(move |_| {
        let Some(el) = el.get() else { return };
        el.set_inner_html(&markdown.with(|m| lmgw_chat_ui::md_to_html(m)));
    });
    let on_click = move |ev: web_sys::MouseEvent| {
        let Some(target) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let Ok(Some(a)) = target.closest("a") else {
            return;
        };
        ev.prevent_default();
        let href = a.get_attribute("href").unwrap_or_default();
        if opens_outside(&href) {
            if let Some(w) = web_sys::window() {
                let _ =
                    w.open_with_url_and_target_and_features(&href, "_blank", "noopener,noreferrer");
            }
        }
    };
    view! { <div class="md doclab-md" node_ref=el on:click=on_click></div> }
}

#[cfg(test)]
mod tests {
    use super::opens_outside;

    /// A hostile document: none of its script, handlers, `javascript:` links
    /// or remote pictures survives the renderer as markup.
    #[test]
    fn a_hostile_document_renders_inert() {
        let src = "# Report\n\n<script>alert('x')</script>\n\n\
                   <img src=x onerror=\"alert(1)\">\n\n\
                   [click](javascript:alert(1)) and [also](JaVa\tScRiPt:alert(2))\n\n\
                   ![tracker](https://evil.example/pixel.png)\n\n\
                   ![inline](data:image/png;base64,iVBORw0KGgo=)\n\n\
                   <iframe src=\"https://evil.example\"></iframe>\n\n\
                   [ok](https://docs.example/page)\n";
        let html = lmgw_chat_ui::md_to_html(src);
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("<script"), "{html}");
        assert!(!lower.contains("<img src=\"x\""), "{html}");
        assert!(!lower.contains("<iframe"), "{html}");
        assert!(!lower.contains("javascript:"), "{html}");
        assert!(!lower.contains("<img src=\"https://"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "raw html is text: {html}");
        assert!(
            html.contains("<a href=\"https://evil.example/pixel.png\""),
            "a remote picture is a link to it: {html}"
        );
        assert!(html.contains("<img src=\"data:image/png;base64,"), "{html}");
        assert!(opens_outside("https://docs.example/page"));
        assert!(opens_outside("mailto:someone@example.org"));
        assert!(!opens_outside("#"));
        assert!(!opens_outside("javascript:alert(1)"));
        assert!(!opens_outside("/api/op/settings_set"));
    }

    /// Security round B, L2: lmgw's own answers for the security review's
    /// hostile files (`xss.html`, `xss.pdf`; converted 2026-11-19 on the
    /// round's dev instance, markdown, pictures as placeholders) render
    /// inert: no script, no handler, no `javascript:` or `data:` link, no
    /// remote picture.
    #[test]
    fn the_review_s_hostile_files_as_lmgw_converts_them_render_inert() {
        let html_file = "# T\n\na \\<script>alert(1)\\</script> b\n\n\
                         [j](javascript:alert\\(4\\)) [d](data:text/html,<script>alert\\(5\\)</script>) \n\n\
                         <!-- image -->";
        let pdf_file = "\\<img src=x onerror=alert(1)> <script>alert(2)</script> \
                        [click](javascript:alert(3)) ![p](http://127.0.0.1:991";
        let model_answer = "<script>alert(1)</script> ![x](http://192.0.2.1/x.png) \
                            [l](javascript:alert(2)) <think>hidden</think>";
        for md in [html_file, pdf_file, model_answer] {
            let html = lmgw_chat_ui::md_to_html(md);
            let lower = html.to_ascii_lowercase();
            for bad in [
                "<script",
                "<img src=\"x\"",
                "<img src=\"http",
                "onerror=\"",
                "<svg",
                "href=\"javascript:",
                "href=\"data:",
                "<iframe",
                "<think",
            ] {
                assert!(!lower.contains(bad), "{bad} in {html}");
            }
        }
    }
}
