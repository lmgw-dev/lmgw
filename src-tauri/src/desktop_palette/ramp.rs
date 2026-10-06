//! The neutrals the page takes from the desktop's title-bar shade.
//!
//! The steps are the bundled palette's own (the app.css tokens), measured as
//! OKLCH lightness above or below its --bg0, re-based on the title bar's
//! shade and kept in its hue and chroma: the panes become lighter greys of
//! that shade, where the bundled ones get bluer as they get lighter.
//! Re-measure them when the bundled neutrals change.
//!
//! Plain `#rrggbb` values, not CSS relative colours: codeblocks.js hands
//! these tokens to mermaid and reads them back as numbers.

use super::Rgb;

/// (token without its `--`, step in the dark theme, step in the light one).
const STEPS: &[(&str, f64, f64)] = &[
    ("bg1", 0.03, 0.03),
    ("surface", 0.06, 0.055),
    ("surface-hi", 0.095, 0.065),
    ("border", 0.13, -0.055),
    ("border-strong", 0.205, -0.145),
    ("grid", 0.11, -0.007),
    ("seq0", 0.08, 0.025),
    ("text-3", 0.34, -0.285),
    ("text-2", 0.495, -0.44),
];

/// Every stepped neutral, by token name.
pub(super) fn neutrals(base: Rgb, dark: bool) -> Vec<(&'static str, String)> {
    let [l, c, h] = to_oklch(base);
    STEPS
        .iter()
        .map(|&(name, on_dark, on_light)| {
            let step = if dark { on_dark } else { on_light };
            (name, hex(from_oklch([l + step, c, h])))
        })
        .collect()
}

pub(super) fn hex([r, g, b]: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Dark when white text would read better on it than black: relative
/// luminance below 0.179, where the two contrasts are equal.
pub(super) fn is_dark([r, g, b]: Rgb) -> bool {
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b) < 0.179
}

fn to_linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn to_srgb(x: f64) -> u8 {
    let x = x.clamp(0.0, 1.0);
    let v = if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round() as u8
}

/// Lightness, chroma, hue (radians). Björn Ottosson's OKLab matrices.
fn to_oklch([r, g, b]: Rgb) -> [f64; 3] {
    let (r, g, b) = (to_linear(r), to_linear(g), to_linear(b));
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    let lightness = 0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s;
    let a = 1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s;
    let b = 0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s;
    [lightness, a.hypot(b), b.atan2(a)]
}

fn from_oklch([lightness, c, h]: [f64; 3]) -> Rgb {
    let lightness = lightness.clamp(0.0, 1.0);
    let (a, b) = (c * h.cos(), c * h.sin());
    let l = (lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
    let m = (lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
    let s = (lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b).powi(3);
    [
        to_srgb(4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s),
        to_srgb(-1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s),
        to_srgb(-0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: &str) -> Rgb {
        let byte = |i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
        [byte(1), byte(3), byte(5)]
    }

    fn near(a: Rgb, b: Rgb) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 1)
    }

    #[test]
    fn a_colour_survives_the_round_trip() {
        for c in [
            [41, 44, 48],
            [222, 224, 226],
            [61, 174, 233],
            [0, 0, 0],
            [255, 255, 255],
        ] {
            assert!(near(from_oklch(to_oklch(c)), c), "{c:?}");
        }
    }

    const APP_CSS: &str = include_str!("../../../crates/lmgw-ui/assets/app.css");

    /// A token's `#RRGGBB` in the first block that `opens` starts.
    fn bundled(opens: &str, name: &str) -> Rgb {
        let block = &APP_CSS[APP_CSS.find(opens).unwrap()..];
        let block = &block[..block.find('}').unwrap()];
        let at = block.find(&format!("--{name}: #")).unwrap() + name.len() + 4;
        rgb(&block[at..at + 7])
    }

    #[test]
    fn the_steps_rebuild_the_bundled_palette_from_its_own_base() {
        // Re-based on the bundled --bg0, every step lands on its bundled
        // token's lightness (their chroma, bluer, is what differs).
        for (opens, dark) in [(":root {", true), ("[data-theme=\"light\"] {", false)] {
            let got = neutrals(bundled(opens, "bg0"), dark);
            for (name, value) in got {
                let l = |c: Rgb| to_oklch(c)[0];
                let want = bundled(opens, name);
                let diff = (l(rgb(&value)) - l(want)).abs();
                assert!(diff < 0.006, "{opens} --{name}: {value} vs {}", hex(want));
            }
        }
    }

    #[test]
    fn breeze_dark_steps_lighter_in_the_same_shade() {
        let base = [41, 44, 48];
        let got: std::collections::HashMap<_, _> = neutrals(base, true).into_iter().collect();
        let bg1 = rgb(&got["bg1"]);
        assert!(near(bg1, [48, 52, 56]), "{bg1:?}");
        let [_, c0, h0] = to_oklch(base);
        let [_, c1, h1] = to_oklch(bg1);
        assert!(
            (c1 - c0).abs() < 0.002 && (h1 - h0).abs() < 0.15,
            "{c0} {h0} → {c1} {h1}"
        );
    }

    #[test]
    fn darkness_follows_luminance() {
        assert!(is_dark([41, 44, 48]));
        assert!(!is_dark([222, 224, 226]));
    }
}
