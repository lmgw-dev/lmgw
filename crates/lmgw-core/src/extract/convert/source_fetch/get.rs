//! One source's fetch: a slot in the fetch pool, then the hops, each
//! resolved, checked and pinned, and the body read under its bound (the
//! rules are the parent module's).

use std::net::{IpAddr, SocketAddr};

use bytes::{Bytes, BytesMut};
use reqwest::header::{HeaderMap, HeaderValue, LOCATION};
use reqwest::StatusCode;

use super::ranges::{self, Class};
use super::{check_url, Checked, FetchError, Host, Limits, Rules};

/// A fetched source.
#[derive(Debug)]
pub struct Fetched {
    pub bytes: Bytes,
    /// Redirects followed.
    pub redirects: u32,
}

/// Fetch `first` (checked with [`super::check`]) by `rules`, with the
/// caller's `headers` for its own origin, at most `left` bytes (what the
/// request's sources may still download, at most `rules.limits.max_bytes`).
///
/// It waits for a slot in `rules.slots` first (round robin over the
/// callers, [`super::slots`]); the wait ends when the request goes away and
/// is not counted against `SOURCE_FETCH_TIMEOUT`, which starts with the slot.
pub async fn fetch(
    first: Checked,
    headers: &HeaderMap,
    rules: &Rules<'_>,
    left: u64,
) -> Result<Fetched, FetchError> {
    let host = first.host.to_string();
    let _slot = rules.slots.acquire().await;
    let walk = Box::pin(walk(first, headers, rules, left));
    match tokio::time::timeout(rules.limits.total, walk).await {
        Ok(r) => r,
        Err(_) => Err(FetchError::Timeout {
            host,
            secs: rules.limits.total.as_secs(),
        }),
    }
}

/// A hop's origin: scheme, host and port, as browsers and reqwest compare
/// it before sending credentials on.
fn origin(c: &Checked) -> (&str, &Host, u16) {
    (c.url.scheme(), &c.host, c.port)
}

async fn walk(
    first: Checked,
    headers: &HeaderMap,
    rules: &Rules<'_>,
    left: u64,
) -> Result<Fetched, FetchError> {
    let limits = rules.limits;
    let named = first.host.to_string();
    let first_origin = {
        let (s, h, p) = origin(&first);
        (s.to_string(), h.clone(), p)
    };
    let mut at = first;
    let mut redirects = 0u32;
    // Set at the first hop to another origin, and never cleared: a walk
    // that comes back to the source's origin after a hop elsewhere was
    // steered there by the other site, so the caller's headers stay off.
    let mut left_origin = false;
    loop {
        let addrs = match pin(&at, rules).await {
            Ok(a) => a,
            Err(e) if redirects == 0 => return Err(e),
            Err(e) => {
                tracing::info!(
                    host = %named,
                    to = %at.host,
                    "docling source: redirect refused ({})",
                    e.code()
                );
                return Err(FetchError::RedirectRefused {
                    host: named,
                    why: redirect_why(&e),
                });
            }
        };
        let (s, h, p) = origin(&at);
        if (s, h, p) != (first_origin.0.as_str(), &first_origin.1, first_origin.2) {
            left_origin = true;
        }
        // The caller's headers go to the source's own origin only.
        let send_headers = (!left_origin).then_some(headers);
        let resp = send(&at, &addrs, send_headers, limits, &named).await?;
        let status = resp.status();
        if is_redirect(status) {
            if redirects >= limits.max_redirects {
                return Err(FetchError::TooManyRedirects {
                    host: named,
                    max: limits.max_redirects,
                });
            }
            let next = next_url(&at.url, resp.headers().get(LOCATION)).map_err(|why| {
                FetchError::RedirectRefused {
                    host: named.clone(),
                    why: why.to_string(),
                }
            })?;
            drop(resp);
            at = match check_url(next, rules.list) {
                Ok(c) => c,
                Err(e) => {
                    tracing::info!(host = %named, "docling source: redirect refused ({})", e.code());
                    return Err(FetchError::RedirectRefused {
                        host: named,
                        why: redirect_why(&e),
                    });
                }
            };
            redirects += 1;
            continue;
        }
        if !status.is_success() {
            return Err(FetchError::Status {
                host: named,
                status: status.as_u16(),
            });
        }
        let bytes = read(resp, limits, left, &named).await?;
        return Ok(Fetched { bytes, redirects });
    }
}

/// Where a redirect from `from` with `location` goes, or why it is
/// refused, in words without its target. An empty `Location` is refused (it
/// would name the same address again); a relative or scheme-relative one is
/// read against `from`, as browsers read it, and then checked like any
/// address; `https` → `http` is refused.
pub(super) fn next_url(
    from: &url::Url,
    location: Option<&HeaderValue>,
) -> Result<url::Url, &'static str> {
    let text = location
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("it names no place to go")?;
    let next = from
        .join(text)
        .map_err(|_| "its target is not a web address")?;
    if from.scheme() == "https" && next.scheme() == "http" {
        return Err("it goes from https to plain http");
    }
    Ok(next)
}

