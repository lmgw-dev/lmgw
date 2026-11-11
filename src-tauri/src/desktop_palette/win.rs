//! The Windows theme for the app window: Windows 11's own neutrals and the
//! user's accent colour, in the system's dark or light mode.
//!
//! The shell reads the mode and the accent from WinRT's `UISettings`, the
//! documented API for both (no rights needed, nothing installed): the mode
//! as Microsoft's own "support dark and light themes in Win32 apps" reads
//! it, from the foreground colour (light text is the dark mode); the accent
//! with the shades Windows derives from it for each mode. It hands the page
//! both themes' colours and the system's mode: the page shows the system's
//! when lmgw's theme setting is "system" (the default here) and the forced
//! one otherwise, in Windows' colours either way, since unlike KDE's title
//! bar shade neither set depends on the mode Windows is in.
//!
//! The neutrals are the bundled palette's steps (ramp.rs) anchored on
//! Windows 11's base surface as the content background: WinUI's
//! `SolidBackgroundFillColorBase`, `#202020` dark and `#F3F3F3` light, what
//! Mica falls back to with transparency off and what Settings' pages are.
//! WebView2 has no Mica here, so the solid fallback is the surface. The
//! sidebar is the step below it and the cards the steps above, which lands
//! within a few levels of Windows' own: dark `#191919` (Explorer's panes),
//! cards `#272727` (Settings' `#2B2B2B`), borders `#393939` (Explorer's
//! `#383838`); light `#E9E9E9`, cards `#FBFBFB` (Settings' and WinUI's card
//! over the base). Measured on Windows 11 build 26100, 2026-11-11.
//!
//! The sidebar is the exception to "the step below the content": it is the
//! title bar's colour, so the two read as one L. Rather than guess DWM's
//! colour the shell sets it: `DWMWA_CAPTION_COLOR` (Windows 11, build 22000
//! and up) is given the sidebar's colour, the same for the active and the
//! inactive window, and `DWMWA_TEXT_COLOR` the black or white that reads on
//! it. That colour is the step below the content (the `#191919` and
//! `#E9E9E9` above), or, with "Show accent colour on title bars and window
//! borders" on (`HKCU\Software\Microsoft\Windows\DWM\ColorPrevalence`), the
//! accent DWM would paint (`DWM\AccentColor`); the sidebar's text and icons
//! then take the colours that read on it, and the panes stay as they are.
//! Where the attribute is not there (Windows 10) the sidebar is anchored on
//! the caption colour DWM paints by default for the mode ([`DEFAULT_DARK`],
//! [`DEFAULT_LIGHT`]) or the accent, and the log says once which path was
//! taken.
//!
//! The accent fills are WinUI's `AccentFillColorDefault` for the mode, the
//! shade Windows paints its own accent buttons and selection marks in:
//! `AccentLight2` in dark, `AccentDark1` in light. A shade whose contrast
//! with the palette's text on accent (`--accent-ink`) is under 4.5:1 gives
//! way to the next one further from it; the hover is the bundled palette's
//! own step away from that text.

use super::{ramp, Rgb};

/// Windows 11's base surface, the content background (WinUI 3's
/// `SolidBackgroundFillColorBase`, Common_themeresources_any.xaml).
const CONTENT_DARK: Rgb = [0x20, 0x20, 0x20];
const CONTENT_LIGHT: Rgb = [0xf3, 0xf3, 0xf3];

/// The caption colour DWM paints by default where `DWMWA_CAPTION_COLOR` is
/// not there (Windows 10): black-ish in the dark mode, white in the light.
/// Not measured on a Windows 10 machine yet.
const DEFAULT_DARK: Rgb = [0x20, 0x20, 0x20];
const DEFAULT_LIGHT: Rgb = [0xff, 0xff, 0xff];

/// The caption text on a dark or a light caption.
const CAPTION_TEXT_ON_DARK: Rgb = [0xff, 0xff, 0xff];
const CAPTION_TEXT_ON_LIGHT: Rgb = [0x00, 0x00, 0x00];

/// The bundled palette's text on accent fills (kit.css `--accent-ink`).
const INK_DARK: Rgb = [0x10, 0x16, 0x1c];
const INK_LIGHT: Rgb = [0xff, 0xff, 0xff];

