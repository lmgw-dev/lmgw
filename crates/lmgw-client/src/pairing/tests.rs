//! The link v2's parser and the dialog's facts (moved from Kai desktop's
//! `pairing.rs` with its tests, for v=2).

use super::*;

const TOKEN: &str = "lmgw-enrol-4b1d0123456789abcdef0123456789ab";
const FP_HEX: &str = "9f2c0123456789abcdef0123456789abcdef0123456789abcdef0123456741ab";

fn link(params: &str) -> String {
    format!("lmgw-pair:?{params}")
}

fn good() -> String {
    link(&format!(
        "v=2&name=phone&url=https%3A%2F%2F192.0.2.1%3A8788&url=https%3A%2F%2F198.51.100.20%3A8788\
         &fp=sha256%3A{FP_HEX}&token={TOKEN}"
    ))
}

#[test]
fn a_good_link_parses() {
    let l = PairingLink::parse(&good()).unwrap();
    assert_eq!(l.name, "phone");
    assert_eq!(
        l.urls.as_slice(),
        ["https://192.0.2.1:8788", "https://198.51.100.20:8788"]
    );
    assert_eq!(l.fp.to_string(), format!("sha256:{FP_HEX}"));
    assert_eq!(l.fp.short(), "sha256:9f2c…41ab");
    assert_eq!(l.token.expose(), TOKEN);
    // Pasted with whitespace, an upper-case scheme, parameters in another
    // order, `+` as a space, an unescaped `:` in fp and a path prefix.
    let l = PairingLink::parse(&format!(
        "  LMGW-PAIR:?token={TOKEN}&fp=sha256:{}&name=my+phone&url=https%3A%2F%2Fgateway.example%2Fbase%2F&v=2\n",
        FP_HEX.to_uppercase()
    ))
    .unwrap();
    assert_eq!(
        (l.urls.as_slice()[0].as_str(), l.name.as_str()),
        ("https://gateway.example/base", "my phone")
    );
    assert_eq!(
        l.fp.to_string(),
        format!("sha256:{FP_HEX}"),
        "hex in either case"
    );
}

