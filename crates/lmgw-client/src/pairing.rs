//! Pairing with lmgw over the device link (device link design §3.1, §6.3;
//! kai-mobile P4): the `lmgw-pair:` link v2 parsed strictly, what it pins
//! and carries, and what a client shows before it uses one.
//!
//! ```text
//! lmgw-pair:?v=2&name=<name>&url=<https URL>[&url=<https URL>…]&fp=sha256:<64 hex>&token=lmgw-enrol-<32 hex>
//! ```
//!
//! - [`PairingLink::parse`]: the link, strictly (every parameter but `url`
//!   once, nothing unknown, the same `url` never twice, `v=2` only; a v1
//!   link is told it is from before TLS);
//! - [`Fingerprint`]: `fp`, the SHA-256 of lmgw's device CA's
//!   SubjectPublicKeyInfo, which the enrolment pins;
//! - [`PairingToken`]: the one-time token the enrolment presents;
//! - [`Urls`]: the device's lmgw URLs, `https` only, the order to try them
//!   in, and the rule for a list lmgw refreshes ([`refresh_urls`]);
//! - [`PairFacts`] and [`display_name`]: what the owner confirms before a
//!   client uses a link.
//!
//! The link is a credential while its token lives. Nothing here logs it, and
//! no error echoes any value taken from it: a mangled link can put the token
//! anywhere (a parameter name, a URL).

#[cfg(test)]
mod tests;
mod urls;

use std::fmt;

use lmgw_api_types::device_link::{
    FP_PREFIX, LINK_PREFIX, LINK_VERSION, TOKEN_HEX_DIGITS, TOKEN_PREFIX,
};
use sha2::{Digest, Sha256};

pub use urls::{refresh_urls, Urls};

/// `fp=sha256:<64 hex>`: the SHA-256 of lmgw's device CA certificate's DER
/// SubjectPublicKeyInfo (device link §2.1). It pins the CA's key, not a
/// certificate, so lmgw re-issues its server certificates without a
/// device noticing. Shown as `sha256:<64 lowercase hex>` ([`fmt::Display`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// `sha256:` and 64 hex digits; the digits are read in either case.
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let hex = s.strip_prefix(FP_PREFIX).ok_or(LinkError::BadFingerprint)?;
        let mut digest = [0u8; 32];
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(LinkError::BadFingerprint);
        }
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| LinkError::BadFingerprint)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| LinkError::BadFingerprint)?;
        }
        Ok(Self(digest))
    }

    /// The fingerprint of a DER SubjectPublicKeyInfo.
    pub fn of_spki(spki_der: &[u8]) -> Self {
        Self(Sha256::digest(spki_der).into())
    }

    /// Whether DER certificate `cert` carries the key this fingerprint pins.
    /// A certificate that does not parse pins nothing.
    pub fn pins_certificate(&self, cert: &[u8]) -> bool {
        crate::der::certificate_spki(cert).is_some_and(|spki| Self::of_spki(spki) == *self)
    }

    /// The SHA-256 itself.
    pub fn digest(&self) -> &[u8; 32] {
        &self.0
    }

    /// The short form the gateway's log line and Devices card show:
    /// `sha256:9f2c…41ab`, the first and last four hex digits.
    pub fn short(&self) -> String {
        let hex = self.hex();
        format!("{FP_PREFIX}{}…{}", &hex[..4], &hex[60..])
    }

    fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{FP_PREFIX}{}", self.hex())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// A pairing token (`lmgw-enrol-<32 lowercase hex>`, device link §3.3):
/// single-use and short-lived, presented once to `POST /devices/enrol`. Its
/// `Debug` is redacted and it has no `Display`, so it cannot reach a log
/// line by accident. The digits must be lowercase, as lmgw writes them: it
/// keeps only the token's hash, so a token is sent exactly as given.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingToken(String);

impl PairingToken {
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let ok = s.strip_prefix(TOKEN_PREFIX).is_some_and(|hex| {
            hex.len() == TOKEN_HEX_DIGITS
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(LinkError::BadToken {
                len: s.chars().count(),
            })
        }
    }

    /// The token as the enrolment sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PairingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingToken(<redacted>)")
    }
}

/// A parsed `lmgw-pair:` link v2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingLink {
    /// The device's name as lmgw gave it, without `device:`. Show it with
    /// [`display_name`]: the link may come from anyone.
    pub name: String,
    /// The `https` URLs, normalized, in the owner's order.
    pub urls: Urls,
    /// The device CA the enrolment pins.
    pub fp: Fingerprint,
    /// The one-time token the enrolment presents.
    pub token: PairingToken,
}

