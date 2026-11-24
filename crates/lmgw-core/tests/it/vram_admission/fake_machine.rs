//! The shared fake machine's own rules (device-ledger design §12): what a
//! machine of several cards answers, which every fake probe of the suite
//! builds its answers on (`support::fake_machine`).

use lmgw_core::vram::ProcessMemory;

use crate::support::fake_machine::{card_id, Machine};

/// The builder's own rules, on four cards of two sizes
/// (device-ledger design §1).
#[test]
fn a_machine_answers_per_card() {
    const MIB: u64 = 1024 * 1024;
    let mut m = Machine::new()
        .card(card_id(0), "RTX 6000 Ada", 49140 * MIB)
        .card(card_id(1), "RTX 6000 Ada", 49140 * MIB)
        .card(card_id(2), "RTX PRO 6000 Blackwell", 97887 * MIB)
        .card(card_id(3), "RTX 6000 Ada", 49140 * MIB);
    m.hold(1, 42, Some(5256 * MIB))
        .hold(2, 7, Some(1000 * MIB))
        .hold(2, 7, Some(MIB))
        .hold(2, 8, None)
        .unlisted(3, 300 * MIB);
    let d = m.devices();
    assert_eq!(d.len(), 4);
    assert_eq!(d[1].id, card_id(1));
    assert_eq!(d[1].index, 1);
    assert_eq!(d[1].used_bytes, 5256 * MIB);
    assert_eq!(
        d[2].used_bytes,
        1000 * MIB,
        "listed once; no figure counts nothing"
    );
    assert_eq!(d[3].used_bytes, 300 * MIB);
    assert_eq!(d[0].free_bytes, 49140 * MIB);
    let p = m.processes();
    assert_eq!(p[2].device, card_id(2));
    assert_eq!(
        p[2].processes,
        vec![
            ProcessMemory {
                pid: 7,
                bytes: Some(1000 * MIB)
            },
            ProcessMemory {
                pid: 8,
                bytes: None
            },
        ]
    );
    assert!(p[3].processes.is_empty(), "unlisted memory lists nothing");

    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        m.cards_named(&argv(&["run", "--device", "nvidia.com/gpu=all"])),
        vec![0, 1, 2, 3]
    );
    assert_eq!(
        m.cards_named(&argv(&["--device", "nvidia.com/gpu=1"])),
        vec![1]
    );
    let uuid = format!("nvidia.com/gpu={}", card_id(2));
    assert_eq!(
        m.cards_named(&argv(&["--device", &uuid, "--device", "nvidia.com/gpu=3"])),
        vec![2, 3]
    );
    assert!(m.cards_named(&argv(&["run"])).is_empty());
    assert!(m
        .cards_named(&argv(&["--device", "nvidia.com/gpu=9"]))
        .is_empty());
}
