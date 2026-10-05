//! Output routing for the voice features (chat-voice §12.3, §13.2).
//!
//! WebKitGTK cannot choose an output device, so the shell does it from the
//! outside: `audio_outputs` lists PipeWire's sinks and `audio_output_set`
//! points lmgw's own playback streams at one of them, through the `default`
//! metadata's `target.object` — the key WirePlumber's linking policy follows
//! and its restore-stream remembers. Both run `pw-dump`/`pw-metadata`
//! (pipewire-utils), which is not a hard dependency: without it the list
//! offers the system default only and says why.
//!
//! **Which streams are lmgw's** is decided by process first: every
//! `Stream/Output/Audio` node whose process is this shell or one of its
//! descendants (see [`graph::stream_pid`]) and that is WebKit's — its web or
//! GPU process — or carries lmgw's stream id. A browser `xdg-open` started as
//! the shell's child descends from it too, and is not lmgw's audio.
//!
//! **Stream identity.** WirePlumber keys what it remembers about a stream —
//! its target and its volume — by `application.id`, else `application.name`
//! (`state-stream.lua`'s `formKey`). [`apply_identity_env`] gives lmgw's
//! streams [`STREAM_APP`] for both before WebKit starts, so a target chosen
//! here is remembered for lmgw and never for another WebKitGTK app.
//!
//! The identity is process environment, so every child the shell starts
//! inherits it — WebKit's processes, and also what the gateway in this
//! process spawns (an MCP stdio server, podman, git, the PDF tools). Only
//! `xdg-open` (the browser it may start) and the `pw-*` runs here are
//! scrubbed ([`scrub_identity_env`]). A descendant that plays audio itself
//! carries lmgw's id, so it is taken for lmgw's: routed with it and
//! remembered under its key (WP11 review n1). WebKit spawns its web
//! processes on demand, so the variables cannot be dropped once it runs.

mod graph;
#[cfg(test)]
mod tests;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;

/// The deadline of one command call, child processes included:
/// `audio_output_set` waits this long for lmgw's playback stream to appear,
/// and no `pw-dump`/`pw-metadata` run may outlast it. The page calls the
/// command as it creates its playback context, so the stream is normally
/// there within a few hundred milliseconds; past this the call fails with a
/// message that names the wait.
const ROUTE_WAIT: Duration = Duration::from_secs(2);
/// Between two looks for a stream that has not appeared yet.
const ROUTE_POLL: Duration = Duration::from_millis(150);

/// The note the device list shows when it cannot list anything.
const NO_UTILS: &str = "system default only: pipewire-utils not found";

/// The name and id lmgw's audio streams carry (see the module docs). The id
/// is the desktop file's name, which is what desktop mixers look it up by. A
/// debug build's streams are `lmgw-dev`: WirePlumber remembers per id, and an
/// output chosen in a dev window must never be applied to the installed
/// app's streams, nor the other way round (review m3).
pub(crate) const STREAM_APP: &str = if cfg!(debug_assertions) {
    "lmgw-dev"
} else {
    "lmgw"
};

/// The variables [`apply_identity_env`] set, so child processes that are not
/// lmgw's audio (the browser `xdg-open` starts) do not inherit them.
static SET_BY_US: OnceLock<Vec<&'static str>> = OnceLock::new();

/// The variables that give lmgw's streams their identity, given the names of
/// the variables already in the environment. An explicit value of either
/// library's identity variables means the user decided, and that side is left
/// alone.
///
/// - **libpulse** (WebKitGTK's `pulsesink`, through pipewire-pulse) reads
///   `PULSE_PROP_<key>` only for keys the application has not set itself, and
///   GStreamer sets `application.name`; `PULSE_PROP_OVERRIDE_<key>` replaces it.
///   Only `PULSE_PROP` itself (a whole proplist, which may name either key)
///   and the two keys' own variables count; a `PULSE_PROP_media.role` set for
///   the session says nothing about the identity (WP11 review n2).
/// - **libpipewire** (a native `pipewiresink`) merges `PIPEWIRE_PROPS` into
///   every stream's properties.
pub(crate) fn identity_env(present: &[String]) -> Vec<(&'static str, String)> {
    let mut vars = Vec::new();
    let pulse_identity_set = present.iter().any(|k| {
        k == "PULSE_PROP"
            || ["application.name", "application.id"].iter().any(|key| {
                *k == format!("PULSE_PROP_{key}") || *k == format!("PULSE_PROP_OVERRIDE_{key}")
            })
    });
    if !pulse_identity_set {
        vars.push(("PULSE_PROP_OVERRIDE_application.name", STREAM_APP.into()));
        vars.push(("PULSE_PROP_OVERRIDE_application.id", STREAM_APP.into()));
    }
    if !present.iter().any(|k| k == "PIPEWIRE_PROPS") {
        vars.push((
            "PIPEWIRE_PROPS",
            format!("{{ application.name = \"{STREAM_APP}\" application.id = \"{STREAM_APP}\" }}"),
        ));
    }
    vars
}

