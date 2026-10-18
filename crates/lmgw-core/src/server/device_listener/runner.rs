//! [`DeviceListener`]: what the gateway holds of the device listener while
//! it runs (device link design §4.1, §4.4, §9.2).
//!
//! It is **armed** while the gateway serves (`server::serve` arms it at its
//! start and disarms it at its stop), and while armed it serves whatever
//! the `devices` settings ask: `devices.listener` on starts it (making the
//! CA on its first start), a change of `devices.listen` or `devices.port`
//! moves it, off stops it, each at once ([`DeviceListener::reconcile`],
//! which every save of those settings calls).
//!
//! **The CA is opened before the listener's lock is taken**: a wrapped key
//! asks the Secret Service, whose unlock prompt waits for as long as nobody
//! answers it. So no save and no stop waits on the lock behind it, and the
//! gateway's stop races the opening alone ([`DeviceListener::claim`], then
//! [`DeviceListener::open_ca`] raced, then [`DeviceListener::start_claimed`]):
//! a stop during the prompt cancels the opening, and the start never runs.
//! Nothing that changes the listener's state is ever cut half-way.
//!
//! **A stop** closes its sockets at once (so a move rebinds the same port),
//! moves the device listener's generation (`Stops::stop_of`), which ends its
//! feeds, sessions and calls as at a gateway stop, then lets its requests
//! drain within [`STOP_WITHIN`](crate::server::STOP_WITHIN) and closes what
//! is still open after it. The plain listener's connections are never
//! touched. A move or an off drains in the background; the gateway's own
//! stop waits for it, within the bound it shares with the plain listener.
//!
//! **The calls' device socket group follows it** ([`DeviceListener::watch`],
//! read by `Calls::follow_devices`, which the first arm starts): UDP
//! sockets on the same addresses and port, closed and bound again at each
//! start, move and stop, its calls ending as at a stop.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_api_types::device_link as wire;

use super::bind::{bind, Plan};
use super::connection::Serving;
use super::tls_listener::TlsListener;
use crate::devices::tls::Ca;
use crate::server::{ServedAt, Via, STOP_WITHIN};
use crate::state::SharedState;

/// The device listener as the gateway holds it.
pub struct DeviceListener {
    inner: tokio::sync::Mutex<Inner>,
    /// What the status shows; written under `inner`, read without it.
    view: Mutex<View>,
    /// Where it serves now, for what follows it ([`Self::watch`]).
    bound: tokio::sync::watch::Sender<Option<Bound>>,
}

impl Default for DeviceListener {
    fn default() -> Self {
        Self {
            inner: Default::default(),
            view: Default::default(),
            bound: tokio::sync::watch::Sender::new(None),
        }
    }
}

/// Where the device listener serves now: what a socket that follows it
/// binds beside it (the calls' device socket group, device link design §8),
/// and the generation its requests carry, whose stop ends what they opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The addresses it is bound to (the loopback pair, `[::]`, or one).
    pub addrs: Vec<SocketAddr>,
    pub served_at: ServedAt,
}

impl std::fmt::Debug for DeviceListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("DeviceListener")
            .field("state", &view.state)
            .field("plan", &view.plan)
            .finish()
    }
}

#[derive(Default)]
struct Inner {
    /// Bumped by each arm, so a gateway stop disarms only its own start.
    epoch: u64,
    armed: Option<u64>,
    live: Option<Live>,
}

/// One start of the listener.
struct Live {
    plan: Plan,
    listener: TlsListener,
    serving: Arc<Serving>,
}

#[derive(Default)]
struct View {
    state: wire::DeviceListenerState,
    plan: Option<Plan>,
    addrs: Vec<SocketAddr>,
    error: Option<String>,
    serving: Option<Arc<Serving>>,
}

/// What a [`DeviceListener::reconcile`] did, as a save's answer says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The gateway is not serving: the settings apply at its next start.
    NotServing,
    /// It already serves what the settings ask, or stays off.
    Unchanged,
    /// It serves on `urls` now; `ended` connections of the listener it
    /// replaced ended (0 when it was off).
    Listening { urls: Vec<String>, ended: usize },
    /// It is off now; `ended` connections ended.
    Stopped { ended: usize },
    /// It does not serve, and why; `ended` connections of the listener it
    /// replaced ended.
    Failed { error: String, ended: usize },
}

