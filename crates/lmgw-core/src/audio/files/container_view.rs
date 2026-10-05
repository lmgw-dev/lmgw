//! What audio.cpp, inside its container, finds at a path of the models dir:
//! the rule lmgw counts a row's GGUFs by.
//!
//! An audio container mounts the models dir at `/models` (read-only) and
//! the row's config dir at `/config`, and nothing else of the host
//! (`runtime/argv.rs`). A regular file is the same file on both sides. A
//! symlink is followed by the kernel inside the container, against the
//! container's paths — a relative target from the link's directory, an
//! absolute one from the container's root — so following it on the host
//! can reach a file audio.cpp never sees. lmgw resolves a path the way the
//! container does instead, component by component and link by link, with
//! `/models` standing for the models dir:
//! - a path that stays under `/models` all the way is the file it reaches
//!   on the host ([`Seen::File`] when that is a regular file);
//! - an absolute target under `/models/` resolves in the container, though
//!   it dangles on the host, and is followed there the same way;
//! - a target that leaves `/models` — an absolute host path (a Hugging Face
//!   cache blob, `/home/…/models/x.gguf` spelled out), or a relative one
//!   climbing above the models dir — is outside the mount: in the container
//!   lmgw starts it is a dangling link, which audio.cpp does not count
//!   ([`Seen::Outside`]). The row's own run args can mount more, and lmgw
//!   does not read them, so it cannot be sure: such a link is not counted,
//!   and the row's problems name it ([`links_outside`],
//!   `lmgw__local_model_get`) rather than leaving the difference unsaid —
//!   a row directory linked out of the mount as well, under which no GGUF
//!   counts.
//!
//! More than 40 links on one path is the kernel's `ELOOP`: nothing.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Where the container mounts the models dir.
const MOUNT: &str = "/models";
/// The kernel's limit on links followed for one path (`MAXSYMLINKS`).
const MAX_LINKS: u32 = 40;

/// What the container finds at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// A regular file — at this host path, once every link is followed the
    /// container's way.
    File(PathBuf),
    /// Nothing, a directory, or a link dangling inside `/models`.
    NotFile,
    /// A link whose target (this one) leaves the `/models` mount.
    Outside(PathBuf),
}

/// What the container finds at `host`, a path under `models_dir` (module
/// doc). A path not under `models_dir` at all is [`Seen::Outside`].
pub fn seen(models_dir: &Path, host: &Path) -> Seen {
    let Ok(rel) = host.strip_prefix(models_dir) else {
        return Seen::Outside(host.to_path_buf());
    };
    let mut todo: VecDeque<Component<'_>> = rel.components().collect();
    // Owned once a link's target is spliced in.
    let mut owned: VecDeque<OsString> = VecDeque::new();
    let mut at: Vec<OsString> = Vec::new();
    let mut links = 0u32;
    let mut last_target: Option<PathBuf> = None;
    loop {
        let part: OsString = match owned.pop_front() {
            Some(p) => p,
            None => match todo.pop_front() {
                Some(Component::Normal(n)) => n.to_os_string(),
                Some(Component::ParentDir) => "..".into(),
                Some(_) => continue,
                None => break,
            },
        };
        if part == "." || part.is_empty() {
            continue;
        }
        if part == ".." {
            if at.pop().is_none() {
                return Seen::Outside(last_target.unwrap_or_else(|| host.to_path_buf()));
            }
            continue;
        }
        at.push(part);
        let here = join(models_dir, &at);
        let Ok(meta) = std::fs::symlink_metadata(&here) else {
            return Seen::NotFile;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        links += 1;
        let Ok(target) = std::fs::read_link(&here) else {
            return Seen::NotFile;
        };
        if links > MAX_LINKS {
            return Seen::NotFile;
        }
        at.pop();
        let rest: Vec<OsString> = if target.is_absolute() {
            let Ok(under) = target.strip_prefix(MOUNT) else {
                return Seen::Outside(target);
            };
            at.clear();
            parts(under)
        } else {
            parts(&target)
        };
        for p in rest.into_iter().rev() {
            owned.push_front(p);
        }
        last_target = Some(target);
    }
    let here = join(models_dir, &at);
    match std::fs::symlink_metadata(&here) {
        Ok(m) if m.is_file() => Seen::File(here),
        _ => Seen::NotFile,
    }
}

/// The links on the way to a row's GGUFs whose target leaves the `/models`
/// mount: `(name, target)`, sorted. What the row's problems name.
///
/// The row's path itself first: when it leaves on its way — `root` is a
/// link out, a GGUF's or the directory's, or sits under one — the container
/// finds nothing there at all, and every GGUF under it reads as outside
/// without being a link of its own. That one is named, by its path under
/// the models dir (with a `/` after a directory), and nothing else is.
/// Otherwise the top-level GGUF links of `root` that leave.
pub fn links_outside(models_dir: &Path, root: &Path) -> Vec<(String, PathBuf)> {
    if let Seen::Outside(target) = seen(models_dir, root) {
        let at = root.strip_prefix(models_dir).unwrap_or(root).display();
        let name = match std::fs::metadata(root).is_ok_and(|m| m.is_dir()) {
            true => format!("{at}/"),
            false => at.to_string(),
        };
        return vec![(name, target)];
    }
    let gguf = |p: &Path| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().ends_with(".gguf"))
    };
    let paths: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    let mut out: Vec<(String, PathBuf)> = paths
        .into_iter()
        .filter(|p| gguf(p))
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        .filter_map(|p| match seen(models_dir, &p) {
            Seen::Outside(target) => Some((p.file_name()?.to_string_lossy().into_owned(), target)),
            _ => None,
        })
        .collect();
    out.sort();
    out
}

