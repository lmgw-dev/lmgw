//! `/transfers/api` (device transfers design §9): a paired device offers a
//! file to another device of the same app, uploads it, and the other device
//! downloads it — relayed through lmgw's disk (`crate::transfers`). Served
//! on both listeners behind `Cap::Transfer` (§10): the owner and paired
//! devices; the gate refuses everyone else 403 `transfer_device_only`.
//!
//! - **Who does what** (§1): the targets, the offer and the upload are the
//!   sender's; the download, `received` and `decline` the receiver's;
//!   `cancel` the sender's and the owner's; the list and a read anyone's
//!   who sees the transfer. The owner on a device's route is 403
//!   `transfer_device_only`; a device on a route that is not its part is
//!   404 `transfer_not_found`, as one that is no party is (I7).
//! - **The receiver is offered a transfer at `ready`**: its list and its
//!   feed carry it from then on. By id it is a party from the offer on, and
//!   its download, `received` and `decline` answer 409 `transfer_state`
//!   until the transfer is `ready`.
//! - **Refusals** are [`TransferRefusal`]s, the flat `{code, message}` with
//!   the extra field the code names (`refuse`).
//!
//! The content routes, which stream the file in and out, are `content`.

mod content;
mod refuse;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use chrono::Utc;
use lmgw_api_types::transfer::{
    self as wire, code, TargetsQuery, TransferCreate, TransferList, TransferListQuery,
    TransferRefusal, TransferRole, TransferState,
};

use super::chat_extract::{ChatPath, ChatQuery};
use crate::config::{ApiKey, ApiKeyKind, DeviceCredential, DeviceLink};
use crate::principal::{Principal, Refusal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::transfers::{self as table, EndsFrom, Listed, Offered, TransferRow};
use crate::transfers;

use refuse::{answer, bad_request, internal};

/// The routes. The content route reads its own body, the file, unbounded
/// but by the free space (§3); every other route's body is bounded at
/// [`wire::BODY_LIMIT_BYTES`] by its own reader.
pub(super) fn routes() -> Router<SharedState> {
    Router::new()
        .route("/transfers/api/targets", get(targets))
        .route("/transfers/api/transfers", get(list).post(create))
        .route("/transfers/api/transfers/{id}", get(read))
        .route(
            "/transfers/api/transfers/{id}/content",
            get(content::download)
                .put(content::upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/transfers/api/transfers/{id}/received", post(received))
        .route("/transfers/api/transfers/{id}/decline", post(decline))
        .route("/transfers/api/transfers/{id}/cancel", post(cancel))
}

/// Who a transfers request is (§1): the owner, or a paired device.
#[derive(Debug, Clone)]
pub(super) enum Party {
    Owner,
    Device {
        key_id: i64,
        /// Its paired name, without `device:`.
        name: String,
    },
}

impl Party {
    /// The device's key id and name; the owner is refused 403
    /// `transfer_device_only`, naming `what` it asked.
    fn device(&self, what: &str) -> Result<(i64, &str), Response> {
        match self {
            Self::Device { key_id, name } => Ok((*key_id, name)),
            Self::Owner => Err(answer(
                StatusCode::FORBIDDEN,
                &TransferRefusal::new(
                    code::TRANSFER_DEVICE_ONLY,
                    format!(
                        "only paired devices send and receive transfers: the owner lists them \
                         and cancels them, and does not {what}"
                    ),
                ),
            )),
        }
    }

    /// The device's key id; `None` for the owner.
    fn key_id(&self) -> Option<i64> {
        match self {
            Self::Device { key_id, .. } => Some(*key_id),
            Self::Owner => None,
        }
    }

    /// How an end it made names it.
    fn named(&self) -> String {
        match self {
            Self::Owner => crate::store::feed::BY_OWNER.to_string(),
            Self::Device { name, .. } => format!("device '{name}'"),
        }
    }
}

/// [`Party`] from the request's resolved principal. Behind the
/// `Transfer` gate only an owner key or a device arrives; anything else
/// fails closed.
pub(super) struct TransferCaller(pub Party);

impl<S: Send + Sync> FromRequestParts<S> for TransferCaller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let Some(ctx) = parts.extensions.get::<RequestCtx>() else {
            return Err(internal(
                "this transfers route was reached without a resolved caller",
            ));
        };
        match &ctx.principal {
            Principal::Key {
                kind: ApiKeyKind::Owner,
                ..
            } => Ok(Self(Party::Owner)),
            Principal::Key {
                id,
                kind: ApiKeyKind::Device,
                name,
                ..
            } => Ok(Self(Party::Device {
                key_id: *id,
                name: crate::devices::short_name(name).to_string(),
            })),
            other => Err(answer(
                StatusCode::FORBIDDEN,
                &TransferRefusal::new(
                    code::TRANSFER_DEVICE_ONLY,
                    format!(
                        "only paired devices send and receive transfers; the request presented {}",
                        other.describe()
                    ),
                ),
            )),
        }
    }
}

/// Which part of a transfer a route is (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Part {
    /// The list's and a read's: anyone who sees it.
    Read,
    /// The upload: the sender.
    Send,
    /// The download, `received`, `decline`: the receiver.
    Receive,
    /// `cancel`: the sender and the owner.
    Cancel,
}

