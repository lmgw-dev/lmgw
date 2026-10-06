//! Speech while a response's server-side calls run (realtime-server-tools
//! design §2.5; realtime design §4.3, §6.4, §6.5).
//!
//! **"Producing"** — the answer still counts as playing, so the playing
//! window is open-ended and a turn is a barge-in — used to last until the
//! phase reached Playing. Server-side calls keep a response Closing for as
//! long as they run: every utterance during a tool run then faced the
//! barge-in gate, half duplex did not listen at all, and an answer that had
//! played out was never `PlayedOut`. So it follows what the response still
//! says instead ([`Active::producing`]):
//! - a spoken response produces until its speaker handed the last clause
//!   to the writer ([`Msg::SpeakerDone`]); from then on its window ends with
//!   the playback, as in Playing;
//! - a text response produces until its end of generation, as before;
//! - a bound session's response is the thread's turn, which sends no
//!   speaker-done: it produces until Playing, as before.
//!
//! **Only its calls are left** ([`Core::only_tools_run`]) when generation is
//! over, nothing more will be said, a server-side call is still open, and
//! nothing of the response plays at the moment in question: it never spoke,
//! or its audio had played out — by the modelled end, with none of it
//! waiting in the writer, as `PlayedOut` judges. A turn that starts then is
//! an ordinary turn (`Interruption::ToolsRun`): committed, and cancelling
//! nothing; the response finishes with its calls' results, and what the
//! turn is owed waits for it — a client's follow-up `response.create` sent
//! while the user still speaks is held until the turn ends, and answers
//! both (`pending`). To the input side it is no barge-in either:
//! it cuts nothing (`Listen::cuts`), and nothing the user was listening to
//! is in progress (`Listen::heard`: §6.5's plain silence window). A turn
//! while the spoken preamble still plays is a barge-in as before, and its
//! cancel abandons the calls (`output::closing`). A response with no
//! server-side call open is never judged by this.
//!
//! [`Msg::SpeakerDone`]: super::super::responder::Msg::SpeakerDone

use tokio::time::Instant;

use super::{Active, Core, Phase};

impl Active {
    /// Whether the response still says something its listener gets (module
    /// doc).
    pub(super) fn producing(&self) -> bool {
        match self.phase {
            Phase::Playing => false,
            _ if self.output.speaks() => !self.spoken,
            Phase::Closing => false,
            Phase::AwaitingTranscripts | Phase::Generating => true,
        }
    }
}

impl Core {
    /// Whether only `a`'s server-side calls are left at `at` (module doc).
    pub(super) fn only_tools_run(&self, a: &Active, at: Instant) -> bool {
        if a.phase != Phase::Closing || a.producing() || !a.output.mcp_open() {
            return false;
        }
        if !a.output.spoke() {
            return true;
        }
        self.out
            .playback()
            .filter(|p| p.gen == a.output.gen && !p.waiting)
            .and_then(|p| p.end())
            .is_some_and(|end| at >= end)
    }
}
