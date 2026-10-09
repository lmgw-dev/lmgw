//! Who a Chat request and its turn run as (client-apps design §1.3, L3–L5):
//! the owner, or a paired device.
//!
//! **The owner** — the dashboard's cookie, an owner key — keeps what the
//! Chat has always done: its model calls are charged to `internal:chat` and
//! go unchecked by any key, and its tools resolve under
//! [`ToolScope::gateway`].
//!
//! **A device** runs as its key, everywhere a Chat request or a turn it
//! starts reaches a model or a tool:
//! - every model call — the chat model, each call of the tool loop, ASR and
//!   TTS, a knowledge base's embedder and reranker — passes
//!   [`policy_checked_call`](crate::proxy::policy_checked_call) against the
//!   key (disabled or deleted, scope, budget, expiry, rpm, tpm) and its row
//!   is the key's ([`Caller::check`], [`Caller::key`]);
//! - its tools resolve under the key's tool scope ([`Caller::scope`]), and
//!   the tool labels it writes are checked against that scope
//!   (`chat_tool_write`, L5);
//! - Admin Chat threads do not exist for it ([`Caller::sees`]): by id they
//!   are not found, and no listing carries them (L3). Nor do the threads and
//!   folders with the self-admin toolset, unless the device may use lmgw's
//!   admin tools (`ApiKey::self_admin`, 2026-10-07) — read from the
//!   snapshot each time it is asked, so the switch applies at once.
//!
//! The extractor reads the principal the router resolved
//! (`RequestCtx::principal`); the `Chat` gate has already let only an owner
//! or a device through.
//!
//! **Fails closed** (reviews W3-12, W4-9). A request with no principal, or
//! an anonymous one, is never the owner: the extractor refuses it, and
//! [`Caller::of`] maps it to [`Caller::Refused`], which sees no thread,
//! reaches no tool and has every model call refused. Only an in-process
//! caller is the owner without a key ([`Caller::default`]).

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::sse::Event as SseEvent;
use futures::stream::BoxStream;
use futures::Stream;

use crate::config::ApiKeyKind;
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::mcp::scope::ToolScope;
use crate::principal::Principal;
use crate::proxy::{KeyRef, RequestCtx};
use crate::state::SharedState;
use crate::store::{AdminThreads, ChatThread};
use crate::telemetry::RequestClass;

/// Who a Chat request or turn runs as (module doc).
#[derive(Debug, Clone)]
pub(crate) enum Caller {
    /// The owner, and the gateway's own in-process callers: today's Chat.
    /// The request's context when an owner key made it (the dashboard's
    /// session is its own owner key): its Chat streams end when that key is
    /// disabled, rotated or deleted (review W3-6). `None` for an in-process
    /// caller, which no key carries.
    Owner(Option<Box<RequestCtx>>),
    /// A paired device: its request's context, whose principal is its key.
    Device(Box<RequestCtx>),
    /// A request that holds nothing here (no principal, or an anonymous
    /// one): it sees no thread or folder, reaches no tool, and every model
    /// call it would make is refused. Never reached behind the `Chat` gate;
    /// what a path that bypassed it gets instead of the owner's reach.
    Refused(Box<RequestCtx>),
}

impl Default for Caller {
    /// The gateway's own in-process caller.
    fn default() -> Self {
        Self::Owner(None)
    }
}

impl Caller {
    /// The caller behind a request's context. An owner key is the owner;
    /// every other key — a device is the only other one that holds `Chat` —
    /// runs as itself, never as the gateway; an anonymous principal holds
    /// nothing ([`Self::Refused`]).
    pub(crate) fn of(ctx: &RequestCtx) -> Self {
        match &ctx.principal {
            Principal::Anonymous => Self::Refused(Box::new(ctx.clone())),
            Principal::Key {
                kind: ApiKeyKind::Owner,
                ..
            } => Self::Owner(Some(Box::new(ctx.clone()))),
            Principal::Key { .. } => Self::Device(Box::new(ctx.clone())),
        }
    }

