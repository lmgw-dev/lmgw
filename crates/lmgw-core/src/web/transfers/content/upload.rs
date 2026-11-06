//! The upload (`PUT …/content`, the sender's, while `uploading`; device
//! transfers design §4).
//!
//! - **Checked first, without the slot**: the caller, the transfer and the
//!   `Content-Range`, so a malformed request stops nobody's upload.
//! - **Then a task of its own** (I22) takes the slot (a newer upload stops
//!   an older one, I9), reads the resume point — the file's length — under
//!   it and writes the body at its offsets. The task owns the slot and the
//!   file: a handler hyper drops when its client went leaves nothing half
//!   done, and the slot goes only once what was written is synced and
//!   counted (I11).
//! - **A file that holds every byte is completed** by whoever holds the
//!   slot and finds it so (I25): the upload that wrote the last byte (also
//!   when its body then broke off or went on past its range), and any later
//!   upload of it, whatever its range — `Content-Range: bytes */<size>`
//!   with no body is the form that asks only that and the resume point.
//!   The status form takes the slot as an upload does, so it stops an
//!   upload still running.
//! - **An end that stops an upload** has it answer 409 `transfer_state`
//!   with the state the end left (I23); a newer upload 409
//!   `transfer_offset` with the bytes lmgw holds — unless the stopped one
//!   had written the file's last byte: it completes the file first, and
//!   answers as the completion does (I25).
//! - **Every `received` an answer reports is synced** (§3, I31): the upload
//!   syncs what it wrote, and the status answer and the 409 for another
//!   first byte sync the file before they read its length.

use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures::{FutureExt as _, StreamExt};
use lmgw_api_types::transfer::{
    self as wire, code, ContentReceived, TransferRefusal, TransferState,
};

use super::super::refuse::{self, answer, bad_request, internal};
use super::super::{reached, wrong_state, Part, TransferCaller};
use super::PROGRESS_EVERY;
use crate::state::SharedState;
use crate::store::transfers::{self as table, TransferRow};
use crate::transfers::{self, Completed, Held, Taker};
use crate::web::chat_extract::ChatPath;

/// How an upload writes: a block at a time, at its offset.
const WRITE_BLOCK: usize = 1 << 20;

/// What a `PUT` asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// The bytes `first..=last` of the file, in the body.
    Range(u64, u64),
    /// `bytes */<size>` with no body: where the upload stands, and its
    /// completion when lmgw holds every byte (I25).
    Status,
}

/// What a body came to as it was written.
enum Wrote {
    /// Every byte of the range, and nothing past it.
    All,
    /// The body ended after this many bytes of the range.
    Short(u64),
    /// The body went on past the range; the range was written.
    Long,
    /// The connection broke off.
    BrokeOff,
    /// A newer upload, or an end, asked for the slot.
    Preempted(Taker),
    /// The disk is full.
    Full,
    /// Another write failure.
    Failed(std::io::Error),
}

/// Whether `e` says the disk is full.
fn disk_full(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(libc::ENOSPC)
}

/// The message every refusal of a transfer that does not take bytes says.
const ONLY_UPLOADING: &str = "only an uploading transfer takes bytes";

/// `PUT /transfers/api/transfers/{id}/content` (§4).
pub(in super::super) async fn upload(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatPath(id): ChatPath<String>,
    req: Request,
) -> Response {
    if let Err(r) = p.device("upload files") {
        return r;
    }
    let row = match reached(&st, &p, &id, Part::Send).await {
        Ok(row) => row,
        Err(r) => return r,
    };
    if row.state != "uploading" {
        return wrong_state(&row, ONLY_UPLOADING);
    }
    let size = u64::try_from(row.size).unwrap_or(0);
    let ask = match ask_of(req.headers(), size) {
        Ok(a) => a,
        Err(why) => return bad_request(why),
    };
    let job = tokio::spawn(run(st, row, ask, req.into_body()));
    match job.await {
        Ok(resp) => resp,
        Err(e) => internal(format!("the upload stopped: {e}")),
    }
}

