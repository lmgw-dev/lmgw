//! What the audio commands read out of one `pw-dump` run (chat-voice §12.3):
//! the sinks, the default one, and which playback streams are lmgw's own.
//!
//! Pure over the dump's JSON and a parent-pid lookup, so the tests run on
//! synthetic dumps and a synthetic process tree.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

type Props = Map<String, Value>;

/// One `Audio/Sink` node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sink {
    /// `node.name`: stable across sessions, what the page stores and sends.
    pub name: String,
    /// `node.description`, else `node.nick`, else the name.
    pub description: String,
    /// `object.serial`: what `target.object` is set to.
    pub serial: u64,
}

/// One `Stream/Output/Audio` node and the process it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Playback {
    /// The node's global id, the metadata subject.
    pub id: u32,
    /// The pid [`stream_pid`] settled on, if any.
    pub pid: Option<u32>,
    /// `application.process.binary`, on the node or else its client.
    pub binary: Option<String>,
    /// `application.id`, on the node or else its client.
    pub app_id: Option<String>,
}

impl Playback {
    /// Whether the stream is one of WebKit's (its web or GPU process), or
    /// carries lmgw's own stream id (`ours`, when the shell set it): a
    /// descendant of the shell that is neither — a browser `xdg-open`
    /// started as its child — is not lmgw's audio (review m2).
    fn is_webkit_or(&self, ours: Option<&str>) -> bool {
        self.binary
            .as_deref()
            .is_some_and(|b| b.starts_with("WebKit"))
            || ours.is_some_and(|id| self.app_id.as_deref() == Some(id))
    }
}

/// The objects of one dump that the commands need.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    sinks: Vec<Sink>,
    playback: Vec<Playback>,
    default_sink: Option<String>,
}

impl Graph {
    /// Every `Audio/Sink`, in the dump's order.
    pub fn sinks(&self) -> &[Sink] {
        &self.sinks
    }

    /// `default.audio.sink` from the `default` metadata: the node name
    /// WirePlumber links new streams to.
    pub fn default_sink(&self) -> Option<&str> {
        self.default_sink.as_deref()
    }

    /// The playback streams whose process is `shell_pid` or one of its
    /// descendants (`parent_of` walks the tree up) and that are WebKit's or
    /// carry lmgw's stream id `ours` ([`Playback::is_webkit_or`]).
    pub fn own_playback(
        &self,
        shell_pid: u32,
        parent_of: &dyn Fn(u32) -> Option<u32>,
        ours: Option<&str>,
    ) -> Vec<u32> {
        self.playback
            .iter()
            .filter(|p| {
                p.pid
                    .is_some_and(|pid| descends_from(pid, shell_pid, parent_of))
                    && p.is_webkit_or(ours)
            })
            .map(|p| p.id)
            .collect()
    }
}

/// Read a `pw-dump` document: a JSON array of objects.
pub(crate) fn parse(text: &str) -> Result<Graph, String> {
    let doc: Value =
        serde_json::from_str(text).map_err(|e| format!("pw-dump printed no JSON: {e}"))?;
    let objects = doc
        .as_array()
        .ok_or("pw-dump printed JSON that is not a list of objects")?;

    let clients: HashMap<u64, &Props> = objects
        .iter()
        .filter(|o| kind(o) == Some("PipeWire:Interface:Client"))
        .filter_map(|o| Some((o.get("id")?.as_u64()?, info_props(o)?)))
        .collect();

    let mut graph = Graph::default();
    for o in objects {
        match kind(o) {
            Some("PipeWire:Interface:Node") => {
                let (Some(id), Some(props)) = (o.get("id").and_then(Value::as_u64), info_props(o))
                else {
                    continue;
                };
                match text_prop(props, "media.class") {
                    Some("Audio/Sink") => {
                        if let Some(sink) = sink(props, id) {
                            graph.sinks.push(sink);
                        }
                    }
                    Some("Stream/Output/Audio") => {
                        let Ok(id) = u32::try_from(id) else { continue };
                        let client = num_prop(props, "client.id")
                            .and_then(|c| clients.get(&c))
                            .copied();
                        let text = |key: &str| {
                            text_prop(props, key)
                                .or_else(|| client.and_then(|c| text_prop(c, key)))
                                .map(str::to_string)
                        };
                        graph.playback.push(Playback {
                            id,
                            pid: stream_pid(props, client),
                            binary: text("application.process.binary"),
                            app_id: text("application.id"),
                        });
                    }
                    _ => {}
                }
            }
            Some("PipeWire:Interface:Metadata") => {
                let named_default = o
                    .get("props")
                    .and_then(Value::as_object)
                    .and_then(|p| text_prop(p, "metadata.name"))
                    == Some("default");
                if named_default {
                    graph.default_sink = default_sink(o);
                }
            }
            _ => {}
        }
    }
    Ok(graph)
}

