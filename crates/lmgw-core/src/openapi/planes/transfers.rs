//! `/transfers/api/*`, transfers between devices (device transfers design
//! §9): `Cap::Transfer` in `CAPABILITY_TABLE`, the owner and paired
//! devices, on both listeners. The shapes are `lmgw-api-types`' `transfer`
//! module, the types the handlers read and write; a refusal is a
//! `TransferRefusal`, the flat `{code, message}` with the extra field its
//! code names.

use lmgw_api_types::transfer as t;

use super::super::registry::{Dialect, DocRoute, Req, Resp, SchemaFn};

/// The refusals with an extra field, documented beside the dialect's error.
const REFUSALS: &[(u16, SchemaFn)] = &[
    (409, |g| g.root_schema_for::<t::TransferRefusal>()),
    (507, |g| g.root_schema_for::<t::TransferRefusal>()),
];

fn route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "transfers",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Json(|g| g.root_schema_for::<t::Transfer>()),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        errors: &[],
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            query: Some(|g| g.root_schema_for::<t::TargetsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<t::Targets>()),
            ..route(
                "GET",
                "/transfers/api/targets",
                "Who this device can offer a file to",
                "A device's: its other devices that declared app_id at pairing and can receive \
                 now (enabled, confirmed, not past their key's expiry), never itself, each with \
                 whether it holds its Chat feed open (online) and when it was last seen; with \
                 free_bytes, the largest offer lmgw takes now (the free space of its data \
                 directory less what uploads in progress still need, the only size bound), and \
                 expiry_days, how long an offer made now lasts (devices.transfer_expiry_days). \
                 400 bad_request for an app_id that is no lowercase reverse-DNS name; 403 \
                 transfer_device_only for the owner.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<t::TransferListQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<t::TransferList>()),
            ..route(
                "GET",
                "/transfers/api/transfers",
                "List transfers",
                "Newest first, the ended ones included for chat_feed_retention_days after they \
                 ended. A device lists its outgoing ones (role=outgoing), the incoming ones it \
                 was offered (role=incoming, from ready on), or both; the owner every one, \
                 without role (400 bad_request with one). A receiver lists its pending offers \
                 at its start and follows the Chat feed's transfer.updated for the rest. \
                 stored_bytes is what the files of transfers in progress hold, free_bytes the \
                 largest offer lmgw takes now.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<t::TransferCreate>()),
            response: Resp::CreatedJson(|g| g.root_schema_for::<t::Transfer>()),
            errors: REFUSALS,
            ..route(
                "POST",
                "/transfers/api/transfers",
                "Offer a file to another device",
                "A device offers a file to to_device, a target for app_id: 201 Created with the \
                 transfer, uploading and nothing received; upload its bytes next. The receiver \
                 does not see it until every byte is there and matched sha256. It lasts \
                 devices.transfer_expiry_days (expires_at). 400 bad_request for a malformed \
                 field (kind required, filename without '/', '\\' or control characters, size \
                 at least 1, sha256 64 lowercase hex digits), 403 transfer_device_only for the \
                 owner, 413 body_too_large over 16 KiB, 422 transfer_target_invalid when \
                 to_device is no target for app_id (the message says which part failed), 507 \
                 transfer_storage_full with free_bytes when size is above the space left for \
                 new offers. A sender disabled or deleted while its offer was read is answered \
                 as the device listener answers its next request: 401 device_disabled or 401 \
                 device_key_unknown.",
            )
        },
        route(
            "GET",
            "/transfers/api/transfers/{id}",
            "Read a transfer",
            "The transfer as it is now: while it uploads, received is the resume point (the \
             bytes lmgw holds, which the next upload's range starts at). 404 \
             transfer_not_found for no such transfer and for a device that is neither its \
             sender nor its receiver (a receiver lists it and hears of it from ready on, and \
             reads it by id from the offer on).",
        ),
        DocRoute {
            request: Req::Raw("application/octet-stream"),
            response: Resp::Json(|g| g.root_schema_for::<t::ContentReceived>()),
            errors: REFUSALS,
            ..route(
                "PUT",
                "/transfers/api/transfers/{id}/content",
                "Upload a transfer's bytes",
                "The sender's, while the transfer is uploading. Content-Range: bytes a-b/size \
                 is required, size the offer's and a the transfer's received; the body is the \
                 range's raw bytes (any Content-Type), streamed to lmgw's disk. Chunks are any \
                 size; a body that breaks off keeps every byte that arrived, so received (read \
                 the transfer) is where to go on. A newer upload of the same transfer stops one \
                 still being read. Answers {received, state}; the last byte has the file's \
                 sha256 checked: state ready (offered to the receiver), or 422 \
                 transfer_hash_mismatch, the transfer failed and its bytes deleted. When lmgw \
                 already holds every byte (the last range's answer was lost), any upload of it \
                 completes it the same, whatever range it names; Content-Range: bytes */size \
                 with no body is the form that asks only that: where the upload stands, and its \
                 completion when it is whole. 400 bad_request for a missing or malformed \
                 Content-Range, another total, a Content-Length that is not b-a+1, or a body \
                 shorter or longer than its range when that was not the last byte (what arrived \
                 of the range is kept); 404 transfer_not_found but for the sender; 409 \
                 transfer_offset with received when a is not it (or a newer upload stopped this \
                 one); 409 transfer_state with state once it is not uploading, also for an \
                 upload a cancel, an expiry or a device's end stopped (ready: it is done); 507 \
                 transfer_storage_full with free_bytes when the disk filled.",
            )
        },
        DocRoute {
            response: Resp::Binary(&["application/octet-stream"]),
            errors: REFUSALS,
            ..route(
                "GET",
                "/transfers/api/transfers/{id}/content",
                "Download a transfer's bytes",
                "The receiver's, while the transfer is ready: the file, streamed from lmgw's \
                 disk, with Accept-Ranges: bytes, ETag \"sha256:<hex>\" and \
                 Content-Disposition naming its filename. One Range (bytes=a-b, bytes=a- or \
                 bytes=-n) answers 206 with Content-Range; If-Range with the ETag serves the \
                 range, with anything else the whole file. A Range lmgw does not read (another \
                 unit, several ranges, an invalid one) is ignored: the whole file, 200. 416 \
                 range_not_satisfiable, with Content-Range: bytes */size, for a range that names \
                 no byte of the file (it starts at or past the end, or asks the last 0 bytes). \
                 404 transfer_not_found but for the receiver; 409 transfer_state with state \
                 when it is not ready. A transfer that ends while its bytes stream (a cancel, an \
                 expiry, a device's end) breaks the body off short of its length. HEAD answers \
                 the same head without the body. After its own hash check the receiver sends \
                 …/received.",
            )
        },
        route(
            "POST",
            "/transfers/api/transfers/{id}/received",
            "Say a transfer arrived",
            "The receiver's, after its own check of the bytes: the transfer is delivered and \
             lmgw deletes its bytes; the sender hears it on the feed. 404 transfer_not_found \
             but for the receiver; 409 transfer_state with state when it is not ready.",
        ),
        route(
            "POST",
            "/transfers/api/transfers/{id}/decline",
            "Decline a transfer",
            "The receiver's: the transfer is declined and lmgw deletes its bytes; the sender \
             hears it on the feed. 404 transfer_not_found but for the receiver; 409 \
             transfer_state with state when it is not ready.",
        ),
        DocRoute {
            confirm_note: Some("The transfer ends and lmgw deletes its bytes."),
            ..route(
                "POST",
                "/transfers/api/transfers/{id}/cancel",
                "Cancel a transfer",
                "The sender's and the owner's, while the transfer is uploading or ready: it is \
                 canceled, an upload in progress stops (answered 409 transfer_state, canceled), \
                 a download in progress breaks off, lmgw deletes its bytes, and both devices \
                 hear it on the feed. 404 transfer_not_found for any other device; 409 \
                 transfer_state with state once it ended.",
            )
        },
    ]
}
