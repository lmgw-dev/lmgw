//! The CSD frame on GTK 3 (Wayland): GTK's own client-side decorations
//! around an empty titlebar, themed by a CSS provider scoped to the window.
//!
//! Why GTK's and not an undecorated, transparent window that draws its own
//! shadow: with a titlebar widget set, GTK keeps everything a frame needs
//! besides its look: it draws the shadow outside the content, keeps the
//! resize edges, limits the input region to the content and those edges so
//! clicks in the shadow reach the window below, sends
//! `xdg_surface.set_window_geometry` without the shadow (KWin snaps, tiles
//! and maximizes the content), and drops the shadow when the window is
//! maximized. An empty, zero-height titlebar instead of tao's GtkHeaderBar
//! (or `None`, which makes GTK ask KWin to decorate) leaves the whole content
//! to the webview.
//!
//! **The resize band is the decoration's margin, not its shadow.** GTK 3
//! (gtkwindow.c, `update_border_windows`) makes the resize edges as deep as
//! the decoration's margin plus its border and padding, and sets the input
//! region to the content grown by that much on every side; the shadow only
//! sizes the surface (`get_shadow_width`: the larger of shadow and margin),
//! and the corners reach `decoration-resize-handle` (20 px) further along
//! each edge. So [`RESIZE_BAND`] is set here rather than left to the theme,
//! and dropped on the edges that do not resize (tiled, maximized,
//! fullscreen), where the margin would otherwise leave a strip that takes
//! clicks without resizing. Measured 2026-11-22 (GTK 3.24.52, KWin 6.8.3,
//! Breeze GTK theme, a headless `kwin_wayland --virtual` with two outputs and
//! fake pointer input, the cursor shape and input region read from
//! `WAYLAND_DEBUG`): the resize cursor shows from 1 to 10 px outside every
//! edge that resizes, focused and unfocused alike, corners included, and
//! beyond that the pointer is the window below's. Before this module set the
//! margin it was the same 10 px, from Breeze's own `margin: 10px`, whatever
//! the shadow reached (85/67/103 px focused, 51/41/61 px unfocused before
//! 2566cd88); but a tiled edge kept a 10 px strip on the neighbouring
//! output that took clicks without resizing, gone now, and maximized the
//! input region is the content exactly.
//!
//! **Corners are the page's.** The webview's own background is transparent,
//! and so is the window's, but the page has to round itself: WebKitGTK
//! (2.54.2, DMABuf renderer, NVIDIA) draws its frames past any clip the
//! shell sets on the webview's `draw`, a rounded path and a rectangle list
//! alike (tried 2026-11-15), so a page that paints its whole viewport shows
//! square corners outside the rounded outline. The dashboard and the design
//! sample round their own windows; the fallback strip rounds a page that has
//! no header yet (`page.js`).

use std::cell::{Cell, RefCell};

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use tauri::{Manager, WebviewWindow};
use webkit2gtk::WebViewExt;

use super::{parse_layout, radius_ok, Action, State, DEFAULT_RADIUS};

/// The style class the provider's rules are scoped to: the provider is the
/// screen's, so a file dialog or any other GTK window keeps its theme.
const CLASS: &str = "lmgw-csd";

/// The frame's state on the GTK main thread.
struct Chrome {
    app: tauri::AppHandle,
    provider: gtk::CssProvider,
    radius: Cell<f64>,
    /// The latest button press on the webview: the drag, the window menu
    /// and the compositor need its serial, time and position.
    press: RefCell<Option<gdk::Event>>,
    /// The webview, for its zoom level.
    view: RefCell<glib::WeakRef<webkit2gtk::WebView>>,
}

thread_local! {
    static CHROME: RefCell<Option<std::rc::Rc<Chrome>>> = const { RefCell::new(None) };
}

fn chrome() -> Option<std::rc::Rc<Chrome>> {
    CHROME.with(|c| c.borrow().clone())
}

/// How far outside the drawn edge a drag resizes the window, in px: KWin's
/// invisible resize border around a Breeze-decorated window. Breeze
/// recommends no borders, and KWin 6.8.3 takes that recommendation unless
/// `BorderSize` is set, so Breeze extends each edge by
/// `DecorationSettings::largeSpacing` (breezedecoration.cpp,
/// `recalculateBorders`), the height of an M in the title font rounded up
/// to even (kdecoration 6.8.3): 10 px with Plasma's Noto Sans 10 pt at
/// scale 1. KWin's corners reach `2 * largeSpacing` along each edge, which
/// GTK's default `decoration-resize-handle` (20 px) already matches.
const RESIZE_BAND: u32 = 10;

