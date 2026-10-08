//! The device half of the credential ops (client-apps design §1.4).
//!
//! A device key is created with its whole policy and its pairing link, and
//! rotated by re-pairing: it is stored as a hash only (L1), so Rotate mints a
//! new key on the same row — its id, policy and history stay — and ends every
//! connection the old one holds. `key_reveal` refuses a device row, and
//! `key_delete` ends its connections as it deletes it.

use serde_json::{json, Map, Value};

use super::{bad_request, str_arg};
use crate::config::ApiKey;
use crate::devices::{self, RevokeReason};
use crate::ops::KeyPatch;
use crate::principal::Refusal;
use crate::state::SharedState;
use crate::store;

/// `key_create { kind: "device", name, hosts_label?, self_admin?, url?,
/// <policy> }` → `{ id, name, key, link, url, url_note }`.
///
/// `<policy>` is `key_set`'s fields — alias and tool scope, budget, limits,
/// expiry, note — so the pairing form's confirmed scope is what the row is
/// born with. Left out, a field takes the default the form prefills: `all`
/// and no budget (§11 Q1).
pub(super) async fn create(
    st: &SharedState,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value, Refusal> {
    let bare = name
        .strip_prefix(devices::NAME_PREFIX)
        .unwrap_or(name)
        .trim();
    if bare.is_empty() {
        return Err(bad_request("device name is required"));
    }
    // It is said in close reasons, log lines and takeover messages (review
    // W2-23): printable text, and no `:`, which the name's own prefix uses.
    // Nor invisible format characters (review W3-17): a right-to-left
    // override or a zero-width joiner would make a close reason or a
    // takeover message read as something else.
    if bare
        .chars()
        .any(|c| c.is_control() || c == ':' || is_format(c) || is_separator(c))
    {
        return Err(bad_request(
            "a device name is printable text without ':' or invisible format characters — \
             e.g. 'desktop' or 'phone'",
        ));
    }
    let name = format!("{}{bare}", devices::NAME_PREFIX);
    let snap = st.snapshot();
    if let Some(k) = snap.api_keys.iter().find(|k| k.name == name) {
        return Err(bad_request(
            if k.kind == crate::config::ApiKeyKind::Device {
                format!(
                "a device named '{bare}' is already paired — pick another name, or rotate that one"
            )
            } else {
                let holder = match k.kind {
                    crate::config::ApiKeyKind::Owner => "an owner key",
                    crate::config::ApiKeyKind::Agent => "an agent token",
                    crate::config::ApiKeyKind::Internal => "an internal identity",
                    _ => "a client key",
                };
                format!(
                    "'{name}' is already the name of {holder} — pick another name for the device"
                )
            },
        ));
    }
    let patch: KeyPatch = crate::ops::patch_from_args(Some(args.clone())).map_err(bad_request)?;
    let policy = crate::ops::policy_from_patch(&patch).map_err(bad_request)?;
    let hosts_label = patch
        .hosts_label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    if let Some(label) = hosts_label {
        if let Some(why) = devices::label_refusal(&snap, label, None) {
            return Err(bad_request(why));
        }
    }
    let url = pairing_url(st, args)?;

    let plaintext = devices::mint();
    let note = patch.note.as_deref().map(str::trim).unwrap_or_default();
    // Born with the level the pairing form confirmed (off unless asked): a
    // device is never, even for a moment, allowed more.
    let self_admin = match patch.self_admin.as_ref() {
        Some(v) => crate::ops::parse_device_admin(v).map_err(bad_request)?,
        None => crate::config::DeviceAdmin::Off,
    };
    let id = store::insert_device_key(
        &st.db,
        &name,
        &crate::config::hash_api_key(&plaintext),
        &policy,
        (hosts_label, self_admin),
        note,
    )
    .await
    .map_err(|e| bad_request(e.to_string()))?;
    // Past the insert the key exists and only this answer holds it: a reload
    // that fails is said beside it, never a refusal that throws it away
    // (review W2-6).
    let reloaded = st.reload_snapshot().await.err();
    let link = devices::pairing_link(&url, bare, &plaintext);
    // The one link lmgw's own window may hand to the desktop for it (W2-1).
    st.devices.minted.register(id, &link);
    Ok(json!({
        "ok": true,
        "id": id,
        "name": name,
        "key": plaintext,
        "link": link,
        "url": url,
        "url_note": loopback(&url).then_some(devices::LOOPBACK_NOTE),
        "message": format!(
            "device '{bare}' paired — open or copy the link now, it is not shown again{}",
            unloaded(reloaded.as_ref())
        ),
    }))
}

/// `key_rotate { id, url? }` on a device row: a new key on the same row, a
/// new link, and every connection of the old key ended (§1.6).
pub(super) async fn rotate(
    st: &SharedState,
    key: &ApiKey,
    args: &Map<String, Value>,
) -> Result<Value, Refusal> {
    let url = pairing_url(st, args)?;
    let plaintext = devices::mint();
    let rows = store::set_device_key_hash(&st.db, key.id, &crate::config::hash_api_key(&plaintext))
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    if rows == 0 {
        return Err(bad_request(format!(
            "device key {} disappeared before it could be rotated — nothing was changed",
            key.id
        )));
    }
    // The old key is dead in the table: its connections end now, whether
    // or not the reload succeeds, and the new key goes back to the owner
    // either way (review W2-6).
    let reloaded = st.reload_snapshot().await.err();
    if reloaded.is_some() {
        st.key_written(
            key.id,
            crate::state::KeyWritten::Rehashed {
                hash: crate::config::hash_api_key(&plaintext),
                plain: None,
            },
        );
    }
    devices::revoke(st, key.id, RevokeReason::Rotated);
    let bare = devices::short_name(&key.name);
    let link = devices::pairing_link(&url, bare, &plaintext);
    st.devices.minted.register(key.id, &link);
    Ok(json!({
        "ok": true,
        "id": key.id,
        "name": key.name,
        "key": plaintext,
        "link": link,
        "url": url,
        "url_note": loopback(&url).then_some(devices::LOOPBACK_NOTE),
        "message": format!(
            "device '{bare}' is rotated — its connections are closed; pair it again with the new \
             link{}",
            unloaded(reloaded.as_ref())
        ),
    }))
}

/// What a reload that failed after the write leaves to say: the key is
/// written, and the gateway serves the previous configuration until the next
/// reload.
fn unloaded(e: Option<&crate::error::GatewayError>) -> String {
    match e {
        None => String::new(),
        Some(e) => format!(
            " (the key is written, but the configuration could not be reloaded: {e} — it takes \
             effect at the next reload)"
        ),
    }
}

/// After a delete: a deleted key's connections and streams end now (§1.6) —
/// a device's, and every other kind's (*changed 2026-10-06*).
pub(super) fn deleted(st: &SharedState, key: &ApiKey) {
    devices::revoke(st, key.id, RevokeReason::Deleted);
}

/// The refusal `key_reveal` gives a device row (L1, §11 Q5).
pub(super) fn reveal_refusal(key: &ApiKey) -> Refusal {
    bad_request(format!(
        "'{}' is a device key: only its hash is stored, so it cannot be shown again — rotate \
         it to pair the device again",
        key.name
    ))
}

/// The URL the link points the device at: the form's, or this gateway's
/// primary address (`net::primary_base_url`). A phone needs the tunnel's
/// name, which only the owner knows, so it stays editable (§1.4).
fn pairing_url(st: &SharedState, args: &Map<String, Value>) -> Result<String, Refusal> {
    let url = str_arg(args, "url")
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| crate::net::primary_base_url(&st.snapshot().settings.bind_addr));
    match reqwest::Url::parse(&url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(url),
        _ => Err(bad_request(format!(
            "'{url}' is not an address a device can dial — use http://host:port (or https://…, \
             through a tunnel)"
        ))),
    }
}

/// Is `url` this computer only (§1.4's note)?
fn loopback(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or_default();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// Unicode's format characters (general category Cf), every one of them as
/// of Unicode 16 (review W4-19): direction overrides and isolates,
/// zero-width characters, the soft hyphen, the byte-order mark, the
/// prepended number marks, the hieroglyph, shorthand and musical format
/// controls, and the tag characters.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// The line and paragraph separators (general categories Zl, Zp): neither a
/// control nor a format character, and a line break all the same in a close
/// reason or a log line (review W4-19).
fn is_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}
