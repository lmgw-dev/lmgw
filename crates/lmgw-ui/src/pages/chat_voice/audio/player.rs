//! The page's one player (chat-voice §11.1).
//!
//! One `AudioContext({sampleRate: 24000})` per page — one PipeWire stream for
//! the shell to route — created at the first voice press ([`player`] creates
//! the context synchronously, inside the press's gesture) and kept for the
//! page's life. It is suspended after [`IDLE_SUSPEND_MS`] with nothing to
//! play and nobody holding it awake ([`Player::hold_awake`]: a realtime
//! session, a streaming read-aloud), and resumed on the next push.
//!
//! **One playback per page.** Audio is pushed per *item* (one read-aloud,
//! one realtime response, the test tone), and [`Player::begin`] is the only
//! way to get one: it flushes whatever plays first, and tells that item's
//! owner when it asked to be told ([`Player::begin_owned`]): a read-aloud
//! then stops fetching, whoever took the player (WP7 review m7). [`Player::push`] queues
//! PCM16-LE bytes, [`Player::end`] says the item is complete and resolves
//! when it has played out, and [`Player::flush`] drops what is queued and
//! answers how much of each item was played and heard — the
//! `truncate.audio_end_ms` of a barge-in. The bookkeeping, and its rules for
//! a barge-in's races, is [`ledger`].
//!
//! **The first sample plays where it should.** [`player`]'s future resolves
//! once the context runs (a browser that did not let it start is an error
//! that says so) and the first route has been applied ([`route`]); in a
//! browser the context is also made on the stored output. A route that
//! failed does not hold the audio back: it plays where the system puts it,
//! and [`status`]'s `route` says why, on the composer's devices button too.
//! Each resume applies the route again, one application at a time.

mod ledger;
mod route;
mod starve;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use futures::channel::oneshot;
use futures::future::{FutureExt, LocalBoxFuture, Shared};
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

pub(crate) use ledger::ItemCount;
use ledger::{Ledger, Push};
pub(crate) use starve::Starved;

use super::pcm::RATE;
use super::shell;

/// Nothing to play for this long, with nobody holding the player awake,
/// suspends the context (§11.1).
pub(crate) const IDLE_SUSPEND_MS: u64 = 10_000;
/// How long a flush waits for the worklet's count before it answers with
/// the last progress it reported.
const FLUSH_WAIT_MS: u64 = 1_000;
/// How long a new context may take to run. A browser that holds it (no
/// click or key press reached the page yet) fails the press visibly.
const START_WAIT_MS: u64 = 5_000;
/// How long the first route may take before the audio goes ahead anyway
/// (the shell bounds each of its two commands at 2 s), saying so.
const ROUTE_WAIT_MS: u64 = 6_000;
/// A resume that has not settled after this is asked for again.
const RESUME_RETRY_MS: f64 = 2_000.0;

const WORKLET_URL: &str = "/voice/player-worklet.js";

/// Where lmgw's playback goes, as last applied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Route {
    /// No playback context yet.
    #[default]
    None,
    Pending,
    /// Applied: where it plays, in words.
    Ok(String),
    /// Applying failed: the reason, shown as it is.
    Failed(String),
}

/// What the page shows about the player.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Status {
    /// The context exists and is running (not suspended).
    pub running: bool,
    pub route: Route,
    /// Creating or starting the context failed.
    pub error: Option<String>,
}

static STATUS: OnceLock<ArcRwSignal<Status>> = OnceLock::new();

/// The player's status, readable from any component (it outlives them).
pub(crate) fn status() -> ArcRwSignal<Status> {
    STATUS
        .get_or_init(|| ArcRwSignal::new(Status::default()))
        .clone()
}

/// Change the status, notifying only when it changed.
fn set_status(f: impl FnOnce(&mut Status)) {
    let st = status();
    let mut s = st.get_untracked();
    let before = s.clone();
    f(&mut s);
    if s != before {
        st.set(s);
    }
}

pub(crate) type Ready = Shared<LocalBoxFuture<'static, Result<Rc<Player>, String>>>;

thread_local! {
    static PLAYER: RefCell<Option<Ready>> = const { RefCell::new(None) };
}