/// Set [`identity_env`] on this process. Must run at the top of `main()`,
/// before GTK, WebKit or any thread starts: WebKit's processes inherit the
/// environment they are spawned with. Returns what was set, for the log.
pub(crate) fn apply_identity_env() -> Vec<(&'static str, String)> {
    let present: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    let vars = identity_env(&present);
    for (k, v) in &vars {
        std::env::set_var(k, v);
    }
    let _ = SET_BY_US.set(vars.iter().map(|(k, _)| *k).collect());
    vars
}

/// lmgw's stream id, when this process set it ([`apply_identity_env`]):
/// what [`graph::Graph::own_playback`] accepts besides a WebKit process.
fn identity_in_effect() -> Option<&'static str> {
    SET_BY_US
        .get()
        .is_some_and(|set| {
            set.iter()
                .any(|k| k.ends_with("application.id") || *k == "PIPEWIRE_PROPS")
        })
        .then_some(STREAM_APP)
}

/// Remove the identity variables this process set from a child's
/// environment: a child that is not lmgw's audio (see the module docs for
/// the children that are not scrubbed).
pub(crate) fn scrub_identity_env(cmd: &mut std::process::Command) {
    for k in SET_BY_US.get().map(Vec::as_slice).unwrap_or_default() {
        cmd.env_remove(k);
    }
}

/// One entry of the device list.
#[derive(Debug, Serialize, PartialEq)]
pub struct AudioOutput {
    /// `node.name`: what the page stores as the device id and sends back.
    pub name: String,
    pub description: String,
    pub serial: u64,
    /// The sink WirePlumber links new streams to (`default.audio.sink`).
    pub default: bool,
}

/// `audio_outputs`' answer.
#[derive(Debug, Serialize, PartialEq)]
pub struct AudioOutputs {
    /// Every `Audio/Sink`, in `pw-dump`'s order.
    pub outputs: Vec<AudioOutput>,
    /// Why only the system default is offered, when that is so.
    pub note: Option<String>,
}

/// `audio_output_set`'s answer.
#[derive(Debug, Serialize, PartialEq)]
pub struct Routed {
    /// The node ids of the streams that were pointed at the sink.
    pub streams: Vec<u32>,
    /// The sink's `node.name`; `None` when the streams follow the default again.
    pub sink: Option<String>,
    /// The serial `target.object` was set to.
    pub serial: Option<u64>,
}

pub(crate) fn outputs_from(graph: &graph::Graph) -> Vec<AudioOutput> {
    let default = graph.default_sink();
    graph
        .sinks()
        .iter()
        .map(|s| AudioOutput {
            name: s.name.clone(),
            description: s.description.clone(),
            serial: s.serial,
            default: Some(s.name.as_str()) == default,
        })
        .collect()
}

/// List the outputs (chat-voice §12.3).
#[tauri::command]
pub async fn audio_outputs() -> Result<AudioOutputs, String> {
    let deadline = Instant::now() + ROUTE_WAIT;
    match run("pw-dump", &[], deadline).await {
        Ok(text) => Ok(AudioOutputs {
            outputs: outputs_from(&graph::parse(&text)?),
            note: None,
        }),
        Err(RunError::Missing) => Ok(AudioOutputs {
            outputs: Vec::new(),
            note: Some(NO_UTILS.into()),
        }),
        Err(RunError::Failed(e)) => Err(e),
    }
}

/// `pw-metadata`'s argv for pointing node `id` at the sink with `serial`:
/// the form WirePlumber writes itself. With `None`, `-1`: WirePlumber's "no
/// defined target" (`find-defined-target.lua`), so the stream follows the
/// default sink. It is written rather than the key deleted because the
/// metadata overrides a target the stream carries in its own properties —
/// WebKitGTK's `pulsesink` re-opens a resumed context's stream with an
/// explicit target, the sink it last played on, which a deleted key would
/// leave in force (measured by scripts/shell-check.py) — and because
/// `state-stream.lua` stores no target for `-1`, so the remembered one is
/// cleared too.
///
/// The subject is the node's global id, which PipeWire recycles: a stream
/// that closes between the dump and this write could hand its id to another
/// client's new node. The window is one `pw-metadata` start long and is
/// accepted (review n2); WirePlumber drops a target it cannot resolve.
pub(crate) fn metadata_args(id: u32, serial: Option<u64>) -> Vec<String> {
    let target = serial.map_or_else(|| "-1".to_string(), |s| s.to_string());
    // `--`: getopt would read the value `-1` as an option.
    vec![
        "-n".into(),
        "default".into(),
        "--".into(),
        id.to_string(),
        "target.object".into(),
        target,
        "Spa:Id".into(),
    ]
}