/// Why a link does not parse. No variant carries a value taken from the
/// link.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkError {
    NotALink,
    MissingVersion,
    /// `v=1`: a link from before TLS, carrying a bearer key lmgw no longer
    /// accepts.
    PairedBeforeTls,
    UnsupportedVersion(String),
    Missing(&'static str),
    Empty(&'static str),
    Duplicate(&'static str),
    /// The same URL (after normalizing) twice, at these 1-based positions
    /// among the link's `url`s.
    DuplicateUrl {
        first: usize,
        second: usize,
    },
    Unknown(String),
    /// The `url` at this 1-based position is not usable, and why (a rule,
    /// never the URL).
    BadUrl {
        position: usize,
        reason: String,
    },
    BadFingerprint,
    BadToken {
        len: usize,
    },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotALink => write!(f, "not a pairing link: it must start with `{LINK_PREFIX}`"),
            Self::MissingVersion => write!(
                f,
                "the link has no `v` parameter; this client reads v={LINK_VERSION} links"
            ),
            Self::PairedBeforeTls => f.write_str(
                "this link is from a pairing before TLS; make a new one on lmgw's Devices card",
            ),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported link version v={v}; this client reads v={LINK_VERSION} links"
            ),
            Self::Missing(name) => write!(f, "the link has no `{name}` parameter"),
            Self::Empty(name) => write!(f, "the link's `{name}` parameter is empty"),
            Self::Duplicate(name) => write!(f, "the link carries `{name}` twice"),
            Self::DuplicateUrl { first, second } => write!(
                f,
                "the link carries the same url twice (urls {first} and {second})"
            ),
            Self::Unknown(what) => write!(
                f,
                "the link carries {what}, which a v={LINK_VERSION} link does not have"
            ),
            Self::BadUrl { position, reason } => {
                write!(f, "the link's url {position} is not usable: {reason}")
            }
            Self::BadFingerprint => write!(
                f,
                "the link's fp is not a fingerprint: expected `{FP_PREFIX}` and 64 hex digits"
            ),
            Self::BadToken { len } => write!(
                f,
                "the link's token is not a pairing token: expected `{TOKEN_PREFIX}` and \
                 {TOKEN_HEX_DIGITS} lowercase hex digits, got {len} characters"
            ),
        }
    }
}

impl std::error::Error for LinkError {}

/// The parameters a v2 link has; `url` may repeat.
const SINGLE: [&str; 4] = ["v", "name", "fp", "token"];

impl PairingLink {
    /// Parses a link, strictly: `v` must be `2`; `name`, `fp` and `token`
    /// appear exactly once and `url` at least once, never the same URL
    /// twice; nothing else is accepted. Surrounding whitespace (from a
    /// paste) is ignored, and the scheme is read in any case.
    pub fn parse(input: &str) -> Result<Self, LinkError> {
        let s = input.trim();
        let query = s
            .get(..LINK_PREFIX.len())
            .filter(|p| p.eq_ignore_ascii_case(LINK_PREFIX))
            .map(|_| &s[LINK_PREFIX.len()..])
            .ok_or(LinkError::NotALink)?;

        let mut single: [Option<String>; 4] = Default::default();
        let mut urls: Vec<String> = Vec::new();
        let mut unknown: Option<String> = None;
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            if k == "url" {
                urls.push(v.into_owned());
                continue;
            }
            match SINGLE.iter().position(|n| *n == k) {
                Some(i) if single[i].is_some() => return Err(LinkError::Duplicate(SINGLE[i])),
                Some(i) => single[i] = Some(v.into_owned()),
                None => {
                    unknown.get_or_insert_with(|| describe_name(&k));
                }
            }
        }
        let [v, name, fp, token] = single;

        // The version decides what the rest means, so it is checked first.
        match v.as_deref() {
            None => return Err(LinkError::MissingVersion),
            Some(LINK_VERSION) => {}
            Some("1") => return Err(LinkError::PairedBeforeTls),
            Some(other) => return Err(LinkError::UnsupportedVersion(describe_value(other))),
        }
        if let Some(u) = unknown {
            return Err(LinkError::Unknown(u));
        }
        let required = |value: Option<String>, name: &'static str| match value {
            None => Err(LinkError::Missing(name)),
            Some(s) if s.trim().is_empty() => Err(LinkError::Empty(name)),
            Some(s) => Ok(s),
        };
        let name = required(name, "name")?;
        if urls.is_empty() {
            return Err(LinkError::Missing("url"));
        }
        let fp = required(fp, "fp")?;
        let token = required(token, "token")?;

        let mut normalized: Vec<String> = Vec::with_capacity(urls.len());
        for (i, u) in urls.iter().enumerate() {
            if u.trim().is_empty() {
                return Err(LinkError::Empty("url"));
            }
            let u = normalize_base_url(u).map_err(|reason| LinkError::BadUrl {
                position: i + 1,
                reason,
            })?;
            if let Some(first) = normalized.iter().position(|n| *n == u) {
                return Err(LinkError::DuplicateUrl {
                    first: first + 1,
                    second: i + 1,
                });
            }
            normalized.push(u);
        }
        Ok(Self {
            name,
            urls: Urls::from_normalized(normalized),
            fp: Fingerprint::parse(&fp)?,
            token: PairingToken::parse(&token)?,
        })
    }
}

