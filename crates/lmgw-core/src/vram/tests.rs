use super::slot_is_busy;
use super::{
    external_table, larger_per_pid, pooled, trigger_off, unmeasured_start, ExternalShortfall,
    ExternalVerdict, GpuMemory, ProcessMemory, Reservation, Roster, Shares, Target,
};
use crate::config::Snapshot;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use serde_json::json;

const GIB: u64 = 1024 * 1024 * 1024;

fn target() -> Target {
    Target {
        class: Class::Chat,
        model_id: "m".into(),
    }
}

fn shares(lmgw: u64, outside: u64) -> Shares {
    Shares { lmgw, outside }
}

/// §4.7's table, row by row, on a 24 GiB card.
#[test]
fn the_external_table_in_its_order() {
    // Larger than the capacity: a configuration error, whatever else is
    // true — never a fallback.
    assert_eq!(
        external_table(&target(), 25 * GIB, 24 * GIB, 0, shares(0, 24 * GIB)),
        ExternalVerdict::TooLarge
    );
    // Free alone suffices.
    assert_eq!(
        external_table(&target(), 8 * GIB, 24 * GIB, 10 * GIB, shares(0, 14 * GIB)),
        ExternalVerdict::Fits
    );
    // Free is short, but lmgw's own models hold enough: evict idle ones,
    // wait for busy ones — today's path. Exactly equal counts as fitting.
    assert_eq!(
        external_table(
            &target(),
            8 * GIB,
            24 * GIB,
            2 * GIB,
            shares(6 * GIB, 16 * GIB)
        ),
        ExternalVerdict::Fits
    );
    // Short even with every lmgw model gone: outside use.
    assert_eq!(
        external_table(
            &target(),
            8 * GIB,
            24 * GIB,
            2 * GIB,
            shares(5 * GIB, 17 * GIB)
        ),
        ExternalVerdict::External(ExternalShortfall {
            class: Class::Chat,
            model: "m".into(),
            needed_bytes: 8 * GIB,
            free_bytes: 2 * GIB,
            lmgw_share_bytes: 5 * GIB,
            outside_bytes: 17 * GIB,
        })
    );
}

/// The configuration gates, each named — and nothing else stops the
/// trigger at this stage.
#[test]
fn the_trigger_is_off_for_the_switch_admission_and_the_hold() {
    let mut snap = Snapshot::default();
    assert!(snap.settings.vram.fallback_on_external, "on by default");
    assert_eq!(trigger_off(&snap), None);

    snap.settings.hold.active = true;
    assert!(trigger_off(&snap).unwrap().contains("hold"));
    snap.settings.vram.enabled = false;
    assert!(trigger_off(&snap).unwrap().contains("vram.enabled"));
    snap.settings.vram.fallback_on_external = false;
    assert!(trigger_off(&snap)
        .unwrap()
        .contains("vram.fallback_on_external"));
}

/// A stored settings blob from before the switch existed reads as on.
#[test]
fn a_settings_blob_without_the_switch_reads_as_on() {
    let v: crate::config::VramSettings =
        serde_json::from_value(json!({"enabled": true, "headroom_mb": 512})).unwrap();
    assert!(v.fallback_on_external);
    assert_eq!(v.headroom_mb, 512);
}

fn entry(model: &str, state: RuntimeState) -> RuntimeView {
    RuntimeView {
        class: Class::Chat,
        model_id: model.into(),
        container_name: format!("lmgw-chat-{model}"),
        generation: 1,
        port: 1,
        state,
        in_flight: 0,
        started_at_age_seconds: 0,
        last_used_age_seconds: 0,
        warnings: vec![],
        image_capabilities: None,
        rung: None,
        climbing: None,
        sends: 0,
        charge: None,
        owner: Default::default(),
        draining_for_owner: false,
    }
}

fn reservation(model: &str) -> Reservation {
    Reservation {
        id: 1,
        class: Class::Chat,
        model_id: model.into(),
        bytes: GIB,
    }
}

/// Review finding 2: a pass compares lmgw's containers before and after
/// its reads. A container that appeared, left or changed state — or a
/// start reserved meanwhile — is a change; the in-flight count and the
/// idle age are not.
#[test]
fn the_roster_sees_every_change_to_what_lmgw_holds() {
    let at = |g: u64, state: RuntimeState| RuntimeView {
        generation: g,
        ..entry(&format!("m{g}"), state)
    };
    let base = Roster::of(
        &[at(2, RuntimeState::Ready), at(1, RuntimeState::Ready)],
        &[],
    );
    // The same containers, listed in another order, busier and older.
    let mut busy = at(1, RuntimeState::Ready);
    busy.in_flight = 3;
    busy.last_used_age_seconds = 99;
    assert_eq!(
        Roster::of(&[busy, at(2, RuntimeState::Ready)], &[]),
        base,
        "in-flight and idle age change nothing on the card"
    );
    for (what, changed) in [
        (
            "a stop begun",
            Roster::of(
                &[at(1, RuntimeState::Stopping), at(2, RuntimeState::Ready)],
                &[],
            ),
        ),
        (
            "a container gone",
            Roster::of(&[at(1, RuntimeState::Ready)], &[]),
        ),
        (
            "a container re-run (a new generation)",
            Roster::of(
                &[at(1, RuntimeState::Ready), at(3, RuntimeState::Ready)],
                &[],
            ),
        ),
        (
            "a start reserved",
            Roster::of(
                &[at(1, RuntimeState::Ready), at(2, RuntimeState::Ready)],
                &[reservation("m9")],
            ),
        ),
    ] {
        assert_ne!(changed, base, "{what}");
    }
}

