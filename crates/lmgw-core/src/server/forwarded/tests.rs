//! [`super::read`]: whose headers count, the client's hop, the one value
//! of the scheme and the host, and the refusals.

use super::*;

fn nets(list: &[&str]) -> Vec<TrustedNet> {
    list.iter().map(|n| TrustedNet::parse(n).unwrap()).collect()
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

fn addr(t: &str) -> SocketAddr {
    t.parse().unwrap()
}

const PROXY: &str = "10.89.0.2:51234";

#[test]
fn an_untrusted_peer_s_headers_change_nothing() {
    let h = headers(&[
        ("x-forwarded-for", "127.0.0.1"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-host", "lmgw.example"),
    ]);
    assert_eq!(
        read(&h, addr("192.0.2.7:1"), &nets(&["10.89.0.0/24"])),
        Ok(None)
    );
    // Proxy mode off: nobody is trusted.
    assert_eq!(read(&h, addr(PROXY), &[]), Ok(None));
    // A trusted address with no forwarded header talks to lmgw directly.
    assert_eq!(
        read(&headers(&[]), addr(PROXY), &nets(&["10.89.0.0/24"])),
        Ok(None)
    );
}

#[test]
fn the_client_is_the_right_most_hop_that_is_not_a_trusted_proxy() {
    let trusted = nets(&["10.89.0.0/24", "fd00::/8"]);
    let client = |xff: &[&str]| {
        let pairs: Vec<(&str, &str)> = xff.iter().map(|v| ("x-forwarded-for", *v)).collect();
        read(&headers(&pairs), addr(PROXY), &trusted)
            .unwrap()
            .unwrap()
            .client
    };
    // A client that writes its own hop is left of the one the proxy
    // appended.
    assert_eq!(client(&["127.0.0.1, 203.0.113.5"]), addr("203.0.113.5:0"));
    // Across lines, in order; the trusted hops on the right skipped.
    assert_eq!(
        client(&["198.51.100.1", "203.0.113.5, 10.89.0.9", "[fd00::3]:443"]),
        addr("203.0.113.5:0")
    );
    // Every hop trusted: the left-most.
    assert_eq!(client(&["10.89.0.7, 10.89.0.9"]), addr("10.89.0.7:0"));
    // A hop the walk reaches that names no address: unknown, never the
    // proxy's own address.
    assert_eq!(client(&["203.0.113.5, unknown, 10.89.0.9"]), UNKNOWN_CLIENT);
    // Left of the client it is the client's to write: not reached.
    assert_eq!(client(&["unknown, 203.0.113.5"]), addr("203.0.113.5:0"));
    // An IPv4-mapped hop and a port.
    assert_eq!(
        client(&["[::ffff:203.0.113.5]:4711"]),
        addr("203.0.113.5:4711")
    );
    assert_eq!(
        client(&["::ffff:10.89.0.7"]),
        addr("10.89.0.7:0"),
        "mapped and trusted"
    );
    // No X-Forwarded-For beside the other two: the request crossed the
    // proxy, and its client is unknown, never the proxy's own address.
    for pairs in [
        vec![("x-forwarded-proto", "https")],
        vec![("x-forwarded-host", "lmgw.example")],
        vec![("x-forwarded-for", " , ,"), ("x-forwarded-proto", "https")],
    ] {
        let f = read(&headers(&pairs), addr(PROXY), &trusted)
            .unwrap()
            .unwrap();
        assert_eq!(f.client, UNKNOWN_CLIENT, "{pairs:?}");
    }
    // The peer is compared canonical, as the listener hands it over.
    let mapped = addr("[::ffff:10.89.0.2]:51234");
    assert!(read(
        &headers(&[("x-forwarded-for", "203.0.113.5")]),
        mapped,
        &trusted
    )
    .unwrap()
    .is_some());
}

#[test]
fn the_scheme_and_the_host_are_one_value_each() {
    let trusted = nets(&["10.89.0.2"]);
    let f = read(
        &headers(&[
            ("x-forwarded-for", "203.0.113.5"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "lmgw.example"),
        ]),
        addr(PROXY),
        &trusted,
    )
    .unwrap()
    .unwrap();
    assert_eq!(f.scheme, Some(Scheme::Https));
    assert_eq!(f.host.as_deref(), Some("lmgw.example"));
    // Only the client hop: the proxy's Host stands.
    let f = read(
        &headers(&[("x-forwarded-for", "203.0.113.5")]),
        addr(PROXY),
        &trusted,
    )
    .unwrap()
    .unwrap();
    assert_eq!((f.scheme, f.host), (None, None));
    for (pairs, says) in [
        (
            vec![
                ("x-forwarded-proto", "https"),
                ("x-forwarded-proto", "http"),
            ],
            "X-Forwarded-Proto carries values that differ (https, http)",
        ),
        (
            vec![("x-forwarded-host", "lmgw.example, evil.example")],
            "X-Forwarded-Host carries values that differ",
        ),
        (
            vec![("x-forwarded-proto", "gopher")],
            "X-Forwarded-Proto names the scheme 'gopher'",
        ),
        (
            vec![("x-forwarded-host", "u@lmgw.example")],
            "X-Forwarded-Host names the host 'u@lmgw.example'",
        ),
        (
            vec![
                ("x-forwarded-for", "203.0.113.5"),
                ("forwarded", "for=203.0.113.6"),
            ],
            "both Forwarded and X-Forwarded-*",
        ),
        (
            vec![("forwarded", "for=1;for=2")],
            "Forwarded does not parse",
        ),
    ] {
        let e = read(&headers(&pairs), addr(PROXY), &trusted).unwrap_err();
        assert!(e.contains(says), "{pairs:?}: {e}");
        // From an untrusted peer the same headers are not even read.
        assert_eq!(
            read(&headers(&pairs), addr("192.0.2.7:1"), &trusted),
            Ok(None)
        );
    }
}

#[test]
fn forwarded_reads_the_client_s_own_element() {
    let trusted = nets(&["10.89.0.0/24"]);
    let read_fw = |v: &str| {
        read(&headers(&[("forwarded", v)]), addr(PROXY), &trusted)
            .unwrap()
            .unwrap()
    };
    // The client's own element is left of the proxy's; the proxy's says
    // what the client used.
    let f = read_fw(
        "for=127.0.0.1;proto=http;host=evil.example, \
         for=\"203.0.113.5:4711\";proto=https;host=lmgw.example",
    );
    assert_eq!(f.client, addr("203.0.113.5:4711"));
    assert_eq!(f.scheme, Some(Scheme::Https));
    assert_eq!(f.host.as_deref(), Some("lmgw.example"));
    // A chain of two trusted proxies: the outer one's element.
    let f = read_fw(
        "for=\"[2001:db8::5]\";proto=https;host=lmgw.example, for=10.89.0.9;proto=http;host=inner",
    );
    assert_eq!(f.client, addr("[2001:db8::5]:0"));
    assert_eq!(f.scheme, Some(Scheme::Https));
    assert_eq!(f.host.as_deref(), Some("lmgw.example"));
    // An obfuscated client, and an element without `for`.
    let f = read_fw("for=_hidden;proto=https");
    assert_eq!((f.client, f.scheme), (UNKNOWN_CLIENT, Some(Scheme::Https)));
    let f = read_fw("proto=https;host=lmgw.example");
    assert_eq!(f.client, UNKNOWN_CLIENT);
    assert_eq!(f.host.as_deref(), Some("lmgw.example"));
    // Every element trusted: the left-most.
    assert_eq!(
        read_fw("for=10.89.0.7, for=10.89.0.9").client,
        addr("10.89.0.7:0")
    );
    // Empty elements only: the request crossed the proxy, which named no
    // client; unknown, never the proxy's own address.
    assert_eq!(read_fw(",,").client, UNKNOWN_CLIENT);
    let e = read(
        &headers(&[("forwarded", "for=203.0.113.5;proto=ftp")]),
        addr(PROXY),
        &trusted,
    )
    .unwrap_err();
    assert!(
        e.contains("Forwarded's proto names the scheme 'ftp'"),
        "{e}"
    );
}

#[test]
fn the_forwarded_host_replaces_the_host_and_an_http2_authority() {
    let mut req = Request::builder()
        .uri("http://127.0.0.1:8787/v1/models?x=1")
        .header("host", "127.0.0.1:8787")
        .body(axum::body::Body::empty())
        .unwrap();
    set_host(&mut req, "lmgw.example:8443");
    assert_eq!(req.headers()["host"], "lmgw.example:8443");
    assert_eq!(
        req.uri().to_string(),
        "http://lmgw.example:8443/v1/models?x=1"
    );
    // HTTP/1.1's origin form has no authority to replace.
    let mut req = Request::builder()
        .uri("/v1/models")
        .body(axum::body::Body::empty())
        .unwrap();
    set_host(&mut req, "lmgw.example");
    assert_eq!(req.headers()["host"], "lmgw.example");
    assert_eq!(req.uri().to_string(), "/v1/models");
}
