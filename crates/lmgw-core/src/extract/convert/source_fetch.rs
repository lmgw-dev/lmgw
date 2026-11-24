//! Fetching the docling adapter's `http`/`https` sources
//! (document-conversion design ruling 9, D21, D28, §7.4.1, §9): the one
//! place lmgw makes an outbound request to a URL a caller chose, so every
//! rule here is against server-side request forgery on the owner's network.
//!
//! Only with Settings › Documents' "Let callers send web addresses"
//! (`documents.fetch_sources`) on; the adapter's `docling_serve::web`
//! answers the named 400 while it is off. Then, for each source:
//! - The URL is parsed once ([`url::Url`]) and only its parts are used.
//!   `http` and `https` only; a URL with a user name or password is refused.
//! - Only for a caller with a key: a request without a credential is
//!   refused by the adapter (`docling_serve::web`) whatever "Require gateway
//!   API keys on /v1" says, since `/v1` answers cross-origin and a web page
//!   open on this machine would otherwise read the allowed sites through
//!   the gateway.
//! - Only under a set `max_body_mb`: 0 (no limit) refuses web sources by
//!   name, as nothing would bound a download.
//! - Its host and port must be on the allowed sites ([`hosts`], with the
//!   matching rule), checked before anything is resolved.
//! - The host is resolved once per hop ([`resolve`]); **every** address it
//!   resolves to is classed ([`ranges`], then this machine's own addresses
//!   and networks, [`local`]), and a refused one refuses the hop unless the
//!   list names that address with the hop's port. The connection goes to exactly the
//!   addresses checked (the client's resolver is pinned to them and answers
//!   no other name), so there is no second lookup a rebinding name could
//!   answer differently.
//! - Redirects are followed by hand, at most [`SOURCE_FETCH_MAX_REDIRECTS`];
//!   each hop is checked like the first. `http` → `https` is followed,
//!   `https` → `http` is refused.
//! - The body is read as it streams and cut at `max_body_mb` (never trusting
//!   `Content-Length`, never buffered past the bound). Nothing is
//!   decompressed: the client asks for no encoding and decodes none, so the
//!   bound is on the bytes the conversion reads.
//! - [`SOURCE_FETCH_CONNECT_TIMEOUT`] per connection, [`SOURCE_FETCH_TIMEOUT`]
//!   for the whole fetch, redirects and body included.
//! - Fetches in flight gateway-wide are bounded by a pool of their own
//!   ([`slots`]) with the tool pool's size, so at most that many times
//!   `max_body_mb` is held at once; a fetch waits for its slot before its
//!   clock starts, and the wait ends when the request goes away.
//! - Nothing of lmgw's is sent: no credential, no cookie store, no proxy (an
//!   `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` in the environment is not used:
//!   the client is built with `no_proxy`), no user agent; HTTP/1.1 only. The
//!   caller's `headers` go to the source's own origin (scheme, host and
//!   port) only, and once a hop has left it, never again on that walk.
//! - TLS: the gateway's client stack, rustls with the platform verifier and
//!   the system's roots; nothing is turned off or pinned.
//!
//! Errors ([`FetchError`]) name the source's host and the rule; they never
//! carry what a remote host answered (no body, no header, no redirect
//! target, no resolved address).

use std::time::Duration;

use lmgw_api_types::documents::convert::ErrorCode;
use lmgw_api_types::units::fmt_bytes;

mod get;
pub mod hosts;
pub mod local;
pub mod ranges;
pub mod resolve;
pub mod slots;
#[cfg(test)]
mod tests;

pub use get::{fetch, Fetched};
pub use hosts::{Host, HostList};
pub use local::Local;

/// What a fetch is checked and bounded by.
pub struct Rules<'a> {
    /// The allowed sites.
    pub list: &'a HostList,
    pub resolver: &'a dyn resolve::Resolve,
    /// This machine's addresses, refused like private ones.
    pub local: &'a Local,
    pub limits: &'a Limits,
    /// The fetch pool ([`slots::global`] in the gateway).
    pub slots: &'a crate::extract::convert::pool::ToolPool,
}

/// The longest one source's fetch takes, its redirects and body included.
/// Five minutes is `PDF_TIMEOUT`'s budget: a body at the default
/// `max_body_mb` (64 MiB) needs about 220 KiB/s to arrive in it.
pub const SOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest one connection (TCP and TLS) takes to open: the gateway's
/// other clients' connect timeout.
pub const SOURCE_FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most redirects one source follows: reqwest's and browsers' default
/// bound.
pub const SOURCE_FETCH_MAX_REDIRECTS: u32 = 10;

/// What bounds a fetch.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// `max_body_mb` in bytes: one source's body, and the request's sources
    /// together.
    pub max_bytes: u64,
    /// `max_body_mb` as set, for the messages.
    pub max_body_mb: u32,
    pub total: Duration,
    pub connect: Duration,
    pub max_redirects: u32,
}

