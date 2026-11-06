//! Following a setup that is already running (machine-check design §3.5,
//! exit 5): the system task began before lmgw was restarted, or another
//! start won its name. lmgw asks nothing and starts nothing; it asks
//! systemd whether the unit still runs (`lmgw_machine::helper::
//! unit_state_args`, `unit_running`) until it is gone, passes the unit's
//! own lines from the journal on as events while it waits, and then checks.
//!
//! **The journal, by trusted fields only** (D82). Anyone can write an entry
//! with the helper's `SYSLOG_IDENTIFIER` and an `LMGW_EVENT` of their
//! choosing through journald's socket, so the lines are read only from
//! entries whose fields journald sets itself from the sender's credentials
//! and cgroup say root inside the unit: `_SYSTEMD_UNIT=lmgw-machine-setup.
//! service`, `_UID=0`, and the invocation that runs now
//! (`_SYSTEMD_INVOCATION_ID`), both as journalctl's matches and checked
//! again on every entry it prints. A person who may not read the system
//! journal (not in `wheel`, `adm` or `systemd-journal`) gets no entries;
//! the check afterwards says what is in place either way.

use std::time::Duration;

use lmgw_machine::helper::{unit_running, Event, UNIT_SERVICE};

use super::progress::Progress;
use super::{Sink, World};
use lmgw_api_types::machine::MachineApplyEvent;

pub(crate) const SYSTEMCTL: &str = lmgw_machine::helper::SYSTEMCTL;
pub(crate) const JOURNALCTL: &str = "/usr/bin/journalctl";

/// How often systemd and the journal are asked while a setup runs: a
/// sampling rate, not a bound; the following ends when the unit does.
pub(crate) const FOLLOW_POLL: Duration = Duration::from_secs(1);

/// `systemctl show`'s question for the running unit's invocation.
pub(crate) fn invocation_args() -> [&'static str; 4] {
    ["show", "--property=InvocationID", "--value", UNIT_SERVICE]
}

/// The journal's entries of the unit's invocation `invocation`, root's
/// inside the unit only, after `cursor` when one was read already.
pub(crate) fn journal_args(invocation: &str, cursor: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--no-pager".to_string(),
        "--quiet".into(),
        // A field of 4096 bytes or more is printed as null without it.
        "--all".into(),
        "--output=json".into(),
        "--output-fields=LMGW_EVENT,_SYSTEMD_UNIT,_UID,_SYSTEMD_INVOCATION_ID".into(),
        format!("_SYSTEMD_UNIT={UNIT_SERVICE}"),
        "_UID=0".into(),
        format!("_SYSTEMD_INVOCATION_ID={invocation}"),
    ];
    if let Some(c) = cursor {
        args.push(format!("--after-cursor={c}"));
    }
    args
}

/// One journal entry of the unit's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub cursor: String,
    /// Its `LMGW_EVENT`, the helper's JSON line.
    pub event: String,
}

/// A journal field's value as journalctl's JSON prints it: a string, or
/// an array of bytes for a value that is not printable UTF-8.
fn field(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(bytes) => {
            let b: Option<Vec<u8>> = bytes
                .iter()
                .map(|x| x.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            b.map(|b| String::from_utf8_lossy(&b).into_owned())
        }
        _ => None,
    }
}

/// The entries in journalctl's `--output=json` (one object a line) that are
/// root's inside the unit's invocation `invocation` and carry an event. An
/// entry whose trusted fields say anything else is dropped, whatever its
/// `LMGW_EVENT` says.
pub(crate) fn parse_journal(text: &str, invocation: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let get = |k: &str| v.get(k).and_then(field);
            let trusted = get("_SYSTEMD_UNIT").as_deref() == Some(UNIT_SERVICE)
                && get("_UID").as_deref() == Some("0")
                && get("_SYSTEMD_INVOCATION_ID").as_deref() == Some(invocation);
            if !trusted {
                return None;
            }
            Some(Entry {
                cursor: get("__CURSOR")?,
                event: get("LMGW_EVENT")?,
            })
        })
        .collect()
}

/// How following ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Followed {
    /// The unit is gone.
    Ended,
    /// systemd could not say whether it still runs: lmgw stops following.
    Lost(String),
}

/// Follow the running unit (systemd said `state`) until it is gone,
/// passing its journal lines on through `p`.
pub(crate) async fn follow(
    world: &dyn World,
    sink: &dyn Sink,
    state: String,
    p: &mut Progress,
) -> Followed {
    sink.send(MachineApplyEvent::Following {
        state: state.clone(),
    });
    p.detail.push(format!(
        "{UNIT_SERVICE} was already {state}: lmgw followed it instead of starting another"
    ));
    let invocation = world.invocation().await;
    if invocation.is_none() {
        p.detail.push(format!(
            "systemd named no invocation of {UNIT_SERVICE}, so its journal lines were not read"
        ));
    }
    let mut cursor: Option<String> = None;
    let mut journal_said = false;
    let read = |cursor: &mut Option<String>, p: &mut Progress, entries: Vec<Entry>| {
        for e in entries {
            *cursor = Some(e.cursor);
            if let Some(ev) = Event::parse(&e.event) {
                p.event(ev, sink);
            }
        }
    };
    loop {
        if let Some(inv) = &invocation {
            match world.journal(inv, cursor.as_deref()).await {
                Ok(entries) => read(&mut cursor, p, entries),
                Err(why) if !journal_said => {
                    journal_said = true;
                    p.detail
                        .push(format!("the unit's journal could not be read: {why}"));
                }
                Err(_) => {}
            }
        }
        match world.unit_state().await {
            Ok(s) if unit_running(&s) => {}
            Ok(s) => {
                // What it wrote between the last read and its end.
                if let Some(inv) = &invocation {
                    if let Ok(entries) = world.journal(inv, cursor.as_deref()).await {
                        read(&mut cursor, p, entries);
                    }
                }
                p.detail.push(format!(
                    "{UNIT_SERVICE} is {} now",
                    if s.trim().is_empty() {
                        "gone"
                    } else {
                        s.trim()
                    }
                ));
                return Followed::Ended;
            }
            Err(why) => {
                p.detail.push(format!(
                    "systemd could not say whether {UNIT_SERVICE} still runs: {why}"
                ));
                return Followed::Lost(why);
            }
        }
        world.pause().await;
    }
}
