//! The server certificates (device link design §2.2, D3): issued in memory
//! and never stored, chosen per connection by the accepted socket's local
//! address, made canonical (an IPv4-mapped address unmapped):
//!
//! - a loopback address (`127.0.0.0/8`, `::1`) → the one loopback leaf,
//!   SANs `127.0.0.1`, `::1`, `localhost` and the URL-list hosts;
//! - an address this machine's interfaces list (re-read on a miss) → that
//!   address's leaf, SANs the address and the URL-list hosts;
//! - any other address → the shared leaf, whose SANs are the URL-list hosts
//!   alone, and a log line naming the address. So no process can grow the
//!   cache by connecting to addresses of its choosing: it holds at most one
//!   leaf per interface address, the loopback one and the shared one.
//!
//! Each leaf has a fresh P-256 key, `serverAuth`, `KeyUsage:
//! digitalSignature`, the CA's key identifier, `notBefore` a day back (a
//! device whose clock runs behind still takes a fresh one) and `notAfter`
//! `devices.server_cert_days` on; it is replaced at the next connection it
//! would serve once half of that has passed. The chain sent is the leaf and
//! the CA, so a device enrolling finds the CA by `fp`. The cache empties
//! when a URL list changes or the CA rotates; a restart starts it empty. No
//! device notices: it trusts the CA, not a leaf (D1).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use lmgw_api_types::device_link as wire;
use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;

use super::ca::{random_serial, set_validity, Ca};
use super::server_config::{self, ClientAuth};

/// Which connections a leaf serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LeafSlot {
    /// Every loopback address.
    Loopback,
    /// One address an interface lists.
    Interface(IpAddr),
    /// Every address no interface lists.
    Shared,
}

impl LeafSlot {
    fn wire(&self) -> (wire::DeviceLeafSlot, Option<String>) {
        match self {
            Self::Loopback => (wire::DeviceLeafSlot::Loopback, None),
            Self::Interface(ip) => (wire::DeviceLeafSlot::Interface, Some(ip.to_string())),
            Self::Shared => (wire::DeviceLeafSlot::Shared, None),
        }
    }
}

/// A host of a device's URL list (§3.1): an IP address or a DNS name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UrlHost {
    Ip(IpAddr),
    Name(String),
}

/// Whether `name` is a host name a certificate may carry: labels of
/// letters, digits and hyphens (no hyphen at either end), 1 to 63
/// characters each, at most 253 in all. A URL parser takes more (`*`, `_`),
/// and a `*` label would be a wildcard name in every leaf.
fn is_ldh_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl UrlHost {
    /// The host of `url`; `None` when it does not parse, names no host, or
    /// names one no certificate may carry ([`is_ldh_name`]).
    pub fn of_url(url: &str) -> Option<Self> {
        let parsed = reqwest::Url::parse(url.trim()).ok()?;
        let host = parsed.host_str()?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        match bare.parse::<IpAddr>() {
            Ok(ip) => Some(Self::Ip(ip.to_canonical())),
            Err(_) => {
                let name = bare.trim_end_matches('.').to_ascii_lowercase();
                is_ldh_name(&name).then_some(Self::Name(name))
            }
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Ip(ip) => ip.to_string(),
            Self::Name(n) => n.clone(),
        }
    }

    fn san(&self) -> Option<SanType> {
        match self {
            Self::Ip(ip) => Some(SanType::IpAddress(*ip)),
            // Letters, digits, hyphens and dots ([`is_ldh_name`]): IA5 always.
            Self::Name(n) => rcgen::string::Ia5String::try_from(n.clone())
                .ok()
                .map(SanType::DnsName),
        }
    }
}

/// One server certificate with its two configurations.
pub struct Leaf {
    pub slot: LeafSlot,
    pub issued_at: DateTime<Utc>,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    /// From here on, the next connection it would serve gets a new one.
    pub renew_after: DateTime<Utc>,
    /// Its subjectAltNames, as text.
    pub names: Vec<String>,
    der: CertificateDer<'static>,
    mandatory: Arc<ServerConfig>,
    optional: Arc<ServerConfig>,
}