impl Limits {
    /// The gateway's: `max_body_mb` and the named constants.
    pub fn new(max_body_mb: u32) -> Self {
        Self {
            max_bytes: u64::from(max_body_mb) << 20,
            max_body_mb,
            total: SOURCE_FETCH_TIMEOUT,
            connect: SOURCE_FETCH_CONNECT_TIMEOUT,
            max_redirects: SOURCE_FETCH_MAX_REDIRECTS,
        }
    }
}

/// A URL checked against the list: what a hop connects to.
#[derive(Debug, Clone)]
pub struct Checked {
    pub url: url::Url,
    pub host: Host,
    pub port: u16,
}

/// Why a source was not fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Not a URL.
    InvalidUrl,
    /// Not `http` or `https`.
    Scheme(String),
    /// A user name or password in the URL.
    Userinfo,
    /// The host (and port) is not on the allowed sites.
    HostNotListed { host: String, port: u16 },
    /// An address the host resolved to is in a refused range.
    AddressRefused {
        host: String,
        range: &'static str,
        never: bool,
    },
    /// The name did not resolve.
    NotResolved { host: String },
    /// The connection did not open in `secs`.
    ConnectTimeout { host: String, secs: u64 },
    /// The connection or its TLS failed, or broke while reading.
    Unreachable { host: String, reading: bool },
    /// A redirect the rules refuse: `why` in words (its target is not
    /// named).
    RedirectRefused { host: String, why: String },
    /// More than `max` redirects.
    TooManyRedirects { host: String, max: u32 },
    /// The body passed `max_body_mb`; `request` when it was the request's
    /// sources together.
    TooLarge {
        host: String,
        max_body_mb: u32,
        request: bool,
    },
    /// The fetch did not end within `secs`.
    Timeout { host: String, secs: u64 },
    /// `max_body_mb` is 0 (no limit): web sources need a set limit.
    NoLimit,
    /// The request has no credential: web sources need a key.
    NoKey,
    /// The final answer was not a success.
    Status { host: String, status: u16 },
}

