use lmgw_api_types::{AgentField, AgentImportReport, AgentRunSummary};
use serde_json::Value;

use crate::widgets::schema_form::{access_of, is_mount};

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The export URL. Config is deployment state, so it is left out unless asked
/// for; secrets are never in it either way, and the file says which were
/// omitted (§5).
pub fn export_href(id: &str, include_config: bool) -> String {
    if include_config {
        format!("/api/agents/{id}/export?include_config=1")
    } else {
        format!("/api/agents/{id}/export")
    }
}

/// The id Duplicate offers: the same rule `local_model_set duplicate` uses, so
/// two "make me another one of these" actions do not spell it differently.
pub fn copy_id(id: &str) -> String {
    format!("{id}-copy")
}

/// The host an agent origin is dialled on: `http://board.localhost:8001/` →
/// `board.localhost` (origins §4.1).
///
/// The App tab's first standing line names it and prints the `/etc/hosts` line
/// that fixes it, so what it returns has to be the exact name a resolver is
/// asked for — the port is the gateway's and no resolver ever sees it.
pub fn origin_host(origin: &str) -> String {
    let rest = origin.split_once("://").map_or(origin, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    strip_port(authority)
}

/// Host part of an authority, port removed: `board.localhost:8001` →
/// `board.localhost`, `[::]:8001` → `[::]`. An IPv6 literal keeps its
/// brackets, because that is how it is spelled everywhere a URL is.
fn strip_port(authority: &str) -> String {
    let a = authority.trim();
    if a.starts_with('[') {
        if let Some(end) = a.find(']') {
            return a[..=end].to_string();
        }
    }
    match a.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            host.to_string()
        }
        _ => a.to_string(),
    }
}

/// Is this a host only the machine lmgw runs on can dial?
fn is_loopback_host(host: &str) -> bool {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    h == "localhost"
        || h.parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// The bind host §4.9's second standing line names, or `None` when the line
/// does not apply.
///
/// `*.localhost` is resolved to loopback by the browser, or by the resolver of
/// whichever machine is asking — so a gateway that anything but this machine
/// can reach hands out origins that only work here. A wildcard bind counts: no
/// second machine dials `0.0.0.0`, but every one of them reaches it. A suffix
/// the owner has already moved off the default is their DNS's business, and
/// the line stays away from it.
pub fn lan_origin_host(bind_addr: &str, suffix: &str) -> Option<String> {
    if suffix.trim() != "localhost" {
        return None;
    }
    let host = strip_port(bind_addr);
    (!host.is_empty() && !is_loopback_host(&host)).then_some(host)
}

/// One run's progress as the Runs tab prints it. `total` is unknown until the
/// source step has listed, and an unknown total is said rather than guessed at.
pub fn progress_text(r: &AgentRunSummary) -> String {
    match r.total {
        Some(total) => format!("{} / {total}", r.done),
        None if r.done > 0 => format!("{}", r.done),
        None => "—".to_string(),
    }
}

/// The live progress line: what the run is doing, and how far in. The stage is
/// the executor's own word for it ("listing", "fetching", "classifying",
/// "applying"), so this never has to guess which one a phase is on.
pub fn run_line(r: &AgentRunSummary) -> String {
    let stage = if r.stage.is_empty() {
        r.phase.clone()
    } else {
        r.stage.clone()
    };
    format!("{stage} {}", progress_text(r))
}

/// Attention rows first: the output landed on the fallback, or the call failed
/// (§2.4). They are the rows a person actually has to look at.
pub fn attention_note(error: Option<&str>, raw: Option<&str>) -> Option<String> {
    if let Some(e) = error {
        return Some(format!("call failed: {e}"));
    }
    raw.map(|r| format!("raw: {r}"))
}

/// How long a run took. `None` is "still going", which is not a duration —
/// the Runs tab says so rather than printing a zero or an elapsed time that
/// would keep moving between two reads of the same row.
pub fn duration_text(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "—".to_string();
    };
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{}.{} s", secs, (ms % 1000) / 100);
    }
    format!("{} m {:02} s", secs / 60, secs % 60)
}