/// Whether `p` reaches `row` by its id (§1): the owner every one, its two
/// devices theirs. (A receiver's list and feed carry it from `ready` on.)
fn sees(p: &Party, row: &TransferRow) -> bool {
    match p.key_id() {
        None => true,
        Some(k) => row.from_key_id == k || row.to_key_id == k,
    }
}

/// Transfer `id` as `p` reaches it for `part`, read with its lazy expiry
/// (I18); 404 `transfer_not_found` for no such transfer and for one `p`
/// does not reach that way.
pub(super) async fn reached(
    st: &SharedState,
    p: &Party,
    id: &str,
    part: Part,
) -> Result<TransferRow, Response> {
    let nothing = || {
        answer(
            StatusCode::NOT_FOUND,
            &TransferRefusal::new(
                code::TRANSFER_NOT_FOUND,
                match part {
                    Part::Read => format!("no transfer {id} this caller sees"),
                    Part::Send => format!("no transfer {id} this device uploads"),
                    Part::Receive => format!("no transfer {id} offered to this device"),
                    Part::Cancel => format!("no transfer {id} this caller may cancel"),
                },
            ),
        )
    };
    if !transfers::is_id(id) {
        return Err(nothing());
    }
    let row = match transfers::load(st, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(nothing()),
        Err(e) => return Err(internal(e.to_string())),
    };
    let me = p.key_id();
    let ok = sees(p, &row)
        && match part {
            Part::Read => true,
            Part::Send => me == Some(row.from_key_id),
            Part::Receive => me == Some(row.to_key_id),
            Part::Cancel => me.is_none() || me == Some(row.from_key_id),
        };
    if ok {
        Ok(row)
    } else {
        Err(nothing())
    }
}

/// 409 `transfer_state` for `row`, saying what it is and what `wanted`.
pub(super) fn wrong_state(row: &TransferRow, wanted: &str) -> Response {
    let state = transfers::state_of(row);
    let mut message = format!("transfer {} is {state}; {wanted}", row.id);
    if let Some(why) = &row.end_reason {
        message.push_str(&format!(" (it ended: {why})"));
    }
    answer(
        StatusCode::CONFLICT,
        &TransferRefusal {
            state: Some(state),
            ..TransferRefusal::new(code::TRANSFER_STATE, message)
        },
    )
}

