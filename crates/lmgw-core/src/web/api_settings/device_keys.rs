//! The device half of the credential ops (client-apps design §1.4, device
//! link design §3, §5, §9.4).
//!
//! A device is created with its whole policy, its URLs and a one-time
//! pairing token, whose `lmgw-pair:` link v2 goes to the owner once. The
//! device enrols its own key with it (`POST /devices/enrol`) and is served
//! once the owner confirms its code (`device_confirm`). A device holds no
//! secret of lmgw's making: re-pairing is a new token on the same row
//! (`device_token`), its id, policy and history kept, the current key ending
//! at once or when the owner confirms the new one; `key_rotate` and
//! `key_reveal` refuse a device row. `device_token_end` ends a token unused
//! (the QR card's close), `device_ca_rotate` replaces lmgw's device CA and
//! resets every device, and `key_delete` ends a device's connections and its
//! token as it deletes it.

use chrono::Utc;
use lmgw_api_types as dto;
use lmgw_api_types::device_link::{code, DeviceListenerState};
use serde_json::{Map, Value};

use super::bad_request;
use crate::config::{ApiKey, ApiKeyKind, DeviceCredential, DeviceLink};
use crate::devices::{self, RevokeReason};
use crate::ops::KeyPatch;
use crate::principal::Refusal;
use crate::state::SharedState;
use crate::store;

/// `key_create { kind: "device", name, local_only?, urls?, hosts_label?,
/// self_admin?, <policy> }` → [`dto::DevicePaired`] `{ id, name, link,
/// urls, local_only, token_expires_at, fp }`.
///
/// `<policy>` is `key_set`'s fields — alias and tool scope, budget, limits,
/// expiry, note — so the pairing form's confirmed scope is what the row is
/// born with. Left out, a field takes the default the form prefills: `all`
/// and no budget (§11 Q1). `urls` left out is the form's proposal
/// ([`devices::proposed_urls`]). Refused 409 `device_listener_off` while the
/// device listener does not serve: the token would reach nothing.
pub(super) async fn create(
    st: &SharedState,
    name: &str,
    args: &Map<String, Value>,
) -> Result<dto::DevicePaired, Refusal> {
    let bare = name
        .strip_prefix(devices::NAME_PREFIX)
        .unwrap_or(name)
        .trim();
    if bare.is_empty() {
        return Err(bad_request("device name is required"));
    }
    // It is said in close reasons, log lines and takeover messages (review
    // W2-23): printable text, and no `:`, which the name's own prefix uses.
    // Nor invisible format characters (review W3-17): a right-to-left
    // override or a zero-width joiner would make a close reason or a
    // takeover message read as something else.
    if bare.chars().any(|c| c == ':' || devices::is_invisible(c)) {
        return Err(bad_request(
            "a device name is printable text without ':' or invisible format characters — \
             e.g. 'desktop' or 'phone'",
        ));
    }
    let name = format!("{}{bare}", devices::NAME_PREFIX);
    let snap = st.snapshot();
    if let Some(k) = snap.api_keys.iter().find(|k| k.name == name) {
        return Err(bad_request(if k.kind == ApiKeyKind::Device {
            format!(
                "a device named '{bare}' is already paired — pick another name, or give that \
                 one a new pairing token"
            )
        } else {
            let holder = match k.kind {
                ApiKeyKind::Owner => "an owner key",
                ApiKeyKind::Agent => "an agent token",
                ApiKeyKind::Internal => "an internal identity",
                _ => "a client key",
            };
            format!("'{name}' is already the name of {holder} — pick another name for the device")
        }));
    }
    let patch: KeyPatch = crate::ops::patch_from_args(Some(args.clone())).map_err(bad_request)?;
    let policy = crate::ops::policy_from_patch(&patch).map_err(bad_request)?;
    let hosts_label = patch
        .hosts_label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    if let Some(label) = hosts_label {
        if let Some(why) = devices::label_refusal(&snap, label, None) {
            return Err(bad_request(why));
        }
    }
    // Born with the level the pairing form confirmed (off unless asked): a
    // device is never, even for a moment, allowed more.
    let self_admin = match patch.self_admin.as_ref() {
        Some(v) => crate::ops::parse_device_admin(v).map_err(bad_request)?,
        None => crate::config::DeviceAdmin::Off,
    };
    let local_only = local_only_arg(args)?.unwrap_or(false);
    let urls = urls_for(st, patch.urls.as_deref(), local_only)?;
    let fp = listening(st)?;
    let note = patch.note.as_deref().map(str::trim).unwrap_or_default();
    let (token, new) = new_token(st);
    let id = store::insert_paired_device(
        &st.db,
        &name,
        &policy,
        (hosts_label, self_admin),
        note,
        (&urls, local_only),
        &new,
    )
    .await
    .map_err(|e| bad_request(e.to_string()))?;
    // Past the insert the token exists and only this answer holds it: a
    // reload that fails is said beside it, never a refusal that throws it
    // away (review W2-6).
    let reloaded = st.reload_snapshot().await.err();
    let paired = minted(st, id, &name, (urls, local_only), &fp, &token, &new).await;
    Ok(dto::DevicePaired {
        message: format!(
            "device '{bare}' is waiting for its device: open or copy the link now, it is not \
             shown again{}{}",
            reach_note(st, &paired.urls, local_only),
            unloaded(reloaded.as_ref())
        ),
        ..paired
    })
}