/// Review finding 2: the two process readings around the device read
/// merge per PID at the larger figure; a PID one reading lists keeps
/// that figure; a figure either reading could not give is unknown.
#[test]
fn two_process_readings_merge_at_the_larger_figure() {
    let pm = |pid, bytes| ProcessMemory { pid, bytes };
    let merged = larger_per_pid(
        &[
            pm(1, Some(10)),
            pm(2, Some(50)),
            pm(3, Some(7)),
            pm(5, None),
        ],
        &[
            pm(1, Some(30)),
            pm(2, Some(20)),
            pm(4, Some(9)),
            pm(5, Some(1)),
        ],
    );
    assert_eq!(
        merged,
        vec![
            pm(1, Some(30)),
            pm(2, Some(50)),
            pm(3, Some(7)),
            pm(4, Some(9)),
            pm(5, None),
        ]
    );
}

/// The verdict's capacity and free memory follow the ledger's rule.
#[test]
fn pooled_follows_the_ledgers_rule() {
    let dev = |total: u64, used: u64| GpuMemory {
        index: 0,
        name: "g".into(),
        total_bytes: total,
        used_bytes: used,
        free_bytes: total - used,
    };
    let devices = [dev(8 * GIB, 3 * GIB), dev(4 * GIB, GIB)];
    assert_eq!(pooled(0, &devices), (12 * GIB, 8 * GIB));
    assert_eq!(pooled(6 * GIB, &devices), (6 * GIB, 2 * GIB));
    assert_eq!(pooled(2 * GIB, &devices), (2 * GIB, 0));
}

/// A start the driver cannot show yet makes the share a low guess: a
/// `starting` entry, or a reservation with no entry at all. A
/// reservation whose entry is already `ready` is measured.
#[test]
fn a_start_not_on_the_card_yet_is_named() {
    assert_eq!(
        unmeasured_start(&[entry("a", RuntimeState::Ready)], &[]),
        None
    );
    let why = unmeasured_start(
        &[
            entry("a", RuntimeState::Ready),
            entry("b", RuntimeState::Starting),
        ],
        &[],
    )
    .unwrap();
    assert!(why.contains("chat/b") && why.contains("starting"), "{why}");

    let why = unmeasured_start(&[entry("a", RuntimeState::Ready)], &[reservation("c")]).unwrap();
    assert!(why.contains("chat/c"), "{why}");
    assert_eq!(
        unmeasured_start(&[entry("a", RuntimeState::Ready)], &[reservation("a")]),
        None
    );
    // A stopping container is still a member (its PID is attributed),
    // not an unmeasured start.
    assert_eq!(
        unmeasured_start(&[entry("a", RuntimeState::Stopping)], &[]),
        None
    );
}

/// The shapes a `/slots` entry comes in, and the ways of reading them
/// wrong.
///
/// The one that bit (measured 2026-09-17 on `official-latest`): an idle
/// slot that has finished a task still carries that task's id, because
/// the server keeps `task_prev` and `to_json` falls back to it. Read as
/// "has a task id, therefore busy", every model that had answered once
/// became permanently unevictable — an idle resident held the card
/// through the whole queue timeout. Only a slot that has never run a task
/// omits the key. `is_processing` is the server's own verdict and wins
/// whenever it is present; the task id is a fallback for a build that
/// drops the flag, where `-1` is llama.cpp's serialized "no task".
#[test]
fn a_slot_is_busy_only_when_it_is_actually_working() {
    // Idle, never ran a task: no `id_task` key at all.
    assert!(!slot_is_busy(&json!({ "id": 0, "is_processing": false })));
    // Idle after finishing a task — the shape that was misread as busy.
    assert!(!slot_is_busy(
        &json!({ "id": 0, "is_processing": false, "id_task": 0 })
    ));
    assert!(!slot_is_busy(
        &json!({ "id": 0, "is_processing": false, "id_task": 7 })
    ));
    // Idle, builds that always serialize the field with -1.
    assert!(!slot_is_busy(
        &json!({ "id": 0, "is_processing": false, "id_task": -1 })
    ));
    assert!(!slot_is_busy(&json!({ "id": 0, "id_task": null })));
    assert!(!slot_is_busy(&json!({ "id": 0, "id_task": -1 })));
    // Busy: the flag says so.
    assert!(slot_is_busy(
        &json!({ "id": 0, "is_processing": true, "id_task": 7 })
    ));
    assert!(slot_is_busy(&json!({ "id": 0, "is_processing": true })));
    // No flag at all: a task id is the only evidence and is read as work
    // — skipping an eviction candidate costs a wait, killing a live
    // generation costs the work.
    assert!(slot_is_busy(&json!({ "id": 0, "id_task": 0 })));
    assert!(slot_is_busy(&json!({ "id": 0, "id_task": "abc" })));
    // The flag is a verdict, not a hint: a false flag next to an odd task
    // id is still idle.
    assert!(!slot_is_busy(
        &json!({ "id": 0, "is_processing": false, "id_task": "abc" })
    ));
}