/// Point lmgw's playback streams at `sink` (a `node.name` from
/// [`audio_outputs`]), or with `None` let them follow the system default
/// again. Waits up to [`ROUTE_WAIT`] for a stream to appear; the same
/// deadline bounds every child it runs.
#[tauri::command]
pub async fn audio_output_set(sink: Option<String>) -> Result<Routed, String> {
    let started = Instant::now();
    let deadline = started + ROUTE_WAIT;
    let shell = std::process::id();
    let ours = identity_in_effect();
    loop {
        let text = run("pw-dump", &[], deadline).await.map_err(|e| match e {
            RunError::Missing => "output routing needs pipewire-utils (pw-dump, pw-metadata), \
                                  which is not installed"
                .to_string(),
            RunError::Failed(e) => e,
        })?;
        let graph = graph::parse(&text)?;
        let target = match &sink {
            None => None,
            Some(name) => Some(
                graph
                    .sinks()
                    .iter()
                    .find(|s| &s.name == name)
                    .cloned()
                    .ok_or_else(|| format!("output '{name}' is not present"))?,
            ),
        };
        let streams = graph.own_playback(shell, &graph::proc_parent, ours);
        if streams.is_empty() {
            // Another look has to fit before the deadline, writes included.
            if Instant::now() + ROUTE_POLL >= deadline {
                return Err(format!(
                    "output routing failed: no playback stream found (waited {:.1} s)",
                    started.elapsed().as_secs_f64()
                ));
            }
            tokio::time::sleep(ROUTE_POLL).await;
            continue;
        }
        let serial = target.as_ref().map(|t| t.serial);
        let mut moved = Vec::new();
        for id in &streams {
            let args = metadata_args(*id, serial);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) = run("pw-metadata", &args, deadline).await {
                let e = match e {
                    RunError::Missing => "pw-metadata (pipewire-utils) is not installed".into(),
                    RunError::Failed(e) => e,
                };
                return Err(partial_failure(&moved, *id, &e));
            }
            moved.push(*id);
        }
        match &target {
            Some(t) => tracing::info!(
                "audio output: {} lmgw stream(s) {streams:?} -> {} (serial {})",
                streams.len(),
                t.name,
                t.serial
            ),
            None => tracing::info!(
                "audio output: {} lmgw stream(s) {streams:?} follow the system default",
                streams.len()
            ),
        }
        return Ok(Routed {
            streams,
            serial,
            sink: target.map(|t| t.name),
        });
    }
}

/// The error of a routing that failed on stream `failed` after moving
/// `moved`: those stay where they were sent, and the message says so.
pub(crate) fn partial_failure(moved: &[u32], failed: u32, error: &str) -> String {
    if moved.is_empty() {
        format!("output routing failed on stream {failed}: {error}")
    } else {
        format!("output routing moved streams {moved:?}, then failed on stream {failed}: {error}")
    }
}

enum RunError {
    /// The program is not installed.
    Missing,
    /// It ran and failed, or did not finish by the call's deadline.
    Failed(String),
}

/// Run a pipewire-utils program off the main thread and return its stdout;
/// it is killed at `deadline`, the calling command's.
async fn run(program: &str, args: &[&str], deadline: Instant) -> Result<String, RunError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for k in SET_BY_US.get().map(Vec::as_slice).unwrap_or_default() {
        cmd.env_remove(k);
    }
    let out = match tokio::time::timeout_at(deadline.into(), cmd.output()).await {
        Err(_) => {
            return Err(RunError::Failed(format!(
                "{program} gave no answer within the call's {} s",
                ROUTE_WAIT.as_secs()
            )))
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::Missing),
        Ok(Err(e)) => return Err(RunError::Failed(format!("{program}: {e}"))),
        Ok(Ok(out)) => out,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(RunError::Failed(format!(
            "{program} {}: {}",
            out.status,
            err.trim()
        )));
    }
    String::from_utf8(out.stdout).map_err(|e| RunError::Failed(format!("{program}: {e}")))
}
