//! The microphone (chat-voice §11.2).
//!
//! [`open`] asks `getUserMedia` for the window's chosen input with the echo
//! mode's constraints (§12.1) and feeds it to a capture context at the
//! device's own rate — a 24 kHz capture context is avoided, because some
//! browsers refuse a source whose rate differs from its context's. The graph
//! is source → mic `AnalyserNode` (the level tap, §10's `inputs.input`) →
//! `lmgw-capture` worklet → zero gain → destination: WebKit renders by
//! pulling from the destination, so the worklet must be connected to it. The
//! capture is only handed over once its context runs.
//!
//! The worklet hands 40 ms PCM16 chunks at the asked rate to the caller's
//! callback. While *gated* it keeps only a push-to-talk pre-roll;
//! [`Capture::close_gate`] ends an utterance to the sample and keeps the
//! microphone open.
//!
//! **Release.** [`Capture::stop`] — also on drop, and for every open capture
//! on `pagehide` — stops every track first (the microphone is free at that
//! moment), then closes the worklet, the source and the context.
//! [`Capture::finish`] does the same, then hands over the last partial chunk
//! (dictation's release): privacy first, so audio still buffered in the
//! browser when the tracks stop is not delivered.
//!
//! **Ends nobody asked for** — an unplugged device, a revoked permission, a
//! muted track, a hidden page, a stopped context — reach the owner as a
//! [`CaptureEvent`] ([`watch`]).

mod watch;

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use futures::channel::oneshot;
use futures::FutureExt;
use serde_json::json;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

pub(crate) use watch::{CaptureEvent, EndReason};

use super::devices::{constraints, resolve, DevicePick, EchoMode, Resolved, Want};
use super::listing;
use super::player::sleep;
use super::shell::js_text;

const WORKLET_URL: &str = "/voice/capture-worklet.js";
/// How long [`Capture::finish`] and [`Capture::close_gate`] wait for the
/// worklet to hand over the last partial chunk.
const HANDOVER_WAIT_MS: u64 = 500;
/// How long a new capture context may take to run.
const START_WAIT_MS: u64 = 3_000;

/// What to open.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// 24 000 (realtime) or 16 000 (dictation).
    pub rate: u32,
    pub echo: EchoMode,
    /// The window's chosen input (`None`: the system default).
    pub input: Option<DevicePick>,
    pub chunk_ms: u32,
    /// Push-to-talk's pre-roll, kept while gated (`prefix_padding_ms`).
    pub preroll_ms: u32,
    /// Start gated: chunks flow only after [`Capture::open_gate`].
    pub gated: bool,
}

impl Options {
    pub(crate) fn new(rate: u32, echo: EchoMode, input: Option<DevicePick>) -> Self {
        Self {
            rate,
            echo,
            input,
            chunk_ms: 40,
            preroll_ms: 0,
            gated: false,
        }
    }
}

/// What was opened.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Info {
    pub device_id: String,
    pub label: String,
    /// The capture context's rate: the device's own where the browser says.
    pub native_rate: u32,
    /// The chunks' rate.
    pub rate: u32,
    /// Why this is not the chosen microphone, when it is not ("… using the
    /// system default").
    pub note: Option<String>,
}

type OnChunk = Box<dyn FnMut(Vec<i16>)>;
type OnEvent = Box<dyn FnMut(CaptureEvent)>;
type OnMessage = Closure<dyn FnMut(web_sys::MessageEvent)>;

struct Inner {
    /// Itself, for the events handed to the owner later.
    this: Weak<Inner>,
    stream: web_sys::MediaStream,
    ctx: web_sys::AudioContext,
    nodes: Vec<web_sys::AudioNode>,
    port: web_sys::MessagePort,
    analyser: web_sys::AnalyserNode,
    info: Info,
    stopped: Cell<bool>,
    muted: Cell<bool>,
    flushed: RefCell<Option<oneshot::Sender<()>>>,
    gated: RefCell<Option<oneshot::Sender<()>>>,
    on_message: RefCell<Option<OnMessage>>,
    on_event: RefCell<Option<OnEvent>>,
    watch: RefCell<Option<watch::Watch>>,
}

/// An open microphone. Dropping it stops it.
pub(crate) struct Capture(Rc<Inner>);

