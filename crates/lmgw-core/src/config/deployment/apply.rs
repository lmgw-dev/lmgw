//! A container start's environment applied to the stored settings
//! (headless server design §2.2): the pinned settings over the stored
//! values, the seeds into a data dir no container start has seeded, and the
//! lines the start logs of both.

use std::net::SocketAddr;

use lmgw_api_types::RESTART_COMMAND;

use super::{ContainerEnv, Pinned, PinnedSource, Seeded, IMAGE_BIND_ADDR};
use crate::config::{DevicesListen, Settings, TLS_LISTEN_PROPOSED};

/// What [`apply`] did to the settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Applied {
    /// The settings differ from what was stored.
    pub changed: bool,
    /// One line per pinned and seeded setting, for the start's log.
    pub lines: Vec<String>,
    /// Where each pinned setting's value came from, in [`Pinned::ALL`]'s
    /// order.
    pub sources: [PinnedSource; Pinned::ALL.len()],
}

/// Apply a container start's environment to the stored settings `s` (§2.2):
/// every pinned variable that is set over the stored value; on a data dir no
/// container start has seeded yet ([`Settings::container_seeded`] unset)
/// every seeded setting from its variable or the image's default, and the
/// marker. A seeded variable on a data dir that is seeded already is said
/// and left: the stored setting rules.
///
/// The variables that are unset:
/// - **the TLS pair, the public URL and the trusted proxies** are cleared,
///   the image's default of none: the unit file is where TLS and proxy
///   mode are switched on and off, and a pair or a trusted peer kept from
///   an earlier start would serve TLS or trust a peer the unit no longer
///   names;
/// - **the TLS address** is kept, or [`TLS_LISTEN_PROPOSED`] when none is
///   stored;
/// - **the bind address** is kept, or the image's [`IMAGE_BIND_ADDR`] on a
///   data dir this start seeds; then, while its address is one of the two
///   the image itself gives it (`0.0.0.0` or `127.0.0.1`), it follows the
///   TLS listener (D6): loopback with TLS on, where agents, the health
///   check and the CLI reach it, every address with TLS off, on the port it
///   has. Any other address stays as stored;
/// - **the WebRTC ports** are kept.
pub fn apply(s: &mut Settings, env: &ContainerEnv) -> Applied {
    let mut out = Applied::default();
    let seeding = !s.container_seeded;
    // The TLS listener first: the plain listener's address follows it.
    let src = clear_or_set(
        &mut s.tls.cert_path,
        env.tls_cert.as_ref().map(|p| p.display().to_string()),
        Pinned::TlsCert,
        &mut out,
    );
    out.sources[Pinned::TlsCert.index()] = src;
    let src = clear_or_set(
        &mut s.tls.key_path,
        env.tls_key.as_ref().map(|p| p.display().to_string()),
        Pinned::TlsKey,
        &mut out,
    );
    out.sources[Pinned::TlsKey.index()] = src;
    let listen_default = s
        .tls
        .listen
        .trim()
        .is_empty()
        .then(|| TLS_LISTEN_PROPOSED.to_string());
    let src = pin(
        &mut s.tls.listen,
        env.tls_listen.map(|a| a.to_string()),
        listen_default,
        Pinned::TlsListen,
        &mut out,
        String::clone,
    );
    out.sources[Pinned::TlsListen.index()] = src;
    let src = clear_or_set(
        &mut s.public_url,
        env.public_url.clone(),
        Pinned::PublicUrl,
        &mut out,
    );
    out.sources[Pinned::PublicUrl.index()] = src;
    // The plain listener.
    let src = pin(
        &mut s.bind_addr,
        env.bind_addr.map(|a| a.to_string()),
        seeding.then(|| IMAGE_BIND_ADDR.to_string()),
        Pinned::BindAddr,
        &mut out,
        String::clone,
    );
    let src = if env.bind_addr.is_none() {
        follow_tls(s, src, &mut out)
    } else {
        src
    };
    out.sources[Pinned::BindAddr.index()] = src;
    // The WebRTC groups' ports.
    let src = pin(
        &mut s.realtime.webrtc.webrtc_port,
        env.webrtc_port.map(Some),
        None,
        Pinned::WebrtcPort,
        &mut out,
        |p| match p {
            Some(p) => p.to_string(),
            None => "unset (the HTTP port's number)".into(),
        },
    );
    out.sources[Pinned::WebrtcPort.index()] = src;
    let src = pin(
        &mut s.realtime.webrtc.webrtc_tls_port,
        env.webrtc_tls_port.map(Some),
        None,
        Pinned::WebrtcTlsPort,
        &mut out,
        |p| match p {
            Some(p) => p.to_string(),
            None => "unset (the TLS port's number)".into(),
        },
    );
    out.sources[Pinned::WebrtcTlsPort.index()] = src;
    // Proxy mode's trusted peers (§4.5).
    let p = Pinned::TrustedProxies;
    let (value, src) = match &env.trusted_proxies {
        Some(v) => (v.clone(), PinnedSource::Variable),
        None => (Vec::new(), PinnedSource::ImageDefault),
    };
    out.changed |= s.trusted_proxies != value;
    s.trusted_proxies = value;
    out.lines.push(match src {
        PinnedSource::Variable => format!(
            "pinned {} = {} ({})",
            p.setting(),
            s.trusted_proxies.join(", "),
            p.variable()
        ),
        _ => format!(
            "pinned {} = none, proxy mode off: the image's default ({} is not set)",
            p.setting(),
            p.variable()
        ),
    });
    out.sources[p.index()] = src;
    // Seeded.
    let auth = Seeded::RequireApiKey;
    let devices = Seeded::DevicesListen;
    if seeding {
        let on = env.require_api_key.unwrap_or(true);
        s.auth_enabled = on;
        out.lines.push(seed_line(
            auth,
            on.to_string(),
            env.require_api_key.is_some(),
        ));
        let listen = env.devices_listen.unwrap_or(DevicesListen::All);
        s.devices.listen = listen;
        out.lines.push(seed_line(
            devices,
            listen.as_text(),
            env.devices_listen.is_some(),
        ));
        // In the same write as the seeds: a start that dies before it
        // leaves the next one to seed.
        s.container_seeded = true;
        out.changed = true;
    } else {
        if env.require_api_key.is_some() {
            out.lines
                .push(unseeded_line(auth, s.auth_enabled.to_string()));
        }
        if env.devices_listen.is_some() {
            out.lines
                .push(unseeded_line(devices, s.devices.listen.as_text()));
        }
    }
    out
}

