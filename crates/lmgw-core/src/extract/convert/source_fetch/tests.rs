//! The fetcher against loopback servers started here; names resolve through
//! a fixed table, never the machine's DNS, and no test reaches a real host.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::header::HeaderMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::resolve::Table;
use super::*;
use crate::extract::convert::pool::ToolPool;
use crate::extract::convert::pool_size::Machine;

/// The review round's cases: origins, the pin, the pool, proxies, odd
/// redirects, local addresses.
mod hardening;

/// What one request asked: its path and its header lines, lowercase.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: Vec<String>,
}

/// What the server answers a path with.
enum Reply {
    /// Raw response bytes, the connection closed after them.
    Raw(Vec<u8>),
    /// The head, then the body slowly: one byte per `every`.
    Trickle(Vec<u8>, Duration),
}

type Answer = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

/// A loopback HTTP/1.1 server; its port and what it saw.
async fn server(answer: Answer) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            let answer = answer.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut b = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut b).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&b[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let mut lines = text.split("\r\n");
                let first = lines.next().unwrap_or_default();
                let path = first.split(' ').nth(1).unwrap_or_default().to_string();
                let headers = lines
                    .take_while(|l| !l.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect();
                log.lock().unwrap().push(Seen {
                    path: path.clone(),
                    headers,
                });
                match answer(&path) {
                    Reply::Raw(bytes) => {
                        let _ = sock.write_all(&bytes).await;
                    }
                    Reply::Trickle(head, every) => {
                        let _ = sock.write_all(&head).await;
                        for _ in 0..1000 {
                            tokio::time::sleep(every).await;
                            if sock.write_all(b"x").await.is_err() {
                                return;
                            }
                        }
                    }
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    (port, seen)
}

fn ok(body: &[u8]) -> Reply {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    Reply::Raw(r)
}

fn redirect(to: &str) -> Reply {
    Reply::Raw(
        format!(
            "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )
        .into_bytes(),
    )
}

fn limits() -> Limits {
    Limits {
        max_bytes: 1024,
        max_body_mb: 1,
        total: Duration::from_secs(10),
        connect: Duration::from_secs(5),
        max_redirects: 3,
    }
}

fn list(entries: &[String]) -> HostList {
    HostList::parse(entries).unwrap()
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

async fn get(
    url: &str,
    l: &HostList,
    r: &dyn resolve::Resolve,
    lim: Limits,
) -> Result<Fetched, FetchError> {
    let c = check(url, l)?;
    fetch(c, &HeaderMap::new(), &rules(l, r, &lim), lim.max_bytes).await
}

/// The rules with nothing local and a pool far larger than any test's
/// fetches (the pool's own bound has its test in [`hardening`]).
fn rules<'a>(l: &'a HostList, r: &'a dyn resolve::Resolve, lim: &'a Limits) -> Rules<'a> {
    static LOCAL: OnceLock<Local> = OnceLock::new();
    static POOL: OnceLock<ToolPool> = OnceLock::new();
    Rules {
        list: l,
        resolver: r,
        local: LOCAL.get_or_init(Local::none),
        limits: lim,
        slots: POOL.get_or_init(|| pool_of(1024)),
    }
}

/// A fetch pool of `n` slots: a twin, as the fetch pool is, so the
/// callers' shares of the tool pool do not apply.
fn pool_of(n: u32) -> ToolPool {
    ToolPool::new(Machine {
        cores: n,
        memory: None,
    })
    .twin()
}

fn none() -> Table {
    Table(Vec::new())
}

#[tokio::test]
async fn an_explicitly_listed_loopback_address_is_fetched() {
    let (port, seen) = server(Arc::new(|_| ok(b"# Example\n\nText."))).await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    let f = get(
        &format!("http://127.0.0.1:{port}/a/example-notes.md?x=1"),
        &l,
        &none(),
        limits(),
    )
    .await
    .unwrap();
    assert_eq!(&f.bytes[..], b"# Example\n\nText.");
    let s = seen.lock().unwrap();
    assert_eq!(s[0].path, "/a/example-notes.md?x=1");
    // Nothing of lmgw's goes along: no encoding asked for, no agent, no
    // cookie, no credential.
    for h in &s[0].headers {
        assert!(
            !h.starts_with("accept-encoding")
                && !h.starts_with("user-agent")
                && !h.starts_with("cookie")
                && !h.starts_with("authorization"),
            "{h}"
        );
    }
}

#[tokio::test]
async fn every_ipv4_spelling_is_the_address_it_means() {
    let (port, _) = server(Arc::new(|_| ok(b"x"))).await;
    let listed = list(&[format!("127.0.0.1:{port}")]);
    for url in [
        format!("http://2130706433:{port}/"),
        format!("http://0x7f.1:{port}/"),
        format!("http://0177.0.0.1:{port}/"),
        format!("http://127.1:{port}/"),
        format!("http://%31%32%37.0.0.1:{port}/"),
    ] {
        assert!(get(&url, &listed, &none(), limits()).await.is_ok(), "{url}");
        let other = list(&["docs.example".into()]);
        assert_eq!(
            check(&url, &other).unwrap_err(),
            FetchError::HostNotListed {
                host: "127.0.0.1".into(),
                port
            },
            "{url}"
        );
    }
}

#[test]
fn userinfo_other_schemes_and_unlisted_hosts_are_refused_before_anything_resolves() {
    let l = list(&["docs.example".into()]);
    assert_eq!(
        check("http://docs.example@evil.example/a.pdf", &l).unwrap_err(),
        FetchError::Userinfo
    );
    assert_eq!(
        check("http://user:pw@docs.example/a.pdf", &l).unwrap_err(),
        FetchError::Userinfo
    );
    for (u, s) in [
        ("file:///etc/passwd", "file"),
        ("ftp://docs.example/a.pdf", "ftp"),
        ("gopher://docs.example/", "gopher"),
        ("data:text/plain,hi", "data"),
    ] {
        assert_eq!(
            check(u, &l).unwrap_err(),
            FetchError::Scheme(s.into()),
            "{u}"
        );
    }
    assert!(matches!(
        check("https://www.docs.example/a.pdf", &l).unwrap_err(),
        FetchError::HostNotListed { .. }
    ));
    assert!(matches!(
        check("https://docs.example:8443/a.pdf", &l).unwrap_err(),
        FetchError::HostNotListed { port: 8443, .. }
    ));
    assert_eq!(check("not a url", &l).unwrap_err(), FetchError::InvalidUrl);
    // One parser, its parts only: a backslash is a path separator in an
    // http URL, so this is docs.example with an odd path, as a browser
    // reads it, and what is sent is that serialisation.
    let c = check("http://docs.example\\@evil.example/a.pdf", &l).unwrap();
    assert_eq!(c.host, Host::Name("docs.example".into()));
    assert_eq!(c.url.as_str(), "http://docs.example/@evil.example/a.pdf");
    // The listed name in other spellings is the listed name.
    for u in [
        "https://DOCS.example./a.pdf",
        "https://docs.example:443/a.pdf",
        "http://docs.example/a.pdf",
    ] {
        assert!(check(u, &l).is_ok(), "{u}");
    }
    // IDNA: the punycode entry and the Unicode URL are one name.
    let idn = list(&["bücher.example".into()]);
    assert!(check("https://xn--bcher-kva.example/", &idn).is_ok());
    assert!(check("https://BÜCHER.example/", &idn).is_ok());
}

#[tokio::test]
async fn ipv4_mapped_ipv6_is_neither_the_listed_address_nor_a_way_in() {
    let (port, seen) = server(Arc::new(|_| ok(b"x"))).await;
    let l = list(&[format!("127.0.0.1:{port}"), format!("docs.example:{port}")]);
    assert!(matches!(
        check(&format!("http://[::ffff:127.0.0.1]:{port}/"), &l).unwrap_err(),
        FetchError::HostNotListed { .. }
    ));
    // A listed name answering with a mapped private address is refused.
    let r = Table(vec![("docs.example", vec![ip("::ffff:172.16.0.1")])]);
    let e = get(&format!("http://docs.example:{port}/"), &l, &r, limits())
        .await
        .unwrap_err();
    assert!(
        matches!(e, FetchError::AddressRefused { range, never: false, .. } if range.contains("private")),
        "{e:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_listed_name_resolving_to_a_private_address_needs_that_address_listed() {
    let (port, seen) = server(Arc::new(|_| ok(b"fetched"))).await;
    let r = Table(vec![("docs.example", vec![ip("127.0.0.1")])]);
    let name_only = list(&[format!("docs.example:{port}")]);
    let url = format!("http://docs.example:{port}/example.txt");
    let e = get(&url, &name_only, &r, limits()).await.unwrap_err();
    assert!(
        matches!(&e, FetchError::AddressRefused { range, never: false, .. } if range.contains("loopback")),
        "{e:?}"
    );
    assert!(
        e.message().contains("name that address itself"),
        "{}",
        e.message()
    );
    // The address on another port is not that entry.
    let wrong_port = list(&[format!("docs.example:{port}"), "127.0.0.1:1".into()]);
    assert!(get(&url, &wrong_port, &r, limits()).await.is_err());
    assert!(seen.lock().unwrap().is_empty(), "no connection was made");
    // Listed itself: fetched, and the connection went to the checked
    // address (the table's, not any DNS).
    let both = list(&[format!("docs.example:{port}"), format!("127.0.0.1:{port}")]);
    let f = get(&url, &both, &r, limits()).await.unwrap();
    assert_eq!(&f.bytes[..], b"fetched");
    let s = seen.lock().unwrap();
    assert!(s[0]
        .headers
        .iter()
        .any(|h| h == &format!("host: docs.example:{port}")));
}

#[tokio::test]
async fn every_resolved_address_is_checked_and_some_are_never_fetched() {
    let (port, seen) = server(Arc::new(|_| ok(b"x"))).await;
    let l = list(&[
        format!("docs.example:{port}"),
        format!("127.0.0.1:{port}"),
        format!("0.0.0.0:{port}"),
    ]);
    let r = Table(vec![
        ("docs.example", vec![ip("127.0.0.1"), ip("172.16.0.1")]),
        ("zero.example", vec![ip("0.0.0.0")]),
    ]);
    let e = get(&format!("http://docs.example:{port}/"), &l, &r, limits())
        .await
        .unwrap_err();
    assert!(matches!(e, FetchError::AddressRefused { .. }), "{e:?}");
    // 0.0.0.0 is never fetched, even when listed.
    let e = get(&format!("http://0.0.0.0:{port}/"), &l, &r, limits())
        .await
        .unwrap_err();
    assert!(
        matches!(e, FetchError::AddressRefused { never: true, .. }),
        "{e:?}"
    );
    let e = get(
        &format!("http://unknown.example:{port}/"),
        &list(&[format!("unknown.example:{port}")]),
        &r,
        limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), "source_unreachable");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_redirect_to_an_unlisted_host_or_a_private_address_is_refused() {
    let (port, seen) = server(Arc::new(move |path: &str| match path {
        "/to-unlisted" => redirect("http://other.example/x"),
        "/to-mapped" => redirect("http://[::ffff:127.0.0.1]/"),
        _ => ok(b"never"),
    }))
    .await;
    // docs.example is the server; files.example resolves to the cloud
    // metadata address and is listed by name only.
    let r = Table(vec![
        ("docs.example", vec![ip("127.0.0.1")]),
        ("files.example", vec![ip("169.254.169.254")]),
    ]);
    let l = list(&[
        format!("docs.example:{port}"),
        format!("127.0.0.1:{port}"),
        "files.example".into(),
    ]);
    for path in ["/to-unlisted", "/to-mapped"] {
        let e = get(
            &format!("http://docs.example:{port}{path}"),
            &l,
            &r,
            limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), "source_redirect_refused", "{path}: {e:?}");
        assert!(
            !e.message().contains("other.example") && !e.message().contains("::ffff"),
            "the target is not named: {}",
            e.message()
        );
    }
    // A listed name whose address is link-local.
    let srv = Arc::new(move |_: &str| redirect("http://files.example/x"));
    let (p2, _) = server(srv).await;
    let l2 = list(&[
        format!("docs.example:{p2}"),
        format!("127.0.0.1:{p2}"),
        "files.example".into(),
    ]);
    let e = get(&format!("http://docs.example:{p2}/"), &l2, &r, limits())
        .await
        .unwrap_err();
    assert_eq!(e.code(), "source_redirect_refused");
    assert!(e.message().contains("link-local"), "{}", e.message());
    // Only the first hops were made.
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|s| s.path.starts_with("/to-")));
}

