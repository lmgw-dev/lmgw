//! PCI listings (machine-check design §6, "Driver facts"): an NVIDIA card on
//! `nvidia`, on `nouveau`, none at all, and an AMD card.

use lmgw_machine::fake::FakeFacts;
use lmgw_machine::pci::{display_devices, names, Vendor};

const PCI_IDS: &str = "\
# a pci.ids excerpt
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t15bf  Phoenix1
\t\t1002 15bf  a subsystem line
10de  NVIDIA Corporation
\t2204  GA102 [GeForce RTX 3090]
\t2685  GA102 [another]
8086  Intel Corporation
\t4680  AlderLake-S GT1
";

fn device(
    f: &FakeFacts,
    addr: &str,
    class: &str,
    vendor: &str,
    device: &str,
    driver: Option<&str>,
) {
    let dir = format!("/sys/bus/pci/devices/{addr}");
    f.file(format!("{dir}/class"), format!("{class}\n"))
        .file(format!("{dir}/vendor"), format!("{vendor}\n"))
        .file(format!("{dir}/device"), format!("{device}\n"));
    if let Some(d) = driver {
        f.link(
            format!("{dir}/driver"),
            format!("../../../bus/pci/drivers/{d}"),
        );
    }
}

fn machine() -> FakeFacts {
    let f = FakeFacts::new();
    f.file("/usr/share/hwdata/pci.ids", PCI_IDS);
    // A USB controller: not a display device.
    device(
        &f,
        "0000:00:14.0",
        "0x0c0330",
        "0x8086",
        "0x7ae0",
        Some("xhci_hcd"),
    );
    f
}

#[test]
fn an_nvidia_card_on_its_own_driver_is_listed_with_its_names() {
    let f = machine();
    device(
        &f,
        "0000:01:00.0",
        "0x030000",
        "0x10de",
        "0x2204",
        Some("nvidia"),
    );
    let cards = display_devices(&f);
    assert_eq!(cards.len(), 1, "{cards:?}");
    let c = &cards[0];
    assert_eq!(c.address, "0000:01:00.0");
    assert_eq!(c.vendor(), Vendor::Nvidia);
    assert_eq!(c.driver.as_deref(), Some("nvidia"));
    assert_eq!(c.vendor_name.as_deref(), Some("NVIDIA Corporation"));
    assert_eq!(c.device_name.as_deref(), Some("GA102 [GeForce RTX 3090]"));
}

#[test]
fn an_nvidia_card_on_nouveau_names_nouveau() {
    let f = machine();
    device(
        &f,
        "0000:01:00.0",
        "0x030000",
        "0x10de",
        "0x2204",
        Some("nouveau"),
    );
    assert_eq!(display_devices(&f)[0].driver.as_deref(), Some("nouveau"));
}

#[test]
fn a_card_with_no_driver_bound_says_so() {
    let f = machine();
    device(&f, "0000:01:00.0", "0x030200", "0x10de", "0x2204", None);
    let cards = display_devices(&f);
    assert_eq!(cards[0].driver, None);
    assert_eq!(
        cards[0].class, 0x030200,
        "a 3D controller is a display device too"
    );
}

#[test]
fn no_display_device_is_an_empty_list() {
    assert!(display_devices(&machine()).is_empty());
    assert!(display_devices(&FakeFacts::new()).is_empty());
}

#[test]
fn an_amd_apu_and_an_intel_igpu_are_listed_in_bus_order() {
    let f = machine();
    device(
        &f,
        "0000:c5:00.0",
        "0x038000",
        "0x1002",
        "0x15bf",
        Some("amdgpu"),
    );
    device(
        &f,
        "0000:00:02.0",
        "0x030000",
        "0x8086",
        "0x4680",
        Some("i915"),
    );
    let cards = display_devices(&f);
    assert_eq!(
        cards.iter().map(|c| c.vendor()).collect::<Vec<_>>(),
        vec![Vendor::Intel, Vendor::Amd]
    );
    assert_eq!(cards[1].device_name.as_deref(), Some("Phoenix1"));
}

#[test]
fn names_skip_subsystem_lines_and_stop_at_the_next_vendor() {
    assert_eq!(
        names(PCI_IDS, 0x1002, 0x15bf),
        (
            Some("Advanced Micro Devices, Inc. [AMD/ATI]".into()),
            Some("Phoenix1".into())
        )
    );
    // A device the vendor does not list: the vendor's name only.
    assert_eq!(
        names(PCI_IDS, 0x10de, 0x9999),
        (Some("NVIDIA Corporation".into()), None)
    );
    assert_eq!(names(PCI_IDS, 0x1234, 0x0001), (None, None));
}
