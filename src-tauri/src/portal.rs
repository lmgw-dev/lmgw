//! The desktop's own file dialogs: the Save As every download asks with and
//! the picker an agent's mount field opens are `GtkFileChooserNative`s,
//! which outside a sandbox draw GTK's own dialog, foreign on Plasma. GTK 3
//! hands them to the xdg-desktop-portal file chooser, which on Plasma is
//! KDE's own dialog, when `GTK_USE_PORTAL=1`: GTK 3.24's
//! `gdk_should_use_portal()` reads exactly that variable (or a Flatpak
//! sandbox). `GDK_DEBUG=portals` is GTK 4's switch; GTK 3's debug keys have
//! no `portals` (checked against the gtk-3-24 branch, 2026-10-17).
//!
//! Set only where xdg-desktop-portal would send the file chooser to an
//! installed backend, decided the way the daemon (1.22) does: the first
//! `<desktop>-portals.conf` (each `XDG_CURRENT_DESKTOP` entry, lowercased),
//! else `portals.conf`, in the first of its config locations that has one;
//! its `org.freedesktop.impl.portal.FileChooser=` entry, else `default=`;
//! `none` is no portal, a named backend counts when its `.portal` file (in
//! the daemon's own `portals` directory) lists the FileChooser interface.
//! Only when no file names anything does a backend's `UseIn` decide. GTK 3
//! does not fall back to its own dialog when the portal call fails: the
//! dialog never shows and the response is `DELETE_EVENT` (the Save As
//! reports that as a failure). An explicit value always wins,
//! `GTK_USE_PORTAL=0` included.
//!
//! The variable also makes GDK read its Wayland settings (fonts, theme,
//! cursor) from the Settings portal's `org.gnome.*` namespaces, falling
//! back to GSettings when the portal answers none; on Plasma the portal's
//! GTK backend serves the same GSettings values (compared key by key,
//! 2026-10-17), so the window looks the same. A browser `xdg-open` starts
//! does not inherit it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The variable.
pub(crate) const VAR: &str = "GTK_USE_PORTAL";

/// The interface a backend must implement.
const CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";

/// Whether this process set [`VAR`] itself.
static SET_BY_US: OnceLock<bool> = OnceLock::new();

/// One installed backend: the `.portal` file's stem and text.
pub(crate) struct Backend {
    pub name: String,
    pub text: String,
}

/// The `key=` list of a keyfile's `group`, split on `;`; `None` when absent.
fn keyfile_list(text: &str, group: &str, key: &str) -> Option<Vec<String>> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = g == group;
        } else if in_group && !line.starts_with('#') {
            if let Some(v) = line
                .strip_prefix(key)
                .and_then(|r| r.trim_start().strip_prefix('='))
            {
                return Some(
                    v.split(';')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
        }
    }
    None
}

impl Backend {
    fn has_chooser(&self) -> bool {
        keyfile_list(&self.text, "portal", "Interfaces")
            .is_some_and(|l| l.iter().any(|i| i == CHOOSER))
    }
}

/// The backend list the first config file that exists names for the file
/// chooser (its own key, else `default`); `None` when no file exists or the
/// file names nothing. `conf_dirs` are the `xdg-desktop-portal` directories,
/// highest precedence first; in each, the desktop-specific files come
/// before `portals.conf`, and the first file found is the only one read.
pub(crate) fn preferred_backends(
    current_desktop: &str,
    conf_dirs: &[PathBuf],
) -> Option<Vec<String>> {
    let mut names: Vec<String> = current_desktop
        .split(':')
        .map(|d| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .map(|d| format!("{d}-portals.conf"))
        .collect();
    names.push("portals.conf".into());
    for dir in conf_dirs {
        for name in &names {
            let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
                continue;
            };
            return keyfile_list(&text, "preferred", CHOOSER)
                .or_else(|| keyfile_list(&text, "preferred", "default"));
        }
    }
    None
}

