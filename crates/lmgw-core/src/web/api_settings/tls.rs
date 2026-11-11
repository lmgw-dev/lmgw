//! The dashboard's save of the main listener's TLS and the public origin
//! (Settings → Network & access → TLS, headless server design §4.7): the
//! `tls` section, `public_url` and proxy mode's `trusted_proxies` (§4.5),
//! checked as a whole after the patch, with
//! the notes the save answers with. Like `bind_addr`, these are taken by
//! the dashboard's full save only; no self-admin tool changes them.

use serde::Deserialize;

use crate::config::{normalize_trusted_proxies, validate_public_url, validate_tls, Settings};

/// The `tls` section's patch: each field absent leaves it.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TlsSettingsPatch {
    /// The PEM certificate chain, the leaf first: an absolute path; empty =
    /// no TLS listener.
    cert_path: Option<String>,
    /// The PEM private key: an absolute path; empty = no TLS listener.
    key_path: Option<String>,
    /// Where the TLS listener binds, `host:port`; empty = no TLS listener.
    listen: Option<String>,
    /// The seconds a client gets for its TLS handshake; at least 1.
    handshake_timeout_s: Option<u32>,
}

/// What a TLS save changed, for the notes and what follows the write.
#[derive(Debug, Default)]
pub(super) struct TlsSaved {
    pub notes: Vec<String>,
    /// `public_url` moved: agent origins are renamed with it, at once.
    pub origin_moved: bool,
}

