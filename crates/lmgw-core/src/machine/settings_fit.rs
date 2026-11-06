//! `machine_settings_fit` (machine-check design §4.2, D18, D114–D116, D135,
//! D136, WP12): switch the model settings to the ones lmgw ships for the
//! card this computer has.
//!
//! - **Without `confirm`** it writes nothing and answers what the switch
//!   would change, field by field, and which running models it concerns,
//!   as the check's settings item lists them.
//! - **With `confirm`** (the list of changes that was shown) it works the
//!   switch out again under the settings lock, from the settings as they
//!   are then, and writes it only when that gives the confirmed list: the
//!   classes' part through the Settings page's own save (its notes, the
//!   publish, the MCP reconcile), the rows' own image and run arguments in
//!   that save's transaction, each only while the row still holds what the
//!   switch was worked out from, so all of it lands or none of it (D135).
//!   Otherwise nothing is written, and the answer says what differs and
//!   carries the switch as it is now, to confirm again.
//! - **Then the running models** whose settings it changed are stopped,
//!   none of them forced: each starts with the new settings at its next
//!   request. One answering a request keeps running with the settings from
//!   before; the answer names it and what to do, and the check says so
//!   until that container stops (D136). Then it checks the machine again
//!   and answers that check.
//! - **Refused** only when this computer has no card of the maker the
//!   switch is for: there is nothing such settings would fit (D116). No
//!   password (D18): an ordinary settings write.

use std::path::Path;
use std::sync::Arc;

use lmgw_api_types::machine::{
    MachineSettingsFit, MachineSettingsFitArgs, SettingsFit, SettingsFitChange,
    SettingsFitOutdated, SettingsFitRunning, SettingsFitStop,
};
use lmgw_machine::pci::{display_devices, Vendor};

use super::fit::present::{self, AmdCard, Present, Running};
use super::fit::switch::{self, card_name, Switch};
use crate::config::Snapshot;
use crate::runtime::lifecycle::{stop_for_apply_outcome, ApplyStop};
use crate::state::SharedState;
use crate::web::api_settings::{settings_set_full_planned, Planned, PlannedSave};

/// The display devices on the PCI bus, as §1.1 reads them, and the AMD
/// card models can be given.
struct Bus {
    vendors: Vec<Vendor>,
    amd: Option<AmdCard>,
}

async fn bus(state: &SharedState) -> Bus {
    let f = state.machine.facts();
    tokio::task::spawn_blocking(move || {
        let pci = display_devices(&*f);
        let kfd = lmgw_machine::facts::exists(&*f, Path::new(super::gather::KFD));
        let render = super::gather::render_nodes(&*f);
        Bus {
            vendors: pci.iter().map(|d| d.vendor()).collect(),
            amd: present::amd_card(&pci, kfd, &render),
        }
    })
    .await
    .unwrap_or(Bus {
        vendors: Vec::new(),
        amd: None,
    })
}

/// Why a switch to `fit` is refused on this computer, if it is: no card of
/// that maker on the PCI bus (the check's own reading of it, §1.1).
fn refusal(vendors: &[Vendor], fit: SettingsFit) -> Option<String> {
    let maker = match fit {
        SettingsFit::AmdVulkan => Vendor::Amd,
        SettingsFit::NvidiaCuda => Vendor::Nvidia,
    };
    if vendors.contains(&maker) {
        return None;
    }
    let name = card_name(fit);
    Some(
        if vendors
            .iter()
            .any(|v| matches!(v, Vendor::Nvidia | Vendor::Amd))
        {
            format!(
                "This computer has no {name} graphics card, so lmgw doesn't switch to the \
                 settings for {name} cards."
            )
        } else {
            "No graphics card lmgw can use was found, so there are no settings to switch to. \
             Cloud models work without one."
                .to_string()
        },
    )
}

/// The switch as it was worked out under the settings lock.
struct Worked {
    sw: Switch,
    before: Arc<Snapshot>,
    /// Why its patch could not be put together, when it could not.
    broken: Option<String>,
}