thread_local! {
    /// Every capture still open, for `pagehide`.
    static OPEN: RefCell<Vec<Weak<Inner>>> = const { RefCell::new(Vec::new()) };
    static PAGEHIDE: Cell<bool> = const { Cell::new(false) };
}

/// Open the microphone. `on_chunk` gets each PCM16 chunk at `opts.rate`;
/// `on_event` learns of a muted track and of an end nobody asked for.
pub(crate) async fn open(
    opts: Options,
    on_chunk: OnChunk,
    on_event: OnEvent,
) -> Result<Capture, String> {
    if !listing::secure() {
        return Err("voice needs https or localhost: this page is not a secure context".into());
    }
    let md = listing::media_devices()
        .ok_or_else(|| "this browser offers no microphone access".to_string())?;
    let raw = listing::raw_devices().await.unwrap_or_default();
    let (inputs, known) = listing::of_kind(&raw, "audioinput");
    let resolved = resolve(opts.input.as_ref(), &inputs, known);
    let want = match &resolved {
        Resolved::Found { device, .. } => Want::Exact(&device.id),
        Resolved::Unknown(p) if !p.id.is_empty() => Want::Ideal(&p.id),
        _ => Want::Default,
    };
    let mut note = resolved.note("microphone");
    let (mut stream, mut fell_back) = get_user_media(&md, opts.echo, want).await?;

    // Ids were hidden before this grant: now the stored choice can be told.
    // A different device than the one opened means opening the right one.
    if let Resolved::Unknown(pick) = &resolved {
        let raw = listing::raw_devices().await.unwrap_or_default();
        let (inputs, known) = listing::of_kind(&raw, "audioinput");
        let now = resolve(Some(pick), &inputs, known);
        note = now.note("microphone");
        if let Some(id) = now.id() {
            if track_info(&stream).0 != id {
                stop_tracks(&stream);
                (stream, fell_back) = get_user_media(&md, opts.echo, Want::Exact(id)).await?;
            }
        }
    }
    if fell_back {
        // The chosen device went away between the list and the open.
        note = Some("the chosen microphone could not be opened; using the system default".into());
    }

    match build(&stream, &opts, note, on_chunk, on_event).await {
        Ok(c) => Ok(c),
        Err(e) => {
            stop_tracks(&stream);
            Err(e)
        }
    }
}

/// `getUserMedia`; `true` with the stream when an exact device was refused
/// and the system default opened instead.
async fn get_user_media(
    md: &web_sys::MediaDevices,
    echo: EchoMode,
    want: Want<'_>,
) -> Result<(web_sys::MediaStream, bool), String> {
    let ask = |want: Want<'_>| -> Result<js_sys::Promise, String> {
        let c = json!({ "audio": constraints(echo, want), "video": false });
        let c: web_sys::MediaStreamConstraints = js_sys::JSON::parse(&c.to_string())
            .map_err(|e| js_text(&e))?
            .unchecked_into();
        md.get_user_media_with_constraints(&c)
            .map_err(|e| js_text(&e))
    };
    match wasm_bindgen_futures::JsFuture::from(ask(want)?).await {
        Ok(s) => Ok((s.unchecked_into(), false)),
        // A chosen device that went away since the list was read.
        Err(e) if error_name(&e) == "OverconstrainedError" && matches!(want, Want::Exact(_)) => {
            wasm_bindgen_futures::JsFuture::from(ask(Want::Default)?)
                .await
                .map(|s| (s.unchecked_into(), true))
                .map_err(|e| gum_error(&e))
        }
        Err(e) => Err(gum_error(&e)),
    }
}

fn error_name(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &JsValue::from_str("name"))
        .ok()
        .and_then(|n| n.as_string())
        .unwrap_or_default()
}

/// A `getUserMedia` refusal in the page's words.
fn gum_error(e: &JsValue) -> String {
    match error_name(e).as_str() {
        "NotAllowedError" | "SecurityError" => "the microphone was not allowed".into(),
        "NotFoundError" => "no microphone found".into(),
        "NotReadableError" | "AbortError" => {
            "the microphone could not be started (in use, or it failed)".into()
        }
        _ => format!("the microphone could not be opened: {}", js_text(e)),
    }
}