/// Apply `tls` and `public_url` to `s` and check the result: a refusal names
/// the setting. The notes say when each change applies, and that the pair
/// at the new paths does not load when it does not (a warning: the paths
/// may be filled before the files are).
pub(super) fn apply(
    s: &mut Settings,
    tls: Option<TlsSettingsPatch>,
    public_url: Option<String>,
    trusted_proxies: Option<Vec<String>>,
    container: bool,
) -> Result<TlsSaved, String> {
    let before = s.clone();
    if let Some(p) = tls {
        if let Some(v) = p.cert_path {
            s.tls.cert_path = v.trim().to_string();
        }
        if let Some(v) = p.key_path {
            s.tls.key_path = v.trim().to_string();
        }
        if let Some(v) = p.listen {
            s.tls.listen = v.trim().to_string();
        }
        if let Some(v) = p.handshake_timeout_s {
            s.tls.handshake_timeout_s = v;
        }
    }
    if let Some(v) = public_url {
        validate_public_url(&v)?;
        s.public_url = match crate::config::parse_public_url(&v) {
            // Stored as the origin it reads as, so every builder and every
            // comparison sees one spelling.
            Ok(u) => u.base(),
            Err(_) => String::new(),
        };
    }
    if let Some(v) = trusted_proxies {
        // Stored as the networks they are (`127.0.0.1` as `127.0.0.1/32`),
        // so the list reads one way everywhere.
        s.trusted_proxies = normalize_trusted_proxies(&v)?;
    }
    validate_tls(&s.tls)?;
    let mut out = TlsSaved::default();
    let restart = if container {
        format!("{} on the host", lmgw_api_types::RESTART_COMMAND)
    } else {
        "Restart gateway in the tray menu, or a restart of lmgw".to_string()
    };
    if s.tls != before.tls {
        out.notes.push(format!(
            "the TLS listener's settings apply at the next start of the server ({restart}); a \
             renewed pair at the same paths is picked up without one"
        ));
        if let Some(l) = s.tls.listener() {
            if let Err(e) = crate::server::main_tls::load(&l.cert, &l.key) {
                out.notes.push(format!(
                    "the pair at these paths does not load now ({e}): the TLS listener will \
                     not start until it does"
                ));
            }
        } else if s.tls.has_pair() || !s.tls.listen.is_empty() {
            out.notes.push(
                "the TLS listener needs the certificate, its key and an address: until all \
                 three are set it does not start"
                    .into(),
            );
        }
    }
    if s.public_url != before.public_url {
        out.origin_moved = true;
        out.notes.push(if s.public_url.is_empty() {
            "the public URL is cleared: links and snippets are built from the listener's \
             address from now on"
                .into()
        } else {
            format!(
                "links, snippets and agent origins are built from {} from now on",
                s.public_url
            )
        });
    }
    if s.trusted_proxies != before.trusted_proxies {
        out.notes.push(if s.trusted_proxies.is_empty() {
            "proxy mode is off from the next request on: no peer's forwarded headers are read"
                .into()
        } else {
            format!(
                "forwarded headers are read from {} from the next request on",
                s.trusted_proxies.join(", ")
            )
        });
        if !s.trusted_proxies.is_empty() && s.public_url.is_empty() {
            out.notes.push(
                "no public URL is set: links are built from lmgw's own listener; set it to the \
                 address the proxy serves"
                    .into(),
            );
        }
    }
    // The agents' origins follow the TLS listener's scheme and port too, but
    // from the start that serves it on (`server::listeners`), which stops
    // the app containers then: nothing moves at this save.
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_save_takes_the_section_and_spells_the_public_url_once() {
        // An absolute path of this system: Windows' has a drive.
        let abs = |unix: &str| {
            if cfg!(windows) {
                format!("C:{}", unix.replace('/', "\\"))
            } else {
                unix.to_string()
            }
        };
        let mut s = Settings::default();
        let saved = apply(
            &mut s,
            Some(TlsSettingsPatch {
                cert_path: Some(format!(" {} ", abs("/etc/lmgw/tls/tls.crt"))),
                key_path: Some(abs("/etc/lmgw/tls/tls.key")),
                listen: Some("0.0.0.0:8443".into()),
                handshake_timeout_s: None,
            }),
            Some("https://LMGW.example:443/".into()),
            None,
            false,
        )
        .unwrap();
        assert_eq!(s.tls.cert_path, abs("/etc/lmgw/tls/tls.crt"));
        assert_eq!(s.public_url, "https://lmgw.example");
        assert!(saved.origin_moved);
        assert!(
            saved.notes.iter().any(|n| n.contains("Restart gateway")),
            "{saved:?}"
        );
        assert!(
            saved.notes.iter().any(|n| n.contains("does not load now")),
            "the files are not there: {saved:?}"
        );
        let e = apply(&mut s, None, Some("lmgw.example".into()), None, false).unwrap_err();
        assert!(e.starts_with("the public URL"), "{e}");
        let e = apply(
            &mut s,
            Some(TlsSettingsPatch {
                key_path: Some(String::new()),
                ..Default::default()
            }),
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(e.contains("set together"), "{e}");
    }

    #[test]
    fn a_save_writes_the_trusted_proxies_as_networks_and_applies_them_at_once() {
        let mut s = Settings::default();
        let saved = apply(
            &mut s,
            None,
            None,
            Some(vec!["127.0.0.1".into(), "".into(), "fd00::/8".into()]),
            false,
        )
        .unwrap();
        assert_eq!(s.trusted_proxies, vec!["127.0.0.1/32", "fd00::/8"]);
        assert!(
            saved
                .notes
                .iter()
                .any(|n| n.contains("from the next request on")),
            "{saved:?}"
        );
        assert!(
            saved
                .notes
                .iter()
                .any(|n| n.contains("no public URL is set")),
            "{saved:?}"
        );
        let e = apply(&mut s, None, None, Some(vec!["192.0.2.5/24".into()]), false).unwrap_err();
        assert!(e.contains("Trusted proxies"), "{e}");
        assert_eq!(
            s.trusted_proxies,
            vec!["127.0.0.1/32", "fd00::/8"],
            "unchanged"
        );
        let saved = apply(&mut s, None, None, Some(Vec::new()), false).unwrap();
        assert!(s.trusted_proxies.is_empty());
        assert!(
            saved.notes.iter().any(|n| n.contains("proxy mode is off")),
            "{saved:?}"
        );
    }
}