/// The upload under its slot, as a task of its own (I22).
async fn run(st: SharedState, before: TransferRow, ask: Ask, body: Body) -> Response {
    let id = before.id.clone();
    let mut held = st.transfers.uploads.take(&id, Taker::Upload).await;
    // Under the slot: what it is now, expired first when overdue.
    let row = match transfers::load_holding(&st, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return wrong_state(&before, ONLY_UPLOADING),
        Err(e) => return internal(e.to_string()),
    };
    if row.state != "uploading" {
        return wrong_state(&row, ONLY_UPLOADING);
    }
    let size = u64::try_from(row.size).unwrap_or(0);
    let have = transfers::len(&st.data_dir, &id).await;
    if have >= size {
        // Every byte is here already: whatever this one names, it
        // completes the upload (I25).
        return completed(&st, &row, held).await;
    }
    let (first, last) = match ask {
        Ask::Range(first, last) => (first, last),
        Ask::Status => {
            return match synced(&st, &id).await {
                Ok(have) => answer(
                    StatusCode::OK,
                    &ContentReceived {
                        received: have,
                        state: TransferState::Uploading,
                    },
                ),
                Err(r) => r,
            }
        }
    };
    if first != have {
        let have = match synced(&st, &id).await {
            Ok(have) => have,
            Err(r) => return r,
        };
        return refuse::offset(
            have,
            format!(
                "the range starts at byte {first}, and lmgw holds {have} bytes of this transfer: \
                 send from byte {have}"
            ),
        );
    }
    let file = match transfers::open_write(&st.data_dir, &id).await {
        Ok(f) => f,
        Err(e) if disk_full(&e) => return full(&st, have).await,
        Err(e) => return internal(format!("opening the transfer's file failed: {e}")),
    };
    let want = last - first + 1;
    let mut out = Writer {
        file,
        at: first,
        pending: BytesMut::new(),
    };
    let wrote = write_body(&st, &row, &mut held, &mut out, body, want).await;
    let synced = transfers::sync(&out.file).await;
    drop(out);
    let now_have = transfers::len(&st.data_dir, &id).await;
    if let Err(e) =
        table::set_received(&st.db, &id, i64::try_from(now_have).unwrap_or(i64::MAX)).await
    {
        tracing::warn!("transfer {id}: writing its progress failed ({e})");
    }
    st.telemetry.keys(row.from_key_id, "transfer");
    if let Err(e) = synced {
        if disk_full(&e) {
            return full(&st, now_have).await;
        }
        return internal(format!("syncing the transfer's file failed: {e}"));
    }
    match wrote {
        // The one that asked completes a full file, or ends the transfer.
        Wrote::Preempted(Taker::End) => {
            drop(held);
            return after_end(&st, &id, now_have).await;
        }
        // A newer upload asked; one that wrote the last byte completes the
        // file before it lets go (below), and the newer one finds it so.
        Wrote::Preempted(Taker::Upload) if now_have < size => {
            return refuse::offset(
                now_have,
                format!(
                    "a newer upload of this transfer stopped this one; lmgw holds {now_have} \
                     bytes of it"
                ),
            );
        }
        Wrote::Full => return full(&st, now_have).await,
        Wrote::Failed(e) => return internal(format!("writing the transfer's file failed: {e}")),
        Wrote::All | Wrote::Short(_) | Wrote::Long | Wrote::BrokeOff | Wrote::Preempted(_) => {}
    }
    if now_have >= size {
        // The last byte is here: completed, whatever the body did after it.
        return completed(&st, &row, held).await;
    }
    let kept = |what: String| {
        bad_request(format!(
            "{what}; lmgw keeps {now_have} bytes of this transfer: send from byte {now_have}"
        ))
    };
    match wrote {
        Wrote::Short(got) => kept(format!(
            "the body ended after {got} of the {want} bytes its Content-Range names"
        )),
        Wrote::Long => kept(format!(
            "the body goes on past the {want} bytes its Content-Range names, which were written"
        )),
        Wrote::BrokeOff => kept("the body broke off".to_string()),
        _ => answer(
            StatusCode::OK,
            &ContentReceived {
                received: now_have,
                state: TransferState::Uploading,
            },
        ),
    }
}

/// Transfer `id`'s resume point for an answer that wrote nothing (§3): the
/// file synced first — what an earlier holder wrote and could not sync, or
/// a stopped lmgw left unsynced — so the `received` it reports names bytes
/// a crash keeps; or the 500 that says why not.
async fn synced(st: &SharedState, id: &str) -> Result<u64, Response> {
    transfers::synced_len(&st.data_dir, id)
        .await
        .map_err(|e| internal(format!("syncing the transfer's file failed: {e}")))
}