/// Whether device row `k` can receive now (§9.1): enabled, enrolled and
/// confirmed, not past its key's expiry.
fn can_receive(k: &ApiKey, now: chrono::DateTime<Utc>) -> Result<(), &'static str> {
    if !k.enabled {
        return Err("is disabled");
    }
    if !matches!(
        DeviceLink::credential(&k.key_hash),
        DeviceCredential::Key(_)
    ) || k.link.confirmed_at.is_none()
    {
        return Err("is not paired and confirmed");
    }
    if crate::policy::check_expiry(k, now).is_err() {
        return Err("is past its key's expiry date");
    }
    Ok(())
}

/// `GET /transfers/api/targets?app_id=X` (§9.1).
async fn targets(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatQuery(q): ChatQuery<TargetsQuery>,
) -> Response {
    let me = match p.device("offer files") {
        Ok((me, _)) => me,
        Err(r) => return r,
    };
    if let Some(why) = wire::app_id_refusal(&q.app_id) {
        return bad_request(why);
    }
    let snap = st.snapshot();
    let now = Utc::now();
    let mut out = Vec::new();
    for k in snap.api_keys.iter().filter(|k| {
        k.kind == ApiKeyKind::Device
            && k.id != me
            && k.link.app_id.as_deref() == Some(q.app_id.as_str())
            && can_receive(k, now).is_ok()
    }) {
        let last_seen_at = match crate::store::key_last_seen(&st.db, k.id).await {
            Ok(at) => at,
            Err(e) => return internal(e.to_string()),
        };
        out.push(wire::Target {
            device_id: k.id,
            name: crate::devices::short_name(&k.name).to_string(),
            online: st.devices.links.online(k.id).contains(&"feed"),
            last_seen_at,
        });
    }
    let free_bytes = match transfers::available(&st).await {
        Ok(b) => b,
        Err(e) => return internal(e.to_string()),
    };
    answer(
        StatusCode::OK,
        &wire::Targets {
            targets: out,
            free_bytes,
            expiry_days: snap.settings.devices.transfer_expiry_days,
        },
    )
}

/// Characters an offer's text never carries: controls, and the format,
/// direction and line-separator characters that make a line read as
/// something else (`devices::is_invisible`).
fn hidden(c: char) -> bool {
    c.is_control() || crate::devices::is_invisible(c)
}

/// The offer's fields checked (§9.2): `title` with what would hide in it
/// dropped, trimmed; or the refusal naming the field.
fn checked(ask: &TransferCreate) -> Result<String, String> {
    if let Some(why) = wire::app_id_refusal(&ask.app_id) {
        return Err(format!("app_id: {why}"));
    }
    if ask.kind.trim().is_empty() || ask.kind.chars().any(hidden) {
        return Err(
            "kind is the app's own word for what the file is: required, without \
                    control or invisible characters"
                .into(),
        );
    }
    let f = &ask.filename;
    if f.is_empty() || f == "." || f == ".." || f.contains(['/', '\\']) || f.chars().any(hidden) {
        return Err(format!(
            "filename '{}' is no file name: required, without '/', '\\', control or invisible \
             characters, and not '.' or '..'",
            f.escape_debug()
        ));
    }
    if ask.size == 0 {
        return Err(
            "size is at least 1 byte: an upload's Content-Range names no empty range".into(),
        );
    }
    if i64::try_from(ask.size).is_err() {
        return Err(format!("size {} is past 2^63 - 1 bytes", ask.size));
    }
    if !wire::is_sha256_hex(&ask.sha256) {
        return Err("sha256 is the file's SHA-256 as 64 lowercase hex digits".into());
    }
    Ok(ask
        .title
        .chars()
        .filter(|c| !hidden(*c))
        .collect::<String>()
        .trim()
        .to_string())
}

