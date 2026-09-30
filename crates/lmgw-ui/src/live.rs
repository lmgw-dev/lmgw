//! SSE → signal bridge. One EventSource on `/api/events` feeds every live
//! surface (titlebar pulse, overview tiles, feeds, runtime badges); pages
//! read the slices they care about instead of opening their own streams.
//!
//! The browser's EventSource reconnects on its own; frames that fail to
//! decode are logged and skipped, the stream stays alive.

use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::UpdatesSummary;
use lmgw_api_types::{JobRow, McpStatus, RequestRow, RuntimeStatus, StatsView, VramStatus};

#[derive(Clone, Copy)]
pub struct LiveBus {
    /// Refreshed on connect and after every finished request.
    pub stats: ReadSignal<Option<StatsView>>,
    /// One value per finished request/tool call (consumers react per set).
    pub request: ReadSignal<Option<RequestRow>>,
    /// Every per-model container lmgw believes is up (per-model-containers
    /// §3.2): on connect, and whenever the registry changes. Replaces the
    /// fixed three-container list router mode broadcast.
    pub runtime: ReadSignal<Option<Vec<RuntimeStatus>>>,
    /// Full southbound MCP status list on any change.
    pub mcp: ReadSignal<Option<Vec<McpStatus>>>,
    /// Every *running* background job (§9c), refreshed on every progress tick
    /// and on every status transition. A job leaving the list is how consumers
    /// learn it finished — there is no separate completion frame.
    pub jobs: ReadSignal<Option<Vec<JobRow>>>,
    /// The GPU ledger and the admission queue (§9b): on connect, whenever a
    /// request starts or stops waiting for VRAM, and on the 5 s tick.
    pub vram: ReadSignal<Option<VramStatus>>,
    /// The Backends update counts (container builds §8): on connect, and
    /// whenever a check ends, a run finishes, a build is saved, promoted or
    /// verified, or an image is pulled.
    pub updates: ReadSignal<Option<UpdatesSummary>>,
}

pub fn use_live() -> LiveBus {
    expect_context::<LiveBus>()
}

/// Open the shared stream and install [`LiveBus`] in context. Call once, in
/// `App`.
pub fn provide_live_bus() {
    let (stats, set_stats) = signal(None);
    let (request, set_request) = signal(None);
    let (runtime, set_runtime) = signal(None);
    let (mcp, set_mcp) = signal(None);
    let (jobs, set_jobs) = signal(None);
    let (vram, set_vram) = signal(None);
    let (updates, set_updates) = signal(None);

    provide_context(LiveBus {
        stats,
        request,
        runtime,
        mcp,
        jobs,
        vram,
        updates,
    });

    // `/api/events` needs a principal (principals §3.5). While the gate is up
    // every reconnect is another 401 the browser would keep retrying, so the
    // stream is opened when the session is good and let go when it is not: the
    // effect re-runs on the way back down and opens a fresh one, which is what
    // a login does.
    let locked = crate::session::locked_signal();
    Effect::new(move |prev: Option<bool>| {
        let now = locked.get();
        if !now && prev != Some(false) {
            open_stream(Setters {
                stats: set_stats,
                request: set_request,
                runtime: set_runtime,
                mcp: set_mcp,
                jobs: set_jobs,
                vram: set_vram,
                updates: set_updates,
            });
        }
        now
    });
}

/// Which stream owns the bus.
///
/// A stream opened *before* the gate went up can still be connected — the
/// gateway checks the cookie when it accepts the connection and never again —
/// so a login would otherwise leave two EventSources feeding the same signals.
/// The newest opener wins; the older one stops at its next frame.
static STREAM_GEN: AtomicU64 = AtomicU64::new(0);

/// The bus's write ends, one per frame name.
#[derive(Clone, Copy)]
struct Setters {
    stats: WriteSignal<Option<StatsView>>,
    request: WriteSignal<Option<RequestRow>>,
    runtime: WriteSignal<Option<Vec<RuntimeStatus>>>,
    mcp: WriteSignal<Option<Vec<McpStatus>>>,
    jobs: WriteSignal<Option<Vec<JobRow>>>,
    vram: WriteSignal<Option<VramStatus>>,
    updates: WriteSignal<Option<UpdatesSummary>>,
}

/// One EventSource, alive until a newer one opens or the gate goes up.
fn open_stream(set: Setters) {
    let mine = STREAM_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    spawn_local(async move {
        let mut es = match EventSource::new("/api/events") {
            Ok(es) => es,
            Err(err) => {
                leptos::logging::error!("SSE connect /api/events: {err}");
                return;
            }
        };
        let subs = [
            "stats", "request", "runtime", "mcp", "jobs", "vram", "updates",
        ]
        .into_iter()
        .filter_map(|name| es.subscribe(name).ok());
        let mut all = futures::stream::select_all(subs);
        while let Some(item) = all.next().await {
            if STREAM_GEN.load(Ordering::Relaxed) != mine {
                break;
            }
            let Ok((name, msg)) = item else {
                // A transport hiccup reconnects itself. A locked session does
                // not fix itself, so that one ends the stream instead.
                if crate::session::is_locked() {
                    break;
                }
                continue;
            };
            let Some(text) = msg.data().as_string() else {
                continue;
            };
            fn put<T: serde::de::DeserializeOwned + Send + Sync + 'static>(
                set: WriteSignal<Option<T>>,
                text: &str,
            ) {
                match serde_json::from_str::<T>(text) {
                    Ok(v) => set.set(Some(v)),
                    Err(err) => leptos::logging::error!("SSE decode: {err}"),
                }
            }
            match name.as_str() {
                "stats" => put(set.stats, &text),
                "request" => put(set.request, &text),
                "runtime" => put(set.runtime, &text),
                "mcp" => put(set.mcp, &text),
                "jobs" => put(set.jobs, &text),
                "vram" => put(set.vram, &text),
                "updates" => put(set.updates, &text),
                _ => {}
            }
        }
        drop(es);
    });
}
