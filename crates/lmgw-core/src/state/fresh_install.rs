//! What a fresh install starts with beyond the type defaults: a models
//! directory for each of the four classes, under the data dir, so the first
//! **Add from Hugging Face → Download** needs no setting.
//!
//! Each class's `models_dir` is empty in its type default, and an empty one
//! means "nowhere" for every class alike: a download into it is refused
//! (`hf::models_dir_or_refuse`), a container start is refused (the
//! registry's "models dir is not configured"), and the audio class's voice
//! library has no place to keep a clip. Audio's empty value does not mean
//! "the image's own catalog": the audio.cpp catalog is fetched from its spec
//! repo whatever the dir says, and the image ships no models. So all four get
//! a default, `<data_dir>/models/<class>` (`chat`, `aux`, `audio`, `image`,
//! the download targets' names; on Windows the installed app's are under its
//! local dir, `super::local_data_dir`), each its own directory, because a file
//! stays updatable only where the downloader put it and a GGUF is never
//! shared between class dirs.
//!
//! Only a **new** data dir gets them: one whose database did not exist before
//! this start opened it. They are stored as ordinary settings, so Settings →
//! Runtimes shows and changes them like any value the owner typed, and an
//! existing install — whatever its settings say, including none saved yet —
//! is never touched. A dev instance's data dir is its own, so the defaults
//! lie inside it and its rule against writing outside it
//! (`dev_models_dir_refusal`) lets the downloads through — except when that
//! data dir is in RAM ([`seed_skipped`]): there it gets none, and a download
//! is refused until a models dir is chosen. Nothing about the seed fails the
//! start ([`open_db`]).
//!
//! The settings whose empty value has a meaning of its own keep it:
//! `builds_dir` unset is this instance's default builds dir, `voice_dir` is a
//! container path under the audio dir already.

use std::path::{Path, PathBuf};

use lmgw_api_types::settings_labels::{MODELS_DIR, RUNTIMES};
use sqlx::SqlitePool;

use crate::config::deployment::Deployment;
use crate::config::Settings;
use crate::hf::TARGETS;
use crate::store;

/// The database file in a data dir.
pub const DB_FILE: &str = "lmgw.sqlite";

/// The directory under the data dir a fresh install's models dirs sit in.
pub const MODELS_SUBDIR: &str = "models";

/// The models dir a fresh install gives download target `target`:
/// `<local>/models/<target>`, `<local>` the data dir's local dir
/// ([`super::local_data_dir`]: the data dir itself on Linux,
/// `%LOCALAPPDATA%\lmgw` for the installed app on Windows).
pub fn default_models_dir(data_dir: &Path, target: &str) -> PathBuf {
    super::local_data_dir(data_dir)
        .join(MODELS_SUBDIR)
        .join(target)
}

/// The class settings field behind a download target, as
/// [`crate::hf::models_dir_for_target`] reads it.
fn models_dir_mut<'a>(s: &'a mut Settings, target: &str) -> &'a mut String {
    match target {
        "aux" => &mut s.aux_router.models_dir,
        "audio" => &mut s.audio.models_dir,
        "image" => &mut s.image.models_dir,
        _ => &mut s.router.models_dir,
    }
}

/// Give every class whose models dir is empty its fresh-install default, and
/// answer the directories set.
pub fn fill_models_dirs(s: &mut Settings, data_dir: &Path) -> Vec<PathBuf> {
    let mut set = Vec::new();
    for target in TARGETS {
        let dir = models_dir_mut(s, target);
        if dir.trim().is_empty() {
            let path = default_models_dir(data_dir, target);
            *dir = path.display().to_string();
            set.push(path);
        }
    }
    set
}

/// Whether `data_dir` is new: it holds no database yet. Asked before
/// [`open_db`] creates one; a database that cannot be looked for counts as
/// there, so nothing is seeded over a dir this start cannot read.
fn is_new_data_dir(data_dir: &Path) -> bool {
    !data_dir.join(DB_FILE).try_exists().unwrap_or(true)
}

/// Open `data_dir`'s database (creating and migrating it as
/// [`store::open`] does), and when it did not exist before, store the
/// settings a fresh install starts with ([`fill_models_dirs`]) and create
/// their directories — unless [`seed_skipped`] says why not (`dev`: this is
/// a dev instance).
///
/// Nothing about the seed fails the start: settings that cannot be read or
/// stored, or a directory that cannot be created, is a warning. A class
/// whose models dir stays empty then refuses a download by where to set it
/// ([`crate::hf::models_dir_or_refuse`]).
///
/// In container mode a database with a pending migration is copied into
/// the data dir's `backups/` first ([`store::migration_copy`], headless
/// server design ruling 14); the desktop makes no copy.
pub async fn open_db(
    data_dir: &Path,
    dev: bool,
    deployment: Deployment,
) -> anyhow::Result<SqlitePool> {
    open_db_judged(data_dir, dev, deployment, crate::backends::paths::is_tmpfs).await
}

