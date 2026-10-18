//! The device link's wire (device link design §3, §4.7): the pairing link
//! v2's fixed parts, `POST /devices/enrol`'s body and answer, the refusal
//! codes a device reads, and the confirmation code's form. Shared so the
//! gateway, which mints the link, serves enrolment and refuses, and
//! `lmgw-client`, which parses, enrols and reads, agree on every byte.
//!
//! The link v2 (§3.1), one string given once as text and as a QR code:
//!
//! ```text
//! lmgw-pair:?v=2&name=<name>&url=<https URL>[&url=<https URL>…]&fp=sha256:<64 hex>&token=lmgw-enrol-<32 hex>
//! ```
//!
//! `url` repeats, in the owner's order; `fp` pins lmgw's device CA; `token`
//! is single-use and short-lived. The link is a credential while its token
//! lives: no client logs it.
//!
//! Beside the wire, the `devices` settings class (§9.1) with its stated
//! bounds, and the status block's trust half (§9.2): lmgw's device CA and
//! the server certificates issued from it.

use serde::{Deserialize, Serialize};

/// What every pairing link starts with: the scheme and the `?` of its
/// query. A client reads the scheme in any case.
pub const LINK_PREFIX: &str = "lmgw-pair:?";

/// The link's `v`: this design's links are `2`. A `1` is a pairing from
/// before TLS, which this lmgw no longer accepts (§12).
pub const LINK_VERSION: &str = "2";

/// `fp`'s prefix. The rest is the lowercase hex SHA-256 of the device CA
/// certificate's DER SubjectPublicKeyInfo (RFC 7469's pin input, §2.1).
pub const FP_PREFIX: &str = "sha256:";

/// A pairing token's prefix; [`TOKEN_HEX_DIGITS`] lowercase hex digits
/// follow (128 random bits, §3.3).
pub const TOKEN_PREFIX: &str = "lmgw-enrol-";

/// The hex digits after [`TOKEN_PREFIX`].
pub const TOKEN_HEX_DIGITS: usize = 32;

/// `POST /devices/enrol`, served on the device listener only (§3.4, §4.6).
pub const ENROL_PATH: &str = "/devices/enrol";

/// The largest body a `/devices/*` route takes, in bytes: 16 KiB, stated in
/// the routes' `DocRoute` instead of `max_body_mb`. A larger one is
/// answered 413 [`code::BODY_TOO_LARGE`], naming it.
pub const BODY_LIMIT_BYTES: usize = 16 * 1024;

/// `POST /devices/enrol`'s body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnrolRequest {
    /// The link's `token`: `lmgw-enrol-` and 32 lowercase hex digits. Secret
    /// until used.
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::secret))]
    pub token: String,
    /// The device's public key: standard base64 (RFC 4648 §4, padded) of its
    /// DER SubjectPublicKeyInfo, an EC P-256 key.
    pub public_key: String,
    /// Free text naming the client, shown on the Devices card
    /// ("Kai mobile 0.1.0 (Android 16)"). Optional, and never trusted for
    /// anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

impl std::fmt::Debug for EnrolRequest {
    /// Everything but the token, which a log line must not carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolRequest")
            .field("token", &"<redacted>")
            .field("public_key", &self.public_key)
            .field("client", &self.client)
            .finish()
    }
}