    /// Whether it is not the owner: a device, or a caller that holds
    /// nothing. Every restriction a device has applies to it.
    pub(crate) fn is_device(&self) -> bool {
        matches!(self, Self::Device(_) | Self::Refused(_))
    }

    /// The refusal of a model call by a caller that holds nothing.
    fn refused() -> GatewayError {
        GatewayError::Refused {
            status: 401,
            code: "session_required",
            message: "the Chat API needs an owner session or a device key".into(),
        }
    }

    /// The context its model and tool calls carry: the device's, or the
    /// in-process default the Chat has always used.
    pub(crate) fn ctx(&self) -> RequestCtx {
        match self {
            Self::Owner(_) => RequestCtx::default(),
            Self::Device(ctx) | Self::Refused(ctx) => (**ctx).clone(),
        }
    }

    /// Who its tool calls run as, for a device-hosted server's `_meta`
    /// (client-apps design §5.5): the owner by its key, the gateway for an
    /// in-process caller, the device.
    pub(crate) fn call_from(&self) -> crate::mcp::host::CallFrom {
        match self {
            Self::Owner(None) => crate::mcp::host::CallFrom::gateway(),
            Self::Owner(Some(ctx)) | Self::Device(ctx) | Self::Refused(ctx) => {
                crate::mcp::host::CallFrom::of(ctx)
            }
        }
    }

    /// Whom its rows are charged to: the device's key, or nobody — the row's
    /// label then names the internal identity (`internal:chat`).
    pub(crate) fn key(&self) -> KeyRef {
        match self {
            Self::Owner(_) => KeyRef::default(),
            Self::Device(ctx) | Self::Refused(ctx) => ctx.key_ref(),
        }
    }

    /// The context a gateway helper that takes an optional caller charges
    /// to: the device's, `None` for the owner (the helper's own charging).
    pub(crate) fn charged(&self) -> Option<RequestCtx> {
        match self {
            Self::Owner(_) => None,
            Self::Device(ctx) | Self::Refused(ctx) => Some((**ctx).clone()),
        }
    }

    /// One model call's check (L4): nothing for the owner, whose turns no
    /// key bounds; [`policy_checked_call`](crate::proxy::policy_checked_call)
    /// for a device — its key read again by id, scope, budget, expiry and
    /// the per-minute windows, the call counted. A refusal has written its
    /// row, labelled `proto`.
    pub(crate) async fn check(
        &self,
        state: &SharedState,
        proto: ClientProto,
        alias: &str,
        class: RequestClass,
    ) -> Result<(), GatewayError> {
        match self {
            Self::Owner(_) => Ok(()),
            Self::Device(ctx) => {
                crate::proxy::policy_checked_call(state, proto, ctx, alias, class).await
            }
            Self::Refused(_) => Err(Self::refused()),
        }
    }

    /// The check before anything is loaded for a call (review W3-2): for a
    /// device, its key's scope and budget for `alias`, not counted — the call
    /// itself is, by [`Self::check`] — and a refusal's row written, labelled
    /// `proto`; nothing for the owner. What a path runs before GPU admission
    /// when its first counted check comes later (the tool loop's).
    pub(crate) async fn precheck(
        &self,
        state: &SharedState,
        proto: ClientProto,
        alias: &str,
        class: RequestClass,
    ) -> Result<(), GatewayError> {
        match self {
            Self::Owner(_) => Ok(()),
            Self::Device(ctx) => {
                crate::proxy::policy_checked(state, proto, ctx, alias, class).await
            }
            Self::Refused(_) => Err(Self::refused()),
        }
    }

