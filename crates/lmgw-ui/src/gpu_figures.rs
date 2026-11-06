//! The GPU's figures as both GPU surfaces say them — the shell's popover
//! (`shell.rs`) and the Traffic page's GPU panel — so the two add up the
//! same way: the free figure, what admission keeps free on top of the
//! memory in use (`VramStatus::kept_free`), the memory in use, and what the
//! probe reports as neither used nor free.

use lmgw_api_types::VramStatus;

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
/// parts. `None` when nothing is kept free.
pub fn kept_free_line(v: &VramStatus) -> Option<String> {
    let k = &v.kept_free;
    let parts: Vec<(u64, &str)> = [
        (k.pending_load_bytes, "models not loaded yet"),
        (k.starting_bytes, "models starting"),
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
    let label = if v.telemetry.starts_with("NVML") {
        "driver reserve"
    } else if v.telemetry.starts_with("amdgpu") {
        "not backed by free RAM"
    } else {
        "neither used nor free"
    };
    Some((label, bytes))
}

/// Who holds the card, in one line: lmgw's and the outside share (a dash
/// where unmeasured — never 0), the devices' used memory where the shares
/// are not measured but the devices are, and [`unavailable`].
pub fn holders_line(v: Option<&VramStatus>) -> String {
    let Some(v) = v else {
        return "lmgw — · outside —".to_string();
    };
    let dash = |b: Option<u64>| b.map(fmt_bytes).unwrap_or_else(|| "—".to_string());
    let mut parts = vec![
        format!("lmgw {}", dash(v.lmgw_share_bytes)),
        format!("outside {}", dash(v.outside_share_bytes)),
    ];
    // Only a device read measures it: with a budget and no devices there
    // is nothing to say "in use" of.
    if v.lmgw_share_bytes.is_none() && !v.devices.is_empty() {
        let used: u64 = v.devices.iter().map(|d| d.used_bytes).sum();
        parts.push(format!("in use {}", fmt_bytes(used)));
    }
    if let Some((label, bytes)) = unavailable(v) {
        parts.push(format!("{label} {}", fmt_bytes(bytes)));
    }
    parts.join(" · ")
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

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::{VramDevice, VramKeptFree};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn card(telemetry: &str, total: u64, used: u64, free: u64) -> VramStatus {
        VramStatus {
            telemetry: telemetry.into(),
            capacity_bytes: total,
            devices: vec![VramDevice {
                index: 0,
                name: "card".into(),
                total_bytes: total,
                used_bytes: used,
                free_bytes: free,
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
            line.contains(&format!("in use {}", fmt_bytes(10 * GIB))),
            "{line}"
        );
        let budget = VramStatus {
            capacity_bytes: 20 * GIB,
            ..VramStatus::default()
        };
        assert_eq!(holders_line(Some(&budget)), "lmgw — · outside —");
        let mut shared = nv.clone();
        shared.lmgw_share_bytes = Some(8 * GIB);
        shared.outside_share_bytes = Some(2 * GIB);
        assert!(!holders_line(Some(&shared)).contains("in use"));
        assert_eq!(holders_line(None), "lmgw — · outside —");
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
}
