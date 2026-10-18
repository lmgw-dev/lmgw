//! `ops-keys`: the credential ops (api-docs design §4.7, principals design
//! §3.12) and the device link's pairing ops (device link design §9.4). None
//! of these has a self-admin tool — the self-admin plane has never exposed a
//! credential, `key_reveal`/`key_rotate` hand out the key that holds every
//! capability, and `key_create`/`device_token` a pairing link that makes a
//! device.

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-keys";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "key_create",
        tag: TAG,
        summary: "Create a client, owner or device API key",
        description: Some(
            "Create a new API key: a client key (default), an owner key or a device key. The \
             plaintext is returned once for a client key; an owner key's plaintext stays \
             retrievable with key_reveal for as long as the row exists. A client key may be \
             created with its alias scope, MCP tool scope and device-tools grant already set \
             (scope_mode, scope_patterns, tool_scope_mode, tool_scope_patterns, device_tools — \
             the same fields and rules as key_set); an owner key refuses them. A device key \
             (kind 'device', named device:<name>) pairs one client app: it is created with its \
             whole policy — the scopes, the device-tools grant, budget, limits and expiry \
             key_set takes — an optional hosts_label, an \
             optional self_admin level (off by default), local_only (paired as this computer) \
             and urls (its lmgw URLs on the device listener; proposed when left out). It holds \
             no key yet: it answers DevicePaired {id, name, link, urls, local_only, \
             token_expires_at, fp}, where link is lmgw-pair:?v=2&name=…&url=…&fp=…&token=…, \
             carrying a one-time pairing token that lapses after devices.token_minutes. The \
             device enrols its own key with it on POST /devices/enrol and is served once the \
             owner confirms its code (device_confirm). Refused 409 device_listener_off while \
             the device listener does not serve. A device holds the Chat API on top of a \
             client key's inference, and nothing of the admin plane. A client or owner key \
             answers KeyCreated ({id, name, key}).",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_create),
        response: Resp::Json(|g| g.root_schema_for::<dto::KeyCreateAnswer>()),
        writes: true,
        // The answer carries the new key's `key`, or a device's pairing link
        // with its token (review R2 #8).
        reveals_secret: true,
        confirm_note: None,
        deprecated: false,
        example: Some(
            r#"{"name":"ci-bot","tool_scope_mode":"allow","tool_scope_patterns":"docs__*"}"#,
        ),
    },
    OpDoc {
        name: "key_set",
        tag: TAG,
        summary: "Change a key's policy — scope, budget, rate limits, expiry",
        description: Some(
            "The settable half of one key's policy. Sparse like every other patch: a field \
             left out keeps the value the row already has. The other half of a key's policy is \
             derived (an agent token's scope and enabled flag are written by lmgw from the \
             agent's manifest and row) and is refused here rather than accepted and silently \
             taken back by the next resync. A client or device key's device_tools names the \
             device-hosted labels whose tools it reaches (e.g. [\"kai\"]), whatever its tool \
             scope's mode, which then filters those tools as any server's; [] clears it, and \
             a label no device hosts yet is taken. A device key also takes hosts_label (\"\" \
             clears it), urls (its lmgw URLs, which every server certificate carries from the next \
             connection on) and self_admin, its level of lmgw's admin tools: off, read_only or \
             full, capped \
             by the self-admin level in Settings; what the device sees follows the capped \
             level. Raised above off (under a self-admin level above off), the device's change feed \
             receives the threads and folders with the self-admin toolset as created; set to \
             off, as deleted, its Chat turns on such a thread stop, and its realtime sessions \
             bound to such a thread close with 4004. Lowered to read_only, a turn still running \
             is refused its next write tool. On any change its change feed sends a state event \
             with self_admin, what its admin tools may do now, and its realtime sessions that \
             carry the lmgw label list it again. Disabling a key of any \
             kind, or its expires_at passing, ends what it holds \
             open at once: its realtime sessions close with 4003, its Chat streams end with an \
             error frame whose code is revoked, its change feed with a revoked event, and its \
             /mcp notification stream ends.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::KeyPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::KeySaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "key_delete",
        tag: TAG,
        summary: "Revoke an API key",
        description: Some(
            "Delete a key by id. Deleting the dashboard's own owner key is allowed and means \
             \"closed\": the next start seeds it again, disabled, until it is enabled \
             — the credential handed out until then is gone. Deleting a key ends what it holds \
             open at once, as disabling it does.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_delete),
        response: Resp::Json(|g| g.root_schema_for::<dto::MessageAck>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "key_reveal",
        tag: TAG,
        summary: "Show an owner key's stored plaintext",
        description: Some(
            "The stored plaintext of one owner-kind key. A client key is hashed and shown only \
             once at creation, so there is nothing to reveal for it; a device authenticates \
             with a key of its own that never leaves it (device_token pairs it again); an \
             agent token is read from the agent's own page instead.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_reveal),
        response: Resp::Json(|g| g.root_schema_for::<dto::KeySecret>()),
        writes: false,
        reveals_secret: true,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "key_rotate",
        tag: TAG,
        summary: "Mint a new plaintext for an owner key, invalidating the old one",
        description: Some(
            "Replace an owner key's hash and plaintext in one write, so the old value stops \
             working the moment the new one is live. A disabled owner:self-admin key stays \
             disabled after rotation — the plane stays closed. What the old value holds open \
             ends at once, as on a disable: realtime sessions close with 4003, Chat streams end \
             with an error frame whose code is revoked, the change feed with a revoked event, \
             and /mcp notification streams end. A device row is refused: it holds no secret of \
             lmgw's making, and pairs again with device_token. Answers KeySecret ({id, name, \
             key}).",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_rotate),
        response: Resp::Json(|g| g.root_schema_for::<dto::KeySecret>()),
        writes: true,
        // The answer carries the new plaintext `key` (review R2 #8).
        reveals_secret: true,
        confirm_note: Some(
            "rotating owner:dashboard signs every browser out and ends the dashboard's running \
             Chat turn and voice session; rotating any other owner key ends what it holds open",
        ),
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "device_token",
        tag: TAG,
        summary: "Give a device a new pairing token (re-pair it)",
        description: Some(
            "A new one-time pairing token for a device row, replacing its earlier one; the row \
             keeps its id, policy and history. end_current: true ends the key the device holds \
             now (a lost or stolen device): what it holds open closes at once, as on a disable, \
             and its requests are refused until the new token enrols. Left out or false, the \
             current key works on: a new key the token enrols waits beside it for its own \
             confirmation (device_confirm), which ends the current key (a new phone), and the \
             same key enrolling again gets a fresh certificate and keeps its confirmation (a \
             renewal before the old one lapses). A key that waited from an earlier token, and \
             the contested mark, go with the new token. local_only and urls replace the row's, \
             which a device paired before TLS has none of. Answers DevicePaired with the new link, as key_create does. \
             Refused 409 device_listener_off while the device listener does not serve.",
        ),
        tool: None,
        args: OpArgs::Hand(args::device_token),
        response: Resp::Json(|g| g.root_schema_for::<dto::DevicePaired>()),
        writes: true,
        // The answer carries the pairing link with its token.
        reveals_secret: true,
        confirm_note: Some(
            "with end_current the device's current key stops working at once and what it holds \
             open closes",
        ),
        deprecated: false,
        example: Some(r#"{"id":12,"end_current":false}"#),
    },
    OpDoc {
        name: "device_token_end",
        tag: TAG,
        summary: "End a device's pairing token",
        description: Some(
            "End a device's pairing token while it lives (the QR card's close): its link pairs \
             nothing any more, a used one included, whose own key can then no longer ask for \
             its enrolment's answer again. Answers DeviceTokenEnded, ended false when no token \
             was alive.",
        ),
        tool: None,
        args: OpArgs::Hand(args::device_id),
        response: Resp::Json(|g| g.root_schema_for::<dto::DeviceTokenEnded>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "device_confirm",
        tag: TAG,
        summary: "Confirm a device's enrolment",
        description: Some(
            "Confirm the key a device enrolled, after comparing the code the Keys list shows \
             (pairing.confirm_code) with the one the device shows, and sending that code: the \
             key waiting for the confirmation must have it, else 409 confirm_code_mismatch and \
             nothing changes (a card showing an earlier enrolment confirms nothing). Until then \
             every request of that key is refused 403 device_unconfirmed; from now on it is \
             served. A key that waited beside one the device kept (a re-pair that kept the \
             current key) replaces it: the kept key ends, and what it holds open closes. \
             Answers DeviceConfirmed with the code confirmed and whether it replaced a kept \
             key.",
        ),
        tool: None,
        args: OpArgs::Hand(args::device_confirm),
        response: Resp::Json(|g| g.root_schema_for::<dto::DeviceConfirmed>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"id":12,"confirm_code":"7F3A-91C2"}"#),
    },
    OpDoc {
        name: "device_ca_rotate",
        tag: TAG,
        summary: "Replace lmgw's device CA and reset every device",
        description: Some(
            "Replace lmgw's device CA: a new key under a new wrapping key and a new \
             certificate replace tls/ca.key and tls/ca.crt, and the old key is not kept. Every \
             enrolled device loses its key and certificate (its policy kept) and must be paired \
             again with a new token; what each holds open closes at once, and every pairing \
             token alive ends, since its link pins the old CA. The server certificates are \
             issued anew. Answers DeviceCaRotated with the new fp and the devices reset.",
        ),
        tool: None,
        args: OpArgs::Hand(args::device_ca_rotate),
        response: Resp::Json(|g| g.root_schema_for::<dto::DeviceCaRotated>()),
        writes: true,
        reveals_secret: false,
        confirm_note: Some(
            "every paired device stops connecting at once and must be paired again with a new \
             token; rotate only if lmgw's data directory, or its wallet, may have been copied",
        ),
        deprecated: false,
        example: None,
    },
];