/// `device_token { id, end_current?, local_only?, urls? }` → a new pairing
/// token for device `key` (§5.2), in the shape `key_create` answers.
///
/// The row keeps its id, policy and history. `end_current: true` ends the
/// key it holds at once (a stolen device): its connections close, and its
/// requests are refused until the new token enrols. Left out or `false`, the
/// current key works until the owner confirms the key the new token enrols
/// (a move to a new phone), or renews its own certificate when the device
/// enrols the same key (before it lapses, §5.3). `local_only` and `urls`
/// replace the row's, which a row paired before TLS has none of.
pub(super) async fn token(
    st: &SharedState,
    key: &ApiKey,
    args: &Map<String, Value>,
) -> Result<dto::DevicePaired, Refusal> {
    let end_current = match args.get("end_current") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad_request("end_current is true or false")),
    };
    let patch: KeyPatch = crate::ops::patch_from_args(Some(args.clone())).map_err(bad_request)?;
    let local_only = local_only_arg(args)?;
    let link = match (local_only, patch.urls.as_deref()) {
        (None, None) if !key.link.urls.is_empty() => None,
        (local_only, urls) => {
            let local_only = local_only.unwrap_or(key.link.local_only);
            Some((urls_for(st, urls, local_only)?, local_only))
        }
    };
    let fp = listening(st)?;
    let (token, new) = new_token(st);
    let issued = store::new_device_token(
        &st.db,
        key.id,
        &new,
        end_current,
        link.as_ref().map(|(u, l)| (u.as_slice(), *l)),
    )
    .await
    .map_err(|e| bad_request(e.to_string()))?
    .ok_or_else(|| {
        bad_request(format!(
            "device {} disappeared before it could get a new token — nothing was changed",
            key.id
        ))
    })?;
    let reloaded = st.reload_snapshot().await.err();
    // The key it held is dead in the table: what it opened ends now, whether
    // or not the reload succeeded (review W2-6).
    if issued.ended_key.is_some() {
        if reloaded.is_some() {
            st.key_written(key.id, crate::state::KeyWritten::Pending);
        }
        st.devices.revocations.revoke(key.id, RevokeReason::Rotated);
    }
    let (urls, local_only) = link.unwrap_or_else(|| (key.link.urls.clone(), key.link.local_only));
    let paired = minted(st, key.id, &key.name, (urls, local_only), &fp, &token, &new).await;
    let bare = devices::short_name(&key.name);
    let current = match DeviceLink::credential(&key.key_hash) {
        DeviceCredential::Key(_) if issued.ended_key.is_some() => {
            " Its current key is ended: its connections are closed."
        }
        DeviceCredential::Key(_) if key.link.confirmed_at.is_some() => {
            " Its current key works until you confirm the key the device enrols with this \
             link; the same key enrolling again only renews its certificate."
        }
        DeviceCredential::Key(_) => {
            " Its enrolled key was never confirmed; the key the device enrols with this link \
             replaces it and waits for your confirmation."
        }
        DeviceCredential::V1 => {
            " It was paired before TLS: its old key opens nothing, so there was no key to end; \
             the device pairs again with this link."
        }
        DeviceCredential::Pending => "",
    };
    Ok(dto::DevicePaired {
        message: format!(
            "a new pairing link for device '{bare}': open or copy it now, it is not shown \
             again.{current}{}{}",
            reach_note(st, &paired.urls, local_only),
            unloaded(reloaded.as_ref())
        ),
        ..paired
    })
}

