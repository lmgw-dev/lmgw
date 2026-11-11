//! [`TlsListenerSlot`]: what the gateway knows of its main TLS listener (the
//! one running now, or why none runs), the [`crate::net::ServingTls`] it
//! publishes on the snapshot for every URL builder, and Settings' view of it
//! with its warnings (headless server design §4.3, D37).

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use lmgw_api_types as wire;

use super::{name_warnings, Loaded, MainTls};
use crate::config::{Settings, TlsListenerConfig};
use crate::net::{Scheme, ServingTls};
use crate::state::AppState;

/// From how many days before its end a leaf's validity is a warning on the
/// TLS card and in the log. A display choice, not a limit: nothing is
/// refused or reloaded by it, and an expired leaf keeps being served until a
/// renewed one is at its paths.
pub const EXPIRY_NOTICE_DAYS: i64 = 14;

/// What the gateway knows of its TLS listener, for Settings and the URL
/// builders: the one running now, or why none runs.
#[derive(Debug, Default)]
pub struct TlsListenerSlot {
    inner: Mutex<SlotState>,
}

#[derive(Debug, Default)]
struct SlotState {
    /// Each start's number, so the end of an old start never clears a newer
    /// one (a restart starts the next before the old one has drained).
    next: u64,
    /// Serving: the start's number, the pair, the bound address and what
    /// it started with.
    running: Option<(u64, Arc<MainTls>, std::net::SocketAddr, TlsListenerConfig)>,
    /// Why the last start did not bring it up.
    error: Option<String>,
    /// What the last start served, kept past its stop: the TLS listener the
    /// running app containers' origin was built on, until a start decides
    /// otherwise.
    last: Option<ServingTls>,
}

impl TlsListenerSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A start serves `tls` on `addr`, started with `config`: published on
    /// `state`'s snapshot ([`AppState::set_serving_tls`]) under this slot's
    /// lock, so an older start's [`Self::stopped`] never publishes over it.
    /// Its number, for [`Self::stopped`].
    pub fn serving(
        &self,
        state: &AppState,
        tls: Arc<MainTls>,
        addr: std::net::SocketAddr,
        config: TlsListenerConfig,
    ) -> u64 {
        let mut g = self.lock();
        g.next += 1;
        let id = g.next;
        g.running = Some((id, tls, addr, config.clone()));
        g.error = None;
        let serving = ServingTls { addr, config };
        g.last = Some(serving.clone());
        state.set_serving_tls(Some(serving));
        id
    }

    /// A start did not bring it up, for `why`: nothing serves TLS.
    pub fn failed(&self, state: &AppState, why: String) {
        let mut g = self.lock();
        g.running = None;
        g.error = Some(why);
        g.last = None;
        state.set_serving_tls(None);
    }

    /// Start `id` stopped; nothing when a newer start serves already.
    pub fn stopped(&self, state: &AppState, id: u64) {
        let mut g = self.lock();
        if g.running.as_ref().is_some_and(|(r, ..)| *r == id) {
            g.running = None;
            state.set_serving_tls(None);
        }
    }

    /// A start with no TLS listener configured: nothing serves, nothing
    /// failed.
    pub fn none(&self, state: &AppState) {
        let mut g = self.lock();
        g.running = None;
        g.error = None;
        g.last = None;
        state.set_serving_tls(None);
    }

    /// The TLS listener the last start served, whether or not it still
    /// does: what the agents' origins were last built on.
    pub fn last_served(&self) -> Option<ServingTls> {
        self.lock().last.clone()
    }

    /// The pair it serves now, while it serves.
    pub fn current(&self) -> Option<Arc<MainTls>> {
        self.lock().running.as_ref().map(|(_, t, _, _)| t.clone())
    }

    /// Settings' view under `s`, the settings stored now.
    pub fn status(&self, s: &Settings) -> wire::TlsStatus {
        let g = self.lock();
        let configured = s.tls.listener();
        let fmt = |t: DateTime<Utc>| t.format("%Y-%m-%d %H:%M:%S").to_string();
        let mut warnings = scheme_warnings(s, g.running.is_some());
        match (&g.running, configured) {
            (Some((_, tls, addr, started)), now) => {
                let loaded = tls.loaded();
                let info = loaded.info.clone();
                warnings.extend(name_warnings(s, &info.sans));
                warnings.extend(pair_warnings(&loaded, tls.key_path(), Utc::now()));
                if let Some((why, since)) = tls.accept_error() {
                    warnings.push(accept_warning(&why, since));
                }
                wire::TlsStatus {
                    state: wire::TlsListenerState::Serving,
                    listening: Some(addr.to_string()),
                    warnings,
                    certificate: Some(wire::TlsCertificate {
                        names: info.names,
                        subject: info.subject,
                        issuer: info.issuer,
                        serial: info.serial,
                        not_after: fmt(info.not_after),
                        loaded_at: fmt(info.loaded_at),
                    }),
                    error: tls.reload_error(),
                    restart_pending: now.as_ref() != Some(started),
                }
            }
            (None, None) => wire::TlsStatus {
                warnings,
                ..Default::default()
            },
            (None, Some(_)) => wire::TlsStatus {
                state: wire::TlsListenerState::NotServing,
                error: g.error.clone(),
                restart_pending: g.error.is_none(),
                warnings,
                ..Default::default()
            },
        }
    }
}