/// The bind address's step after its pin, its variable unset (D6): while
/// its address is the image's own (`0.0.0.0` or `127.0.0.1`), loopback with
/// the TLS listener on and every address with it off, on the port it has.
fn follow_tls(s: &mut Settings, src: PinnedSource, out: &mut Applied) -> PinnedSource {
    let Ok(addr) = s.bind_addr.trim().parse::<SocketAddr>() else {
        return src;
    };
    let image_own = addr.ip() == std::net::IpAddr::from([0, 0, 0, 0])
        || addr.ip() == std::net::IpAddr::from([127, 0, 0, 1]);
    if !image_own {
        return src;
    }
    let tls = s.tls.on();
    let ip: std::net::IpAddr = if tls { [127, 0, 0, 1] } else { [0, 0, 0, 0] }.into();
    let wanted = SocketAddr::new(ip, addr.port());
    if wanted == addr {
        return src;
    }
    s.bind_addr = wanted.to_string();
    out.changed = true;
    out.lines.push(if tls {
        format!(
            "the plain listener moves to {wanted}: behind the TLS listener it serves the \
             container's own agents, health check and command line only ({} is not set)",
            Pinned::BindAddr.variable()
        )
    } else {
        format!(
            "the plain listener moves to {wanted}: without a TLS listener it is the published \
             one ({} is not set)",
            Pinned::BindAddr.variable()
        )
    });
    PinnedSource::ImageDefault
}

/// One pinned setting: `from_env` over `field` when its variable is set,
/// else `image_default` when there is one, else the stored value kept;
/// said in a line, and answered with where the value came from.
fn pin<T: PartialEq>(
    field: &mut T,
    from_env: Option<T>,
    image_default: Option<T>,
    p: Pinned,
    out: &mut Applied,
    show: impl Fn(&T) -> String,
) -> PinnedSource {
    let (value, source) = match (from_env, image_default) {
        (Some(v), _) => (v, PinnedSource::Variable),
        (None, Some(v)) => (v, PinnedSource::ImageDefault),
        (None, None) => {
            out.lines.push(format!(
                "pinned {} = {}, kept as stored ({} is not set)",
                p.setting(),
                show(field),
                p.variable()
            ));
            return PinnedSource::Kept;
        }
    };
    out.changed |= *field != value;
    *field = value;
    out.lines.push(match source {
        PinnedSource::ImageDefault => format!(
            "pinned {} = {}: the image's default ({} is not set)",
            p.setting(),
            show(field),
            p.variable()
        ),
        _ => format!(
            "pinned {} = {} ({})",
            p.setting(),
            show(field),
            p.variable()
        ),
    });
    source
}

