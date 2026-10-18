//! Allow patterns that name a paired device's hosted label the key holds no
//! device-tools grant for (client-apps design L16 as changed 2026-10-15).
//!
//! Such a pattern filters tools its key cannot reach. Migration 0082 leaves
//! exactly these behind on a key whose `allow` list mixed a naming line with
//! a broad one (`*` and `kai__notify`): granting it would have widened its
//! reach, so it lost the narrow reach instead. The boot log names each key
//! and label so the owner can grant it on purpose; the Keys page flags the
//! same pattern.

use crate::config::{ApiKey, ApiKeyKind, ScopeMode};

/// `(key name, label)` for every client key or device whose `allow` tool
/// patterns name a label a device hosts that its grant does not hold. A
/// device's own label is not counted: it reaches it anyway.
pub fn ungranted_patterns(keys: &[ApiKey]) -> Vec<(String, String)> {
    let hosted: Vec<(i64, String)> = keys
        .iter()
        .filter(|k| k.kind == ApiKeyKind::Device)
        .filter_map(|k| Some((k.id, k.hosts_label.as_deref()?.trim().to_string())))
        .filter(|(_, l)| !l.is_empty())
        .collect();
    let mut out = Vec::new();
    for k in keys {
        if !matches!(k.kind, ApiKeyKind::Key | ApiKeyKind::Device)
            || k.policy.tool_scope_mode != ScopeMode::Allow
        {
            continue;
        }
        let labels: Vec<String> = hosted
            .iter()
            .filter(|(id, _)| *id != k.id)
            .map(|(_, l)| l.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for label in lmgw_api_types::scope::ungranted_named(
            &k.policy.tool_scope_patterns,
            &k.policy.device_tools,
            &labels,
        ) {
            out.push((k.name.clone(), label.to_string()));
        }
    }
    out
}

/// Log one warning per key and label [`ungranted_patterns`] finds.
pub fn warn_at_boot(keys: &[ApiKey]) {
    for (name, label) in ungranted_patterns(keys) {
        tracing::warn!(
            "key '{name}': its tool scope names '{label}__…', a paired device's tools, but it \
             holds no Device tools grant for '{label}', so it reaches none of them; grant it \
             under the key's Device tools and its allow patterns will then filter the label's \
             tools"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyPolicy;

    fn key(id: i64, name: &str, kind: ApiKeyKind, mode: ScopeMode, patterns: &str) -> ApiKey {
        ApiKey {
            id,
            name: name.into(),
            kind,
            policy: KeyPolicy {
                tool_scope_mode: mode,
                tool_scope_patterns: patterns.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_pattern_naming_an_ungranted_hosted_label_is_found() {
        let mut host = key(1, "device:desk", ApiKeyKind::Device, ScopeMode::All, "");
        host.hosts_label = Some("kai".into());
        let mixed = key(2, "ci", ApiKeyKind::Key, ScopeMode::Allow, "*\nKAI__notify");
        let mut granted = key(3, "ok", ApiKeyKind::Key, ScopeMode::Allow, "kai__*");
        granted.policy.device_tools = vec!["Kai".into()];
        let deny = key(4, "deny", ApiKeyKind::Key, ScopeMode::Deny, "kai__*");
        let mut own = key(
            5,
            "device:mine",
            ApiKeyKind::Device,
            ScopeMode::Allow,
            "mine__*",
        );
        own.hosts_label = Some("mine".into());
        let got = ungranted_patterns(&[host, mixed, granted, deny, own]);
        assert_eq!(got, vec![("ci".to_string(), "kai".to_string())]);
    }
}