/// `POST /devices/enrol`'s `201 Created`: the device's certificate for the
/// key it sent, and what it keeps beside it. The same key presenting the
/// same used token again within its life gets this answer again (§3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Enrolment {
    /// The device row the token belongs to.
    pub device: EnrolledDevice,
    /// The device's certificate: standard base64 of its DER. `clientAuth`
    /// only, issued by the device CA for the key the body sent.
    pub certificate: String,
    /// lmgw's device CA certificate: standard base64 of its DER. Its
    /// SubjectPublicKeyInfo hashes to the link's `fp`; a client keeps it only
    /// then.
    pub ca_certificate: String,
    /// When the certificate lapses (RFC 3339, UTC).
    pub not_after: String,
    /// From when the client says "this device's certificate lapses on
    /// `<date>`; pair it again before then": the issue plus two thirds of the
    /// validity (RFC 3339, UTC; §2.3).
    pub notice_after: String,
    /// The device's lmgw URLs, `https`, in the owner's order (§3.1).
    pub urls: Vec<String>,
    /// The confirmation code ([`confirm_code`]): the device shows it, and
    /// the owner confirms the same code on the Devices card (§3.5).
    pub confirm_code: String,
    /// Whether the owner confirmed this enrolment already (an answer asked
    /// again after the Confirm). Until then every request of the device is
    /// refused 403 [`code::DEVICE_UNCONFIRMED`].
    #[serde(default)]
    pub confirmed: bool,
}

/// [`Enrolment::device`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnrolledDevice {
    /// The device row's `api_keys.id`; re-pairing keeps it.
    pub id: i64,
    /// The device's name as lmgw gave it, without `device:`.
    pub name: String,
}

/// The refusal codes of the device listener (§4.7) and of enrolment
/// (§3.4) that a device reads. Enrolment also answers `bad_request` (not
/// the body's shape) and `device_disabled` (the token's device is
/// disabled; the token stays unused), and the listener `device_disabled`
/// and `device_key_unknown`, as every route does.
pub mod code {
    /// `401`: a request without a client certificate on any route but
    /// enrolment. Every such answer carries `Connection: close`.
    pub const CLIENT_CERTIFICATE_REQUIRED: &str = "client_certificate_required";
    /// `400`: a bearer, `x-api-key`, session cookie or key subprotocol on
    /// the device listener, which authenticates by client certificate only.
    pub const CREDENTIAL_ON_DEVICE_LISTENER: &str = "credential_on_device_listener";
    /// `403`: an enrolled device the owner has not confirmed yet; the
    /// message names the code ([`super::unconfirmed_message`]).
    pub const DEVICE_UNCONFIRMED: &str = "device_unconfirmed";
    /// `401`: the client certificate is past its `notAfter`: pair again.
    pub const DEVICE_CERTIFICATE_EXPIRED: &str = "device_certificate_expired";
    /// `400`: `public_key` is no DER SubjectPublicKeyInfo, or not EC P-256;
    /// the message names the accepted kind.
    pub const PUBLIC_KEY_UNSUPPORTED: &str = "public_key_unsupported";
    /// `401`: no such token (never issued, or its row deleted).
    pub const PAIRING_TOKEN_UNKNOWN: &str = "pairing_token_unknown";
    /// `409`: another device enrolled this key.
    pub const PUBLIC_KEY_IN_USE: &str = "public_key_in_use";
    /// `410`: another key used this token; the device row is now contested.
    /// For the device: someone else used this code, delete the device on
    /// lmgw.
    pub const PAIRING_TOKEN_USED: &str = "pairing_token_used";
    /// `410`: the token expired; the message names the time and
    /// `devices.token_minutes`.
    pub const PAIRING_TOKEN_EXPIRED: &str = "pairing_token_expired";
    /// `413`: a `/devices/*` body over [`super::BODY_LIMIT_BYTES`].
    pub const BODY_TOO_LARGE: &str = "body_too_large";
    /// `429`, with `Retry-After`: too many failed enrolments from one
    /// address (an IPv6 address by its /64) in a minute, past
    /// `devices.enrol_refuse_per_min`.
    pub const PAIRING_RATE_LIMITED: &str = "pairing_rate_limited";
    /// `503`: the device CA cannot be read or unwrapped; the message names
    /// the reason.
    pub const DEVICES_CA_UNAVAILABLE: &str = "devices_ca_unavailable";
    /// `409`, from `key_create` and `device_token` (not a device's to read):
    /// the device listener does not serve, so a pairing token would reach
    /// nothing; the message names the setting and why.
    pub const DEVICE_LISTENER_OFF: &str = "device_listener_off";
    /// `409`, from `device_confirm` (not a device's to read): the code sent
    /// is not the one of the key waiting for the confirmation, so nothing
    /// was confirmed. The message never says the right one: the owner
    /// compares with the code the device shows.
    pub const CONFIRM_CODE_MISMATCH: &str = "confirm_code_mismatch";
}

