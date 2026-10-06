//! lmgw Tauri v2 shell (§12): system tray + a window showing the local web
//! UI; the Axum gateway runs in-process and tray actions call core functions
//! directly.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio_out;
#[cfg(target_os = "linux")]
mod context_menu;
mod desktop_palette;
mod dev_guard;
mod gateway;
#[cfg(target_os = "linux")]
mod media;
#[cfg(target_os = "linux")]
mod reveal;
mod updater;

use std::net::SocketAddr;

use gateway::Gateway;
use lmgw_core::state::{default_data_dir, AppState, SharedState};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn bind_addr(state: &SharedState) -> SocketAddr {
    state
        .snapshot()
        .settings
        .bind_addr
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 8787)))
}

/// Where a **newly created** window — or the tray's *Open in Browser* — opens:
/// the login route, carrying a nonce this process just minted (principals
/// design §3.4), on the address this process is bound to (`addr`, from
/// [`Gateway::status`]).
///
/// Loopback for a wildcard bind, as it has always been, and the bound address
/// otherwise ([`gateway::base_url`]); the nonce is single-use, so the
/// webview's first history entry is a dead credential rather than the durable
/// key. The gateway runs in this same process, so the nonce is minted through
/// the managed [`SharedState`] and never travels anywhere to be issued. Every
/// caller mints its own at the moment it navigates: the 60 s life means a URL
/// held anywhere — a menu item built at startup — is already spent.
fn login_url(state: &SharedState, addr: &SocketAddr) -> String {
    let nonce = lmgw_core::web::session::mint_login_nonce(state);
    format!(
        "{}/api/session/login?nonce={nonce}",
        gateway::base_url(*addr)
    )
}

/// Show the window, creating it if it is not there yet.
///
/// Takes no URL: a window that already exists holds the session cookie and is
/// simply raised, and a window that has to be built gets a **fresh** nonce
/// (§3.4) — one per creation, which is what makes it single-use. The state it
/// mints from is the one `setup` managed, so every caller (the tray, the
/// second launch, the first start) reaches it the same way. A window is built
/// only on the address this process holds; while it holds none, the reason
/// is shown instead.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let gw: tauri::State<Gateway> = app.state();
    let addr = match gw.status() {
        Ok(addr) => addr,
        Err(why) => return gateway::not_serving(app, &why),
    };
    let state: tauri::State<SharedState> = app.state();
    let url = login_url(&state, &addr);
    // Every judgement of the window's document reads what this process
    // serves at that moment, not the origin the window was built for.
    let serving = gw.serving();
    #[cfg(target_os = "linux")]
    let media_serving = serving.clone();
    gw.set_window_origin(Some(gateway::origin_of(addr)));
    let shared: SharedState = (*state).clone();
    let webview_dir = dev_guard::webview_data_dir(&state.data_dir, cfg!(debug_assertions));
    // The colour scheme's title-bar shade, which the sidebar takes up.
    let desktop = desktop_palette::Desktop::read();
    let mut builder = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External(url.parse().expect("valid local url")),
    )
    .title("lmgw")
    .inner_size(1440.0, 960.0)
    // The UI links out to the backend containers' own web UIs
    // (target="_blank"). Inside the shell those must go to the system
    // browser — the window has no browser chrome, so a navigation away from
    // the gateway UI would strand the user with no back button.
    .on_new_window(|url, _features| {
        // Only web and mail links may leave for the system browser; a
        // file:, data:, javascript: or custom-scheme URL is dropped.
        if new_window_may_open(&url) {
            if let Err(e) = xdg_open(url.as_str()) {
                tracing::warn!("xdg-open {url}: {e}");
            }
        }
        tauri::webview::NewWindowResponse::Deny
    })
    // Every navigation, top level or iframe (the HTML preview), that is not
    // the gateway's own origin is denied silently: nothing here ever opens a
    // browser, so a frame cannot be used to launch one.
    .on_navigation(move |url| {
        let Some(origin) = serving.origin() else {
            return false;
        };
        let settings = shared.snapshot().settings.clone();
        navigation_allowed(url, &origin, &|h| {
            lmgw_core::agents::service::origin_label(&settings, h).is_some()
        })
    })
    // Exports (and any other `Content-Disposition: attachment`): WebKitGTK
    // only saves a file when a download handler decides its destination —
    // without one the download is cancelled and the click does nothing.
    .on_download(handle_download)
    // Before the first paint, and again after every load: a reload keeps
    // the reading and focus of the moment, not the window's first ones.
    .initialization_script(desktop.init_script())
    .on_page_load({
        let desktop = desktop.clone();
        move |window, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                let focused = window.is_focused().unwrap_or(true);
                let _ = window.eval(desktop.apply_js(focused));
            }
        }
    })
    // The desktop draws the frame: KWin's title bar, shadow, outline and
    // resize borders, like every other window. An undecorated window had none
    // of them and read as a picture pasted onto the screen.
    .decorations(true);
    // Hidden until the frame is settled (below): GTK picks client- or
    // server-side decorations when the window is realized, which showing it
    // does. And then until the app has drawn itself (reveal.rs).
    #[cfg(target_os = "linux")]
    {
        builder = builder.visible(false);
    }
    if let Some(dir) = webview_dir {
        builder = builder.data_directory(dir);
    }
    match builder.build() {
        // The microphone for the voice features (chat-voice §13.1).
        #[cfg(target_os = "linux")]
        Ok(window) => {
            // tao gives every Wayland window its own GtkHeaderBar, and a
            // window with a custom titlebar is client-decorated: GTK draws
            // the title bar and the shadow itself, in its theme's sizes. With
            // no titlebar GTK asks the compositor to decorate (KDE's
            // server-decoration protocol), so KWin draws the same frame as
            // on every other window: its title bar, buttons, window menu.
            if let Ok(gtk_window) = window.gtk_window() {
                use gtk::prelude::GtkWindowExt;
                gtk_window.set_titlebar(None::<&gtk::Widget>);
            }
            desktop_palette::follow(&window, desktop);
            context_menu::install(&window);
            reveal::when_mounted(&window);
            media::install(&window, media_serving)
        }
        #[cfg(not(target_os = "linux"))]
        Ok(window) => desktop_palette::follow(&window, desktop),
        Err(e) => {
            gw.set_window_origin(None);
            tracing::error!("failed to create window: {e}");
        }
    }
}