/// [`open_db`], asking whether the data dir is in RAM through `in_ram`
/// instead of [`crate::backends::paths::is_tmpfs`], so a test can answer it
/// on any filesystem. `pub` (hidden) for the integration tests.
#[doc(hidden)]
pub async fn open_db_judged(
    data_dir: &Path,
    dev: bool,
    deployment: Deployment,
    in_ram: fn(&Path) -> bool,
) -> anyhow::Result<SqlitePool> {
    // Before `open` creates it.
    let fresh = is_new_data_dir(data_dir);
    let backups = deployment
        .is_container()
        .then(|| store::backups_dir(data_dir));
    let db = store::open_for_start(&data_dir.join(DB_FILE), backups.as_deref()).await?;
    if !fresh {
        return Ok(db);
    }
    if let Some(why) = seed_skipped(dev, in_ram(data_dir)) {
        tracing::info!(
            "fresh install: no models dirs under {}: {why}. A download is refused until one is \
             chosen (Settings → {RUNTIMES} → each class's {MODELS_DIR})",
            data_dir.display()
        );
        return Ok(db);
    }
    if let Err(e) = seed(&db, data_dir).await {
        tracing::warn!(
            "fresh install: {e}; the start goes on without them, and a download is refused \
             until a models dir is chosen (Settings → {RUNTIMES} → each class's {MODELS_DIR})"
        );
    }
    Ok(db)
}

/// Why a new data dir gets no models dirs, when it gets none: a dev
/// instance's data dir in RAM. `scripts/dev-instance.sh` makes one with
/// `mktemp -d -p /tmp`, and `/tmp` is a tmpfs on many distributions (Fedora's default)
/// ([`crate::backends::paths`]), so a first download into a models dir
/// under it would write gigabytes of weights into memory. A production data
/// dir is where its owner put it and always gets them, and so does a dev
/// instance's on disk.
pub fn seed_skipped(dev: bool, data_dir_in_ram: bool) -> Option<&'static str> {
    (dev && data_dir_in_ram).then_some(
        "this dev instance's data dir is on tmpfs, i.e. in RAM, and a download into a models \
         dir under it would keep its weights (gigabytes) in memory",
    )
}

async fn seed(db: &SqlitePool, data_dir: &Path) -> anyhow::Result<()> {
    let mut s = store::load_settings(db)
        .await
        .map_err(|e| anyhow::anyhow!("reading a new database's settings: {e}"))?;
    let set = fill_models_dirs(&mut s, data_dir);
    if set.is_empty() {
        return Ok(());
    }
    store::save_settings(db, &s)
        .await
        .map_err(|e| anyhow::anyhow!("storing a fresh install's models dirs: {e}"))?;
    for dir in &set {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                "fresh install: could not create the models dir {}: {e} (a download creates it)",
                dir.display()
            );
        }
    }
    tracing::info!(
        "fresh install: models are stored under {} (one directory per class; change them under \
         Settings → {RUNTIMES})",
        super::local_data_dir(data_dir)
            .join(MODELS_SUBDIR)
            .display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<data>/models/<class>` written with this system's separator
    /// (`/srv/lmgw-data/models/chat` on Linux).
    fn models(data: &Path, class: &str) -> String {
        data.join("models").join(class).display().to_string()
    }

    #[test]
    fn every_class_gets_its_own_dir_under_the_data_dir() {
        let mut s = Settings::default();
        let data = Path::new("/srv/lmgw-data");
        let set = fill_models_dirs(&mut s, data);
        assert_eq!(set.len(), 4);
        assert_eq!(s.router.models_dir, models(data, "chat"));
        assert_eq!(s.aux_router.models_dir, models(data, "aux"));
        assert_eq!(s.audio.models_dir, models(data, "audio"));
        assert_eq!(s.image.models_dir, models(data, "image"));
        for t in TARGETS {
            assert_eq!(
                crate::hf::models_dir_for_target(&s, t),
                default_models_dir(data, t).display().to_string(),
                "{t}: the field the downloader reads"
            );
        }
    }

    #[test]
    fn only_a_dev_instance_in_ram_goes_without() {
        assert!(seed_skipped(true, true).is_some_and(|why| why.contains("tmpfs")));
        assert_eq!(seed_skipped(true, false), None, "a dev data dir on disk");
        assert_eq!(
            seed_skipped(false, true),
            None,
            "production's, wherever it is"
        );
        assert_eq!(seed_skipped(false, false), None);
    }

    #[test]
    fn a_dir_already_set_is_kept() {
        let mut s = Settings::default();
        s.audio.models_dir = "/mnt/voices".into();
        let set = fill_models_dirs(&mut s, Path::new("/d"));
        assert_eq!(set.len(), 3);
        assert_eq!(s.audio.models_dir, "/mnt/voices");
        assert_eq!(s.router.models_dir, models(Path::new("/d"), "chat"));
    }
}