/// The confirmation code (§3.5, D23) of a key whose DER
/// SubjectPublicKeyInfo has the SHA-256 `spki_sha256`: its first 32 bits as
/// two groups of four uppercase hex digits, `7F3A-91C2`. Both ends derive it
/// from the same key, so a stranger enrolling with a stolen token, who holds
/// another key, shows another code. Each side hashes with its own SHA-256;
/// this is the form they share.
pub fn confirm_code(spki_sha256: &[u8; 32]) -> String {
    let [a, b, c, d, ..] = *spki_sha256;
    format!("{a:02X}{b:02X}-{c:02X}{d:02X}")
}

/// A confirmation code as a person types it, in [`confirm_code`]'s form:
/// eight hex digits in either case, the hyphen and spaces optional
/// (`7f3a 91c2`, `7F3A-91C2`). `None` for anything else.
pub fn parse_confirm_code(typed: &str) -> Option<String> {
    let digits: String = typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digits = digits.to_ascii_uppercase();
    Some(format!("{}-{}", &digits[..4], &digits[4..]))
}

/// The gateway's wording of [`code::DEVICE_UNCONFIRMED`]'s message: the code
/// and where to confirm it. For people, not a contract: a client reads the
/// refusal by its code and shows the code of its own key, which
/// `read_enrolment` already held to lmgw's.
pub fn unconfirmed_message(confirm_code: &str) -> String {
    format!(
        "this device waits for the owner's confirmation: confirm the code {confirm_code} on \
         lmgw's Devices card"
    )
}

/// A `devices.*` setting's stated range, both ends inclusive (§9.1): a save
/// outside it is refused by name, and a stored value outside it is clamped
/// on load with a warning naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    pub min: u32,
    pub max: u32,
}

/// `devices.token_minutes`: a pairing token's lifetime, in minutes.
pub const TOKEN_MINUTES: Bound = Bound { min: 1, max: 1440 };
/// `devices.client_cert_days`: a device certificate's validity, in days.
pub const CLIENT_CERT_DAYS: Bound = Bound { min: 1, max: 3650 };
/// `devices.server_cert_days`: a server certificate's validity, in days.
pub const SERVER_CERT_DAYS: Bound = Bound { min: 1, max: 3650 };
/// `devices.handshake_timeout_s`: a TLS handshake's bound, in seconds.
pub const HANDSHAKE_TIMEOUT_S: Bound = Bound { min: 1, max: 300 };
/// `devices.header_timeout_s`: a request head's bound, and an idle
/// kept-alive connection's, in seconds.
pub const HEADER_TIMEOUT_S: Bound = Bound { min: 1, max: 3600 };
/// `devices.unauthenticated_timeout_s`: a whole connection without a client
/// certificate, in seconds.
pub const UNAUTHENTICATED_TIMEOUT_S: Bound = Bound { min: 1, max: 300 };
/// `devices.enrol_refuse_per_min`: failed enrolments per address (an IPv6
/// address by its /64) per minute before a 429.
pub const ENROL_REFUSE_PER_MIN: Bound = Bound {
    min: 1,
    max: 100_000,
};
/// `devices.unauthenticated_max`: the device listener's connections that
/// have not presented a certificate yet (handshakes in progress and
/// connections without one), at most, from every peer together.
pub const UNAUTHENTICATED_MAX: Bound = Bound { min: 1, max: 4096 };
/// `devices.unauthenticated_per_peer`: the same, from one peer (an IPv4
/// address, an IPv6 address by its /64).
pub const UNAUTHENTICATED_PER_PEER: Bound = Bound { min: 1, max: 4096 };

