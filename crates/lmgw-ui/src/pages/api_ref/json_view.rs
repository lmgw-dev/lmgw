//! Pretty JSON rendered as syntax-highlighted spans (api-docs design §6.8,
//! §6.10): `.json .k/.s/.n/.b/.z` map to the existing `--code-key/-str/
//! -num/-type/-meta` tokens (`assets/app.css`), the same ones the markdown
//! renderer already uses — no new colours. A long string starts folded with
//! a "show" link; folding only changes what is *displayed*, never the value
//! itself, so nothing the response actually said is unreachable.

use leptos::prelude::*;
use serde_json::Value;

/// A string this long or longer starts folded (§6.8: "over 2,000 chars").
pub const FOLD_AT: usize = 2000;
const PREVIEW_CHARS: usize = 200;

pub fn should_fold(s: &str) -> bool {
    s.chars().count() > FOLD_AT
}

/// What is shown for a string value: the whole thing when `expanded` or it
/// is not long enough to fold, otherwise a preview plus the char count.
pub fn displayed(s: &str, expanded: bool) -> String {
    if expanded || !should_fold(s) {
        return s.to_string();
    }
    let preview: String = s.chars().take(PREVIEW_CHARS).collect();
    format!("{preview}… ({} chars)", s.chars().count())
}

#[component]
pub fn JsonView(value: Value) -> impl IntoView {
    view! { <div class="json">{render(&value)}</div> }
}

fn render(v: &Value) -> AnyView {
    match v {
        Value::Null => view! { <span class="z">"null"</span> }.into_any(),
        Value::Bool(b) => view! { <span class="b">{b.to_string()}</span> }.into_any(),
        Value::Number(n) => view! { <span class="n">{n.to_string()}</span> }.into_any(),
        Value::String(s) => render_string(s.clone()).into_any(),
        Value::Array(items) => {
            if items.is_empty() {
                return view! { <span class="z">"[]"</span> }.into_any();
            }
            let n = items.len();
            let rows: Vec<AnyView> = items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let comma = i + 1 < n;
                    view! {
                        <div class="json-row">
                            {render(item)}
                            {comma.then(|| view! { <span class="z">","</span> })}
                        </div>
                    }
                    .into_any()
                })
                .collect();
            view! {
                <span class="z">"["</span>
                <div class="json-indent">{rows}</div>
                <span class="z">"]"</span>
            }
            .into_any()
        }
        Value::Object(map) => {
            if map.is_empty() {
                return view! { <span class="z">"{}"</span> }.into_any();
            }
            let n = map.len();
            let rows: Vec<AnyView> = map
                .iter()
                .enumerate()
                .map(|(i, (k, val))| {
                    let comma = i + 1 < n;
                    view! {
                        <div class="json-row">
                            <span class="k">{format!("\"{k}\"")}</span>
                            <span class="z">": "</span>
                            {render(val)}
                            {comma.then(|| view! { <span class="z">","</span> })}
                        </div>
                    }
                    .into_any()
                })
                .collect();
            view! {
                <span class="z">"{"</span>
                <div class="json-indent">{rows}</div>
                <span class="z">"}"</span>
            }
            .into_any()
        }
    }
}

fn render_string(s: String) -> impl IntoView {
    let expanded = RwSignal::new(false);
    let folded = should_fold(&s);
    let text = s;
    view! {
        <span class="s">
            "\""
            {move || displayed(&text, expanded.get() || !folded)}
            "\""
            <Show when=move || folded>
                <button
                    type="button"
                    class="link-btn json-fold"
                    on:click=move |_| expanded.update(|e| *e = !*e)
                >
                    {move || if expanded.get() { "hide" } else { "show" }}
                </button>
            </Show>
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_strings_are_never_folded() {
        assert!(!should_fold("hello"));
        assert_eq!(displayed("hello", false), "hello");
    }

    #[test]
    fn folding_keeps_the_full_text_reachable() {
        let long: String = "x".repeat(FOLD_AT + 50);
        assert!(should_fold(&long));
        // Folded: shorter than the original, but nothing is thrown away —
        // `expanded` still shows every character of it.
        let folded = displayed(&long, false);
        assert!(folded.len() < long.len());
        assert_eq!(displayed(&long, true), long);
    }
}