/// `device_token_end { id }`: end device `key`'s pairing token if it is
/// alive (the QR card's close, §3.3); a used one too, which ends its own
/// key's repeat. Its link is forgotten either way.
pub(super) async fn token_end(
    st: &SharedState,
    key: &ApiKey,
) -> Result<dto::DeviceTokenEnded, Refusal> {
    let ended = store::end_device_token(&st.db, key.id, Utc::now())
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    st.devices.pending.end(key.id);
    st.devices.minted.forget(key.id);
    let bare = devices::short_name(&key.name);
    if ended {
        st.telemetry.keys(key.id, "token");
    }
    Ok(dto::DeviceTokenEnded {
        ok: true,
        id: key.id,
        name: key.name.clone(),
        ended,
        message: if ended {
            format!("the pairing token of device '{bare}' is ended; its link pairs nothing")
        } else {
            format!("device '{bare}' had no pairing token alive; nothing was changed")
        },
    })
}

/// `device_confirm { id, confirm_code }`: the owner confirms device `key`'s
/// enrolled key (§3.5, D23) after comparing its code with the one the device
/// shows, and sends that code: the key waiting for the confirmation must
/// have it, so a card that showed an earlier enrolment's code, or a code
/// the owner did not compare, confirms nothing (409
/// [`code::CONFIRM_CODE_MISMATCH`]). Its next request is served. A key that
/// waited beside a kept one replaces it: the kept key ends, and what it held
/// open closes (§5.2).
pub(super) async fn confirm(
    st: &SharedState,
    key: &ApiKey,
    args: &Map<String, Value>,
) -> Result<dto::DeviceConfirmed, Refusal> {
    let bare = devices::short_name(&key.name).to_string();
    let typed =
        match args.get("confirm_code") {
            Some(Value::String(c)) => c.as_str(),
            None | Some(Value::Null) => return Err(bad_request(
                "confirm_code is required: the code the device shows (e.g. 7F3A-91C2), compared \
                 with the Keys list's pairing.confirm_code",
            )),
            Some(_) => return Err(bad_request("confirm_code is a string, e.g. \"7F3A-91C2\"")),
        };
    let Some(code_) = lmgw_api_types::device_link::parse_confirm_code(typed) else {
        return Err(bad_request(
            "confirm_code is eight hex digits, as the device shows it (e.g. 7F3A-91C2)",
        ));
    };
    let confirmed = store::confirm_device(&st.db, key.id, &code_, Utc::now())
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    let (key_hash, replaced, message) = match confirmed {
        store::Confirmed::Now { key_hash, replaced } => {
            if let Err(e) = st.reload_snapshot().await {
                tracing::warn!("device '{bare}' was confirmed, and the reload failed ({e})");
                // A kept key ended in the table: the snapshot must not keep
                // serving it until the next reload.
                if replaced.is_some() {
                    st.key_written(
                        key.id,
                        crate::state::KeyWritten::Confirmed {
                            key_hash: key_hash.clone(),
                        },
                    );
                }
            }
            if replaced.is_some() {
                st.devices.revocations.revoke(key.id, RevokeReason::Rotated);
            }
            st.telemetry.keys(key.id, "confirmed");
            let message = if replaced.is_some() {
                format!(
                    "device '{bare}' is confirmed with its new key: its next request is served, \
                     and the key it kept until now is ended, its connections closed"
                )
            } else {
                format!("device '{bare}' is confirmed: its next request is served")
            };
            (key_hash, replaced.is_some(), message)
        }
        store::Confirmed::Already { key_hash, at } => (
            key_hash,
            false,
            format!("device '{bare}' was confirmed already, at {at}; nothing was changed"),
        ),
        store::Confirmed::Mismatch => {
            return Err(Refusal {
                status: axum::http::StatusCode::CONFLICT,
                code: code::CONFIRM_CODE_MISMATCH,
                message: format!(
                    "{code_} is not the code of the key device '{bare}' enrolled: nothing was \
                     confirmed — compare with the code the device shows (the card may show an \
                     earlier enrolment: reload it)"
                ),
            })
        }
        store::Confirmed::NotEnrolled => {
            return Err(bad_request(format!(
                "device '{bare}' has not enrolled a key yet: there is no code to confirm — open \
                 its pairing link on the device first"
            )))
        }
        store::Confirmed::Missing => {
            return Err(bad_request(format!(
                "device {} disappeared before it could be confirmed — nothing was changed",
                key.id
            )))
        }
    };
    Ok(dto::DeviceConfirmed {
        ok: true,
        id: key.id,
        name: key.name.clone(),
        confirm_code: DeviceLink::confirm_code(&key_hash).unwrap_or_default(),
        replaced,
        message,
    })
}

