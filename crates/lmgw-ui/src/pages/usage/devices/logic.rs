//! The Devices card's decisions, apart from the views that show them (device
//! link design §3.2, §3.5, §4.1, §5.2, §5.3, §9.3): where the device is and
//! the URLs that follow, what the pairing form must turn on or widen, what a
//! row's pairing asks of the owner, the certificate's notice, each open
//! link in words. Pure, so the unit tests below hold them; the clock and the
//! local time zone come in as arguments.

use lmgw_api_types::device_link::{
    DeviceCaState, DeviceListenerState, DeviceListenerStatus, DevicePairingState,
    DevicePairingView, DevicesSettings,
};
use lmgw_api_types::{DeviceUrl, KeyRow, OpenLink};
use serde_json::{json, Value};

/// Where the device is (§3.2): the pairing form's first question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Where {
    /// This computer: `https://127.0.0.1:<port>` alone.
    Computer,
    /// Another device on the LAN: the machine's LAN addresses.
    Lan,
    /// Another device over WireGuard (or another point-to-point link).
    Wireguard,
}

impl Where {
    pub(super) const ALL: [Where; 3] = [Where::Computer, Where::Lan, Where::Wireguard];

    pub(super) fn label(self) -> &'static str {
        match self {
            Where::Computer => "this computer",
            Where::Lan => "the LAN",
            Where::Wireguard => "WireGuard",
        }
    }

    pub(super) fn key(self) -> &'static str {
        match self {
            Where::Computer => "computer",
            Where::Lan => "lan",
            Where::Wireguard => "wireguard",
        }
    }

    pub(super) fn from_key(key: &str) -> Where {
        Where::ALL
            .into_iter()
            .find(|w| w.key() == key)
            .unwrap_or(Where::Computer)
    }

    /// Paired as "this computer" (`local_only`, §4.1).
    pub(super) fn local_only(self) -> bool {
        self == Where::Computer
    }
}

/// The URL list lmgw proposes for `w` (§3.1): this computer's loopback
/// address alone; for another device the addresses it can count on, split
/// by the kind of interface. Empty when there is none, which the form says.
pub(super) fn proposal(w: Where, device_port: Option<u16>, urls: &[DeviceUrl]) -> Vec<String> {
    match w {
        Where::Computer => device_port
            .map(|p| vec![format!("https://127.0.0.1:{p}")])
            .unwrap_or_default(),
        Where::Lan => urls
            .iter()
            .filter(|u| !u.wireguard)
            .map(|u| u.url.clone())
            .collect(),
        Where::Wireguard => urls
            .iter()
            .filter(|u| u.wireguard)
            .map(|u| u.url.clone())
            .collect(),
    }
}

/// The note beside a proposed URL: its interface and which kind.
pub(super) fn url_note(u: &DeviceUrl) -> String {
    let kind = if u.wireguard { "WireGuard" } else { "LAN" };
    if u.interface.is_empty() {
        format!("{kind}, the address the listener is set to")
    } else {
        format!("{} · {kind}", u.interface)
    }
}

/// The URL list as typed, one URL a line, in the owner's order (§3.1):
/// each `https://` with a host, none twice. The gateway checks the rest
/// (what a server certificate may carry) and names what it refuses.
pub(in crate::pages::usage) fn parse_urls(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let rest = line
            .get(..8)
            .filter(|p| p.eq_ignore_ascii_case("https://"))
            .map(|_| &line[8..])
            .ok_or_else(|| format!("'{line}' is not an https URL: the device link is TLS only"))?;
        if rest.trim_end_matches('/').is_empty() || rest.starts_with('/') {
            return Err(format!("'{line}' has no host"));
        }
        if line.chars().any(char::is_whitespace) {
            return Err(format!("'{line}' has a space in it"));
        }
        let url = line.trim_end_matches('/').to_string();
        if out.iter().any(|u| u.eq_ignore_ascii_case(&url)) {
            return Err(format!("'{url}' is in the list twice"));
        }
        out.push(url);
    }
    if out.is_empty() {
        return Err("the device needs at least one URL to reach lmgw at".into());
    }
    Ok(out)
}

