//! The server certificates (§2.2, D3): one loopback leaf, a leaf per
//! interface address, the shared leaf for anything else, every URL-list host
//! in each, and the re-issue from half the validity. The interface rule runs
//! over an injected address list.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use lmgw_core::devices::tls::{Ca, LeafCache, LeafSlot, UrlHost};
use x509_parser::extensions::GeneralName;

use super::plain_ca;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// The subjectAltNames of a certificate, as text.
fn sans(der: &[u8]) -> Vec<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).unwrap();
    let Some(ext) = cert.subject_alternative_name().unwrap() else {
        return Vec::new();
    };
    ext.value
        .general_names
        .iter()
        .map(|n| match n {
            GeneralName::DNSName(d) => d.to_string(),
            GeneralName::IPAddress(b) => match b.len() {
                4 => IpAddr::from(<[u8; 4]>::try_from(*b).unwrap()).to_string(),
                16 => IpAddr::from(<[u8; 16]>::try_from(*b).unwrap()).to_string(),
                _ => panic!("odd IP SAN"),
            },
            other => panic!("unexpected SAN {other:?}"),
        })
        .collect()
}

/// A cache over a list of interface addresses a test can change, and how
/// many times the list was read.
fn cache(list: &[&str]) -> (LeafCache, Arc<Mutex<Vec<IpAddr>>>, Arc<AtomicUsize>) {
    let addrs = Arc::new(Mutex::new(list.iter().map(|s| ip(s)).collect::<Vec<_>>()));
    let reads = Arc::new(AtomicUsize::new(0));
    let (a, r) = (addrs.clone(), reads.clone());
    let c = LeafCache::with_interfaces(move || {
        r.fetch_add(1, Ordering::SeqCst);
        a.lock().unwrap().clone()
    });
    (c, addrs, reads)
}

async fn ca() -> (tempfile::TempDir, Arc<Ca>) {
    let dir = tempfile::tempdir().unwrap();
    let (_, ca) = plain_ca(dir.path()).await;
    (dir, ca)
}

#[tokio::test]
async fn every_loopback_address_shares_one_leaf() {
    let (_d, ca) = ca().await;
    let (c, _, reads) = cache(&[]);
    let now = Utc::now();
    let first = c.leaf(&ca, ip("127.0.0.1"), 30, now).unwrap();
    for other in ["127.0.0.2", "127.255.0.9", "::1", "::ffff:127.0.0.1"] {
        let l = c.leaf(&ca, ip(other), 30, now).unwrap();
        assert!(Arc::ptr_eq(&l, &first), "{other}");
    }
    assert_eq!(first.slot, LeafSlot::Loopback);
    assert_eq!(c.leaves().len(), 1, "no entry per loopback address");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "loopback needs no interface list"
    );
    assert_eq!(first.names, ["127.0.0.1", "::1", "localhost"]);
    assert_eq!(sans(first.certificate()), ["127.0.0.1", "::1", "localhost"]);
}

#[tokio::test]
async fn an_interface_address_gets_its_own_leaf_and_any_other_the_shared_one() {
    let (_d, ca) = ca().await;
    let (c, addrs, reads) = cache(&["203.0.113.5", "fe80::1"]);
    let now = Utc::now();

    let lan = c.leaf(&ca, ip("203.0.113.5"), 30, now).unwrap();
    assert_eq!(lan.slot, LeafSlot::Interface(ip("203.0.113.5")));
    assert_eq!(sans(lan.certificate()), ["203.0.113.5"]);
    // An IPv4-mapped address is the address.
    let mapped = c.leaf(&ca, ip("::ffff:203.0.113.5"), 30, now).unwrap();
    assert!(Arc::ptr_eq(&mapped, &lan));
    let link = c.leaf(&ca, ip("fe80::1"), 30, now).unwrap();
    assert_eq!(sans(link.certificate()), ["fe80::1"]);

    // Addresses no interface lists share one leaf, which carries only the
    // URL-list hosts (none yet); the list is read again on each miss.
    let before = reads.load(Ordering::SeqCst);
    let x = c.leaf(&ca, ip("192.0.2.9"), 30, now).unwrap();
    let y = c.leaf(&ca, ip("192.0.2.10"), 30, now).unwrap();
    assert!(Arc::ptr_eq(&x, &y));
    assert_eq!(x.slot, LeafSlot::Shared);
    assert!(sans(x.certificate()).is_empty());
    assert_eq!(reads.load(Ordering::SeqCst), before + 2);
    assert_eq!(
        c.leaves().len(),
        3,
        "203.0.113.5, fe80::1 and the shared one"
    );

    // A known address is not read again; an interface that comes up later
    // is found on its first miss.
    let known = reads.load(Ordering::SeqCst);
    c.leaf(&ca, ip("203.0.113.5"), 30, now).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), known);
    addrs.lock().unwrap().push(ip("192.0.2.1"));
    let wg = c.leaf(&ca, ip("192.0.2.1"), 30, now).unwrap();
    assert_eq!(wg.slot, LeafSlot::Interface(ip("192.0.2.1")));
    assert_eq!(c.leaves().len(), 4);
}

