//! The GPU's figures as both GPU surfaces say them — the shell's popover
//! (`shell.rs`) and the Traffic page's GPU panel — so the two add up the
//! same way: the free figure, what admission keeps free on top of the
//! memory in use (`VramStatus::kept_free`), the memory in use, and what the
//! probe reports as neither used nor free.

use lmgw_api_types::{VramDevice, VramStatus};

use crate::fmt::fmt_bytes;

/// What admission keeps free on top of the memory in use, all parts.
pub fn kept_free_bytes(v: &VramStatus) -> u64 {
    let k = &v.kept_free;
    k.pending_load_bytes
        .saturating_add(k.starting_bytes)
        .saturating_add(k.image_peak_bytes)
}

/// What the free figure leaves out on top of the memory in use, in one
/// line: "7.4 GiB kept free for models not loaded yet", or the sum and its
/// parts. `None` when nothing is kept free. A start is charged its estimate
/// and its margin until it is measured (device-ledger design WP13), which
/// can be more than it will hold: "models starting, not measured yet".
pub fn kept_free_line(v: &VramStatus) -> Option<String> {
    let k = &v.kept_free;
    let parts: Vec<(u64, &str)> = [
        (k.pending_load_bytes, "models not loaded yet"),
        (k.starting_bytes, "models starting, not measured yet"),
        (k.image_peak_bytes, "image generation peaks"),
    ]
    .into_iter()
    .filter(|(b, _)| *b > 0)
    .collect();
    match parts.as_slice() {
        [] => None,
        [(b, what)] => Some(format!("{} kept free for {what}", fmt_bytes(*b))),
        _ => Some(format!(
            "{} kept free: {}",
            fmt_bytes(kept_free_bytes(v)),
            parts
                .iter()
                .map(|(b, what)| format!("{what} {}", fmt_bytes(*b)))
                .collect::<Vec<_>>()
                .join(" · ")
        )),
    }
}

/// The devices' total less their used and free memory, and what to call it
/// — `None` when there is none, or the capacity is not the devices' total
/// (a declared budget, no device read). What it is depends on the probe:
/// NVML's is the driver's own reserve; an AMD APU's (amdgpu sysfs) is GTT
/// the host's RAM cannot back, its free figure clamped by `MemAvailable`;
/// any other is said as what it is, neither used nor free.
pub fn unavailable(v: &VramStatus) -> Option<(&'static str, u64)> {
    if v.devices.is_empty() {
        return None;
    }
    let total: u64 = v.devices.iter().map(|d| d.total_bytes).sum();
    let bytes: u64 = v
        .devices
        .iter()
        .map(|d| {
            d.total_bytes
                .saturating_sub(d.used_bytes)
                .saturating_sub(d.free_bytes)
        })
        .sum();
    if bytes == 0 || total != v.capacity_bytes {
        return None;
    }
    Some((unavailable_label(v), bytes))
}

/// What the probe's memory that is neither used nor free is called.
fn unavailable_label(v: &VramStatus) -> &'static str {
    if v.telemetry.starts_with("NVML") {
        "driver reserve"
    } else if v.telemetry.starts_with("amdgpu") {
        "not backed by free RAM"
    } else {
        "neither used nor free"
    }
}

/// Who holds the card, in one line: lmgw's and the outside share (left out
/// where unmeasured — never 0), the devices' used memory where the shares
/// are not measured but the devices are, and [`unavailable`]. With nothing
/// measured it says "Not measured".
pub fn holders_line(v: Option<&VramStatus>) -> String {
    let Some(v) = v else {
        return NOT_MEASURED.to_string();
    };
    let mut parts = Vec::new();
    if let Some(b) = v.lmgw_share_bytes {
        parts.push(format!("lmgw {}", fmt_bytes(b)));
    }
    if let Some(b) = v.outside_share_bytes {
        parts.push(format!("outside {}", fmt_bytes(b)));
    }
    // Only a device read measures it: with a budget and no devices there
    // is nothing to say "in use" of.
    if v.lmgw_share_bytes.is_none() && !v.devices.is_empty() {
        let used: u64 = v.devices.iter().map(|d| d.used_bytes).sum();
        parts.push(format!("in use {}", fmt_bytes(used)));
    }
    if let Some((label, bytes)) = unavailable(v) {
        parts.push(format!("{label} {}", fmt_bytes(bytes)));
    }
    sentence(parts)
}

