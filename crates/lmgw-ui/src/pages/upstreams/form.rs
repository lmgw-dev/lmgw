//! An upstream's form: one signal per value, what it was loaded from (the
//! baseline a change is measured against), the values that do not read,
//! and the body `upstream_set_full` takes — the same payload the modal
//! editor posted.
//!
//! Secrets follow the house convention: an empty API key keeps the stored
//! one (so a typed key is a change, an empty one never is), and header
//! values round-trip as "<set>".

use leptos::prelude::*;
use lmgw_api_types::UpstreamView;
use serde_json::{json, Value};

use super::{kind_after_protocol, OPENAI_COMPATIBLE};

/// Every value the form edits, as text (switches as "true"/"false"), in
/// one order: what the baseline and the dirty count compare.
const KEYS: [&str; 12] = [
    "name",
    "protocol",
    "kind",
    "base_url",
    "api_key",
    "headers",
    "timeout",
    "expose_all",
    "expose_prefix",
    "supports_responses",
    "catalog_all_modalities",
    "enabled",
];

#[derive(Clone, Copy)]
pub(super) struct UpForm {
    pub name: RwSignal<String>,
    pub protocol: RwSignal<String>,
    pub kind: RwSignal<String>,
    pub base_url: RwSignal<String>,
    /// Typed only: empty keeps the stored key.
    pub api_key: RwSignal<String>,
    /// `Name: Value` per line.
    pub headers: RwSignal<String>,
    pub timeout: RwSignal<String>,
    pub expose_all: RwSignal<bool>,
    pub expose_prefix: RwSignal<String>,
    pub supports_responses: RwSignal<bool>,
    pub catalog_all_modalities: RwSignal<bool>,
    pub enabled: RwSignal<bool>,
    baseline: RwSignal<Vec<String>>,
    /// The protocol the kind last followed: a write of a whole row (a
    /// merge, Discard) moves it with the protocol, so its kind stands.
    followed: StoredValue<String>,
    /// The keys whose value differs from the baseline.
    pub dirty: Memo<Vec<&'static str>>,
    /// Why the timeout does not read, if it does not.
    pub timeout_error: Memo<Option<String>>,
    /// Why the headers do not read, if they do not.
    pub headers_error: Memo<Option<String>>,
}

/// The headers as the row edits them, one `Name: Value` per line.
fn headers_text(u: &UpstreamView) -> String {
    u.extra_headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The values of `u` in [`KEYS`]'s order; the API key is never loaded.
fn values_of(u: &UpstreamView) -> Vec<String> {
    vec![
        u.name.clone(),
        u.protocol.clone(),
        u.kind.clone(),
        u.base_url.clone(),
        String::new(),
        headers_text(u),
        u.timeout_ms.to_string(),
        u.expose_all.to_string(),
        u.expose_prefix.clone(),
        u.supports_responses.to_string(),
        u.catalog_all_modalities.to_string(),
        u.enabled.to_string(),
    ]
}

/// Where the API key sits in [`KEYS`].
const KEY_AT: usize = 4;

/// The baseline a save of `sent` leaves: the key is never loaded, so it is
/// empty there once stored.
fn baseline_after(mut sent: Vec<String>) -> Vec<String> {
    sent[KEY_AT].clear();
    sent
}

/// The extra headers out of their lines, or the line that is not
/// `Name: Value`.
pub(super) fn parse_headers(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once(':') {
            Some((k, v)) => out.push((k.trim().into(), v.trim().into())),
            None => {
                return Err(format!(
                    "\u{201c}{line}\u{201d} is not \u{201c}Name: Value\u{201d}"
                ))
            }
        }
    }
    Ok(out)
}

/// A timeout as typed, in milliseconds.
pub(super) fn parse_timeout(text: &str) -> Result<u64, String> {
    let t = text.trim().replace([' ', '\u{2009}', ',', '_'], "");
    t.parse()
        .map_err(|_| "a whole number of milliseconds (0 = no limit)".to_string())
}

/// What a new upstream starts as (the modal editor's defaults).
pub(super) fn blank() -> UpstreamView {
    UpstreamView {
        timeout_ms: 120_000,
        enabled: true,
        protocol: OPENAI_COMPATIBLE.into(),
        kind: "generic".into(),
        catalog_all_modalities: true,
        ..Default::default()
    }
}

