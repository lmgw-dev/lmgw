//! A debug build of the shell keeps away from the installed app.
//!
//! The RPM is a release build, so a debug build is always a development run
//! (`cfg!(debug_assertions)` is that switch: the workspace's release profile
//! must keep debug assertions off) — and these would let one disturb the app
//! installed beside it:
//!
//! - **The single-instance plugin.** A second launch hands its arguments to
//!   the first over the session bus (`<identifier>.SingleInstance`) and exits,
//!   and the first answers by raising its window. A debug build registers
//!   `<identifier>.dev.d<hash of its data dir>` instead, so it neither reaches
//!   nor is reached by the installed app, and a second dev shell on another
//!   data dir runs beside the first rather than handing over to it.
//! - **The data dir.** Without `LMGW_DATA_DIR` the shell opens the installed
//!   app's, whose migrations are one-way. A debug build needs the variable and
//!   refuses that dir (also as `~/.local/share/lmgw` when `XDG_DATA_HOME`
//!   points elsewhere).
//! - **The container prefix.** Boot reconciliation removes every container
//!   under the prefix it does not know; under the production default those
//!   are the installed app's models. A debug build refuses a data dir still
//!   on the default prefix. `scripts/dev-instance.sh` and `scripts/dev-copy.sh`
//!   leave theirs on `lmgw-dev`. (`server::bind` refuses any dev instance on
//!   it too, the settings write refuses to move one onto it, and nothing
//!   lists or removes a container before the server runs.)
//! - **The port.** A debug build builds its window only once this process
//!   holds its port (`crate::gateway`), and refuses to start when the bind
//!   fails: beside the installed app or a headless instance on the same dir,
//!   it would otherwise show that gateway, with the microphone granted to it.
//! - **The updater.** It downloads to the installed app's path and installs
//!   the RPM over the installed app; a debug build does not run it.
//! - **WirePlumber's memory.** A debug build's audio streams are
//!   `lmgw-dev`, not `lmgw` (`audio_out`), so an output chosen in a dev window
//!   is never applied to the installed app's streams, nor the other way round.
//! - **The webview's own store.** WebKit keeps cookies, local storage and its
//!   HTTP cache in `~/.local/share/<identifier>`, the same for every build. A
//!   cookie ignores the port, so a debug window logging in on its dev port
//!   rewrites the installed window's `lmgw_session` in the shared jar, and two
//!   network processes share one cache. A debug build keeps its store in
//!   `<data dir>/webview`.
//! - **The tray icon's files.** tray-icon writes the icon to
//!   `$XDG_RUNTIME_DIR/tray-icon/tray-icon-<id>-<n>.png`, deletes the previous
//!   file at every icon change and the current one when it is dropped; the id
//!   and the counter are the installed app's too, so a dev quit or hold
//!   toggle deleted the installed tray's icon file (WP11 review m2). A debug
//!   build writes into `<data dir>/tray-icon`, and its tooltip names it
//!   `lmgw (dev)` with its data dir, so "Stop all models" is not clicked on the
//!   wrong tray.

use std::path::{Path, PathBuf};

/// The single-instance D-Bus id, when it differs from the plugin's default
/// (the bundle identifier): a debug build's own, per data dir.
pub(crate) fn single_instance_id(
    identifier: &str,
    debug_build: bool,
    data_dir: Option<&Path>,
) -> Option<String> {
    debug_build.then(|| match data_dir {
        // A bus-name element must not start with a digit.
        Some(dir) => format!("{identifier}.dev.d{:08x}", dir_hash(dir)),
        None => format!("{identifier}.dev"),
    })
}

/// A short hash of `dir` (as it resolves), the same in every build: FNV-1a,
/// folded to 32 bits. Not a security boundary — it only keeps two dev shells
/// on different data dirs from sharing a single-instance name.
fn dir_hash(dir: &Path) -> u32 {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dir.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h ^ (h >> 32)) as u32
}

/// The data dir to open: `LMGW_DATA_DIR` (`from_env`, empty read as unset by
/// `lmgw_core::state::data_dir_from_env`) or the installed app's
/// (`installed`, which follows `XDG_DATA_HOME`). A debug build refuses
/// `installed` and also `home_default` (`~/.local/share/lmgw`), so pointing
/// `XDG_DATA_HOME` elsewhere does not make the real dir look like a copy —
/// the headless runner's rule (`lmgw_core::state::is_installed_data_dir`).
pub(crate) fn data_dir(
    from_env: Option<PathBuf>,
    installed: &Path,
    home_default: Option<&Path>,
    debug_build: bool,
) -> Result<PathBuf, String> {
    let is_installed =
        |dir: &Path| lmgw_core::state::is_installed_data_dir(dir, installed, home_default);
    match from_env {
        None if debug_build => Err(format!(
            "a debug build of the shell needs LMGW_DATA_DIR, and never opens the installed \
             app's data dir ({}); seed a dev dir with scripts/dev-instance.sh or \
             scripts/dev-copy.sh copy",
            installed.display()
        )),
        None => Ok(installed.to_path_buf()),
        Some(dir) if debug_build && is_installed(&dir) => Err(format!(
            "LMGW_DATA_DIR ({}) is the installed app's data dir; a debug build needs a dev copy",
            dir.display()
        )),
        Some(dir) => Ok(dir),
    }
}

/// Whether this build runs the updater: not a debug build, which would
/// download to the installed app's path and install over the installed app.
pub(crate) fn runs_updater(debug_build: bool) -> bool {
    !debug_build
}

