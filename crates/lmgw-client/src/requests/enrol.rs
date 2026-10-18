//! Enrolment over the device link (device link design §3.4, §3.5, §6.3):
//! the device sends its public key with the link's one-time token and gets
//! its certificate, which it then authenticates with on every connection.
//!
//! - [`enrol`](fn@enrol): `POST /devices/enrol`, sent over TLS pinned to the link's
//!   `fp` (the `tls` feature's `EnrolVerifier`) without a client
//!   certificate;
//! - [`read_enrolment`]: its answer as [`Credentials`], kept only when the
//!   CA certificate is the one `fp` pins, the certificate is for the key the
//!   device sent and the confirmation code is that key's;
//! - [`Credentials::state`]: valid, past `notice_after`, or lapsed.
//!
//! A refusal reads with [`read_refusal`](crate::requests::read_refusal): 410
//! [`code::PAIRING_TOKEN_USED`](crate::requests::code::PAIRING_TOKEN_USED) is
//! [`KeyRefused::TokenUsedElsewhere`](crate::requests::KeyRefused::TokenUsedElsewhere);
//! 410 `pairing_token_expired` and 401 `pairing_token_unknown` ask the owner
//! for a new link; the rest are shown with lmgw's code and message. An
//! answer lost on the way is asked for again with the same key while the
//! token lives (until it lapses or the owner ends it), which gets
//! the same answer; a new key would meet `pairing_token_used`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use lmgw_api_types::device_link::{EnrolRequest, EnrolledDevice, Enrolment, ENROL_PATH};

use super::{code, read_refusal, ApiError, Method, Request};
use crate::pairing::{refresh_urls, PairingLink, PairingToken, Urls};

/// `POST /devices/enrol` with the link's `token`, the device's public key
/// (`public_key`, its DER SubjectPublicKeyInfo, an EC P-256 key) and the
/// `client` text the Devices card shows ("Kai mobile 0.1.0 (Android 16)").
/// Read with [`read_enrolment`]. The request carries the token: keep it out
/// of logs.
pub fn enrol(token: &PairingToken, public_key: &[u8], client: Option<&str>) -> Request {
    let body = EnrolRequest {
        token: token.expose().to_string(),
        public_key: crate::base64::encode(public_key),
        client: client.map(str::to_string),
    };
    Request::new(Method::Post, ENROL_PATH.into())
        .json(serde_json::to_string(&body).expect("an enrolment always serializes"))
}

/// The confirmation code of the key whose DER SubjectPublicKeyInfo is
/// `public_key` (device link §3.5): `7F3A-91C2`, as lmgw derives it. The
/// device shows it; the owner confirms the same code on the Devices card.
pub fn confirm_code(public_key: &[u8]) -> String {
    lmgw_api_types::device_link::confirm_code(&Sha256::digest(public_key).into())
}

/// What a device keeps of its enrolment (device link §6.1, §6.2). None of
/// it is secret: the private key stays in the device's key store. Stored as
/// JSON (certificates as standard base64, dates as unix milliseconds); a
/// stored list of URLs is checked again when read.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// The device row's id on lmgw.
    pub device_id: i64,
    /// The device's certificate (DER), for the key it enrolled.
    #[serde(with = "der_base64")]
    pub certificate: Vec<u8>,
    /// lmgw's device CA (DER): the one root every later connection trusts.
    #[serde(with = "der_base64")]
    pub ca_certificate: Vec<u8>,
    /// When the certificate lapses, unix milliseconds.
    pub not_after_ms: i64,
    /// From when the client says "this device's certificate lapses on
    /// `<date>`; pair it again before then", unix milliseconds.
    pub notice_after_ms: i64,
    /// Where lmgw is, `https` only, in the owner's order; the feed's
    /// `hello.urls` replaces it ([`refresh_urls`]).
    pub urls: Urls,
    /// The confirmation code the device shows until the owner confirms.
    pub confirm_code: String,
}

impl std::fmt::Debug for Credentials {
    /// The certificates by their size: a log line gains nothing from their
    /// bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("device_id", &self.device_id)
            .field(
                "certificate",
                &format_args!("<{} bytes>", self.certificate.len()),
            )
            .field(
                "ca_certificate",
                &format_args!("<{} bytes>", self.ca_certificate.len()),
            )
            .field("not_after_ms", &self.not_after_ms)
            .field("notice_after_ms", &self.notice_after_ms)
            .field("urls", &self.urls)
            .field("confirm_code", &self.confirm_code)
            .finish()
    }
}

/// [`Credentials::state`]'s answer (device link §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// Before `notice_after`.
    Valid,
    /// From `notice_after` on: "this device's certificate lapses on
    /// `<not_after>`; pair it again before then".
    Notice,
    /// From `not_after` on: "this device's certificate lapsed on
    /// `<not_after>`; pair it again". lmgw refuses it at the next handshake.
    Lapsed,
}

impl Credentials {
    /// The certificate's state at `now_ms` (unix milliseconds, the client's
    /// clock).
    pub fn state(&self, now_ms: i64) -> CredentialState {
        self.dates().state(now_ms)
    }