/// A public URL whose scheme is not the one this gateway serves it with:
/// `https` while no TLS listener serves and proxy mode is off (with a
/// trusted proxy, the proxy terminates TLS for it, §4.5), `http` while one
/// does.
fn scheme_warnings(s: &Settings, tls_serving: bool) -> Vec<String> {
    let Some(u) = crate::net::public_url(s) else {
        return Vec::new();
    };
    let proxied = !crate::config::trusted_nets(&s.trusted_proxies).is_empty();
    match (u.scheme, tls_serving) {
        (Scheme::Https, false) if !proxied => vec![format!(
            "the public URL {} is https, and no TLS listener serves here: unless a proxy in \
             front of lmgw serves TLS for it, every link built from it fails to open. With a \
             proxy, list its address in the trusted proxies so lmgw reads the scheme it \
             forwards",
            u.base()
        )],
        (Scheme::Http, true) => vec![format!(
            "the public URL {} is plain http while the TLS listener serves: links built from it \
             send people to the unencrypted address. Set it to the https address",
            u.base()
        )],
        _ => Vec::new(),
    }
}

/// What a loaded pair itself warns of, at `now`: a leaf that ended or ends
/// within [`EXPIRY_NOTICE_DAYS`], and a key file at `key` its group or
/// everyone can read.
pub(super) fn pair_warnings(loaded: &Loaded, key: &Path, now: DateTime<Utc>) -> Vec<String> {
    let mut out = Vec::new();
    let end = loaded.info.not_after;
    let date = end.format("%Y-%m-%d %H:%M UTC");
    if end <= now {
        out.push(format!(
            "this certificate expired on {date}: browsers refuse it. Put a renewed pair at its \
             paths; the next connection is served it"
        ));
    } else if end - now <= chrono::Duration::days(EXPIRY_NOTICE_DAYS) {
        let days = (end - now).num_days();
        out.push(format!(
            "this certificate expires on {date}, {}: put a renewed pair at its paths before \
             then; the next connection is served it",
            match days {
                0 => "within a day".to_string(),
                1 => "in 1 day".to_string(),
                n => format!("in {n} days"),
            }
        ));
    }
    // Windows: who may read the key is its owner and DACL (`lmgw_private`).
    if let Some(why) = &loaded.key_not_private {
        out.push(format!(
            "the private key {} is not private: {why}. Whoever reads it can pose as this \
             server. Only the account lmgw runs as should read it (with SYSTEM and the \
             Administrators): icacls \"{}\" /inheritance:r /grant:r \"%USERNAME%:F\" \
             \"SYSTEM:F\" \"*S-1-5-32-544:F\"",
            key.display(),
            key.display()
        ));
    }
    if let Some(mode) = loaded.key_mode {
        let who = if mode & 0o004 != 0 {
            Some("every account on this machine")
        } else if mode & 0o070 != 0 {
            Some("the file's group")
        } else {
            None
        };
        if let Some(who) = who {
            out.push(format!(
                "the private key {} can be read by {who} (mode {mode:04o}): whoever reads it can \
                 pose as this server. Only the account lmgw runs as should read it: mode 600, \
                 owned by that account (in the server image the host's service account, not \
                 root). If the file belongs to another account and only group read is set, that \
                 group read is what lets lmgw read it; then the group must contain nothing else",
                key.display()
            ));
        }
    }
    out
}

