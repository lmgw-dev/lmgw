//! `/devices/*`, the device listener's own routes (device link design §3.4,
//! §4.6): `Public` in `CAPABILITY_TABLE`, since the pairing token decides,
//! and on the device listener's face only.

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use lmgw_api_types::device_link::{EnrolRequest, Enrolment};

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![DocRoute {
        method: "POST",
        path: "/devices/enrol",
        tag: "devices",
        summary: "Enrol a device's key with a pairing token",
        description: "A device sends the pairing link's one-time token with its public key \
            (an EC P-256 key as its DER SubjectPublicKeyInfo, standard base64) and the client \
            it is, over TLS 1.3 pinned to the link's fp and without a client certificate (one \
            presented anyway, and any credential header, is ignored: the token decides). It \
            gets its certificate (clientAuth, for that key, valid devices.client_cert_days), \
            lmgw's device CA, the dates, its URLs and the confirmation code of its key, 201 \
            Created. Every request of the device is then refused 403 device_unconfirmed until \
            the owner confirms that code on the Devices card. The same key presenting the \
            same token again while it lives (devices.token_minutes from its minting, unless \
            the owner ended it) gets the same answer; another key gets 410 pairing_token_used \
            and the device is marked contested. Refusals: 400 bad_request (not this shape), \
            400 public_key_unsupported (not a canonical EC P-256 SubjectPublicKeyInfo), 401 \
            pairing_token_unknown, 401 device_disabled (the token stays unused), 409 \
            public_key_in_use (another device enrolled the key), 410 pairing_token_used, 410 \
            pairing_token_expired (lapsed or ended; the message says which and names \
            devices.token_minutes), 413 body_too_large (over 16 KiB), 429 \
            pairing_rate_limited with Retry-After (failed enrolments from the address, an \
            IPv6 address by its /64, past devices.enrol_refuse_per_min in a minute), 503 \
            devices_ca_unavailable. Served on the device listener only: the plain listener \
            answers it 404. While no token lives, the device listener's handshake requires a \
            client certificate, so a device without one meets the TLS alert \
            certificate_required instead (lmgw-client: this pairing link has ended).",
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::Json(|g| g.root_schema_for::<EnrolRequest>()),
        response: Resp::CreatedJson(|g| g.root_schema_for::<Enrolment>()),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        errors: &[],
        example: None,
    }]
}