/// `device_ca_rotate {}`: replace lmgw's device CA (§5.5, D19). A new key
/// under a new wrapping key and a new certificate replace `tls/ca.key` and
/// `tls/ca.crt`, the old key is not kept, the server certificates are issued
/// anew, every enrolled device loses its key and certificate (its policy
/// kept) and waits for a new token, and every token alive ends, since its
/// link pins the old CA. Each reset device's connections close.
pub(super) async fn ca_rotate(st: &SharedState) -> Result<dto::DeviceCaRotated, Refusal> {
    let ca = st.devices.trust.status(&st.data_dir).ca;
    if ca.state == dto::device_link::DeviceCaState::Missing {
        return Err(bad_request(
            "lmgw has no device CA to rotate: it is created when the device listener is first \
             turned on",
        ));
    }
    let new = st
        .devices
        .trust
        .replace(&st.data_dir)
        .await
        .map_err(|e| Refusal {
            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
            code: code::DEVICES_CA_UNAVAILABLE,
            message: format!("the device CA was not rotated: {e}"),
        })?;
    let reset = store::reset_devices_for_new_ca(&st.db, Utc::now())
        .await
        .map_err(|e| {
            bad_request(format!(
                "the device CA was rotated (fp {}), and the devices could not be reset: {e} — \
                 they fail to verify lmgw already; rotate again or delete them",
                new.fp()
            ))
        })?;
    let reloaded = st.reload_snapshot().await.err();
    devices::tokens::sync(st).await;
    st.devices.minted.forget_all();
    for (id, _) in &reset {
        if reloaded.is_some() {
            st.key_written(*id, crate::state::KeyWritten::Pending);
        }
        st.devices.revocations.revoke(*id, RevokeReason::Rotated);
        st.telemetry.keys(*id, "rotated");
    }
    let names: Vec<String> = reset
        .iter()
        .map(|(_, n)| devices::short_name(n).to_string())
        .collect();
    tracing::warn!(
        "the device CA was rotated by the owner: now {}; {} device(s) reset",
        new.facts().fp_short(),
        names.len()
    );
    Ok(dto::DeviceCaRotated {
        ok: true,
        fp: new.fp().to_string(),
        message: match names.len() {
            0 => "lmgw's device CA is new; no device was paired under the old one".to_string(),
            _ => format!(
                "lmgw's device CA is new: {} stopped connecting and must be paired again with \
                 a new token{}",
                names
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", "),
                unloaded(reloaded.as_ref())
            ),
        },
        devices_reset: names,
    })
}

/// The refusal `key_rotate` gives a device row (§5.2): it holds no secret
/// of lmgw's making.
pub(super) fn rotate_refusal(key: &ApiKey) -> Refusal {
    bad_request(format!(
        "'{}' is a device: it authenticates with a key of its own, so there is nothing of \
         lmgw's to rotate — give it a new pairing token (device_token, New token… on the Devices \
         card)",
        key.name
    ))
}

/// The refusal `key_reveal` gives a device row (L1, §11 Q5).
pub(super) fn reveal_refusal(key: &ApiKey) -> Refusal {
    bad_request(format!(
        "'{}' is a device: it authenticates with a key of its own, which never leaves it — \
         there is nothing to reveal; give it a new pairing token to pair it again",
        key.name
    ))
}

/// After a delete: a deleted key's connections and streams end now (§1.6) —
/// a device's, and every other kind's (*changed 2026-10-06*); a device's
/// token went with its row, so it is no longer pending (device link §5.1).
pub(super) fn deleted(st: &SharedState, key: &ApiKey) {
    devices::revoke(st, key.id, RevokeReason::Deleted);
    if key.kind == ApiKeyKind::Device {
        st.devices.pending.end(key.id);
    }
}