/// Hand a URL to the desktop. The browser it may start is not lmgw's audio,
/// so it does not inherit lmgw's stream identity (chat-voice §12.3).
fn xdg_open(url: &str) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url);
    audio_out::scrub_identity_env(&mut cmd);
    cmd.spawn()
}

/// The origin the window was opened with: scheme, host and port.
#[derive(Clone, Debug, PartialEq)]
struct WindowOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl WindowOrigin {
    fn of(url: &tauri::Url) -> Self {
        Self {
            scheme: url.scheme().to_string(),
            host: url.host_str().unwrap_or_default().to_ascii_lowercase(),
            port: url.port_or_known_default(),
        }
    }

    fn same_scheme_and_port(&self, url: &tauri::Url) -> bool {
        url.scheme() == self.scheme
            && url.port_or_known_default() == self.port
            // `http://127.0.0.1:8001@evil/` has the host `evil`; a URL that
            // carries userinfo at all is never the gateway.
            && url.username().is_empty()
            && url.password().is_none()
    }
}

/// Whether the window may navigate to `url`: only the gateway's own origin
/// (scheme, host and port as opened), an agent's app origin on the same port
/// (`<id>.<agent_origin_suffix>`, told by `is_agent_host`), `about:blank`, and
/// `blob:` URLs minted by one of those origins. Everything else — other hosts,
/// other ports, `file:`, `javascript:`, top-level `data:`, custom schemes — is
/// refused.
fn navigation_allowed(
    url: &tauri::Url,
    origin: &WindowOrigin,
    is_agent_host: &dyn Fn(&str) -> bool,
) -> bool {
    let host_ok = |u: &tauri::Url| {
        let h = u.host_str().unwrap_or_default().to_ascii_lowercase();
        origin.same_scheme_and_port(u) && (h == origin.host || is_agent_host(&h))
    };
    match url.scheme() {
        // `about:srcdoc` is the HTML preview's own iframe: inert, its content
        // is the page's own attribute.
        "about" => matches!(url.as_str(), "about:blank" | "about:srcdoc"),
        "blob" => url.as_str()["blob:".len()..]
            .parse::<tauri::Url>()
            .map(|inner| inner.scheme() != "blob" && host_ok(&inner))
            .unwrap_or(false),
        _ => origin.same_scheme_and_port(url) && host_ok(url),
    }
}

/// `target=_blank` links: web and mail only.
fn new_window_may_open(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "mailto")
}