/// The accept loop's own failure (descriptors out, D36) as Settings and the
/// log say it.
fn accept_warning(why: &str, since: DateTime<Utc>) -> String {
    format!(
        "the TLS listener cannot accept connections since {} ({why}): new connections wait until \
         it can. When it says too many open files, the process is out of file descriptors; \
         raise its limit (LimitNOFILE= in the unit) or find what holds them",
        since.format("%Y-%m-%d %H:%M:%S UTC")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(not_after: DateTime<Utc>, key_mode: Option<u32>) -> Loaded {
        let provider = crate::devices::tls::provider();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["lmgw.example".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap();
        Loaded {
            config: Arc::new(config),
            info: super::super::CertInfo {
                names: vec!["lmgw.example".into()],
                sans: vec![super::super::San::Dns("lmgw.example".into())],
                subject: String::new(),
                issuer: String::new(),
                serial: "01".into(),
                not_after,
                loaded_at: Utc::now(),
            },
            key_mode,
            key_not_private: None,
        }
    }

    #[test]
    fn an_ending_leaf_and_a_readable_key_are_warned_of() {
        let now = Utc::now();
        let key = Path::new("/etc/lmgw/tls/tls.key");
        let far = loaded(now + chrono::Duration::days(60), Some(0o600));
        assert!(pair_warnings(&far, key, now).is_empty());
        let soon = loaded(
            now + chrono::Duration::days(5) + chrono::Duration::hours(1),
            None,
        );
        let w = pair_warnings(&soon, key, now);
        assert!(
            w.len() == 1 && w[0].contains("expires on") && w[0].contains("in 5 days"),
            "{w:?}"
        );
        let gone = loaded(now - chrono::Duration::days(1), None);
        assert!(pair_warnings(&gone, key, now)[0].contains("expired on"));
        let group = loaded(now + chrono::Duration::days(60), Some(0o640));
        let w = pair_warnings(&group, key, now);
        assert!(
            w[0].contains("the file's group")
                && w[0].contains("0640")
                && w[0].contains("mode 600")
                && w[0].contains("group read is what lets lmgw read it"),
            "{w:?}"
        );
        let world = loaded(now + chrono::Duration::days(60), Some(0o644));
        assert!(pair_warnings(&world, key, now)[0].contains("every account"));
        // Windows' finding, by owner and DACL.
        let mut acl = loaded(now + chrono::Duration::days(60), None);
        acl.key_not_private = Some("its access list lets BUILTIN\\Users read it".into());
        let w = pair_warnings(&acl, key, now);
        assert!(
            w.len() == 1 && w[0].contains("is not private: its access list lets BUILTIN"),
            "{w:?}"
        );
    }

    #[test]
    fn a_public_url_of_the_other_scheme_is_warned_of() {
        let s = |u: &str| Settings {
            public_url: u.into(),
            ..Default::default()
        };
        assert!(scheme_warnings(&s("https://lmgw.example"), false)[0].contains("no TLS listener"));
        assert!(scheme_warnings(&s("https://lmgw.example"), true).is_empty());
        assert!(scheme_warnings(&s("http://lmgw.example"), true)[0].contains("plain http"));
        assert!(scheme_warnings(&s("http://lmgw.example"), false).is_empty());
        assert!(scheme_warnings(&s(""), true).is_empty());
    }
}
