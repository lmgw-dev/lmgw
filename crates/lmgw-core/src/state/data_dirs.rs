//! Where lmgw keeps its files: the one place that knows the platform's
//! directories (Windows design §4.4, WP6).
//!
//! - **The data dir** ([`default_data_dir`], or `LMGW_DATA_DIR`): the
//!   databases, the settings in them, the device CA, everything that is the
//!   install's state. Linux: XDG's `~/.local/share/lmgw`. Windows:
//!   `%APPDATA%\lmgw`, the roaming profile's application data.
//! - **The local dir** ([`local_data_dir`]): what is big, machine-bound or
//!   can be fetched again — the models a fresh install downloads into and
//!   the engines (`<local>/engines`). Linux: the data dir itself. Windows,
//!   for the installed app's data dir: `%LOCALAPPDATA%\lmgw`, which a roaming
//!   profile does not carry to another machine. Any other data dir (a dev
//!   instance, `LMGW_DATA_DIR`) keeps everything in itself, so one dir is all
//!   such an instance writes.
//!
//! The rest of lmgw asks here and nowhere else; an engines root, a models
//! default and the like are `local_data_dir(data_dir).join(…)`.

use std::path::{Path, PathBuf};

/// The directory's name under the platform's application-data dir.
const APP_DIR: &str = "lmgw";

/// The installed app's data dir as the environment names it: Linux
/// `$XDG_DATA_HOME/lmgw`, else `~/.local/share/lmgw`; Windows
/// `%APPDATA%\lmgw`, else the roaming application-data known folder's.
#[cfg(not(windows))]
pub fn default_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join(APP_DIR);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share").join(APP_DIR)
}

#[cfg(windows)]
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).join(APP_DIR))
        .or_else(home_default_data_dir)
        .unwrap_or_else(|| PathBuf::from(".").join(APP_DIR))
}

/// The installed app's data dir whatever the environment says: Linux
/// `~/.local/share/lmgw` from `HOME` (`XDG_DATA_HOME` left aside); Windows
/// the roaming application-data known folder's `lmgw`
/// (`FOLDERID_RoamingAppData`, `APPDATA` left aside).
#[cfg(not(windows))]
pub fn home_default_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share").join(APP_DIR))
}

#[cfg(windows)]
pub fn home_default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DIR))
}

/// Where `data_dir`'s models, engines and caches go (module doc): the data
/// dir itself, except on Windows for the installed app's data dir, whose
/// local dir is `%LOCALAPPDATA%\lmgw`.
pub fn local_data_dir(data_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    if super::is_installed_data_dir(
        data_dir,
        &default_data_dir(),
        home_default_data_dir().as_deref(),
    ) {
        if let Some(local) = installed_local_dir() {
            return local;
        }
        // Said once per process: every engines-root and models-dir read
        // comes through here, and the answer does not change while lmgw runs.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "neither LOCALAPPDATA nor the local application-data folder is known, so models \
                 and engines stay in {}",
                data_dir.display()
            )
        });
    }
    data_dir.to_path_buf()
}

/// `%LOCALAPPDATA%\lmgw`, else the local application-data known folder's.
#[cfg(windows)]
fn installed_local_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .map(|d| d.join(APP_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data dir that is not the installed app's keeps everything in itself.
    #[test]
    fn another_data_dir_is_its_own_local_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(local_data_dir(dir.path()), dir.path());
    }

    /// Linux: one dir for both.
    #[cfg(not(windows))]
    #[test]
    fn the_installed_data_dir_is_its_own_local_dir_on_linux() {
        assert_eq!(local_data_dir(&default_data_dir()), default_data_dir());
    }

    /// Windows: the installed app's data dir is under the roaming profile,
    /// its models and engines under the local one.
    #[cfg(windows)]
    #[test]
    fn the_installed_data_dir_roams_and_its_local_dir_does_not() {
        let roaming = PathBuf::from(std::env::var_os("APPDATA").unwrap()).join("lmgw");
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("lmgw");
        assert_eq!(default_data_dir(), roaming);
        assert_eq!(home_default_data_dir(), Some(roaming.clone()));
        assert_eq!(local_data_dir(&roaming), local);
    }
}