/// The op (module doc).
pub async fn settings_fit(
    state: &SharedState,
    args: MachineSettingsFitArgs,
) -> Result<MachineSettingsFit, String> {
    let fit = args.fit;
    let bus = bus(state).await;
    if let Some(why) = refusal(&bus.vendors, fit) {
        return Err(why);
    }
    let snap = state.snapshot();
    // Read before the lock: an image's own reading may start a throwaway
    // container, which no settings save should wait for.
    let present = Arc::new(Present {
        amd: bus.amd,
        images: present::read_images(state, &snap).await,
        running: present::running(state),
        kept: Vec::new(),
    });
    let Some(confirmed) = args.confirm else {
        let sw = switch::plan(&snap, fit, &present);
        return Ok(offer(fit, &snap, sw, &present.running, None));
    };
    let p = present.clone();
    let wanted = confirmed.clone();
    let saved = settings_set_full_planned(state, move |snap| {
        let sw = switch::plan(snap, fit, &p);
        let before = snap.clone();
        if sw.is_empty() || !same(&sw.changes, &wanted) {
            return Planned::Refuse(Worked {
                sw,
                before,
                broken: None,
            });
        }
        match serde_json::from_value(serde_json::Value::Object(sw.patch())) {
            Ok(patch) => Planned::Save {
                patch: Box::new(patch),
                rows: sw.rows.clone(),
                carry: Worked {
                    sw,
                    before,
                    broken: None,
                },
            },
            Err(e) => Planned::Refuse(Worked {
                sw,
                before,
                broken: Some(e.to_string()),
            }),
        }
    })
    .await
    .map_err(|r| r.message)?;
    let name = card_name(fit);
    match saved {
        PlannedSave::Refused(Worked {
            broken: Some(e), ..
        }) => Err(format!(
            "the settings for {name} cards could not be put together: {e}"
        )),
        PlannedSave::Refused(w) if w.sw.is_empty() => Ok(offer(fit, &w.before, w.sw, &[], None)),
        PlannedSave::Refused(w) => {
            let running = present::running(state);
            let outdated = outdated(&confirmed, &w.sw.changes, Vec::new());
            Ok(offer(fit, &w.before, w.sw, &running, Some(outdated)))
        }
        PlannedSave::RowChanged { model_id } => {
            // The row's edit may still be on its way to the snapshot: the
            // list is worked out from what is published now, and confirming
            // it is checked against the store again.
            let snap = state.snapshot();
            let sw = switch::plan(&snap, fit, &present);
            let running = present::running(state);
            let outdated = outdated(&confirmed, &sw.changes, vec![model_id]);
            Ok(offer(fit, &snap, sw, &running, Some(outdated)))
        }
        PlannedSave::Saved { answer, carry } => Ok(applied(state, fit, answer, carry).await),
    }
}

/// Whether two lists of changes are the same changes, in any order.
fn same(a: &[SettingsFitChange], b: &[SettingsFitChange]) -> bool {
    a.len() == b.len() && a.iter().all(|c| b.contains(c))
}

/// What differs between the confirmed list and the switch now.
fn outdated(
    confirmed: &[SettingsFitChange],
    now: &[SettingsFitChange],
    models: Vec<String>,
) -> SettingsFitOutdated {
    SettingsFitOutdated {
        withdrawn: confirmed
            .iter()
            .filter(|c| !now.contains(c))
            .cloned()
            .collect(),
        added: now
            .iter()
            .filter(|c| !confirmed.contains(c))
            .cloned()
            .collect(),
        models,
    }
}