/// Whether the file chooser has a portal backend: the configured list when
/// there is one (first backend that lists the interface wins; `none` stops;
/// `*` is any), else a backend whose `UseIn` has one of the desktops.
pub(crate) fn chooser_available(
    current_desktop: &str,
    conf_dirs: &[PathBuf],
    backends: &[Backend],
) -> bool {
    if let Some(list) = preferred_backends(current_desktop, conf_dirs) {
        for name in &list {
            match name.as_str() {
                "none" => return false,
                "*" => {
                    if backends.iter().any(Backend::has_chooser) {
                        return true;
                    }
                }
                n => {
                    if backends.iter().any(|b| b.name == n && b.has_chooser()) {
                        return true;
                    }
                }
            }
        }
        return false;
    }
    let desktops: Vec<String> = current_desktop
        .split(':')
        .map(|d| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect();
    backends.iter().any(|b| {
        b.has_chooser()
            && keyfile_list(&b.text, "portal", "UseIn")
                .is_some_and(|l| l.iter().any(|u| desktops.contains(&u.to_ascii_lowercase())))
    })
}

/// The `*.portal` files of xdg-desktop-portal's own directory
/// (`$XDG_DESKTOP_PORTAL_DIR`, else `/usr/share/xdg-desktop-portal/portals`).
fn installed_backends() -> Vec<Backend> {
    let dir = std::env::var_os("XDG_DESKTOP_PORTAL_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/usr/share/xdg-desktop-portal/portals".into());
    read_backends(&dir)
}

/// The `*.portal` files in `dir`, by name.
pub(crate) fn read_backends(dir: &Path) -> Vec<Backend> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Backend> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "portal" {
                return None;
            }
            Some(Backend {
                name: p.file_stem()?.to_string_lossy().into_owned(),
                text: std::fs::read_to_string(&p).ok()?,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The `xdg-desktop-portal` directories that can hold `portals.conf`, in the
/// order of `portals.conf(5)`: config home, config dirs, `/etc`, data home,
/// data dirs, `/usr/share`.
fn conf_dirs() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let home = env("HOME").map(PathBuf::from);
    let list = |v: String| v.split(':').map(PathBuf::from).collect::<Vec<_>>();
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.extend(
        env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config"))),
    );
    roots.extend(list(
        env("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into()),
    ));
    roots.push("/etc".into());
    roots.extend(
        env("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share"))),
    );
    roots.extend(list(
        env("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
    ));
    roots.push("/usr/share".into());
    roots
        .into_iter()
        .filter(|r| r.is_absolute())
        .map(|r| r.join("xdg-desktop-portal"))
        .collect()
}

/// Whether [`VAR`] is `1` now, which is what GTK 3 tests: the file dialogs
/// go through the portal.
pub(crate) fn in_use() -> bool {
    std::env::var(VAR).is_ok_and(|v| v == "1")
}

/// Set [`VAR`] when it is unset and the desktop has a file-chooser portal.
/// Before GTK starts, so first in `main`. Answers what was decided, for the
/// log once there is one.
pub(crate) fn apply() -> &'static str {
    if std::env::var_os(VAR).is_some() {
        let _ = SET_BY_US.set(false);
        return "GTK_USE_PORTAL is set: the file dialogs follow it";
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let set = chooser_available(&desktop, &conf_dirs(), &installed_backends());
    if set {
        std::env::set_var(VAR, "1");
    }
    let _ = SET_BY_US.set(set);
    if set {
        "GTK_USE_PORTAL=1: the file dialogs are the desktop's own (its file-chooser portal)"
    } else {
        "no file-chooser portal for this desktop: the file dialogs are GTK's own"
    }
}

/// Remove [`VAR`] from a child's environment when this process set it: what
/// `xdg-open` starts is not lmgw's window.
pub(crate) fn scrub(cmd: &mut std::process::Command) {
    if SET_BY_US.get().copied().unwrap_or(false) {
        cmd.env_remove(VAR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KDE: &str = "[portal]\nDBusName=org.freedesktop.impl.portal.desktop.kde\n\
                       Interfaces=org.freedesktop.impl.portal.Access;\
                       org.freedesktop.impl.portal.FileChooser;org.freedesktop.impl.portal.Print\n\
                       UseIn=KDE\n";
    const GTK: &str = "[portal]\nInterfaces=org.freedesktop.impl.portal.FileChooser;\n\
                       UseIn=gnome\n";
    const WALLET: &str = "[portal]\nInterfaces=org.freedesktop.impl.portal.Secret\nUseIn=KDE\n";

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    /// A portals directory with kde, gtk and a wallet-only backend.
    fn portals(root: &Path) -> Vec<Backend> {
        let dir = root.join("portals");
        write(&dir, "kde.portal", KDE);
        write(&dir, "gtk.portal", GTK);
        write(&dir, "kwallet.portal", WALLET);
        write(&dir, "notes.txt", "not a portal");
        read_backends(&dir)
    }

    #[test]
    fn only_portal_files_are_read_by_name() {
        let t = tempfile::tempdir().unwrap();
        let names: Vec<_> = portals(t.path()).into_iter().map(|b| b.name).collect();
        assert_eq!(names, ["gtk", "kde", "kwallet"]);
    }

    #[test]
    fn the_real_plasma_case_follows_its_conf_to_kde() {
        let t = tempfile::tempdir().unwrap();
        let backends = portals(t.path());
        let conf = t.path().join("conf");
        write(
            &conf,
            "kde-portals.conf",
            "[preferred]\ndefault=kde\norg.freedesktop.impl.portal.Settings=kde;gtk;\n",
        );
        assert!(chooser_available(
            "KDE",
            std::slice::from_ref(&conf),
            &backends
        ));
    }

    #[test]
    fn a_conf_that_names_none_or_a_backend_without_a_chooser_means_no_portal() {
        let t = tempfile::tempdir().unwrap();
        let backends = portals(t.path());
        let conf = t.path().join("conf");
        write(&conf, "kde-portals.conf", "[preferred]\ndefault=none\n");
        assert!(
            !chooser_available("KDE", std::slice::from_ref(&conf), &backends),
            "none"
        );
        write(&conf, "kde-portals.conf", "[preferred]\ndefault=kwallet\n");
        assert!(
            !chooser_available("KDE", std::slice::from_ref(&conf), &backends),
            "no chooser"
        );
        write(&conf, "kde-portals.conf", "[preferred]\ndefault=missing\n");
        assert!(
            !chooser_available("KDE", std::slice::from_ref(&conf), &backends),
            "not installed"
        );
        write(
            &conf,
            "kde-portals.conf",
            "[preferred]\ndefault=none\norg.freedesktop.impl.portal.FileChooser=missing;gtk\n",
        );
        assert!(
            chooser_available("KDE", std::slice::from_ref(&conf), &backends),
            "own key, next entry"
        );
        write(&conf, "kde-portals.conf", "[preferred]\ndefault=*\n");
        assert!(chooser_available("KDE", &[conf], &backends), "any");
    }

    #[test]
    fn the_first_config_location_with_a_file_wins_and_desktop_files_come_first() {
        let t = tempfile::tempdir().unwrap();
        let backends = portals(t.path());
        let (user, system) = (t.path().join("user"), t.path().join("system"));
        write(&system, "kde-portals.conf", "[preferred]\ndefault=kde\n");
        write(&user, "portals.conf", "[preferred]\ndefault=none\n");
        let dirs = [user.clone(), system.clone()];
        assert!(
            !chooser_available("KDE", &dirs, &backends),
            "the user's file shadows"
        );
        write(&user, "kde-portals.conf", "[preferred]\ndefault=gtk\n");
        assert!(
            chooser_available("KDE", &dirs, &backends),
            "desktop file before portals.conf"
        );
        assert!(
            !chooser_available("XFCE", &dirs, &backends),
            "portals.conf for others"
        );
    }

    #[test]
    fn without_a_naming_conf_use_in_decides() {
        let t = tempfile::tempdir().unwrap();
        let backends = portals(t.path());
        let empty = t.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let dirs = [empty.clone()];
        assert!(chooser_available("KDE", &dirs, &backends));
        assert!(
            chooser_available("ubuntu:GNOME", &dirs, &backends),
            "any entry, any case"
        );
        assert!(
            !chooser_available("XFCE", &dirs, &backends),
            "no backend made for it"
        );
        assert!(!chooser_available("", &dirs, &backends), "no desktop named");
        assert!(!chooser_available("KDE", &dirs, &[]), "no portal installed");
        write(&empty, "portals.conf", "[preferred]\n# nothing\n");
        assert!(
            chooser_available("KDE", &dirs, &backends),
            "a conf naming nothing"
        );
    }
}