/// Settings → Network & access → Devices (`devices.*` in
/// `GET /api/settings-full`, device link design §9.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DevicesSettings {
    /// Whether the device listener runs: the TLS listener paired devices
    /// connect to. Off until the first pairing turns it on.
    pub listener: bool,
    /// Where the device listener binds: `loopback` (`127.0.0.1` and `[::1]`,
    /// while every paired device is this computer), `all` (`[::]`, every
    /// address) or one IP address.
    pub listen: String,
    /// The device listener's port; `null` = the HTTP port's number plus one,
    /// written as a number the first time the listener starts.
    pub port: Option<u16>,
    /// The port the device listener binds: `port`, else the HTTP port's
    /// number plus one; `null` when `port` is `null` and the HTTP port is
    /// 65535 (no port above it), so the setting must name one.
    pub port_effective: Option<u16>,
    /// A pairing token's lifetime, in minutes (1 to 1440); applies to the
    /// next token.
    pub token_minutes: u32,
    /// A device certificate's validity, in days (1 to 3650); the card and
    /// the device show a notice from two thirds of it. Applies to the next
    /// certificate issued.
    pub client_cert_days: u32,
    /// A server certificate's validity, in days (1 to 3650); a server
    /// certificate is re-issued from half of it, unnoticed by devices.
    /// Applies to the next certificate issued.
    pub server_cert_days: u32,
    /// A TLS handshake's bound, in seconds (1 to 300); applies to the next
    /// connection.
    pub handshake_timeout_s: u32,
    /// A request head's bound, and an idle kept-alive connection's wait for
    /// the next one, in seconds (1 to 3600); applies to the next connection.
    pub header_timeout_s: u32,
    /// A whole connection without a client certificate, in seconds (1 to
    /// 300); applies to the next connection.
    pub unauthenticated_timeout_s: u32,
    /// Failed enrolments per address (an IPv6 address by its /64) per
    /// minute before the address is answered 429 (1 to 100000); applies at
    /// once.
    pub enrol_refuse_per_min: u32,
    /// The device listener's connections that have not authenticated yet
    /// (handshakes in progress, connections without a certificate or with
    /// one that opens no working device), at most, from every peer together
    /// (1 to 4096); a new one past it closes the oldest. Applies to the next
    /// connection.
    pub unauthenticated_max: u32,
    /// The same, from one peer: an IPv4 address, an IPv6 address by its /64
    /// (1 to 4096); a new one past it closes that peer's oldest. Applies to
    /// the next connection.
    pub unauthenticated_per_peer: u32,
}

/// `devices` in `GET /api/status` (device link design §9.2): lmgw's device
/// CA and the server certificates issued from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DevicesStatus {
    /// lmgw's device CA: whether it exists and is loaded, its pin and how
    /// its key is kept, or why it cannot be used.
    pub ca: DeviceCaStatus,
    /// The server certificates issued since this start, one per local
    /// address that served a device connection (one for every loopback
    /// address); empty until the device listener serves.
    pub leaves: Vec<DeviceLeafStatus>,
    /// The device listener: off, where it listens, or why it does not.
    pub listener: DeviceListenerStatus,
    /// Pairing: the tokens alive and the failed enrolments counted.
    pub pairing: DevicePairingStatus,
}

/// Pairing in `GET /api/status` (device link design §3.3, §9.2): the tokens
/// alive, and the failed enrolments of the last hour.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DevicePairingStatus {
    /// The pairing tokens alive now (minted, not lapsed, not ended; a used
    /// one stays alive for its same-key repeat): while any is, a device
    /// listener handshake may come without a client certificate, for
    /// enrolment alone.
    pub tokens_alive: u64,
    /// Failed enrolments (an unknown, used, lapsed or ended token) in the
    /// last hour, from every address.
    pub failures_last_hour: u64,
    /// The address of the last failure (an IPv6 address by its /64), `null`
    /// when none was counted.
    pub last_failure_from: Option<String>,
    /// When it was, RFC 3339 UTC.
    pub last_failure_at: Option<String>,
    /// Since when failures are counted: the counts live in memory and start
    /// at zero with each start of lmgw (RFC 3339 UTC).
    pub counted_since: String,
}

