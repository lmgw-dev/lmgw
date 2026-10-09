//! Pairing (client-apps design §1.4): the plaintext a device key is minted
//! with, and the `lmgw-pair:` link that hands it to the client once.

/// The name prefix a device row carries, written by the server (§1.1).
pub const NAME_PREFIX: &str = "device:";

/// The plaintext prefix, so a leaked string says what it is (§1.1).
pub const KEY_PREFIX: &str = "lmgw-device-";

/// The note a loopback `url` carries on the pairing form (§1.4).
pub const LOOPBACK_NOTE: &str = "reachable from this computer only";

/// A new device key: [`KEY_PREFIX`] and 64 hex characters (256 bits), the
/// owner key's shape.
pub fn mint() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("{KEY_PREFIX}{}", hex::encode(bytes))
}

/// The pairing link (§1.4): `lmgw-pair:?v=1&url=…&name=…&key=…`.
///
/// `name` is the device's own name, without the `device:` prefix. Each value
/// is percent-encoded with RFC 3986's unreserved set kept, so a client may
/// parse the query with any URL library: no `+` for a space, which only form
/// decoding reads as one. `fp=sha256:…` is reserved for a TLS listener, which
/// lmgw does not have, so no link carries it.
pub fn pairing_link(url: &str, name: &str, key: &str) -> String {
    format!(
        "lmgw-pair:?v=1&url={}&name={}&key={}",
        encode(url),
        encode(name),
        encode(key)
    )
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Why `label` cannot be a device's hosting label, or `None` when it can
/// (§1.5).
///
/// Validated like a server's `tool_prefix`: the characters an exposed tool
/// name can carry, not one of lmgw's own namespaces, and not already a
/// registered server's prefix or name — or another device's label, which the
/// unique index also backstops. `own_id` is the device being written, so
/// restating its own label is not a clash.
// The backstop is migration 0062's case-insensitive unique index (review
// W2-25). Created on a database that already held two labels differing only
// in case, it would fail and stop the start. That cannot happen to a
// shipped install (0061 and 0062 shipped together, and labels are ASCII,
// so NOCASE folds exactly what these checks fold), and 0062 is left as it
// is: editing it would change its checksum (review W3-16). Should it ever be
// needed, the remedy is a repair ahead of the migrator,
// `store/migration_guards.rs::repair_before_migrations`, as 0018 got: clear
// the label of all but the lowest id of each colliding group, with a warn
// line, before 0062 runs.
pub fn label_refusal(
    snap: &crate::config::Snapshot,
    label: &str,
    own_id: Option<i64>,
) -> Option<String> {
    if label.is_empty() {
        return Some("a hosting label cannot be empty — leave it out for no grant".into());
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some(format!(
            "hosting label '{label}' may only contain letters, digits, '_' and '-' — it becomes \
             the prefix of the device's tool names ('{label}__…')"
        ));
    }
    if label.contains("__") {
        return Some(format!(
            "hosting label '{label}' cannot contain '__', the separator between a prefix and a \
             tool name"
        ));
    }
    if label.ends_with('_') {
        return Some(format!(
            "hosting label '{label}' cannot end in '_': its tools' names would run into the \
             '__' separator ('{label}__…') and into another label's namespace"
        ));
    }
    let folded = label.to_ascii_lowercase();
    if crate::mcp::RESERVED_NAMESPACES
        .iter()
        .any(|(p, _)| *p == folded)
    {
        return Some(format!(
            "'{label}' is one of lmgw's own tool namespaces ({}) — pick another hosting label",
            crate::mcp::RESERVED_NAMESPACES
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // A device's own hosted-tools row carries its label (client-apps
    // design §5.2): another device's is said by the key check below.
    if let Some(s) = snap
        .mcp_servers
        .values()
        .filter(|s| !s.is_device())
        .find(|s| {
            namespaces_overlap(&s.tool_prefix, label)
                || s.name.eq_ignore_ascii_case(label)
                // Its name as a prefix: what a collision gives its tools.
                || namespaces_overlap(&crate::mcp::names::name_qualifier(&s.name), label)
        })
    {
        return Some(format!(
            "'{label}' is already the MCP server '{}', or its namespace runs into it — its tools \
             are named '{}__…' — pick another hosting label",
            s.name,
            if s.tool_prefix.is_empty() {
                &s.name
            } else {
                &s.tool_prefix
            }
        ));
    }
    if let Some(k) = snap.api_keys.iter().find(|k| {
        Some(k.id) != own_id
            && k.hosts_label
                .as_deref()
                .is_some_and(|l| namespaces_overlap(l, label))
    }) {
        return Some(format!(
            "device '{}' already hosts tools under '{}' — one label, one device, and no label's \
             namespace inside another's",
            super::short_name(&k.name),
            k.hosts_label.as_deref().unwrap_or_default()
        ));
    }
    None
}

/// Whether the namespaces of two prefixes, `<a>__…` and `<b>__…`, share a
/// name: one starts with the other (`desktop` and `desktop_`, whose names
/// begin `desktop___`; `desktop` and `desktop__x`). Case-insensitive, as
/// labels are compared. An empty prefix has no namespace.
pub(crate) fn namespaces_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let a = format!("{}__", a.to_ascii_lowercase());
    let b = format!("{}__", b.to_ascii_lowercase());
    a.starts_with(&b) || b.starts_with(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_key_says_what_it_is() {
        let k = mint();
        assert!(k.starts_with("lmgw-device-"), "{k}");
        assert_eq!(k.len(), KEY_PREFIX.len() + 64);
        assert_ne!(k, mint(), "256 random bits, every time");
    }

    #[test]
    fn the_link_is_the_spec_s_shape_and_any_url_parser_reads_it() {
        let link = pairing_link("http://127.0.0.1:8001", "desktop", "lmgw-device-ab12");
        assert_eq!(
            link,
            "lmgw-pair:?v=1&url=http%3A%2F%2F127.0.0.1%3A8001&name=desktop&key=lmgw-device-ab12"
        );
        // A space is %20, never `+`.
        assert!(pairing_link("http://h:1", "my phone", "k").contains("name=my%20phone"));
    }

    /// A label ending in `_`, or whose namespace runs into a server's
    /// prefix or another device's label, is refused (review finding 2).
    #[test]
    fn a_label_may_not_run_into_another_namespace() {
        use crate::config::{ApiKey, ApiKeyKind, KeyPolicy, McpServer, McpTransport, Snapshot};
        let mut snap = Snapshot::default();
        let server = |id: i64, name: &str, prefix: &str| McpServer {
            id,
            name: name.into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: None,
            headers: vec![],
            tool_prefix: prefix.into(),
            timeout_ms: 60_000,
            autostart: false,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
            device_key_id: None,
        };
        snap.mcp_servers.insert(1, server(1, "under", "web_"));
        snap.mcp_servers.insert(2, server(2, "deep", "git__hub"));
        snap.mcp_servers.insert(3, server(3, "phone.", ""));
        snap.api_keys.push(ApiKey {
            id: 7,
            name: "device:desk".into(),
            key_hash: String::new(),
            enabled: true,
            kind: ApiKeyKind::Device,
            key_plain: None,
            agent_id: None,
            policy: KeyPolicy::default(),
            note: String::new(),
            hosts_label: Some("desk".into()),
            self_admin: crate::config::DeviceAdmin::Off,
        });
        let refused = |l: &str| label_refusal(&snap, l, None);
        assert!(refused("tablet_").is_some_and(|w| w.contains("cannot end in '_'")));
        assert!(
            refused("web").is_some_and(|w| w.contains("'under'")),
            "web_ → web___…"
        );
        assert!(
            refused("git").is_some_and(|w| w.contains("'deep'")),
            "git__hub → git__hub__…"
        );
        assert!(refused("DESK").is_some_and(|w| w.contains("one label, one device")));
        assert!(
            refused("phone").is_some_and(|w| w.contains("'phone.'")),
            "phone. → phone___…, a collision's names"
        );
        assert_eq!(refused("phon"), None);
        assert_eq!(refused("webx"), None);
        assert_eq!(refused("desktop"), None);
        assert_eq!(label_refusal(&snap, "desk", Some(7)), None, "its own label");
        assert!(namespaces_overlap("a", "a_") && namespaces_overlap("a_", "a"));
        assert!(!namespaces_overlap("a", "ab") && !namespaces_overlap("", "a"));
    }
}