    /// One of a device key's concurrent-request slots, held for the length
    /// of a Chat turn it starts (review W3-8: `concurrency_limit` bounds a
    /// device's turns, as it bounds its `/v1` requests and its realtime
    /// sessions). Taking it counts no request and checks no window — each
    /// model call of the turn is checked and counted as it is made. `Ok(None)`
    /// for the owner, and for a key with no limit to hold. A refusal is the key's own `key_rate`, its row
    /// written, labelled `proto`.
    pub(crate) async fn turn_slot(
        &self,
        state: &SharedState,
        proto: ClientProto,
        alias: &str,
    ) -> Result<Option<crate::policy::ConcurrencyGuard>, GatewayError> {
        let ctx = match self {
            Self::Owner(_) => return Ok(None),
            Self::Refused(_) => return Err(Self::refused()),
            Self::Device(ctx) => ctx,
        };
        let snap = state.snapshot();
        let Some(key) = ctx
            .principal
            .key_id()
            .and_then(|id| snap.api_keys.iter().find(|k| k.id == id))
        else {
            // Gone: the turn's first call refuses it, by name.
            return Ok(None);
        };
        match state.policy.take_slot(key) {
            Ok(slot) => Ok(slot),
            Err(e) => {
                crate::proxy::refused_for_key(state, proto, ctx, alias, RequestClass::Chat, &e)
                    .await;
                Err(e)
            }
        }
    }

    /// Whether `alias` is within a device key's alias scope, as its row says
    /// now (review W3-4): what an alias a device writes into a thread or a
    /// folder's defaults must be. Not a call, so nothing is counted or
    /// charged, and no budget applies. Always `Ok` for the owner.
    pub(crate) fn alias_in_scope(
        &self,
        snap: &crate::config::Snapshot,
        alias: &str,
    ) -> Result<(), GatewayError> {
        match self {
            Self::Owner(_) => Ok(()),
            Self::Device(ctx) => {
                crate::policy::check_scope(snap, ctx.current_key_name(snap).as_deref(), alias)
            }
            Self::Refused(_) => Err(Self::refused()),
        }
    }

    /// An audio file transcribed as this caller — an attachment's
    /// transcript at its upload, its retry or its turn: the call's check
    /// ([`Self::check`]), then the call, its row the caller's and labelled
    /// `proto` (the thread's label, `chat_voice::speech_proto`).
    pub(crate) async fn transcribe(
        &self,
        state: &SharedState,
        proto: ClientProto,
        alias: &str,
        (bytes, filename, mime): (bytes::Bytes, &str, &str),
    ) -> Result<String, GatewayError> {
        self.check(state, proto, alias, RequestClass::Audio).await?;
        crate::proxy::transcribe_for(state, &self.ctx(), proto, alias, bytes, filename, mime).await
    }

    /// The tools within its reach: the gateway's own scope for the owner
    /// (the owner attached them), the device key's tool scope otherwise —
    /// read now, so an edit to the key applies to the next turn.
    pub(crate) async fn scope(&self, state: &SharedState) -> ToolScope {
        match self {
            Self::Owner(_) => ToolScope::gateway(),
            Self::Device(ctx) => ToolScope::of_request(state, ctx).await,
            Self::Refused(_) => ToolScope::nothing("an unauthenticated caller"),
        }
    }

    /// Whether `thread` exists for this caller (L3), as `snap` says now: for
    /// a device, Admin Chat never does (review W3-1), and a thread with the
    /// self-admin toolset only when the device may use lmgw's admin tools.
    pub(crate) fn sees(&self, snap: &crate::config::Snapshot, thread: &ChatThread) -> bool {
        match self {
            Self::Owner(_) => true,
            Self::Device(_) => self.reach(snap).sees(thread.reach_level()),
            Self::Refused(_) => false,
        }
    }

    /// Whether `folder` exists for this caller (L3), as `snap` says now: for
    /// a device, one a device deleted while it held threads out of the
    /// device's reach (review W6-1) does not, nor one whose defaults attach
    /// the self-admin toolset (review W3-1) unless the device may use lmgw's
    /// admin tools.
    pub(crate) fn sees_folder(
        &self,
        snap: &crate::config::Snapshot,
        folder: &crate::store::ChatFolder,
    ) -> bool {
        match self {
            Self::Owner(_) => true,
            Self::Device(_) => self.reach(snap).sees(folder.reach_level()),
            Self::Refused(_) => false,
        }
    }

