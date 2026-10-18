//! The live calls (calls design §3.4): what is known of each, by id, in
//! memory only — a call is live state, nothing persists.
//!
//! The driver writes an entry when it takes a call, updates it as the call
//! connects, opens and reports, and removes it when the call ends; the rest
//! of lmgw reads it (the hangup's check of who may end a call, the status
//! block, a session's liveness evidence) and asks a call's owner to end it
//! through it ([`Ending`]): a hangup, or a stop of the call's listener,
//! whose close waits here until the group's calls are gone
//! ([`Registry::wait_gone`]).
//!
//! **A call that let go** ([`Registry::let_go`]; calls design §8.2) has
//! given its slot back and is only delivering its last events before its
//! `Rtc` closes: its [`Released`] resolves, and it is
//! [`CallState::Closing`] until it is removed. Nothing hides it while its
//! `Rtc` lives: [`Registry::get`] and [`Registry::list`] show it, a hangup
//! of it finds it closing ([`Hangup::Closing`]) and closes it at once, and
//! a group's close counts it ([`Registry::wait_gone`]), so the sockets stay
//! open for those last events until the stop's bound. Only the session's
//! liveness no longer reads it ([`Registry::evidence`]): no session owns it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::handle::{Ending, Released};
use super::sockets::SockId;
use super::Group;
use crate::principal::Principal;

/// Where a call stands: its transport's states, which the driver writes,
/// then its session's, which the open path writes (`calls::open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// Answered: waiting for ICE and DTLS.
    Answered,
    /// ICE and DTLS-SRTP are up; `oai-events` has not opened.
    Connected,
    /// `oai-events` is open; the bind runs (§2.8).
    Open,
    /// `takeover=ask` found its thread held: it waits for the client's
    /// `lmgw.session.take_over` or the holder's end ([`CallInfo::held_by`]).
    Pending,
    /// Its session runs ([`CallInfo::session`]), bound to
    /// [`CallInfo::thread`] when it is.
    Running,
    /// It let go (module doc): its slot is back and its owner done, and its
    /// `Rtc` stays only until the client acknowledged its last events, at
    /// most `realtime.ping_interval_s` (none at 0). A hangup closes it now.
    Closing,
}

/// The link's latest figures, from str0m's statistics (a 1 s sample): the
/// client's latest receiver report on lmgw's stream, which sets its own
/// cadence, and lmgw's own count of the client's packets. The round trip is
/// the session's ([`CallPlayout::round_trip`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CallStats {
    /// When the client's latest receiver report arrived (the statistics
    /// event that brought it): the age of `egress_loss`, `client_jitter`
    /// and the round trip those reports feed.
    pub report_at: Option<Instant>,
    /// The share of lmgw's packets the client lost, by that report: what
    /// the encoder's FEC follows (calls design §5.2).
    pub egress_loss: Option<f32>,
    /// The client's interarrival jitter on lmgw's stream, by that report,
    /// in RTP ticks (48 kHz).
    pub client_jitter: Option<u32>,
    /// The share of the client's packets lmgw lost over the last statistics
    /// interval; `None` when none arrived in it, never the one before.
    pub ingress_loss: Option<f32>,
}

/// A call's playout allowance as its frame clock last said it (calls design
/// §5.3), for the status block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CallPlayout {
    pub allowance: Duration,
    pub source: lmgw_api_types::realtime::PlayoutSource,
    /// The session's round trip in whole milliseconds (`RoundTrip`: the
    /// least of the last five the client's receiver reports measured), as
    /// the allowance and the barge-in margin take it.
    pub round_trip: Option<Duration>,
    /// The latest playout report's `jitter_buffer_ms`.
    pub jitter_buffer_ms: Option<f64>,
    /// When the latest playout report arrived, whatever it carried.
    pub last_report: Option<Instant>,
}

/// One live call.
#[derive(Debug, Clone)]
pub struct CallInfo {
    pub id: Arc<str>,
    /// Who made it: a hangup is the creator's or the owner's (§2.6).
    pub creator: Principal,
    pub group: Group,
    /// Its one host candidate (§3.2).
    pub candidate: SocketAddr,
    pub state: CallState,
    pub created: Instant,
    pub connected: Option<Instant>,
    pub opened: Option<Instant>,
    /// Where the client's last authenticated traffic came from.
    pub remote: Option<SocketAddr>,
    /// When the client's last authenticated traffic arrived — a binding
    /// request whose MESSAGE-INTEGRITY the driver checked with the call's
    /// password, or a datagram str0m made media, channel data, a channel's
    /// open or close, a sender report or the connection of (all past
    /// SRTP's or DTLS's checks): the liveness evidence of §8.1.
    pub last_heard: Option<Instant>,
    /// Since when the session's output on `oai-events` has waited for the
    /// client with no acknowledgement: set as data goes in flight with none
    /// before it, moved to each SCTP acknowledgement that takes some off,
    /// `None` while nothing is in flight (str0m counts written data until
    /// the client acknowledges it). The evidence that the client reads
    /// (§8.1): one whole `realtime.ping_interval_s` of it is the stalled
    /// verdict.
    pub unacked_since: Option<Instant>,
    pub stats: CallStats,
    /// Its playout allowance, once its audio out runs.
    pub playout: Option<CallPlayout>,
    /// Who holds its thread while it is [`CallState::Pending`], as
    /// `voice.bound.by` names a client.
    pub held_by: Option<String>,
    /// Its session's id, once the session runs.
    pub session: Option<String>,
    /// The Chat thread its session is bound to.
    pub thread: Option<i64>,
}

