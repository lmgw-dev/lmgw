//! A poppler run whose own limit was not enough, run once more under
//! [`POPPLER_WIDE_MEMORY_LIMIT_BYTES`] (design §14 "Poppler limit from
//! measured peaks", review round): the measured limits are what ordinary
//! files need, and the wide one is what every run had before them, so
//! nothing that read then is refused now.
//!
//! **When.** A run under its own limit that ran out of memory
//! ([`PdfError::OutOfMemory`]), or an image run (`pdftoppm`,
//! `pdftohtml -dataurls`) whose stderr says openjpeg ran out of memory
//! decoding a JPEG 2000 image ([`jpx`]): poppler then draws the image blank
//! and exits 0 (probed 2026-11-20 on poppler 26.08.0 and openjpeg 2.5.4: a
//! 14 000² image under the 2 GiB image limit, a 20 000² one under 4 GiB,
//! renders and picture runs alike). A JPEG 2000 image that is damaged, or
//! in a form openjpeg does not read, is drawn blank under every limit: it
//! is said ([`Blank::Damaged`]) and not run again. Never a timeout, and
//! never twice: a run that passes the wide limit too is refused by that
//! name ([`Retry::Widened`]), and an image it still cannot decode is drawn
//! blank and said ([`Note::Undecoded`]).
//!
//! **Never wait for the wide charge while holding a slot.** A step that
//! holds its own slot gives it back first (with what the run made, which is
//! made again) and takes one charged the wide limit as a step of its own
//! ([`own`]); one that waited holding the smaller slot could deadlock once
//! two such steps hold the memory. A step whose slot already covers the
//! wide run runs it again inside it ([`in_slot`]: a page's crops, whose
//! siblings hold their finished slots until they are taken in order).
//!
//! **Time.** The run again gets the time its caller gives it (the run's
//! timeout, or what is left of it), within what the file's clock has left
//! ([`again_within`]); with nothing left the first outcome stands, said so
//! ([`Retry::Deadline`]). A hostile file costs at most about twice the time
//! and memory of one run.
//!
//! **A blank image's first run is kept** until the run again has answered:
//! its output is usable (only the image is blank), so a run again that
//! ends without an answer (out of the wide limit, out of time) leaves it
//! standing ([`Blank::AgainFailed`]). Its bytes are added to the run
//! again's charge (`kept`, given to `again`), and the first slot is kept
//! charged them, holding no slot, while the wide one is waited for
//! ([`own`]).
//!
//! **Said.** Every run that was run again, and every image left blank, is
//! a [`Note`]: recorded for the conversion it is inside ([`notes`], the
//! file's `memory_limit_widened` and `image_not_decoded` warnings) and
//! handed back to the caller ([`Got::notes`], Chat's and knowledge bases'
//! notes); each also writes an info line to the log.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;

use super::super::convert::ocr::spool::{file_arg, Spool};
use super::super::convert::pool::{self, Slot};
use super::super::convert::run::{self, Input};
use super::limit::{bounds, reading, At, Limit, PopplerTool, POPPLER_PIPE_READ_PER_BYTE};
use super::{into_pdf, PdfError, Retry};

pub mod jpx;
pub mod notes;
pub use jpx::JPX_UNDECODED;
pub use notes::{Blank, Note, Why};

/// Where a run reads the file from.
pub enum Src<'a> {
    /// Its bytes, through a pipe.
    Bytes(Bytes),
    /// The file's spool (by position, or a pipe where it has no file).
    Spool(&'a Spool),
}

impl Src<'_> {
    fn input(&self) -> Input {
        match self {
            Self::Bytes(b) => Input::Bytes(b.clone()),
            Self::Spool(s) => s.input(),
        }
    }

    /// What a step's charge grows by for the file: its bytes times
    /// [`POPPLER_PIPE_READ_PER_BYTE`] where it is piped.
    pub fn piped(&self) -> u64 {
        match self {
            Self::Bytes(b) => (b.len() as u64).saturating_mul(POPPLER_PIPE_READ_PER_BYTE),
            Self::Spool(s) => pool::charge::piped(s),
        }
    }
}

/// One poppler run: the tool, its arguments around the file's (`args`,
/// the file, `tail`), where it reads the file, its working directory and
/// whether it writes there, its stdout's bound and the pages it concerns
/// (for its notes; `None`: the file).
pub struct Call<'a> {
    pub tool: PopplerTool,
    pub bin: &'a Path,
    pub args: Vec<String>,
    pub tail: Vec<String>,
    pub src: Src<'a>,
    pub dir: Option<&'a Path>,
    pub writes: bool,
    pub output: u64,
    pub span: Option<(u32, u32)>,
}