/// "qwen3", "qwen3 and gemma", "a, b and c".
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("'{one}'"),
        [first @ .., last] => format!(
            "{} and '{last}'",
            first
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The answer that writes nothing: the switch `sw` worked out from `snap`,
/// the running models it concerns, and, when a confirmed list was not the
/// switch as it is now, what differs.
fn offer(
    fit: SettingsFit,
    snap: &Snapshot,
    sw: Switch,
    running: &[Running],
    outdated: Option<SettingsFitOutdated>,
) -> MachineSettingsFit {
    let name = card_name(fit);
    if sw.is_empty() {
        return MachineSettingsFit {
            fit,
            applied: false,
            sentence: format!("The model settings already are the ones for {name} cards."),
            changes: Vec::new(),
            left: sw.left,
            running: Vec::new(),
            outdated: None,
            notes: Vec::new(),
            check: None,
        };
    }
    let affected = switch::affected(snap, &sw.apply(snap), running);
    let mut sentence =
        match &outdated {
            Some(o) if !o.models.is_empty() => {
                format!(
            "{} {} changed while the settings were being switched, so nothing was changed. \
             The list here is the switch as it is now: confirm it to switch.",
            if o.models.len() == 1 { "Model" } else { "Models" },
            listed(&o.models)
        )
            }
            Some(_) => "The model settings changed after this list was made, so nothing was \
                    changed. The list here is the switch as it is now: confirm it to switch."
                .to_string(),
            None => format!(
                "Switching to the settings for {name} cards changes {}.",
                count(sw.changes.len())
            ),
        };
    if !affected.is_empty() {
        let names: Vec<String> = affected.iter().map(|r| r.model_id.clone()).collect();
        sentence.push_str(&format!(
            " Once it has switched, it stops {} so {} with the new settings, unless {} \
             answering a request then.",
            listed(&names),
            if names.len() == 1 {
                "it starts again"
            } else {
                "they start again"
            },
            if names.len() == 1 { "it is" } else { "one is" },
        ));
    }
    if outdated.is_none() {
        sentence.push_str(" Nothing is changed until it is confirmed.");
    }
    MachineSettingsFit {
        fit,
        applied: false,
        sentence,
        changes: sw.changes,
        left: sw.left,
        running: switch::offered(&affected),
        outdated,
        notes: Vec::new(),
        check: None,
    }
}

/// After the save: stop the running models it concerns, none forced
/// (D136), record those that keep running, and check the machine again.
async fn applied(
    state: &SharedState,
    fit: SettingsFit,
    answer: serde_json::Value,
    w: Worked,
) -> MachineSettingsFit {
    let name = card_name(fit);
    let after = state.snapshot();
    let affected = switch::affected(&w.before, &after, &present::running(state));
    let stops = futures::future::join_all(
        affected
            .iter()
            .map(|r| stop_for_apply_outcome(state, r.class, &r.model_id)),
    )
    .await;
    let mut running = Vec::new();
    let mut kept = Vec::new();
    let (mut stopped, mut busy, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    for (r, outcome) in affected.into_iter().zip(stops) {
        let stop = match outcome {
            ApplyStop::NotRunning => SettingsFitStop::NotRunning,
            ApplyStop::Stopped => {
                stopped.push(r.model_id.clone());
                SettingsFitStop::Stopped
            }
            ApplyStop::Busy { in_flight } => {
                busy.push(r.model_id.clone());
                SettingsFitStop::Busy { in_flight }
            }
            ApplyStop::Failed(error) => {
                failed.push(format!("'{}' ({error})", r.model_id));
                SettingsFitStop::Failed { error }
            }
        };
        if matches!(
            stop,
            SettingsFitStop::Busy { .. } | SettingsFitStop::Failed { .. }
        ) {
            kept.push(r.clone());
        }
        running.push(SettingsFitRunning {
            class: r.class.as_str().to_string(),
            model: r.model_id,
            stop: Some(stop),
        });
    }
    state.machine.keep(kept);
    let notes: Vec<String> = answer
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .into_iter()
        .collect();
    tracing::info!(
        "machine check: the model settings were switched to the ones for {name} cards: {}",
        w.sw.changes
            .iter()
            .map(switch::change_line)
            .collect::<Vec<_>>()
            .join("; ")
    );
    let mut sentence = format!("lmgw now uses the settings for {name} cards.");
    if !stopped.is_empty() {
        sentence.push_str(&format!(
            " {} {} stopped and {} with the new settings at the next request.",
            listed(&stopped),
            if stopped.len() == 1 { "was" } else { "were" },
            if stopped.len() == 1 {
                "starts"
            } else {
                "start"
            },
        ));
    }
    if !busy.is_empty() {
        sentence.push_str(&format!(
            " {} {} answering requests, so {} the old settings until {}: stop {} on the \
             Overview once {} done.",
            listed(&busy),
            if busy.len() == 1 { "is" } else { "are" },
            if busy.len() == 1 {
                "it keeps"
            } else {
                "they keep"
            },
            if busy.len() == 1 {
                "it stops"
            } else {
                "they stop"
            },
            if busy.len() == 1 { "it" } else { "them" },
            if busy.len() == 1 { "it is" } else { "they are" },
        ));
    }
    if !failed.is_empty() {
        sentence.push_str(&format!(
            " lmgw couldn't stop {}, so {} may still run with the old settings: stop {} on the \
             Overview.",
            failed.join(", "),
            if failed.len() == 1 { "it" } else { "they" },
            if failed.len() == 1 { "it" } else { "them" },
        ));
    }
    let check = (*super::check(state, true).await).clone();
    MachineSettingsFit {
        fit,
        applied: true,
        sentence,
        changes: w.sw.changes,
        left: w.sw.left,
        running,
        outdated: None,
        notes,
        check: Some(check),
    }
}

/// "1 setting", "9 settings".
fn count(n: usize) -> String {
    if n == 1 {
        "1 setting".into()
    } else {
        format!("{n} settings")
    }
}
