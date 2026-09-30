//! The Chat page's default system prompt: what a new chat thread starts with,
//! and the placeholders every thread's prompt is filled in with at send time.
//!
//! A new thread takes the default as its *own* copy (the thread settings show
//! and edit exactly what is sent), so changing the default never rewrites a
//! conversation already under way. The built-in text is not stored: a gateway
//! whose owner never wrote their own keeps following it as it improves, and
//! saving the built-in text back (the dashboard's Reset) returns to that.

use super::Settings;

/// The built-in default. Written for the model, not the owner: where it is
/// running, who it is talking to, and what the page does with its answer —
/// the things a model cannot know from its weights and otherwise guesses.
pub const BUILTIN_CHAT_SYSTEM_PROMPT: &str = "\
You are talking with the owner of lmgw through the Chat page of its dashboard. \
lmgw is a self-hosted LLM gateway running on the owner's own machine: one \
OpenAI- and Anthropic-compatible API in front of cloud providers and of local \
models served by llama.cpp. You are the model behind the alias \"{{model}}\". \
Today is {{date}}.

About this conversation:
- The owner uses this page to try out models and their settings. If asked \
which model you are, name the alias above instead of guessing from your \
training data. Your training data ends before today, so recent events may be \
missing from it.
- Files the owner attaches arrive inside their message: images as images, \
text files as text. An image you cannot see arrives as a short note in square \
brackets instead; say so rather than describing it.
- Your reply is rendered as Markdown, including tables and syntax-highlighted \
fenced code blocks (name the language after the opening fence). LaTeX is not \
rendered, so write formulas as plain text or in a code block, and do not use \
raw HTML.
- Any thinking you do before answering is shown to the owner in a separate, \
collapsible block.
- You can call tools only when tool definitions come with the request. \
Without them you have no web access, no files and no code execution, so never \
claim to have looked something up or run something.

Answer directly and accurately, in the language the owner writes in, and say \
so when you are not sure.";

/// Replaced by the thread's model alias when a message is sent.
pub const CHAT_PROMPT_MODEL: &str = "{{model}}";
/// Replaced by today's date, in the gateway's local time zone.
pub const CHAT_PROMPT_DATE: &str = "{{date}}";

impl Settings {
    /// The prompt a new chat thread starts with: the owner's own, else the
    /// built-in one.
    pub fn default_chat_prompt(&self) -> &str {
        self.chat_system_prompt
            .as_deref()
            .unwrap_or(BUILTIN_CHAT_SYSTEM_PROMPT)
    }

    /// Store `text` as the default, trimmed. The built-in text stores as "no
    /// prompt of the owner's own", so it keeps tracking the built-in one; an
    /// empty text is a real choice — new threads start with no system prompt.
    pub fn set_default_chat_prompt(&mut self, text: &str) {
        let text = text.trim();
        self.chat_system_prompt =
            (text != BUILTIN_CHAT_SYSTEM_PROMPT.trim()).then(|| text.to_string());
    }
}

/// A thread's system prompt as one send puts it in front of the model: the
/// placeholders filled in, everything else verbatim. The date is a day, not
/// a time, so a conversation's prompt — and the prompt cache behind it —
/// only changes once a day.
pub fn expand_chat_prompt(prompt: &str, model: &str, today: chrono::NaiveDate) -> String {
    if !prompt.contains("{{") {
        return prompt.to_string();
    }
    prompt
        .replace(CHAT_PROMPT_MODEL, model)
        .replace(CHAT_PROMPT_DATE, &today.format("%A, %-d %B %Y").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_default_is_not_stored() {
        let mut s = Settings::default();
        assert_eq!(s.default_chat_prompt(), BUILTIN_CHAT_SYSTEM_PROMPT);
        s.set_default_chat_prompt("Be terse.");
        assert_eq!(s.chat_system_prompt.as_deref(), Some("Be terse."));
        assert_eq!(s.default_chat_prompt(), "Be terse.");
        s.set_default_chat_prompt(&format!("\n{BUILTIN_CHAT_SYSTEM_PROMPT}  \n"));
        assert_eq!(s.chat_system_prompt, None);
    }

    #[test]
    fn an_empty_default_is_kept_as_no_prompt() {
        let mut s = Settings::default();
        s.set_default_chat_prompt("   ");
        assert_eq!(s.chat_system_prompt.as_deref(), Some(""));
        assert_eq!(s.default_chat_prompt(), "");
    }

    #[test]
    fn placeholders_are_filled_in_and_nothing_else_moves() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        assert_eq!(
            expand_chat_prompt(
                "You are {{model}}. Today is {{date}}. {{other}}",
                "qwen",
                day
            ),
            "You are qwen. Today is Tuesday, 29 September 2026. {{other}}"
        );
        assert_eq!(
            expand_chat_prompt("plain {braces}", "qwen", day),
            "plain {braces}"
        );
    }

    #[test]
    fn the_builtin_default_uses_both_placeholders() {
        assert!(BUILTIN_CHAT_SYSTEM_PROMPT.contains(CHAT_PROMPT_MODEL));
        assert!(BUILTIN_CHAT_SYSTEM_PROMPT.contains(CHAT_PROMPT_DATE));
    }
}