impl<'a> Call<'a> {
    /// `tool` at `bin` reading `src`, `args` before the file and `tail`
    /// after it, [`super::MAX_UNCOMPRESSED_BYTES`] of stdout, no directory.
    pub fn new(
        tool: PopplerTool,
        bin: &'a Path,
        args: Vec<String>,
        tail: &[&str],
        src: Src<'a>,
    ) -> Self {
        Self {
            tool,
            bin,
            args,
            tail: tail.iter().map(|s| s.to_string()).collect(),
            src,
            dir: None,
            writes: false,
            output: super::MAX_UNCOMPRESSED_BYTES,
            span: None,
        }
    }

    /// The charge of a step that is this run alone under `limit`: the
    /// limit, a piped file's share and its stdout.
    pub fn charge(&self, limit: Limit) -> u64 {
        limit
            .bytes()
            .saturating_add(self.src.piped())
            .saturating_add(self.output)
    }
}

/// A run's time and the bytes its stdout may carry; its memory is its
/// limit's ([`Limit`]).
#[derive(Debug, Clone, Copy)]
pub struct Within {
    pub timeout: Duration,
    pub output: u64,
}

/// One attempt: its stdout, and the JPEG 2000 images it drew blank.
#[derive(Debug)]
pub struct Ran {
    pub out: Vec<u8>,
    pub blank: jpx::Blank,
}

/// A run's outcome: its stdout and what it said ([`Note`]).
#[derive(Debug)]
pub struct Got {
    pub out: Vec<u8>,
    pub notes: Vec<Note>,
}

/// `call` once under `limit` on `slot`.
pub async fn once(
    slot: &Slot,
    call: &Call<'_>,
    limit: Limit,
    timeout: Duration,
) -> Result<Ran, PdfError> {
    let input = call.src.input();
    let mut args: Vec<&str> = call.args.iter().map(String::as_str).collect();
    args.push(file_arg(&input));
    args.extend(call.tail.iter().map(String::as_str));
    let rules = super::super::convert::confine::poppler(call.bin, call.dir.filter(|_| call.writes));
    let (seen, watcher) = jpx::Jpx::watcher();
    let tool = run::Tool {
        dir: call.dir,
        rules: rules.as_ref(),
        watch: call.tool.draws().then_some(&watcher),
        ..run::Tool::new(call.bin, &args)
    };
    let at = At {
        tool: call.tool,
        limit,
        piped: matches!(input, Input::Bytes(_)),
    };
    let bounds = reading(bounds(limit, timeout, call.output), &input);
    // Boxed: the run's own state is large, and the conversion's chains run
    // on a 2 MiB stack.
    let out = Box::pin(run::run_held(slot, tool, input, bounds))
        .await
        .map_err(|e| into_pdf(e, at))?;
    let blank = seen.lock().unwrap_or_else(|e| e.into_inner()).blank;
    Ok(Ran { out, blank })
}

/// `call` on a slot of its own charged its bounds ([`Call::charge`]), and
/// once more wide on a new one when it needs it (module doc).
pub async fn own(call: &Call<'_>, timeout: Duration) -> Result<Got, PdfError> {
    let limit = call.tool.limit();
    let slot = pool::global().acquire_charged(1, call.charge(limit)).await;
    let first = once(&slot, call, limit, timeout).await;
    settle(call.tool, call.span, first, timeout, |t, kept| async move {
        // The first run's output stays charged (no slot) while the wide
        // slot is waited for; the wide slot carries it from then on.
        let keep = slot.shrink(0, kept);
        let wide = pool::global()
            .acquire_charged(1, call.charge(Limit::Wide).saturating_add(kept))
            .await;
        drop(keep);
        once(&wide, call, Limit::Wide, t).await
    })
    .await
}

/// `call` on `slot`, which covers its wide run too, and once more wide
/// inside it when it needs it (module doc). A page's crops: the slot is
/// [`pool::charge::PAGE_CROPS`], the wide run's charge and one more output,
/// so the first run's kept output fits beside the wide run.
pub async fn in_slot(slot: &Slot, call: &Call<'_>, timeout: Duration) -> Result<Got, PdfError> {
    let first = once(slot, call, call.tool.limit(), timeout).await;
    settle(call.tool, call.span, first, timeout, |t, _kept| {
        once(slot, call, Limit::Wide, t)
    })
    .await
}

/// Why `first` (a run under `tool`'s own limit) is worth a run under the
/// wide one, if it is.
pub fn widens(first: &Result<Ran, PdfError>) -> Option<Why> {
    match first {
        Err(PdfError::OutOfMemory { limit, .. }) if *limit != Limit::Wide => Some(Why::Memory),
        Ok(r) if r.blank.memory > 0 => Some(Why::Jpx),
        _ => None,
    }
}