/// The least contrast of text on an accent fill (WCAG AA, body text).
const MIN_CONTRAST: f64 = 4.5;

/// How much lighter (dark theme) or darker (light theme) the hover is than
/// the accent, in OKLCH lightness: the bundled `--accent-hi` against
/// `--accent` (a test re-measures them in kit.css).
const HOVER_DARK: f64 = 0.048;
const HOVER_LIGHT: f64 = 0.065;

/// The accent's shades as `UISettings::GetColorValue` gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Accent {
    /// `AccentDark1` to `AccentDark3`.
    pub dark: [Rgb; 3],
    pub accent: Rgb,
    /// `AccentLight1` to `AccentLight3`.
    pub light: [Rgb; 3],
}

/// One reading of the system's colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Theme {
    /// The system's mode for apps.
    pub dark: bool,
    pub accent: Accent,
    /// The accent DWM paints the title bar in, when "Show accent colour on
    /// title bars and window borders" is on.
    pub title: Option<Rgb>,
    /// Whether `DWMWA_CAPTION_COLOR` exists (Windows 11, build 22000+).
    pub caption: bool,
}

/// The sidebar's colour for one mode: the title bar's.
pub(super) fn sidebar(t: Theme, dark: bool) -> Rgb {
    if let Some(c) = t.title {
        return c;
    }
    if t.caption {
        return ramp::base_under(content(dark), dark);
    }
    if dark {
        DEFAULT_DARK
    } else {
        DEFAULT_LIGHT
    }
}

/// The caption text that reads on a caption colour.
pub(super) fn caption_text(caption: Rgb) -> Rgb {
    if ramp::contrast(caption, CAPTION_TEXT_ON_DARK)
        >= ramp::contrast(caption, CAPTION_TEXT_ON_LIGHT)
    {
        CAPTION_TEXT_ON_DARK
    } else {
        CAPTION_TEXT_ON_LIGHT
    }
}

fn content(dark: bool) -> Rgb {
    if dark {
        CONTENT_DARK
    } else {
        CONTENT_LIGHT
    }
}

/// What the page's Windows hook (`HOOK`) reads.
pub(super) fn page(t: Theme) -> serde_json::Value {
    serde_json::json!({
        "system": if t.dark { "dark" } else { "light" },
        "themes": { "dark": colours(t, true), "light": colours(t, false) },
    })
}

/// One theme's `--desk-<name>` values.
fn colours(t: Theme, dark: bool) -> serde_json::Map<String, serde_json::Value> {
    let accent = t.accent;
    let base = ramp::base_under(content(dark), dark);
    let side = sidebar(t, dark);
    let mut c = serde_json::Map::new();
    c.insert("bg0".into(), ramp::hex(base).into());
    // The sidebar is the title bar's colour, active or not (the caption
    // colour is set the same for both).
    c.insert("sidebar".into(), ramp::hex(side).into());
    c.insert("title-inactive".into(), ramp::hex(side).into());
    if side != base {
        for (name, value) in sidebar_inks(side) {
            c.insert(name.into(), value.into());
        }
    }
    for (name, value) in ramp::neutrals(base, dark) {
        c.insert(name.into(), value.into());
    }
    let a = accent_tokens(accent, dark);
    c.insert("accent".into(), ramp::hex(a.fill).into());
    c.insert("accent-hi".into(), ramp::hex(a.hover).into());
    c.insert("accent-ink".into(), ramp::hex(a.ink).into());
    let [r, g, b] = a.fill;
    let alpha = if dark { ".13" } else { ".10" }; // the bundled --accent-bg's
    c.insert(
        "accent-bg".into(),
        format!("rgba({r},{g},{b},{alpha})").into(),
    );
    let (sel, sel_ink) = selection(accent.accent);
    c.insert("selection".into(), ramp::hex(sel).into());
    c.insert("selection-ink".into(), ramp::hex(sel_ink).into());
    c
}

