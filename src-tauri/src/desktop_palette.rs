//! The desktop's own shade for the app window (KDE).
//!
//! KWin's Breeze decoration paints the title bar in the colour scheme's
//! header colour, and the web UI's sidebar starts right below it. The shell
//! reads that colour from kdeglobals and hands it to the page as the
//! sidebar's background, with the other neutrals stepped off it (ramp.rs);
//! app.css ("desktop palette") puts them in place of its own. The title bar
//! runs on into the sidebar, and the panes are lighter greys of the same
//! shade. A LAN browser, and any desktop but KDE, keeps the bundled palette.
//!
//! kdeglobals is read again each time the window gains focus, so a scheme
//! picked in System Settings shows when you come back to the window. The
//! focus is passed on too: an unfocused window's title bar takes the scheme's
//! inactive shade, and the sidebar follows it.

mod ramp;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tauri::{Runtime, WebviewWindow, WindowEvent};

type Rgb = [u8; 3];

/// The title bar's two shades, as KWin's decoration paints them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TitleBar {
    active: Rgb,
    inactive: Rgb,
}

/// The latest reading, shared by the window's focus handler and its page
/// loads.
#[derive(Clone, Default)]
pub struct Desktop(Arc<Mutex<Option<TitleBar>>>);

/// The page's end: sets the classes and custom properties app.css keys the
/// desktop palette on. Defined by the initialization script, so it exists
/// before the page's own scripts run.
const HOOK: &str = r#"window.__lmgwDesktop = function (p) {
  const el = document.documentElement;
  if (!el) return;
  const d = p.desktop;
  el.classList.toggle("desk-dark", !!d && d.dark);
  el.classList.toggle("desk-light", !!d && !d.dark);
  el.classList.toggle("desk-inactive", !p.focused);
  if (d) for (const k in d.colors) el.style.setProperty("--desk-" + k, d.colors[k]);
};"#;

impl Desktop {
    pub fn read() -> Self {
        Self(Arc::new(Mutex::new(read_title_bar())))
    }

    /// Runs at document start, before the first paint: the hook plus the
    /// reading the window was built with.
    pub fn init_script(&self) -> String {
        format!("{HOOK}\n{}", self.apply_js(true))
    }

    /// Hands the page the current reading and the window's focus.
    pub fn apply_js(&self, focused: bool) -> String {
        let p = serde_json::json!({ "desktop": self.get().map(page_colours), "focused": focused });
        format!("window.__lmgwDesktop && window.__lmgwDesktop({p});")
    }

    fn get(&self) -> Option<TitleBar> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reload(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = read_title_bar();
    }
}

/// Keeps the page in step with the window's focus, re-reading kdeglobals
/// whenever it comes back.
pub fn follow<R: Runtime>(window: &WebviewWindow<R>, desktop: Desktop) {
    let page = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Focused(focused) = event {
            if *focused {
                desktop.reload();
            }
            let _ = page.eval(desktop.apply_js(*focused));
        }
    });
}

/// What app.css reads, as `--desk-<name>`: the sidebar's two shades and the
/// stepped neutrals, plus which of its themes they belong to.
fn page_colours(t: TitleBar) -> serde_json::Value {
    let dark = ramp::is_dark(t.active);
    let mut colours = serde_json::Map::new();
    colours.insert("bg0".into(), ramp::hex(t.active).into());
    colours.insert("title-inactive".into(), ramp::hex(t.inactive).into());
    for (name, value) in ramp::neutrals(t.active, dark) {
        colours.insert(name.into(), value.into());
    }
    serde_json::json!({ "dark": dark, "colors": colours })
}

/// None off KDE: only KWin paints the title bar from kdeglobals.
fn read_title_bar() -> Option<TitleBar> {
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
    if !kde {
        return None;
    }
    let mut scheme = Scheme::default();
    for file in kdeglobals_files() {
        if let Ok(text) = std::fs::read_to_string(file) {
            scheme.scan(&text);
        }
    }
    scheme.title_bar()
}

/// kdeglobals as KConfig cascades it: the system files, least important
/// first, then the user's, whose keys win.
fn kdeglobals_files() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_CONFIG_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    let mut files: Vec<PathBuf> = dirs
        .split(':')
        .rev()
        .map(|d| PathBuf::from(d).join("kdeglobals"))
        .collect();
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(home) = home {
        files.push(home.join("kdeglobals"));
    }
    files
}

