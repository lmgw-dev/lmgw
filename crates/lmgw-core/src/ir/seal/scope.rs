//! Which upstream row, as configured, a seal is for (WP8 review W-R2): the
//! reasoning is encrypted for the organization and project the row's key
//! belongs to, and an owner can give a row another key (another project,
//! another organization), another address or another `OpenAI-Project`
//! header without its id changing. A seal records the configuration it was
//! answered with as a fingerprint, and goes back only while the row still
//! has it; after a change the turn replays as on a row that never sealed
//! it, its reasoning text kept. The fingerprint is an HMAC under the
//! install's secret seal key (`super::key`, WP8 verify V-N2), never the key
//! in clear: a carrier (`Seal::carrier`) goes to the client, which cannot
//! check a guessed provider key against it, nor see a rotation.

use aws_lc_rs::hmac;

use crate::config::Upstream;

/// The row a seal goes back to and the configuration it must still have.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SealScope {
    /// The row's `upstreams.id`. `0` only for a decoder no send site made
    /// (`Egress::new_decoder` in a test), which matches no row.
    pub upstream_id: i64,
    /// [`config_fingerprint`] of the row as it answered.
    pub config: String,
}

impl SealScope {
    pub fn new(upstream_id: i64, config: impl Into<String>) -> Self {
        Self {
            upstream_id,
            config: config.into(),
        }
    }

    /// The scope of `up` as it is configured now.
    pub fn of(up: &Upstream) -> Self {
        Self::new(up.id, config_fingerprint(up))
    }
}

/// What a row's answers are encrypted for, keyed: its address, its key and
/// its extra headers (an `OpenAI-Organization` or `OpenAI-Project` header
/// picks the project), as `catalog/read_key.rs` keys a catalog read by the
/// configuration it went out with. 128 bits of HMAC-SHA256 under the
/// install's seal key (`super::key`), hex: an equality check between two
/// configurations of one row, which no one without the key can compute.
pub fn config_fingerprint(up: &Upstream) -> String {
    fingerprint_under(super::key::key(), up)
}

/// [`config_fingerprint`] under `key`.
fn fingerprint_under(key: &[u8], up: &Upstream) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut h = hmac::Context::with_key(&key);
    h.update(b"lmgw seal config v2\0");
    h.update(up.base().as_bytes());
    h.update(&[0]);
    h.update(up.api_key.as_deref().unwrap_or_default().as_bytes());
    h.update(&[0]);
    let mut headers: Vec<(String, &str)> = up
        .extra_headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str()))
        .collect();
    headers.sort();
    for (k, v) in headers {
        h.update(k.as_bytes());
        h.update(b":");
        h.update(v.as_bytes());
        h.update(&[0]);
    }
    hex::encode(&h.sign().as_ref()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Upstream {
        Upstream {
            id: 4,
            name: "openai".into(),
            protocol: crate::config::Protocol::Openai,
            kind: crate::config::UpstreamKind::Generic,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("sk-one".into()),
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        }
    }

    #[test]
    fn the_scope_changes_with_the_key_the_address_or_a_header_and_hides_the_key() {
        let base = SealScope::of(&row());
        assert_eq!(base.upstream_id, 4);
        assert_eq!(base.config.len(), 32);
        assert!(!base.config.contains("sk-one"));
        assert_eq!(SealScope::of(&row()), base, "stable");
        let key = Upstream {
            api_key: Some("sk-two".into()),
            ..row()
        };
        let address = Upstream {
            base_url: "https://eu.api.openai.com/v1".into(),
            ..row()
        };
        let project = Upstream {
            extra_headers: vec![("OpenAI-Project".into(), "proj_b".into())],
            ..row()
        };
        for other in [key, address, project] {
            assert_ne!(SealScope::of(&other).config, base.config);
        }
    }

    /// The fingerprint is keyed (WP8 verify V-N2): another install's key
    /// gives another one for the same row, and the installed key is this
    /// one's.
    #[test]
    fn the_fingerprint_is_keyed_by_the_install() {
        let one = fingerprint_under(&[1; 32], &row());
        assert_eq!(one.len(), 32);
        assert_ne!(one, fingerprint_under(&[2; 32], &row()));
        assert_eq!(
            config_fingerprint(&row()),
            fingerprint_under(super::super::key::key(), &row())
        );
    }
}