    /// The generation of the server its request came in on
    /// (`RequestCtx::served_at`); `None` for an in-process caller.
    pub(crate) fn served_at(&self) -> Option<u64> {
        match self {
            Self::Owner(None) => None,
            Self::Owner(Some(ctx)) | Self::Device(ctx) | Self::Refused(ctx) => ctx.served_at,
        }
    }

    /// The key it runs as; `None` for an in-process caller.
    pub(crate) fn key_id(&self) -> Option<i64> {
        match self {
            Self::Owner(None) => None,
            Self::Owner(Some(ctx)) | Self::Device(ctx) | Self::Refused(ctx) => {
                ctx.principal.key_id()
            }
        }
    }

    /// The device key whose own admin-tools level a write is checked
    /// against (`ops::chat_profiles`'s admin-use guard); `None` for the
    /// owner. A caller that holds nothing is a device without a key here:
    /// id 0, which no key has, so it holds no level (fails closed).
    pub(crate) fn device_id(&self) -> Option<i64> {
        match self {
            Self::Owner(_) => None,
            Self::Device(ctx) | Self::Refused(ctx) => Some(ctx.principal.key_id().unwrap_or(0)),
        }
    }

    /// How far a listing it reads reaches into the threads and folders that
    /// drive the self-admin plane (L3), as `snap` says now: everything for
    /// the owner; the toolset's for a device allowed lmgw's admin tools;
    /// none of them for any other device or a caller that holds nothing.
    pub(crate) fn reach(&self, snap: &crate::config::Snapshot) -> AdminThreads {
        match self {
            Self::Owner(_) => AdminThreads::Shown,
            Self::Device(ctx) => crate::devices::reach(snap, ctx.principal.key_id()),
            Self::Refused(_) => AdminThreads::Hidden,
        }
    }

    /// The principal that started a gated turn, by its key (`None`: the
    /// gateway's own in-process run), as the resumed turn runs as it
    /// (client-apps design L13): `Err` naming the key when it is gone or
    /// disabled. `name` is its name when the turn started.
    pub(crate) fn resumed(
        snap: &crate::config::Snapshot,
        key_id: Option<i64>,
        name: Option<&str>,
    ) -> Result<Self, String> {
        let Some(id) = key_id else {
            return Ok(Self::Owner(None));
        };
        let named = name.map_or_else(|| format!("key {id}"), |n| format!("key '{n}'"));
        let Some(key) = snap.api_keys.iter().find(|k| k.id == id) else {
            return Err(format!(
                "{named}, which started this turn, is gone, and a resumed turn runs as the \
                 principal that started it"
            ));
        };
        if !key.enabled {
            return Err(format!(
                "key '{}', which started this turn, is disabled, and a resumed turn runs as the \
                 principal that started it",
                key.name
            ));
        }
        let ctx = RequestCtx {
            principal: Principal::from_key(key),
            client_key: Some(key.name.clone()),
            ..RequestCtx::default()
        };
        Ok(match key.kind {
            ApiKeyKind::Internal => Self::Owner(None),
            _ => Self::of(&ctx),
        })
    }

    /// Who it decides as (client-apps design §6.3): its principal as a
    /// device reads it, and its name in the feed.
    pub(crate) fn decider(&self) -> crate::store::Decider {
        crate::store::Decider {
            who: self.call_from().caller,
            named: self.named(),
        }
    }