/// The first audio track's device id and label.
fn track_info(stream: &web_sys::MediaStream) -> (String, String, Option<u32>) {
    let Some(track) = stream
        .get_audio_tracks()
        .get(0)
        .dyn_into::<web_sys::MediaStreamTrack>()
        .ok()
    else {
        return Default::default();
    };
    let settings: JsValue = track.get_settings().into();
    let get = |k: &str| js_sys::Reflect::get(&settings, &JsValue::from_str(k)).unwrap_or_default();
    (
        get("deviceId").as_string().unwrap_or_default(),
        track.label(),
        get("sampleRate")
            .as_f64()
            .map(|r| r as u32)
            .filter(|r| *r > 0),
    )
}

fn stop_tracks(stream: &web_sys::MediaStream) {
    for t in stream.get_tracks().iter() {
        if let Ok(t) = t.dyn_into::<web_sys::MediaStreamTrack>() {
            t.stop();
        }
    }
}

/// Wait until a capture context runs; the error says it did not.
async fn started(ctx: &web_sys::AudioContext) -> Result<(), String> {
    if ctx.state() != web_sys::AudioContextState::Running {
        if let Ok(p) = ctx.resume() {
            let resumed = wasm_bindgen_futures::JsFuture::from(p).fuse();
            let timeout = sleep(START_WAIT_MS).fuse();
            futures::pin_mut!(resumed, timeout);
            futures::select! {
                _ = resumed => {},
                _ = timeout => {},
            }
        }
    }
    if ctx.state() == web_sys::AudioContextState::Running {
        return Ok(());
    }
    let state = js_sys::Reflect::get(ctx, &"state".into())
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_default();
    Err(format!(
        "the microphone's audio context did not start within {} s (it stayed {state}): \
         nothing is recorded",
        START_WAIT_MS / 1000
    ))
}

async fn build(
    stream: &web_sys::MediaStream,
    opts: &Options,
    note: Option<String>,
    on_chunk: OnChunk,
    on_event: OnEvent,
) -> Result<Capture, String> {
    let (device_id, label, native) = track_info(stream);
    let ctx = match native {
        Some(rate) => {
            let o = web_sys::AudioContextOptions::new();
            o.set_sample_rate(rate as f32);
            web_sys::AudioContext::new_with_context_options(&o)
                .or_else(|_| web_sys::AudioContext::new())
        }
        None => web_sys::AudioContext::new(),
    }
    .map_err(|e| format!("the capture context could not be made: {}", js_text(&e)))?;
    let close_on_err = |e: String| {
        let _ = ctx.close();
        e
    };
    let fail = |e: JsValue| js_text(&e);
    let _ = ctx.resume();
    let worklet = ctx.audio_worklet().map_err(fail).map_err(close_on_err)?;
    let added = worklet
        .add_module(WORKLET_URL)
        .map_err(fail)
        .map_err(close_on_err)?;
    wasm_bindgen_futures::JsFuture::from(added)
        .await
        .map_err(|e| close_on_err(format!("{WORKLET_URL} did not load: {}", js_text(&e))))?;

    let wire = || -> Result<_, JsValue> {
        let source = ctx.create_media_stream_source(stream)?;
        let analyser = ctx.create_analyser()?;
        analyser.set_fft_size(2048);
        let po = js_sys::JSON::parse(
            &json!({
                "targetRate": opts.rate,
                "chunkMs": opts.chunk_ms,
                "prerollMs": opts.preroll_ms,
                "gated": opts.gated,
            })
            .to_string(),
        )?;
        let no = web_sys::AudioWorkletNodeOptions::new();
        no.set_number_of_inputs(1);
        no.set_number_of_outputs(1);
        no.set_output_channel_count(&js_sys::Array::of1(&JsValue::from(1)));
        no.set_processor_options(Some(po.unchecked_ref()));
        let node = web_sys::AudioWorkletNode::new_with_options(&ctx, "lmgw-capture", &no)?;
        let zero = ctx.create_gain()?;
        zero.gain().set_value(0.0);
        source.connect_with_audio_node(&analyser)?;
        analyser.connect_with_audio_node(&node)?;
        node.connect_with_audio_node(&zero)?;
        zero.connect_with_audio_node(&ctx.destination())?;
        let port = node.port()?;
        Ok((source, analyser, node, zero, port))
    };
    let (source, analyser, node, zero, port) = wire().map_err(fail).map_err(close_on_err)?;
    // A context that never runs would hand over a microphone that records
    // nothing while the page shows it open.
    started(&ctx).await.map_err(close_on_err)?;

    let inner = Rc::new_cyclic(|this| Inner {
        this: this.clone(),
        // `Clone::clone`, the handle: `stream.clone()` is MediaStream's own
        // `clone()`, a new stream of new live tracks — stopping those would
        // leave the microphone open (the media probe caught it; clippy.toml
        // now refuses the inherent one).
        stream: Clone::clone(stream),
        info: Info {
            device_id,
            label,
            native_rate: ctx.sample_rate() as u32,
            rate: opts.rate,
            note,
        },
        ctx,
        nodes: vec![
            source.into(),
            analyser.clone().into(),
            node.into(),
            zero.into(),
        ],
        port,
        analyser,
        stopped: Cell::new(false),
        muted: Cell::new(false),
        flushed: RefCell::new(None),
        gated: RefCell::new(None),
        on_message: RefCell::new(None),
        on_event: RefCell::new(Some(on_event)),
        watch: RefCell::new(None),
    });
    let weak = Rc::downgrade(&inner);
    let on_chunk = RefCell::new(on_chunk);
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let Some(me) = weak.upgrade() else { return };
            let data = ev.data();
            let kind = js_sys::Reflect::get(&data, &JsValue::from_str("type"))
                .ok()
                .and_then(|t| t.as_string())
                .unwrap_or_default();
            match kind.as_str() {
                "chunk" => {
                    let Ok(pcm) = js_sys::Reflect::get(&data, &JsValue::from_str("pcm")) else {
                        return;
                    };
                    let samples = js_sys::Int16Array::new(&pcm).to_vec();
                    if let Ok(mut f) = on_chunk.try_borrow_mut() {
                        f(samples);
                    }
                }
                "flushed" => {
                    if let Some(tx) = me.flushed.borrow_mut().take() {
                        let _ = tx.send(());
                    }
                }
                "gated" => {
                    if let Some(tx) = me.gated.borrow_mut().take() {
                        let _ = tx.send(());
                    }
                }
                _ => {}
            }
        });
    inner
        .port
        .set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    *inner.on_message.borrow_mut() = Some(on_message);
    let watch = watch::Watch::install(&inner);
    inner.muted.set(watch.any_muted());
    *inner.watch.borrow_mut() = Some(watch);
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        o.retain(|w| w.strong_count() > 0);
        o.push(Rc::downgrade(&inner));
    });
    install_pagehide();
    Ok(Capture(inner))
}

