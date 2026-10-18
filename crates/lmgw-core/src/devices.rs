//! Device keys (client-apps design §1): a paired client app's credential.
//!
//! A device is an `api_keys` row of kind `device`, named `device:<name>`,
//! stored as a hash only (L1). It holds `Chat` on top of a client key's
//! `Inference` (`principal::Principal::holds`), and its policy applies whole.
//! What is specific to a device lives here:
//!
//! - **pairing** (`pairing`, `tokens`, `enrol`; device link design §3): the
//!   `lmgw-pair:` link v2 that carries a one-time pairing token once, the
//!   token's single use and its failed uses counted, and `POST
//!   /devices/enrol`, where the device sends its public key and gets its
//!   certificate, to be served once the owner confirms its code;
//! - **its connections** (`links`): which of its long-lived connections are
//!   open now — the Devices card's "online (feed, voice, tools)" — and the
//!   `last_seen_at` stamp written as one opens or closes (L15);
//! - **revocation** (`revoke`): one signal keyed by key id that every such
//!   connection watches, raised by Disable, Rotate and Delete, and by the
//!   key's own `expires_at` (L17, L18, §1.6). It is raised for **every** key
//!   kind, not only devices (*changed 2026-10-06*: Disable means disable): a
//!   realtime session of any key closes with 4003 on it, a Chat stream ends
//!   with a `revoked` error frame, and the Chat feed (`revoked`) and the host
//!   link watch the same signal when they exist. Only a device's connections
//!   count as "online" and stamp `last_seen_at`.

/// `POST /devices/enrol`: a device's public key for its certificate (device
/// link design §3.4).
mod enrol;
pub mod grant_audit;
/// Which device row a client certificate's key opens (device link design
/// §4.7).
mod keys;
mod links;
mod minted;
mod pairing;
/// Whether a pairing token is pending: what each device listener handshake
/// asks (device link design §4.2).
mod pending;
mod revoke;
/// The device listener's own `/devices/*` routes and their body limit
/// (device link design §4.6).
mod routes;
mod stream;
/// lmgw's device CA and the certificates it issues (device link design §2).
pub mod tls;
/// Pairing tokens and their failed uses (device link design §3.3).
pub mod tokens;

pub use keys::{DeviceKeys, KeyLookup, KeyMatch, MemoryKeys, StoredKeys};
pub use links::{LinkDetail, LinkGuard, LinkKind, Links};
pub use minted::MintedLinks;
pub use pairing::{
    all_loopback, check_urls, device_tools_grant, label_refusal, pairing_link, proposed_for_device,
    proposed_urls, Proposed, KEY_PREFIX, LOOPBACK_NOTE, NAME_PREFIX,
};
pub(crate) use pairing::{is_invisible, namespaces_overlap};
pub use pending::PendingTokens;
pub use revoke::{RevocationWatch, Revocations, RevokeReason, RECHECK};
pub use routes::{read_body, routes, too_large, with_body_limit};
pub use stream::until_revoked;
pub use tokens::EnrolFailures;

use crate::config::ApiKeyKind;
use crate::principal::Principal;
use crate::state::SharedState;

/// The context a retrieval's embedder and reranker calls are charged to on
/// the inference planes — `/mcp`, `/v1/responses`, a realtime session's
/// tools (review W3-3): a device's own, so nothing a device triggers runs
/// with the gateway's reach (client-apps design L4); `None` for every other
/// key, whose retrieval stays the gateway's own, as it always was.
pub fn charged(ctx: &crate::proxy::RequestCtx) -> Option<crate::proxy::RequestCtx> {
    matches!(
        ctx.principal,
        Principal::Key {
            kind: ApiKeyKind::Device,
            ..
        }
    )
    .then(|| ctx.clone())
}

/// Key `key_id`'s level of lmgw's admin tools (`ApiKey::self_admin`,
/// client-apps design L3/L5, 2026-10-07; a level since the pre-merge
/// review's P-3), as the snapshot `snap` says now. A key that is gone, of
/// another kind, disabled or expired is `off` (review P-8): Disable is the
/// owner's stop for a misbehaving device, and must not leave it the admin
/// tools until its connections notice.
pub fn self_admin(snap: &crate::config::Snapshot, key_id: i64) -> crate::config::DeviceAdmin {
    snap.api_keys
        .iter()
        .find(|k| {
            k.id == key_id
                && k.kind == ApiKeyKind::Device
                && k.enabled
                && crate::policy::check_expiry(k, chrono::Utc::now()).is_ok()
        })
        .map_or(crate::config::DeviceAdmin::Off, |k| k.self_admin)
}

