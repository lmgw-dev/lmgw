//! The main TLS listener (headless server design §4): the
//! administrator's certificate read from two files and reloaded by itself,
//! and the listener that serves the plain listener's router over it.
//!
//! **The pair reloads at an accept** (D8, D34). [`MainTls`] keeps the loaded
//! pair with the two files' stamps (modification and change time, size,
//! inode and device, read through symlinks); an accepted connection asks
//! [`MainTls::config`], which stats both files and, when either changed,
//! loads both and checks that the key matches the certificate (rustls' own
//! check in `with_single_cert`). The change time is in the stamp because a
//! fix that only changes the files' mode, owner or label (`chmod`, `chown`,
//! `restorecon`) changes nothing else. A load that fails keeps the old pair,
//! logs, and shows on Settings ([`TlsListenerSlot::status`]); it is tried
//! again once either file changes again, so a deploy hook that writes the
//! certificate first and the key a moment later is picked up at the key's
//! write, and a pair that stays broken is not re-read and re-logged at every
//! connection.
//!
//! **No file is read on a runtime worker.** The stat and the load run on the
//! blocking pool, one check at a time: a connection accepted while one runs
//! is served the pair loaded now without waiting for it, so a stalled mount
//! holds one blocking thread and the connection that started the check, and
//! every other handshake goes on with the pair that serves.
//!
//! **What it speaks** (D7): TLS 1.2 and 1.3 with the aws-lc-rs provider
//! the device listener uses, ALPN `http/1.1`, no client certificate asked.
//! The device listener stays TLS 1.3 only with client certificates.
//!
//! **The names** (§4.3): at each load the leaf's subject alternative names
//! are read; a public URL whose host they do not cover, and an agent origin
//! suffix whose `*.<suffix>` they do not cover, are each a warning on
//! Settings and in the log, never silent ([`TlsListenerSlot::status`] has
//! the others: the leaf's end of validity, the key file's mode, a public URL
//! whose scheme is not the one served, a failed accept).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

/// The listener: its accept loop, its handshakes, its connect info.
mod listener;
/// Which names a certificate covers, and what Settings warns about.
mod names;
/// What the gateway knows of its TLS listener: the slot and its status.
mod slot;

pub use listener::MainTlsListener;
pub use names::{covers, name_warnings, San};
pub use slot::{TlsListenerSlot, EXPIRY_NOTICE_DAYS};

/// The ALPN protocol the main TLS listener speaks.
pub const ALPN_HTTP1: &[u8] = b"http/1.1";

/// One file's stamp: what changes when it is rewritten, replaced, or has its
/// mode, owner or label changed (the change time). On Windows the
/// modification and creation times and the length: a rewrite or a
/// replacement shows, a change of the file's access rights does not yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(windows)]
    created: Option<std::time::SystemTime>,
}

/// Both files' stamps; `None` for a file that cannot be stat'ed now.
type Stamps = (Option<Stamp>, Option<Stamp>);

#[cfg(unix)]
fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: m.modified().ok(),
        len: m.len(),
        ctime: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
        ino: m.ino(),
        dev: m.dev(),
    })
}

#[cfg(windows)]
fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: m.modified().ok(),
        len: m.len(),
        created: m.created().ok(),
    })
}

/// A pair as loaded: the server configuration, what Settings shows of its
/// leaf, and the key file's permission bits.
#[derive(Clone)]
pub struct Loaded {
    pub config: Arc<ServerConfig>,
    pub info: CertInfo,
    /// The key file's mode (`st_mode & 0o7777`) at the load; `None` when it
    /// could not be read, and on Windows, which has no mode bits.
    pub key_mode: Option<u32>,
    /// Windows: why the key file is not private by `lmgw_private`'s
    /// meaning (another account may read it, or it is another account's, or
    /// that could not be checked), at the load; `None` when it is private,
    /// and on Unix, where `key_mode` says it.
    pub key_not_private: Option<String>,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("info", &self.info)
            .field("key_mode", &self.key_mode)
            .field("key_not_private", &self.key_not_private)
            .finish()
    }
}

/// What a loaded leaf says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// DNS and IP subject alternative names, lower-cased, as written: what
    /// Settings lists.
    pub names: Vec<String>,
    /// The same names by kind: what [`covers`] matches against.
    pub sans: Vec<San>,
    pub subject: String,
    pub issuer: String,
    /// Lowercase hex of the serial's bytes.
    pub serial: String,
    pub not_after: DateTime<Utc>,
    pub loaded_at: DateTime<Utc>,
}