    /// The certificate's dates.
    pub fn dates(&self) -> CertDates {
        CertDates {
            not_after_ms: self.not_after_ms,
            notice_after_ms: self.notice_after_ms,
        }
    }
}

/// A device certificate's two dates as unix milliseconds: stored with the
/// [`Credentials`], and read from the feed's `hello.cert`
/// ([`CertDates::of_hello`], [`crate::feed::FeedReader::certificate`]) so
/// the app can show the notice from lmgw's own record of the certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertDates {
    /// When the certificate lapses.
    pub not_after_ms: i64,
    /// From when the client says "this device's certificate lapses on
    /// `<not_after>`; pair it again before then".
    pub notice_after_ms: i64,
}

impl CertDates {
    /// `hello.cert`'s dates; `None` for a feed that carries none (not a
    /// device's, or a certificate not issued yet) or dates that do not read
    /// as RFC 3339.
    pub fn of_hello(cert: Option<&lmgw_api_types::chat_feed::HelloCert>) -> Option<Self> {
        let cert = cert?;
        Some(Self {
            not_after_ms: crate::rfc3339::unix_ms(&cert.not_after)?,
            notice_after_ms: crate::rfc3339::unix_ms(&cert.notice_after)?,
        })
    }

    /// The state at `now_ms` (unix milliseconds, the client's clock).
    pub fn state(&self, now_ms: i64) -> CredentialState {
        if now_ms >= self.not_after_ms {
            CredentialState::Lapsed
        } else if now_ms >= self.notice_after_ms {
            CredentialState::Notice
        } else {
            CredentialState::Valid
        }
    }
}

/// [`enrol`]'s answer as the device keeps it, `link` the link it enrolled
/// with and `public_key` the DER SubjectPublicKeyInfo it sent. A refusal is
/// lmgw's ([`read_refusal`]). An answer is refused, and nothing should be
/// stored, when:
/// - its CA certificate's key is not the one the link's `fp` pins
///   ([`code::CA_NOT_PINNED`]: not the lmgw this link pairs with);
/// - its certificate is not for `public_key`
///   ([`code::CERTIFICATE_NOT_FOR_KEY`]);
/// - its confirmation code is not `public_key`'s
///   ([`code::CONFIRM_CODE_MISMATCH`]): the owner would compare two codes
///   that cannot agree;
/// - it does not read (`unreadable`).
///
/// Of its URLs the `https` ones are kept ([`refresh_urls`]); with none
/// left, the link's.
pub fn read_enrolment(
    status: u16,
    body: &str,
    link: &PairingLink,
    public_key: &[u8],
) -> Result<Credentials, ApiError> {
    if !(200..300).contains(&status) {
        return Err(read_refusal(status, body));
    }
    let unreadable = |what: &str| ApiError {
        code: "unreadable".into(),
        message: format!("the enrolment's answer does not read: {what}"),
        ..Default::default()
    };
    let answer: Enrolment =
        serde_json::from_str(body).map_err(|e| unreadable(&format!("as Enrolment: {e}")))?;
    let ca_certificate = crate::base64::decode(&answer.ca_certificate)
        .ok_or_else(|| unreadable("ca_certificate is not base64"))?;
    if !link.fp.pins_certificate(&ca_certificate) {
        return Err(ApiError {
            code: code::CA_NOT_PINNED.into(),
            message: format!(
                "the answer's device CA is not the one the pairing link pins ({}): this is not \
                 the lmgw this link pairs with; nothing was stored",
                link.fp.short()
            ),
            ..Default::default()
        });
    }
    let certificate = crate::base64::decode(&answer.certificate)
        .ok_or_else(|| unreadable("certificate is not base64"))?;
    if crate::der::certificate_spki(&certificate) != Some(public_key) {
        return Err(ApiError {
            code: code::CERTIFICATE_NOT_FOR_KEY.into(),
            message: "lmgw's certificate is not for the key this device sent; nothing was stored"
                .into(),
            ..Default::default()
        });
    }
    let ours = confirm_code(public_key);
    if answer.confirm_code != ours {
        return Err(ApiError {
            code: code::CONFIRM_CODE_MISMATCH.into(),
            message: format!(
                "lmgw names the confirmation code {} where this device's key gives {ours}; \
                 nothing was stored",
                answer.confirm_code
            ),
            ..Default::default()
        });
    }
    let date = |field: &str, value: &str| {
        crate::rfc3339::unix_ms(value)
            .ok_or_else(|| unreadable(&format!("{field} is not an RFC 3339 time")))
    };
    Ok(Credentials {
        device_id: answer.device.id,
        not_after_ms: date("not_after", &answer.not_after)?,
        notice_after_ms: date("notice_after", &answer.notice_after)?,
        certificate,
        ca_certificate,
        urls: refresh_urls(&answer.urls).unwrap_or_else(|| link.urls.clone()),
        confirm_code: ours,
    })
}

/// DER bytes as standard base64 in a stored [`Credentials`].
mod der_base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(der: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::base64::encode(der))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        crate::base64::decode(&s).ok_or_else(|| serde::de::Error::custom("not base64"))
    }
}

#[cfg(test)]
mod tests;