/// A pinned setting whose unset variable clears it (the TLS pair, the
/// public URL): the variable's value, or empty.
fn clear_or_set(
    field: &mut String,
    from_env: Option<String>,
    p: Pinned,
    out: &mut Applied,
) -> PinnedSource {
    let (value, source) = match from_env {
        Some(v) => (v, PinnedSource::Variable),
        None => (String::new(), PinnedSource::ImageDefault),
    };
    out.changed |= *field != value;
    *field = value;
    out.lines.push(match source {
        PinnedSource::Variable => format!("pinned {} = {} ({})", p.setting(), field, p.variable()),
        _ => format!(
            "pinned {} = unset: the image's default ({} is not set)",
            p.setting(),
            p.variable()
        ),
    });
    source
}

fn seed_line(k: Seeded, value: String, from_variable: bool) -> String {
    let from = if from_variable {
        k.variable().to_string()
    } else {
        format!("the image's default; {} is not set", k.variable())
    };
    format!(
        "seeded {} = {value} into a data dir no container start had seeded ({from})",
        k.setting()
    )
}

fn unseeded_line(k: Seeded, stored: String) -> String {
    format!(
        "{} seeds a data dir once only: this one's stored {} = {stored} stays (change it on the \
         dashboard)",
        k.variable(),
        k.setting()
    )
}

/// The warning a container start logs when its plain listener is on a
/// loopback address without a TLS listener, when it is: nothing outside the
/// container reaches it through the published port, while a health check
/// inside the container still passes, so the service looks healthy and
/// serves nobody. Behind a TLS listener loopback is where it belongs (D6).
pub fn loopback_warning(s: &Settings) -> Option<String> {
    if s.tls.on() {
        return None;
    }
    let addr: SocketAddr = s.bind_addr.trim().parse().ok()?;
    addr.ip().is_loopback().then(|| {
        format!(
            "this server is unreachable from outside its container: the bind address {addr} is \
             a loopback address and no TLS listener is configured, so the published port \
             reaches nothing while the health check inside the container still passes. Set \
             {}=0.0.0.0:{} in the unit file (or configure TLS) and restart the service \
             ({RESTART_COMMAND})",
            Pinned::BindAddr.variable(),
            addr.port()
        )
    })
}

/// [`apply`] on `db`'s stored settings, checked for two listeners on one TCP
/// port ([`crate::config::tcp_port_collision`]), written when it changed
/// them, each line logged, and the loopback warning ([`loopback_warning`]): the
/// container start's step before the snapshot is first loaded
/// ([`crate::state::AppState::init_started`]). Answers where each pinned
/// value came from. A failure ends the start: a server that comes up on a
/// guessed address is worse than one that does not come up.
pub async fn apply_at_start(
    db: &sqlx::SqlitePool,
    env: &ContainerEnv,
) -> anyhow::Result<[PinnedSource; Pinned::ALL.len()]> {
    let mut s = crate::store::load_settings(db)
        .await
        .map_err(|e| anyhow::anyhow!("reading the settings to pin and seed: {e}"))?;
    let applied = apply(&mut s, env);
    // Two listeners on one TCP port end the start here, naming both rows
    // and the unit's variables, rather than at the second bind.
    crate::config::tcp_port_collision(&s, true).map_err(|e| anyhow::anyhow!("{e}"))?;
    if applied.changed {
        crate::store::save_settings(db, &s)
            .await
            .map_err(|e| anyhow::anyhow!("writing the pinned and seeded settings: {e}"))?;
    }
    for line in &applied.lines {
        tracing::info!("container mode: {line}");
    }
    if let Some(warning) = loopback_warning(&s) {
        tracing::warn!("{warning}");
    }
    Ok(applied.sources)
}