impl Applied {
    /// The sentence a settings save adds to its answer.
    pub fn note(&self) -> Option<String> {
        let ended = |n: usize| match n {
            0 => String::new(),
            1 => "; 1 device connection ended".into(),
            n => format!("; {n} device connections ended"),
        };
        match self {
            Self::NotServing => {
                Some("the device listener follows these settings when the gateway serves".into())
            }
            Self::Unchanged => None,
            Self::Listening { urls, ended: n } => Some(format!(
                "the device listener listens on {}{}",
                urls.join(" and "),
                ended(*n)
            )),
            Self::Stopped { ended: n } => Some(format!("the device listener is off{}", ended(*n))),
            Self::Failed { error, ended: n } => Some(format!(
                "the device listener does not serve: {error}{}",
                ended(*n)
            )),
        }
    }
}

/// The CA as [`DeviceListener::open_ca`] found it, for
/// [`DeviceListener::start_claimed`].
pub struct OpenedCa(Option<Result<Arc<Ca>, String>>);

/// A stopped start, draining.
struct Draining {
    serving: Arc<Serving>,
    stopped: ServedAt,
    /// Its connections at the stop.
    ended: usize,
}

impl DeviceListener {
    fn set_view(&self, view: View) {
        let bound = view.serving.as_ref().map(|s| Bound {
            addrs: view.addrs.clone(),
            served_at: s.served_at,
        });
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = view;
        self.bound.send_if_modified(|b| {
            let moved = *b != bound;
            *b = bound;
            moved
        });
    }

    /// Where it serves, from now on: `None` while it does not. Every start,
    /// move and stop is a change, so what follows it (the calls' device
    /// socket group) rebinds as it does, and ends what it holds of an
    /// earlier start at that start's stop (`Stops::stopped_after`).
    pub fn watch(&self) -> tokio::sync::watch::Receiver<Option<Bound>> {
        self.bound.subscribe()
    }

    /// The gateway serves from now on: [`Self::claim`], then
    /// [`Self::start_claimed`]. Its epoch, which [`Self::disarm`] takes
    /// back.
    pub async fn arm(&self, state: &SharedState) -> u64 {
        let epoch = self.claim(state).await;
        let ca = self.open_ca(state).await;
        self.start_claimed(state, epoch, ca).await;
        epoch
    }

    /// The gateway serves from now on: an epoch of its own, which
    /// [`Self::disarm`] takes back whether or not the start got anywhere. A
    /// start left by an earlier arm whose disarm has not run yet (a restart
    /// racing the old server's stop) is stopped, as a gateway stop would.
    /// The first start's port is written here too (`persist_port`), and the
    /// calls' device socket group set to follow the listener from here on
    /// (`Calls::follow_devices`, once per gateway).
    pub async fn claim(&self, state: &SharedState) -> u64 {
        state.calls.follow_devices(self.watch());
        persist_port(state).await;
        let mut inner = self.inner.lock().await;
        inner.epoch += 1;
        let epoch = inner.epoch;
        inner.armed = Some(epoch);
        if let Some(live) = inner.live.take() {
            self.set_view(View::default());
            let draining = stop(state, live).await;
            tokio::spawn(drain(state.clone(), draining, STOP_WITHIN));
        }
        epoch
    }

    /// The CA the settings' listener needs, opened without the lock: it may
    /// wait on the wallet's prompt for as long as nobody answers it. Holds
    /// nothing and changes nothing but the trust's own state, so it is safe
    /// to drop at any await: `server::serve` races it against the stop.
    pub async fn open_ca(&self, state: &SharedState) -> OpenedCa {
        OpenedCa(open_ca(state).await)
    }

    /// Start what the settings ask, for `epoch`, with `ca`: the sockets,
    /// unless the epoch was disarmed meanwhile. Not to be raced: it changes
    /// the listener's state.
    pub async fn start_claimed(&self, state: &SharedState, epoch: u64, ca: OpenedCa) {
        let mut inner = self.inner.lock().await;
        if inner.armed != Some(epoch) {
            return;
        }
        self.apply(state, &mut inner, ca.0).await;
    }

    /// The gateway's stop: if `epoch` is still the armed one, stop the
    /// listener and wait, at most `within`, for its connections and its
    /// sessions and turns. Whether all of it ended within the bound (`true`
    /// too when nothing ran).
    pub async fn disarm(&self, state: &SharedState, epoch: u64, within: Duration) -> bool {
        let draining = {
            let mut inner = self.inner.lock().await;
            if inner.armed != Some(epoch) {
                return true;
            }
            inner.armed = None;
            self.set_view(View::default());
            match inner.live.take() {
                Some(live) => stop(state, live).await,
                None => return true,
            }
        };
        drain(state.clone(), draining, within).await
    }