/// Load the pair at `cert` and `key`: the chain (leaf first) and the key,
/// PEM, into a TLS 1.2 + 1.3 server configuration; an error that names the
/// file and what is wrong with it.
pub fn load(cert: &Path, key: &Path) -> Result<Loaded, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("reading the certificate {}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("reading the certificate {}: {e}", cert.display()))?;
    let Some(leaf) = chain.first() else {
        return Err(format!(
            "the certificate file {} holds no PEM certificate",
            cert.display()
        ));
    };
    let info = cert_info(leaf).map_err(|e| format!("{}: {e}", cert.display()))?;
    let key_der = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("reading the private key {}: {e}", key.display()))?;
    #[cfg(unix)]
    let key_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(key)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let key_mode = None;
    #[cfg(windows)]
    let key_not_private = lmgw_private::check_path(key, lmgw_private::Others::Nothing)
        .err()
        .map(|e| e.to_string());
    #[cfg(not(windows))]
    let key_not_private = None;
    let mut config = ServerConfig::builder_with_provider(crate::devices::tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("TLS 1.2 and 1.3 with aws-lc-rs: {e}"))?
        .with_no_client_auth()
        .with_single_cert(chain, key_der)
        .map_err(|e| match e {
            rustls::Error::InconsistentKeys(_) => format!(
                "the private key {} does not belong to the certificate {}",
                key.display(),
                cert.display()
            ),
            other => format!(
                "the pair {} and {} does not load: {other}",
                cert.display(),
                key.display()
            ),
        })?;
    config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
    Ok(Loaded {
        config: Arc::new(config),
        info,
        key_mode,
        key_not_private,
    })
}

/// The leaf's names, subject, issuer, serial and end of validity.
fn cert_info(der: &CertificateDer<'_>) -> Result<CertInfo, String> {
    use x509_parser::extensions::GeneralName;
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| format!("the leaf certificate does not parse: {e}"))?;
    let tbs = &cert.tbs_certificate;
    let mut sans = Vec::new();
    if let Ok(Some(san)) = tbs.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                GeneralName::DNSName(d) => sans.push(San::Dns(d.to_ascii_lowercase())),
                GeneralName::IPAddress(raw) => {
                    let ip = match raw.len() {
                        4 => <[u8; 4]>::try_from(*raw).ok().map(std::net::IpAddr::from),
                        16 => <[u8; 16]>::try_from(*raw).ok().map(std::net::IpAddr::from),
                        _ => None,
                    };
                    if let Some(ip) = ip {
                        sans.push(San::Ip(ip));
                    }
                }
                _ => {}
            }
        }
    }
    let raw = tbs.raw_serial();
    let start = usize::from(raw.len() > 1 && raw[0] == 0 && raw[1] >= 0x80);
    let not_after = DateTime::from_timestamp(tbs.validity.not_after.timestamp(), 0)
        .ok_or_else(|| "the leaf certificate's notAfter is out of range".to_string())?;
    Ok(CertInfo {
        names: sans.iter().map(San::to_string).collect(),
        sans,
        subject: tbs.subject.to_string(),
        issuer: tbs.issuer.to_string(),
        serial: hex::encode(&raw[start..]),
        not_after,
        loaded_at: Utc::now(),
    })
}

/// The pair one start of the TLS listener serves, reloaded at an accept
/// (module doc), and what its accept loop last failed on.
pub struct MainTls {
    cert: PathBuf,
    key: PathBuf,
    /// The pair that serves and the stamps it was loaded at: swapped by the
    /// one check that runs at a time, read by every handshake without a lock.
    current: ArcSwap<Current>,
    /// The last failed reload: the stamps it saw, why, and when.
    failed: Mutex<Option<Failed>>,
    /// A check (stat, maybe a load) runs on the blocking pool now.
    checking: AtomicBool,
    /// The accept loop's last failure of its own (descriptors out), while it
    /// lasts: why and since when.
    accept_error: Mutex<Option<(String, DateTime<Utc>)>>,
    accept_failing: AtomicBool,
}

impl std::fmt::Debug for MainTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainTls")
            .field("cert", &self.cert)
            .field("key", &self.key)
            .finish()
    }
}

struct Current {
    loaded: Loaded,
    stamps: Stamps,
}

struct Failed {
    stamps: Stamps,
    why: String,
    at: DateTime<Utc>,
}

/// Clears [`MainTls::checking`] when the check it guards ends, however it
/// ends.
struct Checking(Arc<MainTls>);

impl Drop for Checking {
    fn drop(&mut self) {
        self.0.checking.store(false, Ordering::Release);
    }
}

impl MainTls {
    /// The pair at `cert` and `key`, loaded now; the load's error when it
    /// does not load (the start says it, D8: nothing to fall back on yet).
    /// Reads both files: call it off the runtime's workers.
    pub fn open(cert: &Path, key: &Path) -> Result<Self, String> {
        let stamps = (stamp(cert), stamp(key));
        let loaded = load(cert, key)?;
        Ok(Self {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
            current: ArcSwap::from_pointee(Current { loaded, stamps }),
            failed: Mutex::new(None),
            checking: AtomicBool::new(false),
            accept_error: Mutex::new(None),
            accept_failing: AtomicBool::new(false),
        })
    }