/// The mockup's window shadow with its hairline outline, dark or light, and
/// a lighter one for an unfocused window. GTK 3 blurs with the blur radius
/// as the standard deviation, where browsers take half of it (CSS
/// Backgrounds 3, `box-shadow`), so the mockup's blur radii are halved here:
/// `0 18px 60px` in the page is `0 18px 30px` in GTK. Halving also halves
/// the surface GTK adds around the content for the shadow (85 px instead of
/// 169 px on each side at 60 px).
///
/// The shadow decides how large the surface is, not where a drag resizes:
/// the input region ends at the decoration's margin ([`RESIZE_BAND`]), so a
/// press or the pointer in the shadow beyond it goes to the window below.
///
/// **Both shadows reach equally far.** GTK sizes the window's surface by the
/// shadow's extents (the content sits that far in), so a backdrop shadow
/// that reaches less far changes the surface on every focus change. GTK
/// applies that in steps, across the shadow's transition: each step first
/// allocates the content within the old surface less the new shadow, then
/// resizes the surface, so the webview went 68 px narrower and shorter and
/// back on focus-in (a 1440 px wide window at 125 % dropped below the
/// sidebar's rail breakpoint for a frame) and a few px larger and back on
/// every frame of a focus-out, and the page laid itself out again each time
/// (GTK 3.24.52, KWin 6.8.3, 2026-11-22). So the unfocused list keeps the
/// active one's widest layer, transparent, at the same place in the list:
/// the extents stay the same, and so do they at every step of the
/// transition between the two lists, which GTK interpolates layer by layer
/// (Adwaita's own backdrop shadow does the same).
fn shadows(dark: bool) -> (&'static str, &'static str, &'static str) {
    if dark {
        (
            "0 0 0 1px rgba(0,0,0,0.75), 0 18px 30px rgba(0,0,0,0.55), 0 4px 7px rgba(0,0,0,0.35)",
            "0 0 0 1px rgba(0,0,0,0.6), 0 18px 30px transparent, 0 10px 18px rgba(0,0,0,0.4), \
             0 2px 4px rgba(0,0,0,0.25)",
            "0 0 0 1px rgba(0,0,0,0.75)",
        )
    } else {
        (
            "0 0 0 1px rgba(0,0,0,0.14), 0 18px 30px rgba(20,30,40,0.22), 0 4px 7px rgba(20,30,40,0.10)",
            "0 0 0 1px rgba(0,0,0,0.12), 0 18px 30px transparent, 0 10px 18px rgba(20,30,40,0.14), \
             0 2px 4px rgba(20,30,40,0.08)",
            "0 0 0 1px rgba(0,0,0,0.14)",
        )
    }
}

/// The window's rules: a transparent window background (the page draws the
/// content, the corners stay clear), a zero-height titlebar, the
/// decoration's radius and shadow; no radius and no shadow maximized or
/// fullscreen; tiled, the hairline only.
///
/// The decoration's margin is the resize band ([`RESIZE_BAND`], the module
/// doc says why), the same focused and unfocused (`:backdrop` changes only
/// the shadow). An edge that does not resize has none: a tiled edge (GTK
/// adds `tiled-top` and the like for the edges the compositor tiled, and
/// only `tiled` when it says nothing per edge, which GTK reads as all four),
/// and every edge maximized or fullscreen.
fn css(radius: f64, dark: bool) -> String {
    let (active, backdrop, hairline) = shadows(dark);
    let w = format!("window.{CLASS}");
    format!(
        "{w}, {w}.background {{ background-color: transparent; background-image: none; }}\n\
         {w} .lmgw-titlebar {{ min-height: 0; min-width: 0; padding: 0; margin: 0; border: none; \
         background: none; box-shadow: none; }}\n\
         {w} decoration {{ border-radius: {radius}px; border: none; padding: 0; \
         margin: {RESIZE_BAND}px; box-shadow: {active}; }}\n\
         {w}:backdrop decoration {{ box-shadow: {backdrop}; }}\n\
         {w}.tiled decoration, {w}.tiled-top decoration, {w}.tiled-right decoration, \
         {w}.tiled-bottom decoration, {w}.tiled-left decoration {{ border-radius: 0; \
         box-shadow: {hairline}; }}\n\
         {w}.tiled-top decoration {{ margin-top: 0; }}\n\
         {w}.tiled-right decoration {{ margin-right: 0; }}\n\
         {w}.tiled-bottom decoration {{ margin-bottom: 0; }}\n\
         {w}.tiled-left decoration {{ margin-left: 0; }}\n\
         {w}.tiled:not(.tiled-top):not(.tiled-right):not(.tiled-bottom):not(.tiled-left) \
         decoration {{ margin: 0; }}\n\
         {w}.maximized decoration, {w}.fullscreen decoration {{ border-radius: 0; \
         box-shadow: none; margin: 0; }}\n"
    )
}