impl UpForm {
    pub fn new(from: &UpstreamView) -> Self {
        let v = values_of(from);
        let name = RwSignal::new(v[0].clone());
        let protocol = RwSignal::new(v[1].clone());
        let kind = RwSignal::new(v[2].clone());
        let base_url = RwSignal::new(v[3].clone());
        let api_key = RwSignal::new(String::new());
        let headers = RwSignal::new(v[5].clone());
        let timeout = RwSignal::new(v[6].clone());
        let expose_all = RwSignal::new(from.expose_all);
        let expose_prefix = RwSignal::new(v[8].clone());
        let supports_responses = RwSignal::new(from.supports_responses);
        let catalog_all_modalities = RwSignal::new(from.catalog_all_modalities);
        let enabled = RwSignal::new(from.enabled);
        let followed = StoredValue::new(v[1].clone());
        let baseline = RwSignal::new(v);
        let now = move || {
            vec![
                name.get().trim().to_string(),
                protocol.get(),
                kind.get(),
                base_url.get().trim().to_string(),
                api_key.get(),
                headers.get().trim().to_string(),
                timeout.get().trim().to_string(),
                expose_all.get().to_string(),
                expose_prefix.get().trim().to_string(),
                supports_responses.get().to_string(),
                catalog_all_modalities.get().to_string(),
                enabled.get().to_string(),
            ]
        };
        let dirty = Memo::new(move |_| {
            let now = now();
            baseline.with(|b| {
                KEYS.iter()
                    .zip(now.iter().zip(b))
                    .filter(|(_, (n, b))| n.as_str() != b.trim())
                    .map(|(k, _)| *k)
                    .collect()
            })
        });
        let timeout_error = Memo::new(move |_| parse_timeout(&timeout.get()).err());
        let headers_error = Memo::new(move |_| parse_headers(&headers.get()).err());
        let form = Self {
            name,
            protocol,
            kind,
            base_url,
            api_key,
            headers,
            timeout,
            expose_all,
            expose_prefix,
            supports_responses,
            catalog_all_modalities,
            enabled,
            baseline,
            followed,
            dirty,
            timeout_error,
            headers_error,
        };
        form.follow_protocol();
        form
    }

    /// The kind follows the protocol into and out of llama.cpp and the
    /// OpenAI platform (llama egress design §5, openai egress design §1.5):
    /// the form always posts `kind`, so without the reset a switch would post
    /// a pair the server refuses, such as openai + audio_cpp.
    fn follow_protocol(self) {
        let (protocol, kind, followed) = (self.protocol, self.kind, self.followed);
        Effect::new(move |_| {
            let p = protocol.get();
            let Some(prev) = followed.try_get_value() else {
                return;
            };
            if prev != p {
                followed.set_value(p.clone());
                let k = kind_after_protocol(&prev, &p, &kind.get_untracked());
                if k != kind.get_untracked() {
                    kind.set(k);
                }
            }
        });
    }

    /// Whether `key` has an unsaved change.
    pub fn is_dirty(self, key: &'static str) -> Signal<bool> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(|d| d.contains(&key)))
    }

    pub fn dirty_count(self) -> Signal<usize> {
        let dirty = self.dirty;
        Signal::derive(move || dirty.with(Vec::len))
    }

    /// The values that do not read: they block Save.
    pub fn invalid(self) -> Signal<usize> {
        let (t, h) = (self.timeout_error, self.headers_error);
        Signal::derive(move || {
            usize::from(t.with(Option::is_some)) + usize::from(h.with(Option::is_some))
        })
    }

    fn write(self, values: &[String]) {
        let flag = |s: &str| s == "true";
        self.name.set(values[0].clone());
        // A whole row's protocol and kind are a pair already.
        self.followed.set_value(values[1].clone());
        self.protocol.set(values[1].clone());
        self.kind.set(values[2].clone());
        self.base_url.set(values[3].clone());
        self.api_key.set(values[4].clone());
        self.headers.set(values[5].clone());
        self.timeout.set(values[6].clone());
        self.expose_all.set(flag(&values[7]));
        self.expose_prefix.set(values[8].clone());
        self.supports_responses.set(flag(&values[9]));
        self.catalog_all_modalities.set(flag(&values[10]));
        self.enabled.set(flag(&values[11]));
    }

