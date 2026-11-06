//! What an audio container holds now counts against its pending load
//! (`vram::residency::now`): a load lmgw did not route — audio.cpp's own web
//! UI, a client on the container's port — is on the card and in the
//! driver's `used` already, and keeping the whole load free on top of it
//! counted it twice (an audio model could hold 9.2 GiB while
//! 6.2 GiB of it still counted as pending). The fake container plays the outside load
//! by growing without a request through the gateway.

use super::audio_residency::{add_audio_model, audio_resident, learn, note, residency, CONTEXT};
use super::audio_residency_fixes::start_at_rest;
use super::*;

const MIB: u64 = 1024 * 1024;

/// What the container holds now, as the fake driver lists it.
fn holds(f: &Fixture, model_id: &str, bytes: u64) {
    f.world().size.insert(model_id.into(), bytes);
}

#[tokio::test]
async fn a_load_lmgw_did_not_route_is_not_kept_free_twice() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    start_at_rest(&f, "tts").await;

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["pending_bytes"], 3 * GIB - CONTEXT, "{a}");
    assert_eq!(
        v["kept_free"]["pending_load_bytes"],
        3 * GIB - CONTEXT,
        "{v}"
    );
    assert_eq!(v["kept_free"]["starting_bytes"], 0, "{v}");
    assert_eq!(v["free_bytes"], 7 * GIB, "{v}");

    // Loaded outside lmgw, larger than the learned figure: nothing pending,
    // and the free figure is the driver's own.
    holds(&f, "tts", 4 * GIB);
    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert!(a["pending_bytes"].is_null(), "{a}");
    assert_eq!(v["kept_free"]["pending_load_bytes"], 0, "{v}");
    assert_eq!(v["free_bytes"], 6 * GIB, "{v}");
    assert!(
        note(&a).contains("Loaded without an answer lmgw saw")
            && note(&a).contains("its processes hold 4.0 GiB now (measured)"),
        "{a}"
    );
    assert_eq!(
        residency(&f, "tts").map(|r| r.bytes),
        Some(3 * GIB),
        "a load lmgw did not route teaches nothing"
    );

    // Unloaded outside lmgw, its context kept and larger than at rest: only
    // the rest stays pending.
    holds(&f, "tts", 512 * MIB);
    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["pending_bytes"], 3 * GIB - 512 * MIB, "{a}");
    assert_eq!(v["free_bytes"], 7 * GIB, "{v}");
    assert!(
        note(&a).contains("its processes hold 512 MiB now, measured"),
        "{a}"
    );
}

/// Without per-process figures nothing is read, and the load stays pending
/// whole, as before.
#[tokio::test]
async fn without_per_process_figures_the_whole_load_stays_pending() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    super::audio_residency::start_idle(&f, "tts").await;
    holds(&f, "tts", 3 * GIB);

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["pending_bytes"], 3 * GIB, "{a}");
    assert_eq!(v["kept_free"]["pending_load_bytes"], 3 * GIB, "{v}");
    assert!(
        note(&a).contains("this host gives no per-process figures (FakeGPU lists no processes)"),
        "{a}"
    );
}

/// A load under way while the ledger reads (`residency::now`'s order: the
/// process list, the devices, the process list again, the smaller sum): the
/// fake container takes 256 MiB more before every process list, so the
/// first list already sees more than at rest and the second more than the
/// device read. Whatever the reading subtracts is in the device read's
/// `used`, so `used` plus the pending load covers the whole expected
/// residency — the free figure never offers memory the load is about to
/// take. A reading taken after the device read (one list after it, or the
/// larger of the two) subtracted memory `used` had not seen yet.
#[tokio::test]
async fn a_load_under_way_is_never_subtracted_before_the_device_read_saw_it() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    start_at_rest(&f, "tts").await;
    let step = 256 * MIB;
    f.world().growing.insert("tts".into(), step);

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    let pending = a["pending_bytes"].as_u64().unwrap_or(0);
    let used = v["devices"][0]["used_bytes"].as_u64().unwrap();
    assert!(
        pending + used >= 3 * GIB,
        "pending {pending} + used {used} must cover the 3 GiB residency: {v}"
    );
    assert!(
        v["free_bytes"].as_u64().unwrap() <= 7 * GIB,
        "the free figure offers part of the load: {v}"
    );
    // The reading did count: the load under way is less of it pending than
    // the at-rest figure left.
    assert!(pending <= 3 * GIB - CONTEXT - step, "{a}");
}

/// A container whose PID the cache has not read yet — nothing at rest was
/// read (no per-process figures while it started), and no pass has asked
/// podman since — is not read now either (the ledger spawns no inspect):
/// its whole load stays pending, and its note says why what it holds now
/// is not subtracted.
#[tokio::test]
async fn a_pid_not_read_yet_leaves_the_load_pending_and_says_why() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    // No outside-VRAM pass: the status view would plan the PIDs after its
    // ledger, and a broadcast's view could read them before this one.
    let mut s = f.state.snapshot().settings.clone();
    s.vram.fallback_on_external = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    super::audio_residency::start_idle(&f, "tts").await;
    // Per-process figures from here on, with the container already up and
    // its PID never asked for; it holds the whole load, loaded outside lmgw.
    f.attribute(0);
    holds(&f, "tts", 3 * GIB);

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["pending_bytes"], 3 * GIB, "{a}");
    assert!(
        note(&a).contains(
            "What it holds now was not read (a container's PID has not been read), so a load \
             lmgw did not route is not subtracted"
        ),
        "{a}"
    );
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
}