/// The sidebar's text, icons, hover and lines on a colour that is not the
/// pane base: the caption text's colour, mixed toward the sidebar for the
/// quieter ones. The page's `desk-side` class puts them in place.
fn sidebar_inks(side: Rgb) -> Vec<(&'static str, String)> {
    let ink = caption_text(side);
    let mix = |t: f64| {
        let m = |i: usize| (f64::from(ink[i]) * t + f64::from(side[i]) * (1.0 - t)).round() as u8;
        ramp::hex([m(0), m(1), m(2)])
    };
    let [r, g, b] = ink;
    vec![
        ("sidebar-ink", ramp::hex(ink)),
        ("sidebar-ink-2", mix(0.78)),
        ("sidebar-ink-3", mix(0.62)),
        ("sidebar-hover", format!("rgba({r},{g},{b},.14)")),
        ("sidebar-line", format!("rgba({r},{g},{b},.28)")),
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct AccentTokens {
    fill: Rgb,
    hover: Rgb,
    ink: Rgb,
}

/// The accent fill, its hover and the text on both for one theme.
fn accent_tokens(a: Accent, dark: bool) -> AccentTokens {
    // WinUI's fill for the mode first, then the shades further from the
    // theme's text on accent.
    let shades: [Rgb; 3] = if dark {
        [a.light[1], a.light[2], a.light[0]]
    } else {
        [a.dark[0], a.dark[1], a.dark[2]]
    };
    let ink = if dark { INK_DARK } else { INK_LIGHT };
    let (fill, ink) = shades
        .iter()
        .find(|&&s| ramp::contrast(s, ink) >= MIN_CONTRAST)
        .map(|&s| (s, ink))
        // No shade reads with the theme's text on it: the pair, of every
        // shade with the dark or the white text, that reads best.
        .unwrap_or_else(|| {
            let mut best = (shades[0], ink, 0.0);
            for &s in &shades {
                for i in [INK_DARK, INK_LIGHT] {
                    let c = ramp::contrast(s, i);
                    if c > best.2 {
                        best = (s, i, c);
                    }
                }
            }
            (best.0, best.1)
        });
    // The hover moves away from the text, so its contrast only grows.
    let step = if dark { HOVER_DARK } else { HOVER_LIGHT };
    let hover = ramp::shift(fill, if ramp::is_dark(ink) { step } else { -step });
    AccentTokens { fill, hover, ink }
}

/// Selected text: the accent itself, under the dark or the white text,
/// whichever reads better on it.
fn selection(accent: Rgb) -> (Rgb, Rgb) {
    let ink = if ramp::contrast(accent, INK_DARK) >= ramp::contrast(accent, INK_LIGHT) {
        INK_DARK
    } else {
        INK_LIGHT
    };
    (accent, ink)
}

/// The page's end on Windows, after the shared hook it wraps: picks the
/// theme (the stored setting, else the system's), marks the page `desk-win`
/// and `data-system-theme` (Settings offers "system" by it), hands the shared
/// hook that theme's colours, and tells the shell the theme for the window's
/// frame (`THEME_EVENT`). The page's own theme switch is followed through
/// `data-theme`. At document start there may be no `<html>` yet; the hook
/// waits for it.
pub(super) const HOOK: &str = r#"(function () {
  const paint = window.__lmgwDesktop;
  let last = null, told = null;
  function stored() {
    try { return localStorage.getItem("lmgw-theme"); } catch (_) { return null; }
  }
  function apply() {
    const el = document.documentElement;
    if (!el || !last) return;
    const w = last.desktop;
    if (!w) { paint(last); return; }
    const s = stored();
    const mode = s === "dark" || s === "light" ? s : w.system;
    el.classList.add("desk-win");
    el.dataset.systemTheme = w.system;
    if (el.dataset.theme !== mode) el.dataset.theme = mode;
    const colors = w.themes[mode];
    paint({ desktop: { dark: mode === "dark", colors: colors }, focused: last.focused });
    el.classList.toggle("desk-side", !!colors["sidebar-ink"]);
    const api = window.__TAURI_INTERNALS__;
    if (api && told !== mode) {
      told = mode;
      api.invoke("plugin:event|emit", { event: "lmgw:theme", payload: mode }).catch(() => { told = null; });
    }
  }
  function watch() {
    new MutationObserver(apply).observe(document.documentElement,
      { attributes: true, attributeFilter: ["data-theme"] });
    apply();
  }
  window.__lmgwDesktop = function (p) { last = p; apply(); };
  if (document.documentElement) watch();
  else new MutationObserver(function (_, o) {
    if (document.documentElement) { o.disconnect(); watch(); }
  }).observe(document, { childList: true });
})();"#;

/// What the page emits with the theme it shows.
#[cfg(windows)]
const THEME_EVENT: &str = "lmgw:theme";