/// A device row's pairing (device link design §3.6, §9.3), as the Keys
/// list shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DevicePairingView {
    /// Where its pairing stands.
    pub state: DevicePairingState,
    /// Its latest pairing token, if it was ever given one.
    pub token: Option<PairingTokenView>,
    /// When its last enrolment was (RFC 3339 UTC), from where (the peer
    /// address) and with which client (the enrolment's free text, never
    /// trusted for anything): a waiting key's, while one waits.
    pub enrolled_at: Option<String>,
    pub enrolled_from: Option<String>,
    pub client: Option<String>,
    /// The confirmation code, `7F3A-91C2`, of the key that waits for the
    /// owner's confirmation in state `enrolled`, of the confirmed key
    /// otherwise: what the device shows, and what the owner compares and
    /// sends with `device_confirm`.
    pub confirm_code: Option<String>,
    /// With a new key waiting beside the device's confirmed one (a re-pair
    /// that kept the current key): the kept key's code. The kept key is
    /// served until the waiting one is confirmed, which ends it.
    pub kept_code: Option<String>,
    /// When the owner confirmed the key that works now (RFC 3339 UTC); with
    /// a key waiting, the kept key's confirmation.
    pub confirmed_at: Option<String>,
    /// When a used token of its came back with another key, and from where:
    /// if the device did not pair, someone else holds a certificate for the
    /// row (unconfirmed until the owner confirms it), so delete it.
    pub contested_at: Option<String>,
    pub contested_from: Option<String>,
    /// The certificate of the key that works now: its `notAfter`, and from
    /// when the card and the device say it lapses (two thirds of its
    /// validity), RFC 3339 UTC.
    pub cert_not_after: Option<String>,
    pub cert_notice_after: Option<String>,
    /// Its lmgw URLs, `https`, in the owner's order.
    pub urls: Vec<String>,
    /// Paired as "this computer".
    pub local_only: bool,
}

/// Where a device's pairing stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DevicePairingState {
    /// No key: waiting for its device to enrol with a token (or for a new
    /// token, when its last one ended).
    #[default]
    Waiting,
    /// A key enrolled and waiting for the owner's confirmation: its every
    /// request is refused 403 `device_unconfirmed` until then. A key kept
    /// beside it (`kept_code`) is served until the confirmation ends it.
    Enrolled,
    /// Enrolled and confirmed: the device is served.
    Confirmed,
    /// Paired with a link from before TLS: its bearer key no longer works,
    /// and it needs a new token.
    V1,
}

/// A device's latest pairing token: when it lapses, and what became of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PairingTokenView {
    /// When it was minted and when it lapses, RFC 3339 UTC.
    pub created_at: String,
    pub expires_at: String,
    /// When it was used, and from which address.
    pub used_at: Option<String>,
    pub used_from: Option<String>,
    /// When the owner ended it (closing the QR card, a new token).
    pub ended_at: Option<String>,
    /// Whether it is alive now: not lapsed, not ended. A used token stays
    /// alive until then, for its own key's repeat.
    pub alive: bool,
}

/// lmgw's device CA (device link design §2.1), as the status shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceCaStatus {
    /// Whether the CA exists and signs.
    pub state: DeviceCaState,
    /// `sha256:` and the lowercase hex SHA-256 of the CA certificate's
    /// SubjectPublicKeyInfo: the pairing link's `fp`. `null` while the CA is
    /// missing or its certificate cannot be read.
    pub fp: Option<String>,
    /// The CA's creation day (its `notBefore`), RFC 3339 UTC; `null` with
    /// `fp`.
    pub created: Option<String>,
    /// How the key file keeps the key; `null` while the CA is missing or the
    /// key file cannot be read.
    pub key_mode: Option<DeviceCaKeyMode>,
    /// Why the CA cannot be used, with state `error`.
    pub error: Option<DeviceCaError>,
    /// The key file's path (`tls/ca.key` in the data directory).
    pub key_file: String,
    /// The certificate's path (`tls/ca.crt` in the data directory), PEM,
    /// readable by anyone: what `curl --cacert` takes.
    pub cert_file: String,
    /// The CA certificate, PEM: what `curl --cacert` takes and the Devices
    /// card copies. Public like the file (`0644`) and the pin; `null` while
    /// the CA is missing or its certificate cannot be read.
    pub certificate_pem: Option<String>,
}

