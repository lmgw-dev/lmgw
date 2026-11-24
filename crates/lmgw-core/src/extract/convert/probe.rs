//! Probing one tool (document-conversion design §4.3): a `PATH` lookup of its
//! binaries, which starts no process, then a version run with
//! [`crate::child`] hygiene, stdin closed, a private empty working
//! directory, its own process group killed whole on a timeout or a drop
//! ([`crate::child::group`]) and [`TOOL_PROBE_TIMEOUT`], which the failure
//! names. The parsers are plain functions over the tools' own output, tested
//! on what the real tools print.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use super::caps::{Extra, Tool, AUDIO_DECODERS, COMPONENTS};
use super::tools::{Fact, Found};
use super::watch::Watched;
use super::winpath;
use lmgw_api_types::documents::{ExtraKind, ToolExtra};

/// How long one probe run (a version run, or `--list-langs`, `-decoders`)
/// may take before it is stopped and the tool is marked as failing its
/// probe, with this constant named.
///
/// Ten seconds. A version run prints a line and exits: measured on
/// 2026-11-18 (Fedora 45), `soffice --version` with a fresh profile took
/// 0.09 s and the other tools less. One still going after ten seconds is
/// hung (a network home directory, a broken install), and the page says so
/// instead of waiting.
pub const TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a binary was found, and what identifies that file: a changed
/// stamp means it was updated or replaced, and the tool is probed again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    pub stamp: Stamp,
}

/// `(device, inode, mtime)` on unix; `(length, mtime)` elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    dev: u64,
    ino: u64,
    mtime: Option<SystemTime>,
}

/// The stamp of the file at `path` (symlinks followed), or `None` when it
/// is gone.
pub fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0, m.len());
    Some(Stamp {
        dev,
        ino,
        mtime: m.modified().ok(),
    })
}

/// The binary's file name on this system.
fn exe_name(name: &str) -> OsString {
    if cfg!(windows) {
        OsString::from(format!("{name}.exe"))
    } else {
        OsString::from(name)
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(m) = std::fs::metadata(path) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// Find `name` on `search_path` (a `PATH`-style list), then in `dirs`
/// (a tool's known install directories on Windows and macOS, `%VAR%`
/// expanded). No process is started. A relative entry (an empty one, `.`,
/// `bin`) is skipped, as Go's `exec.LookPath` does: what it names depends
/// on the working directory, which is not the person's.
pub fn lookup(name: &str, search_path: &OsStr, dirs: &[&str]) -> Option<Located> {
    let file = exe_name(name);
    std::env::split_paths(search_path)
        .chain(dirs.iter().map(|d| PathBuf::from(winpath::expand(d))))
        .filter(|d| d.is_absolute())
        .map(|d| d.join(&file))
        .find(|p| is_executable(p))
        .and_then(|path| {
            let stamp = stamp(&path)?;
            Some(Located { path, stamp })
        })
}

/// What one run printed.
struct Ran {
    stdout: String,
    stderr: String,
    success: bool,
    status: String,
}

/// Run `bin args…` for a probe: [`crate::child::system_tool_tokio`], stdin
/// closed, a private empty working directory, in a process group of its own
/// that is killed whole after [`TOOL_PROBE_TIMEOUT`] or when dropped (a
/// LibreOffice `soffice` script starts `soffice.bin` behind it). `Err` is
/// the sentence the page shows.
async fn run(bin: &Path, args: &[OsString], cwd: &Path, timeout: Duration) -> Result<Ran, String> {
    let shown = || {
        let mut s = bin.display().to_string();
        for a in args {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    };
    let mut cmd = tokio::process::Command::new(bin);
    crate::child::system_tool_tokio(&mut cmd);
    crate::child::group::own_group(&mut cmd);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| format!("{} could not be run: {e}", shown()))?;
    let group = crate::child::group::KillGroup::of(&child);
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("{} could not be run: {e}", shown())),
        // `group` is dropped here: the whole group is killed.
        Err(_) => {
            return Err(format!(
                "{} did not answer within {} (TOOL_PROBE_TIMEOUT) and was stopped",
                shown(),
                said(timeout)
            ))
        }
    };
    group.disarm();
    Ok(Ran {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
        status: out.status.to_string(),
    })
}

