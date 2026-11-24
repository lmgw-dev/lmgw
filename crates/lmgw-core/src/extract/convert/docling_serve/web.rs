//! The docling adapter's `http` sources (document-conversion design ruling
//! 9, D28, §7.4.1): refused by name while Settings › Documents' "Let callers
//! send web addresses" (`documents.fetch_sources`) is off; with it on,
//! checked against the allowed sites (`documents.source_hosts`) and fetched
//! through `convert::source_fetch`, whose module doc has the rules.
//!
//! - A request without a credential (the anonymous caller, possible while
//!   "Require gateway API keys on /v1" is off) is refused with a named 401,
//!   whatever that setting says: `/v1` answers cross-origin, so a web page
//!   open on this machine could otherwise read the allowed sites through
//!   the gateway. Keyless uploads keep working as before.
//! - `max_body_mb` at 0 (no limit) refuses web sources with a named 400:
//!   nothing would bound a download.
//! - Before anything is fetched, every source's URL is checked: a scheme
//!   other than http/https, userinfo, or a host not on the list refuses the
//!   request (a named 400 that gives the source, the host and the setting).
//!   The route judges the request's options before this runs, so a request
//!   that answers 422 downloads nothing.
//! - Then the sources are fetched one after the other, in request order,
//!   each in a slot of the gateway-wide fetch pool
//!   (`convert::source_fetch::slots`).
//!   One that fails (a refused address or redirect, too large, a timeout, an
//!   HTTP error) is that file's failure, with the error's name; the others
//!   go on (§7.6). The request's sources together download at most
//!   `max_body_mb`, as its uploads would carry.
//! - The bytes become the file like an upload's: its format is sniffed from
//!   them, never from the URL or the remote's `Content-Type`. Its name is the
//!   URL path's last segment, as docling names it.
//! - Async requests fetch here too, at acceptance, so the job holds no URL
//!   and no header (the caller's headers are never stored) and its files go
//!   to the job's directory like uploads. The 202 waits for the downloads.
//! - The caller's `headers` are sent to the source's own origin only. The
//!   framing and hop-by-hop headers lmgw sets itself are dropped, by name,
//!   in `x-lmgw-option-notes` ([`DROPPED_HEADERS`]); header values are never
//!   logged or recorded.
//! - Each source's host is recorded with its outcome (`fetched`, `failed`,
//!   `refused`: also a fetch stopped by a refused address or redirect, so
//!   the owner sees a caller aim at one) and logged with it; never the URL,
//!   path or query.

use lmgw_api_types::documents::adapter_usage::OptionOutcome;
use lmgw_api_types::documents::convert::{
    ConvertError, ConvertStats, ConvertedDocument, DocStatus, ErrorCode,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

use super::{shape_note, Parsed, Refused};
use crate::extract::convert::file::Input;
use crate::extract::convert::pool::ToolPool;
use crate::extract::convert::source_fetch::{
    self, resolve::Resolve, FetchError, HostList, Limits, Local, Rules,
};

/// One `http` source as the request sent it, with what the record keeps.
pub struct WebSource {
    /// Its position in the request.
    pub index: u32,
    /// Its host as recorded ([`super::sources::host`]).
    pub host: String,
    /// `refused` until fetched; then `fetched`, `failed`, or `refused` when
    /// the fetch stopped at a refused address or redirect.
    pub outcome: &'static str,
    /// Whether its fetch ran (it became an input or a failed file).
    pub tried: bool,
    /// Emptied once the source is fetched.
    pub url: String,
    pub headers: Option<Map<String, Value>>,
}

impl WebSource {
    /// Whether it became an input or a failed file.
    pub fn done(&self) -> bool {
        self.tried
    }
}

/// Headers a caller's source may not set: lmgw sets the request's framing
/// and connection itself, sends nothing to a proxy, and decodes no content
/// encoding. `proxy-*` are the three HTTP defines.
pub const DROPPED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
    "accept-encoding",
];

/// What the fetch reads from the settings and the request.
pub struct WebConfig<'a> {
    /// `documents.fetch_sources`.
    pub on: bool,
    /// The request carries a credential (not the anonymous caller).
    pub keyed: bool,
    pub list: HostList,
    pub limits: Limits,
    pub resolver: &'a dyn Resolve,
    /// This machine's addresses, read for this request.
    pub local: Local,
    /// The fetch pool.
    pub slots: &'a ToolPool,
}

impl<'a> WebConfig<'a> {
    /// The gateway's: the settings in force, this machine's interfaces and
    /// listeners, the gateway-wide fetch pool.
    pub fn from_settings(
        s: &crate::config::Settings,
        keyed: bool,
        resolver: &'a dyn Resolve,
    ) -> Self {
        Self {
            on: s.documents.fetch_sources,
            keyed,
            list: HostList::lenient(&s.documents.source_hosts),
            limits: Limits::new(s.max_body_mb),
            resolver,
            local: Local::read(Local::listeners(s)),
            slots: source_fetch::slots::global(),
        }
    }
}

/// A refusal of the whole request by `e`.
fn refuse(e: &FetchError, status: u16, at: Option<u32>) -> Refused {
    tracing::info!(host = e.host(), "docling source refused ({})", e.code());
    let text = match at {
        Some(i) => format!("sources[{i}]: {}", e.message()),
        None => e.message(),
    };
    Refused::new(status, e.code(), text)
}