/// An end stopped this upload (I23): once it let go of the slot, the state
/// it left; 409 `transfer_offset` when the transfer still uploads (the end
/// did not take, or it was the sweep's look at a stray file).
async fn after_end(st: &SharedState, id: &str, now_have: u64) -> Response {
    st.transfers.uploads.settled(id).await;
    match table::get(&st.db, id).await {
        Ok(Some(now)) if now.state != "uploading" => {
            wrong_state(&now, "an end of it stopped this upload")
        }
        Ok(_) => refuse::offset(
            now_have,
            format!(
                "another request for this transfer stopped this upload; lmgw holds {now_have} \
                 bytes of it"
            ),
        ),
        Err(e) => internal(e.to_string()),
    }
}

/// Complete `row`, whose file holds every byte (I25), and answer it; the
/// slot goes after.
async fn completed(st: &SharedState, row: &TransferRow, held: Held) -> Response {
    let size = u64::try_from(row.size).unwrap_or(0);
    let done = transfers::complete_holding(st, row).await;
    drop(held);
    match done {
        Ok(Completed::Ready) => answer(
            StatusCode::OK,
            &ContentReceived {
                received: size,
                state: TransferState::Ready,
            },
        ),
        Ok(Completed::Failed { sha }) => answer(
            StatusCode::UNPROCESSABLE_ENTITY,
            &TransferRefusal::new(
                code::TRANSFER_HASH_MISMATCH,
                format!(
                    "the uploaded bytes' sha256 is {sha}, and the offer named {}: the transfer \
                     failed and its bytes are deleted; offer it again",
                    row.sha256
                ),
            ),
        ),
        Ok(Completed::Moved(Some(now))) => wrong_state(&now, ONLY_UPLOADING),
        Ok(Completed::Moved(None)) => wrong_state(row, ONLY_UPLOADING),
        Err(why) => internal(why),
    }
}

/// 507 for a write that met a full disk, `kept` bytes held.
async fn full(st: &SharedState, kept: u64) -> Response {
    let free = transfers::available(st).await.unwrap_or(0);
    refuse::storage_full(
        free,
        format!(
            "the disk of lmgw's data directory is full; lmgw keeps {kept} bytes of this transfer \
             and has {free} bytes left for new offers"
        ),
    )
}

/// What the `PUT` asks, from its `Content-Range` (the file's `size` its
/// total) and its `Content-Length`; or why it is malformed.
fn ask_of(headers: &HeaderMap, size: u64) -> Result<Ask, String> {
    let Some(raw) = headers.get(header::CONTENT_RANGE) else {
        return Err(format!(
            "an upload names its range: Content-Range: bytes <first>-<last>/{size} (or bytes \
             */{size} with no body, to ask where it stands)"
        ));
    };
    let text = raw.to_str().unwrap_or_default();
    let length = match headers.get(header::CONTENT_LENGTH) {
        None => None,
        // hyper refuses a request whose Content-Length is not a number
        // before routing (a bare 400, §9.4); this is for one built in
        // process.
        Some(l) => match l.to_str().ok().and_then(|l| l.trim().parse::<u64>().ok()) {
            Some(n) => Some(n),
            None => return Err("Content-Length is not a number of bytes".into()),
        },
    };
    if let Some(total) = wire::parse_upload_status_range(text) {
        if total != size {
            return Err(format!(
                "Content-Range names a file of {total} bytes; this transfer's is {size}"
            ));
        }
        if length.is_some_and(|n| n != 0) {
            return Err(format!(
                "Content-Range: bytes */{size} asks where the upload stands and carries no body"
            ));
        }
        return Ok(Ask::Status);
    }
    let Some((first, last, total)) = wire::parse_content_range(text) else {
        return Err(format!(
            "Content-Range '{}' is not bytes <first>-<last>/{size} with first ≤ last < {size}, \
             nor bytes */{size}",
            text.escape_debug()
        ));
    };
    if total != size {
        return Err(format!(
            "Content-Range names a file of {total} bytes; this transfer's is {size}"
        ));
    }
    if length.is_some_and(|n| n != last - first + 1) {
        return Err(format!(
            "Content-Length is not the {} bytes Content-Range names",
            last - first + 1
        ));
    }
    Ok(Ask::Range(first, last))
}