/// The settled group's heading (its count is the pill beside it).
/// "Classified" is a claim about the rows, so it is only made when every one
/// of them carries an answer: a list-only run and a run still in flight both
/// have rows that were never sent to a model, and calling those classified
/// would be a lie the table tells for half a second on every run.
pub fn settled_heading(all_answered: bool) -> &'static str {
    if all_answered {
        "Classified"
    } else {
        "Rows"
    }
}

/// Whether a run is still going, which is the only state Cancel applies to.
pub fn is_live(r: &AgentRunSummary) -> bool {
    matches!(r.status.as_str(), "queued" | "running")
}

/// Why a manifest that declares a mount runs differently, printed with the
/// start summary (mounts §5.5).
///
/// A property of the **manifest**, not of the form: it holds whether the slots
/// are bound or not, so one image always runs one way.
pub const KEEP_ID_NOTE: &str =
    "this manifest declares mounts: the container runs as your uid (--userns=keep-id)";

/// The mount fields a manifest declares, in the author's order (mounts §5.1).
pub fn mount_fields(fields: &[AgentField]) -> Vec<AgentField> {
    fields.iter().filter(|f| is_mount(f)).cloned().collect()
}

/// One bound mount, as the Run tab's start summary prints it (mounts §5.8):
/// the host path, the path the container actually sees, and the mode. Before
/// the start, not only in the log afterwards — principle 4.
pub fn mount_line(f: &AgentField, host: &str) -> String {
    format!(
        "{host} → /lmgw/mounts/{} ({}, {})",
        f.name,
        access_of(f),
        f.format,
    )
}

/// The one line the export note and the import report both print about mount
/// slots (mounts §5.2): an export carries the **names** and never a host path,
/// so the receiving box has something to bind and nothing to undo.
pub fn unbound_line(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| {
        format!(
            "slots to bind after import: {} — an export never carries a host path, so the \
             receiver says which folder fills each one.",
            names.join(", "),
        )
    })
}

/// One bound host mount of a service container, as `dto::AgentService.mounts`
/// carries it (mounts §5.7).
///
/// Read out of the detail's **raw** document rather than off `AgentService`:
/// the list is what the container was started with, and the DTO field lands
/// with the core half of this work package. Reading the document means the App
/// tab draws the list whether or not this build's `lmgw-api-types` has caught
/// up, and a gateway that sends nothing simply has no mounts to list.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ServiceMount {
    /// The config field that bound it.
    pub field: String,
    /// The path on this machine. Absent for a slot nothing has bound — which
    /// is not a mount, and is not listed.
    pub host: Option<String>,
    /// What the container sees: `/lmgw/mounts/<field>`.
    pub inside: String,
    /// `directory` or `file`.
    pub kind: String,
    /// `ro` or `rw`.
    pub access: String,
}

/// The service's bound mounts, out of a `GET /api/agents/{id}` document.
///
/// Deliberately tolerant: an absent key, a `null`, an entry shaped some other
/// way or one with no host path all read as "nothing to list". A tab is not the
/// place to fail over the half of a document it did not need.
pub fn service_mounts(detail: &Value) -> Vec<ServiceMount> {
    detail
        .get("service")
        .and_then(|s| s.get("mounts"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| serde_json::from_value::<ServiceMount>(r.clone()).ok())
                .filter(|m| m.host.as_deref().is_some_and(|h| !h.trim().is_empty()))
                .collect()
        })
        .unwrap_or_default()
}

/// Every warning a manifest save came back with, in the order the server put
/// them — an ordering warning and a missing server are both worth seeing, and
/// neither blocks the save (§5).
pub fn report_lines(r: &AgentImportReport) -> Vec<String> {
    let mut out = Vec::new();
    if r.validate_only {
        out.push(format!("'{}' checks out — nothing written.", r.id));
    } else if r.replaced {
        out.push(format!("saved '{}'.", r.id));
    } else {
        out.push(format!("created '{}'.", r.id));
    }
    out.extend(r.warnings.iter().cloned());
    // The slots this row now has and nothing has bound (mounts §5.2). Not a
    // warning about the manifest — a thing left for the owner to do.
    out.extend(unbound_line(&r.config_unbound));
    out
}