const NOT_MEASURED: &str = "Not measured";

/// The parts joined into one line that starts with a capital ("lmgw" keeps
/// its lower case: it is the product's name), or [`NOT_MEASURED`] for none.
fn sentence(parts: Vec<String>) -> String {
    let line = parts.join(" · ");
    if line.is_empty() {
        return NOT_MEASURED.to_string();
    }
    if line.starts_with("lmgw") {
        return line;
    }
    let mut chars = line.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => line,
    }
}

/// A capacity bar's two filled parts, as percentages of the capacity: the
/// memory in use (everything not free and not kept free — what the driver
/// shows, or the ledger's estimate on a budget alone) and what admission
/// keeps free on top. `None` without a capacity or a free figure.
pub fn bar(v: &VramStatus) -> Option<(f64, f64)> {
    let cap = v.capacity_bytes;
    let free = v.free_bytes?;
    if cap == 0 {
        return None;
    }
    let taken = cap.saturating_sub(free);
    let kept = kept_free_bytes(v).min(taken);
    let used = taken - kept;
    let pct = |b: u64| (b as f64 * 100.0 / cap as f64).clamp(0.0, 100.0);
    Some((pct(used), pct(kept)))
}

/// Whether the GPU surfaces show the cards one by one (device-ledger design
/// §10): with several. A machine of one card shows its figures as before,
/// since the card's are the machine's.
pub fn per_card(v: &VramStatus) -> bool {
    v.devices.len() > 1
}

/// A card as the GPU surfaces name it: its name and its index, "RTX PRO 6000
/// Blackwell (card 2)" — never its id, which is a detail line's.
pub fn card_label(d: &VramDevice) -> String {
    format!("{} (card {})", d.name, d.index)
}

/// A card's free memory as the driver reports it, of its total.
pub fn card_free(d: &VramDevice) -> String {
    format!(
        "{} free of {}",
        fmt_bytes(d.free_bytes),
        fmt_bytes(d.total_bytes)
    )
}

/// What one part of a card's bar is.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// One of lmgw's models, by the hue its name gives.
    Model(String),
    /// lmgw's share here that no running model accounts for: a stopped
    /// model's process the driver still lists, a benchmark run's.
    LmgwOther,
    /// The used memory here that is not lmgw's.
    Outside,
    /// The used memory here, where lmgw's share is not known.
    InUse,
    /// What the probe reports as neither used nor free ([`unavailable`]).
    Unavailable,
}

/// One part of a card's bar: what it is, its bytes, its width in percent
/// of the card's total, and its words for a tooltip.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub part: Part,
    pub bytes: u64,
    pub pct: f64,
    pub title: String,
}