    /// How it is named where another client is told about it (§1.7): "the
    /// dashboard" for the owner, "device '<name>'" for a device.
    pub(crate) fn named(&self) -> String {
        match self {
            Self::Owner(_) => crate::store::feed::BY_OWNER.to_string(),
            Self::Device(ctx) => match &ctx.principal {
                Principal::Key {
                    kind: ApiKeyKind::Device,
                    name,
                    ..
                } => lmgw_api_types::chat_feed::by_device(crate::devices::short_name(name)),
                other => other.describe(),
            },
            Self::Refused(_) => "an unauthenticated caller".to_string(),
        }
    }
}

impl Caller {
    /// A Chat SSE stream run for this caller (§1.6, review W2-2): a device's
    /// ends the moment its key is disabled, rotated, deleted or expires, with
    /// a last `error` frame `{code: "revoked", message}` — and the turn, the
    /// read-aloud or the warm behind it hears its reader go and stops. So
    /// does an owner key's (review W3-6: Disable means disable), the
    /// dashboard's session included: its own Rotate ends the turn it has
    /// running. An in-process caller's runs as it always has.
    pub(crate) fn sse<S>(
        &self,
        state: &SharedState,
        events: S,
    ) -> BoxStream<'static, Result<SseEvent, Infallible>>
    where
        S: Stream<Item = Result<SseEvent, Infallible>> + Send + 'static,
    {
        let watch = match self {
            Self::Owner(None) | Self::Refused(_) => None,
            Self::Owner(Some(ctx)) | Self::Device(ctx) => {
                crate::devices::watch(state, &ctx.principal, ctx.revocation_mark)
            }
        };
        let events = crate::devices::until_revoked(events, watch, |said| {
            // `kind` says what to do, as a realtime close's token and the
            // feed's `revoked` say it (review G-7).
            let data = serde_json::json!({
                "code": "revoked",
                "message": said.message,
                "kind": said.kind.as_str(),
            });
            Some(Ok(SseEvent::default()
                .event("error")
                .data(data.to_string())))
        });
        // And when the server stops, with a last `error` frame
        // `{code: "gateway_stopping"}`: the turn behind it hears its reader
        // go and saves what it had, as at a revocation.
        let data = serde_json::json!({
            "code": "gateway_stopping",
            "message": crate::server::STOPPING,
        });
        let last = Ok(SseEvent::default().event("error").data(data.to_string()));
        crate::server::until_stopped(&state.stops, self.served_at(), events, Some(last))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = axum::response::Response;

    /// Fails closed (review W3-12): a request with no resolved principal —
    /// a route mounted without the router's principal layer — is a 500, and
    /// an anonymous one a 401, never the gateway's own reach. Behind the
    /// `Chat` gate neither can happen.
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let Some(ctx) = parts.extensions.get::<RequestCtx>() else {
            return Err(super::chat::err_json(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "this Chat route was reached without a resolved caller",
            ));
        };
        if ctx.principal == Principal::Anonymous {
            return Err(super::chat::err_json(
                axum::http::StatusCode::UNAUTHORIZED,
                "session_required",
                "the Chat API needs an owner session or a device key",
            ));
        }
        Ok(Caller::of(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reviews W3-12, W4-9: an anonymous principal is never the owner — it
    /// sees no thread or folder, reaches no tool, and is not watched as one.
    #[test]
    fn an_anonymous_principal_holds_nothing() {
        let c = Caller::of(&RequestCtx::default());
        assert!(matches!(c, Caller::Refused(_)));
        assert!(c.is_device(), "every restriction of a device applies");
        let plain = ChatThread {
            kind: "chat".into(),
            ..Default::default()
        };
        let snap = crate::config::Snapshot::default();
        assert!(!c.sees(&snap, &plain));
        assert!(!c.sees_folder(&snap, &crate::store::ChatFolder::default()));
        assert_eq!(c.reach(&snap), AdminThreads::Hidden);
        assert_eq!(c.named(), "an unauthenticated caller");
        assert!(c.alias_in_scope(&snap, "any").is_err());
        // The in-process default stays the owner.
        assert!(Caller::default().sees(&snap, &plain) && !Caller::default().is_device());
    }
}