/// What a caller's admin tools may do at most, before the gateway's own
/// self-admin level caps it: a device's own level, as `snap` says now; for
/// every other principal the gateway's level alone decides (whether it may
/// use them at all is the tool scope's question, `ToolScope::self_admin`).
pub fn admin_cap(
    snap: &crate::config::Snapshot,
    principal: &Principal,
) -> crate::config::SelfAdmin {
    match principal {
        Principal::Key {
            id,
            kind: ApiKeyKind::Device,
            ..
        } => self_admin(snap, *id).as_self_admin(),
        _ => crate::config::SelfAdmin::Full,
    }
}

/// What key `key_id`'s admin tools may do, as `snap` says now: its own
/// level ([`self_admin`]) capped by the gateway's self-admin level.
pub fn may_do(snap: &crate::config::Snapshot, key_id: i64) -> crate::config::SelfAdmin {
    self_admin(snap, key_id).capped(snap.settings.self_admin)
}

/// How far a device key reaches into the threads and folders that drive
/// the self-admin plane, as `snap` says now: the toolset's while its admin
/// tools may do anything ([`may_do`] above `off`: its own level and the
/// gateway's, 2026-10-07), none of them otherwise. Admin Chat never.
pub fn reach(snap: &crate::config::Snapshot, key_id: Option<i64>) -> crate::store::AdminThreads {
    crate::store::AdminThreads::of_device(key_id.is_some_and(|id| may_do(snap, id).allows_read()))
}

/// The status block's `devices` (device link design §9.2): the trust half,
/// the device listener and pairing.
pub fn status(state: &crate::state::AppState) -> lmgw_api_types::device_link::DevicesStatus {
    let alive = state.devices.pending.count(chrono::Utc::now()) as u64;
    lmgw_api_types::device_link::DevicesStatus {
        listener: state.devices.listener.status(state),
        pairing: state
            .devices
            .failures
            .status(alive, tokio::time::Instant::now()),
        ..state.devices.trust.status(&state.data_dir)
    }
}

/// A device row's pairing as the Keys list shows it (device link design
/// §3.6, §9.3), with its latest token `token`, at `now`.
pub fn pairing_view(
    key: &crate::config::ApiKey,
    token: Option<&crate::store::TokenRow>,
    now: chrono::DateTime<chrono::Utc>,
) -> lmgw_api_types::device_link::DevicePairingView {
    use crate::config::{DeviceCredential, DeviceLink};
    use lmgw_api_types::device_link::{DevicePairingState, DevicePairingView, PairingTokenView};
    let link = &key.link;
    let credential = DeviceLink::credential(&key.key_hash);
    let waiting = link.waiting.as_ref();
    DevicePairingView {
        state: match credential {
            _ if waiting.is_some() => DevicePairingState::Enrolled,
            DeviceCredential::V1 => DevicePairingState::V1,
            DeviceCredential::Pending => DevicePairingState::Waiting,
            DeviceCredential::Key(_) if link.confirmed_at.is_some() => {
                DevicePairingState::Confirmed
            }
            DeviceCredential::Key(_) => DevicePairingState::Enrolled,
        },
        token: token.map(|t| PairingTokenView {
            created_at: t.created_at.clone(),
            expires_at: t.expires_at.clone(),
            used_at: t.used_at.clone(),
            used_from: t.used_from.clone(),
            ended_at: t.ended_at.clone(),
            alive: t.alive(now),
        }),
        enrolled_at: link.enrolled_at.clone(),
        enrolled_from: link.enrolled_from.clone(),
        client: link.client_info.clone(),
        confirm_code: match waiting {
            Some(w) => DeviceLink::confirm_code(&w.key_hash),
            None => DeviceLink::confirm_code(&key.key_hash),
        },
        kept_code: waiting.and_then(|_| DeviceLink::confirm_code(&key.key_hash)),
        confirmed_at: link.confirmed_at.clone(),
        contested_at: link.contested_at.clone(),
        contested_from: link.contested_from.clone(),
        cert_not_after: link.cert_not_after.clone(),
        cert_notice_after: link.cert_notice_after.clone(),
        urls: link.urls.clone(),
        local_only: link.local_only,
    }
}