    fn read_untracked(self) -> Vec<String> {
        vec![
            self.name.get_untracked(),
            self.protocol.get_untracked(),
            self.kind.get_untracked(),
            self.base_url.get_untracked(),
            self.api_key.get_untracked(),
            self.headers.get_untracked(),
            self.timeout.get_untracked(),
            self.expose_all.get_untracked().to_string(),
            self.expose_prefix.get_untracked(),
            self.supports_responses.get_untracked().to_string(),
            self.catalog_all_modalities.get_untracked().to_string(),
            self.enabled.get_untracked().to_string(),
        ]
    }

    /// The row as the gateway has it now (a reload after a save, an Enable
    /// from the menu): the baseline moves to it, and every value the owner
    /// has not changed follows; the ones they changed stay as typed.
    pub fn merge(self, u: &UpstreamView) {
        let new = values_of(u);
        let old = self.baseline.get_untracked();
        let mut now = self.read_untracked();
        for i in 0..KEYS.len() {
            if now[i].trim() == old[i].trim() {
                now[i] = new[i].clone();
            }
        }
        self.baseline.set(new);
        self.write(&now);
    }

    /// The values as they stand, taken when a save's body is built: what
    /// the save posts, and so what it rebases to.
    pub fn snapshot(self) -> Vec<String> {
        self.read_untracked()
    }

    /// What was just saved (`sent`) is the baseline: the API key sent is
    /// stored now (it leaves the field unless another was typed since), and
    /// whatever was typed after the body was built stays a change.
    pub fn rebase(self, sent: Vec<String>) {
        if self.api_key.get_untracked() == sent[KEY_AT] {
            self.api_key.set(String::new());
        }
        self.baseline.set(baseline_after(sent));
    }

    /// Back to the baseline.
    pub fn discard(self) {
        let b = self.baseline.get_untracked();
        self.write(&b);
    }

    /// The body of `upstream_set_full`: `id` 0 creates. `Err` names what
    /// does not read.
    pub fn body(self, id: i64) -> Result<Value, String> {
        let headers = parse_headers(&self.headers.get_untracked())
            .map_err(|e| format!("extra headers: {e}"))?;
        let timeout =
            parse_timeout(&self.timeout.get_untracked()).map_err(|e| format!("timeout: {e}"))?;
        let compatible = self.protocol.with_untracked(|p| p == OPENAI_COMPATIBLE);
        let create = id == 0;
        Ok(json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "name": self.name.get_untracked().trim(),
            "protocol": self.protocol.get_untracked(),
            "kind": self.kind.get_untracked(),
            "base_url": self.base_url.get_untracked().trim(),
            "api_key": self.api_key.get_untracked(),
            "extra_headers": headers,
            "timeout_ms": timeout,
            "expose_all": self.expose_all.get_untracked(),
            "expose_prefix": self.expose_prefix.get_untracked().trim(),
            // A choice on openai_compatible rows only (openai egress design,
            // decision 8); the switch is hidden everywhere else.
            "supports_responses": self.supports_responses.get_untracked() && compatible,
            // Read on an openai_compatible row only; kept as set on the others.
            "catalog_all_modalities": self.catalog_all_modalities.get_untracked(),
            "enabled": self.enabled.get_untracked(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_read_one_name_and_value_per_line() {
        assert_eq!(
            parse_headers("X-Title: lmgw\n\n  HTTP-Referer: https://example.com  \n"),
            Ok(vec![
                ("X-Title".to_string(), "lmgw".to_string()),
                (
                    "HTTP-Referer".to_string(),
                    "https://example.com".to_string()
                ),
            ])
        );
        assert!(parse_headers("no colon here").is_err());
        assert_eq!(parse_headers(""), Ok(Vec::new()));
    }

    #[test]
    fn a_save_rebases_to_what_it_sent_without_the_key() {
        let mut sent = values_of(&blank());
        sent[0] = "a".into();
        sent[KEY_AT] = "sk-sent".into();
        let b = baseline_after(sent.clone());
        assert_eq!(b[0], "a");
        assert_eq!(b[KEY_AT], "");
        // Text typed after the body was built differs from the baseline.
        assert_ne!(b[0], "ab");
        assert_eq!(b.len(), sent.len());
    }

    #[test]
    fn a_timeout_reads_grouped_digits_and_refuses_words() {
        assert_eq!(parse_timeout("120000"), Ok(120_000));
        assert_eq!(parse_timeout("120 000"), Ok(120_000));
        assert_eq!(parse_timeout(" 0 "), Ok(0));
        assert!(parse_timeout("two minutes").is_err());
        assert!(parse_timeout("-1").is_err());
    }
}