impl std::fmt::Debug for Leaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leaf")
            .field("slot", &self.slot)
            .field("issued_at", &self.issued_at)
            .field("not_after", &self.not_after)
            .field("names", &self.names)
            .field("key", &"[redacted]")
            .finish()
    }
}

impl Leaf {
    /// The configuration a handshake runs under (§4.2).
    pub fn config(&self, auth: ClientAuth) -> Arc<ServerConfig> {
        match auth {
            ClientAuth::Mandatory => self.mandatory.clone(),
            ClientAuth::Optional => self.optional.clone(),
        }
    }

    /// The certificate, DER.
    pub fn certificate(&self) -> &CertificateDer<'static> {
        &self.der
    }

    /// The status block's line for it.
    pub fn view(&self) -> wire::DeviceLeafStatus {
        let (slot, address) = self.slot.wire();
        wire::DeviceLeafStatus {
            slot,
            address,
            issued: rfc3339(self.issued_at),
            valid_until: rfc3339(self.not_after),
            renew_after: rfc3339(self.renew_after),
            names: self.names.clone(),
        }
    }
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

type Interfaces = Box<dyn Fn() -> Vec<IpAddr> + Send + Sync>;

/// This machine's interface addresses, as `if_addrs` lists them.
fn system_interfaces() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|v| v.iter().map(|i| i.ip().to_canonical()).collect())
        .unwrap_or_else(|e| {
            tracing::warn!("listing the interface addresses failed: {e}");
            Vec::new()
        })
}

#[derive(Default)]
struct Inner {
    /// The CA the leaves are from: another one empties the cache.
    ca_fp: Option<String>,
    /// The URL-list hosts every leaf carries.
    hosts: Vec<UrlHost>,
    /// The interface addresses as last read.
    known: Vec<IpAddr>,
    leaves: HashMap<LeafSlot, Arc<Leaf>>,
}

/// The leaves, by slot.
pub struct LeafCache {
    inner: Mutex<Inner>,
    interfaces: Interfaces,
}

impl Default for LeafCache {
    fn default() -> Self {
        Self::with_interfaces(system_interfaces)
    }
}

impl std::fmt::Debug for LeafCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("LeafCache")
            .field("hosts", &inner.hosts)
            .field("leaves", &inner.leaves.len())
            .finish()
    }
}

impl LeafCache {
    /// A cache that reads the interface addresses from `list`: the system's
    /// in the gateway, a fixed list in a test.
    pub fn with_interfaces(list: impl Fn() -> Vec<IpAddr> + Send + Sync + 'static) -> Self {
        Self {
            inner: Mutex::default(),
            interfaces: Box::new(list),
        }
    }

    /// The slot a connection to local address `local` takes. The interface
    /// list is read again when `local` is not in it.
    pub fn slot_for(&self, local: IpAddr) -> LeafSlot {
        let local = local.to_canonical();
        if local.is_loopback() {
            return LeafSlot::Loopback;
        }
        if self.inner.lock().unwrap().known.contains(&local) {
            return LeafSlot::Interface(local);
        }
        let fresh = (self.interfaces)();
        let listed = fresh.contains(&local);
        let mut inner = self.inner.lock().unwrap();
        // A leaf for an address no interface lists any more goes with it.
        inner
            .leaves
            .retain(|slot, _| !matches!(slot, LeafSlot::Interface(ip) if !fresh.contains(ip)));
        inner.known = fresh;
        drop(inner);
        if listed {
            LeafSlot::Interface(local)
        } else {
            tracing::info!(
                "a device connection arrived on {local}, which no interface lists; it gets the \
                 shared server certificate (the URL-list hosts only)"
            );
            LeafSlot::Shared
        }
    }