/// `POST /transfers/api/transfers` (§9.2): 201 with the transfer,
/// `uploading`.
async fn create(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    req: Request,
) -> Response {
    let (me, my_name) = match p.device("offer files") {
        Ok((me, name)) => (me, name.to_string()),
        Err(r) => return r,
    };
    let ask: TransferCreate = match refuse::json_body(req.into_body()).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let title = match checked(&ask) {
        Ok(t) => t,
        Err(why) => return bad_request(why),
    };
    let snap = st.snapshot();
    let now = Utc::now();
    let invalid = |why: String| {
        answer(
            StatusCode::UNPROCESSABLE_ENTITY,
            &TransferRefusal::new(code::TRANSFER_TARGET_INVALID, why),
        )
    };
    if ask.to_device == me {
        return invalid(
            "to_device is this device itself: a transfer goes to another device".into(),
        );
    }
    let Some(to) = snap
        .api_keys
        .iter()
        .find(|k| k.id == ask.to_device && k.kind == ApiKeyKind::Device)
    else {
        return invalid(format!(
            "to_device {} is no device of this lmgw",
            ask.to_device
        ));
    };
    let to_name = crate::devices::short_name(&to.name).to_string();
    if to.link.app_id.as_deref() != Some(ask.app_id.as_str()) {
        return invalid(match &to.link.app_id {
            Some(other) => format!(
                "device '{to_name}' declared the app id {other}, not {}",
                ask.app_id
            ),
            None => format!(
                "device '{to_name}' declared no app id at pairing, so it receives no transfer"
            ),
        });
    }
    if let Err(why) = can_receive(to, now) {
        return invalid(format!("device '{to_name}' {why}"));
    }
    // One offer at a time from the space check to the insert (§3).
    let _offers = st.transfers.offers.lock().await;
    let free = match transfers::available(&st).await {
        Ok(b) => b,
        Err(e) => return internal(e.to_string()),
    };
    if ask.size > free {
        return refuse::storage_full(
            free,
            format!(
                "the file is {} bytes and lmgw has {free} bytes left for new offers (the free \
                 space of its data directory, less what uploads in progress still need)",
                ask.size
            ),
        );
    }
    let days = snap.settings.devices.transfer_expiry_days;
    let id = transfers::new_id();
    let offered = table::insert(
        &st.db,
        &table::NewTransfer {
            id: &id,
            app_id: &ask.app_id,
            kind: ask.kind.trim(),
            title: &title,
            filename: &ask.filename,
            size: i64::try_from(ask.size).unwrap_or(i64::MAX),
            sha256: &ask.sha256,
            from_key_id: me,
            from_name: &my_name,
            to_key_id: to.id,
            to_name: &to_name,
            created_at: now,
            expires_at: now + chrono::Duration::days(i64::from(days)),
        },
    )
    .await;
    match offered {
        Ok(Offered::Made(row)) => {
            transfers::said(&st, &row);
            answer(StatusCode::CREATED, &transfers::wire_with(&row, 0))
        }
        // The caller is no device any more: the refusal the gate gives its
        // next request (I29), not one about the target.
        Ok(Offered::SenderGone) => {
            axum::response::IntoResponse::into_response(Refusal::device_key_unknown())
        }
        Ok(Offered::SenderDisabled(name)) => {
            axum::response::IntoResponse::into_response(Refusal::device_disabled(&name))
        }
        Ok(Offered::Refused(why)) => invalid(why),
        Err(e) => internal(e.to_string()),
    }
}

/// `GET /transfers/api/transfers[?role=…]` (§9.3).
async fn list(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatQuery(q): ChatQuery<TransferListQuery>,
) -> Response {
    let which = match (&p, q.role) {
        (Party::Owner, None) => Listed::All,
        (Party::Owner, Some(role)) => {
            return bad_request(format!(
                "role={} is a device's: the owner is no party to a transfer, and lists every \
                 one without it",
                role.as_str()
            ));
        }
        (Party::Device { key_id, .. }, None) => Listed::Both(*key_id),
        (Party::Device { key_id, .. }, Some(TransferRole::Incoming)) => Listed::Incoming(*key_id),
        (Party::Device { key_id, .. }, Some(TransferRole::Outgoing)) => Listed::Outgoing(*key_id),
    };
    // What is overdue now is expired before it is listed (I18).
    if let Ok(ids) = table::overdue(&st.db, Utc::now()).await {
        for id in ids {
            let _ = transfers::load(&st, &id).await;
        }
    }
    let rows = match table::list(&st.db, which).await {
        Ok(r) => r,
        Err(e) => return internal(e.to_string()),
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(transfers::wire(&st, row).await);
    }
    let (stored_bytes, free_bytes) = match (
        transfers::stored_bytes(&st).await,
        transfers::available(&st).await,
    ) {
        (Ok(s), Ok(f)) => (s, f),
        (Err(e), _) | (_, Err(e)) => return internal(e.to_string()),
    };
    answer(
        StatusCode::OK,
        &TransferList {
            transfers: out,
            stored_bytes,
            free_bytes,
        },
    )
}