/// The few keys the title bar's colour comes from.
#[derive(Default)]
struct Scheme {
    header: Option<Rgb>,
    header_inactive: Option<Rgb>,
    wm_active: Option<Rgb>,
    wm_inactive: Option<Rgb>,
}

impl Scheme {
    /// One file; a later file's keys replace an earlier one's.
    fn scan(&mut self, text: &str) {
        let mut group = "";
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                group = line;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let slot = match (group, key.trim()) {
                ("[Colors:Header]", "BackgroundNormal") => &mut self.header,
                ("[Colors:Header][Inactive]", "BackgroundNormal") => &mut self.header_inactive,
                ("[WM]", "activeBackground") => &mut self.wm_active,
                ("[WM]", "inactiveBackground") => &mut self.wm_inactive,
                _ => continue,
            };
            if let Some(rgb) = parse_rgb(value.trim()) {
                *slot = Some(rgb);
            }
        }
    }

    /// As KDecoration's palette picks them: the header colours when the
    /// scheme has them (every scheme since Plasma 5.21), the [WM] keys
    /// otherwise.
    fn title_bar(&self) -> Option<TitleBar> {
        let (active, inactive) = match self.header {
            Some(active) => (active, self.header_inactive),
            None => (self.wm_active?, self.wm_inactive),
        };
        Some(TitleBar {
            active,
            inactive: inactive.unwrap_or(active),
        })
    }
}

/// KConfig's colour forms: `r,g,b` (alpha after it ignored) or `#rrggbb`.
fn parse_rgb(value: &str) -> Option<Rgb> {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
        return Some([byte(0)?, byte(2)?, byte(4)?]);
    }
    let mut parts = value.split(',').map(|p| p.trim().parse::<u8>().ok());
    Some([parts.next()??, parts.next()??, parts.next()??])
}

#[cfg(test)]
mod tests {
    use super::*;

    const BREEZE_DARK: &str = "\
[Colors:Header]
BackgroundAlternate=32,35,38
BackgroundNormal=41,44,48

[Colors:Header][Inactive]
BackgroundNormal=32,35,38

[WM]
activeBackground=39,44,49
inactiveBackground=32,36,40
";

    fn scheme(files: &[&str]) -> Option<TitleBar> {
        let mut s = Scheme::default();
        for f in files {
            s.scan(f);
        }
        s.title_bar()
    }

    #[test]
    fn header_colours_win_over_wm_keys() {
        assert_eq!(
            scheme(&[BREEZE_DARK]),
            Some(TitleBar {
                active: [41, 44, 48],
                inactive: [32, 35, 38]
            })
        );
    }

    #[test]
    fn a_scheme_without_header_colours_uses_wm() {
        let old = "[WM]\nactiveBackground=48,174,232\ninactiveBackground=#eff0f1\n";
        assert_eq!(
            scheme(&[old]),
            Some(TitleBar {
                active: [48, 174, 232],
                inactive: [0xef, 0xf0, 0xf1]
            })
        );
    }

    #[test]
    fn the_user_file_overrides_the_system_file() {
        let user = "[Colors:Header]\nBackgroundNormal=1,2,3\n";
        let t = scheme(&[BREEZE_DARK, user]).unwrap();
        assert_eq!(t.active, [1, 2, 3]);
        // A key the user file does not set stays the system one's.
        assert_eq!(t.inactive, [32, 35, 38]);
    }

    #[test]
    fn a_missing_inactive_shade_is_the_active_one() {
        let t = scheme(&["[Colors:Header]\nBackgroundNormal=41,44,48,255\n"]).unwrap();
        assert_eq!(t.inactive, [41, 44, 48]);
    }

    #[test]
    fn no_colours_no_palette() {
        assert_eq!(
            scheme(&["[KDE]\nLookAndFeelPackage=org.kde.breezedark.desktop\n"]),
            None
        );
        assert_eq!(
            scheme(&["[Colors:Header]\nBackgroundNormal=nonsense\n"]),
            None
        );
    }

    #[test]
    fn the_page_gets_css_colours() {
        let d = Desktop(Arc::new(Mutex::new(scheme(&[BREEZE_DARK]))));
        let js = d.apply_js(false);
        assert!(js.contains(r##""bg0":"#292c30""##), "{js}");
        assert!(js.contains(r##""title-inactive":"#202326""##), "{js}");
        assert!(js.contains(r#""dark":true"#), "{js}");
        assert!(js.contains(r#""focused":false"#), "{js}");
        assert!(Desktop::default()
            .apply_js(true)
            .contains(r#""desktop":null"#));
    }
}
