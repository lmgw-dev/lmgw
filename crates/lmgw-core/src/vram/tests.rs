use super::attribution::CardHeld;
use super::nvml::{DeviceId, DeviceProcesses};
use super::slot_is_busy;
use super::{
    card_shares, external_table, larger_per_pid, pooled, trigger_off, unmeasured_start,
    ExternalShortfall, ExternalVerdict, GpuMemory, Measured, ProcessMemory, Reservation, Roster,
    Shares, Target,
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
    // Every reason is one clause with its own full stop: the Realtime page
    // and the hold popover add none.
    for (enabled, external) in [(true, false), (false, true)] {
        let mut off = Snapshot::default();
        off.settings.vram.enabled = enabled;
        off.settings.vram.fallback_on_external = external;
        let why = trigger_off(&off).unwrap();
        assert!(why.ends_with('.') && !why.contains(" — "), "{why}");
    }

    snap.settings.hold.active = true;
    assert_eq!(
        trigger_off(&snap).unwrap(),
        "while the hold is on, every local request goes to its own fallback or is refused."
    );
    snap.settings.vram.enabled = false;
    assert!(trigger_off(&snap).unwrap().contains("Arbitrate GPU memory"));
    snap.settings.vram.fallback_on_external = false;
    assert!(trigger_off(&snap)
        .unwrap()
        .contains("Fall back when VRAM outside"));
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
        claims: 0,
        started_at_age_seconds: 0,
        last_used_age_seconds: 0,
        warnings: vec![],
        image_capabilities: None,
        llama_props: None,
        rung: None,
        climbing: None,
        sends: 0,
        charge: None,
        resident_key: None,
        placement: Default::default(),
        owner: Default::default(),
        draining_for_owner: false,
        grant: None,
        served: false,
        adopted: false,
        engine_key: None,
        card_charge: None,
        moved: false,
    }
}