fn dark() -> bool {
    gtk::Settings::default().is_some_and(|s| s.is_gtk_application_prefer_dark_theme())
}

/// Sets the window up for the CSD frame. Call before it is first shown.
pub(super) fn decorate(window: &WebviewWindow) {
    let gtk_window = match window.gtk_window() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("CSD frame: no GTK window ({e}); the window keeps tao's header bar");
            return;
        }
    };
    install(window.app_handle());

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    bar.style_context().add_class("lmgw-titlebar");
    bar.show();
    gtk_window.set_titlebar(Some(&bar));
    gtk_window.style_context().add_class(CLASS);

    // The content's size, shadow excluded (`content_size`).
    gtk_window.connect_size_allocate(|w, _| {
        let (width, height) = w.size();
        super::set_content_size(tauri::LogicalSize::new(width.into(), height.into()));
    });
    // Every change of maximized, tiled, fullscreen and focus goes to the
    // page.
    gtk_window.connect_window_state_event(|_, _| {
        push();
        glib::Propagation::Proceed
    });

    let result = window.with_webview(move |platform| {
        let view = platform.inner();
        // The page's transparent parts show through to the window, whose
        // background the provider makes transparent: the corners.
        view.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
        if let Some(c) = chrome() {
            c.view.borrow().set(Some(&view));
        }
        // The page's radius is in its CSS pixels, which the zoom scales.
        view.connect_zoom_level_notify(|_| push());
        view.connect_button_press_event(|_, event| {
            if event.event_type() == gdk::EventType::ButtonPress {
                if let Some(c) = chrome() {
                    *c.press.borrow_mut() = Some((**event).clone());
                }
            }
            glib::Propagation::Proceed
        });
    });
    if let Err(e) = result {
        tracing::warn!("CSD frame: the webview could not be set up: {e}");
    }
}

/// The provider (once per process) and the frame's state.
fn install(app: &tauri::AppHandle) -> std::rc::Rc<Chrome> {
    if let Some(c) = chrome() {
        return c;
    }
    let provider = gtk::CssProvider::new();
    let radius = DEFAULT_RADIUS;
    if let Err(e) = provider.load_from_data(css(radius, dark()).as_bytes()) {
        tracing::warn!("CSD frame: the window's CSS was refused: {e}");
    }
    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
        );
    }
    if let Some(settings) = gtk::Settings::default() {
        // KDE writes its button order and colour scheme through to GTK.
        settings.connect_gtk_decoration_layout_notify(|_| push());
        settings.connect_gtk_application_prefer_dark_theme_notify(|_| reload_css());
    }
    let chrome = std::rc::Rc::new(Chrome {
        app: app.clone(),
        provider,
        radius: Cell::new(radius),
        press: RefCell::new(None),
        view: RefCell::new(glib::WeakRef::new()),
    });
    CHROME.with(|c| *c.borrow_mut() = Some(chrome.clone()));
    chrome
}

fn reload_css() {
    if let Some(c) = chrome() {
        if let Err(e) = c
            .provider
            .load_from_data(css(c.radius.get(), dark()).as_bytes())
        {
            tracing::warn!("CSD frame: the window's CSS was refused: {e}");
        }
    }
}

const TILED: gdk::WindowState = gdk::WindowState::TILED
    .union(gdk::WindowState::TOP_TILED)
    .union(gdk::WindowState::RIGHT_TILED)
    .union(gdk::WindowState::BOTTOM_TILED)
    .union(gdk::WindowState::LEFT_TILED);

