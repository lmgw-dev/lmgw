//! An MCP server's form: one signal per value, the baseline a change is
//! measured against, the values that do not read, and the body
//! `mcp_server_set` takes — the same payload the modal editor posted.
//! Env and headers are edited as stored (`KEY=value`, `Name: value` lines).

use leptos::prelude::*;
use lmgw_api_types::McpServerView;
use serde_json::{json, Value};

/// The values, in the order [`McpForm::values`] and the baseline hold them.
const KEYS: [&str; 17] = [
    "name",
    "transport",
    "command",
    "args",
    "env",
    "cwd",
    "image",
    "run_args",
    "url",
    "headers",
    "prefix",
    "timeout",
    "idle",
    "sampling_alias",
    "autostart",
    "allow_sampling",
    "enabled",
];

/// The text fields, in [`KEYS`]'s order (the last three are switches).
const TEXTS: usize = 14;

#[derive(Clone, Copy)]
pub(super) struct McpForm {
    texts: [RwSignal<String>; TEXTS],
    pub autostart: RwSignal<bool>,
    pub allow_sampling: RwSignal<bool>,
    pub enabled: RwSignal<bool>,
    baseline: RwSignal<Vec<String>>,
    pub dirty: Memo<Vec<&'static str>>,
    pub timeout_error: Memo<Option<String>>,
    pub idle_error: Memo<Option<String>>,
}

fn values_of(s: &McpServerView) -> Vec<String> {
    vec![
        s.name.clone(),
        s.transport.clone(),
        s.command.clone().unwrap_or_default(),
        s.args.clone(),
        s.env.clone(),
        s.cwd.clone().unwrap_or_default(),
        s.container_image.clone().unwrap_or_default(),
        s.extra_run_args.join("\n"),
        s.url.clone().unwrap_or_default(),
        s.headers.clone(),
        s.tool_prefix.clone(),
        s.timeout_ms.to_string(),
        s.idle_seconds.to_string(),
        s.sampling_alias.clone().unwrap_or_default(),
        s.autostart.to_string(),
        s.allow_sampling.to_string(),
        s.enabled.to_string(),
    ]
}

fn digits(text: &str) -> String {
    text.trim().replace([' ', '\u{2009}', ',', '_'], "")
}

pub(super) fn parse_timeout(text: &str) -> Result<u64, String> {
    digits(text)
        .parse()
        .map_err(|_| "a whole number of milliseconds".to_string())
}

pub(super) fn parse_idle(text: &str) -> Result<i64, String> {
    digits(text)
        .parse()
        .map_err(|_| "a whole number of seconds (0 = never)".to_string())
}

/// What a new server starts as (the modal editor's defaults).
pub(super) fn blank() -> McpServerView {
    McpServerView {
        transport: "stdio".into(),
        timeout_ms: 60_000,
        autostart: true,
        allow_sampling: true,
        enabled: true,
        ..Default::default()
    }
}

impl McpForm {
    pub fn new(from: &McpServerView) -> Self {
        let v = values_of(from);
        let texts: [RwSignal<String>; TEXTS] = std::array::from_fn(|i| RwSignal::new(v[i].clone()));
        let flag = |i: usize| RwSignal::new(v[i] == "true");
        let (autostart, allow_sampling, enabled) = (flag(14), flag(15), flag(16));
        let baseline = RwSignal::new(v);
        let dirty = Memo::new(move |_| {
            let mut now: Vec<String> = texts.iter().map(|t| t.get().trim().to_string()).collect();
            now.extend([autostart, allow_sampling, enabled].map(|b| b.get().to_string()));
            baseline.with(|b| {
                KEYS.iter()
                    .zip(now.iter().zip(b))
                    .filter(|(_, (n, b))| n.as_str() != b.trim())
                    .map(|(k, _)| *k)
                    .collect()
            })
        });
        let timeout = texts[11];
        let idle = texts[12];
        Self {
            texts,
            autostart,
            allow_sampling,
            enabled,
            baseline,
            dirty,
            timeout_error: Memo::new(move |_| parse_timeout(&timeout.get()).err()),
            idle_error: Memo::new(move |_| parse_idle(&idle.get()).err()),
        }
    }

