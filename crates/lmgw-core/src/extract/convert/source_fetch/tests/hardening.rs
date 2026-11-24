//! The security review round's cases (WP5b): the caller's headers by
//! origin, the pin against a name that answers differently later, the
//! hop's client resolving nothing itself, the fetch pool's bound, odd
//! `Location` values, `https` → `http`, and this machine's own addresses.

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use futures::future::BoxFuture;
use reqwest::header::HeaderValue;

use super::*;

fn bearer() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("authorization", "Bearer example-secret".parse().unwrap());
    h
}

fn has_auth(s: &Seen) -> bool {
    s.headers.iter().any(|h| h.starts_with("authorization"))
}

#[tokio::test]
async fn headers_never_go_to_another_port_on_the_same_host() {
    let (other, seen_other) = server(Arc::new(|_| ok(b"other service"))).await;
    let (first, seen_first) = server(Arc::new(move |_: &str| {
        redirect(&format!("http://127.0.0.1:{other}/final"))
    }))
    .await;
    let l = list(&[format!("127.0.0.1:{first}"), format!("127.0.0.1:{other}")]);
    let c = check(&format!("http://127.0.0.1:{first}/start"), &l).unwrap();
    let (t, lim) = (none(), limits());
    let f = fetch(c, &bearer(), &rules(&l, &t, &lim), 1024)
        .await
        .unwrap();
    assert_eq!(&f.bytes[..], b"other service");
    assert!(has_auth(&seen_first.lock().unwrap()[0]));
    let s = seen_other.lock().unwrap();
    assert!(!has_auth(&s[0]), "{:?}", s[0].headers);
}

#[tokio::test]
async fn headers_never_come_back_once_the_walk_left_the_origin() {
    // A: /start goes to B; B sends the walk back to A at a path B chose.
    let b_port = Arc::new(AtomicU16::new(0));
    let bp = b_port.clone();
    let (a, seen_a) = server(Arc::new(move |path: &str| match path {
        "/start" => redirect(&format!(
            "http://127.0.0.1:{}/elsewhere",
            bp.load(Ordering::SeqCst)
        )),
        _ => ok(b"steered"),
    }))
    .await;
    let (b, _) = server(Arc::new(move |_: &str| {
        redirect(&format!("http://127.0.0.1:{a}/chosen-by-b"))
    }))
    .await;
    b_port.store(b, Ordering::SeqCst);
    let l = list(&[format!("127.0.0.1:{a}"), format!("127.0.0.1:{b}")]);
    let c = check(&format!("http://127.0.0.1:{a}/start"), &l).unwrap();
    let (t, lim) = (none(), limits());
    let f = fetch(c, &bearer(), &rules(&l, &t, &lim), 1024)
        .await
        .unwrap();
    assert_eq!(f.redirects, 2);
    let s = seen_a.lock().unwrap();
    assert_eq!(s[0].path, "/start");
    assert!(has_auth(&s[0]));
    assert_eq!(s[1].path, "/chosen-by-b");
    assert!(!has_auth(&s[1]), "{:?}", s[1].headers);
}

/// A name that answers the server's address first and another address on
/// every later lookup.
struct Rebinding {
    calls: AtomicUsize,
}

impl resolve::Resolve for Rebinding {
    fn lookup(&self, _: &str, _: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let a = if n == 0 {
            ip("127.0.0.1")
        } else {
            ip("198.51.100.1")
        };
        Box::pin(async move { Ok(vec![a]) })
    }
}

#[tokio::test]
async fn the_pin_holds_when_the_name_answers_differently_later() {
    let (port, seen) = server(Arc::new(|path: &str| match path {
        "/again" => redirect("/final"),
        _ => ok(b"pinned"),
    }))
    .await;
    let l = list(&[format!("docs.example:{port}"), format!("127.0.0.1:{port}")]);
    // One hop: one lookup, and the connection goes to its answer (a second
    // lookup would have answered an address no server is on).
    let r = Rebinding {
        calls: AtomicUsize::new(0),
    };
    let f = get(&format!("http://docs.example:{port}/x"), &l, &r, limits())
        .await
        .unwrap();
    assert_eq!(&f.bytes[..], b"pinned");
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    // A redirect on the same name is looked up and checked again: the new
    // answer is a refused range, so the second hop is not made.
    let r = Rebinding {
        calls: AtomicUsize::new(0),
    };
    let e = get(
        &format!("http://docs.example:{port}/again"),
        &l,
        &r,
        limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), "source_redirect_refused");
    assert!(e.message().contains("documentation"), "{}", e.message());
    assert_eq!(r.calls.load(Ordering::SeqCst), 2);
    assert_eq!(seen.lock().unwrap().len(), 2, "/x, then /again only");
}

#[tokio::test]
async fn the_hops_client_looks_up_no_name_itself() {
    let (port, seen) = server(Arc::new(|_| ok(b"never"))).await;
    // A hop whose name was not pinned: the system would resolve
    // `localhost` to this server; the hop's own resolver answers nothing.
    let at = Checked {
        url: url::Url::parse(&format!("http://localhost:{port}/")).unwrap(),
        host: Host::Ip(ip("127.0.0.1")),
        port,
    };
    let e = crate::extract::convert::source_fetch::get::send(
        &at,
        &[ip("127.0.0.1")],
        None,
        &limits(),
        "localhost",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), "source_unreachable");
    assert!(seen.lock().unwrap().is_empty());
}