impl FetchError {
    /// Its name, as the caller sees it.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidUrl => "http_source_invalid_url",
            Self::NoLimit => "http_source_needs_limit",
            Self::NoKey => "http_source_needs_key",
            Self::Scheme(_) => "http_source_scheme_refused",
            Self::Userinfo => "http_source_userinfo_refused",
            Self::HostNotListed { .. } => "http_source_host_refused",
            Self::AddressRefused { .. } => "source_address_refused",
            Self::NotResolved { .. } | Self::Unreachable { .. } => "source_unreachable",
            Self::ConnectTimeout { .. } | Self::Timeout { .. } => "source_timeout",
            Self::RedirectRefused { .. } | Self::TooManyRedirects { .. } => {
                "source_redirect_refused"
            }
            Self::TooLarge { .. } => "source_too_large",
            Self::Status { .. } => "source_http_status",
        }
    }

    /// Whether it refuses the request before anything is fetched (a named
    /// 400), rather than failing its source alone.
    pub fn refuses_request(&self) -> bool {
        matches!(
            self,
            Self::InvalidUrl
                | Self::Scheme(_)
                | Self::Userinfo
                | Self::HostNotListed { .. }
                | Self::NoLimit
                | Self::NoKey
        )
    }

    /// The failed source's error code in the answer.
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidUrl
            | Self::Scheme(_)
            | Self::Userinfo
            | Self::HostNotListed { .. }
            | Self::NoLimit
            | Self::NoKey
            | Self::AddressRefused { .. } => ErrorCode::SourceRefused,
            Self::RedirectRefused { .. } | Self::TooManyRedirects { .. } => {
                ErrorCode::SourceRefused
            }
            Self::NotResolved { .. } | Self::Unreachable { .. } => ErrorCode::SourceUnreachable,
            Self::ConnectTimeout { .. } | Self::Timeout { .. } => ErrorCode::SourceTimeout,
            Self::TooLarge { .. } => ErrorCode::SourceTooLarge,
            Self::Status { .. } => ErrorCode::SourceHttpStatus,
        }
    }

    /// In words, starting with its name.
    pub fn message(&self) -> String {
        let said = match self {
            Self::InvalidUrl => "the url is not a web address lmgw can read".to_string(),
            Self::NoLimit => "web sources need a set request size limit, and max_body_mb is \
                              0 (no limit), so nothing would bound a download; the owner can \
                              set one under Settings › Network & access › Max request body, \
                              or send the file itself as a source of kind \"file\" with \
                              base64_string"
                .to_string(),
            Self::NoKey => "web addresses need an API key: lmgw fetches web sources only for \
                            a request that carries a key (Authorization: Bearer, or \
                            X-Api-Key), also while Settings › Network & access › \"Require \
                            gateway API keys on /v1\" is off; without one, send the file \
                            itself as a source of kind \"file\" with base64_string, or to \
                            /v1/convert/file"
                .to_string(),
            Self::Scheme(s) => {
                format!(
                    "the url's scheme is '{}'; lmgw fetches http and https sources only",
                    super::quoted::quoted(s)
                )
            }
            Self::Userinfo => "the url has a user name or password in it, which lmgw does not \
                               send; put credentials in the source's headers"
                .to_string(),
            Self::HostNotListed { host, port } => format!(
                "{host} (port {port}) is not on the owner's allowed sites (Settings › Documents › \
                 Web addresses › Allowed sites, documents.source_hosts); the owner can add it, \
                 or send the file itself as a source of kind \"file\" with base64_string"
            ),
            Self::AddressRefused { host, range, never } => {
                if *never {
                    format!(
                        "{host} resolves to an address in {range}, which lmgw never fetches \
                         from"
                    )
                } else {
                    format!(
                        "{host} resolves to an address in {range}, which lmgw fetches from only \
                         when the owner's allowed sites name that address itself"
                    )
                }
            }
            Self::NotResolved { host } => format!("the name {host} did not resolve"),
            Self::ConnectTimeout { host, secs } => format!(
                "the connection to {host} did not open within SOURCE_FETCH_CONNECT_TIMEOUT \
                 ({secs} s)"
            ),
            Self::Unreachable { host, reading } => {
                if *reading {
                    format!("the connection to {host} broke while the file was read")
                } else {
                    format!(
                        "lmgw could not connect to {host} (the connection failed or its TLS \
                         certificate was not accepted)"
                    )
                }
            }
            Self::RedirectRefused { host, why } => {
                format!("{host} redirected the request, and the redirect is refused: {why}")
            }
            Self::TooManyRedirects { host, max } => {
                format!("{host} redirected more than SOURCE_FETCH_MAX_REDIRECTS ({max}) times")
            }
            Self::TooLarge {
                host,
                max_body_mb,
                request,
            } => {
                let bound = fmt_bytes(u64::from(*max_body_mb) << 20);
                if *request {
                    format!(
                        "the file from {host} passed what is left of max_body_mb ({bound}), \
                         the most one request's web sources may download together, as its \
                         uploads may; send fewer sources per request, or the owner can raise \
                         max_body_mb under Settings → Network & access"
                    )
                } else {
                    format!(
                        "the file from {host} is larger than max_body_mb ({bound}), the most \
                         lmgw downloads for one source; the owner can raise it under Settings → \
                         Network & access"
                    )
                }
            }
            Self::Timeout { host, secs } => format!(
                "fetching from {host} did not finish within SOURCE_FETCH_TIMEOUT ({secs} s)"
            ),
            Self::Status { host, status } => format!("{host} answered HTTP {status}"),
        };
        format!("{}: {said}", self.code())
    }

    /// The host it names, for the record and the log (empty before one was
    /// read).
    pub fn host(&self) -> &str {
        match self {
            Self::InvalidUrl | Self::Scheme(_) | Self::Userinfo | Self::NoLimit | Self::NoKey => "",
            Self::HostNotListed { host, .. }
            | Self::AddressRefused { host, .. }
            | Self::NotResolved { host }
            | Self::ConnectTimeout { host, .. }
            | Self::Unreachable { host, .. }
            | Self::RedirectRefused { host, .. }
            | Self::TooManyRedirects { host, .. }
            | Self::TooLarge { host, .. }
            | Self::Timeout { host, .. }
            | Self::Status { host, .. } => host,
        }
    }
}

/// `text` parsed and checked against `list`: the scheme, the userinfo and
/// the host with its port. Nothing is resolved.
pub fn check(text: &str, list: &HostList) -> Result<Checked, FetchError> {
    let url = url::Url::parse(text.trim()).map_err(|_| FetchError::InvalidUrl)?;
    check_url(url, list)
}

/// [`check`] on a parsed URL (a redirect's target).
pub fn check_url(url: url::Url, list: &HostList) -> Result<Checked, FetchError> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(FetchError::Scheme(other.to_string())),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::Userinfo);
    }
    let host = url
        .host()
        .and_then(Host::from_url_host)
        .ok_or(FetchError::InvalidUrl)?;
    let port = url.port_or_known_default().ok_or(FetchError::InvalidUrl)?;
    if !list.allows(&host, port, url.scheme()) {
        return Err(FetchError::HostNotListed {
            host: host.to_string(),
            port,
        });
    }
    Ok(Checked { url, host, port })
}

/// The name docling gives a fetched source: the URL path's last segment as
/// it stands in the URL (not decoded), `file` when the path has none.
pub fn file_name(text: &str) -> String {
    url::Url::parse(text.trim())
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back().map(String::from))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "file".to_string())
}
