# Device-to-device transfers: offers relayed through lmgw

**Approved, with the go-ahead to build, 2026-10-21.** The design under "Decided" is the owner's,
approved for the first client, a game on the desktop and on Android (a Tauri 2 app, every
install a paired device). "Implementation decisions" (I1 to I31) are this record's own, taken while
building (I22 to I31, and the changes marked 2026-10-22, in the review's two fix rounds); each
stands until the owner changes it. "Not yet" lists what the first version leaves
out; nothing there is ruled out.

Companion records, cited by short name:
- "device link §n": [the device link](2026-10-14-device-link-design.md) (pairing, the device
  listener, the feed as push channel);
- "client-apps §n": [lmgw for client apps](2026-10-06-client-apps-design.md) (device keys, the Chat
  change feed);
- "principals §n": [principals, origins and mounts](2026-09-22-principals-origins-mounts-design.md)
  (capabilities and the gate).

**What this record covers.** A paired device (A) offers a file — one playthrough archive, a few MB
to a few hundred MB — to another paired device of the same app (B). A uploads the bytes to lmgw;
lmgw checks them and offers them to B; B downloads them when it runs, says it got them, and lmgw
deletes them. lmgw stays generic: it knows an app by the id the app declared at pairing, never what
the file means.

| § | Content |
|---|---|
| 1 | Overview: relay, states, who sees what |
| 2 | The app id |
| 3 | Storage: the files, free space, crash safety |
| 4 | The upload |
| 5 | The download |
| 6 | The feed event |
| 7 | Expiry and retention |
| 8 | Devices that end |
| 9 | The API and its refusals |
| 10 | The capability |
| 11 | Surfaces: the Devices card, Settings, `lmgw-client` |
| 12 | Tests |

## Decided by the owner (2026-10-21)

1. **Relay through lmgw** (store and forward on lmgw's disk), not direct between devices: phones do
   not listen, B may be offline, and lmgw is the one endpoint both reach.
2. **The app id is declared at pairing.** `EnrolRequest` gains an optional `app_id`, beside the
   free-text `client`; it is stored on the device row, shown on the Devices card and carried on the
   owner's `KeyRow`. Every install is its own device row (Kai and the game on one phone are two
   devices). A device without an app id cannot receive. Its format is checked, and a refusal states
   the rule. `lmgw-client`'s enrolment builder takes it.
3. **The offer reaches B only once the upload is complete and lmgw verified its sha256** (state
   `ready`). A sees its own upload's progress.
4. **Offers expire.** A setting with a human label, default 7 days; every offer carries
   `expires_at`; the hourly tick expires overdue offers and deletes their bytes, and both sides
   hear the new state. How long ended rows stay listed is tied to an existing visible retention.
5. **No hidden size cap.** The only cap is the free space of the data directory's filesystem, less
   what uploads in progress still need; the targets answer reports it as `free_bytes`, and an offer
   larger than that is refused 507 `transfer_storage_full` carrying `free_bytes`.
6. **Only device principals send or receive.** The owner lists every transfer and cancels any, on
   the Devices card, after a confirmation. Plain API keys are refused.
7. **A device revoked or deleted ends its transfers** (canceled, with a reason), their bytes are
   deleted, and the other side hears it.
8. **The API below was promised to the first client** and is built as written (§9).

Standing rules that bind this design: no backward compatibility on lmgw's own wire (alpha); no
hidden limits (every bound is a visible setting, a stated rule or a real quantity reported to the
client); destructive actions confirm, then really happen.

## 1. Overview

```
A (sender)                         lmgw                                   B (receiver)
POST /transfers  ──────────────▶  row: uploading, expires_at
PUT  …/content  (chunks) ───────▶  <data_dir>/transfers/<id>
PUT  …/content  (last chunk) ───▶  sha256 checked → ready ──feed: transfer.updated──▶  (online or later)
                                                         ◀── GET …/content (Range)
                                                         ◀── POST …/received
                                   delivered, bytes deleted ──feed──▶ A
```

**States.** `uploading` → `ready` (the last byte arrived and the sha256 matched) or `failed` (it did
not: 422 `transfer_hash_mismatch`). From `uploading` or `ready`: `canceled` (the sender, the owner,
or a device that ended, §8) and `expired` (§7). From `ready`: `delivered` (the receiver's `received`)
and `declined` (the receiver's `decline`). `delivered`, `declined`, `canceled`, `expired` and
`failed` are ended: the bytes are deleted when a transfer ends, `ended_at` is set, and `end_reason`
says why in words for every end but `delivered`.

**Who sees a transfer.** The sender, always. The receiver is *offered* it at `ready`: its list and
its feed carry the transfer from then on, never before. By id it is a party from the offer on (I21):
a read shows the upload's progress, and its download, `received` and `decline` answer 409
`transfer_state` until `ready`. The owner, every one. Any other device, never: 404
`transfer_not_found`, the same answer as for an id that does not exist.

**Who does what.** The upload is the sender's; the download, `received` and `decline` are the
receiver's; `cancel` is the sender's and the owner's. A device asking a route that is not its part
is answered 404 `transfer_not_found`, as a device that is no party is (I7). The owner on a device's
route (targets, create, upload, download, received, decline) is answered 403
`transfer_device_only`.

## 2. The app id

The enrolment's `app_id` names the app the install is, as a reverse-DNS name in lowercase: two or
more labels separated by dots, each starting with a letter `a`–`z` and then letters, digits, `-` or
`_` (`dev.example.storyplayer`), at most 253 characters (a DNS name's length). Anything else is
refused 400 `bad_request` before the pairing token is used, with that sentence
(`lmgw_api_types::transfer::app_id_refusal`). It is optional; an enrolment without one stores none.

- **Stored on the row** (`api_keys.app_id`, migration 0088), written by every enrolment that makes
  or renews the row's key. A **new key that waits beside a confirmed one** (a re-pair that kept the
  current key, device link §5.2) writes `waiting_app_id` instead, and the owner's Confirm moves it
  to `app_id` with the key (I3): an enrolment nobody confirmed never changes what the working
  device receives.
- **Shown** on the Devices card beside the device's name, and on the owner's `KeyRow.app_id`.
- **Used** by the targets list (§9.1) and the offer's check (§9.2): an offer names an app id, and
  its receiver must have declared that one. A device without an app id is never a target.
- **Not trusted for identity.** It is a routing label the device declares, like `client`; what
  authenticates is the device's certificate. Two devices may declare the same app id (that is the
  point), and the sender's own app id is not checked (I5).

## 3. Storage

**Files.** One file per transfer, `<data_dir>/transfers/<id>`: the directory `0700`, the file
`0600`, created with its mode before a byte is written (the lab artifacts' rules). The id is 32
random lowercase hex digits (128 bits, opaque; not a sequence), so a file name is never a path.

**The resume point is the file's length.** While a transfer uploads, its `received` is the length
of its file on disk: a body that breaks off keeps every byte that reached lmgw, a restart of lmgw
keeps them too, and the next `PUT` continues there. Every write goes to its own offset (the range's
first byte plus what was written of it), never appended (I22). A `PUT` syncs the file before it
answers, and the first write that makes the file syncs the `transfers/` directory too (and the data
directory, when it made `transfers/`), so a `received` an answer gave survives a crash. A crash may
leave a tail written after the last sync that is not what the sender sent; the sha256 check at
completion catches it (the offer is then `failed`, and the sender offers again). The row's
`received` column is the file's length as of the last `PUT`, and the value an ended transfer keeps.

**Free space** is read with `statvfs` for the data directory (`backends::paths::free_bytes`), and
**available** for a new offer is that less what every `uploading` transfer still needs (its `size`
less its file's length). The targets answer and the list answer report it as `free_bytes`; an offer
whose `size` is above it is 507 `transfer_storage_full` with `free_bytes`. Offers are checked one at
a time (an in-process lock around the check and the insert), so two offers cannot both take the
same free space. A write that meets a full disk anyway (something else filled it) keeps what was
written and is answered 507 `transfer_storage_full` with `free_bytes` as it is then. There is no
other size cap.

**Deleting.** A transfer's bytes go when it ends, and a download that still streams them breaks off
and lets go of the file, so its disk space goes too (I26). The hourly tick also removes any file in the directory whose name is a transfer id and whose
transfer is not `uploading` or `ready` (a crash between an end's commit and its unlink); it touches
no other name.

**Completing what nobody finished.** An `uploading` transfer whose file holds every byte is
completed by whoever finds it so (I25): its next `PUT` (§4), and the tick, whose first pass runs as
lmgw starts, for a file whose upload nobody runs (an upload that runs completes it itself). A stop
between the last write and the completion, or a hash that failed to read, never leaves a transfer
that nothing can finish.

## 4. The upload

`PUT /transfers/api/transfers/{id}/content`, by the sender, while the transfer is `uploading`:

- `Content-Range: bytes a-b/size` is required: `size` must be the offer's, `a ≤ b < size`, and `a`
  must equal the current `received`, else 409 `transfer_offset` carrying `received`. A malformed
  header, another total, or a `Content-Length` other than `b − a + 1` is 400 `bad_request`. (A
  `Content-Length` that is not a number never reaches lmgw's routes: hyper refuses the request
  before routing with a bare 400 and no JSON body.)
- **`Content-Range: bytes */size` with no body asks where the upload stands** (I25): `{received,
  state: uploading}`, and when lmgw holds every byte, the completion's answer below. It is how a
  client whose last range's answer was lost finishes (`lmgw-client`'s `upload_status`). It takes the
  upload slot as an upload does, so it **stops an upload of the transfer that still runs** (that one
  is answered as a newer `PUT`'s stop says, below): a client asks it once its own upload's answer
  was lost, not beside an upload it runs.
- The body is raw bytes, any `Content-Type`; it is streamed to the file as it arrives, never held
  whole. Chunks are any size; the whole file in one `PUT` is fine.
- **A body that breaks off** keeps what was written (synced); `GET /transfers/api/transfers/{id}`
  then says the resume point. A body that ends before `b` is 400 `bad_request` after its bytes
  were kept; bytes past `b` are not written, and the answer is 400 `bad_request` — unless the
  range's last byte was the file's: then the upload completes as below whatever the body did after
  it (I25).
- **A newer `PUT` for the same transfer stops an older one** that is still being read (a client that
  gave up on a stalled connection and retried): the older one is answered 409 `transfer_offset` if
  anyone still listens, and the newer one checks `a` against the length the older one left (I9).
  The older one lets go only once what it wrote is synced and counted, so the newer one reads the
  real length (I22). An older one that had written the file's last byte completes the file before
  it lets go and is answered as the completion below; the newer one then finds the transfer
  `ready` (409 `transfer_state`) or `failed` (I25).
- **An end stops an upload too** (a cancel, an expiry, a device's Disable or Delete, §8): it is
  answered 409 `transfer_state` with the state the end left, `canceled` or `expired`, and its
  reason (I23).
- **The answer** is `{received, state}`, `received` synced to the disk in every answer (I11, I31).
  When `received` reaches `size`, lmgw syncs and hashes the
  whole file (about a second for a few hundred MB) before it answers: `ready`, or `failed` with
  422 `transfer_hash_mismatch` and the bytes deleted. **A `PUT` that finds every byte there
  already** — whatever range it names — gives that same answer without reading its body (I25). A
  `PUT` once the transfer is `ready` is 409 `transfer_state` with `ready`: a client takes that as
  done.
- While a `PUT` writes, the owner's Devices card hears its progress at most once a second (§11.1);
  A reads its own progress from what it sent and, after a break, from `received`.

The listeners stop as they always do (a 10 s drain, then the connections go, device link §4.4):
an upload cut by a stop resumes like any other that broke off.

## 5. The download

`GET /transfers/api/transfers/{id}/content`, by the receiver, while the transfer is `ready`; any
other state is 409 `transfer_state` carrying `state`. Streamed from the file, never read whole.

- `Accept-Ranges: bytes`, `ETag: "sha256:<hex>"` (strong: the bytes are fixed once `ready`),
  `Content-Type: application/octet-stream`, `Content-Disposition: attachment` with the offer's
  `filename` (RFC 6266, `filename*` in UTF-8).
- `Range: bytes=a-b`, `bytes=a-` or `bytes=-n` (the unit in any case): one range, answered 206 with
  `Content-Range`. One that names no byte of the file — it starts at or past the end, or asks the
  last 0 bytes — is 416 with `Content-Range: bytes */<size>` and the flat refusal
  `range_not_satisfiable`. A `Range` lmgw does not read is ignored and the whole file served, 200
  (RFC 9110 §14.2, I27): another unit (`items=…`), several ranges, an invalid one (its last byte
  before its first, a number past 2⁶⁴ − 1), a header that does not parse.
- `If-Range` with the ETag serves the range; with anything else (another ETag, a date — lmgw keeps
  no `Last-Modified`) the whole file, 200.
- After its own hash check the receiver sends `POST …/received`: `delivered`, the bytes deleted.
  `POST …/decline` says it does not want them: `declined`, the bytes deleted.
- **A transfer that ends while its bytes stream** (a cancel, an expiry, a device's end, the
  receiver's own `received` from another connection) breaks the body off short of its
  `Content-Length`, and lmgw lets go of the file at once — also for a receiver that stopped reading
  and keeps its connection open (I26). One that ended between the state check and the file's
  opening is 409 `transfer_state` with the state it left.
- `HEAD` answers the whole file's head without the body: 200 with `Content-Length` (the file's
  size), `ETag`, `Accept-Ranges` and the rest of the head above. It ignores `Range` and `If-Range`
  (RFC 9110 §14.2 reads `Range` on `GET` only), so it is never 206 or 416 (I27).

## 6. The feed event

`transfer.updated`, data the `Transfer` as it is when the event is delivered (the feed's rule for
every stored event, client-apps §2.3), is a **stored** event of `GET /chat/api/feed`: it has a
cursor, and a device that was away gets it in its catch-up. It is recorded in the transaction of
each state change — the offer's creation, `ready`, and every end — never per chunk (I12). It is
rendered for:

- the **sender**, every record;
- the **receiver**, records written once the transfer had been `ready` (the record carries whether
  it was offered by then, so a transfer canceled during its upload is never heard of by B);
- the **owner** (the dashboard, owner keys), every record.

No other device receives it, live or in a catch-up. A record whose transfer row is gone (pruned,
§7) renders as nothing. The record names no thread or folder; its level is 0 and the self-admin
reach rules do not apply to it (`web::chat_feed::transfers` decides before the thread logic, as
`device.revoked` does).

## 7. Expiry and retention

- **Expiry**: `devices.transfer_expiry_days`, Settings → Network & access → Devices, label
  "Transfer offers last", default 7 days, from 1 to 3650 (I14). It applies to the next offer;
  every offer carries its `expires_at` from its creation. A transfer still `uploading` or `ready`
  at `expires_at` is `expired` and its bytes deleted: by the hourly tick, and at once by any
  request that touches it after that moment (so a download an hour late is refused, not served).
  `end_reason` says how long the offer lasted ("not delivered within 7 days of the offer", "its
  upload did not finish within 7 days of the offer"), from the row's own times; no setting's key and
  no raw time (I17).
- **Ended rows stay listed for `chat_feed_retention_days` after `ended_at`** (Settings → Chat →
  Change feed → retention, default 7; `0` keeps them), then the hourly tick deletes them. The
  feed's records of a transfer are pruned by the same setting, so a client replaying the feed
  finds the row its events name (I15).

## 8. Devices that end

**Disable** and **Delete** on the Devices card end every transfer the device sends or receives that
is `uploading` or `ready`: `canceled`, `end_reason` "device 'phone' was disabled" or "… was
deleted", the bytes deleted, a `transfer.updated` for each (the other side hears it by §6's rule).
A disabled device enabled again finds them ended. Re-pairing (a new token, a renewal) and a key's
own expiry date end nothing: the row and its id stay, and the offers' own expiry applies (I16) —
but for one change: **a re-enrolment that changes the device's app id** (to another, or to none)
ends every transfer offered to it that is in progress and names the old one, `canceled` with
"device 'phone' paired again as another app (…)" or "… without an app id", the bytes deleted and
the sender told (I28). For a waiting key that is the owner's Confirm (§2). What the device sends
goes on (I5). The hourly tick also ends a transfer whose party's row is gone or disabled, or whose
receiver declares another app id now, so a crash between a key's write and the transfers' end
leaves nothing open.

## 9. The API and its refusals

The prefix `/transfers/api`, served on both listeners. Bodies of the JSON routes are at most 16 KiB
(16384 bytes, `lmgw_api_types::transfer::BODY_LIMIT_BYTES`), refused 413 `body_too_large` naming
it; the content route's body is the file and has no such bound (§3).

```
GET  /transfers/api/targets?app_id=X   → {targets:[{device_id,name,online,last_seen_at}], free_bytes, expiry_days}
POST /transfers/api/transfers          {to_device, app_id, kind, title, filename, size, sha256} → 201 Transfer
PUT  /transfers/api/transfers/{id}/content   Content-Range: bytes a-b/size, raw body → {received, state}
GET  /transfers/api/transfers?role=incoming|outgoing   → {transfers:[Transfer], stored_bytes, free_bytes}
GET  /transfers/api/transfers/{id}                      → Transfer
GET  /transfers/api/transfers/{id}/content   receiver only, ready; Range / If-Range → 200 | 206
POST /transfers/api/transfers/{id}/received  receiver → Transfer (delivered)
POST /transfers/api/transfers/{id}/decline   receiver → Transfer (declined)
POST /transfers/api/transfers/{id}/cancel    sender or owner → Transfer (canceled)
feed: transfer.updated {Transfer}

Transfer = {id, app_id, kind, title, filename, size, sha256, from{device_id,name}, to{device_id,name},
            state: uploading|ready|delivered|declined|canceled|expired|failed,
            received, created_at, expires_at, ended_at?, end_reason?}
```

`device_id` is the device's key id as the owner's `KeyRow` shows it; `name` its paired name without
`device:`. Times are RFC 3339 UTC to the second.

### 9.1 Targets

The caller's other devices that declared `app_id`, and only those that could receive now: enabled,
enrolled and confirmed, not past their key's expiry. Never the caller. `online` is whether the
device holds its Chat feed open now (`LinkKind::Feed`, device link §7); `last_seen_at` when one of
its connections last opened or closed. `free_bytes` is §3's available space; `expiry_days` the
setting an offer made now gets.

### 9.2 The offer

`to_device` must be such a target for `app_id`, else 422 `transfer_target_invalid` saying which
part failed (not a device of this lmgw, the caller itself, another or no app id, disabled, not
confirmed, expired). The offer reads both devices again in its transaction; a sender that was
disabled or deleted since the gate admitted it is answered as the gate answers its next request,
401 `device_disabled` or 401 `device_key_unknown` (I29). `kind` is the app's own word for what the file is (required; no control or
invisible characters); `title` free text the card and the receiver show (may be empty; invisible
characters dropped); `filename` the receiver's name for the file (required; no `/`, `\`, control or
invisible characters; not `.` or `..`); `size` at least 1 byte (a `Content-Range` names no empty
range) and at most 2⁶³ − 1; `sha256` 64 lowercase hex digits. A malformed field is 400
`bad_request` naming it. Then §3's space check, then 201 with the `Transfer` (`uploading`,
`received` 0).

### 9.3 Lists and reads

`GET /transfers/api/transfers` lists newest first, ended ones included for §7's retention: for a
device its outgoing ones (`role=outgoing`), the incoming ones it sees (`role=incoming`, from `ready`
on), or both without `role`; for the owner every transfer (a `role` is 400: the owner is no party).
`stored_bytes` is what the files of transfers in progress hold on disk now, `free_bytes` §3's
available space. A receiver lists the pending offers at its start with `role=incoming` and keeps
the feed for the rest.

### 9.4 Refusals

The flat `{code, message}` of the Chat API (`ApiError`), with one named extra field where listed,
typed as `lmgw_api_types::transfer::TransferRefusal`; the codes are
`lmgw_api_types::transfer::code`.

| Status | Code | When | Extra |
|---|---|---|---|
| 400 | `bad_request` | a malformed field, query or `Content-Range`, a `Content-Length` other than the range's, a short or long body that did not reach the file's last byte, a `role` from the owner | |
| 401 | `session_required` | a request that presents nothing, whatever **Require API key** says (§10) | |
| 401 | `device_disabled`, `device_key_unknown` | an offer whose sender was disabled or deleted while it was read (I29) | |
| 403 | `transfer_device_only` | a plain API key or an agent token on any route; the owner on a device's route | |
| 404 | `transfer_not_found` | no such transfer, or not one this caller may reach that way (§1) | |
| 409 | `transfer_offset` | a `PUT` whose `a` is not `received`, or one a newer `PUT` stopped | `received` |
| 409 | `transfer_state` | the transfer is not in a state for this (uploading, ready, ended); a `PUT` an end stopped (I23); a download whose transfer ended before its file opened | `state` |
| 413 | `body_too_large` | a JSON body over 16 KiB (16384 bytes), the same code and bound as the device listener's `/devices/*` routes | |
| 416 | `range_not_satisfiable` | a download's one byte range that names no byte of the file (§5) | |
| 422 | `transfer_target_invalid` | the offer's `to_device` is no target for its `app_id` | |
| 422 | `transfer_hash_mismatch` | the uploaded bytes' sha256 is not the offer's: it is `failed` | |
| 507 | `transfer_storage_full` | the offer is larger than the free space, or a write met a full disk | `free_bytes` |

The device listener's own refusals (`device_unconfirmed`, `device_disabled`, `key_expired`, …)
apply as on every route there. A request whose `Content-Length` is not a number is refused by hyper
before it is routed: a bare 400 with no body, none of the codes above.

## 10. The capability

A new capability, `Cap::Transfer` ("transfer"), held by an owner and by a paired device, nobody
else (I1). The device listener's face serves it (`server::serves_on`), as it serves `Inference`
and `Chat`. The gate checks it as it checks `Chat`: the key's row still the one presented, its
expiry, no request slot and no rate window (an upload of a few hundred MB is one request that runs
for minutes and spends nothing). A principal that does not hold it is refused by the gate with 403
`transfer_device_only` — a plain API key, an agent token — instead of the generic `forbidden`, and
an anonymous request with 401 `session_required`, whatever **Require API key** says.

## 11. Surfaces

### 11.1 The Devices card

- **Each device's app id**, as a chip beside its name (`app dev.example.storyplayer`).
- **A Transfers section** under the devices: every transfer lmgw keeps, newest first — from → to,
  the app id, the title (the file name under it), the size, the progress while it uploads, the
  state, when it expires (or ended, with the reason), and Cancel on one in progress, after a
  confirmation that says what it ends ("its bytes on lmgw are deleted; both devices hear it"). Its
  head says the bytes the transfers in progress hold and the free space left for new offers.
- It reads `GET /transfers/api/transfers` as the owner and reads it again on `/api/events`' `keys`
  frame with `what: "transfer"`, which lmgw sends on every state change and at most once a second
  while an upload writes (I13). The rest of the Keys page does not refetch on that `what`.

### 11.2 Settings

`devices.transfer_expiry_days` (§7) in Settings → Network & access → Devices, as "Transfer offers
last", in days, 1 to 3650.

### 11.3 `lmgw-client`

Sans-IO, wasm-clean (`requests::transfer`):
- `enrol` sends the app id (the builder's signature changed: Kai and Kai mobile pass `None` at
  their next pin raise). Its signature changed again on 2026-10-23: the device's self-description
  is one `DeviceSelf { name, client, app_id }` (device link design §6.3);
- builders for every route; the content routes return method, path and headers only
  (`Content-Range`, `Content-Length`, `Range`, `If-Range`), and the app streams the bytes itself;
  `upload_status` is the `bytes */size` form (§4);
- **`Method::Put` is new**: Kai (`lmgw_link::realtime`, its two request senders) and Kai mobile's
  core (`link::http`, `voice::signal`) match `requests::Method` exhaustively, so each needs a `Put`
  arm at its next pin raise, beside the `enrol` change above;
- `ByteRange` for `Range` (`ByteRange::parse` is `None` for a header the gateway ignores, §5),
  `content_range` / `parse_content_range` for both directions, `upload_status_range` /
  `parse_upload_status_range` for the status form, the ETag a transfer's bytes carry (`etag`), and
  the resume point (`Transfer::received`);
- readers for every answer, and `read_transfer_refusal` for every refusal with its extra field.

## 12. Tests

`tests/it/transfers/` (one module of the `it` binary), between real paired devices on the device
listener: the whole flow; targets by app id and never the caller; a third device never sees the
offer or its event; resume after a body that broke off; chunks; 409 `transfer_offset` with
`received`; 422 `transfer_hash_mismatch` → `failed`; ranged download, `If-Range`, 416; download
before `ready` → 409 `transfer_state`; 507 with `free_bytes` (free space injected); `received`,
`decline` and `cancel` delete the bytes; the expiry sweep; a deleted device's transfers end; a plain
API key → 403 `transfer_device_only`; a feed reopened after a disconnect replays
`transfer.updated`. `lmgw-client`'s builders and readers have unit tests of their own.

Added 2026-10-22 (the review's fix round): a full file completed by the next `PUT` whatever it
names, by the status form, by the upload whose body went on or broke off after the last byte, and
by the sweep (`complete`); a cancel, a disable and an expiry during a streaming `PUT` answered with
the state they left, an overdue upload, a body cut mid-block and resumed at once, a cancel during a
streaming `GET` (`stops`); ignored ranges and `HEAD` (`download`); nobody (401) and an agent token
(403), a sender that ended while its offer was read (`access`); the sweep's backstop for a row that
went without its end, a re-enrolment without an app id and a confirmed waiting key of another app
(`ends`); a new token dropping a waiting key's app id (`flow`); and no upload slot left behind
(`transfers::uploads`, `transfers::tests`).

Added in the second fix round (2026-10-22): an end lets go of a download nobody reads, its fd
closed (`stops`); `HEAD` ignores `Range` and `If-Range`, and no download's file stays open after its
body (`download`); an upload that wrote the last byte is passed over by the sweep and completes the
file when a newer one stops it (`complete`); an end whose caller goes still finishes
(`transfers::tests`); the download registry (`transfers::downloads`) and the sweep's `try_take`
(`transfers::uploads`).

## Implementation decisions (for review)

**I1. A capability of its own, not `Chat` or `Inference`.** `Chat` would refuse a plain key with
the gate's `forbidden`, not `transfer_device_only`; `Inference` would admit anonymous callers with
**Require API key** off, take a concurrency slot for the length of an upload and count requests
against the key's per-minute windows. `Transfer` says exactly who: the owner and devices.

**I2. `transfer_device_only` comes from the gate** for a principal without the capability, and from
the handlers for the owner on a device's route. One code for both, as the promised API names one.

**I3. A waiting key's app id waits with it** (`waiting_app_id`), moved to `app_id` by the Confirm
that makes the key the row's. The enrolment's `client` text has no such rule (it is display only);
the app id decides what a device receives.

**I4. A re-enrolment without an app id clears it.** The app id is what the enrolment declared; a
build that stops declaring one stops receiving.

**I5. The sender's own app id is not checked.** Any device may offer a file to a device of an app
id it names; the receiver's app decides what to do with it. A sender check would add a rule the
owner did not ask for.

**I6. A transfer's id is 32 random hex digits;** any other path id is 404 before a query.

**I7. A route that is not the caller's part answers 404**, as for a device that is no party: one
rule, and the message says what the route is for.

**I8. Field rules are content rules, not length caps.** The 16 KiB body bound is the only bound on
the JSON fields, stated and refused by name.

**I9. A newer upload stops an older one.** Each transfer has one upload slot; a new `PUT` raises
the slot's generation (the older writer stops reading its body at once and answers 409
`transfer_offset`) and waits for the slot before it reads `received`. Every end of a transfer takes
the slot too, so a cancel never races a write, and its file is deleted with nothing writing it.

**I10. The hash is computed once, at completion**, from the file (not incrementally while chunks
arrive): resumes across restarts would otherwise need the hasher's state on disk.

**I11. A `PUT` answers after `fsync`.** A few milliseconds per chunk for a `received` that survives
a crash.

**I12. No feed record per chunk.** The feed is stored and replayed; a few hundred records per
upload would crowd every device's catch-up. The sender knows what it sent; the owner's card hears
progress through `/api/events`.

**I13. The card's live updates ride the `keys` frame** (`what: "transfer"`) instead of a frame of
their own: one `/api/events` event the Keys page already listens to, without a new name in that
stream's documented list.

**I14. The expiry setting has no `0 = never`.** Every offer carries `expires_at`, as promised, so
the setting starts at 1 day; 3650 is the devices class's longest stated bound (the certificates').

**I15. Ended rows live as long as the feed's records**, for the reason in §7; a separate retention
setting would be a second number meaning the same.

**I16. Only Disable and Delete end a device's transfers.** A rotation re-pairs the same row, and the
device comes back as itself; its key's expiry date is a pause the owner can lift, and the offers
expire on their own. *Changed 2026-10-22:* a re-enrolment that changes the app id ends what was
offered as the old one (I28).

**I17. `end_reason` is a sentence**, for people; `state` is what a client acts on. *Changed
2026-10-22:* every reason is a plain sentence the Devices card shows as it is: an expiry says how
long the offer lasted, from the row's own times, instead of naming the setting's key and the
`expires_at` stamp; a failed upload says its bytes did not match the offer's sha256 (the 422 names
both hashes).

**I18. Lazy expiry.** A request that reads a transfer past its `expires_at` expires it first, so
the hourly tick is a sweep, not the only clock.

**I19. Names are stored on the transfer** (`from_name`, `to_name`) at the offer, so a transfer of a
deleted device still says who it was.

**I20. The receiver's `received` is not checked against its download.** lmgw trusts the device's
own hash check (it has the sha256); a receiver that sends `received` without downloading only loses
its own copy.

**I21. The receiver reaches a transfer by id before `ready`.** The offer reaches it at `ready` (its
list, its feed), but a receiver that knows the id (the sender's app told it) reads it and is
answered 409 `transfer_state` on its download: a refusal that says "not yet" instead of a 404 that
says "no such transfer".

**I22. The upload runs as a task of its own, writing at explicit offsets** (2026-10-22). The
handler checks the request and spawns the upload, which owns the slot and the file and is awaited:
a handler hyper drops when its client went (before or after the last byte) leaves no write queued
in the blocking pool behind a slot already let go, and the slot goes only once what was written is
synced and its `received` stored. Every write is at the range's first byte plus what was written
of it, so even a write that lands late writes the bytes that belong there and cannot shift a
retry's. Bytes that arrive while a write runs are gathered for the next one (a block of 1 MiB at
most, an I/O size, not a bound on anything); when no more are at hand they are written before the
wait, so `received` follows a slow body.

**I23. An end that stops an upload answers it with the state it left** (2026-10-22). A taker says
whether it is an upload or an end; an upload an end stopped lets go, waits — asking nobody to stop
— until the end let go too, and answers 409 `transfer_state` with the state then (`transfer_offset`
when it still uploads: the end did not take). A newer upload's stop stays 409 `transfer_offset`,
unless the stopped one had written the last byte (I25).

**I24. A slot lives while someone holds it or waits for it** (2026-10-22). Every holder and waiter
holds a share; the last share to go removes the slot from the map, under the map's lock. An end
with a taker queued behind it, a request that gave up while it waited and an upload that found its
transfer overdue leave nothing behind, with no forget step to miss.

**I25. A file that holds every byte is completed by whoever finds it so** (2026-10-22): the upload
that wrote the last byte, however its body ended after it (it went on past the range, it broke off,
its client went); any later `PUT`, whatever its range, without reading its body; the status form
`bytes */size`; the tick, at start and hourly. An upload stopped by another one leaves the
completion to that one, which finds the file full; one stopped by an end leaves it to the end. The
completion syncs the file before it hashes, so `ready` names bytes on the disk. RFC 9110 gives no
request form for "where does this stand"; `bytes */size` is its unsatisfied-range form, the one
resumable-upload protocols use for that. *Changed in the second fix round (2026-10-22):* an upload
a newer one stopped after it wrote the last byte completes the file itself and answers as the
completion does (it answered 409 `transfer_offset` with `received` equal to `size`, which told the
client to send from past the end); and the tick takes only a slot nobody holds or waits for
(`Uploads::try_take`), so it never stops an upload that runs: what it is for is the start's pass
and a full file nobody touches, and the next tick finds one it passed over.

**I26. A download ends with its transfer's bytes** (2026-10-22). Every end closes the transfer's
downloads and deletes its file. A download body reads the file a block at a time through a registry
(`transfers::downloads`): the registry holds each open file, the body only a weak handle, and a read
holds the file only while it runs. An end (any of them: cancel, decline, received, expiry, a
device's end, an app id's change, a hash that did not match) and the tick's stray-file step let go
of the transfer's files, so each fd closes once a read in flight returns and the body's next block
fails with an error. A download registers its file, then reads the state again: an end that
committed before the registration answers it 409 with the state it left, one after it closes it. A
file gone at the opening is that 409 too. *Changed in the second fix round (2026-10-22):* the body
held the file itself and checked its link count with each block, so a receiver that stopped reading
kept the fd, and the deleted file's disk space, for as long as its connection lived (nothing bounds
that), and on NFS, FUSE or a hard-linked snapshot the count stays above 0 and an ended transfer
streamed to its end.

**I27. A `Range` lmgw does not read is ignored** (2026-10-22), as RFC 9110 §14.2 allows: another
unit, several ranges, an invalid one or numbers past `u64` serve the whole file, 200. Several
ranges are not answered with the first, which a `multipart/byteranges` reader would misread; a
client resumes with one range. 416 is for one syntactically valid byte range that names no byte.
*Changed in the second fix round (2026-10-22):* `HEAD` ignores `Range` and `If-Range` (§14.2 reads
`Range` on `GET` only) and answers the whole file's head, 200, opening no file; it answered the
range's 206 (or 416) before.

**I28. A device's app id changing ends what was offered to it as the old one** (2026-10-22). The
app id decides what a device receives; an offer made to the old one would stay downloadable by an
install that no longer declares it. The enrolment that renews or replaces the row's key, and the
Confirm that makes a waiting key the row's, check the device's incoming transfers in progress;
the tick does too. Outgoing ones go on (I5).

**I29. A sender that ended while its offer was read gets the gate's refusal** (2026-10-22): 401
`device_disabled` or `device_key_unknown`, what its next request gets on the device listener,
instead of 422 `transfer_target_invalid`, which is about the target.

**I30. An end runs as a task of its own** (2026-10-22, second fix round). hyper drops a handler
whose client closed its side, and an end dropped that way could commit and never close its
downloads, delete its file or tell the feed. `transfers::end` spawns the slot's take, the commit,
the downloads' close, the unlink and the feed record as one task and awaits it, as the upload does
(I22). A device's Disable or Delete (§8) and a change of its app id (I28) spawn their whole loop of
ends too, so every transfer they end is ended, not the first few; the tick's backstop stays for a
crash.

**I31. A `received` an answer reports without writing is synced first** (2026-10-22, second fix
round). The status form's answer and the 409 `transfer_offset` for another first byte sync the file
before they read its length (`transfers::synced_len`), so the resume point they give survives a
crash as an upload's own answer does (I11): an earlier holder whose sync failed, or a stopped lmgw,
can leave bytes in the page cache only. A read's `received` (`GET …/{id}`, the list, the feed) is
the file's length as it is, with no sync: progress, which may run ahead of the disk by what a
running upload has not synced yet; a `PUT` from there is answered 409 with the real one when a
crash took that tail.

## Not yet

- Direct device-to-device transfer (both on one LAN, without lmgw's disk).
- Streaming the upload through to a receiver that is online while A uploads.
- OS notifications on Android while the app is closed (the feed reaches an app that runs).
- A self-admin MCP tool for transfers.
- App-defined metadata on an offer beyond `kind`, `title` and `filename`.
- A `state` filter on the list.