/// Whether `s` is hex digits only: what a piece of a token (a link split by
/// a stray `&` or `=`) looks like.
fn hex_only(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Names an unknown parameter in an error only when it looks like a
/// parameter name and not like a piece of the token; anything else could be
/// part of it.
fn describe_name(k: &str) -> String {
    if !k.is_empty()
        && k.len() <= 16
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !hex_only(k)
    {
        format!("a parameter `{k}`")
    } else {
        "an unnamed or malformed parameter".into()
    }
}

/// An unsupported `v` as an error shows it: a version number (three
/// characters at most when it is hex digits only, which leaks nothing a
/// token would miss), or `<malformed>`.
fn describe_value(v: &str) -> String {
    if !v.is_empty()
        && v.len() <= 8
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        && (v.len() <= 3 || !hex_only(v))
    {
        v.to_owned()
    } else {
        "<malformed>".into()
    }
}

/// An lmgw base URL on the device link: `https`, a host, an optional port
/// and path prefix; no user info, query or fragment. Returned without a
/// trailing slash, so a stored URL and a refreshed one compare equal however
/// either was written. The error is a rule, never the URL itself.
pub fn normalize_base_url(s: &str) -> Result<String, String> {
    let u = url::Url::parse(s.trim()).map_err(|e| format!("not a URL ({e})"))?;
    if u.scheme() != "https" {
        return Err("the scheme must be https: the device link is TLS only".into());
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err("it has no host".into());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("it must not carry a user name or password".into());
    }
    if u.query().is_some() {
        return Err("it must not carry a query (?…)".into());
    }
    if u.fragment().is_some() {
        return Err("it must not carry a fragment (#…)".into());
    }
    Ok(u.as_str().trim_end_matches('/').to_owned())
}

/// How many characters of the device name a dialog shows; a longer name is
/// shortened with "… (shortened)", so the dialog says so. The name is the
/// link's own text, and the link may come from anyone.
pub const NAME_SHOWN_CHARS: usize = 64;

/// What the owner confirms before a client uses a link (client-apps §1.4:
/// a client confirms before it replaces an existing pairing, showing the new
/// URLs). Every value is safe to show: the token is not among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairFacts {
    /// The link's URLs, in its order.
    pub urls: Vec<String>,
    /// The device name, safe to show ([`display_name`]).
    pub name: String,
    /// The pinned CA's fingerprint, shortened ([`Fingerprint::short`]).
    pub fp: String,
    /// What names the pairing this one replaces (the client's current
    /// gateway URL); `None` for a first pairing.
    pub replaces: Option<String>,
}

/// One line of a pairing dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactLine {
    Plain(String),
    Warning(String),
}

impl PairFacts {
    /// `current`: what names the client's present pairing, its first URL.
    pub fn new(link: &PairingLink, current: Option<&str>) -> Self {
        Self {
            urls: link.urls.as_slice().to_vec(),
            name: display_name(&link.name),
            fp: link.fp.short(),
            replaces: current.map(str::to_owned),
        }
    }

    /// The dialog's lines, in order: the gateway's addresses, the device,
    /// lmgw's identity, what it replaces.
    pub fn lines(&self) -> Vec<FactLine> {
        let mut lines = vec![match self.urls.as_slice() {
            [one] => FactLine::Plain(format!("Gateway: {one}")),
            many => FactLine::Plain(format!("Gateway: {}, tried in that order", many.join(", "))),
        }];
        lines.push(FactLine::Plain(format!("Device name: {}", self.name)));
        lines.push(FactLine::Plain(format!("Gateway identity: {}", self.fp)));
        match &self.replaces {
            Some(old) if self.urls.iter().any(|u| u == old) => {
                lines.push(FactLine::Warning(format!(
                    "This replaces this device's pairing with {old}: it gets a new key and \
                     certificate, and the old ones stop working."
                )))
            }
            Some(old) => lines.push(FactLine::Warning(format!(
                "This replaces this device's pairing with {old}: the device stops using that \
                 gateway and deletes the key it keeps for it."
            ))),
            None => {}
        }
        lines
    }
}

/// Unicode's format characters (general category Cf: bidirectional
/// formatting, zero-width and joiners, the soft hyphen, the BOM, tags) and
/// its line and paragraph separators (Zl, Zp), which toolkits break lines
/// at.
fn format_or_separator(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}'
        | '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// The device name as a dialog shows it: control characters, format
/// characters (bidirectional formatting, zero-width) and line or paragraph
/// separators, which could make one name read as another or add a line
/// under it, become U+FFFD (Kai's review KAI-2 K2-14); past
/// [`NAME_SHOWN_CHARS`] it is cut with "… (shortened)".
pub fn display_name(name: &str) -> String {
    let clean: Vec<char> = name
        .chars()
        .map(|c| {
            if c.is_control() || format_or_separator(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    if clean.len() > NAME_SHOWN_CHARS {
        let mut s: String = clean[..NAME_SHOWN_CHARS].iter().collect();
        s.push_str("… (shortened)");
        s
    } else {
        clean.into_iter().collect()
    }
}