#[tokio::test]
async fn redirects_stop_at_the_named_bound() {
    let (port, seen) = server(Arc::new(|_| redirect("/again"))).await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    let e = get(&format!("http://127.0.0.1:{port}/"), &l, &none(), limits())
        .await
        .unwrap_err();
    assert_eq!(
        e,
        FetchError::TooManyRedirects {
            host: "127.0.0.1".into(),
            max: 3
        }
    );
    assert!(e.message().contains("SOURCE_FETCH_MAX_REDIRECTS (3)"));
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn the_callers_headers_follow_a_redirect_on_the_same_host() {
    let mut h = HeaderMap::new();
    h.insert("x-example-token", "example-secret".parse().unwrap());
    // Same host after the redirect: the headers go along.
    let (p3, seen3) = server(Arc::new(move |path: &str| match path {
        "/start" => redirect("/final"),
        _ => ok(b"done"),
    }))
    .await;
    let l3 = list(&[format!("127.0.0.1:{p3}")]);
    let c = check(&format!("http://127.0.0.1:{p3}/start"), &l3).unwrap();
    let (t, lim) = (none(), limits());
    let f = fetch(c, &h, &rules(&l3, &t, &lim), 1024).await.unwrap();
    assert_eq!(f.redirects, 1);
    let s3 = seen3.lock().unwrap();
    assert!(s3[1]
        .headers
        .iter()
        .any(|h| h == "x-example-token: example-secret"));
}

#[tokio::test]
async fn headers_are_dropped_on_a_redirect_to_another_listed_host() {
    let (port, seen) = server(Arc::new(move |path: &str| match path {
        "/start" => Reply::Raw(Vec::new()),
        _ => ok(b"done"),
    }))
    .await;
    let r = Table(vec![
        ("docs.example", vec![ip("127.0.0.1")]),
        ("files.example", vec![ip("127.0.0.1")]),
    ]);
    let (first, _) = server(Arc::new(move |_: &str| {
        redirect(&format!("http://files.example:{port}/final"))
    }))
    .await;
    let l = list(&[
        format!("docs.example:{first}"),
        format!("files.example:{port}"),
        format!("127.0.0.1:{first}"),
        format!("127.0.0.1:{port}"),
    ]);
    let c = check(&format!("http://docs.example:{first}/start"), &l).unwrap();
    let mut h = HeaderMap::new();
    h.insert("authorization", "Bearer example-secret".parse().unwrap());
    let lim = limits();
    let f = fetch(c, &h, &rules(&l, &r, &lim), 1024).await.unwrap();
    assert_eq!(&f.bytes[..], b"done");
    let s = seen.lock().unwrap();
    assert_eq!(s[0].path, "/final");
    assert!(
        !s[0].headers.iter().any(|h| h.starts_with("authorization")),
        "{:?}",
        s[0].headers
    );
}

#[tokio::test]
async fn a_body_past_the_bound_fails_with_and_without_content_length() {
    let big = vec![b'a'; 4096];
    let body = big.clone();
    let (port, _) = server(Arc::new(move |path: &str| match path {
        "/declared" => ok(&body),
        // Close-delimited: no length, read until the connection ends.
        "/undeclared" => {
            let mut r = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
            r.extend_from_slice(&body);
            Reply::Raw(r)
        }
        // Chunked: no length, the bound is on what arrives.
        _ => {
            let mut r = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n".to_vec();
            for _ in 0..8 {
                r.extend_from_slice(b"200\r\n");
                r.extend_from_slice(&[b'b'; 512]);
                r.extend_from_slice(b"\r\n");
            }
            r.extend_from_slice(b"0\r\n\r\n");
            Reply::Raw(r)
        }
    }))
    .await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    for path in ["/declared", "/undeclared", "/chunked"] {
        let e = get(
            &format!("http://127.0.0.1:{port}{path}"),
            &l,
            &none(),
            limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), "source_too_large", "{path}");
        assert!(
            e.message().contains("max_body_mb (1.0 MiB)"),
            "{}",
            e.message()
        );
    }
    // What is left of the request's bound is the bound.
    let c = check(&format!("http://127.0.0.1:{port}/declared"), &l).unwrap();
    let mut lim = limits();
    lim.max_bytes = 8192;
    let t = none();
    let e = fetch(c, &HeaderMap::new(), &rules(&l, &t, &lim), 100)
        .await
        .unwrap_err();
    assert!(matches!(e, FetchError::TooLarge { request: true, .. }));
}