/// Whether `url` names this computer only (an IP is read as one, a name
/// only when it is `localhost`).
pub(super) fn url_is_loopback(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.to_canonical().is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// Whether a `devices.listen` scope reaches this computer alone.
pub(super) fn listen_is_loopback(listen: &str) -> bool {
    listen == "loopback"
        || listen
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// What the pairing form must do to the device listener before the device
/// can reach lmgw (§3.2, §4.1, D24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Need {
    /// It serves where the device reaches it.
    Nothing,
    /// It is off. `widen`: and a remote device needs it beyond loopback.
    TurnOn { widen: bool },
    /// It serves on loopback only and the device is another one.
    Widen,
}

pub(super) fn listener_need(s: &DevicesSettings, local_only: bool) -> Need {
    let remote_on_loopback = !local_only && listen_is_loopback(&s.listen);
    match (s.listener, remote_on_loopback) {
        (false, widen) => Need::TurnOn { widen },
        (true, true) => Need::Widen,
        (true, false) => Need::Nothing,
    }
}

impl Need {
    fn widens(&self) -> bool {
        matches!(self, Need::TurnOn { widen: true } | Need::Widen)
    }
}

/// The `settings_set_full` patch that does `need`: on, the scope it needs
/// (`all` for a remote device on a loopback scope; the saved scope
/// otherwise) and the port as it is, written as a number.
pub(super) fn need_patch(s: &DevicesSettings, need: &Need) -> Result<Value, String> {
    let port = s.port.or(s.port_effective).ok_or_else(|| {
        "the device listener has no port: set one under Settings → Network & access → Devices"
            .to_string()
    })?;
    let listen = if need.widens() {
        "all".to_string()
    } else {
        s.listen.clone()
    };
    Ok(json!({ "devices": { "listener": true, "listen": listen, "port": port } }))
}

/// What the confirmation says (§4.1): the exposure, in the settings
/// card's own words.
pub(super) fn need_text(s: &DevicesSettings, need: &Need) -> Vec<String> {
    let port = s.port.or(s.port_effective).map(|p| p.to_string());
    let port = port.as_deref().unwrap_or("(unset)");
    let mut out = Vec::new();
    match need {
        Need::Nothing => {}
        Need::TurnOn { widen: false } => out.push(format!(
            "The device listener is off. Pairing turns it on, on this computer only \
             (127.0.0.1 and [::1]), port {port}. The first start makes lmgw's device CA."
        )),
        Need::TurnOn { widen: true } => {
            out.push(format!(
                "The device listener is off. Pairing turns it on for every network address, \
                 port {port}. The first start makes lmgw's device CA."
            ));
            out.push(EXPOSURE.into());
        }
        Need::Widen => {
            out.push(format!(
                "The device listener serves this computer only. A device on another machine \
                 needs it on every network address, port {port}."
            ));
            out.push(EXPOSURE.into());
        }
    }
    out
}

/// §4.1's warning, as the Settings card words it.
const EXPOSURE: &str = "On Fedora's default desktop zone, TCP and UDP ports 1025–65535 are open, \
    so devices on your LAN can reach it at once; they get nothing without a pairing token and \
    your confirmation, or a certificate.";

/// What declining the widening leaves (§4.1): the row is written, and the
/// device will not reach lmgw.
pub(super) const DECLINED: &str = "Without widening, the device cannot reach lmgw: the listener \
    stays on this computer only until you widen it in Settings → Network & access → Devices.";

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// An RFC 3339 timestamp (`2026-10-15T14:32:00Z`, a fraction or an offset
/// accepted) as Unix seconds.
pub(super) fn parse_utc_secs(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(f) = rest.strip_prefix('.') {
        let digits = f.bytes().take_while(u8::is_ascii_digit).count();
        frac = format!("0.{}", &f[..digits]).parse::<f64>().ok()?;
        rest = &f[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && matches!(o.as_bytes()[0], b'+' | b'-') && o.as_bytes()[3] == b':' => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o.get(1..3)?.parse::<i64>().ok()? * 3600
                + o.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    // Howard Hinnant's days-from-civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + sec - offset) as f64 + frac)
}

/// The whole minutes a token has left at `now_s` (rounded up: "1 min left"
/// until it lapses), `None` once it has lapsed.
pub(super) fn minutes_left(expires_at: &str, now_s: f64) -> Option<u64> {
    let left = parse_utc_secs(expires_at)? - now_s;
    (left > 0.0).then(|| (left / 60.0).ceil() as u64)
}

/// A timestamp as the card prints a date: `2027-10-14`.
pub(super) fn date_of(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

// ---------------------------------------------------------------------------
// A row's pairing
// ---------------------------------------------------------------------------

/// What a device row's pairing asks of the owner on the card (§3.5, §9.3):
/// a key waiting for its Confirm, and a pairing code still alive, which the
/// owner may end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PairingAsk {
    pub confirm: Option<Awaiting>,
    /// Until when the pairing code (the link's token) is alive (RFC 3339).
    pub token_until: Option<String>,
}

/// A key waiting for the owner's Confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Awaiting {
    /// Its code as lmgw derived it from the key, `7F3A-91C2`.
    pub code: String,
    /// The peer address it enrolled from.
    pub from: Option<String>,
    /// Where a used code came back from with another key (§3.3): then the
    /// card shows no code to click through, and the owner types the one the
    /// device shows.
    pub contested_from: Option<String>,
    /// It replaces a key the device kept (a re-pair that kept the current
    /// key, §5.2).
    pub replaces: bool,
}

/// What `r`'s pairing asks of the owner now; `None` when nothing.
pub(super) fn pairing_ask(r: &KeyRow) -> Option<PairingAsk> {
    let p = r.pairing.as_ref()?;
    let confirm = awaiting(p);
    let token_until = p
        .token
        .as_ref()
        .filter(|t| t.alive)
        .map(|t| t.expires_at.clone());
    (confirm.is_some() || token_until.is_some()).then_some(PairingAsk {
        confirm,
        token_until,
    })
}

/// The key waiting for its Confirm, if one does.
pub(super) fn awaiting(p: &DevicePairingView) -> Option<Awaiting> {
    match (p.state, p.confirm_code.as_ref()) {
        (DevicePairingState::Enrolled, Some(code)) => Some(Awaiting {
            code: code.clone(),
            from: p.enrolled_from.clone(),
            contested_from: p
                .contested_at
                .as_ref()
                .map(|_| p.contested_from.clone().unwrap_or_default()),
            replaces: p.kept_code.is_some(),
        }),
        _ => None,
    }
}

/// `device_confirm`'s body for device `id`'s waiting key `a`: the code the
/// card showed, which the key must still have (a stale card confirms
/// nothing); for a contested row the code the owner typed from the device's
/// screen, refused here when it is none.
pub(super) fn confirm_body(id: i64, a: &Awaiting, typed: &str) -> Result<Value, &'static str> {
    let code = if a.contested_from.is_some() {
        lmgw_api_types::device_link::parse_confirm_code(typed)
            .ok_or("type the code the device shows: eight hex digits, like 7F3A-91C2")?
    } else {
        a.code.clone()
    };
    Ok(json!({ "id": id, "confirm_code": code }))
}

/// What the QR card shows (§3.5): the code to scan, then, when the device
/// enrols, the confirmation, and when it is confirmed, the end of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// Waiting for the device to use the link.
    Waiting,
    /// The device enrolled: "Does the device show this code?".
    Confirm(Awaiting),
    /// Enrolled and confirmed (or a renewal that needed no confirmation).
    Paired,
    /// The token ended or lapsed before the device used it.
    Ended,
}