/// The first [`super::run::STDERR_LINES_SHOWN`] lines of `text`,
/// with how many more there were.
fn first_lines(text: &str) -> String {
    let n = super::run::STDERR_LINES_SHOWN;
    let lines: Vec<&str> = text.trim().lines().collect();
    let mut shown = lines[..lines.len().min(n)].join("\n");
    if lines.len() > n {
        shown.push_str(&format!("\n(… {} more lines)", lines.len() - n));
    }
    shown
}

/// The version that follows `marker` in `text`: on the first line where
/// the word right after it holds a digit, that word (a leading `v`
/// dropped). `pdftotext version 26.08.0` → `26.08.0`; a git build's
/// `ffmpeg version N-117000-gabc` → `N-117000-gabc`. The marker is a whole
/// word, with an optional `.exe` after it (Windows' `pandoc.exe 3.7.0.2`).
pub fn version_after(text: &str, marker: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.match_indices(marker).find_map(|(at, _)| {
            let before = line[..at].chars().next_back();
            if before.is_some_and(char::is_alphanumeric) {
                return None;
            }
            let mut rest = &line[at + marker.len()..];
            if rest.len() >= 4 && rest.is_char_boundary(4) && rest[..4].eq_ignore_ascii_case(".exe")
            {
                rest = &rest[4..];
            }
            if rest.chars().next().is_some_and(char::is_alphanumeric) {
                return None;
            }
            let word = rest.split_whitespace().next()?;
            let word = word.strip_prefix('v').unwrap_or(word);
            word.contains(|c: char| c.is_ascii_digit())
                .then(|| word.to_string())
        })
    })
}

/// The directory tesseract names in its `--list-langs` heading
/// (`List of available languages in "/usr/share/tesseract/tessdata/" (3):`),
/// which a new language pack changes.
pub fn tessdata_dir(text: &str) -> Option<PathBuf> {
    let line = text
        .lines()
        .find(|l| l.contains("List of available languages"))?;
    let from = line.find('"')? + 1;
    let len = line[from..].find('"')?;
    Some(PathBuf::from(&line[from..from + len]))
}

/// tesseract's `--list-langs`: the codes after its heading line, `osd`
/// (orientation and script detection, not a language) left out.
pub fn languages(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .lines()
        .skip_while(|l| !l.contains("List of available languages"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "osd" && !l.contains(' '))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// ffmpeg's `-decoders`: which of [`AUDIO_DECODERS`] it has (present,
/// absent), by its audio decoders' names.
pub fn audio_decoders(text: &str) -> (Vec<String>, Vec<String>) {
    let have: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let flags = w.next()?;
            flags.starts_with('A').then(|| w.next()).flatten()
        })
        .collect();
    let (mut present, mut absent) = (Vec::new(), Vec::new());
    for (name, decoders) in AUDIO_DECODERS {
        if decoders.iter().any(|d| have.contains(d)) {
            present.push(name.to_string());
        } else {
            absent.push(name.to_string());
        }
    }
    (present, absent)
}

/// LibreOffice's parts, by their launchers in `soffice`'s real directory,
/// and those launchers as [`Watched`] files.
fn components(soffice: &Path) -> (Vec<String>, Vec<String>, Vec<Watched>) {
    let dir = std::fs::canonicalize(soffice)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let (mut present, mut absent, mut watched) = (Vec::new(), Vec::new(), Vec::new());
    for (part, launcher) in COMPONENTS {
        let Some(path) = dir.as_ref().map(|d| d.join(exe_name(launcher))) else {
            absent.push(part.to_string());
            continue;
        };
        if path.is_file() {
            present.push(part.to_string());
        } else {
            absent.push(part.to_string());
        }
        watched.push(Watched::file(path));
    }
    (present, absent, watched)
}