/// A snapshot was published: every server certificate carries the hosts of
/// every device's URL list from now on (§2.2; the leaf cache empties when
/// they changed).
pub fn published(state: &crate::state::AppState, snap: &crate::config::Snapshot) {
    let urls = snap
        .api_keys
        .iter()
        .filter(|k| k.kind == ApiKeyKind::Device)
        .flat_map(|k| k.link.urls.iter().map(String::as_str));
    state.devices.trust.leaves.set_url_hosts(urls);
}

/// `device:phone` reads as `phone` in a sentence that already says "device".
pub fn short_name(name: &str) -> &str {
    name.strip_prefix(NAME_PREFIX).unwrap_or(name)
}

/// What the gateway keeps about its devices while it runs: the open
/// connections and the revocation signal. In-process, like the live turns.
#[derive(Debug, Default)]
pub struct Devices {
    pub links: Links,
    pub revocations: Revocations,
    /// The pairing links minted and not yet handed to the desktop (review
    /// W2-1): what lmgw's own window may open.
    pub minted: MintedLinks,
    /// A device's admin-tools level moved, or the gateway's self-admin
    /// level that caps it: what a realtime session lists its `lmgw` tools
    /// again on (review P-8, 2026-10-07).
    pub reach_moves: ReachMoves,
    /// lmgw's device CA, its server certificates and the store of its key's
    /// wrapping key (device link design §2).
    pub trust: tls::Trust,
    /// The device listener: on, off, where, and what it serves now (device
    /// link design §4).
    pub listener: crate::server::DeviceListener,
    /// Which device row a client certificate's key opens (§4.7).
    pub keys: KeyLookup,
    /// The pending pairing tokens: whether a handshake may come without a
    /// certificate (§4.2).
    pub pending: PendingTokens,
    /// Failed enrolments per address, and the last hour's (§3.3).
    pub failures: EnrolFailures,
}

/// A device's level of lmgw's admin tools moved (`ApiKey::self_admin`,
/// review P-8), or the gateway's self-admin level that caps every device's
/// (2026-10-07, `AppState::store_carrying_lease`): a generation that moves
/// on every change, so a session busy when one came still sees the latest. A realtime session of a device
/// whose level now says otherwise than when its `lmgw` label was listed
/// lists it again (`realtime::mcp_tools`), so its model is offered what the
/// device may use now, not what it was offered before.
#[derive(Debug)]
pub struct ReachMoves(tokio::sync::watch::Sender<u64>);

impl Default for ReachMoves {
    fn default() -> Self {
        Self(tokio::sync::watch::Sender::new(0))
    }
}

impl ReachMoves {
    /// A level moved: every watcher wakes.
    pub fn moved(&self) {
        self.0.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// A watch that wakes at the next move.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.0.subscribe()
    }
}

/// One long-lived connection a key holds open: the revocation signal its
/// owner task selects on, and — for a device — counted as online while it
/// lives, `last_seen_at` stamped as it opens and as it closes.
pub struct DeviceConnection {
    /// A device's link on the card; `None` for every other key.
    _link: Option<LinkGuard>,
    watch: Watched,
}

impl DeviceConnection {
    /// Resolves once the key is disabled, rotated, deleted or past its
    /// expiry — whichever came first after this connection's principal was
    /// resolved — with the reason.
    pub async fn revoked(&mut self) -> RevokeReason {
        let reason = self.watch.revoked().await;
        if reason == RevokeReason::Rotated {
            if let Some(link) = self._link.as_mut() {
                link.close_unseen();
            }
        }
        reason
    }

    /// Who the reason a close carries names: "device 'phone'", "key
    /// 'laptop'" ([`who`]).
    pub fn name(&self) -> &str {
        &self.watch.who
    }

    /// Whether it is a paired device's: what kind of revocation it hears
    /// ([`RevokeReason::kind`]). The principal's kind, not whether the
    /// connection is counted as a link (review G-13).
    pub fn is_device(&self) -> bool {
        self.watch.device
    }
}