/// A redirect's refusal in words, without its target.
fn redirect_why(e: &FetchError) -> String {
    match e {
        FetchError::Scheme(_) => "its target's scheme is not http or https".to_string(),
        FetchError::Userinfo => "its target has a user name or password in it".to_string(),
        FetchError::HostNotListed { .. } => {
            "its target is not on the owner's allowed sites".to_string()
        }
        FetchError::AddressRefused { range, .. } => format!(
            "its target resolves to an address in {range} that the allowed sites do not name"
        ),
        FetchError::NotResolved { .. } => "its target's name did not resolve".to_string(),
        _ => "its target is not a web address lmgw can read".to_string(),
    }
}

fn is_redirect(s: StatusCode) -> bool {
    matches!(s.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// The hop's addresses: resolved once, each one checked, the fixed ranges
/// first and then this machine's own addresses ([`super::local`]).
async fn pin(at: &Checked, rules: &Rules<'_>) -> Result<Vec<IpAddr>, FetchError> {
    let host = at.host.to_string();
    let list = rules.list;
    let scheme = at.url.scheme();
    let addrs = match &at.host {
        Host::Ip(ip) => vec![*ip],
        Host::Name(n) => rules
            .resolver
            .lookup(n, at.port)
            .await
            .ok()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| FetchError::NotResolved { host: host.clone() })?,
    };
    for ip in &addrs {
        let class = match ranges::class(*ip) {
            Class::Public => rules
                .local
                .class(*ip)
                .map_or(Class::Public, Class::UnlessListed),
            c => c,
        };
        match class {
            Class::Public => {}
            Class::UnlessListed(_) if list.lists_address(*ip, at.port, scheme) => {}
            Class::UnlessListed(range) => {
                return Err(FetchError::AddressRefused {
                    host,
                    range,
                    never: false,
                })
            }
            Class::Never(range) => {
                return Err(FetchError::AddressRefused {
                    host,
                    range,
                    never: true,
                })
            }
        }
    }
    Ok(addrs)
}

/// One request to `at`, connecting to `addrs` only.
pub(super) async fn send(
    at: &Checked,
    addrs: &[IpAddr],
    headers: Option<&HeaderMap>,
    limits: &Limits,
    named: &str,
) -> Result<reqwest::Response, FetchError> {
    let mut b = reqwest::Client::builder()
        // A name the pin below does not answer is not looked up at all.
        .dns_resolver(std::sync::Arc::new(NoLookup))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        // HTTP/1.1, and no content encoding asked for or decoded, whatever
        // features reqwest is built with elsewhere in the workspace.
        .http1_only()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .connect_timeout(limits.connect)
        .pool_max_idle_per_host(0);
    if let (Host::Name(_), Some(name)) = (&at.host, at.url.host_str()) {
        // The resolver of this client answers this name with the checked
        // addresses only (the port is the URL's).
        let pinned: Vec<SocketAddr> = addrs
            .iter()
            .map(|ip| SocketAddr::new(*ip, at.port))
            .collect();
        b = b.resolve_to_addrs(name, &pinned);
    }
    let client = b.build().map_err(|e| {
        tracing::warn!(
            host = named,
            "docling source: building the client failed: {e}"
        );
        FetchError::Unreachable {
            host: named.to_string(),
            reading: false,
        }
    })?;
    let mut rb = client.get(at.url.clone());
    if let Some(h) = headers {
        rb = rb.headers(h.clone());
    }
    rb.send().await.map_err(|e| {
        let e = e.without_url();
        tracing::info!(host = %at.host, "docling source: request failed: {e}");
        if e.is_timeout() {
            FetchError::ConnectTimeout {
                host: named.to_string(),
                secs: limits.connect.as_secs(),
            }
        } else {
            FetchError::Unreachable {
                host: named.to_string(),
                reading: false,
            }
        }
    })
}

/// The client's own resolver: it answers nothing, so the only addresses a
/// hop's client can connect to are the pinned ones (`resolve_to_addrs`) or
/// the URL's own address.
struct NoLookup;

impl reqwest::dns::Resolve for NoLookup {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(std::future::ready(Err(
            "web sources resolve before they connect; this name was not pinned".into(),
        )))
    }
}

/// The body, cut at `left` (never past `limits.max_bytes`).
async fn read(
    mut resp: reqwest::Response,
    limits: &Limits,
    left: u64,
    named: &str,
) -> Result<Bytes, FetchError> {
    let cap = left.min(limits.max_bytes);
    let too_large = || FetchError::TooLarge {
        host: named.to_string(),
        max_body_mb: limits.max_body_mb,
        request: cap < limits.max_bytes,
    };
    let declared = resp.content_length();
    if declared.is_some_and(|n| n > cap) {
        return Err(too_large());
    }
    // Sized once from a declared length (at most the bound, checked above),
    // so a declared body is never copied into a doubled buffer; the bound
    // below still holds on what arrives.
    let mut buf =
        BytesMut::with_capacity(declared.map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX)));
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                if (buf.len() as u64).saturating_add(c.len() as u64) > cap {
                    return Err(too_large());
                }
                buf.extend_from_slice(&c);
            }
            Ok(None) => return Ok(buf.freeze()),
            Err(e) => {
                tracing::info!(
                    host = named,
                    "docling source: reading failed: {}",
                    e.without_url()
                );
                return Err(FetchError::Unreachable {
                    host: named.to_string(),
                    reading: true,
                });
            }
        }
    }
}