/// A server that holds each body back for `hold`, counting the
/// connections open at once and the most at once.
async fn slow_counting(hold: Duration) -> (u16, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let now = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let most_out = most.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            let (now, most) = (now.clone(), most.clone());
            tokio::spawn(async move {
                let open = now.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(open, Ordering::SeqCst);
                let mut b = [0u8; 1024];
                let _ = sock.read(&mut b).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n")
                    .await;
                tokio::time::sleep(hold).await;
                // Counted out before the body ends, so the client cannot
                // finish (and free its slot) before this connection is.
                now.fetch_sub(1, Ordering::SeqCst);
                let _ = sock.write_all(b"done").await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (port, most_out)
}

#[tokio::test]
async fn n_plus_one_fetches_never_run_more_than_n_at_once_and_the_wait_is_not_their_time() {
    let hold = Duration::from_millis(700);
    let (port, most) = slow_counting(hold).await;
    let l = list(&[format!("127.0.0.1:{port}")]);
    let t = none();
    let mut lim = limits();
    // Shorter than a wait plus a fetch: the third fetch only succeeds if
    // its wait for a slot is not counted.
    lim.total = Duration::from_millis(1200);
    let local = Local::none();
    let slots = pool_of(2);
    let r = Rules {
        list: &l,
        resolver: &t,
        local: &local,
        limits: &lim,
        slots: &slots,
    };
    let url = format!("http://127.0.0.1:{port}/slow");
    let one = || async {
        let c = check(&url, &l).unwrap();
        fetch(c, &HeaderMap::new(), &r, 1024).await
    };
    let (a, b, c) = tokio::join!(one(), one(), one());
    for got in [a, b, c] {
        assert_eq!(&got.unwrap().bytes[..], b"done");
    }
    assert_eq!(most.load(Ordering::SeqCst), 2);
    assert_eq!(slots.facts().running, 0);
}

#[tokio::test]
async fn empty_scheme_relative_and_userinfo_locations_are_refused() {
    let (port, seen) = server(Arc::new(move |path: &str| match path {
        "/empty" => Reply::Raw(
            b"HTTP/1.1 302 Found\r\nlocation: \r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_vec(),
        ),
        "/none" => Reply::Raw(
            b"HTTP/1.1 302 Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
        ),
        "/scheme-relative" => redirect("//other.example/x"),
        "/userinfo" => redirect(&format!("http://user:pw@127.0.0.1:{}/x", 1)),
        _ => ok(b"never"),
    }))
    .await;
    let l = list(&[format!("127.0.0.1:{port}"), "127.0.0.1:1".into()]);
    for (path, why) in [
        ("/empty", "names no place to go"),
        ("/none", "names no place to go"),
        ("/scheme-relative", "not on the owner's allowed sites"),
        ("/userinfo", "user name or password"),
    ] {
        let e = get(
            &format!("http://127.0.0.1:{port}{path}"),
            &l,
            &none(),
            limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), "source_redirect_refused", "{path}");
        let m = e.message();
        assert!(m.contains(why), "{path}: {m}");
        assert!(
            !m.contains("other.example") && !m.contains("pw"),
            "{path}: {m}"
        );
    }
    // Each was one request: nothing was followed.
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn https_to_http_is_refused_and_a_scheme_relative_target_keeps_https() {
    let from = url::Url::parse("https://docs.example/a/b.pdf").unwrap();
    let v = |s: &'static str| HeaderValue::from_static(s);
    assert_eq!(
        crate::extract::convert::source_fetch::get::next_url(
            &from,
            Some(&v("http://docs.example/c.pdf"))
        )
        .unwrap_err(),
        "it goes from https to plain http"
    );
    assert_eq!(
        crate::extract::convert::source_fetch::get::next_url(
            &from,
            Some(&v("//docs.example/c.pdf"))
        )
        .unwrap()
        .as_str(),
        "https://docs.example/c.pdf"
    );
    assert_eq!(
        crate::extract::convert::source_fetch::get::next_url(&from, Some(&v("c.pdf")))
            .unwrap()
            .as_str(),
        "https://docs.example/a/c.pdf"
    );
    let plain = url::Url::parse("http://docs.example/").unwrap();
    assert_eq!(
        crate::extract::convert::source_fetch::get::next_url(
            &plain,
            Some(&v("https://docs.example/x"))
        )
        .unwrap()
        .scheme(),
        "https"
    );
}

#[tokio::test]
async fn this_machines_own_addresses_are_refused_unless_listed() {
    // A global IPv6 network on an interface: a listed name answering a
    // neighbour on it is the local network, not the internet.
    let local = Local::from_parts(vec![(ip("2606:4700:20::7"), 64)], Vec::new());
    let l = list(&["docs.example".into()]);
    let t = Table(vec![("docs.example", vec![ip("2606:4700:20::99")])]);
    let lim = limits();
    let slots = pool_of(1);
    let r = Rules {
        list: &l,
        resolver: &t,
        local: &local,
        limits: &lim,
        slots: &slots,
    };
    let c = check("http://docs.example/x", &l).unwrap();
    let e = fetch(c, &HeaderMap::new(), &r, 1024).await.unwrap_err();
    assert!(
        matches!(&e, FetchError::AddressRefused { range, never: false, .. } if range.contains("local network")),
        "{e:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_interfaces_are_read_on_this_machine() {
    // Loopback is always an interface here.
    let l = Local::read(Vec::new());
    assert!(l.class(ip("127.0.0.1")).is_some());
}