/// Whether lmgw's device CA exists and signs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeviceCaState {
    /// None exists: it is created when the device listener is first turned
    /// on.
    #[default]
    Missing,
    /// On disk and not loaded since this start (the device listener has not
    /// run): `fp`, `created` and `key_mode` are read from the files, the key
    /// itself is not opened.
    Stored,
    /// Loaded: its key is open and signs.
    Ready,
    /// It cannot be used; `error` says why.
    Error,
}

/// How `tls/ca.key` keeps the CA's key (device link design D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeviceCaKeyMode {
    /// Encrypted (AES-256-GCM) under a key held in the session's Secret
    /// Service.
    Wrapped,
    /// Plain PKCS#8, readable by its owner only: a start without a Secret
    /// Service (headless). The next start that reaches one wraps it.
    Plain,
}

/// Why lmgw's device CA cannot be used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceCaError {
    /// What kind of failure it is.
    pub kind: DeviceCaErrorKind,
    /// The reason in a sentence, naming the file, the mode found or the
    /// Secret Service's answer.
    pub message: String,
}

/// The kind of [`DeviceCaError`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeviceCaErrorKind {
    /// No CA exists, and the call needed one.
    #[default]
    Missing,
    /// The wallet's unlock prompt was declined.
    Locked,
    /// A file cannot be read, does not parse, or the key does not open or
    /// does not match the certificate.
    Unreadable,
    /// The key file is readable by group or others, not owned by lmgw's
    /// user, or a symbolic link.
    BadMode,
    /// The key is wrapped and no Secret Service answers this start.
    NoSecretService,
    /// The key is wrapped and the Secret Service holds no wrapping key for
    /// this data directory.
    WrapKeyMissing,
    /// The Secret Service answered with an error.
    SecretService,
}

/// One server certificate the device listener presents (device link design
/// §2.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceLeafStatus {
    /// Which connections it serves.
    pub slot: DeviceLeafSlot,
    /// The local address it serves, for `interface`; `null` for `loopback`
    /// and `shared`.
    pub address: Option<String>,
    /// When it was issued, RFC 3339 UTC.
    pub issued: String,
    /// Its `notAfter`, RFC 3339 UTC.
    pub valid_until: String,
    /// From when the next connection it would serve gets a new one: half its
    /// validity.
    pub renew_after: String,
    /// The names it carries (its subjectAltNames): its address, or the
    /// loopback addresses and `localhost`, and every host of every device's
    /// URL list.
    pub names: Vec<String>,
}

/// Which connections a server certificate serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeviceLeafSlot {
    /// Every connection to a loopback address (`127.0.0.0/8`, `::1`).
    #[default]
    Loopback,
    /// Connections to one address of this machine's interfaces.
    Interface,
    /// Connections to an address no interface lists: the URL-list hosts
    /// alone.
    Shared,
}