/// The window's download handler: save into the XDG Downloads folder, never
/// over an existing file, and tell the page where it went (or that it did
/// not) so the UI can toast it — a saved file otherwise leaves no trace in an
/// undecorated window.
///
/// The folder is decided by [`download_destination`], not taken from wry's
/// proposal (which is the working directory on a host with no
/// `XDG_DOWNLOAD_DIR`), and a taken name steps to `<name> (1).<ext>`.
fn handle_download(webview: tauri::Webview, event: tauri::webview::DownloadEvent<'_>) -> bool {
    use tauri::webview::DownloadEvent;
    match event {
        DownloadEvent::Requested { destination, .. } => match download_destination(destination) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("download refused: {e}");
                notify_page(
                    &webview,
                    "lmgw-download-failed",
                    "error",
                    &format!("no folder to save into: {e}"),
                );
                false
            }
        },
        DownloadEvent::Finished { path, success, url } => {
            match (success, path) {
                (true, Some(p)) => {
                    tracing::info!("download saved: {}", p.display());
                    notify_page(&webview, "lmgw-downloaded", "path", &p.to_string_lossy());
                }
                _ => notify_page(
                    &webview,
                    "lmgw-download-failed",
                    "error",
                    &format!("{url} did not finish"),
                ),
            }
            true
        }
        _ => true,
    }
}

/// Make `dest` (the file wry proposes) a path that can be written and does
/// not exist, in the Downloads folder ([`download_target`]), whose folder is
/// created when missing.
fn download_destination(dest: &mut std::path::PathBuf) -> std::io::Result<()> {
    let env = DownloadEnv::from_process();
    *dest = download_target(dest, &env)?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    unique_path(dest);
    Ok(())
}

/// What [`download_target`] reads from the process: `$HOME`,
/// `$XDG_CONFIG_HOME` and the contents of `user-dirs.dirs` under it.
struct DownloadEnv {
    home: Option<std::path::PathBuf>,
    user_dirs: String,
}

impl DownloadEnv {
    fn from_process() -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            // The XDG spec: a relative value is invalid and ignored.
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let user_dirs = config
            .and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok())
            .unwrap_or_default();
        Self { home, user_dirs }
    }
}

/// Where a download proposed as `proposed` is saved: its **file name only**
/// (a name carrying path components — `../x`, an absolute path — keeps just
/// its last part) in the Downloads folder: `XDG_DOWNLOAD_DIR` from
/// `user-dirs.dirs`, else `~/Downloads` (created when missing).
///
/// wry proposes `dirs::download_dir()/<name>`, and `dirs` reads only
/// `user-dirs.dirs`: without an `XDG_DOWNLOAD_DIR` there it proposes the
/// working directory, even when `~/Downloads` exists (review R1 finding 7 —
/// the old check redirected only when neither existed, so the file landed in
/// the CWD). The folder is therefore always decided here, never taken from
/// the proposal.
fn download_target(
    proposed: &std::path::Path,
    env: &DownloadEnv,
) -> std::io::Result<std::path::PathBuf> {
    let name = match proposed.components().next_back() {
        Some(std::path::Component::Normal(n)) => n.to_os_string(),
        _ => std::ffi::OsString::from("download"),
    };
    let home = env.home.as_deref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no HOME and no Downloads folder",
        )
    })?;
    let dir = xdg_download_dir(&env.user_dirs, home).unwrap_or_else(|| home.join("Downloads"));
    Ok(dir.join(name))
}

/// `XDG_DOWNLOAD_DIR` as `xdg-user-dirs` writes it into `user-dirs.dirs`
/// (`"$HOME/…"` or an absolute path); `None` when it is not set there, or
/// set to something that is neither.
fn xdg_download_dir(user_dirs: &str, home: &std::path::Path) -> Option<std::path::PathBuf> {
    user_dirs.lines().find_map(|l| {
        let v = l
            .trim()
            .strip_prefix("XDG_DOWNLOAD_DIR=")?
            .trim()
            .trim_matches('"');
        let p = match v.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => std::path::PathBuf::from(v),
        };
        p.is_absolute().then_some(p)
    })
}

/// `name.ext` → `name (1).ext`, `name (2).ext`, … until nothing is there.
fn unique_path(p: &mut std::path::PathBuf) {
    if !p.exists() {
        return;
    }
    let file = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match file.split_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (file.clone(), String::new()),
    };
    let mut n = 1;
    while p.exists() {
        p.set_file_name(format!("{stem} ({n}){ext}"));
        n += 1;
    }
}