#[tokio::test]
async fn a_slow_body_ends_at_the_named_timeout() {
    let (port, _) = server(Arc::new(|_| {
        Reply::Trickle(
            b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n".to_vec(),
            Duration::from_millis(50),
        )
    }))
    .await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    let mut lim = limits();
    lim.total = Duration::from_secs(1);
    let e = get(&format!("http://127.0.0.1:{port}/"), &l, &none(), lim)
        .await
        .unwrap_err();
    assert_eq!(
        e,
        FetchError::Timeout {
            host: "127.0.0.1".into(),
            secs: 1
        }
    );
    assert!(e.message().contains("SOURCE_FETCH_TIMEOUT (1 s)"));
}

#[tokio::test]
async fn an_error_status_is_named_without_what_the_host_said() {
    let (port, _) = server(Arc::new(|_| {
        Reply::Raw(
            b"HTTP/1.1 404 Not Found\r\nx-internal: example-internal-detail\r\ncontent-length: 29\r\n\r\nexample-internal-body-content"
                .to_vec(),
        )
    }))
    .await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    let e = get(&format!("http://127.0.0.1:{port}/"), &l, &none(), limits())
        .await
        .unwrap_err();
    assert_eq!(
        e.message(),
        "source_http_status: 127.0.0.1 answered HTTP 404"
    );
}

#[tokio::test]
async fn a_compressed_body_is_not_decompressed() {
    let gz: Vec<u8> = {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&vec![b'z'; 100_000]).unwrap();
        e.finish().unwrap()
    };
    let body = gz.clone();
    let (port, _) = server(Arc::new(move |_| {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(&body);
        ok_raw(r)
    }))
    .await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    let f = get(&format!("http://127.0.0.1:{port}/"), &l, &none(), limits())
        .await
        .unwrap();
    assert_eq!(&f.bytes[..], &gz[..]);
}

fn ok_raw(r: Vec<u8>) -> Reply {
    Reply::Raw(r)
}

#[test]
fn the_file_name_is_the_paths_last_segment_as_docling_takes_it() {
    assert_eq!(
        file_name("https://docs.example/a/example-report.pdf?x=1"),
        "example-report.pdf"
    );
    assert_eq!(file_name("https://docs.example/"), "file");
    assert_eq!(file_name("https://docs.example"), "file");
    assert_eq!(file_name("https://docs.example/a%20b.pdf"), "a%20b.pdf");
}