/// The device listener (device link design §4.1, §9.2), as the status
/// shows it: whether it serves, where, or why not, and what it holds open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceListenerStatus {
    /// Whether it serves.
    pub state: DeviceListenerState,
    /// The scope it serves, or tried to: `loopback`, `all` or an address
    /// (`devices.listen`); `null` while off.
    pub listen: Option<String>,
    /// The port it serves, or tried to (`devices.port`, or the HTTP port's
    /// number plus one); `null` while off.
    pub port: Option<u16>,
    /// What it is bound to, as `https://` base URLs: two for `loopback`
    /// (one when the machine has no IPv6), one otherwise. Empty unless
    /// `listening`.
    pub urls: Vec<String>,
    /// Why it does not serve, with state `error`: a bind error naming the
    /// address and the settings, the device CA's reason, or a port it cannot
    /// derive.
    pub error: Option<String>,
    /// Its open connections now, TLS handshakes included.
    pub connections: u64,
    /// The requests in flight on it now (an open Chat feed is one).
    pub requests: u64,
    /// The realtime sessions and turns running on it now.
    pub sessions: u64,
    /// Its connections that have not authenticated, now: handshakes in
    /// progress, connections without a certificate, and connections whose
    /// certificate opens no working device. What `devices.unauthenticated_max`
    /// and `devices.unauthenticated_per_peer` bound.
    pub unauthenticated: u64,
    /// Such connections it closed since it started to make room for a newer
    /// one, one of those two bounds reached (the oldest goes).
    pub evicted: u64,
}

/// Whether the device listener serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeviceListenerState {
    /// `devices.listener` is off, or the gateway is not serving.
    #[default]
    Off,
    /// It serves on `urls`.
    Listening,
    /// It is on and does not serve; `error` says why. The plain listener
    /// serves on.
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_confirmation_code_is_the_digest_s_first_four_bytes() {
        let mut digest = [0u8; 32];
        digest[..5].copy_from_slice(&[0x7f, 0x3a, 0x91, 0xc2, 0xff]);
        assert_eq!(confirm_code(&digest), "7F3A-91C2");
        assert_eq!(confirm_code(&[0; 32]), "0000-0000");
        assert!(unconfirmed_message("7F3A-91C2").contains("7F3A-91C2"));
    }

    /// The code as the owner types it from the device's screen reads as the
    /// card shows it; anything else is no code.
    #[test]
    fn a_typed_code_reads_in_the_code_s_form() {
        for typed in ["7F3A-91C2", "7f3a91c2", " 7f3a 91C2 ", "7F3A - 91C2"] {
            assert_eq!(
                parse_confirm_code(typed).as_deref(),
                Some("7F3A-91C2"),
                "{typed}"
            );
        }
        for typed in ["", "7F3A-91C", "7F3A-91C2-0", "7G3A-91C2", "７F3A-91C2"] {
            assert_eq!(parse_confirm_code(typed), None, "{typed}");
        }
    }

    #[test]
    fn the_enrolment_reads_as_the_design_writes_it() {
        let answer: Enrolment = serde_json::from_str(
            r#"{"device": {"id": 12, "name": "phone"},
                "certificate": "AAAA", "ca_certificate": "BBBB",
                "not_after": "2027-10-14T14:03:11Z", "notice_after": "2027-06-14T14:03:11Z",
                "urls": ["https://192.0.2.1:8788", "https://198.51.100.20:8788"],
                "confirm_code": "7F3A-91C2", "confirmed": false}"#,
        )
        .unwrap();
        assert_eq!(answer.device.id, 12);
        assert_eq!(answer.urls.len(), 2);
        // `confirmed` absent reads as not confirmed.
        let mut v = serde_json::to_value(&answer).unwrap();
        v.as_object_mut().unwrap().remove("confirmed");
        assert!(!serde_json::from_value::<Enrolment>(v).unwrap().confirmed);
    }

    #[test]
    fn the_request_is_strict_and_its_debug_redacts_the_token() {
        let r = EnrolRequest {
            token: format!("{TOKEN_PREFIX}{}", "4b1d".repeat(8)),
            public_key: "MFkw".into(),
            client: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("client"), "{json}");
        assert!(!format!("{r:?}").contains("4b1d"));
        let extra = r#"{"token": "t", "public_key": "k", "key": "x"}"#;
        assert!(serde_json::from_str::<EnrolRequest>(extra).is_err());
    }
}