/// Fire `event` on the page's `window` with `{key: value}` as its detail.
fn notify_page(webview: &tauri::Webview, event: &str, key: &str, value: &str) {
    let detail = serde_json::json!({ key: value });
    let js = format!(
        "window.dispatchEvent(new CustomEvent({}, {{detail: {}}}));",
        serde_json::json!(event),
        detail
    );
    if let Err(e) = webview.eval(js) {
        tracing::warn!("telling the page about a download: {e}");
    }
}

/// Push the GPU-hold state to every tray surface it drives: the checkbox,
/// the icon (`tray.png` ↔ `tray-hold.png`) and the tooltip (§6). Shared by
/// the menu handler — so a click flips the tray immediately — and the 5 s
/// status loop, which calls it only when the value changed so a toggle from
/// the dashboard or MCP reaches the tray within one tick without re-setting
/// the icon every 5 s.
fn sync_hold(app: &tauri::AppHandle, hold_item: &CheckMenuItem<tauri::Wry>, active: bool) {
    let _ = hold_item.set_checked(active);
    let Some(tray) = app.tray_by_id("lmgw-tray") else {
        return;
    };
    let state: tauri::State<SharedState> = app.state();
    let icon_bytes: &[u8] = if active {
        include_bytes!("../icons/tray-hold.png").as_slice()
    } else {
        include_bytes!("../icons/tray.png").as_slice()
    };
    match tauri::image::Image::from_bytes(icon_bytes) {
        Ok(img) => {
            let _ = tray.set_icon(Some(img));
        }
        Err(e) => tracing::error!("tray icon decode: {e}"),
    }
    let tooltip = dev_guard::tray_tooltip(&state.data_dir, cfg!(debug_assertions), active);
    let _ = tray.set_tooltip(Some(tooltip));
}

/// True when this host has NVIDIA's proprietary driver loaded.
///
/// The character device is the driver actually being *there*; the module
/// directory covers a driver loaded but with no device node created yet (no X
/// session has touched it). Neither costs more than a `stat`, and both are
/// absent on an AMD or Intel box — which is the whole point, see
/// [`render_workaround`].
fn nvidia_driver_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists()
        || std::path::Path::new("/sys/module/nvidia").exists()
}

/// The environment variables that pick WebKitGTK's render path. Any explicit
/// value of one of them means the user decided, and lmgw leaves all of them alone.
const RENDER_VARS: [&str; 3] = [
    "WEBKIT_DISABLE_DMABUF_RENDERER",
    "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
    "__NV_DISABLE_EXPLICIT_SYNC",
];

/// Which WebKitGTK workaround applies (§12). `None` means leave WebKit alone.
///
/// On NVIDIA + Wayland, WebKitGTK's DMABUF renderer dies at startup with
/// `Gdk-Message: Error 71 (Protocol error) dispatching to Wayland display`
/// unless the process opts out of the driver's explicit sync, so the app sets
/// `__NV_DISABLE_EXPLICIT_SYNC=1` before GTK initializes. That keeps the
/// zero-copy hardware renderer. The alternatives cost real frames (measured
/// 2026-09-27, RTX 4090, driver 615.71, WebKitGTK 2.52.5, Plasma Wayland):
/// `WEBKIT_DISABLE_DMABUF_RENDERER=1` drops to software compositing (21 fps at
/// 1600×1000, 4 fps at 5000×1400, against 60 at both), and
/// `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1` reads the GPU back every frame (26 fps
/// at 5000×1400). `GDK_BACKEND=x11` is *not* a fix either: it routes through
/// XWayland and mis-renders on NVIDIA.
///
/// Set only where the bug is: AMD and Intel need nothing. An explicit value of
/// any of [`RENDER_VARS`] always wins, so `WEBKIT_DISABLE_DMABUF_RENDERER=1 lmgw`
/// stays the escape hatch for a host where the hardware path misbehaves.
fn render_workaround() -> Option<&'static str> {
    if RENDER_VARS.iter().any(|v| std::env::var_os(v).is_some()) {
        return None;
    }
    nvidia_driver_present().then_some("__NV_DISABLE_EXPLICIT_SYNC")
}

/// The shell's own commands (chat-voice §13.2), behind the origin check in
/// `main`'s invoke handler.
fn shell_commands() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![audio_out::audio_outputs, audio_out::audio_output_set]
}

