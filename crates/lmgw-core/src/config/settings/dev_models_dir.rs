//! A dev instance never writes into a models directory outside its own data
//! dir (the owner's ruling of 2026-10-04 on chat-voice WP11 review m6).
//!
//! A dev copy (`scripts/dev-copy.sh copy`) keeps production's absolute models
//! dirs, because it needs them to run the owner's models, and every container
//! mounts them read-only. What lmgw itself writes there is another matter: a
//! download (and its retry, update or re-download), the delete of a tracked
//! download, a voice-library clip, its transcript, an image start's LoRA and
//! upscaler dirs. On a copy all of these land in the installed app's tree,
//! including the `.part` files its own downloads resume into. One predicate,
//! [`dev_models_dir_refusal`], is asked at every one of those write sites.
//!
//! Two ways stay open on purpose, because each needs a deliberate step of the
//! owner's: a row's own extra run args (a mount of their choosing), and
//! re-enabling a copied agent, whose read-write mounts are production paths.
//! `scripts/dev-copy.sh copy` names both.

use std::path::{Component, Path, PathBuf};

/// The stable code of [`dev_models_dir_refusal`]'s message. The message ends
/// with it in parentheses, the way a mount refusal carries its code, so a
/// front door that only has the string still answers with the code
/// ([`dev_models_dir_code`]).
pub const DEV_SHARED_MODELS_DIR: &str = "dev_shared_models_dir";

/// Why a dev instance must not write to `target`, when it must not: `target`
/// lies outside `data_dir`, compared as resolved paths (symlinks followed as
/// far as the path exists, so another spelling or a link into production's
/// tree is the same place). `None` for production, whatever the path, and for
/// a target inside the instance's own data dir.
///
/// Built like [`super::dev_prefix_refusal`]: the rule and the way out in one
/// sentence, and nothing asked of the filesystem beyond resolving the paths.
pub fn dev_models_dir_refusal(dev: bool, data_dir: &Path, target: &Path) -> Option<String> {
    if !dev || resolved(target).starts_with(resolved(data_dir)) {
        return None;
    }
    Some(format!(
        "this is a dev instance, and {} is outside its data dir {}: a dev instance does not \
         write into a models directory outside its own data dir, because that directory may be \
         the installed app's. To test this here, point the copy's models dir at a folder of its \
         own inside its data dir. ({DEV_SHARED_MODELS_DIR})",
        target.display(),
        data_dir.display()
    ))
}

/// The code a refusal message carries, when it is [`dev_models_dir_refusal`]'s.
pub fn dev_models_dir_code(message: &str) -> Option<&'static str> {
    message
        .trim_end()
        .strip_suffix(')')
        .and_then(|m| m.strip_suffix(DEV_SHARED_MODELS_DIR))
        .is_some_and(|m| m.ends_with('('))
        .then_some(DEV_SHARED_MODELS_DIR)
}

/// `path` as the filesystem resolves it: absolute, canonical as far as it
/// exists, and the part that does not exist yet appended as written, with `.`
/// and `..` folded. A models dir a download is about to create has no
/// canonical form yet, and refusing to compare it would let it through.
fn resolved(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    // Whether `out` exists and is canonical: then a `..` is its real parent.
    let mut real = true;
    for comp in abs.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if real {
                    match out.canonicalize() {
                        Ok(c) => out = c,
                        Err(_) => real = false,
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_is_never_refused() {
        let d = tempfile::tempdir().unwrap();
        assert!(dev_models_dir_refusal(false, d.path(), Path::new("/srv/models")).is_none());
    }

    #[test]
    fn a_dev_instance_writes_only_inside_its_own_data_dir() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let outside = root.path().join("models");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Inside, existing or not yet.
        assert!(dev_models_dir_refusal(true, &data, &data.join("models")).is_none());
        assert!(dev_models_dir_refusal(true, &data, &data).is_none());
        // Outside, and a `..` that only looks inside.
        let why = dev_models_dir_refusal(true, &data, &outside).unwrap();
        assert!(why.contains("outside its data dir"), "{why}");
        assert!(why.contains("folder of its own"), "{why}");
        assert_eq!(dev_models_dir_code(&why), Some(DEV_SHARED_MODELS_DIR));
        assert!(dev_models_dir_refusal(true, &data, &data.join("x/../../models")).is_some());
        // A sibling whose name only starts like the data dir is outside.
        assert!(dev_models_dir_refusal(true, &data, &root.path().join("data2")).is_some());
    }

    #[test]
    fn a_link_into_another_tree_is_resolved() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let prod = root.path().join("prod-models");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&prod).unwrap();
        let link = data.join("models");
        std::os::unix::fs::symlink(&prod, &link).unwrap();
        assert!(dev_models_dir_refusal(true, &data, &link).is_some());
        assert!(dev_models_dir_refusal(true, &data, &link.join("voices")).is_some());
        // And a data dir reached through a link is still its own.
        let data_link = root.path().join("data-link");
        std::os::unix::fs::symlink(&data, &data_link).unwrap();
        assert!(dev_models_dir_refusal(true, &data_link, &data.join("m")).is_none());
    }

    #[test]
    fn only_this_refusal_carries_the_code() {
        assert_eq!(dev_models_dir_code("download failed"), None);
        assert_eq!(
            dev_models_dir_code("x (not_dev_shared_models_dir)"),
            None,
            "the code is the whole word in the parentheses"
        );
        assert_eq!(
            dev_models_dir_code("… (dev_shared_models_dir) "),
            Some(DEV_SHARED_MODELS_DIR)
        );
    }
}