    /// The private key's path.
    pub fn key_path(&self) -> &Path {
        &self.key
    }

    fn failed(&self) -> std::sync::MutexGuard<'_, Option<Failed>> {
        self.failed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The configuration for a connection accepted now: the loaded pair,
    /// reloaded first when either file changed since it (or since the last
    /// failed reload). The check runs on the blocking pool; while another
    /// connection's check runs, the pair loaded now, at once.
    pub async fn config(self: &Arc<Self>) -> Arc<ServerConfig> {
        if self.checking.swap(true, Ordering::AcqRel) {
            return self.current.load().loaded.config.clone();
        }
        let guard = Checking(self.clone());
        let this = self.clone();
        match tokio::task::spawn_blocking(move || {
            let _checking = guard;
            this.check()
        })
        .await
        {
            Ok(config) => config,
            Err(_) => self.current.load().loaded.config.clone(),
        }
    }

    /// [`Self::config`]'s check itself: stats both files and reloads when
    /// they changed. Blocking; only one runs at a time.
    fn check(&self) -> Arc<ServerConfig> {
        let now = (stamp(&self.cert), stamp(&self.key));
        let cur = self.current.load_full();
        if now == cur.stamps {
            // Back to the pair that serves: a failure seen in between is
            // no longer on disk.
            *self.failed() = None;
            return cur.loaded.config.clone();
        }
        if self.failed().as_ref().is_some_and(|f| f.stamps == now) {
            return cur.loaded.config.clone();
        }
        match load(&self.cert, &self.key) {
            Ok(loaded) => {
                tracing::info!(
                    "TLS listener: the changed pair is loaded (serial {}, names {}, valid until \
                     {})",
                    loaded.info.serial,
                    loaded.info.names.join(", "),
                    loaded.info.not_after.format("%Y-%m-%d %H:%M:%S UTC")
                );
                for w in slot::pair_warnings(&loaded, &self.key, Utc::now()) {
                    tracing::warn!("TLS listener: {w}");
                }
                let config = loaded.config.clone();
                self.current.store(Arc::new(Current {
                    loaded,
                    stamps: now,
                }));
                *self.failed() = None;
                config
            }
            Err(e) => {
                tracing::warn!(
                    "TLS listener: the changed pair does not load ({e}); the pair loaded \
                     before (serial {}) still serves, and the next change of either file is \
                     tried again",
                    cur.loaded.info.serial
                );
                *self.failed() = Some(Failed {
                    stamps: now,
                    why: e,
                    at: Utc::now(),
                });
                cur.loaded.config.clone()
            }
        }
    }

    /// The pair served now.
    pub fn loaded(&self) -> Loaded {
        self.current.load().loaded.clone()
    }

    /// The leaf served now.
    pub fn info(&self) -> CertInfo {
        self.current.load().loaded.info.clone()
    }

    /// The last failed reload's sentence, with when, while the pair it
    /// failed on is still on disk.
    pub fn reload_error(&self) -> Option<String> {
        self.failed().as_ref().map(|f| {
            format!(
                "{} (at {}; the pair loaded before still serves)",
                f.why,
                f.at.format("%Y-%m-%d %H:%M:%S UTC")
            )
        })
    }

    fn accept_error_slot(&self) -> std::sync::MutexGuard<'_, Option<(String, DateTime<Utc>)>> {
        self.accept_error.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The accept loop failed for a reason of its own (`e`, the process out
    /// of descriptors): kept for Settings until an accept works again, and
    /// logged once per episode. Answers whether this one began it.
    pub(crate) fn accept_failed(&self, e: &std::io::Error) -> bool {
        let began = !self.accept_failing.swap(true, Ordering::AcqRel);
        let mut g = self.accept_error_slot();
        match g.as_mut() {
            Some((why, _)) => *why = e.to_string(),
            None => *g = Some((e.to_string(), Utc::now())),
        }
        began
    }

    /// An accept worked: a failure kept before is over. Answers whether
    /// there was one.
    pub(crate) fn accept_worked(&self) -> bool {
        if !self.accept_failing.load(Ordering::Acquire) {
            return false;
        }
        self.accept_failing.store(false, Ordering::Release);
        self.accept_error_slot().take().is_some()
    }

    /// The accept loop's failure while it lasts: why, and since when.
    pub fn accept_error(&self) -> Option<(String, DateTime<Utc>)> {
        self.accept_error_slot().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let e =
            MainTls::open(&dir.path().join("tls.crt"), &dir.path().join("tls.key")).unwrap_err();
        assert!(
            e.contains("reading the certificate") && e.contains("tls.crt"),
            "{e}"
        );
    }
}