/// A card's bar (device-ledger design §10), in percent of its total:
/// lmgw's models on it, each with its bytes there; the rest of lmgw's
/// share there; the outside use there — or, where lmgw's share is not
/// known, the used memory as one part; then what the probe reports as
/// neither used nor free. The rest of the track is the card's free memory.
/// A model whose bytes here are not known is not drawn (the card's share
/// is not known then either). Empty parts are left out, and no part runs
/// past the card's total.
pub fn card_bar(v: &VramStatus, d: &VramDevice) -> Vec<Segment> {
    let mut parts: Vec<(Part, u64, String)> = Vec::new();
    match (d.lmgw_share_bytes, d.outside_share_bytes) {
        (Some(share), Some(outside)) => {
            let mut models: u64 = 0;
            for r in &d.resident {
                if let Some(b) = r.bytes {
                    models = models.saturating_add(b);
                    parts.push((
                        Part::Model(r.model.clone()),
                        b,
                        format!("{} ({}): {} here", r.model, r.container, fmt_bytes(b)),
                    ));
                }
            }
            let other = share.saturating_sub(models);
            parts.push((
                Part::LmgwOther,
                other,
                format!(
                    "lmgw, not a running model: {} (a stopped model the driver still lists, \
                     a benchmark run)",
                    fmt_bytes(other)
                ),
            ));
            parts.push((
                Part::Outside,
                outside,
                format!("outside lmgw: {}", fmt_bytes(outside)),
            ));
        }
        _ => parts.push((
            Part::InUse,
            d.used_bytes,
            format!("in use: {}", fmt_bytes(d.used_bytes)),
        )),
    }
    let gap = d
        .total_bytes
        .saturating_sub(d.used_bytes)
        .saturating_sub(d.free_bytes);
    let label = unavailable_label(v);
    parts.push((
        Part::Unavailable,
        gap,
        format!("{label}: {}", fmt_bytes(gap)),
    ));
    if d.total_bytes == 0 {
        return Vec::new();
    }
    let mut left = d.total_bytes;
    parts
        .into_iter()
        .filter(|(_, b, _)| *b > 0)
        .filter_map(|(part, bytes, title)| {
            let drawn = bytes.min(left);
            left -= drawn;
            (drawn > 0).then(|| Segment {
                part,
                bytes,
                pct: (drawn as f64 * 100.0 / d.total_bytes as f64).clamp(0.0, 100.0),
                title,
            })
        })
        .collect()
}

