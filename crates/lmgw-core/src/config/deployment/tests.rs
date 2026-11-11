//! The pinned and seeded settings, the stop budget (headless server design
//! §2, D20).

use super::*;
use crate::config::TLS_LISTEN_PROPOSED;

use PinnedSource::{ImageDefault as Image, Kept, Variable as Var};

#[test]
fn the_deployment_is_container_or_the_desktop() {
    assert_eq!(parse_deployment(None), Ok(Deployment::Desktop));
    assert_eq!(parse_deployment(Some("")), Ok(Deployment::Desktop));
    assert_eq!(
        parse_deployment(Some("container")),
        Ok(Deployment::Container)
    );
    let e = parse_deployment(Some("server")).unwrap_err();
    assert!(
        e.contains(DEPLOYMENT_VAR) && e.contains("\"server\""),
        "{e}"
    );
}

/// Settings a container start has seeded already.
fn seeded() -> Settings {
    Settings {
        container_seeded: true,
        bind_addr: IMAGE_BIND_ADDR.into(),
        tls: crate::config::TlsSettings {
            listen: TLS_LISTEN_PROPOSED.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn a_pinned_variable_wins_and_a_later_start_without_it_keeps_the_value() {
    let mut s = seeded();
    let env = ContainerEnv {
        bind_addr: Some("0.0.0.0:9000".parse().unwrap()),
        webrtc_port: Some(9001),
        ..Default::default()
    };
    let a = apply(&mut s, &env);
    assert!(a.changed);
    assert_eq!(
        a.sources,
        [Var, Var, Image, Image, Kept, Image, Kept, Image]
    );
    assert_eq!(s.bind_addr, "0.0.0.0:9000");
    assert_eq!(s.realtime.webrtc.webrtc_port, Some(9001));
    let again = apply(&mut s, &ContainerEnv::default());
    assert!(!again.changed, "{again:?}");
    assert_eq!(
        again.sources,
        [Kept, Kept, Image, Image, Kept, Image, Kept, Image]
    );
    assert_eq!(s.bind_addr, "0.0.0.0:9000");
    assert_eq!(s.realtime.webrtc.webrtc_port, Some(9001));
    assert!(
        again
            .lines
            .iter()
            .any(|l| l.contains("kept as stored") && l.contains("LMGW_BIND_ADDR is not set")),
        "{again:?}"
    );
}

#[test]
fn an_unseeded_data_dir_gets_the_image_defaults_the_seeds_and_the_marker() {
    let mut s = Settings::default();
    assert!(!s.auth_enabled && !s.container_seeded);
    let a = apply(&mut s, &ContainerEnv::default());
    assert!(a.changed);
    assert!(s.container_seeded, "the marker is in the seeds' write");
    assert_eq!(
        a.sources,
        [Image, Kept, Image, Image, Image, Image, Kept, Image],
        "{a:?}"
    );
    assert_eq!(s.bind_addr, IMAGE_BIND_ADDR);
    assert!(
        s.auth_enabled,
        "Require API key is on in the image (ruling 9)"
    );
    assert_eq!(s.devices.listen, DevicesListen::All);
    assert_eq!(s.realtime.webrtc.webrtc_port, None);
    for seeded in ["seeded auth_enabled = true", "seeded devices.listen = all"] {
        assert!(a.lines.iter().any(|l| l.starts_with(seeded)), "{a:?}");
    }

    let mut s = Settings::default();
    let env = ContainerEnv {
        require_api_key: Some(false),
        devices_listen: Some(DevicesListen::Loopback),
        ..Default::default()
    };
    apply(&mut s, &env);
    assert!(!s.auth_enabled);
    assert_eq!(s.devices.listen, DevicesListen::Loopback);
    assert!(s.container_seeded);
}

#[test]
fn a_seed_never_overrides_a_seeded_data_dir() {
    let mut s = seeded();
    let env = ContainerEnv {
        require_api_key: Some(true),
        devices_listen: Some(DevicesListen::All),
        ..Default::default()
    };
    let a = apply(&mut s, &env);
    assert!(!a.changed, "{a:?}");
    assert!(!s.auth_enabled);
    assert_eq!(s.devices.listen, DevicesListen::Loopback);
    assert!(
        a.lines
            .iter()
            .any(|l| l.contains("LMGW_REQUIRE_API_KEY seeds a data dir once only")),
        "{a:?}"
    );
}

#[test]
fn the_row_says_whether_the_variable_was_set() {
    let set = Pinned::WebrtcPort.why(PinnedSource::Variable);
    assert!(set.starts_with("Set by this server's container (LMGW_WEBRTC_PORT)."));
    let kept = Pinned::WebrtcPort.why(PinnedSource::Kept);
    assert!(
        kept.starts_with("Kept as stored; LMGW_WEBRTC_PORT is not set on this start."),
        "{kept}"
    );
    let image = Pinned::BindAddr.why(PinnedSource::ImageDefault);
    assert!(image.contains("LMGW_BIND_ADDR is not set"), "{image}");
    for why in [set, kept, image] {
        assert!(why.contains(RESTART_COMMAND), "{why}");
    }
}

fn container(sources: [PinnedSource; 8]) -> StartFacts {
    StartFacts {
        deployment: Deployment::Container,
        stop_timeout: None,
        sources,
    }
}

#[test]
fn only_container_mode_refuses_a_pinned_change() {
    let before = Settings::default();
    let mut after = before.clone();
    after.bind_addr = "0.0.0.0:1".into();
    assert!(refuse_pinned(StartFacts::default(), &before, &after).is_ok());
    let e = refuse_pinned(container([PinnedSource::Variable; 8]), &before, &after).unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (400, PINNED_CODE));
    assert!(
        e.message
            .starts_with("Settings → Network & access → Gateway → Bind address: Set by")
            && e.message.contains("LMGW_BIND_ADDR")
            && e.message.contains(RESTART_COMMAND)
            && !e.message.contains("(bind_addr)"),
        "{}",
        e.message
    );

    let mut after = before.clone();
    after.realtime.webrtc.webrtc_port = Some(3478);
    let e = refuse_pinned(container([PinnedSource::Kept; 8]), &before, &after).unwrap_err();
    assert!(
        e.message
            .starts_with("Settings → Realtime → WebRTC calls → UDP port: Kept as stored")
            && e.message.contains("LMGW_WEBRTC_PORT is not set"),
        "{}",
        e.message
    );

    // Anything else, and a pinned field at its stored value, goes through.
    let mut after = before.clone();
    after.retention_days += 1;
    assert!(refuse_pinned(container([PinnedSource::Kept; 8]), &before, &after).is_ok());
}

#[test]
fn a_loopback_bind_address_is_warned_about() {
    let mut s = Settings {
        bind_addr: "127.0.0.1:8787".into(),
        ..Default::default()
    };
    let w = loopback_warning(&s).expect("loopback is unreachable from outside");
    assert!(
        w.contains("127.0.0.1:8787") && w.contains("LMGW_BIND_ADDR=0.0.0.0:8787"),
        "{w}"
    );
    s.bind_addr = "[::1]:8787".into();
    assert!(loopback_warning(&s).is_some());
    s.bind_addr = IMAGE_BIND_ADDR.into();
    assert_eq!(loopback_warning(&s), None);
}

#[test]
fn the_stop_budget_is_the_four_bounds() {
    assert_eq!(quit_constant_seconds(), 35);
    let fits = stop_budget(Some(95), 60);
    assert_eq!(fits.needed_seconds, 95);
    assert!(!fits.over, "{fits:?}");
    let over = stop_budget(Some(95), 120);
    assert_eq!(over.needed_seconds, 155);
    assert!(over.over);
    assert!(
        over.sentence.starts_with(
            "this server's unit gives lmgw 95 s to stop; with an unload timeout of 120 s a \
             quit can take 155 s."
        ),
        "{}",
        over.sentence
    );
    let unset = stop_budget(None, 60);
    assert!(!unset.over);
    assert!(
        unset.sentence.contains("LMGW_STOP_TIMEOUT is not set"),
        "{}",
        unset.sentence
    );
}

#[test]
fn the_stop_timeout_is_kept_in_container_mode_only() {
    let env = ContainerEnv {
        stop_timeout: Some(95),
        ..Default::default()
    };
    let desktop = Start {
        deployment: Deployment::Desktop,
        env: env.clone(),
    };
    assert_eq!(StartFacts::from(&desktop).stop_timeout, None);
    let container = Start {
        deployment: Deployment::Container,
        env,
    };
    assert_eq!(StartFacts::from(&container).stop_timeout, Some(95));
}

#[test]
fn a_set_pinned_variable_is_the_value_s_source() {
    let start = Start {
        deployment: Deployment::Container,
        env: ContainerEnv {
            webrtc_port: Some(3478),
            ..Default::default()
        },
    };
    let facts = StartFacts::from(&start);
    assert_eq!(facts.source(Pinned::BindAddr), PinnedSource::Kept);
    assert_eq!(facts.source(Pinned::WebrtcPort), PinnedSource::Variable);
}

/// The TLS variables (§2.1, §4): the pair and the public URL from the unit
/// or cleared, the address kept or the image's, and the plain listener
/// following TLS onto loopback and back (D6) while its address is the
/// image's own.
#[test]
fn the_tls_variables_pin_and_the_plain_listener_follows_them() {
    let mut s = Settings::default();
    let tls = ContainerEnv {
        tls_cert: Some("/etc/lmgw/tls/tls.crt".into()),
        tls_key: Some("/etc/lmgw/tls/tls.key".into()),
        public_url: Some("https://lmgw.example:8443".into()),
        ..Default::default()
    };
    let a = apply(&mut s, &tls);
    assert_eq!(s.tls.cert_path, "/etc/lmgw/tls/tls.crt");
    assert_eq!(s.tls.listen, TLS_LISTEN_PROPOSED);
    assert_eq!(s.public_url, "https://lmgw.example:8443");
    assert_eq!(s.bind_addr, "127.0.0.1:8787", "behind TLS: loopback");
    assert_eq!(a.sources, [Image, Kept, Var, Var, Image, Var, Kept, Image]);
    assert!(
        a.lines
            .iter()
            .any(|l| l.starts_with("the plain listener moves to 127.0.0.1:8787")),
        "{a:?}"
    );
    assert_eq!(loopback_warning(&s), None, "loopback is right behind TLS");
    // Again with the same unit: nothing moves.
    let again = apply(&mut s, &tls);
    assert!(!again.changed, "{again:?}");
    assert_eq!(again.sources[Pinned::BindAddr.index()], Kept);
    // The unit drops TLS: the pair and the URL go, the plain listener is
    // the published one again.
    let off = apply(&mut s, &ContainerEnv::default());
    assert!(off.changed);
    assert!(s.tls.cert_path.is_empty() && s.tls.key_path.is_empty() && s.public_url.is_empty());
    assert_eq!(s.bind_addr, "0.0.0.0:8787");
    assert!(loopback_warning(&s).is_none());
    // An address the administrator set stays as set, TLS or not.
    let mut s = seeded();
    s.bind_addr = "192.0.2.5:9000".into();
    apply(&mut s, &tls);
    assert_eq!(s.bind_addr, "192.0.2.5:9000");
    // And one the unit sets wins outright.
    let pinned = ContainerEnv {
        bind_addr: Some("0.0.0.0:8787".parse().unwrap()),
        tls_listen: Some("0.0.0.0:9443".parse().unwrap()),
        webrtc_tls_port: Some(9443),
        ..tls.clone()
    };
    apply(&mut s, &pinned);
    assert_eq!(s.bind_addr, "0.0.0.0:8787");
    assert_eq!(s.tls.listen, "0.0.0.0:9443");
    assert_eq!(s.realtime.webrtc.webrtc_tls_port, Some(9443));
}

#[test]
fn a_pinned_tls_field_is_refused_by_a_save() {
    let before = Settings::default();
    for (field, set) in [
        (
            "Certificate",
            (|s: &mut Settings| s.tls.cert_path = "/x".into()) as fn(&mut Settings),
        ),
        ("Private key", |s| s.tls.key_path = "/x".into()),
        ("TLS address", |s| s.tls.listen = "0.0.0.0:1".into()),
        ("Public URL", |s| s.public_url = "https://x.example".into()),
        ("TLS UDP port", |s| {
            s.realtime.webrtc.webrtc_tls_port = Some(1)
        }),
        ("Trusted proxies", |s| {
            s.trusted_proxies = vec!["10.89.0.0/24".into()]
        }),
    ] {
        let mut after = before.clone();
        set(&mut after);
        let e = refuse_pinned(container([PinnedSource::Variable; 8]), &before, &after).unwrap_err();
        assert!(e.message.contains(field), "{field}: {}", e.message);
        assert!(refuse_pinned(StartFacts::default(), &before, &after).is_ok());
    }
}

#[test]
fn an_unset_tls_variable_reads_as_cleared_before_the_start_applies() {
    let start = Start {
        deployment: Deployment::Container,
        env: ContainerEnv {
            tls_listen: Some("0.0.0.0:8443".parse().unwrap()),
            ..Default::default()
        },
    };
    let facts = StartFacts::from(&start);
    assert_eq!(facts.source(Pinned::TlsCert), Image);
    assert_eq!(facts.source(Pinned::TlsListen), Var);
    assert_eq!(facts.source(Pinned::WebrtcTlsPort), Kept);
}

#[test]
fn the_trusted_proxies_are_pinned_and_an_unset_variable_turns_proxy_mode_off() {
    let mut s = Settings {
        container_seeded: true,
        trusted_proxies: vec!["192.0.2.0/24".into()],
        ..Default::default()
    };
    let env = ContainerEnv {
        trusted_proxies: Some(vec!["10.89.0.0/24".into(), "127.0.0.1/32".into()]),
        ..Default::default()
    };
    let a = apply(&mut s, &env);
    assert!(a.changed);
    assert_eq!(s.trusted_proxies, vec!["10.89.0.0/24", "127.0.0.1/32"]);
    assert_eq!(a.sources[Pinned::TrustedProxies.index()], Var);
    assert!(
        a.lines
            .iter()
            .any(|l| l
                == "pinned trusted_proxies = 10.89.0.0/24, 127.0.0.1/32 (LMGW_TRUSTED_PROXIES)"),
        "{a:?}"
    );
    // A later start without the variable: off, not kept — a peer the unit
    // no longer names is trusted no more.
    let a = apply(&mut s, &ContainerEnv::default());
    assert!(s.trusted_proxies.is_empty());
    assert_eq!(a.sources[Pinned::TrustedProxies.index()], Image);
    assert!(
        a.lines
            .iter()
            .any(|l| l.starts_with("pinned trusted_proxies = none, proxy mode off")),
        "{a:?}"
    );
    let facts = StartFacts::from(&Start {
        deployment: Deployment::Container,
        env: ContainerEnv::default(),
    });
    assert_eq!(facts.source(Pinned::TrustedProxies), Image);
}
