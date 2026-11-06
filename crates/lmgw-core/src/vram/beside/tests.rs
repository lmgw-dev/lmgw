use super::{crowded, residents_now};
use crate::runtime::Class;
use crate::vram::{ResidentView, Target};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

fn resident(class: Class, model: &str, estimated: u64, pending: Option<u64>) -> ResidentView {
    ResidentView {
        container: class,
        model: model.into(),
        state: "ready".into(),
        estimated_bytes: estimated,
        in_flight: 0,
        idle_seconds: Some(0),
        peak_extra_bytes: None,
        pending_bytes: pending,
        note: None,
    }
}

fn target(class: Class, model: &str) -> Target {
    Target {
        class,
        model_id: model.into(),
    }
}

/// Evicting an audio container that has not loaded frees what it holds
/// (in the driver's `used`) and the pending load kept free for it — its
/// expected residency, the charge, and no more. A measured 10 GiB card: a
/// kept 4 GiB chat model, and a TTS container charged 3 GiB that holds its
/// 256 MiB context, so 2.75 GiB of it stays pending. Used 4.25 GiB, raw free
/// 5.75 GiB, free 3 GiB. A 6.5 GiB stage could get 3 + 3 = 6 GiB beside the
/// kept model: crowded. Crediting the pending load on top of the charge
/// read 8.75 GiB of room that eviction cannot give.
#[test]
fn an_unloaded_audio_victim_is_credited_its_charge_once() {
    let pending = 3 * GIB - 256 * MIB;
    let residents = [
        resident(Class::Chat, "kept", 4 * GIB, None),
        resident(Class::Audio, "tts", 3 * GIB, Some(pending)),
    ];
    let free = 10 * GIB - (4 * GIB + 256 * MIB) - pending;
    assert_eq!(free, 3 * GIB);
    let stage = target(Class::Chat, "stage");
    let kept = [target(Class::Chat, "kept")];

    let c = crowded(free, &residents, &stage, &kept, 6 * GIB + GIB / 2)
        .expect("evicting the TTS container frees 3 GiB, not 5.75");
    assert_eq!(c.room, 6 * GIB);
    assert_eq!(c.kept, 4 * GIB);
    assert_eq!(
        crowded(free, &residents, &stage, &kept, 6 * GIB),
        None,
        "exactly the free memory plus the charge fits"
    );
}

/// A learned image peak is kept free on top of the charge, so evicting the
/// pipeline gives it back too; nothing is evictable that is kept, the
/// target itself, or a reservation; and with no kept model on the card the
/// group holds nothing the stage could need.
#[test]
fn what_eviction_gives_back_and_what_it_cannot() {
    let mut image = resident(Class::Image, "sd", 2 * GIB, None);
    image.peak_extra_bytes = Some(GIB);
    let mut reserved = resident(Class::Chat, "other", 5 * GIB, None);
    reserved.state = "reserved".into();
    let residents = [
        resident(Class::Chat, "kept", 4 * GIB, None),
        image,
        reserved,
        resident(Class::Chat, "stage", GIB, None),
    ];
    let stage = target(Class::Chat, "stage");
    let kept = [target(Class::Chat, "kept")];
    let c = crowded(GIB, &residents, &stage, &kept, 10 * GIB).unwrap();
    assert_eq!(c.room, GIB + 2 * GIB + GIB);
    assert_eq!(
        crowded(
            GIB,
            &residents,
            &stage,
            &[target(Class::Chat, "gone")],
            100 * GIB
        ),
        None
    );
}

/// A fit beside what runs (find-a-model design D120) charges each of lmgw's
/// models on the card its charge plus its learned peak, a start under way
/// (a reservation) included: its bytes are about to be on the card.
#[test]
fn a_fit_beside_charges_each_resident_with_its_peak() {
    let mut image = resident(Class::Image, "draw", 6 * GIB, None);
    image.peak_extra_bytes = Some(2 * GIB);
    let mut starting = resident(Class::Chat, "next", 3 * GIB, None);
    starting.state = "reserved".into();
    let now = residents_now(&[
        resident(Class::Audio, "voice", GIB, Some(GIB / 2)),
        image,
        starting,
    ]);
    assert_eq!(
        now,
        vec![
            (Class::Audio, "voice".to_string(), GIB),
            (Class::Image, "draw".to_string(), 8 * GIB),
            (Class::Chat, "next".to_string(), 3 * GIB),
        ]
    );
}