/// Who holds a card, in one line, as [`holders_line`] says it for the
/// machine: lmgw's share there and its models with their bytes there, the
/// outside use, the used memory where the shares are not known, and what
/// the probe reports as neither used nor free.
pub fn card_holders_line(v: &VramStatus, d: &VramDevice) -> String {
    let mut parts = Vec::new();
    match (d.lmgw_share_bytes, d.outside_share_bytes) {
        (Some(share), Some(outside)) => {
            let models: Vec<String> = d
                .resident
                .iter()
                .map(|r| match r.bytes {
                    Some(b) => format!("{} {}", r.model, fmt_bytes(b)),
                    None => format!("{} —", r.model),
                })
                .collect();
            parts.push(match models.is_empty() {
                true => format!("lmgw {}", fmt_bytes(share)),
                false => format!("lmgw {}: {}", fmt_bytes(share), models.join(", ")),
            });
            parts.push(format!("outside {}", fmt_bytes(outside)));
        }
        _ => {
            parts.push(format!("in use {}", fmt_bytes(d.used_bytes)));
        }
    }
    let gap = d
        .total_bytes
        .saturating_sub(d.used_bytes)
        .saturating_sub(d.free_bytes);
    if gap > 0 {
        parts.push(format!("{} {}", unavailable_label(v), fmt_bytes(gap)));
    }
    sentence(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::{DeviceId, VramDeviceResident, VramKeptFree};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn card(telemetry: &str, total: u64, used: u64, free: u64) -> VramStatus {
        VramStatus {
            telemetry: telemetry.into(),
            capacity_bytes: total,
            devices: vec![VramDevice {
                id: Default::default(),
                index: 0,
                name: "card".into(),
                total_bytes: total,
                used_bytes: used,
                free_bytes: free,
                ..VramDevice::default()
            }],
            ..VramStatus::default()
        }
    }

    #[test]
    fn the_kept_free_line_names_each_part() {
        let mut v = VramStatus::default();
        assert_eq!(kept_free_line(&v), None);
        v.kept_free = VramKeptFree {
            pending_load_bytes: 2 * GIB,
            ..VramKeptFree::default()
        };
        assert_eq!(
            kept_free_line(&v).unwrap(),
            format!("{} kept free for models not loaded yet", fmt_bytes(2 * GIB))
        );
        v.kept_free.image_peak_bytes = GIB;
        let line = kept_free_line(&v).unwrap();
        assert!(
            line.starts_with(&format!("{} kept free: ", fmt_bytes(3 * GIB))),
            "{line}"
        );
        assert_eq!(kept_free_bytes(&v), 3 * GIB);
    }

    /// The gap between total and used plus free is the driver's reserve on
    /// NVML, the RAM clamp on an AMD APU, and named neutrally elsewhere —
    /// and not said at all against a declared budget.
    #[test]
    fn the_unavailable_memory_is_named_by_its_probe() {
        let nv = card("NVML (driver 600.10)", 24 * GIB, 10 * GIB, 13 * GIB);
        assert_eq!(unavailable(&nv), Some(("driver reserve", GIB)));
        let apu = card("amdgpu sysfs (/sys/class/drm)", 16 * GIB, 2 * GIB, 4 * GIB);
        assert_eq!(
            unavailable(&apu),
            Some(("not backed by free RAM", 10 * GIB))
        );
        let other = card("FakeGPU", 8 * GIB, GIB, 6 * GIB);
        assert_eq!(unavailable(&other), Some(("neither used nor free", GIB)));
        let mut budget = nv.clone();
        budget.capacity_bytes = 20 * GIB;
        assert_eq!(unavailable(&budget), None);
        let exact = card("NVML", 24 * GIB, 10 * GIB, 14 * GIB);
        assert_eq!(unavailable(&exact), None);
    }

    /// "in use" is a device reading: said where the shares are not measured
    /// and a device was read, never for a budget with no devices.
    #[test]
    fn in_use_is_said_only_with_a_device_reading() {
        let nv = card("NVML", 24 * GIB, 10 * GIB, 14 * GIB);
        let line = holders_line(Some(&nv));
        assert!(
            line.contains(&format!("In use {}", fmt_bytes(10 * GIB))),
            "{line}"
        );
        let budget = VramStatus {
            capacity_bytes: 20 * GIB,
            ..VramStatus::default()
        };
        assert_eq!(holders_line(Some(&budget)), "Not measured");
        let mut shared = nv.clone();
        shared.lmgw_share_bytes = Some(8 * GIB);
        shared.outside_share_bytes = Some(2 * GIB);
        assert!(!holders_line(Some(&shared))
            .to_lowercase()
            .contains("in use"));
        assert_eq!(holders_line(None), "Not measured");
        assert_eq!(
            holders_line(Some(&shared)),
            format!(
                "lmgw {} · outside {}",
                fmt_bytes(8 * GIB),
                fmt_bytes(2 * GIB)
            )
        );
    }

    /// The bar's used part is not what is kept free: a 24 GiB card with
    /// 10 GiB used and 3 GiB kept free for a pending load shows 10 used and
    /// 3 kept, not 13 used.
    #[test]
    fn the_bar_shows_kept_free_apart_from_used() {
        let mut v = card("NVML", 24 * GIB, 10 * GIB, 14 * GIB);
        v.free_bytes = Some(11 * GIB);
        v.kept_free.pending_load_bytes = 3 * GIB;
        let (used, kept) = bar(&v).unwrap();
        assert!((used - 10.0 * 100.0 / 24.0).abs() < 1e-9, "{used}");
        assert!((kept - 3.0 * 100.0 / 24.0).abs() < 1e-9, "{kept}");
        v.free_bytes = None;
        assert_eq!(bar(&v), None);
    }

    /// A start not measured yet is said so where it is kept free, and a
    /// kept-free figure larger than what is not free (a start that gave its
    /// margin up is charged it until measured) never runs the bar past the
    /// card: the kept part is cut at what is taken.
    #[test]
    fn a_start_not_measured_yet_is_said_and_never_overfills_the_bar() {
        let mut v = card("NVML", 24 * GIB, 20 * GIB, 4 * GIB);
        v.free_bytes = Some(0);
        v.kept_free.starting_bytes = 30 * GIB;
        assert_eq!(
            kept_free_line(&v).unwrap(),
            format!(
                "{} kept free for models starting, not measured yet",
                fmt_bytes(30 * GIB)
            )
        );
        let (used, kept) = bar(&v).unwrap();
        assert!(used >= 0.0 && kept >= 0.0, "{used} {kept}");
        assert!((used + kept - 100.0).abs() < 1e-9, "{used} + {kept}");
    }

    /// A card of a machine with several, as the panel draws it
    /// (device-ledger design §10): named by its name and index, never its
    /// id; its bar lmgw's models with their bytes there, the rest of
    /// lmgw's share, the outside use and the driver's reserve, the track
    /// the free part; and the same in words.
    #[test]
    fn a_card_is_named_by_its_index_and_drawn_with_its_holders() {
        let v = VramStatus {
            telemetry: "NVML (driver 600.10.01)".into(),
            ..VramStatus::default()
        };
        let d = VramDevice {
            id: DeviceId::new("GPU-0b1c"),
            index: 2,
            name: "RTX PRO 6000 Blackwell".into(),
            total_bytes: 100 * GIB,
            used_bytes: 60 * GIB,
            free_bytes: 39 * GIB,
            lmgw_share_bytes: Some(12 * GIB),
            outside_share_bytes: Some(48 * GIB),
            resident: vec![
                VramDeviceResident {
                    container: "chat".into(),
                    model: "big".into(),
                    bytes: Some(10 * GIB),
                },
                VramDeviceResident {
                    container: "aux".into(),
                    model: "embed".into(),
                    bytes: Some(GIB),
                },
            ],
            ..VramDevice::default()
        };
        assert_eq!(card_label(&d), "RTX PRO 6000 Blackwell (card 2)");
        assert!(!card_label(&d).contains("GPU-0b1c"));
        let bar = card_bar(&v, &d);
        let parts: Vec<(Part, u64)> = bar.iter().map(|s| (s.part.clone(), s.bytes)).collect();
        assert_eq!(
            parts,
            vec![
                (Part::Model("big".into()), 10 * GIB),
                (Part::Model("embed".into()), GIB),
                (Part::LmgwOther, GIB),
                (Part::Outside, 48 * GIB),
                (Part::Unavailable, GIB),
            ]
        );
        let drawn: f64 = bar.iter().map(|s| s.pct).sum();
        assert!(
            (drawn - 61.0).abs() < 1e-9,
            "the free 39 % is the track: {drawn}"
        );
        assert_eq!(
            card_holders_line(&v, &d),
            format!(
                "lmgw {}: big {}, embed {} · outside {} · driver reserve {}",
                fmt_bytes(12 * GIB),
                fmt_bytes(10 * GIB),
                fmt_bytes(GIB),
                fmt_bytes(48 * GIB),
                fmt_bytes(GIB),
            )
        );

        // lmgw's share here not known: the used memory is one part, and no
        // model is drawn.
        let mut blind = d.clone();
        blind.lmgw_share_bytes = None;
        blind.outside_share_bytes = None;
        blind.share_unknown = Some("chat/big is not attributed".into());
        blind.resident[0].bytes = None;
        let parts: Vec<Part> = card_bar(&v, &blind).into_iter().map(|s| s.part).collect();
        assert_eq!(parts, vec![Part::InUse, Part::Unavailable]);
        assert!(card_holders_line(&v, &blind).starts_with("In use"));
    }

    /// The cards one by one only with several: a machine of one card is
    /// drawn as before.
    #[test]
    fn one_card_is_drawn_as_the_machine() {
        let one = card("NVML", 24 * GIB, 10 * GIB, 14 * GIB);
        assert!(!per_card(&one));
        let mut two = one.clone();
        two.devices.push(two.devices[0].clone());
        assert!(per_card(&two));
    }

    /// A bar never runs past its card: a process reading that saw more
    /// than the device reading is cut at the total.
    #[test]
    fn a_bar_is_never_longer_than_its_card() {
        let v = VramStatus::default();
        let d = VramDevice {
            total_bytes: 10 * GIB,
            used_bytes: 10 * GIB,
            lmgw_share_bytes: Some(12 * GIB),
            outside_share_bytes: Some(0),
            resident: vec![VramDeviceResident {
                container: "chat".into(),
                model: "m".into(),
                bytes: Some(12 * GIB),
            }],
            ..VramDevice::default()
        };
        let bar = card_bar(&v, &d);
        assert_eq!(bar.len(), 1);
        assert!((bar[0].pct - 100.0).abs() < 1e-9);
    }
}