/// `dir` as a `file://` URL, for LibreOffice's `-env:UserInstallation`:
/// every byte outside the unreserved set and `/` percent-encoded.
pub(super) fn file_url(dir: &Path) -> String {
    let s = dir.to_string_lossy().replace('\\', "/");
    let mut out = String::from(if s.starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A timeout as the message says it: whole seconds, or milliseconds below
/// one.
fn said(t: Duration) -> String {
    if t.as_millis() < 1000 {
        format!("{} ms", t.as_millis())
    } else {
        format!("{} s", t.as_secs())
    }
}

/// Probe `tool` on `search_path`: [`Fact::Missing`] when its first binary
/// is on no directory, [`Fact::ProbeFailed`] when its version run fails or
/// prints no version, [`Fact::Found`] otherwise.
pub async fn probe(tool: &Tool, search_path: &OsStr) -> Fact {
    probe_within(tool, search_path, TOOL_PROBE_TIMEOUT).await
}

/// [`probe`] with each run's timeout named (tests use a short one).
pub async fn probe_within(tool: &Tool, search_path: &OsStr, timeout: Duration) -> Fact {
    let p = &tool.probe;
    let Some(main) = lookup(p.binaries[0], search_path, p.install_dirs) else {
        return Fact::Missing;
    };
    let failed = |message: String| Fact::ProbeFailed {
        located: main.clone(),
        message,
    };
    let dir = match tempfile::Builder::new().prefix("lmgw-probe-").tempdir() {
        Ok(d) => d,
        Err(e) => return failed(format!("no private directory for the probe: {e}")),
    };
    let mut args: Vec<OsString> = p.version_args.iter().map(OsString::from).collect();
    if p.extras.contains(&Extra::Components) {
        // A throwaway profile, so the probe neither reads nor locks the
        // person's own (design §4.3).
        args.push(
            format!(
                "-env:UserInstallation={}",
                file_url(&dir.path().join("profile"))
            )
            .into(),
        );
    }
    let ran = match run(&main.path, &args, dir.path(), timeout).await {
        Ok(r) => r,
        Err(e) => return failed(e),
    };
    // Some tools print their version on stderr (pdftotext), older ones exit
    // non-zero after it: the version is what counts.
    let both = format!("{}\n{}", ran.stdout, ran.stderr);
    let Some(version) = version_after(&both, p.version_after) else {
        let why = if ran.success {
            "printed no version".to_string()
        } else {
            format!("failed ({})", ran.status)
        };
        return failed(format!(
            "{} {why}: {}",
            main.path.display(),
            first_lines(&both)
        ));
    };
    // pandoc reads untrusted files only sandboxed: one that does not honour
    // `--sandbox` fails its probe, so nothing is read through it (WP7).
    if tool.id == super::caps::ToolId::Pandoc {
        let (bin, cwd) = (main.path.clone(), dir.path().to_path_buf());
        let checked = super::pandoc::check_sandbox(dir.path(), |args| async move {
            run(&bin, &args, &cwd, timeout)
                .await
                .map(|r| (r.stdout, r.stderr, r.success))
        })
        .await;
        if let Err(why) = checked {
            return failed(why);
        }
    }
    let mut extras = Vec::new();
    let mut watched = Vec::new();
    for e in p.extras {
        match e {
            Extra::Binaries => {
                let (mut present, mut absent) = (Vec::new(), Vec::new());
                for b in p.binaries {
                    let w = Watched::binary(b, search_path, p.install_dirs);
                    match &w {
                        Watched::Binary { at: Some(_), .. } => present.push(b.to_string()),
                        _ => absent.push(b.to_string()),
                    }
                    watched.push(w);
                }
                extras.push(ToolExtra {
                    kind: ExtraKind::Binaries,
                    present,
                    absent,
                });
            }
            Extra::Languages => {
                let text =
                    match run(&main.path, &["--list-langs".into()], dir.path(), timeout).await {
                        Ok(r) => format!("{}\n{}", r.stdout, r.stderr),
                        Err(e) => return failed(e),
                    };
                let list = languages(&text);
                watched.extend(tessdata_dir(&text).map(Watched::file));
                // `osd` is no language: it is named only when it is
                // missing (text recognition's page orientation, WP4).
                let osd = text.lines().any(|l| l.trim() == "osd");
                extras.push(ToolExtra {
                    kind: ExtraKind::Languages,
                    present: list,
                    absent: if osd {
                        Vec::new()
                    } else {
                        vec!["osd".to_string()]
                    },
                });
            }
            Extra::AudioDecoders => {
                let args = ["-hide_banner".into(), "-decoders".into()];
                let (present, absent) = match run(&main.path, &args, dir.path(), timeout).await {
                    Ok(r) => audio_decoders(&r.stdout),
                    Err(e) => return failed(e),
                };
                extras.push(ToolExtra {
                    kind: ExtraKind::AudioDecoders,
                    present,
                    absent,
                });
            }
            Extra::Components => {
                let (present, absent, parts) = components(&main.path);
                watched.extend(parts);
                extras.push(ToolExtra {
                    kind: ExtraKind::Components,
                    present,
                    absent,
                });
            }
        }
    }
    Fact::Found(Found {
        located: main,
        version,
        extras,
        watched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_come_from_what_the_real_tools_print() {
        // Printed on 2026-11-18 by Fedora 45's packages (§4.3).
        assert_eq!(
            version_after(
                "pdftotext version 26.08.0\nCopyright 2005-2026 The Poppler Developers",
                "version"
            )
            .as_deref(),
            Some("26.08.0")
        );
        assert_eq!(
            version_after("pandoc 3.7.0.2\nFeatures: +server +lua", "pandoc").as_deref(),
            Some("3.7.0.2")
        );
        assert_eq!(
            version_after(
                "ffmpeg version 9.0.2 Copyright (c) 2000-2026 the FFmpeg developers",
                "version"
            )
            .as_deref(),
            Some("9.0.2")
        );
        assert_eq!(
            version_after("LibreOffice 26.8.2.2 680(Build:2)\n", "LibreOffice").as_deref(),
            Some("26.8.2.2")
        );
        assert_eq!(
            version_after("tesseract v5.5.4\n leptonica-1.85.0", "tesseract").as_deref(),
            Some("5.5.4")
        );
        assert_eq!(
            version_after("pandoc: command not understood", "pandoc"),
            None
        );
    }

    /// Windows' build prints its executable's name.
    #[test]
    fn a_windows_pandoc_names_its_exe_and_the_marker_is_a_whole_word() {
        assert_eq!(
            version_after("pandoc.exe 3.7.0.2\nFeatures: +server +lua", "pandoc").as_deref(),
            Some("3.7.0.2")
        );
        assert_eq!(
            version_after("pandoc 3.7.0.2", "pandoc").as_deref(),
            Some("3.7.0.2")
        );
        assert_eq!(version_after("nopandoc 3.7.0.2", "pandoc"), None);
        assert_eq!(version_after("pandoc.exe", "pandoc"), None);
    }

    #[test]
    fn the_tessdata_directory_is_read_off_the_heading() {
        let langs = "List of available languages in \"/usr/share/tesseract/tessdata/\" (3):\neng\n";
        assert_eq!(
            tessdata_dir(langs),
            Some(PathBuf::from("/usr/share/tesseract/tessdata/"))
        );
        assert_eq!(tessdata_dir("eng\n"), None);
    }

    /// A relative `PATH` entry names a directory under whatever the working
    /// directory is: it finds nothing, where the same directory written
    /// absolutely does.
    #[cfg(unix)]
    #[test]
    fn a_relative_path_entry_is_skipped() {
        let scripts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts");
        assert!(lookup("build-slot.sh", OsStr::new("../../scripts"), &[]).is_none());
        assert!(lookup("build-slot.sh", OsStr::new(scripts), &[]).is_some());
    }

    #[test]
    fn language_packs_and_audio_decoders_are_read_off_the_lists() {
        let langs = "List of available languages in \"/usr/share/tesseract/tessdata/\" (3):\n\
                     eng\nosd\ndeu\n";
        assert_eq!(languages(langs), vec!["deu", "eng"]);

        let decoders = " A....D = Audio\n ------\n V....D h264  H.264\n \
                        A....D aac                  AAC (Advanced Audio Coding)\n \
                        A....D mp3float             MP3 (MPEG audio layer 3)\n \
                        A....D libopus              libopus Opus\n \
                        A....D flac                 FLAC\n";
        let (yes, no) = audio_decoders(decoders);
        assert_eq!(yes, vec!["aac", "mp3", "opus", "flac"]);
        assert_eq!(no, vec!["vorbis", "ac3", "eac3"]);
    }

    #[test]
    fn a_profile_path_becomes_a_file_url() {
        assert_eq!(
            file_url(Path::new("/var/tmp/lmgw-probe-x y/profile")),
            "file:///var/tmp/lmgw-probe-x%20y/profile"
        );
    }

    #[test]
    fn lookup_needs_an_executable_file_on_the_path() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("pdftotext");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        let path = std::env::join_paths([d.path()]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(lookup("pdftotext", &path, &[]).is_none(), "not executable");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let found = lookup("pdftotext", &path, &[]).unwrap();
        assert_eq!(found.path, p);
        assert!(lookup("pdftoppm", &path, &[]).is_none());
        assert!(lookup("pdftotext", OsStr::new(""), &[]).is_none());
    }
}