/// The page's player, created on first use. The context is made before this
/// returns its future, so call it inside the press's event handler: a
/// context made there is allowed to start. The future resolves once the
/// context runs and its first route was applied. A creation that failed is
/// tried again on the next call.
pub(crate) fn player() -> Ready {
    let current = PLAYER.with(|slot| slot.borrow().clone());
    if let Some(ready) = current {
        match ready.peek() {
            Some(Err(_)) => {}
            Some(Ok(p)) => {
                // In the press's gesture: a resume asked here may start it.
                p.wake(true);
                return ready;
            }
            None => return ready,
        }
    }
    let ready = match new_context() {
        Ok(ctx) => async move { build(ctx).await }.boxed_local().shared(),
        Err(e) => {
            set_status(|s| s.error = Some(e.clone()));
            async move { Err(e) }.boxed_local().shared()
        }
    };
    PLAYER.with(|slot| *slot.borrow_mut() = Some(ready.clone()));
    ready
}

/// The player if it exists already (nothing is created).
pub(crate) fn existing() -> Option<Rc<Player>> {
    PLAYER.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|r| r.peek().and_then(|r| r.as_ref().ok().cloned()))
    })
}

fn new_context() -> Result<web_sys::AudioContext, String> {
    let made = |with_sink: bool| {
        web_sys::AudioContext::new_with_context_options(&route::context_options(
            RATE as f32,
            with_sink,
        ))
    };
    let with_sink = route::stored_sink_id().is_some();
    // A stored output the browser refuses (gone since): made on the default;
    // the first route tries again and says what came of it.
    let ctx = made(with_sink)
        .or_else(|e| if with_sink { made(false) } else { Err(e) })
        .map_err(|e| {
            format!(
                "the playback context could not be made: {}",
                shell::js_text(&e)
            )
        })?;
    // Resumed at once: made in a gesture it may start.
    let _ = ctx.resume();
    Ok(ctx)
}

async fn build(ctx: web_sys::AudioContext) -> Result<Rc<Player>, String> {
    let keep = Clone::clone(&ctx);
    let made: Result<Rc<Player>, String> = async {
        let p = assemble(ctx).await?;
        p.start().await?;
        p.first_route().await;
        Ok(p)
    }
    .await;
    match &made {
        Ok(p) => {
            p.observe_state();
            set_status(|s| s.error = None);
        }
        Err(e) => {
            // Nothing half-built stays behind; the next press makes a new one.
            let _ = keep.close();
            set_status(|s| {
                s.error = Some(e.clone());
                s.running = false;
            });
        }
    }
    made
}

async fn assemble(ctx: web_sys::AudioContext) -> Result<Rc<Player>, String> {
    let rate = ctx.sample_rate();
    if (rate - RATE as f32).abs() > 0.5 {
        return Err(format!(
            "this browser plays at {rate} Hz and refused a 24 kHz context"
        ));
    }
    let fail = |e: JsValue| shell::js_text(&e);
    let worklet = ctx.audio_worklet().map_err(fail)?;
    wasm_bindgen_futures::JsFuture::from(worklet.add_module(WORKLET_URL).map_err(fail)?)
        .await
        .map_err(|e| format!("{WORKLET_URL} did not load: {}", shell::js_text(&e)))?;
    let opts = web_sys::AudioWorkletNodeOptions::new();
    opts.set_number_of_inputs(0);
    opts.set_number_of_outputs(1);
    opts.set_output_channel_count(&js_sys::Array::of1(&JsValue::from(1)));
    let node =
        web_sys::AudioWorkletNode::new_with_options(&ctx, "lmgw-player", &opts).map_err(fail)?;
    let gain = ctx.create_gain().map_err(fail)?;
    let analyser = ctx.create_analyser().map_err(fail)?;
    analyser.set_fft_size(2048);
    node.connect_with_audio_node(&gain).map_err(fail)?;
    gain.connect_with_audio_node(&analyser).map_err(fail)?;
    analyser
        .connect_with_audio_node(&ctx.destination())
        .map_err(fail)?;
    let port = node.port().map_err(fail)?;

    let player = Rc::new(Player {
        ctx,
        node,
        port,
        gain,
        analyser,
        ledger: RefCell::new(Ledger::default()),
        activity: Cell::new(0),
        playing: Cell::new(false),
        awake: Cell::new(0),
        resuming: Cell::new(None),
        was_running: Cell::new(false),
        routed_once: Cell::new(false),
        serial: RefCell::new(route::Serial::default()),
        route_waiters: RefCell::new(Vec::new()),
        owner: RefCell::new(None),
        starve: RefCell::new(starve::Watch::default()),
        handlers: RefCell::new(None),
    });
    let weak = Rc::downgrade(&player);
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            if let Some(p) = weak.upgrade() {
                p.on_message(&ev.data());
            }
        });
    player
        .port
        .set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    // The browser may suspend or resume the context itself (autoplay rules,
    // a device change): the status follows the context, not our calls, and
    // a resume applies the route again.
    let weak = Rc::downgrade(&player);
    let on_state = Closure::<dyn FnMut()>::new(move || {
        if let Some(p) = weak.upgrade() {
            p.observe_state();
        }
    });
    player
        .ctx
        .set_onstatechange(Some(on_state.as_ref().unchecked_ref()));
    *player.handlers.borrow_mut() = Some((on_message, on_state));
    Ok(player)
}