    /// Serve what the settings ask now: start, move, stop, or nothing.
    /// Called by every save that changes `devices.listener`,
    /// `devices.listen` or `devices.port`.
    pub async fn reconcile(&self, state: &SharedState) -> Applied {
        persist_port(state).await;
        if self.inner.lock().await.armed.is_none() {
            return Applied::NotServing;
        }
        let ca = open_ca(state).await;
        let mut inner = self.inner.lock().await;
        if inner.armed.is_none() {
            return Applied::NotServing;
        }
        let applied = self.apply(state, &mut inner, ca).await;
        // A save of the same settings is the retry of a failed UDP bind:
        // nothing changed for the listener, so the follower is woken by hand
        // (its close is a no-op, the bind is tried again).
        if applied == Applied::Unchanged
            && matches!(
                state.calls.group(crate::realtime::calls::Group::Devices),
                crate::realtime::calls::GroupState::Failed(_)
            )
        {
            self.bound.send_modify(|_| {});
        }
        applied
    }

    /// Serve what the settings ask, with `ca` as [`open_ca`] found it.
    async fn apply(
        &self,
        state: &SharedState,
        inner: &mut Inner,
        ca: Option<Result<Arc<Ca>, String>>,
    ) -> Applied {
        let wanted = Plan::of(&state.snapshot().settings);
        let same = matches!((&wanted, &inner.live), (Ok(Some(w)), Some(l)) if *w == l.plan);
        if same || matches!((&wanted, &inner.live), (Ok(None), None)) {
            if matches!(wanted, Ok(None)) {
                self.set_view(View::default());
            }
            return Applied::Unchanged;
        }
        let ended = match inner.live.take() {
            Some(live) => {
                let draining = stop(state, live).await;
                let ended = draining.ended;
                tokio::spawn(drain(state.clone(), draining, STOP_WITHIN));
                ended
            }
            None => 0,
        };
        let plan = match wanted {
            Ok(Some(plan)) => plan,
            Ok(None) => {
                self.set_view(View::default());
                tracing::info!("lmgw devices: the device listener is off");
                return Applied::Stopped { ended };
            }
            Err(error) => {
                tracing::warn!("lmgw devices not listening: {error}");
                self.set_view(View {
                    state: wire::DeviceListenerState::Error,
                    error: Some(error.clone()),
                    ..View::default()
                });
                return Applied::Failed { error, ended };
            }
        };
        // Opened before the lock; a setting turned on since is served by the
        // CA the trust holds, if any.
        let ca = match ca {
            Some(ca) => ca,
            None => state.devices.trust.ca().ok_or_else(|| {
                "the device CA is not loaded yet; save the device listener's settings again"
                    .to_string()
            }),
        };
        match ca.and_then(|ca| start(state, plan, &ca)) {
            Ok(live) => {
                let urls = urls(&live.listener);
                self.set_view(View {
                    state: wire::DeviceListenerState::Listening,
                    plan: Some(plan),
                    addrs: live.listener.addrs().to_vec(),
                    error: None,
                    serving: Some(live.serving.clone()),
                });
                inner.live = Some(live);
                Applied::Listening { urls, ended }
            }
            Err(error) => {
                tracing::warn!("lmgw devices not listening: {error}");
                self.set_view(View {
                    state: wire::DeviceListenerState::Error,
                    plan: Some(plan),
                    error: Some(error.clone()),
                    ..View::default()
                });
                Applied::Failed { error, ended }
            }
        }
    }

    /// The addresses it is bound to now; empty while it does not serve.
    pub fn bound(&self) -> Vec<SocketAddr> {
        self.bound
            .borrow()
            .as_ref()
            .map(|b| b.addrs.clone())
            .unwrap_or_default()
    }

    /// The status block's listener (§9.2).
    pub fn status(&self, state: &crate::state::AppState) -> wire::DeviceListenerStatus {
        let view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        let (connections, requests, unauthenticated, evicted) =
            view.serving.as_ref().map_or((0, 0, 0, 0), |s| {
                (
                    s.connections() as u64,
                    state.stops.open_requests(s.served_at).len() as u64,
                    s.pre.open() as u64,
                    s.pre.evicted(),
                )
            });
        wire::DeviceListenerStatus {
            state: view.state,
            listen: view.plan.map(|p| p.listen.as_text()),
            port: view.plan.map(|p| p.port),
            urls: view.addrs.iter().map(|a| format!("https://{a}")).collect(),
            error: view.error.clone(),
            connections,
            requests,
            sessions: state.stops.running_on(Via::Devices) as u64,
            unauthenticated,
            evicted,
        }
    }
}

/// `https://` base URLs of what `listener` is bound to.
fn urls(listener: &TlsListener) -> Vec<String> {
    listener
        .addrs()
        .iter()
        .map(|a| format!("https://{a}"))
        .collect()
}