/// Refuse to boot a debug build on the production container prefix.
pub(crate) fn check_prefix(prefix: &str, default: &str, debug_build: bool) -> Result<(), String> {
    if debug_build && prefix == default {
        return Err(format!(
            "this data dir's container_prefix is the production default '{default}': boot \
             reconciliation would treat the installed app's model containers as its own; seed \
             the dir with scripts/dev-instance.sh or scripts/dev-copy.sh copy (prefix lmgw-dev)"
        ));
    }
    Ok(())
}

/// The webview's data dir, when it is not Tauri's default.
pub(crate) fn webview_data_dir(data_dir: &Path, debug_build: bool) -> Option<PathBuf> {
    debug_build.then(|| data_dir.join("webview"))
}

/// Where tray-icon writes the icon's files, when it is not its default
/// (`$XDG_RUNTIME_DIR/tray-icon`, the installed app's too).
pub(crate) fn tray_icon_dir(data_dir: &Path, debug_build: bool) -> Option<PathBuf> {
    debug_build.then(|| data_dir.join("tray-icon"))
}

/// Who the tray is, for its first menu line.
pub(crate) fn tray_label(debug_build: bool) -> &'static str {
    if debug_build {
        "lmgw (dev)"
    } else {
        "lmgw"
    }
}

/// The tray's tooltip: a debug build names itself and its data dir, and the
/// GPU hold is said when it is on (gpu-hold design §6).
pub(crate) fn tray_tooltip(data_dir: &Path, debug_build: bool, hold: bool) -> String {
    let mut tip = tray_label(debug_build).to_string();
    if debug_build {
        tip.push_str(&format!(" — {}", data_dir.display()));
    }
    if hold {
        tip.push_str(" — GPU hold: local models paused, new loads refused or re-routed");
    }
    tip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_debug_build_has_its_own_single_instance_name_per_data_dir() {
        assert_eq!(
            single_instance_id("io.example.app", true, None).as_deref(),
            Some("io.example.app.dev")
        );
        assert_eq!(
            single_instance_id("io.example.app", false, Some(Path::new("/srv/a"))),
            None
        );
        let a = single_instance_id("io.example.app", true, Some(Path::new("/srv/a"))).unwrap();
        let b = single_instance_id("io.example.app", true, Some(Path::new("/srv/b"))).unwrap();
        assert_ne!(a, b, "two data dirs, two names");
        assert_eq!(
            Some(a.clone()),
            single_instance_id("io.example.app", true, Some(Path::new("/srv/a"))),
            "stable"
        );
        // A bus name: elements of [A-Za-z0-9_-], none starting with a digit.
        for element in a.split('.') {
            assert!(!element.starts_with(|c: char| c.is_ascii_digit()), "{a}");
            assert!(
                element
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{a}"
            );
        }
    }

    #[test]
    fn a_debug_build_does_not_run_the_updater() {
        assert!(!runs_updater(true));
        assert!(runs_updater(false));
    }

    #[test]
    fn a_debug_build_needs_a_data_dir_that_is_not_the_installed_apps() {
        let installed = std::env::temp_dir().join(format!("lmgw-guard-{}", std::process::id()));
        std::fs::create_dir_all(&installed).unwrap();
        let dev = PathBuf::from("/srv/dev-copy");

        assert_eq!(data_dir(None, &installed, None, false).unwrap(), installed);
        assert!(data_dir(None, &installed, None, true).is_err());
        assert_eq!(
            data_dir(Some(dev.clone()), &installed, None, true).unwrap(),
            dev
        );
        assert!(data_dir(Some(installed.clone()), &installed, None, true).is_err());
        // The same dir spelled another way.
        let spelled = installed.join("..").join(installed.file_name().unwrap());
        assert!(data_dir(Some(spelled), &installed, None, true).is_err());
        // XDG_DATA_HOME moved elsewhere: the dir under HOME is still refused.
        let moved = Path::new("/srv/xdg-data/lmgw");
        assert!(data_dir(Some(installed.clone()), moved, Some(&installed), true).is_err());
        // A release build opens what it is told.
        assert_eq!(
            data_dir(Some(installed.clone()), &installed, None, false).unwrap(),
            installed
        );
        std::fs::remove_dir_all(&installed).unwrap();
    }

    #[test]
    fn a_debug_build_keeps_its_webview_store_in_its_data_dir() {
        let dir = Path::new("/srv/dev-copy");
        assert_eq!(
            webview_data_dir(dir, true),
            Some(PathBuf::from("/srv/dev-copy/webview"))
        );
        assert_eq!(webview_data_dir(dir, false), None);
    }

    #[test]
    fn a_debug_builds_tray_is_its_own() {
        let dir = Path::new("/srv/dev-copy");
        assert_eq!(
            tray_icon_dir(dir, true),
            Some(PathBuf::from("/srv/dev-copy/tray-icon"))
        );
        assert_eq!(tray_icon_dir(dir, false), None, "tray-icon's default");
        assert_eq!(tray_tooltip(dir, false, false), "lmgw");
        assert_eq!(tray_tooltip(dir, true, false), "lmgw (dev) — /srv/dev-copy");
        assert!(tray_tooltip(dir, true, true).starts_with("lmgw (dev) — /srv/dev-copy — GPU hold"));
        assert!(tray_tooltip(dir, false, true).starts_with("lmgw — GPU hold"));
    }

    #[test]
    fn a_debug_build_refuses_the_production_prefix() {
        assert!(check_prefix("lmgw", "lmgw", true).is_err());
        assert!(check_prefix("lmgw-dev", "lmgw", true).is_ok());
        assert!(check_prefix("lmgw", "lmgw", false).is_ok());
    }
}