/// The player: see the module docs.
pub(crate) struct Player {
    ctx: web_sys::AudioContext,
    #[allow(dead_code)] // kept: the node must live as long as the graph
    node: web_sys::AudioWorkletNode,
    port: web_sys::MessagePort,
    #[allow(dead_code)] // the volume, for a later control
    gain: web_sys::GainNode,
    analyser: web_sys::AnalyserNode,
    ledger: RefCell<Ledger>,
    /// Bumped by every push: an idle timer that sees it moved does nothing.
    activity: Cell<u64>,
    /// The worklet's queue holds audio (from a push to `drained`/`underrun`).
    playing: Cell<bool>,
    /// [`Awake`] guards alive.
    awake: Cell<u32>,
    /// A resume asked for and not settled yet, since when (ms).
    resuming: Cell<Option<f64>>,
    /// The context's state as last seen, to tell a resume.
    was_running: Cell<bool>,
    /// The first route was started: resumes route again from then on.
    routed_once: Cell<bool>,
    serial: RefCell<route::Serial>,
    route_waiters: RefCell<Vec<oneshot::Sender<()>>>,
    /// The item that asked to be told when another takes the player, and
    /// what to call then ([`Player::begin_owned`]).
    owner: RefCell<Option<(u32, Superseded)>>,
    /// The item whose owner is told when its audio runs dry (`starve.rs`).
    starve: RefCell<starve::Watch>,
    /// The port's and the context's handlers, kept as long as the player.
    #[allow(clippy::type_complexity)]
    handlers: RefCell<
        Option<(
            Closure<dyn FnMut(web_sys::MessageEvent)>,
            Closure<dyn FnMut()>,
        )>,
    >,
}

/// Called once when another [`Player::begin`] takes the player from an item
/// that asked to be told.
pub(crate) type Superseded = Box<dyn FnOnce()>;

/// Holds the player awake: no idle suspend while it lives ([`Player::hold_awake`]).
pub(crate) struct Awake(Rc<Player>);

impl Drop for Awake {
    fn drop(&mut self) {
        let p = &self.0;
        p.awake.set(p.awake.get().saturating_sub(1));
        if p.awake.get() == 0 && !p.playing.get() {
            p.clone().idle_after();
        }
    }
}

// Parts serve dictation and read-aloud (WP7) and the realtime panel (WP9).
impl Player {
    /// A new item — the page's one playback (§6.5, §11.1): whatever plays now
    /// is flushed first, its `end` answers `None` and its later pushes are
    /// dropped. The test tone, a read-aloud and a realtime response all
    /// start here.
    pub(crate) fn begin(self: &Rc<Self>) -> u32 {
        let live = self.ledger.borrow().any_live();
        if live {
            // Answered into the ledger; nobody waits for the counts here.
            let (id, _rx, post) = self.ledger.borrow_mut().flush(None, 0, now_ms());
            if post {
                self.post_flush(id, None);
            }
        }
        let item = self.ledger.borrow_mut().begin();
        self.starve.borrow_mut().forget(None);
        // Taken out before the call: the owner may call back in.
        let before = self.owner.borrow_mut().take();
        if let Some((_, told)) = before {
            told();
        }
        item
    }

    /// [`Self::begin`], and `superseded` is called once if another `begin`
    /// takes the player before the owner [`release`](Self::release)s the
    /// item: a read-aloud stops fetching what nobody will hear.
    pub(crate) fn begin_owned(self: &Rc<Self>, superseded: Superseded) -> u32 {
        let item = self.begin();
        *self.owner.borrow_mut() = Some((item, superseded));
        item
    }

