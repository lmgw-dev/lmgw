//! The window's downloads: every one asks where to save it.
//!
//! An export, a clip, a picture, a chat, a knowledge original, an agent
//! definition: the page starts each as a click on `<a download>` (or any
//! `Content-Disposition: attachment` answer), and WebKitGTK hands it to the
//! shell to decide where it goes. The shell answers with a Save As dialog
//! (`save_dialog`, Linux): the name the server or the link suggested, in
//! the folder chosen last in this app run, else the Downloads folder. The
//! answer comes asynchronously (WebKitGTK ≥ 2.40 lets the
//! `decide-destination` signal be answered later), so the window never
//! waits on the dialog. A chosen file is written there, replaced when it
//! existed (the dialog asked first); Cancel saves nothing and says nothing.
//! The page hears where a file went (`lmgw-downloaded`, toasted) or that it
//! failed (`lmgw-download-failed`); a plain browser decides for itself.
//!
//! A download with no window to ask in (none that WebKit can name) goes to
//! the Downloads folder as before the dialog: under its file name only,
//! never over an existing file (`name (1).ext`).

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub(crate) mod save_dialog;

#[cfg(test)]
mod tests;

/// What the download paths read from the process: `$HOME`,
/// `$XDG_CONFIG_HOME` and the contents of `user-dirs.dirs` under it.
pub(crate) struct DownloadEnv {
    pub home: Option<PathBuf>,
    pub user_dirs: String,
}

impl DownloadEnv {
    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            // The XDG spec: a relative value is invalid and ignored.
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let user_dirs = config
            .and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok())
            .unwrap_or_default();
        Self { home, user_dirs }
    }

    /// The Downloads folder: `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else
    /// `~/Downloads`; `None` without a home.
    pub fn downloads(&self) -> Option<PathBuf> {
        let home = self.home.as_deref()?;
        Some(xdg_download_dir(&self.user_dirs, home).unwrap_or_else(|| home.join("Downloads")))
    }
}

/// The file name of `proposed`, a suggested name or a path: its last part
/// only (`../x`, an absolute path keep just their last component), else
/// `download`.
pub(crate) fn file_name_of(proposed: &Path) -> std::ffi::OsString {
    match proposed.components().next_back() {
        Some(std::path::Component::Normal(n)) => n.to_os_string(),
        _ => std::ffi::OsString::from("download"),
    }
}

/// Where a download proposed as `proposed` is saved without a dialog: its
/// **file name only** ([`file_name_of`]) in the Downloads folder
/// ([`DownloadEnv::downloads`]).
///
/// wry proposes `dirs::download_dir()/<name>`, and `dirs` reads only
/// `user-dirs.dirs`: without an `XDG_DOWNLOAD_DIR` there it proposes the
/// working directory, even when `~/Downloads` exists (review R1 finding 7).
/// The folder is therefore always decided here, never taken from the
/// proposal.
pub(crate) fn download_target(proposed: &Path, env: &DownloadEnv) -> std::io::Result<PathBuf> {
    let dir = env.downloads().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no HOME and no Downloads folder",
        )
    })?;
    Ok(dir.join(file_name_of(proposed)))
}

/// A download saved with no dialog: [`download_target`], its folder created
/// when missing, a taken name stepped to `<name> (1).<ext>`.
pub(crate) fn fallback_destination(proposed: &Path) -> std::io::Result<PathBuf> {
    let mut dest = download_target(proposed, &DownloadEnv::from_process())?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    unique_path(&mut dest);
    Ok(dest)
}

/// `XDG_DOWNLOAD_DIR` as `xdg-user-dirs` writes it into `user-dirs.dirs`
/// (`"$HOME/…"` or an absolute path); `None` when it is not set there, or
/// set to something that is neither.
fn xdg_download_dir(user_dirs: &str, home: &Path) -> Option<PathBuf> {
    user_dirs.lines().find_map(|l| {
        let v = l
            .trim()
            .strip_prefix("XDG_DOWNLOAD_DIR=")?
            .trim()
            .trim_matches('"');
        let p = match v.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(v),
        };
        p.is_absolute().then_some(p)
    })
}

