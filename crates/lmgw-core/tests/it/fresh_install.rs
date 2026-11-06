//! A fresh install's models dirs (`state::fresh_install`): a new data dir
//! gets one per class under itself, stored as ordinary settings, so a first
//! download is not refused — through the gateway's own boot too; a data dir
//! that already has a database keeps what it has, settings saved or not; the
//! settings whose empty value means something keep it; and a dev instance
//! whose data dir is in RAM gets none, so its download is refused by the
//! setting's label instead of writing weights into memory.

use std::path::Path;
use std::sync::Arc;

use lmgw_core::config::{dev_models_dir_refusal, Settings};
use lmgw_core::hf::{self, TARGETS};
use lmgw_core::state::fresh_install::{self, default_models_dir};
use lmgw_core::state::AppState;
use lmgw_core::store;
use lmgw_core::vram::nvml::NoTelemetry;

use crate::support::decision_world::RecordingPodman;

fn json(s: &Settings) -> serde_json::Value {
    serde_json::to_value(s).unwrap()
}

#[tokio::test]
async fn a_new_data_dir_gets_a_models_dir_per_class_under_itself() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("lmgw");
    let db = fresh_install::open_db(&data, false).await.unwrap();
    let s = store::load_settings(&db).await.unwrap();
    for t in TARGETS {
        let want = default_models_dir(&data, t);
        assert_eq!(
            hf::models_dir_for_target(&s, t),
            want.display().to_string(),
            "{t}"
        );
        assert!(want.is_dir(), "{t}: created");
        // The download's own check, and a dev instance's rule against
        // writing outside its data dir: both let it through.
        assert!(hf::models_dir_or_refuse(&s, t).is_ok(), "{t}");
        assert_eq!(dev_models_dir_refusal(true, &data, &want), None, "{t}");
    }
    let dirs: std::collections::BTreeSet<String> = TARGETS
        .iter()
        .map(|t| hf::models_dir_for_target(&s, t))
        .collect();
    assert_eq!(dirs.len(), 4, "no two classes share a dir");
    // Stored, not derived: the settings row holds them, as a saved value.
    let raw: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(
        raw.contains(&default_models_dir(&data, "audio").display().to_string()),
        "{raw}"
    );
    // The settings whose empty value means something keep it.
    let d = Settings::default();
    assert_eq!(s.builds_dir, None, "unset is this instance's builds dir");
    assert_eq!(s.audio.voice_dir, d.audio.voice_dir);
    assert_eq!(s.container_prefix, d.container_prefix);
    db.close().await;

    // The next start is not a fresh install any more, and changes nothing.
    let db = fresh_install::open_db(&data, false).await.unwrap();
    assert_eq!(json(&store::load_settings(&db).await.unwrap()), json(&s));
}

#[tokio::test]
async fn an_existing_install_keeps_its_settings() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let saved = {
        let db = store::open(&data.join(fresh_install::DB_FILE))
            .await
            .unwrap();
        let mut s = store::load_settings(&db).await.unwrap();
        s.router.models_dir = "/srv/models.example/chat".into();
        store::save_settings(&db, &s).await.unwrap();
        db.close().await;
        s
    };
    let db = fresh_install::open_db(&data, false).await.unwrap();
    let s = store::load_settings(&db).await.unwrap();
    assert_eq!(json(&s), json(&saved));
    assert!(
        s.aux_router.models_dir.is_empty(),
        "an empty dir stays empty"
    );
    assert!(!data.join(fresh_install::MODELS_SUBDIR).exists());
}

#[tokio::test]
async fn an_existing_database_without_saved_settings_stays_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    store::open(&data.join(fresh_install::DB_FILE))
        .await
        .unwrap()
        .close()
        .await;
    let db = fresh_install::open_db(&data, false).await.unwrap();
    let row: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
            .fetch_optional(&db)
            .await
            .unwrap();
    assert_eq!(row, None, "nothing was written");
    let s = store::load_settings(&db).await.unwrap();
    for t in TARGETS {
        assert!(hf::models_dir_for_target(&s, t).is_empty(), "{t}");
    }
    assert!(!Path::new(&data).join(fresh_install::MODELS_SUBDIR).exists());
}