fn kind(o: &Value) -> Option<&str> {
    o.get("type")?.as_str()
}

fn info_props(o: &Value) -> Option<&Props> {
    o.get("info")?.get("props")?.as_object()
}

fn text_prop<'a>(props: &'a Props, key: &str) -> Option<&'a str> {
    props.get(key)?.as_str()
}

/// A numeric property: `pw-dump` prints numbers for values that look like
/// one, but a client may have set the same key as a string.
fn num_prop(props: &Props, key: &str) -> Option<u64> {
    match props.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn pid_prop(props: &Props, key: &str) -> Option<u32> {
    num_prop(props, key)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&p| p > 0)
}

fn sink(props: &Props, id: u64) -> Option<Sink> {
    let name = text_prop(props, "node.name")?.to_string();
    let description = ["node.description", "node.nick"]
        .iter()
        .find_map(|k| text_prop(props, k).filter(|s| !s.trim().is_empty()))
        .unwrap_or(&name)
        .to_string();
    // Every node carries a serial since PipeWire 0.3.44; a dump without one
    // is older than `target.object` itself, so the node id stands in.
    let serial = num_prop(props, "object.serial").unwrap_or(id);
    Some(Sink {
        name,
        description,
        serial,
    })
}

/// The process a playback stream belongs to.
///
/// The kernel-verified `pipewire.sec.pid` comes first where it names the
/// stream's own process: on a native stream's node, or else its client. A
/// `pipewire-pulse` stream's `pipewire.sec.pid` is the pulse server's own pid
/// (the socket's peer) wherever it appears, on the client or on the node, so
/// a pulse stream — which is what WebKitGTK's `pulsesink` makes — is told by
/// the `application.process.id` libpulse reported, on the node or else on its
/// client.
pub(crate) fn stream_pid(node: &Props, client: Option<&Props>) -> Option<u32> {
    let pulse = |p: &Props| text_prop(p, "client.api") == Some("pipewire-pulse");
    if !pulse(node) && !client.is_some_and(pulse) {
        let kernel = pid_prop(node, "pipewire.sec.pid")
            .or_else(|| client.and_then(|c| pid_prop(c, "pipewire.sec.pid")));
        if kernel.is_some() {
            return kernel;
        }
    }
    pid_prop(node, "application.process.id")
        .or_else(|| client.and_then(|c| pid_prop(c, "application.process.id")))
}

/// `default.audio.sink` of the `default` metadata. Its value is
/// `{"name": "<node.name>"}`, which `pw-dump` prints as an object and older
/// versions as the JSON text.
fn default_sink(metadata: &Value) -> Option<String> {
    let entry = metadata.get("metadata")?.as_array()?.iter().find(|e| {
        e.get("subject").and_then(Value::as_u64) == Some(0)
            && e.get("key").and_then(Value::as_str) == Some("default.audio.sink")
    })?;
    let value = entry.get("value")?;
    let parsed;
    let object = match value {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).ok()?;
            &parsed
        }
        v => v,
    };
    Some(object.get("name")?.as_str()?.to_string())
}

/// Whether `pid` is `ancestor` or below it. A pid that leaves the tree, a
/// lookup that fails, and a loop all end the walk with `false`.
pub(crate) fn descends_from(
    pid: u32,
    ancestor: u32,
    parent_of: &dyn Fn(u32) -> Option<u32>,
) -> bool {
    let mut seen = HashSet::new();
    let mut cur = pid;
    loop {
        if cur == ancestor {
            return true;
        }
        if cur <= 1 || !seen.insert(cur) {
            return false;
        }
        match parent_of(cur) {
            Some(parent) => cur = parent,
            None => return false,
        }
    }
}

/// The parent pid from the text of `/proc/<pid>/stat`: the fourth field,
/// counted after the command name's closing parenthesis (the name itself may
/// hold spaces and parentheses).
pub(crate) fn ppid_from_stat(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The live process tree: `/proc/<pid>/stat`.
pub(crate) fn proc_parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    ppid_from_stat(&stat)
}