    /// The text field `key` holds.
    pub fn text(self, key: &str) -> RwSignal<String> {
        let i = KEYS[..TEXTS]
            .iter()
            .position(|k| *k == key)
            .unwrap_or_else(|| panic!("no MCP form field {key}"));
        self.texts[i]
    }

    pub fn is_dirty(self, key: &'static str) -> Signal<bool> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(|d| d.contains(&key)))
    }

    pub fn dirty_count(self) -> Signal<usize> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(Vec::len))
    }

    pub fn invalid(self) -> Signal<usize> {
        let (t, i) = (self.timeout_error, self.idle_error);
        Signal::derive(move || {
            usize::from(t.with(Option::is_some)) + usize::from(i.with(Option::is_some))
        })
    }

    fn values(self) -> Vec<String> {
        let mut now: Vec<String> = self.texts.iter().map(|t| t.get_untracked()).collect();
        now.extend(
            [self.autostart, self.allow_sampling, self.enabled]
                .map(|b| b.get_untracked().to_string()),
        );
        now
    }

    fn write(self, v: &[String]) {
        for (t, v) in self.texts.iter().zip(v) {
            if t.get_untracked() != *v {
                t.set(v.clone());
            }
        }
        self.autostart.set(v[14] == "true");
        self.allow_sampling.set(v[15] == "true");
        self.enabled.set(v[16] == "true");
    }

    /// The server as the gateway has it now: the baseline moves to it, and
    /// every value the owner has not changed follows.
    pub fn merge(self, s: &McpServerView) {
        let new = values_of(s);
        let old = self.baseline.get_untracked();
        let mut now = self.values();
        for i in 0..KEYS.len() {
            if now[i].trim() == old[i].trim() {
                now[i] = new[i].clone();
            }
        }
        self.baseline.set(new);
        self.write(&now);
    }

    /// The values as they stand, taken when a save's body is built.
    pub fn snapshot(self) -> Vec<String> {
        self.values()
    }

    /// What was just saved (`sent`) is the baseline; what was typed after
    /// the body was built stays a change.
    pub fn rebase(self, sent: Vec<String>) {
        self.baseline.set(sent);
    }

    pub fn discard(self) {
        let b = self.baseline.get_untracked();
        self.write(&b);
    }

    /// The body of `mcp_server_set`: `id` 0 creates.
    pub fn body(self, id: i64) -> Result<Value, String> {
        let t = |k: &str| self.text(k).get_untracked();
        let timeout = parse_timeout(&t("timeout")).map_err(|e| format!("timeout: {e}"))?;
        let idle = parse_idle(&t("idle")).map_err(|e| format!("idle: {e}"))?;
        let create = id == 0;
        Ok(json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "name": t("name").trim(),
            "transport": t("transport"),
            "command": t("command").trim(),
            "args": t("args"),
            "env": t("env"),
            "cwd": t("cwd").trim(),
            "container_image": t("image").trim(),
            "extra_run_args": t("run_args")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>(),
            "url": t("url").trim(),
            "headers": t("headers"),
            "tool_prefix": t("prefix").trim(),
            "timeout_ms": timeout,
            "idle_seconds": idle,
            "sampling_alias": t("sampling_alias").trim(),
            "autostart": self.autostart.get_untracked(),
            "allow_sampling": self.allow_sampling.get_untracked(),
            "enabled": self.enabled.get_untracked(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_grouped_and_refuse_words() {
        assert_eq!(parse_timeout("60 000"), Ok(60_000));
        assert!(parse_timeout("a minute").is_err());
        assert_eq!(parse_idle("0"), Ok(0));
        assert_eq!(parse_idle("-1"), Ok(-1));
        assert!(parse_idle("").is_err());
    }

    #[test]
    fn every_text_key_has_a_slot() {
        assert_eq!(KEYS.len(), TEXTS + 3);
        assert_eq!(KEYS[11], "timeout");
        assert_eq!(KEYS[12], "idle");
    }
}