/// `name.ext` → `name (1).ext`, `name (2).ext`, … until nothing is there.
pub(crate) fn unique_path(p: &mut PathBuf) {
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

/// Where the Save As dialog opens and with which name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DialogStart {
    /// `None` leaves the folder to the dialog (no home at all).
    pub folder: Option<PathBuf>,
    pub name: String,
}

/// What this app run remembers between downloads: the folder chosen last.
#[derive(Debug, Default)]
pub(crate) struct SaveSession {
    last_dir: Option<PathBuf>,
}

impl SaveSession {
    /// The dialog for a download suggested as `suggested`: its file name
    /// ([`file_name_of`]), in the folder chosen last while `is_dir` says it
    /// is still there, else the Downloads folder while it is there, else
    /// home.
    pub fn start(
        &self,
        suggested: &str,
        env: &DownloadEnv,
        is_dir: impl Fn(&Path) -> bool,
    ) -> DialogStart {
        let folder = self
            .last_dir
            .clone()
            .filter(|d| is_dir(d))
            .or_else(|| env.downloads().filter(|d| is_dir(d)))
            .or_else(|| env.home.clone());
        DialogStart {
            folder,
            name: file_name_of(Path::new(suggested))
                .to_string_lossy()
                .into_owned(),
        }
    }

    /// The owner chose `path`: the download goes there as it is (no
    /// counter: the dialog asked before replacing a file), and the next
    /// dialog opens in its folder.
    pub fn chosen(&mut self, path: PathBuf) -> PathBuf {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            self.last_dir = Some(dir.to_path_buf());
        }
        path
    }
}

/// What the page hears about a finished download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notice {
    /// `lmgw-downloaded` `{path}`.
    Saved(String),
    /// `lmgw-download-failed` `{error}`.
    Failed(String),
}

/// The notice for a download of `url` that finished: nothing after the
/// owner's Cancel, the file it went to, or why it did not finish (`failed`,
/// WebKit's error or the shell's own).
pub(crate) fn notice(
    canceled: bool,
    failed: Option<&str>,
    destination: Option<&str>,
    url: &str,
) -> Option<Notice> {
    if canceled {
        return None;
    }
    Some(match (failed, destination) {
        (None, Some(path)) => Notice::Saved(path.to_string()),
        (Some(why), _) => Notice::Failed(format!("{url} did not finish: {why}")),
        (None, None) => Notice::Failed(format!("{url} did not finish")),
    })
}

/// The script that fires `event` on the page's `window` with `{key: value}`
/// as its detail.
pub(crate) fn page_event_js(event: &str, key: &str, value: &str) -> String {
    let detail = serde_json::json!({ key: value });
    format!(
        "window.dispatchEvent(new CustomEvent({}, {{detail: {}}}));",
        serde_json::json!(event),
        detail
    )
}

impl Notice {
    /// The page event that says it.
    pub fn js(&self) -> String {
        match self {
            Self::Saved(p) => page_event_js("lmgw-downloaded", "path", p),
            Self::Failed(e) => page_event_js("lmgw-download-failed", "error", e),
        }
    }
}

/// Elsewhere than Linux there is no dialog: wry's download handler saves
/// into the Downloads folder ([`fallback_destination`]) and tells the page.
/// Compiled everywhere, so a Linux build type-checks it too.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn to_downloads_folder(
    webview: tauri::Webview,
    event: tauri::webview::DownloadEvent<'_>,
) -> bool {
    use tauri::webview::DownloadEvent;
    let tell = |n: Notice| {
        if let Err(e) = webview.eval(n.js()) {
            tracing::warn!("telling the page about a download: {e}");
        }
    };
    match event {
        DownloadEvent::Requested { destination, .. } => {
            match fallback_destination(destination.as_path()) {
                Ok(p) => {
                    *destination = p;
                    true
                }
                Err(e) => {
                    tracing::warn!("download refused: {e}");
                    tell(Notice::Failed(format!("no folder to save into: {e}")));
                    false
                }
            }
        }
        DownloadEvent::Finished { path, success, url } => {
            let path = path.map(|p| p.to_string_lossy().into_owned());
            let failed = (!success).then_some("the download failed");
            if let Some(n) = notice(false, failed, path.as_deref(), url.as_str()) {
                tell(n);
            }
            true
        }
        _ => true,
    }
}