/// The main window's state as the page is told it.
fn state() -> Option<State> {
    let c = chrome()?;
    let window = c.app.get_webview_window("main")?;
    let gtk_window = window.gtk_window().ok()?;
    // A window still hidden (reveal.rs) has no GDK window yet: the page asks
    // at document start and must get an answer before the window shows.
    let flags = gtk_window.window().map_or_else(
        || {
            if gtk_window.is_maximized() {
                gdk::WindowState::MAXIMIZED
            } else {
                gdk::WindowState::empty()
            }
        },
        |w| w.state(),
    );
    let layout = gtk::Settings::default()
        .and_then(|s| s.gtk_decoration_layout())
        .map(|l| parse_layout(&l))
        .unwrap_or_default();
    let zoom = c.view.borrow().upgrade().map_or(1.0, |v| v.zoom_level());
    Some(State {
        csd: true,
        maximized: flags.contains(gdk::WindowState::MAXIMIZED),
        fullscreen: flags.contains(gdk::WindowState::FULLSCREEN),
        tiled: flags.intersects(TILED),
        inactive: !flags.contains(gdk::WindowState::FOCUSED),
        radius: c.radius.get(),
        zoom,
        layout,
    })
}

/// Hands the current state to the page.
fn push() {
    let Some(state) = state() else { return };
    tracing::debug!("window state to the page: {}", state.apply_js());
    if let Some(window) = chrome().and_then(|c| c.app.get_webview_window("main")) {
        let _ = window.eval(state.apply_js());
    }
}

pub(super) fn act(window: &WebviewWindow, action: Action) -> Result<Option<State>, String> {
    let gtk_window = window.gtk_window().map_err(|e| e.to_string())?;
    let chrome = chrome().ok_or("the window has no CSD frame")?;
    match action {
        Action::State => return Ok(state()),
        Action::Minimize => gtk_window.iconify(),
        Action::Maximize => toggle_maximize(&gtk_window),
        // GTK's delete-event: tao turns it into CloseRequested, and main's
        // handler hides the window to the tray.
        Action::Close => gtk_window.close(),
        Action::Drag => {
            let press = chrome
                .press
                .borrow()
                .clone()
                .ok_or("no press to drag with")?;
            let toplevel = gtk_window.window().ok_or("the window is not realized")?;
            let (x, y) = press.root_coords().unwrap_or((0.0, 0.0));
            let button = press.button().unwrap_or(1);
            toplevel.begin_move_drag(button as i32, x as i32, y as i32, press.time());
        }
        Action::Click { button, count } => {
            let settings = gtk::Settings::default().ok_or("no GTK settings")?;
            let what = match (button, count) {
                (1, 2) => settings.gtk_titlebar_double_click(),
                (2, 1) => settings.gtk_titlebar_middle_click(),
                (3, 1) => settings.gtk_titlebar_right_click(),
                _ => None,
            };
            match what.as_deref() {
                Some(
                    "toggle-maximize"
                    | "toggle-maximize-horizontally"
                    | "toggle-maximize-vertically",
                ) => toggle_maximize(&gtk_window),
                Some("minimize") => gtk_window.iconify(),
                Some("lower") => {
                    if let Some(w) = gtk_window.window() {
                        w.lower();
                    }
                }
                Some("menu") => window_menu(&gtk_window, &chrome)?,
                _ => {}
            }
        }
        Action::Menu => window_menu(&gtk_window, &chrome)?,
        Action::Radius { px } => {
            if !radius_ok(px) {
                return Err(format!("radius {px} is outside 0 to 64 px"));
            }
            chrome.radius.set(px);
            reload_css();
            gtk_window.queue_draw();
            push();
        }
    }
    Ok(None)
}

fn toggle_maximize(w: &gtk::ApplicationWindow) {
    if w.is_maximized() {
        w.unmaximize();
    } else {
        w.maximize();
    }
}