/// Open a connection for `ctx`'s principal, when it is a key; `None` for
/// `Anonymous`, whose connections nothing can revoke.
///
/// `ctx.revocation_mark` is the revocation generation when the principal was
/// resolved: a revocation raised between the gate and this call still ends
/// the connection; `None` (a context built by hand) counts from now. A
/// device on the device listener's connection also ends at its
/// certificate's `notAfter` (`ctx.cert_not_after`). Only a device's
/// connection is counted as a link ([`LinkKind`]).
pub async fn connect(
    state: &SharedState,
    ctx: &crate::proxy::RequestCtx,
    kind: LinkKind,
) -> Option<DeviceConnection> {
    connect_with(state, ctx, kind, None).await
}

/// [`connect`] for a link that asked for a keep-alive of `keepalive_s`
/// seconds (a device's feed): the card shows it beside the link, with the
/// peer and the start (§9.3).
pub async fn connect_with(
    state: &SharedState,
    ctx: &crate::proxy::RequestCtx,
    kind: LinkKind,
    keepalive_s: Option<u32>,
) -> Option<DeviceConnection> {
    open_link(state, ctx, kind, keepalive_s, None).await
}

/// [`connect`] for a realtime session ([`LinkKind::Voice`]): on a WebRTC
/// call `call` is its id, which the card shows beside the link (calls
/// design §9).
pub async fn connect_voice(
    state: &SharedState,
    ctx: &crate::proxy::RequestCtx,
    call: Option<std::sync::Arc<str>>,
) -> Option<DeviceConnection> {
    open_link(state, ctx, LinkKind::Voice, None, call).await
}

async fn open_link(
    state: &SharedState,
    ctx: &crate::proxy::RequestCtx,
    kind: LinkKind,
    keepalive_s: Option<u32>,
    call: Option<std::sync::Arc<str>>,
) -> Option<DeviceConnection> {
    let watch = watch(state, ctx)?;
    let link = match &ctx.principal {
        Principal::Key {
            id,
            kind: ApiKeyKind::Device,
            ..
        } => {
            let detail = LinkDetail {
                kind,
                since: chrono::Utc::now(),
                peer: ctx.conn.as_ref().map(|c| c.peer),
                keepalive_s,
                call,
            };
            Some(LinkGuard::open(state, *id, detail).await)
        }
        _ => None,
    };
    Some(DeviceConnection { _link: link, watch })
}

/// [`DeviceConnection::revoked`] for a connection that may not be a key's:
/// never resolves for `None`, so a `select!` arm on it is inert for an
/// anonymous principal.
pub async fn revoked(conn: Option<&mut DeviceConnection>) -> RevokeReason {
    match conn {
        Some(c) => c.revoked().await,
        None => std::future::pending().await,
    }
}

/// A watch on `ctx`'s principal's key that counts nothing — no link, no
/// `last_seen_at`: what a request that is not a connection of its own
/// watches while it runs (a Chat turn's stream, a read-aloud, a warm, the
/// `/mcp` notification stream; review W2-2). `None` for `Anonymous`.
pub fn watch(state: &SharedState, ctx: &crate::proxy::RequestCtx) -> Option<Watched> {
    let principal = &ctx.principal;
    let Principal::Key {
        id, fingerprint, ..
    } = principal
    else {
        return None;
    };
    let mark = ctx
        .revocation_mark
        .unwrap_or_else(|| state.devices.revocations.mark());
    Some(Watched {
        watch: state.devices.revocations.watch_from(
            *id,
            mark,
            fingerprint.clone(),
            ctx.cert_not_after,
        ),
        state: state.clone(),
        who: who(principal),
        device: principal.is_device_key(),
    })
}

/// One key's revocation watch with what it needs to wait on its own.
pub struct Watched {
    watch: RevocationWatch,
    state: SharedState,
    /// Who a revocation names ([`who`]): a deleted row has none to read.
    who: String,
    /// A paired device's key: what kind of revocation it hears
    /// ([`RevokeReason::kind`]).
    device: bool,
}

impl Watched {
    /// Resolves with the first revocation after the mark.
    pub async fn revoked(&mut self) -> RevokeReason {
        self.watch.revoked(&self.state).await
    }