#[tokio::test]
async fn every_url_list_host_is_in_every_leaf() {
    let (_d, ca) = ca().await;
    let (c, _, _) = cache(&["203.0.113.5"]);
    let now = Utc::now();
    let old = c.leaf(&ca, ip("127.0.0.1"), 30, now).unwrap();

    let urls = [
        "https://192.0.2.1:8788",
        "https://Phone.EXAMPLE:8788/prefix",
        "https://[2001:db8::1]:8788",
        "https://192.0.2.1:9999",
    ];
    assert!(c.set_url_hosts(urls));
    assert!(!c.set_url_hosts(urls), "the same hosts change nothing");
    assert!(c.leaves().is_empty(), "a changed list empties the cache");

    let hosts = ["192.0.2.1", "2001:db8::1", "phone.example"];
    let lo = c.leaf(&ca, ip("127.0.0.1"), 30, now).unwrap();
    assert_ne!(lo.certificate(), old.certificate());
    let want: Vec<String> = ["127.0.0.1", "::1", "localhost"]
        .into_iter()
        .chain(hosts)
        .map(String::from)
        .collect();
    assert_eq!(sans(lo.certificate()), want);
    assert_eq!(lo.names, want);
    let lan = c.leaf(&ca, ip("203.0.113.5"), 30, now).unwrap();
    assert_eq!(
        sans(lan.certificate()),
        ["203.0.113.5", "192.0.2.1", "2001:db8::1", "phone.example"]
    );
    let shared = c.leaf(&ca, ip("192.0.2.1"), 30, now).unwrap();
    assert_eq!(sans(shared.certificate()), hosts);
    // A host that is also the leaf's own address is carried once.
    assert!(c.set_url_hosts(["https://203.0.113.5:8788"]));
    let lan = c.leaf(&ca, ip("203.0.113.5"), 30, now).unwrap();
    assert_eq!(sans(lan.certificate()), ["203.0.113.5"]);
}

#[tokio::test]
async fn a_leaf_is_reissued_from_half_its_validity() {
    let (_d, ca) = ca().await;
    let (c, _, _) = cache(&[]);
    let t0 = Utc::now();
    let a = c.leaf(&ca, ip("127.0.0.1"), 30, t0).unwrap();
    let a2 = c
        .leaf(
            &ca,
            ip("127.0.0.1"),
            30,
            t0 + Duration::days(15) - Duration::seconds(1),
        )
        .unwrap();
    assert!(Arc::ptr_eq(&a, &a2));
    let b = c
        .leaf(&ca, ip("127.0.0.1"), 30, t0 + Duration::days(15))
        .unwrap();
    assert!(!Arc::ptr_eq(&a, &b));
    let key = |l: &lmgw_core::devices::tls::Leaf| {
        let (_, cert) = x509_parser::parse_x509_certificate(l.certificate()).unwrap();
        cert.public_key().raw.to_vec()
    };
    assert_ne!(key(&a), key(&b), "a fresh key each time");
    assert_eq!(c.leaves().len(), 1);

    // The certificate as §2.2 states it.
    let (_, cert) = x509_parser::parse_x509_certificate(a.certificate()).unwrap();
    assert_eq!(
        cert.validity().not_before.timestamp(),
        (t0 - Duration::days(1)).timestamp()
    );
    assert_eq!(
        cert.validity().not_after.timestamp(),
        (t0 + Duration::days(30)).timestamp()
    );
    assert_eq!(
        a.not_after.timestamp(),
        (t0 + Duration::days(30)).timestamp()
    );
    assert_eq!(a.renew_after, t0 + Duration::days(15));
    let eku = cert.extended_key_usage().unwrap().unwrap().value;
    assert!(eku.server_auth && !eku.client_auth && !eku.any && eku.other.is_empty());
    let ku = cert.key_usage().unwrap().unwrap().value;
    assert!(ku.digital_signature() && !ku.key_cert_sign());
    let (_, ca_cert) = x509_parser::parse_x509_certificate(ca.certificate()).unwrap();
    assert_eq!(cert.issuer(), ca_cert.subject());
    cert.verify_signature(Some(ca_cert.public_key()))
        .expect("the CA signed it");
    let aki = cert
        .iter_extensions()
        .find_map(|e| match e.parsed_extension() {
            x509_parser::extensions::ParsedExtension::AuthorityKeyIdentifier(a) => {
                a.key_identifier.as_ref().map(|k| k.0.to_vec())
            }
            _ => None,
        })
        .expect("an authority key identifier");
    let ski = ca_cert
        .iter_extensions()
        .find_map(|e| match e.parsed_extension() {
            x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
            _ => None,
        })
        .expect("the CA's key identifier");
    assert_eq!(aki, ski);

    // A changed validity applies from the next issue on.
    let b2 = c
        .leaf(&ca, ip("127.0.0.1"), 2, t0 + Duration::days(16))
        .unwrap();
    assert!(Arc::ptr_eq(&b, &b2));
    c.clear();
    let short = c
        .leaf(&ca, ip("127.0.0.1"), 2, t0 + Duration::days(16))
        .unwrap();
    assert_eq!(
        short.not_after.timestamp(),
        (t0 + Duration::days(18)).timestamp()
    );
    assert_eq!(
        c.leaves()[0].view().slot,
        lmgw_api_types::device_link::DeviceLeafSlot::Loopback
    );
}