/// The first start's port (§4.1, Q1): while `devices.listener` is on and
/// `devices.port` is unset, the effective number (the HTTP port's plus one)
/// is written into the setting, so a later change of `bind_addr`'s port
/// never moves the device listener, or the links that name its port. A save
/// that turns the listener on writes it itself; this covers a setting
/// written by hand.
async fn persist_port(state: &SharedState) {
    let s = state.snapshot().settings.clone();
    if !s.devices.listener || s.devices.port.is_some() {
        return;
    }
    let guard = state.settings_write.lock().await;
    let mut s = state.snapshot().settings.clone();
    if !s.devices.listener || s.devices.port.is_some() {
        return;
    }
    let Ok(port) = s.devices.effective_port(&s.bind_addr) else {
        // Said by the start, which cannot derive it either.
        return;
    };
    s.devices.port = Some(port);
    if let Err(e) = crate::store::save_settings(&state.db, &s).await {
        tracing::warn!("devices.port could not be written ({e}); the listener uses {port} anyway");
        return;
    }
    let published = state.settings_saved(&s).await;
    drop(guard);
    state.reconcile_mcp().await;
    if let Some(e) = published.reload_failed {
        tracing::warn!("devices.port was written, and the reload failed ({e})");
    }
    tracing::info!(
        "devices.port is {port} from now on (the HTTP port's number plus one, written at the \
         device listener's first start)"
    );
}

/// The CA, opened (made on the first start) when the settings ask for the
/// listener; `None` when they do not. Called without the listener's lock:
/// a wrapped key may wait on the wallet's unlock prompt.
async fn open_ca(state: &SharedState) -> Option<Result<Arc<Ca>, String>> {
    if !matches!(Plan::of(&state.snapshot().settings), Ok(Some(_))) {
        return None;
    }
    Some(
        state
            .devices
            .trust
            .open(&state.data_dir, true)
            .await
            .map_err(|e| format!("the device CA cannot be used: {e}")),
    )
}

/// Start the listener for `plan` with `ca`: the sockets, the accept tasks;
/// the log line.
fn start(state: &SharedState, plan: Plan, ca: &Ca) -> Result<Live, String> {
    let sockets = bind(&plan)?;
    let served_at = state.stops.now_of(Via::Devices);
    let app = crate::server::stop::stamped(
        state,
        crate::server::build_device_router(state.clone()),
        served_at,
    );
    let serving = Serving::new(state.clone(), app, served_at);
    let listener = TlsListener::start(sockets, serving.clone());
    tracing::info!(
        "lmgw devices listening on {} (CA {}, key {})",
        urls(&listener).join(" and "),
        ca.facts().fp_short(),
        ca.key_mode().as_str()
    );
    Ok(Live {
        plan,
        listener,
        serving,
    })
}

/// Stop `live`: its sockets closed, its generation moved (its streams end
/// and its sessions close), its connections told to finish and close.
async fn stop(state: &SharedState, live: Live) -> Draining {
    let Live {
        listener, serving, ..
    } = live;
    let addrs = urls(&listener).join(" and ");
    listener.close().await;
    let stopped = state.stops.stop_of(Via::Devices);
    let ended = serving.connections();
    serving.drain();
    tracing::info!(
        "lmgw devices: stopped listening on {addrs}; its {ended} connection(s) finish the \
         request in progress and close, its feeds, sessions and calls end"
    );
    Draining {
        serving,
        stopped,
        ended,
    }
}

/// Wait, at most `within`, for a stopped start's connections to close and
/// its sessions and turns to end; close what is still open then, and say
/// it. Whether all of it ended within the bound.
async fn drain(state: SharedState, d: Draining, within: Duration) -> bool {
    let started = tokio::time::Instant::now();
    let drained = d.serving.drained(within).await;
    if !drained {
        let open = state.stops.open_requests(d.serving.served_at);
        let closed = d.serving.close_all();
        tracing::warn!(
            "the device listener's drain did not finish within {} s of its stop; {closed} \
             connection(s) closed with {} request(s) open: {}",
            within.as_secs_f64(),
            open.len(),
            if open.is_empty() {
                "(none counted)".to_string()
            } else {
                open.join(", ")
            }
        );
    }
    let left = within.saturating_sub(started.elapsed());
    let open = state.stops.ended(d.stopped, left).await;
    if open > 0 {
        tracing::warn!(
            "{open} realtime session(s) or turn(s) of the device listener had not ended {} s \
             after its stop",
            within.as_secs_f64()
        );
    }
    drained && open == 0
}