/// `GET /transfers/api/transfers/{id}`: the transfer, its resume point in
/// `received`.
async fn read(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatPath(id): ChatPath<String>,
) -> Response {
    match reached(&st, &p, &id, Part::Read).await {
        Ok(row) => answer(StatusCode::OK, &transfers::wire(&st, &row).await),
        Err(r) => r,
    }
}

/// `POST …/received`: the receiver checked the bytes; `delivered`, the
/// bytes deleted.
async fn received(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatPath(id): ChatPath<String>,
) -> Response {
    if let Err(r) = p.device("receive files") {
        return r;
    }
    finish(
        &st,
        &p,
        &id,
        TransferState::Delivered,
        None,
        "only a ready transfer is received",
    )
    .await
}

/// `POST …/decline`: the receiver does not want it; `declined`, the bytes
/// deleted.
async fn decline(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatPath(id): ChatPath<String>,
) -> Response {
    if let Err(r) = p.device("decline files") {
        return r;
    }
    let why = format!("declined by {}", p.named());
    finish(
        &st,
        &p,
        &id,
        TransferState::Declined,
        Some(&why),
        "only a ready transfer is declined",
    )
    .await
}

/// `POST …/cancel`: the sender or the owner ends it; `canceled`, the bytes
/// deleted.
async fn cancel(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatPath(id): ChatPath<String>,
) -> Response {
    let row = match reached(&st, &p, &id, Part::Cancel).await {
        Ok(row) => row,
        Err(r) => return r,
    };
    if !row.is_active() {
        return wrong_state(&row, "only a transfer in progress is canceled");
    }
    let why = format!("canceled by {}", p.named());
    ended(
        &st,
        &row,
        transfers::end(
            &st,
            &id,
            EndsFrom::Active,
            TransferState::Canceled,
            Some(&why),
        )
        .await,
        "only a transfer in progress is canceled",
    )
    .await
}

/// The receiver's end of a `ready` transfer: `to`, for `reason`.
async fn finish(
    st: &SharedState,
    p: &Party,
    id: &str,
    to: TransferState,
    reason: Option<&str>,
    wanted: &str,
) -> Response {
    let row = match reached(st, p, id, Part::Receive).await {
        Ok(row) => row,
        Err(r) => return r,
    };
    if row.state != "ready" {
        return wrong_state(&row, wanted);
    }
    ended(
        st,
        &row,
        transfers::end(st, id, EndsFrom::Ready, to, reason).await,
        wanted,
    )
    .await
}

/// An end's answer: the transfer as it ended, or 409 with the state it
/// moved to meanwhile.
async fn ended(
    st: &SharedState,
    before: &TransferRow,
    ended: crate::store::DbResult<Option<TransferRow>>,
    wanted: &str,
) -> Response {
    match ended {
        Ok(Some(row)) => answer(StatusCode::OK, &transfers::wire(st, &row).await),
        Ok(None) => match table::get(&st.db, &before.id).await {
            Ok(Some(now)) => wrong_state(&now, wanted),
            Ok(None) => wrong_state(before, wanted),
            Err(e) => internal(e.to_string()),
        },
        Err(e) => internal(e.to_string()),
    }
}