/// What a hangup found ([`Registry::ask`]).
pub enum Hangup {
    /// A live call: its owner was asked, and this resolves once it let go.
    Asked(Released),
    /// A call that let go and is still closing ([`CallState::Closing`]):
    /// nobody owns it to ask.
    Closing,
}

impl Hangup {
    /// Wait until the call has let go: at once for one that is closing.
    pub async fn wait(self) {
        if let Self::Asked(released) = self {
            released.wait().await;
        }
    }
}

/// What a session's liveness check reads of its call (§8.1): the two facts
/// [`CallInfo::last_heard`] and [`CallInfo::unacked_since`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Evidence {
    pub last_heard: Option<Instant>,
    pub unacked_since: Option<Instant>,
}

struct Entry {
    info: CallInfo,
    /// The socket it lives on: what a group's close finds it by.
    sock: SockId,
    /// Where its owner hears [`Ending`]; dropped once the call let go
    /// (module doc), which resolves every [`Released`].
    ending: Option<watch::Sender<Option<Ending>>>,
}

impl Entry {
    /// It has not let go (module doc).
    fn live(&self) -> bool {
        self.ending.is_some()
    }
}

#[derive(Default)]
struct Shared {
    calls: Mutex<HashMap<Arc<str>, Entry>>,
    /// Woken at every removal ([`Registry::wait_gone`]).
    removed: Notify,
}

/// The registry, shared by the driver and its readers.
#[derive(Clone, Default)]
pub(super) struct Registry(Arc<Shared>);