/// The device listener serves: the CA's `fp`, which a pairing link pins.
/// Refused 409 [`code::DEVICE_LISTENER_OFF`] otherwise, naming why.
fn listening(st: &SharedState) -> Result<String, Refusal> {
    let status = st.devices.listener.status(st);
    let off = |message: String| Refusal {
        status: axum::http::StatusCode::CONFLICT,
        code: code::DEVICE_LISTENER_OFF,
        message,
    };
    match status.state {
        DeviceListenerState::Listening => {}
        DeviceListenerState::Off => {
            return Err(off(
                "the device listener is off, so a pairing token would reach nothing: turn it on \
                 (devices.listener, Settings → Network & access → Devices; the pairing form does \
                 it with its confirmation), then pair"
                    .into(),
            ))
        }
        DeviceListenerState::Error => {
            return Err(off(format!(
                "the device listener does not serve, so a pairing token would reach nothing: {}",
                status.error.unwrap_or_default()
            )))
        }
    }
    st.devices
        .trust
        .ca()
        .map(|ca| ca.fp().to_string())
        .ok_or_else(|| off("the device listener serves without its CA loaded; try again".into()))
}

/// `local_only` as an op carried it.
fn local_only_arg(args: &Map<String, Value>) -> Result<Option<bool>, Refusal> {
    match args.get("local_only") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad_request("local_only is true or false")),
    }
}

/// `urls` as the op sent them, checked, or the form's proposal.
fn urls_for(
    st: &SharedState,
    urls: Option<&[String]>,
    local_only: bool,
) -> Result<Vec<String>, Refusal> {
    match urls {
        Some(urls) => devices::check_urls(urls).map_err(bad_request),
        None => devices::proposed_urls(&st.snapshot().settings, local_only).map_err(bad_request),
    }
}

/// A token and what the store keeps of it, lapsing `devices.token_minutes`
/// from now.
fn new_token(st: &SharedState) -> (String, store::NewToken) {
    let token = devices::tokens::mint();
    let now = Utc::now();
    let minutes = st.snapshot().settings.devices.token_minutes;
    let new = store::NewToken {
        hash: devices::tokens::hash(&token),
        created_at: now,
        expires_at: now + chrono::Duration::minutes(i64::from(minutes)),
    };
    (token, new)
}

/// The token written for device `id`: pending for the handshake, its link
/// the one lmgw's window may hand off, said on `/api/events`; the answer.
async fn minted(
    st: &SharedState,
    id: i64,
    name: &str,
    (urls, local_only): (Vec<String>, bool),
    fp: &str,
    token: &str,
    new: &store::NewToken,
) -> dto::DevicePaired {
    st.devices.pending.set(id, new.expires_at);
    let link = devices::pairing_link(devices::short_name(name), &urls, fp, token);
    // The one link lmgw's own window may hand to the desktop for it (W2-1).
    st.devices.minted.register(id, &link, new.expires_at);
    st.telemetry.keys(id, "token");
    dto::DevicePaired {
        ok: true,
        id,
        name: name.to_string(),
        link,
        urls,
        local_only,
        token_expires_at: store::stamp(new.expires_at),
        fp: fp.to_string(),
        message: String::new(),
    }
}

/// What the answer says when the device listener's scope cannot reach the
/// device (D24): a device that is not this computer, while the listener
/// listens on loopback only.
fn reach_note(st: &SharedState, urls: &[String], local_only: bool) -> String {
    let loopback = matches!(
        st.snapshot().settings.devices.listen,
        crate::config::DevicesListen::Loopback
    );
    if loopback && !(local_only || devices::all_loopback(urls)) {
        " The device listener listens on this computer only, so this device will not reach it \
         until it listens on every address (devices.listen, Settings → Network & access → \
         Devices)."
            .into()
    } else {
        String::new()
    }
}

/// What a reload that failed after the write leaves to say: the row is
/// written, and the gateway serves the previous configuration until the next
/// reload.
fn unloaded(e: Option<&crate::error::GatewayError>) -> String {
    match e {
        None => String::new(),
        Some(e) => format!(
            " (written, but the configuration could not be reloaded: {e} — it takes effect at \
             the next reload)"
        ),
    }
}