    /// The leaf for a connection to `local`, issued from `ca` with a
    /// validity of `days` when there is none or the one there is past half
    /// its validity at `now`.
    pub fn leaf(
        &self,
        ca: &Ca,
        local: IpAddr,
        days: u32,
        now: DateTime<Utc>,
    ) -> Result<Arc<Leaf>, String> {
        let slot = self.slot_for(local);
        let mut inner = self.inner.lock().unwrap();
        if inner.ca_fp.as_deref() != Some(ca.fp()) {
            inner.leaves.clear();
            inner.ca_fp = Some(ca.fp().to_string());
        }
        if let Some(leaf) = inner.leaves.get(&slot) {
            if now < leaf.renew_after {
                return Ok(leaf.clone());
            }
        }
        let leaf = Arc::new(issue(ca, slot, &inner.hosts, days, now)?);
        inner.leaves.insert(slot, leaf.clone());
        Ok(leaf)
    }

    /// Take the hosts of `urls` (every device row's URL list) into every
    /// leaf from now on; the cache empties when they changed. Whether they
    /// did.
    pub fn set_url_hosts<'a>(&self, urls: impl IntoIterator<Item = &'a str>) -> bool {
        let mut hosts: Vec<UrlHost> = urls
            .into_iter()
            .filter_map(|u| {
                let h = UrlHost::of_url(u);
                if h.is_none() {
                    tracing::warn!(
                        "a device URL names no host a certificate may carry (an IP address, or \
                         letters, digits and hyphens): '{u}'; no server certificate carries it"
                    );
                }
                h
            })
            .collect();
        hosts.sort();
        hosts.dedup();
        let mut inner = self.inner.lock().unwrap();
        if inner.hosts == hosts {
            return false;
        }
        inner.hosts = hosts;
        inner.leaves.clear();
        true
    }

    /// Empty the cache: the next connection of each slot gets a new leaf.
    pub fn clear(&self) {
        self.inner.lock().unwrap().leaves.clear();
    }

    /// The leaves there are now, loopback first, then by address.
    pub fn leaves(&self) -> Vec<Arc<Leaf>> {
        let mut v: Vec<Arc<Leaf>> = self
            .inner
            .lock()
            .unwrap()
            .leaves
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|l| l.slot);
        v
    }
}

/// A new leaf for `slot` from `ca`, carrying `hosts`.
fn issue(
    ca: &Ca,
    slot: LeafSlot,
    hosts: &[UrlHost],
    days: u32,
    now: DateTime<Utc>,
) -> Result<Leaf, String> {
    let own: Vec<UrlHost> = match slot {
        LeafSlot::Loopback => vec![
            UrlHost::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            UrlHost::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            UrlHost::Name("localhost".into()),
        ],
        LeafSlot::Interface(ip) => vec![UrlHost::Ip(ip)],
        LeafSlot::Shared => Vec::new(),
    };
    let mut all: Vec<UrlHost> = Vec::new();
    for h in own.into_iter().chain(hosts.iter().cloned()) {
        if !all.contains(&h) {
            all.push(h);
        }
    }
    let sans: Vec<SanType> = all.iter().filter_map(UrlHost::san).collect();
    let names: Vec<String> = all
        .iter()
        .filter(|h| h.san().is_some())
        .map(UrlHost::text)
        .collect();

    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| format!("generating a server key: {e}"))?;
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "lmgw device listener");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "lmgw");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.use_authority_key_identifier_extension = true;
    params.serial_number = Some(random_serial()?.0);
    let issued_at = now;
    let not_before = now - chrono::Duration::days(1);
    let validity = chrono::Duration::days(i64::from(days.max(1)));
    let not_after = now + validity;
    set_validity(&mut params, not_before, not_after);
    let cert = params
        .signed_by(&key, ca.issuer())
        .map_err(|e| format!("signing a server certificate: {e}"))?;

    let chain = vec![cert.der().clone(), ca.certificate().clone()];
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let mandatory = server_config::build(&chain, &key_der, ca.verifiers(), ClientAuth::Mandatory)?;
    let optional = server_config::build(&chain, &key_der, ca.verifiers(), ClientAuth::Optional)?;
    Ok(Leaf {
        slot,
        issued_at,
        not_before: truncate_secs(not_before),
        not_after: truncate_secs(not_after),
        renew_after: issued_at + validity / 2,
        names,
        der: cert.der().clone(),
        mandatory,
        optional,
    })
}

/// `t` to the second, as the certificate carries it.
fn truncate_secs(t: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(t.timestamp(), 0).unwrap_or(t)
}