fn reservation(model: &str) -> Reservation {
    Reservation {
        id: 1,
        class: Class::Chat,
        model_id: model.into(),
        charge: super::CardCharge::over(Vec::new(), GIB),
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
/// merge per device and PID at the larger figure; a PID one reading lists
/// keeps that figure; a figure either reading could not give is unknown.
#[test]
fn two_process_readings_merge_at_the_larger_figure() {
    let pm = |pid, bytes| ProcessMemory { pid, bytes };
    let card = |n: u32, processes| DeviceProcesses {
        device: id(n),
        processes,
    };
    let merged = larger_per_pid(
        &[card(
            0,
            vec![
                pm(1, Some(10)),
                pm(2, Some(50)),
                pm(3, Some(7)),
                pm(5, None),
            ],
        )],
        &[card(
            0,
            vec![
                pm(1, Some(30)),
                pm(2, Some(20)),
                pm(4, Some(9)),
                pm(5, Some(1)),
            ],
        )],
    );
    assert_eq!(
        merged,
        vec![card(
            0,
            vec![
                pm(1, Some(30)),
                pm(2, Some(50)),
                pm(3, Some(7)),
                pm(4, Some(9)),
                pm(5, None),
            ]
        )]
    );

    // Per card (device-ledger design §2.3): a process on two cards keeps
    // the larger figure on each, and a card only the second reading lists
    // comes last.
    let merged = larger_per_pid(
        &[
            card(0, vec![pm(1, Some(10))]),
            card(1, vec![pm(1, Some(40))]),
        ],
        &[
            card(1, vec![pm(1, Some(20)), pm(2, None)]),
            card(0, vec![pm(1, Some(30))]),
            card(2, vec![pm(3, Some(1))]),
        ],
    );
    assert_eq!(
        merged,
        vec![
            card(0, vec![pm(1, Some(30))]),
            card(1, vec![pm(1, Some(40)), pm(2, None)]),
            card(2, vec![pm(3, Some(1))]),
        ]
    );
}

fn id(n: u32) -> DeviceId {
    DeviceId(format!("GPU-{n}"))
}

/// The verdict's capacity and free memory follow the ledger's rule.
#[test]
fn pooled_follows_the_ledgers_rule() {
    let dev = |total: u64, used: u64| GpuMemory {
        id: id(0),
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

/// A measurement per card (device-ledger design §2.3): each card's used
/// memory and lmgw's share there, and the sums the decisions read. The
/// outside use is the used memory less lmgw's share, both summed: a card on
/// which the process reading saw more of lmgw's than the device reading
/// counts below zero, as the pooled figure did.
#[test]
fn a_measurement_is_per_card_and_its_shares_are_the_sums() {
    let devices = [card(0, 10 * GIB), card(1, 3 * GIB)];
    let cards = card_shares(
        &devices,
        [
            held(0, Ok(6 * GIB)),
            held(1, Ok(4 * GIB)),
            held(0, Ok(GIB)),
            // A card the device read did not list: its share still counts.
            held(7, Ok(GIB)),
        ],
    );
    assert_eq!(
        cards
            .iter()
            .map(|c| (c.id.clone(), c.used, c.lmgw.clone()))
            .collect::<Vec<_>>(),
        vec![
            (id(0), 10 * GIB, Ok(7 * GIB)),
            (id(1), 3 * GIB, Ok(4 * GIB)),
            (id(7), 0, Ok(GIB)),
        ]
    );
    let m = Measured {
        cards,
        members: Vec::new(),
        total: 48 * GIB,
        raw_free: 35 * GIB,
    };
    assert_eq!(m.shares(), Ok(shares(12 * GIB, GIB)));
}

/// One card whose share is unknown (the driver lists one of lmgw's
/// processes there without a figure) is unknown on its own: the other
/// cards keep theirs, and the sums the decisions read are unknown, with
/// that card's reason (device-ledger design WP1 and WP2).
#[test]
fn an_unknown_card_is_unknown_alone_and_the_sums_with_it() {
    let devices = [card(0, 10 * GIB), card(1, 3 * GIB)];
    let cards = card_shares(
        &devices,
        [
            held(0, Ok(6 * GIB)),
            held(1, Err("chat/b is not attributed".into())),
            held(1, Ok(GIB)),
        ],
    );
    assert_eq!(cards[0].lmgw, Ok(6 * GIB));
    assert_eq!(cards[1].lmgw, Err("chat/b is not attributed".into()));
    let m = Measured {
        cards,
        members: Vec::new(),
        total: 48 * GIB,
        raw_free: 35 * GIB,
    };
    assert_eq!(m.shares(), Err("chat/b is not attributed".into()));
}

fn card(n: u32, used: u64) -> GpuMemory {
    GpuMemory {
        id: id(n),
        index: n,
        name: "g".into(),
        total_bytes: 24 * GIB,
        used_bytes: used,
        free_bytes: 24 * GIB - used,
    }
}

fn held(n: u32, bytes: Result<u64, String>) -> CardHeld {
    CardHeld {
        device: id(n),
        bytes,
    }
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

/// Words no phase types clear the phase with them, so the view never pairs
/// "looking for containers lmgw lost track of" with the eviction before
/// it; and the caller that follows the wait is not told the same phase
/// twice across them (chat-media-tools design §5.3).
#[test]
fn a_stage_in_words_clears_the_phase_and_the_watch_hears_no_repeat() {
    use super::scheduler::Waiter;
    use super::stage_watch::{follow, WaitPhase};

    let q = super::VramScheduler::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let watch = rt.block_on(follow(tx, async { super::stage_watch::current() }));
    q.live.lock().unwrap().queue.push(Waiter {
        id: 1,
        alias: "image/z".into(),
        model: "z".into(),
        class: Class::Image,
        needs: super::CardCharge::over(Vec::new(), 7 * GIB),
        since: std::time::Instant::now(),
        stage: String::new(),
        phase: None,
        watch,
        origin: crate::runtime::registry::Origin::Owner,
    });
    let phase = |q: &super::VramScheduler| q.live.lock().unwrap().queue[0].phase.clone();
    let stage = |q: &super::VramScheduler| q.live.lock().unwrap().queue[0].stage.clone();
    let room = WaitPhase::WaitingForRoom {
        needs_bytes: 7 * GIB,
        free_bytes: 2 * GIB,
        cards: Vec::new(),
    };
    q.set_phase(1, room.clone(), Some("waiting for a busy model to finish"));
    q.set_stage(1, "needs 7 GiB, 2 GiB free — evicting");
    assert_eq!(phase(&q), None);
    assert_eq!(stage(&q), "needs 7 GiB, 2 GiB free — evicting");
    q.set_phase(1, room.clone(), Some("waiting for a busy model to finish"));
    assert_eq!(phase(&q), Some(room.clone()));
    let mut heard = Vec::new();
    while let Ok(p) = rx.try_recv() {
        heard.push(p);
    }
    assert_eq!(heard, vec![room], "the same wait, told once");
}