/// The card's phase from the device's pairing. `expires_at` is the token the
/// card shows (the answer's): a list that has not caught up yet, still
/// holding an earlier token, leaves the card waiting.
pub(super) fn card_phase(p: Option<&DevicePairingView>, expires_at: &str) -> Phase {
    let Some(p) = p else { return Phase::Waiting };
    let ours = p.token.as_ref().filter(|t| t.expires_at == expires_at);
    let Some(t) = ours else {
        return Phase::Waiting;
    };
    if let Some(a) = awaiting(p) {
        return Phase::Confirm(a);
    }
    if t.used_at.is_some() {
        return if p.state == DevicePairingState::Confirmed {
            Phase::Paired
        } else {
            Phase::Ended
        };
    }
    if t.alive {
        Phase::Waiting
    } else {
        Phase::Ended
    }
}

/// The contested notice (§3.3).
pub(super) fn contested_words(name: &str, from: &str) -> String {
    let again = if from.is_empty() {
        "presented again by another key".to_string()
    } else {
        format!("presented again from {from} by another key")
    };
    format!("the token for '{name}' was used, then {again}: if your device did not pair, delete it")
}

/// The row's pairing in one line (§9.3). `at` prints an RFC 3339 time in
/// the reader's own zone.
pub(super) fn pairing_words(p: &DevicePairingView, at: &dyn Fn(&str) -> String) -> String {
    let from = |f: &Option<String>| f.as_ref().map(|f| format!(" from {f}")).unwrap_or_default();
    match p.state {
        DevicePairingState::V1 => "paired before TLS; its old key opens nothing".into(),
        DevicePairingState::Waiting => match &p.token {
            Some(t) if t.alive => {
                format!(
                    "waiting for its device, token valid until {}",
                    at(&t.expires_at)
                )
            }
            Some(t) if t.ended_at.is_some() => "token ended unused".into(),
            Some(t) => format!("token expired unused ({})", at(&t.expires_at)),
            None => "no pairing token".into(),
        },
        DevicePairingState::Enrolled => {
            let code = p.confirm_code.clone().unwrap_or_default();
            let lead = if p.kept_code.is_some() {
                "a new key enrolled"
            } else {
                "enrolled"
            };
            format!("{lead}{}, code {code}", from(&p.enrolled_from))
        }
        DevicePairingState::Confirmed => {
            let when = p.enrolled_at.as_deref().map(date_of).unwrap_or("");
            let client = p
                .client
                .as_ref()
                .map(|c| format!(": {c}"))
                .unwrap_or_default();
            format!("paired {when}{}{client}, confirmed", from(&p.enrolled_from))
        }
    }
}

/// How a device's certificate stands (§5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cert {
    /// None yet (no key enrolled, or a v1 pairing).
    None,
    Fine {
        until: String,
    },
    /// From `notice_after`: pair it again before `until`.
    Notice {
        until: String,
    },
    Lapsed {
        until: String,
    },
}

