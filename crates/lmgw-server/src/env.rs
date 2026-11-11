//! The start's environment (headless server design §2.1), read once and
//! parsed: a variable that does not parse ends the start, naming the
//! variable and the value — a server that comes up on a guessed address is
//! worse than one that does not come up.
//!
//! Read through a lookup rather than `std::env` directly, so the rules are
//! tested on a stated environment. A value that is not UTF-8 is refused for
//! every `LMGW_` variable, naming it, rather than read as unset.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use lmgw_core::config::deployment::{
    parse_deployment, ContainerEnv, Deployment, Pinned, Seeded, STOP_TIMEOUT_VAR,
};
use lmgw_core::config::DevicesListen;

/// What the environment says about this start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerEnv {
    /// `LMGW_DEPLOYMENT` (D2).
    pub deployment: Deployment,
    /// `LMGW_DEV`: a dev instance (D1).
    pub dev: bool,
    /// `LMGW_DATA_DIR`, made absolute; `None` when unset or empty.
    pub data_dir: Option<PathBuf>,
    /// `LMGW_CONTAINER_PREFIX`: a dev instance's prefix for a fresh data
    /// dir (the headless runner's rule).
    pub dev_container_prefix: Option<String>,
    /// The pinned and seeded settings' variables and the stop budget.
    pub container: ContainerEnv,
}

impl ServerEnv {
    /// The process's own environment.
    pub fn from_process() -> Result<Self, String> {
        read_vars(std::env::vars_os())
    }
}

/// The prefix of the variables lmgw reads: one of them whose value is not
/// UTF-8 ends the start.
const PREFIX: &str = "LMGW_";