    /// Who the key is, in a sentence ([`who`]).
    pub fn who(&self) -> &str {
        &self.who
    }

    /// `reason` as this key hears it: the sentence and the kind.
    pub fn said(&self, reason: RevokeReason) -> Revocation {
        Revocation {
            message: reason.message(&self.who),
            kind: reason.kind(self.device),
        }
    }
}

/// What a stream says when its key is revoked ([`until_revoked`]): the
/// sentence, and the kind a client acts on — the token a realtime close's
/// reason starts with and the feed's `revoked` carries (review G-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub message: String,
    pub kind: lmgw_api_types::realtime::RevokeKind,
}

/// How a revocation names the key: "device 'phone'" for a device, "key
/// 'laptop'" (owner keys "owner key 'dashboard'", agents "agent token
/// '<agent>'") otherwise.
pub fn who(principal: &Principal) -> String {
    match principal {
        Principal::Key {
            kind: ApiKeyKind::Device,
            name,
            ..
        } => format!("device '{}'", short_name(name)),
        Principal::Key {
            kind: ApiKeyKind::Owner,
            name,
            ..
        } => format!(
            "owner key '{}'",
            name.strip_prefix("owner:").unwrap_or(name)
        ),
        Principal::Key {
            kind: ApiKeyKind::Agent,
            agent_id,
            ..
        } => format!("agent token '{}'", agent_id.as_deref().unwrap_or("?")),
        Principal::Key { name, .. } => format!("key '{name}'"),
        Principal::Anonymous => "no key".to_string(),
    }
}

/// End every connection and stream `key_id` holds open, for `reason`
/// (§1.6), and say so on `/api/events` (`keys`), so the Keys page shows it
/// without polling.
pub fn revoke(state: &crate::state::AppState, key_id: i64, reason: RevokeReason) {
    state.devices.revocations.revoke(key_id, reason);
    state.devices.minted.forget(key_id);
    state.telemetry.keys(key_id, reason.as_str());
}

/// The close code a long-lived connection of a revoked device closes with
/// (§1.6, §1.8): application-defined (RFC 6455 §7.4.2), beside realtime's
/// 4000 for a takeover and 4004 for a thread out of reach. Shared with
/// clients (`lmgw-api-types`).
pub use lmgw_api_types::realtime::CLOSE_REVOKED;

/// The close reason a revoked connection's WebSocket carries: the kind's
/// token ([`RevokeReason::kind`], `device` for a paired device's key), a
/// colon, and [`RevokeReason::message`] for `who` ([`who`]) — "device_disabled:
/// device 'desktop' was disabled" — cut at a character boundary to the 123
/// bytes a close frame's reason may hold (RFC 6455 §5.5), the token always
/// whole: the card shows the name whole; a close has no room for a very
/// long one.
pub fn close_reason(reason: RevokeReason, who: &str, device: bool) -> String {
    let mut text = format!("{}: {}", reason.kind(device), reason.message(who));
    if text.len() > 123 {
        let mut end = 120;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review G-9: a name too long for a close frame is cut, the kind's
    /// token never — a client reads it before anything else.
    #[test]
    fn a_long_close_reason_keeps_its_token_whole() {
        let who = format!("device '{}'", "ä".repeat(200));
        for (reason, device, token) in [
            (RevokeReason::Disabled, true, "device_disabled: "),
            (RevokeReason::Rotated, true, "key_unknown: "),
            (RevokeReason::Expired, true, "key_expired: "),
            (RevokeReason::Deleted, false, "revoked: "),
            (
                RevokeReason::CertificateExpired,
                true,
                "certificate_expired: ",
            ),
        ] {
            let text = close_reason(reason, &who, device);
            assert!(text.len() <= 123, "{}", text.len());
            assert!(text.starts_with(token), "{text}");
            assert!(text.ends_with('…'), "{text}");
            assert_eq!(
                lmgw_api_types::realtime::RevokeKind::of_close_reason(&text),
                reason.kind(device)
            );
        }
        let short = close_reason(RevokeReason::Rotated, "device 'desk'", true);
        assert_eq!(
            short,
            "key_unknown: the key of device 'desk' was replaced — pair it again"
        );
    }
}