/// The file an upload writes, from byte `at` on: what arrives while a write
/// runs is gathered, up to a block, for the next one.
struct Writer {
    file: std::sync::Arc<std::fs::File>,
    /// Where the next block goes: the range's first byte plus what was
    /// written of it (I22).
    at: u64,
    pending: BytesMut,
}

impl Writer {
    async fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= WRITE_BLOCK {
            self.flush().await?;
        }
        Ok(())
    }

    /// Write what was gathered. Every byte that arrived is written before
    /// an upload answers, however its body ended.
    async fn flush(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let block: Bytes = self.pending.split().freeze();
        let n = block.len() as u64;
        transfers::write_at(&self.file, self.at, block).await?;
        self.at += n;
        Ok(())
    }
}

/// Write `body` through `out`, `want` bytes at most, stopping when the slot
/// is asked for; the owner's card told at most every [`PROGRESS_EVERY`].
/// The slot is checked before every chunk, so a body that never pauses is
/// stopped too.
async fn write_body(
    st: &SharedState,
    row: &TransferRow,
    held: &mut Held,
    out: &mut Writer,
    body: Body,
    want: u64,
) -> Wrote {
    let mut stream = body.into_data_stream();
    let mut got = 0u64;
    let mut told = Instant::now();
    let wrote = loop {
        if let Some(who) = held.preempted().now_or_never() {
            break Wrote::Preempted(who);
        }
        let next = match stream.next().now_or_never() {
            Some(next) => next,
            None => {
                // Nothing more at hand: what was gathered goes to the file
                // before the wait, so the resume point is every byte that
                // arrived (§3) and a slow body shows its progress.
                if let Err(e) = out.flush().await {
                    break written_failed(e);
                }
                tokio::select! {
                    biased;
                    who = held.preempted() => break Wrote::Preempted(who),
                    next = stream.next() => next,
                }
            }
        };
        let chunk = match next {
            None if got == want => break Wrote::All,
            None => break Wrote::Short(got),
            Some(Err(_)) => break Wrote::BrokeOff,
            Some(Ok(chunk)) => chunk,
        };
        let room = usize::try_from(want - got).unwrap_or(usize::MAX);
        let take = room.min(chunk.len());
        if let Err(e) = out.push(&chunk[..take]).await {
            break written_failed(e);
        }
        got += take as u64;
        if take < chunk.len() {
            break Wrote::Long;
        }
        if told.elapsed() >= PROGRESS_EVERY {
            told = Instant::now();
            st.telemetry.keys(row.from_key_id, "transfer");
        }
    };
    match out.flush().await {
        Ok(()) => wrote,
        Err(e) => match wrote {
            // A write failure says more than how the body ended.
            Wrote::Full | Wrote::Failed(_) => wrote,
            _ => written_failed(e),
        },
    }
}

fn written_failed(e: std::io::Error) -> Wrote {
    if disk_full(&e) {
        Wrote::Full
    } else {
        Wrote::Failed(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upload_names_its_range_and_its_length_agrees() {
        let h = |range: &str, len: Option<&str>| {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_RANGE, range.parse().unwrap());
            if let Some(l) = len {
                h.insert(header::CONTENT_LENGTH, l.parse().unwrap());
            }
            h
        };
        assert_eq!(
            ask_of(&h("bytes 0-9/10", Some("10")), 10),
            Ok(Ask::Range(0, 9))
        );
        assert_eq!(ask_of(&h("bytes 4-5/10", None), 10), Ok(Ask::Range(4, 5)));
        assert_eq!(ask_of(&h("bytes */10", Some("0")), 10), Ok(Ask::Status));
        assert_eq!(ask_of(&h("bytes */10", None), 10), Ok(Ask::Status));
        for (range, len) in [
            ("bytes 0-9/11", None),
            ("bytes 0-9/10", Some("9")),
            ("bytes */11", None),
            ("bytes */10", Some("3")),
            ("bytes 0-9/10", Some("ten")),
        ] {
            assert!(ask_of(&h(range, len), 10).is_err(), "{range} {len:?}");
        }
        assert!(ask_of(&HeaderMap::new(), 10).is_err());
    }
}