/// The compositor's window menu at the latest press.
///
/// `gdk_window_show_window_menu` takes the position relative to the content
/// (the window geometry's origin) and adds the shadow itself before it sends
/// `xdg_toplevel.show_window_menu`, whose coordinates are surface-local
/// (GTK 3.24.52 and KWin 6.8.2, 2026-11-15: a press at content 400,15 went
/// out as 485,82 with an 85,67 shadow, and the menu opened at the pointer;
/// handing GTK surface coordinates put it the shadow's width off). So the
/// position is the press's root coordinates less the toplevel's root origin
/// and the webview's place in the window.
fn window_menu(gtk_window: &gtk::ApplicationWindow, chrome: &Chrome) -> Result<(), String> {
    let mut press = chrome
        .press
        .borrow()
        .clone()
        .ok_or("no press for the menu")?;
    let toplevel = gtk_window.window().ok_or("the window is not realized")?;
    let (rx, ry) = press.root_coords().ok_or("the press has no position")?;
    let (_, ox, oy) = toplevel.origin();
    let (cx, cy) = chrome
        .view
        .borrow()
        .upgrade()
        .and_then(|v| v.translate_coordinates(gtk_window, 0, 0))
        .unwrap_or((0, 0));
    let x = rx - f64::from(ox) - f64::from(cx);
    let y = ry - f64::from(oy) - f64::from(cy);
    tracing::debug!(
        "window menu at {x},{y} on the content (press {:?}, content at {cx},{cy})",
        press.coords()
    );
    // SAFETY: `press` is our own copy of a button event (only presses are
    // stored); x and y are plain fields of it.
    unsafe {
        let raw = press.as_ptr();
        (*raw).button.x = x;
        (*raw).button.y = y;
    }
    if toplevel.show_window_menu(&mut press) {
        Ok(())
    } else {
        Err("the compositor has no window menu".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_are_scoped_to_the_window_and_drop_the_frame_when_maximized() {
        let css = css(17.5, true);
        for line in css.lines() {
            assert!(line.starts_with("window.lmgw-csd"), "{line}");
        }
        assert!(css.contains("decoration { border-radius: 17.5px;"), "{css}");
        assert!(css.contains(
            "window.lmgw-csd.maximized decoration, window.lmgw-csd.fullscreen decoration \
             { border-radius: 0; box-shadow: none; margin: 0; }"
        ));
    }

    /// The decoration's margin (top, right, bottom, left) and box-shadow for
    /// a window with these style classes besides [`CLASS`], focused or not
    /// (`:backdrop`): the rules of `css` whose selector matches, applied in
    /// specificity and then source order, as GTK 3 cascades them. Knows the
    /// selectors `css` writes: `window` with classes, `:backdrop` and
    /// `:not(.class)`, then ` decoration`.
    fn decoration(css: &str, classes: &[&str], backdrop: bool) -> ([f64; 4], String) {
        let mut hits = Vec::new();
        for (order, line) in css.lines().enumerate() {
            let (selectors, body) = line.split_once(" {").expect(line);
            for selector in selectors.split(", ") {
                let Some(compound) = selector.strip_suffix(" decoration") else {
                    continue;
                };
                let mut rest = compound.strip_prefix("window").expect(selector);
                let (mut matches, mut specificity) = (true, 0);
                while !rest.is_empty() {
                    if let Some(r) = rest.strip_prefix(":backdrop") {
                        matches &= backdrop;
                        rest = r;
                    } else if let Some(r) = rest.strip_prefix(":not(.") {
                        let (name, r) = r.split_once(')').expect(selector);
                        matches &= !classes.contains(&name);
                        rest = r;
                    } else if let Some(r) = rest.strip_prefix('.') {
                        let end = r.find(['.', ':']).unwrap_or(r.len());
                        let name = &r[..end];
                        matches &= name == CLASS || classes.contains(&name);
                        rest = &r[end..];
                    } else {
                        panic!("unknown selector {selector}");
                    }
                    specificity += 1;
                }
                if matches {
                    hits.push((specificity, order, body.trim_end_matches('}')));
                }
            }
        }
        hits.sort_by_key(|&(specificity, order, _)| (specificity, order));
        let px = |v: &str| -> f64 { v.trim_end_matches("px").parse().expect(v) };
        let (mut margin, mut shadow) = ([0.0; 4], String::from("none"));
        for (_, _, body) in hits {
            for decl in body.split(';').map(str::trim).filter(|d| !d.is_empty()) {
                let (property, value) = decl.split_once(": ").expect(decl);
                match property {
                    "margin" => margin = [px(value); 4],
                    "margin-top" => margin[0] = px(value),
                    "margin-right" => margin[1] = px(value),
                    "margin-bottom" => margin[2] = px(value),
                    "margin-left" => margin[3] = px(value),
                    "box-shadow" => shadow = value.to_string(),
                    _ => {}
                }
            }
        }
        (margin, shadow)
    }

    /// GTK 3 (gtkwindow.c, `update_border_windows`) makes the resize edges
    /// as deep as the decoration's margin (border and padding are 0 here)
    /// and grows the input region by it, so the margin is the resize band;
    /// the surface reaches the larger of shadow and margin on each side
    /// (`get_shadow_width`), and nothing past the content maximized or
    /// fullscreen.
    #[test]
    fn the_resize_band_is_kwins_on_the_edges_that_resize_focused_or_not() {
        let b = f64::from(RESIZE_BAND);
        let states: [(&[&str], [f64; 4]); 8] = [
            (&[], [b; 4]),
            // KWin's quick tiles: a quarter resizes inwards on two edges, a
            // half on one.
            (&["tiled", "tiled-top", "tiled-left"], [0.0, b, b, 0.0]),
            (
                &["tiled", "tiled-top", "tiled-bottom", "tiled-left"],
                [0.0, b, 0.0, 0.0],
            ),
            (
                &["tiled", "tiled-top", "tiled-right", "tiled-bottom"],
                [0.0, 0.0, 0.0, b],
            ),
            (
                &[
                    "tiled",
                    "tiled-top",
                    "tiled-right",
                    "tiled-bottom",
                    "tiled-left",
                ],
                [0.0; 4],
            ),
            // A compositor that says only "tiled": GTK resizes no edge.
            (&["tiled"], [0.0; 4]),
            (&["maximized"], [0.0; 4]),
            (&["fullscreen"], [0.0; 4]),
        ];
        for dark in [true, false] {
            let css = css(17.5, dark);
            for (classes, band) in states {
                let surface = |backdrop| {
                    let (margin, shadow) = decoration(&css, classes, backdrop);
                    assert_eq!(margin, band, "{classes:?}, backdrop {backdrop}");
                    let shadow = if shadow == "none" {
                        [0.0; 4]
                    } else {
                        extents(&layers(&shadow))
                    };
                    [0, 1, 2, 3].map(|i| shadow[i].max(margin[i]))
                };
                assert_eq!(
                    surface(false),
                    surface(true),
                    "dark {dark}, {classes:?}: a focus change would resize the surface"
                );
            }
        }
    }

    /// A shadow list's layers as `[x, y, blur, spread]`.
    fn layers(list: &str) -> Vec<[f64; 4]> {
        let mut parts = vec![String::new()];
        let mut depth = 0;
        for ch in list.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(String::new());
                    continue;
                }
                _ => {}
            }
            parts.last_mut().unwrap().push(ch);
        }
        parts
            .iter()
            .map(|layer| {
                let lengths: Vec<f64> = layer
                    .split_whitespace()
                    .filter_map(|t| t.trim_end_matches("px").parse().ok())
                    .collect();
                assert!((2..=4).contains(&lengths.len()), "{layer}");
                let at = |i: usize| lengths.get(i).copied().unwrap_or(0.0);
                [at(0), at(1), at(2), at(3)]
            })
            .collect()
    }

    /// How far GTK 3 extends the window's surface for a shadow list: top,
    /// right, bottom, left (gtkcssshadowvalue.c,
    /// `_gtk_css_shadow_value_get_extents`, the blur's reach as
    /// `_gtk_cairo_blur_compute_pixels` has it), the largest layer per side.
    fn extents(layers: &[[f64; 4]]) -> [f64; 4] {
        let scale = 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0;
        layers.iter().fold([0.0; 4], |acc, [x, y, blur, spread]| {
            let reach = (blur * scale * 1.5 + 0.5).floor() + spread;
            let side = [reach - y, reach + x, reach + y, reach - x].map(|v: f64| v.ceil().max(0.0));
            [0, 1, 2, 3].map(|i| acc[i].max(side[i]))
        })
    }

    /// GTK 3's transition between two shadow lists, at `t` in 0..=1: layer by
    /// layer, the shorter list padded with zero-sized layers
    /// (gtkcssshadowsvalue.c).
    fn between(from: &[[f64; 4]], to: &[[f64; 4]], t: f64) -> Vec<[f64; 4]> {
        let none = [0.0; 4];
        (0..from.len().max(to.len()))
            .map(|i| {
                let (a, b) = (from.get(i).unwrap_or(&none), to.get(i).unwrap_or(&none));
                [0, 1, 2, 3].map(|k| a[k] + (b[k] - a[k]) * t)
            })
            .collect()
    }

    #[test]
    fn a_focus_change_keeps_the_shadow_extents_and_so_the_window_size() {
        for dark in [true, false] {
            let (active, backdrop, _) = shadows(dark);
            let (active, backdrop) = (layers(active), layers(backdrop));
            let want = extents(&active);
            // The mockup's shadow at its 125 %: 85 px at the sides, less
            // above and more below for the 18 px drop.
            assert_eq!(want, [67.0, 85.0, 103.0, 85.0], "dark {dark}");
            for step in 0..=20 {
                let t = f64::from(step) / 20.0;
                for (from, to) in [(&active, &backdrop), (&backdrop, &active)] {
                    assert_eq!(
                        extents(&between(from, to, t)),
                        want,
                        "dark {dark}, step {step}: the surface would change size"
                    );
                }
            }
        }
    }
}