    /// Tell `starved` when `item`'s audio runs dry before its end, and when
    /// audio for it comes again (`starve.rs`). Until it is released or
    /// another item begins.
    pub(crate) fn watch_starved(&self, item: u32, starved: Starved) {
        self.starve.borrow_mut().set(item, starved);
    }

    /// The output tap (§10's `inputs.output`): post-gain, pre-destination.
    pub(crate) fn analyser(&self) -> web_sys::AnalyserNode {
        self.analyser.clone()
    }

    /// Queue PCM16-LE bytes for `item`. Chunk boundaries need not fall on a
    /// sample: an odd byte waits for the item's next chunk. Bytes for an
    /// item that ended or was flushed are dropped (and counted, [`Self::late`]).
    pub(crate) fn push(self: &Rc<Self>, item: u32, bytes: &[u8]) {
        let whole = match self.ledger.borrow_mut().push(item, bytes) {
            Push::Post(w) => w,
            Push::Late { first } => {
                if first {
                    leptos::logging::log!(
                        "voice player: dropping audio for item {item}, which was flushed or ended"
                    );
                }
                return;
            }
        };
        self.activity.set(self.activity.get() + 1);
        self.wake(false);
        if whole.is_empty() {
            return;
        }
        self.playing.set(true);
        let told = self.starve.borrow_mut().pushed(item);
        if let Some((tell, s)) = told {
            tell(s);
        }
        let buf = js_sys::Uint8Array::from(whole.as_slice()).buffer();
        let msg = obj(&[
            ("type", JsValue::from_str("push")),
            ("item", JsValue::from(item)),
            ("pcm", buf.clone().into()),
        ]);
        let _ = self
            .port
            .post_message_with_transferable(&msg, &js_sys::Array::of1(&buf));
    }

    /// `item` is complete: resolves with its counts once it has played to
    /// its end, or `None` if a flush dropped audio of it first (or the
    /// player went away).
    pub(crate) fn end(&self, item: u32) -> impl std::future::Future<Output = Option<ItemCount>> {
        let (rx, post) = self
            .ledger
            .borrow_mut()
            .end(item, self.latency_samples(), now_ms());
        if post {
            self.post(&[
                ("type", JsValue::from_str("end")),
                ("item", JsValue::from(item)),
            ]);
        }
        rx.map(|r| r.ok().flatten())
    }

    /// Drop what is queued — of `item`, or everything — and answer how much
    /// of each item was played and heard; an item that had already played
    /// out answers its final count. The items take no more pushes.
    pub(crate) async fn flush(&self, item: Option<u32>) -> Vec<ItemCount> {
        let latency = self.latency_samples();
        let (id, rx, post) = self.ledger.borrow_mut().flush(item, latency, now_ms());
        if !post {
            return rx.await.unwrap_or_default();
        }
        self.post_flush(id, item);
        let timeout = sleep(FLUSH_WAIT_MS).fuse();
        let rx = rx.fuse();
        futures::pin_mut!(timeout, rx);
        futures::select! {
            got = rx => got.unwrap_or_default(),
            // A suspended context may not answer: the last counts known.
            _ = timeout => self.ledger.borrow_mut().flush_timed_out(id, latency, now_ms()),
        }
    }

    /// Samples of `item` rendered so far, as last reported (about every
    /// 50 ms), or its final count once it ended or was flushed. `None`: not
    /// an item of this player, or released.
    pub(crate) fn played(&self, item: u32) -> Option<u64> {
        self.ledger.borrow().played(item)
    }

    /// [`Self::played`] less the output latency the browser reports: what
    /// had come out of the device (truncate's `audio_end_ms`).
    pub(crate) fn heard(&self, item: u32) -> Option<u64> {
        self.ledger
            .borrow()
            .heard(item, self.latency_samples(), now_ms())
    }

    /// The owner is done with `item`'s final count (and with being told).
    pub(crate) fn release(&self, item: u32) {
        self.ledger.borrow_mut().release(item);
        self.starve.borrow_mut().forget(Some(item));
        let mut owner = self.owner.borrow_mut();
        if owner.as_ref().is_some_and(|(i, _)| *i == item) {
            *owner = None;
        }
    }