fn join(models_dir: &Path, at: &[OsString]) -> PathBuf {
    let mut p = models_dir.to_path_buf();
    p.extend(at);
    p
}

/// A link target's components, `..` included, as the walk takes them.
fn parts(p: &Path) -> Vec<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn file(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn a_link_is_followed_as_the_container_follows_it() {
        let models = tempfile::tempdir().unwrap();
        let m = models.path();
        let outside = tempfile::tempdir().unwrap();
        file(&m.join("blobs/a"));
        file(&m.join("row/plain.gguf"));
        file(&outside.path().join("blob"));
        let row = m.join("row");
        // Relative, inside the models dir: the same file on both sides.
        symlink("../blobs/a", row.join("rel.gguf")).unwrap();
        // Absolute under /models: dangles on the host, resolves inside.
        symlink("/models/blobs/a", row.join("mount.gguf")).unwrap();
        // The host's own path, spelled out: not there in the container.
        symlink(m.join("blobs/a"), row.join("host.gguf")).unwrap();
        // A blob outside the models dir, and a climb out of it.
        symlink(outside.path().join("blob"), row.join("cache.gguf")).unwrap();
        symlink("../../elsewhere/x", row.join("climb.gguf")).unwrap();
        // A link to a link, and one dangling inside.
        symlink("rel.gguf", row.join("chain.gguf")).unwrap();
        symlink("../blobs/gone", row.join("gone.gguf")).unwrap();
        // A directory link inside, then down through it.
        symlink("blobs", m.join("store")).unwrap();
        symlink("../store/a", row.join("via-dir.gguf")).unwrap();

        let at = |n: &str| seen(m, &row.join(n));
        assert_eq!(at("plain.gguf"), Seen::File(row.join("plain.gguf")));
        assert_eq!(at("rel.gguf"), Seen::File(m.join("blobs/a")));
        assert_eq!(at("mount.gguf"), Seen::File(m.join("blobs/a")));
        assert_eq!(at("chain.gguf"), Seen::File(m.join("blobs/a")));
        assert_eq!(at("via-dir.gguf"), Seen::File(m.join("blobs/a")));
        assert_eq!(at("host.gguf"), Seen::Outside(m.join("blobs/a")));
        assert_eq!(at("cache.gguf"), Seen::Outside(outside.path().join("blob")));
        assert_eq!(
            at("climb.gguf"),
            Seen::Outside(PathBuf::from("../../elsewhere/x"))
        );
        assert_eq!(at("gone.gguf"), Seen::NotFile);
        assert_eq!(at("missing.gguf"), Seen::NotFile);
        assert_eq!(seen(m, &row), Seen::NotFile, "a directory");

        let named: Vec<String> = links_outside(m, &row).into_iter().map(|(n, _)| n).collect();
        assert_eq!(named, ["cache.gguf", "climb.gguf", "host.gguf"]);
    }

    /// A row directory that is itself a link out of the models dir, or sits
    /// under one: every GGUF in it is outside to the container, though none
    /// is a link, and the directory's link is what is named. One linked
    /// inside the models dir is no problem.
    #[test]
    fn a_row_directory_linked_out_of_the_mount_is_named() {
        let models = tempfile::tempdir().unwrap();
        let m = models.path();
        let outside = tempfile::tempdir().unwrap();
        file(&outside.path().join("Row-GGUF/x-q8_0.gguf"));
        file(&outside.path().join("Row-GGUF/x-f16.gguf"));
        file(&m.join("store/Row-GGUF/x-q8_0.gguf"));
        symlink(outside.path().join("Row-GGUF"), m.join("row")).unwrap();
        symlink(outside.path(), m.join("vendor")).unwrap();
        symlink("store/Row-GGUF", m.join("inside")).unwrap();

        let row = m.join("row");
        assert_eq!(
            seen(m, &row.join("x-q8_0.gguf")),
            Seen::Outside(outside.path().join("Row-GGUF"))
        );
        assert_eq!(
            links_outside(m, &row),
            [("row/".to_string(), outside.path().join("Row-GGUF"))]
        );
        assert_eq!(
            links_outside(m, &m.join("vendor/Row-GGUF")),
            [("vendor/Row-GGUF/".to_string(), outside.path().to_path_buf())]
        );
        assert_eq!(
            links_outside(m, &m.join("vendor/Row-GGUF/x-f16.gguf")),
            [(
                "vendor/Row-GGUF/x-f16.gguf".to_string(),
                outside.path().to_path_buf()
            )],
            "a row whose path is a GGUF under such a link"
        );
        assert!(links_outside(m, &m.join("inside")).is_empty());
    }

    #[test]
    fn a_link_loop_is_nothing() {
        let models = tempfile::tempdir().unwrap();
        let m = models.path();
        std::fs::create_dir(m.join("row")).unwrap();
        symlink("b.gguf", m.join("row/a.gguf")).unwrap();
        symlink("a.gguf", m.join("row/b.gguf")).unwrap();
        assert_eq!(seen(m, &m.join("row/a.gguf")), Seen::NotFile);
    }
}