pub(super) fn cert_state(p: &DevicePairingView, now_s: f64) -> Cert {
    let Some(not_after) = p.cert_not_after.as_deref() else {
        return Cert::None;
    };
    let until = date_of(not_after).to_string();
    let past = |ts: &str| parse_utc_secs(ts).is_some_and(|t| t <= now_s);
    if past(not_after) {
        Cert::Lapsed { until }
    } else if p.cert_notice_after.as_deref().is_some_and(past) {
        Cert::Notice { until }
    } else {
        Cert::Fine { until }
    }
}

impl Cert {
    pub(super) fn words(&self) -> Option<String> {
        match self {
            Cert::None => None,
            Cert::Fine { until } => Some(format!("certificate until {until}")),
            Cert::Notice { until } => Some(format!("lapses on {until}: pair it again before then")),
            Cert::Lapsed { until } => Some(format!("certificate lapsed on {until}: pair it again")),
        }
    }

    pub(super) fn warns(&self) -> bool {
        matches!(self, Cert::Notice { .. } | Cert::Lapsed { .. })
    }
}

/// An open connection in words (§9.3): "feed (keep-alive 120 s, since
/// 09:00, from 192.0.2.2)", and a voice link that is a WebRTC call says so
/// (calls design §9): "voice (WebRTC call, since 09:00, from 192.0.2.2)".
/// `at` prints an RFC 3339 time as a local clock time.
pub(super) fn link_words(kind: &str, l: &OpenLink, at: &dyn Fn(&str) -> String) -> String {
    let mut parts: Vec<String> = Vec::new();
    if l.call.is_some() {
        parts.push("WebRTC call".to_string());
    }
    if let Some(k) = l.keepalive_s {
        parts.push(format!("keep-alive {k} s"));
    }
    if !l.since.is_empty() {
        parts.push(format!("since {}", at(&l.since)));
    }
    if let Some(p) = l.peer.as_deref().filter(|p| !p.is_empty()) {
        parts.push(format!("from {p}"));
    }
    if parts.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} ({})", parts.join(", "))
    }
}

/// Every open link of a row, one line each, the kinds in the row's order.
pub(super) fn links_words(r: &KeyRow, at: &dyn Fn(&str) -> String) -> Vec<String> {
    r.open_links
        .iter()
        .flat_map(|g| g.links.iter().map(|l| link_words(&g.kind, l, at)))
        .collect()
}

/// How many rows still hold a pairing from before TLS (the strip's count).
pub(super) fn v1_count(keys: &[KeyRow]) -> usize {
    keys.iter()
        .filter(|r| r.kind == "device")
        .filter(|r| {
            r.pairing
                .as_ref()
                .is_some_and(|p| p.state == DevicePairingState::V1)
        })
        .count()
}

/// The devices holding a key of their own (waiting for its Confirm or
/// confirmed), by name without `device:`: whom a rotation of the CA resets
/// (§5.5).
pub(super) fn paired_names(keys: &[KeyRow]) -> Vec<String> {
    keys.iter()
        .filter(|r| r.kind == "device")
        .filter(|r| {
            r.pairing.as_ref().is_some_and(|p| {
                matches!(
                    p.state,
                    DevicePairingState::Enrolled | DevicePairingState::Confirmed
                )
            })
        })
        .map(|r| {
            r.name
                .strip_prefix("device:")
                .unwrap_or(&r.name)
                .to_string()
        })
        .collect()
}