fn main() {
    // Must happen before GTK/WebKit initializes, so before anything else —
    // including the logger, which is why the decision is only reported below.
    let workaround = render_workaround();
    if let Some(var) = workaround {
        std::env::set_var(var, "1");
    }
    // Before WebKit spawns its processes too, which inherit it: the name
    // WirePlumber remembers lmgw's audio routing under (chat-voice §12.3).
    let identity = audio_out::apply_identity_env();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lmgw=info,lmgw_core=info".into()),
        )
        .init();

    if let Some(var) = workaround {
        tracing::info!("NVIDIA driver detected — {var}=1 set for the webview");
    } else {
        tracing::debug!(
            "no NVIDIA driver, or a WebKit render variable is already set — leaving the \
             render path alone; set WEBKIT_DISABLE_DMABUF_RENDERER=1 if the window fails to open"
        );
    }
    let set: Vec<&str> = identity.iter().map(|(k, _)| *k).collect();
    tracing::debug!("audio stream identity: set {set:?} (an explicit value is left alone)");

    let context = tauri::generate_context!();
    let debug_build = cfg!(debug_assertions);
    let mut single_instance =
        tauri_plugin_single_instance::Builder::new().callback(|app, _args, _cwd| {
            // Second launch: focus the existing instance.
            show_main_window(app);
        });
    // Empty reads as unset (review n7): an empty path would open the
    // databases in the working directory.
    let env_data_dir = lmgw_core::state::data_dir_from_env();
    if let Some(id) = dev_guard::single_instance_id(
        &context.config().identifier,
        debug_build,
        env_data_dir.as_deref(),
    ) {
        single_instance = single_instance.dbus_id(id);
    }
    let commands = shell_commands();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(single_instance.build())
        .invoke_handler(move |invoke| {
            // The capability admits the gateway's origin by the webview's
            // active URI, which during a navigation is the provisional one;
            // the commands also need the running document to be the gateway's
            // (chat-voice §13.2, review M1). Tauri calls this on the GTK main
            // thread as the message arrives, where `media` tracks the commits.
            #[cfg(target_os = "linux")]
            if let Err(why) = media::command_allowed() {
                tracing::info!("command {} refused: {why}", invoke.message.command());
                invoke.resolver.reject(format!("refused: {why}"));
                return true;
            }
            commands(invoke)
        })
        .setup(move |app| {
            let data_dir = dev_guard::data_dir(
                env_data_dir,
                &default_data_dir(),
                lmgw_core::state::home_default_data_dir().as_deref(),
                debug_build,
            )?;
            let dev = debug_build || lmgw_core::state::dev_from_env();
            let state = tauri::async_runtime::block_on(AppState::init_with(data_dir, dev))?;
            // Before the server starts: `server::serve` spawns every pass
            // that lists or removes containers by prefix (`init_with` touches
            // none), and `server::bind` refuses a dev instance on the default
            // prefix itself.
            dev_guard::check_prefix(
                &state.snapshot().settings.container_prefix,
                &lmgw_core::config::default_container_prefix(),
                debug_build,
            )?;
            let addr = bind_addr(&state);

            // The window is built only once this process holds the port
            // (review m3): a failed bind would otherwise show — and grant the
            // microphone to — whoever holds it. A debug build refuses to
            // start; a release build keeps its tray and says why.
            let gateway = Gateway::default();
            let bound = tauri::async_runtime::block_on(gateway.start(state.clone(), addr));
            let tray_label = dev_guard::tray_label(debug_build);
            let head = match &bound {
                Ok(bound) => format!("{tray_label} · {bound}"),
                Err(why) if debug_build => {
                    return Err(format!(
                        "the gateway could not serve on {addr}: {why}. A debug shell never \
                         builds its window on a port this process does not hold."
                    )
                    .into())
                }
                Err(why) => {
                    tracing::error!("the gateway could not serve on {addr}: {why}");
                    format!("{tray_label} · not serving")
                }
            };
            app.manage(state.clone());
            app.manage(gateway);

            // --- tray menu (§12) ---
            let status_item = MenuItem::with_id(app, "status", head, false, None::<&str>)?;
            let runtime_item =
                MenuItem::with_id(app, "runtime-status", "models: …", false, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;
            let browser = MenuItem::with_id(app, "browser", "Open in Browser", true, None::<&str>)?;
            // Per-model containers (§3.6/§8): "start" is the group verb, which
            // starts the models flagged `warm_start` — never every configured
            // model, which could ask for more VRAM than the box has.
            let models_start = MenuItem::with_id(
                app,
                "models-start",
                "Start warm-start models",
                true,
                None::<&str>,
            )?;
            // GPU hold (gpu-hold design §6): a checkable item directly above
            // "Stop all models" — the tray's other GPU-affecting action.
            // Initial checked state comes from the persisted setting, so a
            // restart with hold already engaged shows it correctly before
            // the first 5 s tick.
            let hold_active = state.snapshot().settings.hold.active;
            let hold_item = CheckMenuItem::with_id(
                app,
                "hold",
                "Hold GPU · pause local models",
                true,
                hold_active,
                None::<&str>,
            )?;
            let models_stop =
                MenuItem::with_id(app, "models-stop", "Stop all models", true, None::<&str>)?;
            let restart = MenuItem::with_id(app, "restart", "Restart gateway", true, None::<&str>)?;
            let updates = dev_guard::runs_updater(debug_build);
            let check_updates = MenuItem::with_id(
                app,
                "check-updates",
                if updates {
                    "Check for Updates…"
                } else {
                    "Updates: off in a debug build"
                },
                updates,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &status_item,
                    &runtime_item,
                    &PredefinedMenuItem::separator(app)?,
                    &open,
                    &browser,
                    &PredefinedMenuItem::separator(app)?,
                    &models_start,
                    &hold_item,
                    &models_stop,
                    &restart,
                    &PredefinedMenuItem::separator(app)?,
                    &check_updates,
                    &quit,
                ],
            )?;

            let hold_item_for_menu = hold_item.clone();
            let mut tray = TrayIconBuilder::with_id("lmgw-tray");
            // A debug build's icon files beside its data, never in the dir
            // the installed app's tray reads and deletes from (review m2).
            if let Some(dir) = dev_guard::tray_icon_dir(&state.data_dir, debug_build) {
                tray = tray.temp_dir_path(dir);
            }
            tray
                // Monochrome symbolic variant so it blends into the system tray;
                // the colored icon stays as the window/app icon.
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/tray.png"
                ))?)
                .tooltip(dev_guard::tray_tooltip(
                    &state.data_dir,
                    debug_build,
                    hold_active,
                ))
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| {
                    let state: tauri::State<SharedState> = app.state();
                    match event.id.as_ref() {
                        "open" => show_main_window(app),
                        "browser" => match app.state::<Gateway>().status() {
                            // Minted here rather than at menu build time: a
                            // nonce lives 60 s and is single-use (§3.4), so a
                            // URL baked into the menu would be a dead
                            // credential by the first click and land the
                            // browser on the login card. The address this
                            // process holds, as for the window.
                            Ok(addr) => {
                                let url = login_url(&state, &addr);
                                // Not the URL: it carries a login nonce.
                                if let Err(e) = xdg_open(&url) {
                                    tracing::warn!("Open in Browser: xdg-open: {e}");
                                }
                            }
                            Err(why) => gateway::not_serving(app, &why),
                        },
                        "models-start" | "models-stop" => {
                            let action = if event.id.as_ref() == "models-start" {
                                "start"
                            } else {
                                "stop"
                            };
                            let st = (*state).clone();
                            tauri::async_runtime::spawn(async move {
                                // The same op the dashboard and the tool plane
                                // drive (§8) — no second lifecycle path for the
                                // tray. `force`, because a tray click is the
                                // owner deciding.
                                match lmgw_core::ops::container(
                                    &st,
                                    Some("all"),
                                    None,
                                    action,
                                    true,
                                    None,
                                )
                                .await
                                {
                                    Ok(v) => tracing::info!("container {action}: {v}"),
                                    Err(e) => tracing::error!("container {action}: {e}"),
                                }
                            });
                        }
                        "hold" => {
                            // Same op the dashboard's GPU pill and MCP
                            // drive (§6) — no second toggle path for the
                            // tray. Sync runs only on success so a failed
                            // toggle leaves the tray showing the state it
                            // actually is in; the next 5 s tick self-heals
                            // regardless (item 2).
                            let current = state.snapshot().settings.hold.active;
                            let next = !current;
                            let st = (*state).clone();
                            let app = app.clone();
                            let hold_item = hold_item_for_menu.clone();
                            tauri::async_runtime::spawn(async move {
                                match lmgw_core::ops::hold_set(&st, next).await {
                                    Ok(v) => {
                                        tracing::info!("hold_set: {v}");
                                        sync_hold(&app, &hold_item, next);
                                    }
                                    Err(e) => tracing::error!("hold_set: {e}"),
                                }
                            });
                        }
                        // Re-reads bind_addr (review m3: the window follows the
                        // new bind, or closes when there is none).
                        "restart" => gateway::restart(app, (*state).clone()),
                        "check-updates" if updates => updater::check_now(app.clone()),
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .build(app)?;

            // Keep tray status lines fresh.
            let state_for_status = state.clone();
            let app_for_status = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
                // `None` so the first tick always syncs once — that is what
                // makes a hold persisted across a restart show correctly on
                // the tray before anything toggles it again (§6).
                let mut last_hold: Option<bool> = None;
                loop {
                    tick.tick().await;
                    let snap = state_for_status.snapshot();
                    let stats = state_for_status.telemetry.stats();
                    let runtime = state_for_status.runtime().list();
                    let gateway: tauri::State<Gateway> = app_for_status.state();
                    let _ = status_item.set_text(match gateway.status() {
                        Ok(addr) => format!(
                            "{tray_label} · {addr} · {} req/min · {} active",
                            stats.req_last_minute, stats.active_requests
                        ),
                        Err(_) => format!("{tray_label} · not serving"),
                    });
                    // One line for N containers (§3.2): the count, and how many
                    // of them are serving something right now.
                    let busy = runtime.iter().filter(|v| v.in_flight > 0).count();
                    let hold_suffix = if snap.settings.hold.active {
                        " · HOLD"
                    } else {
                        ""
                    };
                    let _ = runtime_item.set_text(format!(
                        "models: {} running, {busy} busy{hold_suffix}",
                        runtime.len()
                    ));
                    // Sync the checkbox/icon/tooltip only on change (§6): a
                    // toggle from the dashboard or MCP reaches the tray
                    // within one tick without re-setting the icon every 5 s.
                    let active = snap.settings.hold.active;
                    if last_hold != Some(active) {
                        sync_hold(&app_for_status, &hold_item, active);
                        last_hold = Some(active);
                    }
                }
            });

            // Background update check (§12): polls the registry and prompts.
            if updates {
                updater::spawn(app.handle().clone());
            } else {
                tracing::info!(
                    "debug build: the updater is off — it would download to the installed \
                     app's path and install over the installed app"
                );
            }

            show_main_window(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides to tray; the gateway keeps running.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(context)
        .expect("error while running lmgw")
        // `build` + `run(callback)` instead of `run(context)` for exactly one
        // reason: `RunEvent::Exit` is the only place the per-model container
        // teardown can hang off (per-model-containers §3.4).
        //
        // "Quit" calls `app.exit(0)`, which never touches `ServerHandle` — the
        // Axum task is simply abandoned as the process dies — so the graceful
        // shutdown Axum has cannot be where containers get stopped. Nor may it
        // be: "Restart gateway" bounces that same task, and a gateway restart
        // must not dump every warm model off the GPU. Quitting must; restarting
        // must not; only this callback can tell the two apart.
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                let state: tauri::State<SharedState> = app.state();
                let state = (*state).clone();
                // Blocking on purpose: this is the last thing the process does,
                // and returning from it is what lets the exit proceed. Bounded
                // inside `shutdown` (its own timeout), so a wedged podman
                // delays the quit by that much and no longer.
                tauri::async_runtime::block_on(lmgw_core::runtime::lifecycle::shutdown(&state));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_is_the_gateways_own_origin_and_nothing_else() {
        let u = |s: &str| s.parse::<tauri::Url>().unwrap();
        let o = WindowOrigin::of(&u("http://127.0.0.1:8001/api/session/login?nonce=x"));
        let agent = |h: &str| h.strip_suffix(".localhost").is_some_and(|l| !l.is_empty());
        let ok = |s: &str| navigation_allowed(&u(s), &o, &agent);
        assert!(ok("http://127.0.0.1:8001/chat"));
        assert!(ok("http://127.0.0.1:8001/"));
        assert!(ok("http://board.localhost:8001/"), "an agent app frame");
        assert!(ok("about:blank"));
        assert!(ok("blob:http://127.0.0.1:8001/6f1b-uuid"));
        assert!(ok("about:srcdoc"), "the HTML preview frame");
        assert!(!ok("about:config"));
        assert!(!ok("http://127.0.0.1:9999/"), "another port");
        assert!(
            !ok("http://board.localhost:9999/"),
            "an agent host, other port"
        );
        assert!(!ok("http://evil.example:8001/"), "another host, same port");
        assert!(
            !ok("http://localhost:8001/"),
            "the bare suffix is not the gateway"
        );
        assert!(!ok("http://board.localhost.evil.example:8001/"));
        assert!(!ok("http://127.0.0.1:8001@evil.example/"), "userinfo trick");
        assert!(
            !ok("http://127.0.0.1:8001@127.0.0.1:8001/"),
            "userinfo at all"
        );
        assert!(!ok("http://evil.example@127.0.0.1:8001/"));
        assert!(!ok("http://[::1]:8001/"), "IPv6 loopback is another host");
        assert!(!ok("https://127.0.0.1:8001/"), "another scheme");
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("javascript:alert(1)"));
        assert!(!ok("data:text/html,<script>1</script>"));
        assert!(!ok("blob:http://evil.example:8001/x"));
        assert!(!ok("blob:data:text/html,x"));
        assert!(!ok("mailto:a@b.c"));
        assert!(!ok("lmgw-evil://x/y"));
        assert!(!ok("ms-msdt:/id"));
    }

    #[test]
    fn a_new_window_opens_only_web_and_mail() {
        let u = |s: &str| s.parse::<tauri::Url>().unwrap();
        assert!(new_window_may_open(&u("https://example.org/x")));
        assert!(new_window_may_open(&u("http://example.org/")));
        assert!(new_window_may_open(&u("mailto:a@b.c")));
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "smb://host/share",
            "lmgw-evil://x",
            "about:blank",
        ] {
            assert!(!new_window_may_open(&u(bad)), "{bad}");
        }
    }

    #[test]
    fn a_taken_download_name_gets_a_counter_before_the_extension() {
        let dir = std::env::temp_dir().join(format!("lmgw-dl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = dir.join("lmgw-chat-1-x.md");
        unique_path(&mut p);
        assert_eq!(p, dir.join("lmgw-chat-1-x.md"), "a free name stays");
        std::fs::write(&p, "a").unwrap();
        std::fs::write(dir.join("lmgw-chat-1-x (1).md"), "b").unwrap();
        let mut again = p.clone();
        unique_path(&mut again);
        assert_eq!(again, dir.join("lmgw-chat-1-x (2).md"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn env(home: &str, user_dirs: &str) -> DownloadEnv {
        DownloadEnv {
            home: Some(std::path::PathBuf::from(home)),
            user_dirs: user_dirs.to_string(),
        }
    }

    #[test]
    fn a_download_goes_to_the_downloads_folder_whatever_wry_proposed() {
        use std::path::{Path, PathBuf};
        // No XDG_DOWNLOAD_DIR: wry proposes the working directory, and the
        // file still lands in ~/Downloads.
        let none = env("/home/u", "");
        assert_eq!(
            download_target(Path::new("/some/cwd/lmgw-chat-1-x.md"), &none).unwrap(),
            PathBuf::from("/home/u/Downloads/lmgw-chat-1-x.md")
        );
        // Configured, as xdg-user-dirs writes it.
        let conf = env(
            "/home/u",
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Herunterladen\"\n",
        );
        assert_eq!(
            download_target(Path::new("/home/u/Herunterladen/a.zip"), &conf).unwrap(),
            PathBuf::from("/home/u/Herunterladen/a.zip")
        );
        let abs = env("/home/u", "XDG_DOWNLOAD_DIR=\"/data/dl\"\n");
        assert_eq!(
            download_target(Path::new("x.json"), &abs).unwrap(),
            PathBuf::from("/data/dl/x.json")
        );
        // A relative value is not a folder to trust.
        let rel = env("/home/u", "XDG_DOWNLOAD_DIR=\"dl\"\n");
        assert_eq!(
            download_target(Path::new("x.json"), &rel).unwrap(),
            PathBuf::from("/home/u/Downloads/x.json")
        );
    }

    #[test]
    fn a_download_keeps_only_its_file_name() {
        use std::path::{Path, PathBuf};
        let e = env("/home/u", "");
        for proposed in [
            "/home/u/Downloads/../../etc/evil.md",
            "/etc/evil.md",
            "../../evil.md",
            "evil.md",
        ] {
            assert_eq!(
                download_target(Path::new(proposed), &e).unwrap(),
                PathBuf::from("/home/u/Downloads/evil.md"),
                "{proposed}"
            );
        }
        assert_eq!(
            download_target(Path::new("/home/u/Downloads/.."), &e).unwrap(),
            PathBuf::from("/home/u/Downloads/download")
        );
        let no_home = DownloadEnv {
            home: None,
            user_dirs: String::new(),
        };
        assert!(download_target(Path::new("a.md"), &no_home).is_err());
    }
}
