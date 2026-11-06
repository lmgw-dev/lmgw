//! Byte figures in words: the one formatter every size lmgw says to a
//! person goes through (files, downloads and their progress, disk space,
//! graphics-card memory, transfer rates, body sizes, notes and tool
//! sentences). Owner ruling 2026-11-04: files are binary, so every figure is
//! in binary units, labelled `KiB`, `MiB`, `GiB` and `TiB`, and `B` below
//! one KiB. No decimal GB, MB or kB anywhere.
//!
//! The precision rule (find-a-model design D160): whole bytes below 1 KiB;
//! from there the largest unit whose rounded figure stays below 1024, with
//! one decimal below 100 of it and whole figures from 100 on ("812 B",
//! "1.5 KiB", "24.0 GiB", "644 MiB"). The unit is picked by the rounded
//! figure, so 1023.97 MiB is "1.0 GiB", never "1024.0 MiB".

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// How a figure is said: its unit and how many decimals it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Said {
    unit: usize,
    decimals: usize,
}

impl Said {
    /// Bytes per unit.
    fn per(self) -> f64 {
        1024f64.powi(self.unit as i32)
    }

    /// Bytes per step said (a tenth of the unit, or the unit).
    fn step(self) -> f64 {
        self.per() / 10f64.powi(self.decimals as i32)
    }

    /// `steps` in figures, without the unit.
    fn figures(self, steps: u64) -> String {
        let v = steps as f64 * self.step() / self.per();
        format!("{v:.*}", self.decimals)
    }
}

/// The unit and decimals `b` is said in, rounded to the nearest step.
fn said(b: u64) -> Said {
    let mut unit = 0;
    loop {
        let s = Said {
            unit,
            decimals: decimals(unit, b as f64 / 1024f64.powi(unit as i32)),
        };
        let rounded = (b as f64 / s.step()).round() * s.step() / s.per();
        if unit + 1 < UNITS.len() && rounded >= 1024.0 {
            unit += 1;
            continue;
        }
        // 99.96 rounds to "100.0": say it whole, as 100 is.
        if s.decimals > 0 && rounded >= 100.0 {
            return Said { unit, decimals: 0 };
        }
        return s;
    }
}

/// Like [`said`], for a figure rounded down: the unit is picked by the
/// rounded-down figure, so 1023.97 MiB stays "1023 MiB".
fn said_down(b: u64) -> Said {
    let mut unit = 0;
    loop {
        let s = Said {
            unit,
            decimals: decimals(unit, b as f64 / 1024f64.powi(unit as i32)),
        };
        let floored = (b as f64 / s.step()).floor() * s.step() / s.per();
        if unit + 1 < UNITS.len() && floored >= 1024.0 {
            unit += 1;
            continue;
        }
        return s;
    }
}

fn decimals(unit: usize, v: f64) -> usize {
    if unit == 0 || v >= 100.0 {
        0
    } else {
        1
    }
}

fn words(s: Said, steps: u64) -> String {
    format!("{} {}", s.figures(steps), UNITS[s.unit])
}

/// A size, rounded: "812 B", "1.5 KiB", "24.0 GiB", "644 MiB".
pub fn fmt_bytes(b: u64) -> String {
    let s = said(b);
    words(s, (b as f64 / s.step()).round() as u64)
}

/// Bytes counted so far with no total, rounded down so the figure never
/// claims more than arrived: "1.2 GiB", "300 B".
pub fn fmt_bytes_so_far(b: u64) -> String {
    let s = said_down(b);
    words(s, (b as f64 / s.step()).floor() as u64)
}

/// Progress, "1.2 of 3.4 GiB": both in the total's unit and decimals, the
/// total rounded, the part done rounded down and never said as the total
/// while bytes are still missing.
pub fn fmt_bytes_of(done: u64, total: u64) -> String {
    let s = said(total);
    let t = (total as f64 / s.step()).round() as u64;
    let d = if done >= total {
        t
    } else {
        ((done as f64 / s.step()).floor() as u64).min(t.saturating_sub(1))
    };
    format!("{} of {}", s.figures(d), words(s, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = KIB * KIB;
    const GIB: u64 = MIB * KIB;

    #[test]
    fn binary_units_one_decimal_below_a_hundred() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(812), "812 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(24 * GIB), "24.0 GiB");
        assert_eq!(fmt_bytes(17_000_000_000), "15.8 GiB");
        assert_eq!(fmt_bytes(675_492_659), "644 MiB");
        assert_eq!(fmt_bytes(512 * MIB), "512 MiB");
        assert_eq!(fmt_bytes(3 * 1024 * GIB), "3.0 TiB");
    }

    #[test]
    fn the_unit_follows_the_rounded_figure() {
        // 1023.97 MiB rounds to 1024: a GiB.
        assert_eq!(fmt_bytes(GIB - 30_000), "1.0 GiB");
        assert_eq!(fmt_bytes(1020 * MIB), "1020 MiB");
        // 99.96 KiB is said whole, not "100.0 KiB".
        assert_eq!(fmt_bytes(102_360), "100 KiB");
        assert_eq!(fmt_bytes(99 * KIB), "99.0 KiB");
    }

    #[test]
    fn progress_in_the_totals_unit_never_says_done_early() {
        assert_eq!(
            fmt_bytes_of(GIB + GIB / 4, 3 * GIB + 2 * GIB / 5),
            "1.2 of 3.4 GiB"
        );
        assert_eq!(fmt_bytes_of(0, 3 * GIB + 2 * GIB / 5), "0.0 of 3.4 GiB");
        assert_eq!(fmt_bytes_of(120 * MIB, 308 * MIB), "120 of 308 MiB");
        let t = 3 * GIB + 2 * GIB / 5;
        assert_eq!(fmt_bytes_of(t - 1, t), "3.3 of 3.4 GiB");
        assert_eq!(fmt_bytes_of(t, t), "3.4 of 3.4 GiB");
        assert_eq!(fmt_bytes_of(100, 300), "100 of 300 B");
        assert_eq!(fmt_bytes_so_far(GIB + GIB * 29 / 100), "1.2 GiB");
        assert_eq!(fmt_bytes_so_far(300), "300 B");
    }

    #[test]
    fn so_far_picks_its_unit_from_the_rounded_down_figure() {
        // 1023.97 MiB is not a GiB yet, and never "0.9 GiB".
        assert_eq!(fmt_bytes_so_far(GIB - 30_000), "1023 MiB");
        assert_eq!(fmt_bytes_so_far(GIB), "1.0 GiB");
        assert_eq!(fmt_bytes_so_far(KIB - 1), "1023 B");
        // 99.96 KiB so far is 99.9, not 100.
        assert_eq!(fmt_bytes_so_far(102_360), "99.9 KiB");
    }
}