/// The time a run again gets: `timeout`, within what the file's clock has
/// left; `None` when nothing is left.
pub fn again_within(timeout: Duration) -> Option<Duration> {
    let t = match pool::clock::current() {
        Some(c) => timeout.min(c.limit().saturating_sub(c.used())),
        None => timeout,
    };
    (!t.is_zero()).then_some(t)
}

/// `first`, a run of `tool` under its own limit, settled: run again by
/// `again` (under the wide limit, given its time and the bytes of the first
/// run's output that are kept meanwhile) once when it needs it, and what it
/// said recorded ([`notes::record`]). A first run out of memory is let go
/// before `again` waits for anything; a first run with a blank image is
/// kept until `again` has answered (module doc).
pub async fn settle<F, Fut>(
    tool: PopplerTool,
    span: Option<(u32, u32)>,
    first: Result<Ran, PdfError>,
    timeout: Duration,
    again: F,
) -> Result<Got, PdfError>
where
    F: FnOnce(Duration, u64) -> Fut,
    Fut: std::future::Future<Output = Result<Ran, PdfError>>,
{
    let from = tool.limit();
    let Some(why) = widens(&first) else {
        return finish(tool, span, from, first, Blank::TooLarge, Vec::new());
    };
    let Some(t) = again_within(timeout) else {
        tracing::info!(
            "poppler: {} needed more than {} and was not run again: the file's deadline left \
             no time",
            tool.name(),
            from.name()
        );
        let first = first.map_err(|e| retried(e, Retry::Deadline));
        return finish(tool, span, from, first, Blank::Deadline, Vec::new());
    };
    tracing::info!(
        "poppler: {} needed more than {}{}; run again under {}",
        tool.name(),
        from.name(),
        match why {
            Why::Memory => "",
            Why::Jpx => " for a JPEG 2000 image",
        },
        Limit::Wide.name()
    );
    let widened = |why| Note::Widened {
        tool: tool.name(),
        from,
        why,
        span,
    };
    let kept = match first {
        Ok(r) => r,
        Err(e) => {
            drop(e);
            let second = again(t, 0)
                .await
                .map_err(|e| retried(e, Retry::Widened(from)));
            let notes = vec![widened(Why::Memory)];
            return finish(tool, span, Limit::Wide, second, Blank::TooLarge, notes);
        }
    };
    let size = kept.out.capacity() as u64;
    match again(t, size).await {
        Ok(second) => {
            // Widened when the run again decoded at least one more image.
            let notes = if second.blank.memory < kept.blank.memory {
                vec![widened(Why::Jpx)]
            } else {
                Vec::new()
            };
            drop(kept);
            finish(tool, span, Limit::Wide, Ok(second), Blank::TooLarge, notes)
        }
        Err(e) => {
            let how = match e {
                PdfError::OutOfMemory { .. } => "ran out of memory",
                PdfError::Timeout { .. } => "did not finish in time",
                _ => "failed",
            };
            tracing::info!(
                "poppler: {} run again under {} for a JPEG 2000 image {how}; the first run's \
                 output is kept",
                tool.name(),
                Limit::Wide.name()
            );
            finish(
                tool,
                span,
                from,
                Ok(kept),
                Blank::AgainFailed(how),
                Vec::new(),
            )
        }
    }
}

/// An out-of-memory error with `retry` said.
fn retried(e: PdfError, retry: Retry) -> PdfError {
    match e {
        PdfError::OutOfMemory {
            tool, limit, piped, ..
        } => PdfError::OutOfMemory {
            tool,
            limit,
            piped,
            retry,
        },
        e => e,
    }
}

/// `r`, a run under `limit`, with `notes` and its blank images' recorded:
/// those openjpeg had no memory for as `memory`, the damaged ones as such.
fn finish(
    tool: PopplerTool,
    span: Option<(u32, u32)>,
    limit: Limit,
    r: Result<Ran, PdfError>,
    memory: Blank,
    mut notes: Vec<Note>,
) -> Result<Got, PdfError> {
    let r = r?;
    let blank = |why| Note::Undecoded {
        tool: tool.name(),
        limit,
        why,
        span,
    };
    if r.blank.memory > 0 {
        notes.push(blank(memory));
    }
    if r.blank.damaged > 0 {
        notes.push(blank(Blank::Damaged));
    }
    if r.blank != jpx::Blank::default() {
        tracing::info!(
            "poppler: {} drew {} JPEG 2000 image(s) blank under {} ({} without memory, {} \
             damaged or unreadable)",
            tool.name(),
            r.blank.memory + r.blank.damaged,
            limit.name(),
            r.blank.memory,
            r.blank.damaged
        );
    }
    for n in &notes {
        notes::record(n.clone());
    }
    Ok(Got { out: r.out, notes })
}

#[cfg(test)]
mod tests;