/// Set `why` unless the owner was asked already: the first reason stands.
fn ask_once(ending: &watch::Sender<Option<Ending>>, why: &Ending) {
    ending.send_if_modified(|v| {
        let first = v.is_none();
        if first {
            *v = Some(why.clone());
        }
        first
    });
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, Entry>> {
        self.0.calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enter a call on `sock`; the receiver its owner hears [`Ending`] on.
    pub fn insert(&self, info: CallInfo, sock: SockId) -> watch::Receiver<Option<Ending>> {
        let (ending, heard) = watch::channel(None);
        self.lock().insert(
            info.id.clone(),
            Entry {
                info,
                sock,
                ending: Some(ending),
            },
        );
        heard
    }

    /// Call `id` let go (module doc): its [`Released`] resolves, and it is
    /// [`CallState::Closing`] until [`Self::remove`].
    pub fn let_go(&self, id: &str) {
        if let Some(e) = self.lock().get_mut(id) {
            e.ending = None;
            e.info.state = CallState::Closing;
        }
    }

    pub fn remove(&self, id: &str) {
        if self.lock().remove(id).is_some() {
            self.0.removed.notify_waiters();
        }
    }

    /// Forget every call: the driver that held them died with them.
    pub fn clear(&self) {
        self.lock().clear();
        self.0.removed.notify_waiters();
    }

    pub fn update(&self, id: &str, f: impl FnOnce(&mut CallInfo)) {
        if let Some(e) = self.lock().get_mut(id) {
            f(&mut e.info);
        }
    }

    /// What is known of call `id`, closing included (module doc).
    pub fn get(&self, id: &str) -> Option<CallInfo> {
        self.lock().get(id).map(|e| e.info.clone())
    }

    /// Whether call `id` is here and has not let go.
    pub fn is_live(&self, id: &str) -> bool {
        self.lock().get(id).is_some_and(Entry::live)
    }

    /// The liveness evidence of call `id`, while it is live: a call that let
    /// go has no session to read it.
    pub fn evidence(&self, id: &str) -> Option<Evidence> {
        self.lock().get(id).filter(|e| e.live()).map(|e| Evidence {
            last_heard: e.info.last_heard,
            unacked_since: e.info.unacked_since,
        })
    }

    /// Every call, closing ones included, oldest first.
    pub fn list(&self) -> Vec<CallInfo> {
        let mut v: Vec<CallInfo> = self.lock().values().map(|e| e.info.clone()).collect();
        v.sort_by_key(|c| c.created);
        v
    }

    /// Ask the owner of call `id` to end it, unless it was asked already:
    /// what resolves once it has let go; [`Hangup::Closing`] for one that
    /// let go already, and `None` for a call not here.
    pub fn ask(&self, id: &str, why: Ending) -> Option<Hangup> {
        let calls = self.lock();
        let Some(ending) = calls.get(id)?.ending.as_ref() else {
            return Some(Hangup::Closing);
        };
        ask_once(ending, &why);
        Some(Hangup::Asked(Released(ending.subscribe())))
    }

    /// Ask the owners of the calls on `socks` to end them (a stop); how
    /// many there are, the calls that let go and still deliver their last
    /// events counted too (module doc).
    pub fn ask_on(&self, socks: &[SockId], why: Ending) -> usize {
        let calls = self.lock();
        let mut n = 0;
        for e in calls.values().filter(|e| socks.contains(&e.sock)) {
            if let Some(ending) = &e.ending {
                ask_once(ending, &why);
            }
            n += 1;
        }
        n
    }

    fn count_on(&self, socks: &[SockId]) -> usize {
        self.lock()
            .values()
            .filter(|e| socks.contains(&e.sock))
            .count()
    }

    /// Wait until no call lives on `socks`, at most `within`; how many are
    /// left when it stops waiting. A call that let go counts until it is
    /// removed (module doc).
    pub async fn wait_gone(&self, socks: &[SockId], within: Duration) -> usize {
        let until = Instant::now() + within;
        loop {
            let removed = self.0.removed.notified();
            tokio::pin!(removed);
            // Registered before the count: a removal in between wakes it.
            removed.as_mut().enable();
            let left = self.count_on(socks);
            if left == 0 {
                return 0;
            }
            if tokio::time::timeout_at(until, removed).await.is_err() {
                return self.count_on(socks);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str) -> CallInfo {
        CallInfo {
            id: id.into(),
            creator: Principal::Anonymous,
            group: Group::Plain,
            candidate: "127.0.0.1:9".parse().unwrap(),
            state: CallState::Running,
            created: Instant::now(),
            connected: None,
            opened: None,
            remote: None,
            last_heard: None,
            unacked_since: None,
            stats: CallStats::default(),
            playout: None,
            held_by: None,
            session: None,
            thread: None,
        }
    }

    /// A call that let go resolves its hangups and shows as closing until
    /// it is removed: listed, found closing by a hangup, waited for by a
    /// group's close, read by no liveness check (module doc).
    #[tokio::test(start_paused = true)]
    async fn a_call_that_let_go_is_closing_until_removed() {
        let reg = Registry::default();
        let sock = SockId(1);
        let mut heard = reg.insert(info("rtc_a"), sock);
        let Some(Hangup::Asked(released)) = reg.ask("rtc_a", Ending::HungUp { by: None }) else {
            panic!("a live call's owner is asked");
        };
        assert!(reg.evidence("rtc_a").is_some());
        assert!(reg.is_live("rtc_a"));
        reg.let_go("rtc_a");
        tokio::time::timeout(Duration::from_secs(1), released.wait())
            .await
            .expect("the hangup resolves at the let-go");
        assert_eq!(reg.get("rtc_a").unwrap().state, CallState::Closing);
        assert!(!reg.is_live("rtc_a"));
        assert!(reg.evidence("rtc_a").is_none());
        let listed: Vec<_> = reg.list().into_iter().map(|c| (c.id, c.state)).collect();
        assert_eq!(listed, [("rtc_a".into(), CallState::Closing)]);
        assert!(matches!(
            reg.ask("rtc_a", Ending::ShuttingDown),
            Some(Hangup::Closing)
        ));
        // The owner had heard the hangup; nothing new reaches it.
        assert_eq!(
            heard.borrow_and_update().clone(),
            Some(Ending::HungUp { by: None })
        );
        // A group's close counts it, and waits for its removal.
        assert_eq!(reg.ask_on(&[sock], Ending::ShuttingDown), 1);
        assert_eq!(reg.wait_gone(&[sock], Duration::from_secs(5)).await, 1);
        let waiting = reg.clone();
        let gone = tokio::spawn(async move {
            waiting
                .wait_gone(&[sock], Duration::from_secs(24 * 3600))
                .await
        });
        tokio::task::yield_now().await;
        reg.remove("rtc_a");
        assert_eq!(gone.await.unwrap(), 0);
        assert!(reg.get("rtc_a").is_none());
        assert!(reg.ask("rtc_a", Ending::ShuttingDown).is_none());
    }
}