#[test]
fn the_version_must_be_2_and_a_v1_link_is_told_why() {
    let rest = format!("name=n&url=https%3A%2F%2Fh&fp=sha256:{FP_HEX}&token={TOKEN}");
    assert_eq!(
        PairingLink::parse(&link(&rest)),
        Err(LinkError::MissingVersion)
    );
    for v in ["3", "0", "02", "2.0", "", "%202"] {
        let e = PairingLink::parse(&link(&format!("v={v}&{rest}"))).unwrap_err();
        assert!(
            matches!(e, LinkError::UnsupportedVersion(_)),
            "v={v}: {e:?}"
        );
    }
    // A v1 link (a bearer key, an http url) reports the version, not its
    // parameters.
    let v1 = link(
        "v=1&url=http%3A%2F%2F127.0.0.1%3A8001&name=desktop\
         &key=lmgw-device-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );
    let e = PairingLink::parse(&v1).unwrap_err();
    assert_eq!(e, LinkError::PairedBeforeTls);
    assert_eq!(
        e.to_string(),
        "this link is from a pairing before TLS; make a new one on lmgw's Devices card"
    );
}

#[test]
fn missing_or_empty_required_parameters() {
    let all = [
        ("name", "n".to_owned()),
        ("url", "https%3A%2F%2Fh".to_owned()),
        ("fp", format!("sha256:{FP_HEX}")),
        ("token", TOKEN.to_owned()),
    ];
    for (skip, _) in &all {
        let q: Vec<String> = all
            .iter()
            .filter(|(n, _)| n != skip)
            .map(|(n, v)| format!("{n}={v}"))
            .collect();
        let e = PairingLink::parse(&link(&format!("v=2&{}", q.join("&")))).unwrap_err();
        assert_eq!(e, LinkError::Missing(skip), "{skip}");
    }
    for empty in ["name", "url", "fp", "token"] {
        let q: Vec<String> = all
            .iter()
            .map(|(n, v)| format!("{n}={}", if *n == empty { "" } else { v.as_str() }))
            .collect();
        let e = PairingLink::parse(&link(&format!("v=2&{}", q.join("&")))).unwrap_err();
        assert_eq!(e, LinkError::Empty(empty), "{empty}");
    }
}

#[test]
fn malformed_links_are_refused() {
    assert_eq!(
        PairingLink::parse("https://example.com"),
        Err(LinkError::NotALink)
    );
    assert_eq!(
        PairingLink::parse("lmgw-pair:v=2"),
        Err(LinkError::NotALink)
    );
    assert_eq!(PairingLink::parse("lmgw-pai"), Err(LinkError::NotALink));
    for dup in ["name=other", "fp=sha256:00", "token=x", "v=2"] {
        let e = PairingLink::parse(&format!("{}&{dup}", good())).unwrap_err();
        assert!(matches!(e, LinkError::Duplicate(_)), "{dup}: {e:?}");
    }
    let unknown = format!("{}&label=x", good());
    assert_eq!(
        PairingLink::parse(&unknown),
        Err(LinkError::Unknown("a parameter `label`".into()))
    );
    // `key` is v1's: a v2 link never carries one.
    let key = format!("{}&key=lmgw-device-00", good());
    assert_eq!(
        PairingLink::parse(&key),
        Err(LinkError::Unknown("a parameter `key`".into()))
    );
    let short = link(&format!(
        "v=2&name=n&url=https%3A%2F%2Fh&fp=sha256:{FP_HEX}&token=lmgw-enrol-0123"
    ));
    assert_eq!(
        PairingLink::parse(&short),
        Err(LinkError::BadToken { len: 15 })
    );
    let upper = link(&format!(
        "v=2&name=n&url=https%3A%2F%2Fh&fp=sha256:{FP_HEX}&token={}",
        TOKEN.replace("abcdef", "ABCDEF")
    ));
    assert!(
        matches!(PairingLink::parse(&upper), Err(LinkError::BadToken { .. })),
        "lmgw hashes the token as written: its digits are lowercase"
    );
    let bad_fp = link(&format!(
        "v=2&name=n&url=https%3A%2F%2Fh&fp=md5:00&token={TOKEN}"
    ));
    assert_eq!(PairingLink::parse(&bad_fp), Err(LinkError::BadFingerprint));
}

#[test]
fn urls_are_https_and_never_twice() {
    let with_urls = |urls: &str| {
        link(&format!(
            "v=2&name=n&{urls}&fp=sha256:{FP_HEX}&token={TOKEN}"
        ))
    };
    let e = PairingLink::parse(&with_urls(
        "url=https%3A%2F%2Fh&url=http%3A%2F%2F198.51.100.20%3A8787",
    ))
    .unwrap_err();
    assert!(
        matches!(&e, LinkError::BadUrl { position: 2, reason } if reason.contains("https")),
        "{e:?}"
    );
    let e = PairingLink::parse(&with_urls(
        "url=https%3A%2F%2Fh%3A8788&url=https%3A%2F%2Fg&url=HTTPS%3A%2F%2FH%3A8788%2F",
    ))
    .unwrap_err();
    assert_eq!(
        e,
        LinkError::DuplicateUrl {
            first: 1,
            second: 3
        }
    );
    for bad in [
        "https%3A%2F%2Fu%3Ap%40h",
        "https%3A%2F%2Fh%3Fq%3D1",
        "https%3A%2F%2Fh%23f",
    ] {
        assert!(
            matches!(
                PairingLink::parse(&with_urls(&format!("url={bad}"))),
                Err(LinkError::BadUrl { position: 1, .. })
            ),
            "{bad}"
        );
    }
}

#[test]
fn no_error_echoes_the_token() {
    let secret = &TOKEN[11..];
    let fp = format!("sha256:{FP_HEX}");
    let cases = [
        // The token smuggled into a parameter name, a url, the version, the
        // fingerprint, and a token one digit too long.
        link(&format!("v=2&name=n&url=https%3A%2F%2Fh&fp={fp}&{TOKEN}")),
        link(&format!(
            "v=2&name=n&url=https%3A%2F%2Fh%3Fk%3D{TOKEN}&fp={fp}&token={TOKEN}"
        )),
        link(&format!("v=2&name=n&url={TOKEN}&fp={fp}&token={TOKEN}")),
        link(&format!(
            "v={TOKEN}&name=n&url=https%3A%2F%2Fh&fp={fp}&token={TOKEN}"
        )),
        link(&format!(
            "v=2&name=n&url=https%3A%2F%2Fh&fp=sha256:{TOKEN}&token={TOKEN}"
        )),
        link(&format!(
            "v=2&name=n&url=https%3A%2F%2Fh&fp={fp}&token={TOKEN}0"
        )),
    ];
    for c in cases {
        let e = PairingLink::parse(&c).unwrap_err();
        assert!(!e.to_string().contains(secret), "{e}");
        assert!(!format!("{e:?}").contains(secret), "{e:?}");
    }
    // A token split by a stray `&` or `=`: its pieces are never named, as a
    // parameter or as a version.
    let (head, tail) = TOKEN.split_at(TOKEN.len() - 16);
    for (c, piece) in [
        (
            link(&format!(
                "v=2&name=n&url=https%3A%2F%2Fh&fp={fp}&token={head}&{tail}"
            )),
            tail,
        ),
        (
            link(&format!(
                "v=4b1d0123&name=n&url=https%3A%2F%2Fh&fp={fp}&token={TOKEN}"
            )),
            "4b1d0123",
        ),
    ] {
        let e = PairingLink::parse(&c).unwrap_err().to_string();
        assert!(!e.contains(piece), "{e}");
    }
    assert!(PairingLink::parse(&format!("{}&label=x", good()))
        .unwrap_err()
        .to_string()
        .contains("`label`"));
    let rest = format!("&name=n&url=https%3A%2F%2Fh&fp={fp}&token={TOKEN}");
    for (v, shown) in [("3", "v=3"), ("2.1", "v=2.1"), ("v3beta", "v=v3beta")] {
        let e = PairingLink::parse(&link(&format!("v={v}{rest}"))).unwrap_err();
        assert!(e.to_string().contains(shown), "{e}");
    }
    let parsed = PairingLink::parse(&good()).unwrap();
    assert!(!format!("{parsed:?}").contains(secret));
    let facts = PairFacts::new(&parsed, None);
    assert!(!format!("{facts:?} {:?}", facts.lines()).contains(secret));
}

#[test]
fn the_facts_name_the_gateway_the_device_its_identity_and_what_is_replaced() {
    let l = PairingLink::parse(&good()).unwrap();
    let f = PairFacts::new(&l, None);
    assert_eq!(
        f.lines(),
        [
            FactLine::Plain(
                "Gateway: https://192.0.2.1:8788, https://198.51.100.20:8788, tried in that order"
                    .into()
            ),
            FactLine::Plain("Device name: phone".into()),
            FactLine::Plain("Gateway identity: sha256:9f2c…41ab".into()),
        ],
        "a first pairing: no warning"
    );
    let f = PairFacts::new(&l, Some("https://gateway.example:8788"));
    assert!(matches!(&f.lines()[3], FactLine::Warning(w)
        if w.contains("replaces this device's pairing with https://gateway.example:8788")
            && w.contains("stops using that gateway")));
    let f = PairFacts::new(&l, Some("https://192.0.2.1:8788"));
    assert!(matches!(&f.lines()[3], FactLine::Warning(w)
        if w.contains("a new key and certificate")));
    let one = link(&format!(
        "v=2&name=desktop&url=https%3A%2F%2F127.0.0.1%3A8788&fp=sha256:{FP_HEX}&token={TOKEN}"
    ));
    let f = PairFacts::new(&PairingLink::parse(&one).unwrap(), None);
    assert_eq!(
        f.lines()[0],
        FactLine::Plain("Gateway: https://127.0.0.1:8788".into())
    );
}

#[test]
fn a_device_name_is_made_safe_to_show() {
    assert_eq!(display_name("desktop"), "desktop");
    assert_eq!(display_name("a\nb\u{202E}c"), "a\u{FFFD}b\u{FFFD}c");
    // K2-14: a line separator would show a second "Gateway" line;
    // zero-width characters and the soft hyphen hide in a name.
    assert_eq!(
        display_name("desktop\u{2028}Gateway: https://127.0.0.1:8788"),
        "desktop\u{FFFD}Gateway: https://127.0.0.1:8788"
    );
    for c in [
        '\u{2029}',
        '\u{200B}',
        '\u{200D}',
        '\u{FEFF}',
        '\u{00AD}',
        '\u{E0041}',
    ] {
        assert_eq!(
            display_name(&format!("a{c}b")),
            "a\u{FFFD}b",
            "U+{:04X}",
            c as u32
        );
    }
    assert_eq!(
        display_name("Küche — Tür"),
        "Küche — Tür",
        "letters and dashes stay"
    );
    let long = "x".repeat(NAME_SHOWN_CHARS + 5);
    let shown = display_name(&long);
    assert!(shown.ends_with("… (shortened)"), "{shown}");
    assert_eq!(
        shown.chars().filter(|c| *c == 'x').count(),
        NAME_SHOWN_CHARS
    );
}

#[test]
fn base_urls_are_https_and_normalized() {
    assert_eq!(
        normalize_base_url("https://127.0.0.1:8788/").unwrap(),
        "https://127.0.0.1:8788"
    );
    assert_eq!(
        normalize_base_url("HTTPS://Host:443").unwrap(),
        "https://host"
    );
    assert_eq!(
        normalize_base_url("https://h/a/b/").unwrap(),
        "https://h/a/b"
    );
    assert_eq!(
        normalize_base_url("https://[::1]:8788").unwrap(),
        "https://[::1]:8788"
    );
    assert!(normalize_base_url("https://h#x").is_err());
    let e = normalize_base_url("http://127.0.0.1:8787").unwrap_err();
    assert!(e.contains("https"), "{e}");
}

#[test]
fn a_fingerprint_pins_a_key_not_a_certificate() {
    use rcgen::{CertificateParams, KeyPair, PublicKeyData};
    let key = KeyPair::generate().unwrap();
    let fp = Fingerprint::of_spki(&key.subject_public_key_info());
    // Two certificates for the one key (a re-issue) both carry the pin.
    for name in ["a.test", "b.test"] {
        let cert = CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        assert!(fp.pins_certificate(cert.der()));
    }
    let other = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(Vec::<String>::new())
        .unwrap()
        .self_signed(&other)
        .unwrap();
    assert!(!fp.pins_certificate(cert.der()));
    assert!(!fp.pins_certificate(b"not a certificate"));
    assert_eq!(Fingerprint::parse(&fp.to_string()), Ok(fp));
}