    /// The output latency the browser reports (`baseLatency` +
    /// `outputLatency`, where it has them), in samples. Buffering below the
    /// browser it does not report — WebKitGTK's pulsesink and PipeWire's
    /// quantum — is not in it.
    pub(crate) fn latency_samples(&self) -> u64 {
        let secs = |k: &str| {
            js_sys::Reflect::get(&self.ctx, &JsValue::from_str(k))
                .ok()
                .and_then(|v| v.as_f64())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(0.0)
        };
        ((secs("baseLatency") + secs("outputLatency")) * f64::from(RATE)).round() as u64
    }

    /// Keep the context running while the guard lives (a realtime session,
    /// a streaming read-aloud): the idle suspend applies between uses only.
    pub(crate) fn hold_awake(self: &Rc<Self>) -> Awake {
        self.awake.set(self.awake.get() + 1);
        self.wake(false);
        Awake(self.clone())
    }

    /// Resume a suspended context; the route follows on its state change.
    /// A press's resume always asks (only it may be allowed to start the
    /// context); a push's waits for one already asked.
    fn wake(self: &Rc<Self>, press: bool) {
        if self.ctx.state() == web_sys::AudioContextState::Running {
            return;
        }
        let now = now_ms();
        if !press
            && self
                .resuming
                .get()
                .is_some_and(|since| now - since < RESUME_RETRY_MS)
        {
            return;
        }
        self.resuming.set(Some(now));
        let Ok(p) = self.ctx.resume() else { return };
        let weak = Rc::downgrade(self);
        leptos::task::spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(p).await;
            if let Some(me) = weak.upgrade() {
                me.resuming.set(None);
                me.observe_state();
            }
        });
    }

    /// Wait until the new context runs, or fail saying the browser held it.
    async fn start(&self) -> Result<(), String> {
        if self.ctx.state() != web_sys::AudioContextState::Running {
            if let Ok(p) = self.ctx.resume() {
                let resumed = wasm_bindgen_futures::JsFuture::from(p).fuse();
                let timeout = sleep(START_WAIT_MS).fuse();
                futures::pin_mut!(resumed, timeout);
                futures::select! {
                    _ = resumed => {},
                    _ = timeout => {},
                }
            }
        }
        match self.ctx.state() {
            web_sys::AudioContextState::Running => Ok(()),
            s => Err(format!(
                "the browser did not let lmgw's playback start within {} s (it stayed {}): \
                 press the control again",
                START_WAIT_MS / 1000,
                state_name(s)
            )),
        }
    }

    /// The first route, awaited before the first sample plays (review M1).
    /// One that does not answer in time lets the audio go ahead, saying so.
    async fn first_route(self: &Rc<Self>) {
        // The context runs already: its own `statechange` to running may
        // still be on its way, and must not count as a resume (a second
        // route application at creation — measured in the app).
        self.was_running
            .set(self.ctx.state() == web_sys::AudioContextState::Running);
        self.routed_once.set(true);
        let settled = self.route_settled().fuse();
        self.reroute();
        let timeout = sleep(ROUTE_WAIT_MS).fuse();
        futures::pin_mut!(settled, timeout);
        futures::select! {
            _ = settled => {},
            _ = timeout => set_status(|s| {
                s.route = Route::Failed(format!(
                    "output routing did not answer within {} s; lmgw plays where the system puts it",
                    ROUTE_WAIT_MS / 1000
                ));
            }),
        }
    }

    /// Resolves when the route application running (or starting next)
    /// published its outcome.
    fn route_settled(&self) -> impl std::future::Future<Output = ()> {
        let (tx, rx) = oneshot::channel();
        self.route_waiters.borrow_mut().push(tx);
        rx.map(|_| ())
    }

    /// Apply the window's chosen output now (the choice changed, a device
    /// came or went, or the context was made or resumed). One application
    /// runs at a time; a call meanwhile makes it run once more.
    pub(crate) fn reroute(self: &Rc<Self>) {
        if !self.serial.borrow_mut().want() {
            return;
        }
        set_status(|s| s.route = Route::Pending);
        let me = self.clone();
        leptos::task::spawn_local(async move {
            loop {
                let outcome = route::apply(&me.ctx).await;
                if me.serial.borrow_mut().done() {
                    continue;
                }
                set_status(|s| {
                    s.route = match outcome {
                        Ok(r) => Route::Ok(r),
                        Err(e) => Route::Failed(e),
                    }
                });
                for w in me.route_waiters.borrow_mut().drain(..) {
                    let _ = w.send(());
                }
                break;
            }
        });
    }

    /// Follow the context's state; a resume routes again.
    fn observe_state(self: &Rc<Self>) {
        let running = self.ctx.state() == web_sys::AudioContextState::Running;
        let was = self.was_running.replace(running);
        set_status(|s| s.running = running);
        if running && !was && self.routed_once.get() {
            self.reroute();
        }
    }

    fn post(&self, fields: &[(&str, JsValue)]) {
        let _ = self.port.post_message(&obj(fields));
    }

    fn post_flush(&self, id: u32, item: Option<u32>) {
        self.post(&[
            ("type", JsValue::from_str("flush")),
            ("id", JsValue::from(id)),
            ("item", item.map(JsValue::from).unwrap_or(JsValue::NULL)),
        ]);
    }

    fn on_message(self: Rc<Self>, data: &JsValue) {
        let get = |k: &str| js_sys::Reflect::get(data, &JsValue::from_str(k)).unwrap_or_default();
        let kind = get("type").as_string().unwrap_or_default();
        match kind.as_str() {
            "progress" => {
                if let (Some(item), Some(played)) = (get("item").as_f64(), get("played").as_f64()) {
                    self.ledger
                        .borrow_mut()
                        .on_progress(item as u32, played as u64);
                }
            }
            "underrun" | "drained" => {
                if kind == "underrun" {
                    self.ledger.borrow_mut().on_underrun();
                    let waiting = waiting_items(&get("items"));
                    let told = self.starve.borrow_mut().underrun(&waiting);
                    if let Some((tell, s)) = told {
                        tell(s);
                    }
                }
                self.playing.set(false);
                self.idle_after();
            }
            "ended" => {
                let c = count_of(data);
                let latency = self.latency_samples();
                self.ledger.borrow_mut().on_ended(c, latency, now_ms());
            }
            "flushed" => {
                let id = get("id").as_f64().unwrap_or(-1.0) as u32;
                let items = counts_of(&get("items"));
                let latency = self.latency_samples();
                self.ledger
                    .borrow_mut()
                    .on_flushed(id, &items, latency, now_ms());
            }
            _ => {}
        }
    }

    /// Suspend once [`IDLE_SUSPEND_MS`] passed with no push, nothing queued
    /// and nobody holding the player awake.
    fn idle_after(self: Rc<Self>) {
        let activity = self.activity.get();
        let weak = Rc::downgrade(&self);
        set_timeout(
            move || {
                let Some(p) = weak.upgrade() else { return };
                if p.activity.get() != activity
                    || p.playing.get()
                    || p.awake.get() > 0
                    || p.ctx.state() != web_sys::AudioContextState::Running
                {
                    return;
                }
                let _ = p.ctx.suspend();
            },
            Duration::from_millis(IDLE_SUSPEND_MS),
        );
    }
}

