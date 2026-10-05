//! Delivery cues and sounds in a spoken reply's bubble (chat-voice design
//! §8.5: "tags and cues a voice reply writes are kept as written … in its
//! text bubble"). A voice reply's `[laughing]` or `[sighs]` renders as a
//! small chip, not as raw brackets: the text is what the model wrote, the
//! chip says it was a direction for the voice, not words.
//!
//! The grammar is the gateway's own (`lmgw_api_types::realtime::is_tag_name`,
//! the canonical `[tag]` its speech shaping and clause cutting read), applied
//! to the rendered reply's text outside code, links and math — a bracket the
//! grammar does not take (`[1]`, `[S1]`, a link's text, a one-letter `[x]`
//! checkbox or `[a]` enumeration) stays as written.
//! Only replies spoken in voice mode get chips: a typed reply's brackets are
//! the model's text.

use lmgw_api_types::realtime::is_tag_name;

/// `html` (a rendered reply) with every inline tag in its text as a chip.
pub(crate) fn cue_chips(html: &str) -> String {
    if !html.contains('[') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + 64);
    let mut skip = 0usize;
    let mut math = false;
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, rest, skip == 0 && !math);
            break;
        };
        push_text(&mut out, &rest[..lt], skip == 0 && !math);
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

fn push_text(out: &mut String, text: &str, live: bool) {
    if !live || !text.contains('[') {
        out.push_str(text);
        return;
    }
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let chip = after.find(']').and_then(|close| {
            let inner = &after[..close];
            // The text is HTML-escaped; the grammar's apostrophe may be an
            // entity here.
            let plain = inner
                .replace("&#39;", "'")
                .replace("&#x27;", "'")
                .replace("&apos;", "'");
            // `[x](…)` is a link the renderer did not take: text.
            let link = after[close + 1..].starts_with('(');
            (is_tag_name(&plain) && !link).then_some((inner, close))
        });
        match chip {
            Some((inner, close)) => {
                out.push_str(&format!(
                    "<span class=\"cue-chip\" title=\"a direction for the voice: {inner}\">{inner}</span>"
                ));
                rest = &after[close + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(s: &str) -> String {
        format!("<span class=\"cue-chip\" title=\"a direction for the voice: {s}\">{s}</span>")
    }

    #[test]
    fn a_cue_and_a_sound_become_chips() {
        assert_eq!(
            cue_chips("<p>[laughing] Na klar! [sighs]</p>"),
            format!("<p>{} Na klar! {}</p>", chip("laughing"), chip("sighs"))
        );
        assert_eq!(
            cue_chips("<p>[clears throat] [don&#39;t-know]</p>"),
            format!(
                "<p>{} {}</p>",
                chip("clears throat"),
                chip("don&#39;t-know")
            )
        );
    }

    #[test]
    fn brackets_the_grammar_does_not_take_stay_text() {
        let long = format!("<p>[{}]</p>", "a".repeat(32));
        for html in [
            "<p>see [1] and [S1] and [Laughing] and [x2]</p>",
            "<p>[x] done, [a] first, [b] second</p>",
            "<p>[] and [ leading]</p>",
            long.as_str(),
            "<p>[link](not taken)</p>",
            "<p>[unclosed</p>",
        ] {
            assert_eq!(cue_chips(html), html, "{html}");
        }
        let at_limit = format!("<p>[{}]</p>", "a".repeat(31));
        assert_ne!(cue_chips(&at_limit), at_limit, "31 bytes is still a tag");
    }

    #[test]
    fn code_links_and_math_are_left_alone() {
        for html in [
            "<pre><code>[laughing]</code></pre>",
            "<p><code>[sighs]</code></p>",
            "<p><a href=\"x\">[laughs]</a></p>",
            "<p><span class=\"math math-inline\">[ab]</span></p>",
        ] {
            assert_eq!(cue_chips(html), html, "{html}");
        }
        assert_eq!(
            cue_chips("<p><code>[ah]</code> [oh]</p>"),
            format!("<p><code>[ah]</code> {}</p>", chip("oh"))
        );
    }

    #[test]
    fn the_renderer_output_of_a_cue_is_chipped() {
        let html = crate::pages::chat::md_to_html("[laughing] Das ist *lustig*. [sighs]");
        let out = cue_chips(&html);
        assert!(out.contains(&chip("laughing")), "{out}");
        assert!(out.contains(&chip("sighs")), "{out}");
        assert!(!out.contains("[laughing]"));
    }
}