/// Stop every open capture when the page goes or is put into the
/// back/forward cache (§11.2); each owner learns why.
fn install_pagehide() {
    if PAGEHIDE.with(|p| p.replace(true)) {
        return;
    }
    let on_hide = Closure::<dyn FnMut()>::new(|| stop_all(EndReason::PageHidden));
    if let Some(w) = web_sys::window() {
        let _ = w.add_event_listener_with_callback("pagehide", on_hide.as_ref().unchecked_ref());
    }
    on_hide.forget();
}

/// End every capture this page has open, telling each owner `why`.
pub(crate) fn stop_all(why: EndReason) {
    let open: Vec<Rc<Inner>> = OPEN.with(|o| {
        o.borrow_mut()
            .drain(..)
            .filter_map(|w| w.upgrade())
            .collect()
    });
    for c in open {
        c.end(why.clone());
    }
}

impl Inner {
    fn stop(&self) {
        if self.stopped.replace(true) {
            return;
        }
        // The microphone first: it is free the moment this returns.
        stop_tracks(&self.stream);
        self.close_graph();
    }

    /// An end nobody asked for: stop, then tell the owner — after this
    /// handler returned, since the owner may drop the capture (and with it
    /// the handler that called this).
    fn end(&self, why: EndReason) {
        if self.stopped.get() {
            return;
        }
        self.stop();
        if let Some(mut f) = self.on_event.borrow_mut().take() {
            leptos::task::spawn_local(async move { f(CaptureEvent::Ended(why)) });
        }
    }

    fn mute_changed(&self, muted: bool) {
        if self.stopped.get() || self.muted.replace(muted) == muted {
            return;
        }
        let this = self.this.clone();
        leptos::task::spawn_local(async move {
            let Some(me) = this.upgrade() else { return };
            if me.stopped.get() {
                return;
            }
            if let Ok(mut slot) = me.on_event.try_borrow_mut() {
                if let Some(f) = slot.as_mut() {
                    f(if muted {
                        CaptureEvent::Muted
                    } else {
                        CaptureEvent::Unmuted
                    });
                }
            };
        });
    }