/// [`read`] on an environment's pairs: an `LMGW_` variable whose value is
/// not UTF-8 ends the start naming it (read as unset, it would start a
/// server on what the variable was meant to change); the rest of the
/// environment is not lmgw's.
pub fn read_vars(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<ServerEnv, String> {
    let mut map = HashMap::new();
    for (k, v) in vars {
        let Some(k) = k.to_str().filter(|k| k.starts_with(PREFIX)) else {
            continue;
        };
        let v = v.into_string().map_err(|v| {
            format!(
                "{k} is not valid UTF-8 ({}): lmgw reads its variables as text",
                v.to_string_lossy()
            )
        })?;
        map.insert(k.to_string(), v);
    }
    read(|k| map.get(k).cloned())
}

/// The environment `lookup` answers, read. An empty value is read as unset,
/// as `LMGW_DATA_DIR` always has been.
pub fn read(lookup: impl Fn(&str) -> Option<String>) -> Result<ServerEnv, String> {
    let get = |k: &str| lookup(k).filter(|v| !v.trim().is_empty());
    let deployment = parse_deployment(get("LMGW_DEPLOYMENT").as_deref())?;
    let container = ContainerEnv {
        bind_addr: parsed(
            &get,
            Pinned::BindAddr.variable(),
            "an address and port (0.0.0.0:8787)",
            |v| v.parse().ok(),
        )?,
        webrtc_port: parsed(
            &get,
            Pinned::WebrtcPort.variable(),
            "a UDP port from 0 to 65535",
            |v| v.parse().ok(),
        )?,
        tls_cert: parsed(
            &get,
            Pinned::TlsCert.variable(),
            "an absolute path to a PEM certificate chain (/etc/lmgw/tls/tls.crt)",
            absolute,
        )?,
        tls_key: parsed(
            &get,
            Pinned::TlsKey.variable(),
            "an absolute path to a PEM private key (/etc/lmgw/tls/tls.key)",
            absolute,
        )?,
        tls_listen: parsed(
            &get,
            Pinned::TlsListen.variable(),
            "an address and port (0.0.0.0:8443)",
            |v| v.parse().ok(),
        )?,
        public_url: parsed(
            &get,
            Pinned::PublicUrl.variable(),
            "an origin such as https://lmgw.example:8443 (a scheme, a host and an optional \
             port; no path)",
            |v| {
                lmgw_core::config::parse_public_url(v)
                    .ok()
                    .map(|u| u.base())
            },
        )?,
        webrtc_tls_port: parsed(
            &get,
            Pinned::WebrtcTlsPort.variable(),
            "a UDP port from 0 to 65535",
            |v| v.parse().ok(),
        )?,
        trusted_proxies: trusted_proxies(&get)?,
        require_api_key: parsed(
            &get,
            Seeded::RequireApiKey.variable(),
            "on (1, true, yes, on) or off (0, false, no, off)",
            switch,
        )?,
        devices_listen: parsed(
            &get,
            Seeded::DevicesListen.variable(),
            "loopback, all or an IP address",
            DevicesListen::parse,
        )?,
        stop_timeout: parsed(&get, STOP_TIMEOUT_VAR, "a whole number of seconds", |v| {
            v.parse().ok()
        })?,
    };
    // Half a pair serves nothing: said now, not as a listener that never
    // comes up.
    let (cert, key) = (Pinned::TlsCert.variable(), Pinned::TlsKey.variable());
    match (&container.tls_cert, &container.tls_key) {
        (Some(_), None) => {
            return Err(format!("{cert} is set without {key}: set both, or neither"))
        }
        (None, Some(_)) => {
            return Err(format!("{key} is set without {cert}: set both, or neither"))
        }
        _ => {}
    }
    // The two listeners the unit names on one TCP port: the second could
    // never bind. Said naming both variables (the stored settings' own
    // collisions are the start's check, `apply_at_start`).
    if let (Some(plain), Some(tls)) = (container.bind_addr, container.tls_listen) {
        if lmgw_core::config::tcp_addrs_collide(plain, tls) {
            return Err(format!(
                "{}={plain} and {}={tls} are one TCP port on overlapping addresses: the TLS \
                 listener could not bind it beside the plain one. Give them two ports (the image's \
                 are 8787 and 8443)",
                Pinned::BindAddr.variable(),
                Pinned::TlsListen.variable()
            ));
        }
    }
    Ok(ServerEnv {
        deployment,
        dev: lmgw_core::state::dev_flag(lookup("LMGW_DEV").as_deref()),
        data_dir: get("LMGW_DATA_DIR").map(|v| {
            let dir = PathBuf::from(v);
            std::path::absolute(&dir).unwrap_or(dir)
        }),
        dev_container_prefix: get("LMGW_CONTAINER_PREFIX"),
        container,
    })
}

/// `var`'s value read by `parse`; `None` when unset, refused naming the
/// variable, the value and what it takes when it does not parse.
fn parsed<T>(
    get: &impl Fn(&str) -> Option<String>,
    var: &str,
    takes: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, String> {
    match get(var) {
        None => Ok(None),
        Some(v) => parse(v.trim())
            .map(Some)
            .ok_or_else(|| format!("{var}={v:?} does not parse: it takes {takes}")),
    }
}

/// `LMGW_TRUSTED_PROXIES` read (§4.5): addresses and CIDR networks
/// separated by commas or spaces, written as the networks they are; the
/// first entry that does not parse is refused with why.
fn trusted_proxies(get: &impl Fn(&str) -> Option<String>) -> Result<Option<Vec<String>>, String> {
    let var = Pinned::TrustedProxies.variable();
    let Some(v) = get(var) else {
        return Ok(None);
    };
    lmgw_core::config::normalize_trusted_proxies(&lmgw_core::config::split_trusted_proxies(&v))
        .map(Some)
        .map_err(|e| {
            format!(
                "{var}={v:?} does not parse: it takes addresses or CIDR networks separated by \
                 commas or spaces (10.89.0.0/24, 127.0.0.1); {e}"
            )
        })
}

/// An absolute path.
fn absolute(v: &str) -> Option<PathBuf> {
    v.starts_with('/').then(|| PathBuf::from(v))
}

/// An on/off variable.
fn switch(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> Result<ServerEnv, String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        read(|k| map.get(k).cloned())
    }

    #[test]
    fn an_empty_environment_is_a_desktop_start_with_nothing_set() {
        let e = env(&[]).unwrap();
        assert_eq!(e.deployment, Deployment::Desktop);
        assert!(!e.dev);
        assert_eq!(e.data_dir, None);
        assert_eq!(e.container, ContainerEnv::default());
    }

    #[test]
    fn the_image_s_variables_are_read() {
        let e = env(&[
            ("LMGW_DEPLOYMENT", "container"),
            ("LMGW_DATA_DIR", "/data"),
            ("LMGW_BIND_ADDR", "0.0.0.0:8787"),
            ("LMGW_WEBRTC_PORT", "3478"),
            ("LMGW_REQUIRE_API_KEY", "0"),
            ("LMGW_DEVICES_LISTEN", "all"),
            ("LMGW_STOP_TIMEOUT", "95"),
        ])
        .unwrap();
        assert_eq!(e.deployment, Deployment::Container);
        assert_eq!(e.data_dir, Some(PathBuf::from("/data")));
        assert_eq!(e.container.bind_addr, Some("0.0.0.0:8787".parse().unwrap()));
        assert_eq!(e.container.webrtc_port, Some(3478));
        assert_eq!(e.container.require_api_key, Some(false));
        assert_eq!(e.container.devices_listen, Some(DevicesListen::All));
        assert_eq!(e.container.stop_timeout, Some(95));
    }

    #[test]
    fn a_variable_that_does_not_parse_is_named_with_its_value() {
        for (var, bad) in [
            ("LMGW_BIND_ADDR", "8787"),
            ("LMGW_WEBRTC_PORT", "70000"),
            ("LMGW_REQUIRE_API_KEY", "maybe"),
            ("LMGW_DEVICES_LISTEN", "lan"),
            ("LMGW_STOP_TIMEOUT", "95s"),
            ("LMGW_DEPLOYMENT", "server"),
        ] {
            let e = env(&[(var, bad)]).unwrap_err();
            assert!(
                e.contains(var) && e.contains(&format!("{bad:?}")),
                "{var}: {e}"
            );
        }
    }

    #[test]
    fn a_value_that_is_not_utf8_is_refused_naming_its_variable() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(b"0.0.0.0:\xff".to_vec());
        let e = read_vars([
            (OsString::from("LMGW_BIND_ADDR"), bad.clone()),
            (
                OsString::from("LMGW_DEPLOYMENT"),
                OsString::from("container"),
            ),
        ])
        .unwrap_err();
        assert!(e.contains("LMGW_BIND_ADDR is not valid UTF-8"), "{e}");
        // Another program's variable is not lmgw's to judge.
        let e = read_vars([(OsString::from("LANG"), bad)]).unwrap();
        assert_eq!(e, ServerEnv::default());
    }

    #[test]
    fn the_trusted_proxies_are_read_as_networks() {
        let e = env(&[("LMGW_TRUSTED_PROXIES", "10.89.0.0/24, 127.0.0.1 fd00::/8")]).unwrap();
        assert_eq!(
            e.container.trusted_proxies,
            Some(vec![
                "10.89.0.0/24".to_string(),
                "127.0.0.1/32".to_string(),
                "fd00::/8".to_string()
            ])
        );
        for bad in ["192.0.2.5/24", "lmgw.example", "192.0.2.0/33"] {
            let e = env(&[("LMGW_TRUSTED_PROXIES", bad)]).unwrap_err();
            assert!(
                e.contains("LMGW_TRUSTED_PROXIES") && e.contains(&format!("{bad:?}")),
                "{bad}: {e}"
            );
        }
        // Empty is unset: proxy mode off.
        assert_eq!(
            env(&[("LMGW_TRUSTED_PROXIES", " ")])
                .unwrap()
                .container
                .trusted_proxies,
            None
        );
    }

    #[test]
    fn the_tls_variables_are_read() {
        let e = env(&[
            ("LMGW_TLS_CERT", "/etc/lmgw/tls/tls.crt"),
            ("LMGW_TLS_KEY", "/etc/lmgw/tls/tls.key"),
            ("LMGW_TLS_LISTEN", "0.0.0.0:8443"),
            ("LMGW_PUBLIC_URL", "https://LMGW.example:8443/"),
            ("LMGW_WEBRTC_TLS_PORT", "8443"),
        ])
        .unwrap();
        let c = &e.container;
        assert_eq!(c.tls_cert, Some(PathBuf::from("/etc/lmgw/tls/tls.crt")));
        assert_eq!(c.tls_key, Some(PathBuf::from("/etc/lmgw/tls/tls.key")));
        assert_eq!(c.tls_listen, Some("0.0.0.0:8443".parse().unwrap()));
        assert_eq!(c.public_url.as_deref(), Some("https://lmgw.example:8443"));
        assert_eq!(c.webrtc_tls_port, Some(8443));
        for (var, bad) in [
            ("LMGW_TLS_LISTEN", "8443"),
            ("LMGW_PUBLIC_URL", "lmgw.example"),
            ("LMGW_PUBLIC_URL", "https://lmgw.example/ui"),
            ("LMGW_WEBRTC_TLS_PORT", "x"),
        ] {
            let e = env(&[(var, bad)]).unwrap_err();
            assert!(
                e.contains(var) && e.contains(&format!("{bad:?}")),
                "{var}: {e}"
            );
        }
        let e = env(&[("LMGW_TLS_CERT", "tls.crt"), ("LMGW_TLS_KEY", "/k")]).unwrap_err();
        assert!(e.contains("an absolute path"), "{e}");
        let e = env(&[
            ("LMGW_BIND_ADDR", "0.0.0.0:8443"),
            ("LMGW_TLS_LISTEN", "0.0.0.0:8443"),
        ])
        .unwrap_err();
        assert!(
            e.contains(
                "LMGW_BIND_ADDR=0.0.0.0:8443 and LMGW_TLS_LISTEN=0.0.0.0:8443 are one TCP port"
            ),
            "{e}"
        );
        env(&[
            ("LMGW_BIND_ADDR", "127.0.0.1:8787"),
            ("LMGW_TLS_LISTEN", "0.0.0.0:8443"),
        ])
        .unwrap();
        let e = env(&[("LMGW_TLS_CERT", "/c")]).unwrap_err();
        assert!(
            e.contains("LMGW_TLS_CERT is set without LMGW_TLS_KEY"),
            "{e}"
        );
    }
}