/// The gateway's own start on a new data dir: the same four dirs, stored
/// in the snapshot every route reads and created on disk.
#[tokio::test]
async fn the_boot_of_a_new_data_dir_stores_and_creates_the_four_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("lmgw");
    // Production's start (`dev` false): a dev instance's builds dir is
    // outside the scratch dir, and this one's data dir may be on tmpfs.
    let state = AppState::init_with_host(
        data.clone(),
        false,
        Arc::new(RecordingPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
        // A machine with nothing on it: no driver, no `nvidia-ctk`, and no
        // `XDG_RUNTIME_DIR`, so nothing here writes outside the data dir.
        Arc::new(lmgw_machine::fake::FakeFacts::new()),
    )
    .await
    .unwrap();
    let s = &state.snapshot().settings;
    let stored = store::load_settings(&state.db).await.unwrap();
    for t in TARGETS {
        let want = default_models_dir(&state.data_dir, t).display().to_string();
        assert_eq!(
            hf::models_dir_for_target(s, t),
            want,
            "{t}: in the snapshot"
        );
        assert_eq!(hf::models_dir_for_target(&stored, t), want, "{t}: stored");
        assert!(Path::new(&want).is_dir(), "{t}: created");
    }
}

/// A dev instance's data dir in RAM gets no models dirs, and its download
/// is refused by where to choose one; on disk, or for production's start,
/// the dirs are there as ever.
#[tokio::test]
async fn a_dev_instance_in_ram_gets_no_models_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("dev-in-ram");
    let db = fresh_install::open_db_judged(&data, true, |_| true)
        .await
        .unwrap();
    let row: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
            .fetch_optional(&db)
            .await
            .unwrap();
    assert_eq!(row, None, "nothing was stored");
    assert!(
        !data.join(fresh_install::MODELS_SUBDIR).exists(),
        "nor created"
    );
    let s = store::load_settings(&db).await.unwrap();
    for t in TARGETS {
        let e = hf::models_dir_or_refuse(&s, t).unwrap_err();
        assert!(e.contains(&hf::models_dir_location(t)), "{t}: {e}");
    }
    db.close().await;

    for (name, dev, in_ram) in [("dev-on-disk", true, false), ("prod-in-ram", false, true)] {
        let data = dir.path().join(name);
        let judge: fn(&Path) -> bool = if in_ram { |_| true } else { |_| false };
        let db = fresh_install::open_db_judged(&data, dev, judge)
            .await
            .unwrap();
        let s = store::load_settings(&db).await.unwrap();
        for t in TARGETS {
            assert_eq!(
                hf::models_dir_for_target(&s, t),
                default_models_dir(&data, t).display().to_string(),
                "{name}: {t}"
            );
        }
        db.close().await;
    }
}

/// The same rule through the real question: a dev instance's new data dir
/// on this box's tmpfs (`/dev/shm`, when it is one) gets no models dirs.
#[tokio::test]
async fn a_dev_data_dir_on_tmpfs_is_asked_for_real() {
    let shm = Path::new("/dev/shm");
    if !lmgw_core::backends::paths::is_tmpfs(shm) {
        return; // no tmpfs to put a data dir on, on this box
    }
    let dir = tempfile::tempdir_in(shm).unwrap();
    let db = fresh_install::open_db(dir.path(), true).await.unwrap();
    let s = store::load_settings(&db).await.unwrap();
    for t in TARGETS {
        assert!(hf::models_dir_for_target(&s, t).is_empty(), "{t}");
    }
    assert!(!dir.path().join(fresh_install::MODELS_SUBDIR).exists());
    db.close().await;
}