/// Whether a re-pair has a working key to keep or end: the two choices
/// (§5.2) mean something only then.
pub(super) fn has_working_key(p: &DevicePairingView) -> bool {
    match p.state {
        DevicePairingState::Confirmed => true,
        // A key beside a kept one: the kept one works.
        DevicePairingState::Enrolled => p.kept_code.is_some(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The link strip
// ---------------------------------------------------------------------------

/// The fingerprint shortened for a line: `sha256:9f2c1a7b…41ab`.
pub(super) fn fp_short(fp: &str) -> String {
    let (algo, hex) = fp.split_once(':').unwrap_or(("", fp));
    if hex.len() <= 16 {
        return fp.to_string();
    }
    let head = &hex[..12];
    let tail = &hex[hex.len() - 4..];
    if algo.is_empty() {
        format!("{head}…{tail}")
    } else {
        format!("{algo}:{head}…{tail}")
    }
}

/// How bad the listener's line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Ok,
    Off,
    Warn,
}

/// The listener in a line: its scope, port, and what it holds.
pub(super) fn listener_words(s: &DeviceListenerStatus) -> (String, Tone) {
    match s.state {
        DeviceListenerState::Off => ("device listener off".into(), Tone::Off),
        DeviceListenerState::Error => (
            format!(
                "device listener not serving: {}",
                s.error.clone().unwrap_or_default()
            ),
            Tone::Warn,
        ),
        DeviceListenerState::Listening => {
            let port = s.port.map(|p| format!(", port {p}")).unwrap_or_default();
            let scope = match s.listen.as_deref() {
                Some("all") => "every address".to_string(),
                Some("loopback") | None => "this computer only".to_string(),
                Some(a) if listen_is_loopback(a) => format!("{a} (this computer only)"),
                Some(a) => a.to_string(),
            };
            (
                format!(
                    "device listener on: {scope}{port} · {} connection(s)",
                    s.connections
                ),
                Tone::Ok,
            )
        }
    }
}

/// The CA's key mode and state in words.
pub(super) fn ca_words(state: DeviceCaState, wrapped: Option<bool>) -> &'static str {
    match (state, wrapped) {
        (DeviceCaState::Missing, _) => "not made yet",
        (_, Some(true)) => "key wrapped in the Secret Service",
        (_, Some(false)) => "key plain (no Secret Service at its start)",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::device_link::PairingTokenView;

    fn url(u: &str, interface: &str, wireguard: bool) -> DeviceUrl {
        DeviceUrl {
            url: u.into(),
            interface: interface.into(),
            wireguard,
        }
    }

    /// §3.1/§3.2: "where" picks the proposal; this computer is the loopback
    /// address alone, the others split by the kind of interface.
    #[test]
    fn where_picks_the_proposed_urls() {
        let urls = [
            url("https://198.51.100.20:8788", "eth0", false),
            url("https://192.0.2.1:8788", "wg0", true),
            url("https://[2001:db8::1]:8788", "wg0", true),
        ];
        assert_eq!(
            proposal(Where::Computer, Some(8788), &urls),
            ["https://127.0.0.1:8788"]
        );
        assert!(proposal(Where::Computer, None, &urls).is_empty());
        assert_eq!(
            proposal(Where::Lan, Some(8788), &urls),
            ["https://198.51.100.20:8788"]
        );
        assert_eq!(
            proposal(Where::Wireguard, Some(8788), &urls),
            ["https://192.0.2.1:8788", "https://[2001:db8::1]:8788"]
        );
        assert!(proposal(Where::Wireguard, Some(8788), &urls[..1]).is_empty());
        assert!(Where::Computer.local_only() && !Where::Lan.local_only());
        for w in Where::ALL {
            assert_eq!(Where::from_key(w.key()), w);
        }
        assert_eq!(url_note(&urls[1]), "wg0 · WireGuard");
        assert_eq!(
            url_note(&url("https://203.0.113.5:1", "", false)),
            "LAN, the address the listener is set to"
        );
    }

    #[test]
    fn the_typed_url_list_keeps_the_owner_s_order_and_refuses_what_cannot_work() {
        assert_eq!(
            parse_urls(" https://192.0.2.1:8788/ \n\nhttps://phone.example.net\n").unwrap(),
            ["https://192.0.2.1:8788", "https://phone.example.net"]
        );
        for (bad, why) in [
            ("", "at least one"),
            ("   \n", "at least one"),
            ("http://192.0.2.1:8788", "TLS only"),
            ("192.0.2.1:8788", "not an https URL"),
            ("https://", "no host"),
            ("https:///x", "no host"),
            ("https://a b", "space"),
            ("https://192.0.2.1:1\nhttps://192.0.2.1:1/", "twice"),
        ] {
            let e = parse_urls(bad).unwrap_err();
            assert!(e.contains(why), "{bad:?}: {e}");
        }
        assert!(url_is_loopback("https://127.0.0.1:8788"));
        assert!(url_is_loopback("https://[::1]:8788"));
        assert!(!url_is_loopback("https://127.example.net"));
        assert!(!url_is_loopback("https://192.0.2.1:8788"));
    }

    fn settings(listener: bool, listen: &str) -> DevicesSettings {
        DevicesSettings {
            listener,
            listen: listen.into(),
            port: None,
            port_effective: Some(8788),
            ..Default::default()
        }
    }

    /// D24: a remote device on a loopback listener asks to widen, with the
    /// exposure said; this computer asks for nothing beyond turning it on.
    #[test]
    fn the_pairing_form_asks_to_turn_on_or_widen_the_listener() {
        use Need::*;
        assert_eq!(
            listener_need(&settings(false, "loopback"), true),
            TurnOn { widen: false }
        );
        assert_eq!(
            listener_need(&settings(false, "loopback"), false),
            TurnOn { widen: true }
        );
        assert_eq!(
            listener_need(&settings(false, "all"), false),
            TurnOn { widen: false }
        );
        assert_eq!(listener_need(&settings(true, "loopback"), true), Nothing);
        assert_eq!(listener_need(&settings(true, "loopback"), false), Widen);
        assert_eq!(listener_need(&settings(true, "127.0.0.2"), false), Widen);
        assert_eq!(listener_need(&settings(true, "all"), false), Nothing);
        assert_eq!(listener_need(&settings(true, "192.0.2.1"), false), Nothing);

        let s = settings(true, "loopback");
        assert_eq!(
            need_patch(&s, &Widen).unwrap(),
            json!({"devices": {"listener": true, "listen": "all", "port": 8788}})
        );
        // Turning on for this computer keeps the saved scope.
        let off = settings(false, "loopback");
        assert_eq!(
            need_patch(&off, &TurnOn { widen: false }).unwrap()["devices"]["listen"],
            "loopback"
        );
        // An explicit port wins over the effective one.
        let set = DevicesSettings {
            port: Some(9000),
            ..off.clone()
        };
        assert_eq!(need_patch(&set, &Widen).unwrap()["devices"]["port"], 9000);
        let none = DevicesSettings {
            port_effective: None,
            ..off
        };
        assert!(need_patch(&none, &Widen).unwrap_err().contains("no port"));

        let text = need_text(&s, &Widen).join(" ");
        assert!(text.contains("every network address, port 8788"), "{text}");
        assert!(text.contains("1025–65535"), "the exposure is named: {text}");
        let text = need_text(&settings(false, "loopback"), &TurnOn { widen: false }).join(" ");
        assert!(
            text.contains("this computer only") && !text.contains("1025"),
            "{text}"
        );
        assert!(need_text(&s, &Nothing).is_empty());
    }

    #[test]
    fn timestamps_read_as_the_gateway_writes_them() {
        assert_eq!(parse_utc_secs("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(
            parse_utc_secs("2026-10-15T14:32:00Z"),
            Some(1_792_074_720.0)
        );
        assert_eq!(
            parse_utc_secs("2024-02-29T00:00:00Z"),
            Some(1_709_164_800.0)
        );
        assert_eq!(
            parse_utc_secs("2026-10-15T16:32:00+02:00"),
            parse_utc_secs("2026-10-15T14:32:00Z")
        );
        assert_eq!(
            parse_utc_secs("2026-10-15T14:32:00.5Z"),
            Some(1_792_074_720.5)
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-10-15T25:00:00Z",
        ] {
            assert_eq!(parse_utc_secs(bad), None, "{bad}");
        }
        let exp = "2026-10-15T14:32:00Z";
        let at = parse_utc_secs(exp).unwrap();
        assert_eq!(minutes_left(exp, at - 9.0 * 60.0), Some(9));
        assert_eq!(minutes_left(exp, at - 30.0), Some(1));
        assert_eq!(minutes_left(exp, at), None);
        assert_eq!(minutes_left(exp, at + 60.0), None);
        assert_eq!(minutes_left("soon", 0.0), None);
        assert_eq!(date_of("2027-10-14T14:03:11Z"), "2027-10-14");
    }

    fn token(alive: bool) -> PairingTokenView {
        PairingTokenView {
            expires_at: "2026-10-15T14:32:00Z".into(),
            alive,
            ..Default::default()
        }
    }

    fn row(p: DevicePairingView) -> KeyRow {
        KeyRow {
            id: 7,
            kind: "device".into(),
            pairing: Some(p),
            ..Default::default()
        }
    }

    /// §3.5: an enrolled key shows its code and is confirmed with that
    /// code; a contested one with the code the owner types from the device;
    /// a pairing code alive can be ended.
    #[test]
    fn a_key_waiting_for_its_confirmation_is_confirmed_with_its_code() {
        let enrolled = DevicePairingView {
            state: DevicePairingState::Enrolled,
            confirm_code: Some("7F3A-91C2".into()),
            enrolled_from: Some("192.0.2.2".into()),
            token: Some(token(true)),
            ..Default::default()
        };
        let ask = pairing_ask(&row(enrolled.clone())).unwrap();
        let a = ask.confirm.clone().unwrap();
        assert_eq!(
            (a.code.as_str(), a.from.as_deref(), a.replaces),
            ("7F3A-91C2", Some("192.0.2.2"), false)
        );
        assert_eq!(ask.token_until.as_deref(), Some("2026-10-15T14:32:00Z"));
        // The code the card showed, whatever was typed.
        assert_eq!(
            confirm_body(7, &a, "").unwrap(),
            json!({ "id": 7, "confirm_code": "7F3A-91C2" })
        );

        // Beside a kept key: the waiting one's code, said to replace it.
        let beside = DevicePairingView {
            kept_code: Some("0000-0001".into()),
            ..enrolled.clone()
        };
        assert!(pairing_ask(&row(beside)).unwrap().confirm.unwrap().replaces);

        // Contested: the owner types the device's code; none is refused.
        let contested = DevicePairingView {
            contested_at: Some("2026-10-15T14:30:00Z".into()),
            contested_from: Some("203.0.113.5".into()),
            ..enrolled.clone()
        };
        let a = pairing_ask(&row(contested)).unwrap().confirm.unwrap();
        assert_eq!(a.contested_from.as_deref(), Some("203.0.113.5"));
        assert!(confirm_body(7, &a, "").is_err());
        assert!(confirm_body(7, &a, "7F3A").is_err());
        assert_eq!(
            confirm_body(7, &a, " 1234 abcd ").unwrap(),
            json!({ "id": 7, "confirm_code": "1234-ABCD" })
        );
        assert!(
            contested_words("phone", "203.0.113.5").contains("presented again from 203.0.113.5")
        );
        assert!(contested_words("phone", "").contains("another key"));

        // Waiting for its device: only the code's end; confirmed with no
        // code alive: nothing.
        let waiting = DevicePairingView {
            state: DevicePairingState::Waiting,
            token: Some(token(true)),
            ..Default::default()
        };
        let ask = pairing_ask(&row(waiting)).unwrap();
        assert!(ask.confirm.is_none() && ask.token_until.is_some());
        let confirmed = DevicePairingView {
            state: DevicePairingState::Confirmed,
            confirm_code: Some("7F3A-91C2".into()),
            token: Some(token(false)),
            ..Default::default()
        };
        assert_eq!(pairing_ask(&row(confirmed)), None);
        assert_eq!(pairing_ask(&KeyRow::default()), None);
    }

    /// §3.5: the QR card is the code, then the confirmation, then done.
    #[test]
    fn the_qr_card_follows_the_row_from_the_code_to_the_confirmation() {
        const EXP: &str = "2026-10-15T14:32:00Z";
        // Nothing in the list yet, or an earlier token still: waiting.
        assert_eq!(card_phase(None, EXP), Phase::Waiting);
        let old = DevicePairingView {
            state: DevicePairingState::Confirmed,
            token: Some(PairingTokenView {
                expires_at: "2026-10-14T10:00:00Z".into(),
                used_at: Some("2026-10-14T09:55:00Z".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(card_phase(Some(&old), EXP), Phase::Waiting);
        // A re-pair that keeps the key: confirmed, but this token unused.
        let kept = DevicePairingView {
            token: Some(token(true)),
            ..old.clone()
        };
        assert_eq!(card_phase(Some(&kept), EXP), Phase::Waiting);
        // Enrolled: the code to compare.
        let enrolled = DevicePairingView {
            state: DevicePairingState::Enrolled,
            confirm_code: Some("7F3A-91C2".into()),
            enrolled_from: Some("192.0.2.2".into()),
            token: Some(PairingTokenView {
                used_at: Some("2026-10-15T14:25:00Z".into()),
                ..token(true)
            }),
            ..Default::default()
        };
        let Phase::Confirm(a) = card_phase(Some(&enrolled), EXP) else {
            panic!("enrolled asks the owner")
        };
        assert_eq!(a.code, "7F3A-91C2");
        // Confirmed; the token ends at the device's first request.
        let done = DevicePairingView {
            state: DevicePairingState::Confirmed,
            token: Some(PairingTokenView {
                used_at: Some("2026-10-15T14:25:00Z".into()),
                ..token(false)
            }),
            ..Default::default()
        };
        assert_eq!(card_phase(Some(&done), EXP), Phase::Paired);
        // Ended or lapsed unused.
        let ended = DevicePairingView {
            token: Some(token(false)),
            ..Default::default()
        };
        assert_eq!(card_phase(Some(&ended), EXP), Phase::Ended);
    }

    #[test]
    fn a_row_s_pairing_reads_in_one_line() {
        let at = |t: &str| t.get(11..16).unwrap_or(t).to_string();
        let waiting = DevicePairingView {
            token: Some(token(true)),
            ..Default::default()
        };
        assert_eq!(
            pairing_words(&waiting, &at),
            "waiting for its device, token valid until 14:32"
        );
        let ended = DevicePairingView {
            token: Some(PairingTokenView {
                ended_at: Some("2026-10-15T14:00:00Z".into()),
                ..token(false)
            }),
            ..Default::default()
        };
        assert_eq!(pairing_words(&ended, &at), "token ended unused");
        let lapsed = DevicePairingView {
            token: Some(token(false)),
            ..Default::default()
        };
        assert!(pairing_words(&lapsed, &at).starts_with("token expired unused"));
        assert_eq!(
            pairing_words(&DevicePairingView::default(), &at),
            "no pairing token"
        );
        let enrolled = DevicePairingView {
            state: DevicePairingState::Enrolled,
            confirm_code: Some("7F3A-91C2".into()),
            enrolled_from: Some("192.0.2.2".into()),
            ..Default::default()
        };
        assert_eq!(
            pairing_words(&enrolled, &at),
            "enrolled from 192.0.2.2, code 7F3A-91C2"
        );
        let confirmed = DevicePairingView {
            state: DevicePairingState::Confirmed,
            enrolled_at: Some("2026-10-14T14:03:11Z".into()),
            enrolled_from: Some("192.0.2.2".into()),
            client: Some("Kai mobile 0.1.0 (Android 16)".into()),
            ..Default::default()
        };
        assert_eq!(
            pairing_words(&confirmed, &at),
            "paired 2026-10-14 from 192.0.2.2: Kai mobile 0.1.0 (Android 16), confirmed"
        );
        let v1 = DevicePairingView {
            state: DevicePairingState::V1,
            ..Default::default()
        };
        assert!(pairing_words(&v1, &at).contains("before TLS"));
        assert_eq!(
            v1_count(&[row(v1), row(confirmed.clone()), KeyRow::default()]),
            1
        );
        let named = |n: &str, p: &DevicePairingView| KeyRow {
            name: n.into(),
            ..row(p.clone())
        };
        assert_eq!(
            paired_names(&[
                named("device:phone", &confirmed),
                named("device:new", &waiting),
                named("device:tab", &enrolled),
                KeyRow::default()
            ]),
            ["phone", "tab"]
        );

        // A re-pair offers its choices only where a key works.
        assert!(has_working_key(&confirmed));
        let beside = DevicePairingView {
            state: DevicePairingState::Enrolled,
            kept_code: Some("0000-0001".into()),
            ..Default::default()
        };
        assert!(has_working_key(&beside));
        assert!(!has_working_key(&enrolled) && !has_working_key(&waiting));
    }

    /// §5.3: a notice from `notice_after`, in amber.
    #[test]
    fn the_certificate_s_notice_comes_from_two_thirds_of_its_life() {
        let p = DevicePairingView {
            cert_not_after: Some("2027-10-14T14:03:11Z".into()),
            cert_notice_after: Some("2027-06-14T14:03:11Z".into()),
            ..Default::default()
        };
        let t = |s: &str| parse_utc_secs(s).unwrap();
        let fine = cert_state(&p, t("2026-10-15T00:00:00Z"));
        assert_eq!(
            fine,
            Cert::Fine {
                until: "2027-10-14".into()
            }
        );
        assert_eq!(fine.words().unwrap(), "certificate until 2027-10-14");
        assert!(!fine.warns());
        let notice = cert_state(&p, t("2027-07-01T00:00:00Z"));
        assert!(notice.warns());
        assert_eq!(
            notice.words().unwrap(),
            "lapses on 2027-10-14: pair it again before then"
        );
        let lapsed = cert_state(&p, t("2027-10-15T00:00:00Z"));
        assert!(lapsed.warns() && lapsed.words().unwrap().contains("lapsed"));
        assert_eq!(cert_state(&DevicePairingView::default(), 0.0), Cert::None);
        assert_eq!(Cert::None.words(), None);
    }

    #[test]
    fn each_open_link_is_one_line() {
        let at = |t: &str| t.get(11..16).unwrap_or(t).to_string();
        let r = KeyRow {
            open_links: vec![
                lmgw_api_types::OpenLinks {
                    kind: "feed".into(),
                    count: 2,
                    links: vec![
                        OpenLink {
                            since: "2026-10-15T09:00:00Z".into(),
                            peer: Some("192.0.2.2:51514".into()),
                            keepalive_s: Some(120),
                            call: None,
                        },
                        OpenLink {
                            since: "2026-10-15T09:00:00Z".into(),
                            ..Default::default()
                        },
                    ],
                },
                lmgw_api_types::OpenLinks {
                    kind: "voice".into(),
                    count: 2,
                    links: vec![
                        OpenLink {
                            since: "2026-10-15T09:30:00Z".into(),
                            peer: Some("192.0.2.2:40312".into()),
                            call: Some("rtc_4f1".into()),
                            ..Default::default()
                        },
                        OpenLink {
                            since: "2026-10-15T09:31:00Z".into(),
                            ..Default::default()
                        },
                    ],
                },
                lmgw_api_types::OpenLinks {
                    kind: "tools".into(),
                    count: 1,
                    links: vec![OpenLink::default()],
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            links_words(&r, &at),
            [
                "feed (keep-alive 120 s, since 09:00, from 192.0.2.2:51514)",
                "feed (since 09:00)",
                "voice (WebRTC call, since 09:30, from 192.0.2.2:40312)",
                "voice (since 09:31)",
                "tools"
            ]
        );
    }

    #[test]
    fn the_strip_says_the_listener_and_the_pin_in_a_line() {
        assert_eq!(
            fp_short(&format!("sha256:{}", "9f2c".repeat(16))),
            "sha256:9f2c9f2c9f2c…9f2c"
        );
        assert_eq!(fp_short("sha256:abc"), "sha256:abc");
        let on = DeviceListenerStatus {
            state: DeviceListenerState::Listening,
            listen: Some("all".into()),
            port: Some(8788),
            connections: 3,
            ..Default::default()
        };
        let (line, tone) = listener_words(&on);
        assert_eq!(tone, Tone::Ok);
        assert_eq!(
            line,
            "device listener on: every address, port 8788 · 3 connection(s)"
        );
        let lo = DeviceListenerStatus {
            listen: Some("loopback".into()),
            ..on.clone()
        };
        assert!(listener_words(&lo).0.contains("this computer only"));
        let bad = DeviceListenerStatus {
            state: DeviceListenerState::Error,
            error: Some("address in use".into()),
            ..Default::default()
        };
        assert_eq!(listener_words(&bad).1, Tone::Warn);
        assert!(listener_words(&bad).0.contains("address in use"));
        assert_eq!(
            listener_words(&DeviceListenerStatus::default()).1,
            Tone::Off
        );
        assert!(ca_words(DeviceCaState::Ready, Some(true)).contains("wrapped"));
        assert!(ca_words(DeviceCaState::Ready, Some(false)).contains("plain"));
    }
}