fn state_name(s: web_sys::AudioContextState) -> &'static str {
    match s {
        web_sys::AudioContextState::Suspended => "suspended",
        web_sys::AudioContextState::Running => "running",
        web_sys::AudioContextState::Closed => "closed",
        _ => "in another state",
    }
}

/// The page's clock, ms.
fn now_ms() -> f64 {
    js_sys::Date::now()
}

fn obj(fields: &[(&str, JsValue)]) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in fields {
        let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o
}

fn count_of(v: &JsValue) -> ItemCount {
    let num = |k: &str| {
        js_sys::Reflect::get(v, &JsValue::from_str(k))
            .ok()
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    ItemCount {
        item: num("item") as u32,
        pushed: num("pushed") as u64,
        played: num("played") as u64,
        heard: 0,
    }
}

fn counts_of(v: &JsValue) -> Vec<ItemCount> {
    if !js_sys::Array::is_array(v) {
        return Vec::new();
    }
    js_sys::Array::from(v)
        .iter()
        .map(|c| count_of(&c))
        .collect()
}

/// The items an `underrun` names as still waiting for audio.
fn waiting_items(v: &JsValue) -> Vec<u32> {
    counts_of(v).into_iter().map(|c| c.item).collect()
}

/// A timer future (the page's own clock).
pub(crate) fn sleep(ms: u64) -> impl std::future::Future<Output = ()> {
    let (tx, rx) = oneshot::channel::<()>();
    set_timeout(
        move || {
            let _ = tx.send(());
        },
        Duration::from_millis(ms),
    );
    rx.map(|_| ())
}