    fn close_graph(&self) {
        if let Some(w) = self.watch.borrow().as_ref() {
            w.detach();
        }
        self.port.set_onmessage(None);
        let _ = self.port.post_message(&msg("stop", None));
        for n in &self.nodes {
            let _ = n.disconnect();
        }
        let _ = self.ctx.close();
        // The handler itself stays until the capture is dropped: a chunk
        // callback may be what called stop, and a closure must not be freed
        // while it runs.
        for slot in [&self.flushed, &self.gated] {
            if let Some(tx) = slot.borrow_mut().take() {
                let _ = tx.send(());
            }
        }
    }

    /// Post `kind` to the worklet and wait for its answer in `slot`, at
    /// most [`HANDOVER_WAIT_MS`]; `false` when it did not come.
    async fn handover(
        &self,
        kind: &str,
        open: Option<bool>,
        slot: &RefCell<Option<oneshot::Sender<()>>>,
    ) -> bool {
        let (tx, rx) = oneshot::channel();
        *slot.borrow_mut() = Some(tx);
        let _ = self.port.post_message(&msg(kind, open));
        let timeout = sleep(HANDOVER_WAIT_MS).fuse();
        let rx = rx.fuse();
        futures::pin_mut!(timeout, rx);
        futures::select! {
            _ = rx => true,
            _ = timeout => false,
        }
    }
}

/// `{type: kind}`, with `open` for the gate.
fn msg(kind: &str, open: Option<bool>) -> JsValue {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &"type".into(), &kind.into());
    if let Some(open) = open {
        let _ = js_sys::Reflect::set(&o, &"open".into(), &JsValue::from_bool(open));
    }
    o.into()
}

// The gate, mute and finish serve dictation (WP7) and the realtime panel
// (WP9); the devices popover uses the rest.
impl Capture {
    pub(crate) fn info(&self) -> &Info {
        &self.0.info
    }

    /// The mic tap (§10's `inputs.input`).
    pub(crate) fn analyser(&self) -> web_sys::AnalyserNode {
        self.0.analyser.clone()
    }

    /// Open the gate: the pre-roll goes at once, then live chunks.
    pub(crate) fn open_gate(&self) {
        let _ = self.0.port.post_message(&msg("gate", Some(true)));
    }

    /// Close the gate and keep the microphone open (push-to-talk's
    /// release): resolves once every chunk of the utterance, its last
    /// partial one included, went to `on_chunk`. The future holds the
    /// capture's inside, not a borrow of it, so its owner may keep the
    /// capture where a borrow cannot live across the wait (WP9).
    pub(crate) fn close_gate(&self) -> impl std::future::Future<Output = ()> + 'static {
        let me = self.0.clone();
        async move {
            if me.stopped.get() {
                return;
            }
            if !me.handover("gate", Some(false), &me.gated).await {
                leptos::logging::warn!(
                    "voice capture: the worklet did not confirm the gate's close within \
                     {HANDOVER_WAIT_MS} ms; the utterance's last chunk may follow late"
                );
            }
        }
    }

    /// Mute: the tracks deliver silence, so an open turn ends naturally
    /// (§9.2's M).
    pub(crate) fn set_muted(&self, muted: bool) {
        for t in self.0.stream.get_audio_tracks().iter() {
            if let Ok(t) = t.dyn_into::<web_sys::MediaStreamTrack>() {
                t.set_enabled(!muted);
            }
        }
    }

    /// Is the track muted by the user agent (delivering silence) now?
    pub(crate) fn muted_by_system(&self) -> bool {
        self.0.muted.get()
    }

    /// Release the microphone now and close everything.
    pub(crate) fn stop(&self) {
        self.0.stop();
    }

    /// Release the microphone now, hand over the last partial chunk, then
    /// close everything (dictation's release).
    pub(crate) async fn finish(&self) {
        let me = &self.0;
        if me.stopped.replace(true) {
            return;
        }
        stop_tracks(&me.stream);
        if !me.handover("flush", None, &me.flushed).await {
            leptos::logging::warn!(
                "voice capture: the worklet did not hand over its last partial chunk within \
                 {HANDOVER_WAIT_MS} ms; up to one chunk at the end is missing"
            );
        }
        me.close_graph();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.0.stop();
    }
}