/// The refusal while the setting is off.
fn off(p: &Parsed) -> Refused {
    let hosts: Vec<&str> = p.web.iter().map(|w| w.host.as_str()).collect();
    Refused::new(
        400,
        "http_source_refused",
        format!(
            "lmgw does not fetch web addresses ({} source(s), host(s): {}) unless the owner \
             allows it; send the file itself instead, as a source of kind \"file\" with \
             base64_string, or to /v1/convert/file. The setting that allows web addresses is \
             Settings › Documents › \"Let callers send web addresses\" \
             (documents.fetch_sources), with the sites it may fetch from listed under \
             \"Allowed sites\" (documents.source_hosts)",
            hosts.len(),
            hosts.join(", ")
        ),
    )
}

/// The caller's headers as sent, and the names dropped.
fn headers(
    index: u32,
    h: Option<&Map<String, Value>>,
) -> Result<(HeaderMap, Vec<&'static str>), Refused> {
    let mut out = HeaderMap::new();
    let mut dropped = Vec::new();
    let bad = |why: String| {
        Refused::new(
            422,
            "invalid_source",
            format!("sources[{index}].headers: {why}"),
        )
    };
    for (k, v) in h.into_iter().flatten() {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| bad("a header name is not one HTTP allows".to_string()))?;
        if let Some(d) = DROPPED_HEADERS.iter().find(|d| **d == name.as_str()) {
            if !dropped.contains(d) {
                dropped.push(*d);
            }
            continue;
        }
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => return Err(bad("a header value is not a string".to_string())),
        };
        let value = HeaderValue::from_str(&text)
            .map_err(|_| bad("a header value is not one HTTP allows".to_string()))?;
        // `append` panics past the map's most entries; docling's 422 instead.
        out.try_append(name, value)
            .map_err(|_| bad("more headers than one request can carry".to_string()))?;
    }
    Ok((out, dropped))
}

/// A source the fetch failed: its file's entry.
fn failed(index: u32, name: String, e: &FetchError) -> ConvertedDocument {
    ConvertedDocument {
        index,
        name,
        status: DocStatus::Failed,
        format: None,
        parts: 0,
        content: None,
        structure: None,
        pictures: Vec::new(),
        text: None,
        warnings: Vec::new(),
        stats: ConvertStats::default(),
        error: Some(ConvertError {
            code: e.error_code(),
            message: e.message(),
            tool: None,
        }),
    }
}

/// Fetch `p`'s `http` sources by `cfg`: each becomes an input or a failed
/// file, and its host's outcome is recorded; or the request is refused
/// before anything is fetched.
pub async fn fetch_all(p: &mut Parsed, cfg: &WebConfig<'_>) -> Result<(), Refused> {
    if p.web.is_empty() {
        return Ok(());
    }
    if !cfg.on {
        return Err(off(p));
    }
    if !cfg.keyed {
        return Err(refuse(&FetchError::NoKey, 401, None));
    }
    if cfg.limits.max_body_mb == 0 {
        return Err(refuse(&FetchError::NoLimit, 400, None));
    }
    let mut ready = Vec::with_capacity(p.web.len());
    let mut all_dropped: Vec<&'static str> = Vec::new();
    for s in &p.web {
        let checked =
            source_fetch::check(&s.url, &cfg.list).map_err(|e| refuse(&e, 400, Some(s.index)))?;
        let (h, dropped) = headers(s.index, s.headers.as_ref())?;
        for d in dropped {
            if !all_dropped.contains(&d) {
                all_dropped.push(d);
            }
        }
        ready.push((s.index, source_fetch::file_name(&s.url), checked, h));
    }
    if !all_dropped.is_empty() {
        p.shape_notes.push(shape_note(
            "headers",
            OptionOutcome::Mapped,
            &format!("not sent because lmgw sets them: {}", all_dropped.join(" ")),
        ));
    }
    let rules = Rules {
        list: &cfg.list,
        resolver: cfg.resolver,
        local: &cfg.local,
        limits: &cfg.limits,
        slots: cfg.slots,
    };
    let mut left = cfg.limits.max_bytes;
    for (n, (index, name, checked, h)) in ready.into_iter().enumerate() {
        let host = checked.host.to_string();
        let got = source_fetch::fetch(checked, &h, &rules, left).await;
        let outcome = match got {
            Ok(f) => {
                tracing::info!(
                    host = %host,
                    bytes = f.bytes.len(),
                    redirects = f.redirects,
                    "docling source fetched"
                );
                left = left.saturating_sub(f.bytes.len() as u64);
                p.inputs.push(Input {
                    index,
                    name,
                    bytes: Ok(f.bytes),
                });
                "fetched"
            }
            Err(e) => {
                tracing::info!(host = %host, "docling source failed ({})", e.code());
                p.decided.push(failed(index, name, &e));
                // A refused address or redirect is a refusal in the record.
                if e.error_code() == ErrorCode::SourceRefused {
                    "refused"
                } else {
                    "failed"
                }
            }
        };
        if let Some(w) = p.web.get_mut(n) {
            w.outcome = outcome;
            w.tried = true;
            // Neither the address nor the caller's headers outlive the fetch.
            w.url = String::new();
            w.headers = None;
        }
    }
    p.inputs.sort_by_key(|i| i.index);
    Ok(())
}