#[cfg(windows)]
pub(super) use system::{follow, read};

#[cfg(windows)]
mod system {
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex, Once, PoisonError};

    use tauri::{Listener, Runtime, Theme as Frame, WebviewWindow, WindowEvent};
    use windows::core::{w, PCWSTR};
    use windows::Foundation::TypedEventHandler;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
    };
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };
    use windows::UI::ViewManagement::{UIColorType, UISettings};

    use super::super::Desktop;
    use super::{caption_text, sidebar, Accent, Rgb, Theme, THEME_EVENT};

    /// The first Windows 11 build, where `DWMWA_CAPTION_COLOR` appeared.
    const CAPTION_COLOR_BUILD: u32 = 22000;

    fn dword(root: HKEY, key: PCWSTR, name: PCWSTR) -> Option<u32> {
        let (mut v, mut n) = (0u32, 4u32);
        // SAFETY: `v` is the 4 bytes `n` says.
        let err = unsafe {
            RegGetValueW(
                root,
                key,
                name,
                RRF_RT_REG_DWORD,
                None,
                Some((&mut v as *mut u32).cast::<c_void>()),
                Some(&mut n),
            )
        };
        err.is_ok().then_some(v)
    }

    fn build_number() -> Option<u32> {
        let mut buf = [0u16; 16];
        let mut n = std::mem::size_of_val(&buf) as u32;
        // SAFETY: `buf` is the `n` bytes it says.
        let err = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
                w!("CurrentBuildNumber"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                Some(&mut n),
            )
        };
        if err.is_err() {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len]).trim().parse().ok()
    }

    /// The accent DWM paints title bars and window borders in, when the
    /// user asked for it ("Show accent colour on title bars and window
    /// borders"): `DWM\AccentColor` is 0xAABBGGRR.
    fn title_accent() -> Option<Rgb> {
        let key = w!(r"Software\Microsoft\Windows\DWM");
        if dword(HKEY_CURRENT_USER, key, w!("ColorPrevalence"))? != 1 {
            return None;
        }
        let abgr = dword(HKEY_CURRENT_USER, key, w!("AccentColor"))?;
        Some([abgr as u8, (abgr >> 8) as u8, (abgr >> 16) as u8])
    }

    /// The system's colours now; None (the bundled palette) when WinRT does
    /// not answer, which the log says.
    pub(in super::super) fn read() -> Option<Theme> {
        let got = UISettings::new().and_then(|s| read_from(&s));
        if let Ok(t) = &got {
            log_path(t);
        }
        got.inspect_err(|e| tracing::warn!("the Windows theme could not be read: {e}"))
            .ok()
    }

    /// Which way the sidebar is matched to the title bar, said once.
    fn log_path(t: &Theme) {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let how = if t.caption {
                "the shell sets the caption colour (DWMWA_CAPTION_COLOR)"
            } else {
                "no DWMWA_CAPTION_COLOR (before Windows 11): the sidebar takes DWM's default caption colour"
            };
            let accent = if t.title.is_some() {
                "the accent colour on title bars"
            } else {
                "the neutral title bar"
            };
            tracing::info!("title bar and sidebar: {how}, {accent}");
        });
    }

    fn read_from(s: &UISettings) -> windows::core::Result<Theme> {
        let c = |t| s.GetColorValue(t).map(|c| [c.R, c.G, c.B]);
        let accent_now = c(UIColorType::Accent)?;
        Ok(Theme {
            dark: !super::ramp::is_dark(c(UIColorType::Foreground)?),
            title: title_accent(),
            caption: build_number().is_some_and(|b| b >= CAPTION_COLOR_BUILD),
            accent: Accent {
                dark: [
                    c(UIColorType::AccentDark1)?,
                    c(UIColorType::AccentDark2)?,
                    c(UIColorType::AccentDark3)?,
                ],
                accent: accent_now,
                light: [
                    c(UIColorType::AccentLight1)?,
                    c(UIColorType::AccentLight2)?,
                    c(UIColorType::AccentLight3)?,
                ],
            },
        })
    }

    /// Gives DWM the sidebar's colour as the caption colour, and the text
    /// that reads on it. Set once per change: the colour is the same for the
    /// active and the inactive window.
    fn set_caption<R: Runtime>(window: &WebviewWindow<R>, desktop: &Desktop, dark: bool) {
        let Some(theme) = desktop.get() else { return };
        if !theme.caption {
            return;
        }
        let colour = sidebar(theme, dark);
        let colorref = |[r, g, b]: Rgb| u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16;
        let hwnd = match window.hwnd() {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("the caption colour needs the window handle: {e}");
                return;
            }
        };
        for (attr, value) in [
            (DWMWA_CAPTION_COLOR, colorref(colour)),
            (DWMWA_TEXT_COLOR, colorref(caption_text(colour))),
        ] {
            // SAFETY: a COLORREF is the 4 bytes the attribute takes.
            let set = unsafe {
                DwmSetWindowAttribute(hwnd, attr, (&value as *const u32).cast::<c_void>(), 4)
            };
            if let Err(e) = set {
                tracing::warn!("the caption colour could not be set: {e}");
                return;
            }
        }
    }

    /// Keeps the window in step with Windows: a mode or accent picked in
    /// Settings reaches the page at once (`ColorValuesChanged`, beside the
    /// shared re-read on focus-in), and the frame takes the theme the page
    /// shows (Tauri's window theme, which sets DWM's immersive dark mode).
    pub(in super::super) fn follow<R: Runtime>(window: &WebviewWindow<R>, desktop: Desktop) {
        // The mode the page shows, which the caption colour follows.
        let shown: Arc<Mutex<Option<bool>>> = Arc::default();
        let frame = window.clone();
        let (told, d) = (shown.clone(), desktop.clone());
        window.listen(THEME_EVENT, move |event| {
            let (theme, dark) = match event.payload() {
                r#""dark""# => (Frame::Dark, true),
                r#""light""# => (Frame::Light, false),
                _ => return,
            };
            if let Err(e) = frame.set_theme(Some(theme)) {
                tracing::warn!("the window frame's theme could not be set: {e}");
            }
            *told.lock().unwrap_or_else(PoisonError::into_inner) = Some(dark);
            set_caption(&frame, &d, dark);
        });
        // A window coming back re-reads the system, and DWM gets the
        // caption colour again (it costs nothing and a change of the accent
        // on title bars may not have raised ColorValuesChanged).
        let (back, d) = (window.clone(), desktop.clone());
        let told = shown.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::Focused(true) = event {
                d.reload();
                let dark = *told.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(dark) = dark {
                    set_caption(&back, &d, dark);
                }
            }
        });

        let settings = match UISettings::new() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Windows theme changes are not followed live: {e}");
                return;
            }
        };
        let page = window.clone();
        let handler = TypedEventHandler::new(move |_, _| {
            // Raised several times for one change, on a thread of WinRT's.
            if desktop.reload() {
                let focused = page.is_focused().unwrap_or(true);
                let _ = page.eval(desktop.apply_js(focused));
                let dark = *shown.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(dark) = dark {
                    set_caption(&page, &desktop, dark);
                }
            }
            Ok(())
        });
        let token = match settings.ColorValuesChanged(&handler) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("Windows theme changes are not followed live: {e}");
                return;
            }
        };
        // The subscription lives as long as the window.
        let held = Mutex::new(Some((settings, token)));
        window.on_window_event(move |event| {
            if let WindowEvent::Destroyed = event {
                let taken = held.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some((settings, token)) = taken {
                    let _ = settings.RemoveColorValuesChanged(token);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: &str) -> Rgb {
        let byte = |i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
        [byte(1), byte(3), byte(5)]
    }

    fn accent(shades: [&str; 7]) -> Accent {
        Accent {
            dark: [rgb(shades[2]), rgb(shades[1]), rgb(shades[0])],
            accent: rgb(shades[3]),
            light: [rgb(shades[4]), rgb(shades[5]), rgb(shades[6])],
        }
    }

    /// The accents as `UISettings` gave them on Windows 11 build 26100
    /// (2026-11-11), Dark3 to Light3: Settings' "Blue" swatch (Windows'
    /// default), "Gold" and "Purple".
    fn blue() -> Accent {
        accent([
            "#001A68", "#003E92", "#0067C0", "#0078D4", "#0091F8", "#4CC2FF", "#99EBFF",
        ])
    }
    fn gold() -> Accent {
        accent([
            "#651900", "#A14600", "#E37700", "#FF8C00", "#FF9910", "#FFB634", "#FFD155",
        ])
    }
    fn purple() -> Accent {
        accent([
            "#400E59", "#692782", "#8E3AA7", "#A94DC1", "#B763CB", "#DB9EE5", "#F0C0F4",
        ])
    }

    #[test]
    fn the_fills_are_winuis_for_the_mode() {
        for a in [blue(), purple()] {
            let d = accent_tokens(a, true);
            assert_eq!((d.fill, d.ink), (a.light[1], INK_DARK));
            let l = accent_tokens(a, false);
            assert_eq!((l.fill, l.ink), (a.dark[0], INK_LIGHT));
        }
    }

    #[test]
    fn a_shade_too_close_to_the_text_gives_way() {
        // Gold's Dark1 is too light for white text; Dark2 is not.
        let gold = gold();
        let t = accent_tokens(gold, false);
        assert!(ramp::contrast(gold.dark[0], INK_LIGHT) < MIN_CONTRAST);
        assert_eq!((t.fill, t.ink), (gold.dark[1], INK_LIGHT));
    }

    #[test]
    fn with_no_shade_left_the_text_flips() {
        // Every light shade too dark for the dark text: white reads better.
        let grim = accent([
            "#000000", "#000000", "#000000", "#101010", "#202020", "#303030", "#404040",
        ]);
        let t = accent_tokens(grim, true);
        assert_eq!(t.ink, INK_LIGHT);
        assert!(ramp::contrast(t.fill, t.ink) >= MIN_CONTRAST);
        // ...and the hover then darkens, away from it.
        assert!(ramp::contrast(t.hover, t.ink) >= ramp::contrast(t.fill, t.ink));
    }

    #[test]
    fn every_fill_and_hover_reads() {
        for a in [blue(), purple(), gold()] {
            for dark in [true, false] {
                let t = accent_tokens(a, dark);
                for c in [t.fill, t.hover] {
                    assert!(ramp::contrast(c, t.ink) >= MIN_CONTRAST, "{c:?} on {dark}");
                }
            }
        }
    }

    #[test]
    fn selection_takes_the_better_text() {
        assert_eq!(selection(rgb("#0078D4")).1, INK_LIGHT);
        assert_eq!(selection(gold().accent).1, INK_DARK);
    }

    fn theme(title: Option<Rgb>, caption: bool) -> Theme {
        Theme {
            dark: true,
            accent: blue(),
            title,
            caption,
        }
    }

    #[test]
    fn the_neutrals_stand_on_windows_base_surface() {
        let t = theme(None, true);
        let d = colours(t, true);
        assert_eq!(d["bg1"], "#202020");
        assert_eq!(d["bg0"], "#191919");
        // The sidebar is the title bar's colour: the step below the content.
        assert_eq!(d["sidebar"], "#191919");
        assert_eq!(d["title-inactive"], "#191919");
        assert!(d.get("sidebar-ink").is_none());
        assert_eq!(d["surface"], "#272727");
        assert_eq!(d["surface-hi"], "#303030");
        assert_eq!(d["border"], "#393939");
        assert_eq!(d["border-strong"], "#4d4d4d");
        let l = colours(t, false);
        assert_eq!(l["bg1"], "#f3f3f3");
        assert_eq!(l["bg0"], "#e9e9e9");
        assert_eq!(l["sidebar"], "#e9e9e9");
        assert_eq!(l["surface"], "#fbfbfb");
        assert_eq!(l["surface-hi"], "#ffffff");
        assert_eq!(l["border"], "#d7d7d7");
        assert_eq!(l["border-strong"], "#bababa");
        assert_eq!(l["accent"], "#0067c0");
        assert_eq!(l["accent-bg"], "rgba(0,103,192,.10)");
    }

    #[test]
    fn the_accent_on_title_bars_is_the_sidebar_and_its_inks_read() {
        let accent = rgb("#0078D4");
        let t = theme(Some(accent), true);
        for dark in [true, false] {
            let c = colours(t, dark);
            assert_eq!(c["sidebar"], "#0078d4");
            assert_eq!(c["title-inactive"], "#0078d4");
            // The panes stay as they are.
            let plain = colours(theme(None, true), dark);
            for k in ["bg0", "bg1", "surface", "border", "text-2", "accent"] {
                assert_eq!(c[k], plain[k], "{k}");
            }
            let ink = rgb(c["sidebar-ink"].as_str().unwrap());
            assert!(ramp::contrast(accent, ink) >= MIN_CONTRAST);
            let ink3 = rgb(c["sidebar-ink-3"].as_str().unwrap());
            assert!(ramp::contrast(accent, ink3) >= 3.0);
        }
        assert_eq!(sidebar(t, false), accent);
        // A light accent gets the dark caption text.
        assert_eq!(caption_text(rgb("#FFB900")), CAPTION_TEXT_ON_LIGHT);
        // The better of black and white: black on Windows' blue, white on a purple.
        assert_eq!(caption_text(accent), CAPTION_TEXT_ON_LIGHT);
        assert_eq!(caption_text(rgb("#680081")), CAPTION_TEXT_ON_DARK);
    }

    #[test]
    fn without_the_caption_attribute_the_sidebar_takes_dwms_default() {
        let t = theme(None, false);
        assert_eq!(sidebar(t, true), DEFAULT_DARK);
        assert_eq!(sidebar(t, false), DEFAULT_LIGHT);
        // The panes' own base stays; the sidebar differs, so it has inks.
        let c = colours(t, false);
        assert_eq!(c["bg0"], "#e9e9e9");
        assert_eq!(c["sidebar"], "#ffffff");
        assert!(c.contains_key("sidebar-ink"));
        // The accent still wins where the attribute is missing.
        assert_eq!(
            sidebar(theme(Some(rgb("#0078D4")), false), true),
            rgb("#0078D4")
        );
    }

    const KIT_CSS: &str = include_str!("../../../crates/lmgw-ui-kit/assets/kit.css");
    const APP_CSS: &str = include_str!("../../../crates/lmgw-ui/assets/app.css");

    /// A token's `#RRGGBB` in the first block that `opens` starts.
    fn bundled(opens: &str, name: &str) -> Rgb {
        let block = &KIT_CSS[KIT_CSS.find(opens).unwrap()..];
        let block = &block[..block.find('}').unwrap()];
        let at = block.find(&format!("--{name}: #")).unwrap() + name.len() + 4;
        rgb(&block[at..at + 7])
    }

    #[test]
    fn the_hover_steps_are_the_bundled_ones() {
        for (opens, dark, step) in [
            (":root {", true, HOVER_DARK),
            ("[data-theme=\"light\"] {", false, -HOVER_LIGHT),
        ] {
            // Their lightness apart; the hue and chroma are the accent's own.
            let (accent, hi) = (bundled(opens, "accent"), bundled(opens, "accent-hi"));
            let measured = ramp::lightness(hi) - ramp::lightness(accent);
            assert!(
                (measured - step).abs() < 0.002,
                "{dark}: {measured} vs {step}"
            );
        }
        // The inks are the bundled ones.
        assert_eq!(bundled(":root {", "accent-ink"), INK_DARK);
        assert_eq!(bundled("[data-theme=\"light\"] {", "accent-ink"), INK_LIGHT);
    }

    #[test]
    fn app_css_reads_every_colour_the_windows_palette_sets() {
        // The accent-on-title-bars palette has every key the neutral one has.
        let t = page(theme(Some(rgb("#0078D4")), true));
        for mode in ["dark", "light"] {
            for name in t["themes"][mode].as_object().unwrap().keys() {
                assert!(
                    APP_CSS.contains(&format!("var(--desk-{name})")),
                    "app.css does not read --desk-{name}"
                );
            }
        }
        // And every token it puts them in place of is one the kit defines.
        for token in [
            "bg0",
            "bg1",
            "surface",
            "surface-hi",
            "border",
            "border-strong",
            "grid",
            "seq0",
            "text-2",
            "text-3",
            "accent",
            "accent-hi",
            "accent-ink",
            "accent-bg",
        ] {
            for opens in [":root {", "[data-theme=\"light\"] {"] {
                let block = &KIT_CSS[KIT_CSS.find(opens).unwrap()..];
                let block = &block[..block.find('}').unwrap()];
                assert!(block.contains(&format!("--{token}:")), "{opens} --{token}");
                assert!(
                    APP_CSS.contains(&format!("--{token}: var(--desk-")),
                    "app.css maps no --{token}"
                );
            }
        }
    }
}