#[tokio::test]
async fn another_ca_empties_the_cache() {
    let (_d1, ca1) = ca().await;
    let (_d2, ca2) = ca().await;
    let (c, _, _) = cache(&[]);
    let now = Utc::now();
    let one = c.leaf(&ca1, ip("::1"), 30, now).unwrap();
    let two = c.leaf(&ca2, ip("::1"), 30, now).unwrap();
    assert!(!Arc::ptr_eq(&one, &two));
    let (_, cert) = x509_parser::parse_x509_certificate(two.certificate()).unwrap();
    let (_, ca2_cert) = x509_parser::parse_x509_certificate(ca2.certificate()).unwrap();
    assert_eq!(cert.issuer(), ca2_cert.subject());
    assert_eq!(c.leaves().len(), 1);
}

#[tokio::test]
async fn only_ip_addresses_and_plain_host_names_become_names() {
    let (_d, ca) = ca().await;
    let (c, _, _) = cache(&[]);
    // A URL parser takes `*` and `_` in a host; a leaf takes neither, so no
    // URL can make a wildcard name.
    for bad in [
        "https://*.example:8788",
        "https://a_b.example:8788",
        "https://-x.example:8788",
    ] {
        assert_eq!(UrlHost::of_url(bad), None, "{bad}");
    }
    c.set_url_hosts([
        "https://*.example:8788",
        "https://a_b.example:8788",
        "https://ok-1.example.:8788",
        "https://192.0.2.1:8788",
    ]);
    let lo = c.leaf(&ca, ip("127.0.0.1"), 30, Utc::now()).unwrap();
    assert_eq!(
        sans(lo.certificate()),
        ["127.0.0.1", "::1", "localhost", "192.0.2.1", "ok-1.example"]
    );
}

#[tokio::test]
async fn a_leaf_for_an_address_that_went_away_goes_at_the_next_reread() {
    let (_d, ca) = ca().await;
    let (c, addrs, _) = cache(&["203.0.113.5", "203.0.113.6"]);
    let now = Utc::now();
    c.leaf(&ca, ip("203.0.113.5"), 30, now).unwrap();
    c.leaf(&ca, ip("203.0.113.6"), 30, now).unwrap();
    assert_eq!(c.leaves().len(), 2);
    addrs.lock().unwrap().retain(|a| *a != ip("203.0.113.6"));
    // A miss reads the list again, and 203.0.113.6's leaf goes with it.
    c.leaf(&ca, ip("192.0.2.1"), 30, now).unwrap();
    let slots: Vec<LeafSlot> = c.leaves().iter().map(|l| l.slot).collect();
    assert_eq!(
        slots,
        [LeafSlot::Interface(ip("203.0.113.5")), LeafSlot::Shared]
    );
}
