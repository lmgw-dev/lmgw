//! The store's half (WP1): `store::chat_profiles` on a migrated in-memory
//! database, and the snapshot it loads.

use lmgw_core::config::chat_profile::{
    create_kind, field, normalise_patch, CreateKind, Example, ProfileCreate, ProfileDraft,
    ProfilePatch, ProfileRefusal, ProfileVoice, Reasoning,
};
use lmgw_core::state::AppState;
use lmgw_core::store::chat_profiles::{self as profiles, builtin::CONCISE};
use lmgw_core::store::{self, AdminThreads, ChatFolderPatch, SeedWrite, ThreadDefaults, ThreadMcp};
use sqlx::SqlitePool;

async fn db() -> SqlitePool {
    store::open_in_memory().await.unwrap()
}

/// The seeded built-in's id.
async fn concise_id(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT id FROM chat_profiles WHERE builtin = 'concise'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn create(pool: &SqlitePool, name: &str, persona: &str) -> i64 {
    let kind = create_kind(&ProfileCreate {
        name: Some(name.into()),
        persona: persona.into(),
        ..Default::default()
    })
    .unwrap();
    profiles::create_chat_profile(pool, &kind)
        .await
        .unwrap()
        .unwrap()
        .id
}

async fn body_of(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT body FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn patch(
    pool: &SqlitePool,
    id: i64,
    mut p: ProfilePatch,
) -> Result<lmgw_core::config::ChatProfile, ProfileRefusal> {
    normalise_patch(&mut p)?;
    profiles::update_chat_profile(pool, id, &p).await.unwrap()
}

/// A thread with profile `profile` set through the settings write.
async fn thread_with(pool: &SqlitePool, profile: Option<i64>) -> i64 {
    let id = store::create_chat_thread(pool, "m", "chat").await.unwrap();
    let mut t = store::get_chat_thread(pool, id).await.unwrap().unwrap();
    t.profile_id = profile;
    store::update_chat_thread_settings(pool, &t, SeedWrite::Keep, None)
        .await
        .unwrap();
    id
}

async fn profile_of(pool: &SqlitePool, thread: i64) -> Option<i64> {
    store::get_chat_thread(pool, thread)
        .await
        .unwrap()
        .unwrap()
        .profile_id
}

async fn feed_since(pool: &SqlitePool, seq: i64) -> Vec<(String, Option<i64>, Option<i64>)> {
    sqlx::query_as("SELECT type, thread_id, folder_id FROM chat_feed WHERE seq > ?1 ORDER BY seq")
        .bind(seq)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn feed_head(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM chat_feed")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_profile_is_created_read_listed_changed_and_deleted() {
    let pool = db().await;
    let kind = create_kind(&ProfileCreate {
        name: Some("  Calm ".into()),
        persona: "Speak calmly.".into(),
        length_rule: "Two sentences.".into(),
        examples: vec![Example {
            user: "Hi".into(),
            reply: "Hello there.".into(),
        }],
        voice_block: Some(String::new()),
        reasoning: Some(Reasoning::On),
        voice: ProfileVoice {
            tts_alias: Some("speak".into()),
            voice: Some("alba".into()),
            speech_style: None,
        },
        ..Default::default()
    })
    .unwrap();
    let p = profiles::create_chat_profile(&pool, &kind)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.name, "Calm");
    assert_eq!(p.builtin, None);
    assert_eq!(p.persona, "Speak calmly.");
    assert_eq!(p.voice_block.as_deref(), Some(""));
    assert_eq!(p.reasoning, Some(Reasoning::On));
    assert_eq!(p.voice.voice.as_deref(), Some("alba"));
    assert!(p.follows_builtin.is_empty());
    assert_eq!(
        profiles::get_chat_profile(&pool, p.id).await.unwrap(),
        Some(p.clone())
    );

    // Listed in name order, the seeded built-in included.
    create(&pool, "aardvark", "").await;
    let names: Vec<String> = profiles::list_chat_profiles(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["aardvark", "Calm", "Concise"]);

    // Absent stays, null unsets.
    let changed = patch(
        &pool,
        p.id,
        ProfilePatch {
            length_rule: Some(None),
            voice_block: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(changed.persona, "Speak calmly.");
    assert_eq!(changed.length_rule, "");
    assert_eq!(changed.voice_block, None);
    assert_eq!(changed.examples.len(), 1);

    let gone = profiles::delete_chat_profile(&pool, p.id, AdminThreads::Shown, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gone.deleted, p.id);
    assert_eq!(profiles::get_chat_profile(&pool, p.id).await.unwrap(), None);
    assert_eq!(
        profiles::delete_chat_profile(&pool, p.id, AdminThreads::Shown, None)
            .await
            .unwrap(),
        Err(ProfileRefusal::NotFound(p.id))
    );
    assert_eq!(
        patch(&pool, p.id, ProfilePatch::default()).await,
        Err(ProfileRefusal::NotFound(p.id))
    );
}

#[tokio::test]
async fn a_name_is_taken_in_any_case_and_default_is_reserved() {
    let pool = db().await;
    let calm = create(&pool, "Ärger", "").await;
    let again = create_kind(&ProfileCreate {
        name: Some("äRGER".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        profiles::create_chat_profile(&pool, &again).await.unwrap(),
        Err(ProfileRefusal::NameTaken("Ärger".into()))
    );
    // The built-in's name is a name like any other.
    assert_eq!(
        patch(
            &pool,
            calm,
            ProfilePatch {
                name: Some("CONCISE".into()),
                ..Default::default()
            }
        )
        .await,
        Err(ProfileRefusal::NameTaken("Concise".into()))
    );
    // Its own name in another case is no collision.
    let renamed = patch(
        &pool,
        calm,
        ProfilePatch {
            name: Some("ärger".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(renamed.name, "ärger");
    for name in ["default", " Default ", "DEFAULT"] {
        let refused = create_kind(&ProfileCreate {
            name: Some(name.into()),
            ..Default::default()
        });
        assert_eq!(refused, Err(ProfileRefusal::NameReserved), "{name}");
        assert_eq!(
            patch(
                &pool,
                calm,
                ProfilePatch {
                    name: Some(name.into()),
                    ..Default::default()
                }
            )
            .await,
            Err(ProfileRefusal::NameReserved)
        );
    }
}

#[tokio::test]
async fn the_builtin_follows_its_texts_until_one_is_written() {
    let pool = db().await;
    let id = concise_id(&pool).await;
    let p = profiles::get_chat_profile(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.name, "Concise");
    assert_eq!(p.builtin.as_deref(), Some("concise"));
    assert_eq!(p.persona, CONCISE.persona);
    assert_eq!(p.length_rule, CONCISE.length_rule);
    assert_eq!(p.examples.len(), CONCISE.examples.len());
    assert_eq!(p.voice_block, None);
    assert_eq!(p.reasoning, Some(Reasoning::Off));
    assert!(p.voice.is_empty());
    assert_eq!(p.follows_builtin, field::FOLLOWABLE.map(String::from));

    // A text of its own is stored, and that field no longer follows.
    let p = patch(
        &pool,
        id,
        ProfilePatch {
            length_rule: Some(Some("One sentence.".into())),
            reasoning: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(p.length_rule, "One sentence.");
    assert_eq!(p.reasoning, None);
    assert_eq!(
        p.follows_builtin,
        [field::PERSONA, field::EXAMPLES, field::VOICE_BLOCK]
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body_of(&pool, id).await).unwrap(),
        serde_json::json!({"length_rule": "One sentence.", "reasoning": null})
    );

    // Writing the built-in text back stores it absent: it follows again.
    let p = patch(
        &pool,
        id,
        ProfilePatch {
            length_rule: Some(Some(format!("  {}\n", CONCISE.length_rule))),
            reasoning: Some(Some(Reasoning::Off)),
            persona: Some(Some(CONCISE.persona.into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(p.follows_builtin, field::FOLLOWABLE.map(String::from));
    assert_eq!(body_of(&pool, id).await, "{}");

    // Reset: every field follows again, the name stays.
    patch(
        &pool,
        id,
        ProfilePatch {
            name: Some("Short".into()),
            persona: Some(Some("Mine.".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p = profiles::reset_chat_profile(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.name, "Short");
    assert_eq!(p.persona, CONCISE.persona);
    let own = create(&pool, "Own", "x").await;
    assert_eq!(
        profiles::reset_chat_profile(&pool, own).await.unwrap(),
        Err(ProfileRefusal::NotBuiltin(own))
    );
}

#[tokio::test]
async fn a_builtin_is_created_again_only_after_its_delete() {
    let pool = db().await;
    let id = concise_id(&pool).await;
    let again = CreateKind::Builtin("concise".into());
    assert_eq!(
        profiles::create_chat_profile(&pool, &again).await.unwrap(),
        Err(ProfileRefusal::BuiltinExists("concise".into()))
    );
    assert_eq!(
        profiles::create_chat_profile(&pool, &CreateKind::Builtin("chatty".into()))
            .await
            .unwrap(),
        Err(ProfileRefusal::UnknownBuiltin("chatty".into()))
    );
    profiles::delete_chat_profile(&pool, id, AdminThreads::Shown, None)
        .await
        .unwrap()
        .unwrap();
    let p = profiles::create_chat_profile(&pool, &again)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(p.id, id, "a deleted profile's id is never handed out again");
    assert_eq!(p.name, "Concise");
    assert_eq!(p.persona, CONCISE.persona);
    assert_eq!(body_of(&pool, p.id).await, "{}");
}

#[tokio::test]
async fn a_body_a_newer_build_wrote_reads_key_by_key() {
    let pool = db().await;
    let id = create(&pool, "Calm", "Speak calmly.").await;
    sqlx::query(
        r#"UPDATE chat_profiles SET body = '{"persona": "Speak calmly.", "mood": "calm",
             "reasoning": "sometimes", "examples": "not a list",
             "voice": {"voice": "alba", "pitch": 2}}' WHERE id = ?1"#,
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let p = profiles::get_chat_profile(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.persona, "Speak calmly.");
    assert_eq!(p.reasoning, None);
    assert!(p.examples.is_empty());
    assert_eq!(p.voice.voice.as_deref(), Some("alba"));
    // A body that is not JSON at all is an empty one, and the row lists.
    sqlx::query("UPDATE chat_profiles SET body = 'oops' WHERE id = ?1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let listed = profiles::list_chat_profiles(&pool).await.unwrap();
    assert!(listed.iter().any(|p| p.id == id && p.persona.is_empty()));
}

#[tokio::test]
async fn a_delete_clears_threads_folders_and_the_default_in_one_go() {
    let pool = db().await;
    let calm = create(&pool, "Calm", "Speak calmly.").await;
    let other = create(&pool, "Other", "").await;
    let a = thread_with(&pool, Some(calm)).await;
    let b = thread_with(&pool, Some(calm)).await;
    let keep = thread_with(&pool, Some(other)).await;
    // An Admin Chat thread a device does not see.
    let admin = store::create_chat_thread(&pool, "m", "admin")
        .await
        .unwrap();
    let mut t = store::get_chat_thread(&pool, admin).await.unwrap().unwrap();
    t.profile_id = Some(calm);
    store::update_chat_thread_settings(&pool, &t, SeedWrite::Keep, None)
        .await
        .unwrap();
    let defaults = |p: i64| ThreadDefaults {
        profile_id: Some(p),
        temperature: Some(0.5),
        ..Default::default()
    };
    let f1 = store::create_chat_folder(&pool, "Assistant", &defaults(calm), None)
        .await
        .unwrap();
    let f2 = store::create_chat_folder(&pool, "Else", &defaults(other), None)
        .await
        .unwrap();
    // A folder whose defaults attach the self-admin toolset: out of a plain
    // device's reach.
    let f3 = store::create_chat_folder(
        &pool,
        "Tools",
        &ThreadDefaults {
            mcp_tools: Some(vec![ThreadMcp {
                server_label: "lmgw".into(),
                allowed_tools: None,
                require_approval: None,
            }]),
            ..defaults(calm)
        },
        None,
    )
    .await
    .unwrap();
    store::set_kv(
        &pool,
        "settings",
        &serde_json::json!({"chat_profile": calm, "chat_archive_days": 3}).to_string(),
    )
    .await
    .unwrap();

    let used = profiles::chat_profiles_used_by(&pool, AdminThreads::Shown)
        .await
        .unwrap();
    assert_eq!(used[&calm].threads, 3);
    assert_eq!(
        used[&calm].folders.iter().map(|f| f.id).collect::<Vec<_>>(),
        [f1, f3]
    );
    let seen = profiles::chat_profiles_used_by(&pool, AdminThreads::Hidden)
        .await
        .unwrap();
    assert_eq!(seen[&calm].threads, 2);
    assert_eq!(seen[&calm].folders.len(), 1);

    let head = feed_head(&pool).await;
    let gone = profiles::delete_chat_profile(&pool, calm, AdminThreads::Hidden, None)
        .await
        .unwrap()
        .unwrap();
    // Counted as far as the reader reaches; everything is cleared.
    assert_eq!(gone.threads_cleared, 2);
    assert_eq!(gone.folders_cleared.len(), 1);
    assert_eq!(gone.folders_cleared[0].id, f1);
    assert_eq!(gone.folders_cleared[0].name, "Assistant");
    assert!(gone.default_cleared);
    for t in [a, b, admin] {
        assert_eq!(profile_of(&pool, t).await, None);
    }
    assert_eq!(profile_of(&pool, keep).await, Some(other));
    for f in [f1, f3] {
        let d = store::get_chat_folder(&pool, f)
            .await
            .unwrap()
            .unwrap()
            .defaults;
        assert_eq!(d.profile_id, None);
        assert_eq!(d.temperature, Some(0.5), "the other defaults stay");
    }
    let d = store::get_chat_folder(&pool, f2)
        .await
        .unwrap()
        .unwrap()
        .defaults;
    assert_eq!(d.profile_id, Some(other));
    let settings: serde_json::Value =
        serde_json::from_str(&store::get_kv(&pool, "settings").await.unwrap().unwrap()).unwrap();
    assert_eq!(settings, serde_json::json!({"chat_archive_days": 3}));

    // Each change is in the feed, recorded in the delete's transaction.
    let mut threads: Vec<i64> = Vec::new();
    let mut folders: Vec<i64> = Vec::new();
    for (kind, thread, folder) in feed_since(&pool, head).await {
        match kind.as_str() {
            "thread.updated" => threads.push(thread.unwrap()),
            "folder.updated" => folders.push(folder.unwrap()),
            other => panic!("unexpected {other}"),
        }
    }
    let mut want = vec![a, b, admin];
    want.sort_unstable();
    assert_eq!(threads, want);
    assert_eq!(folders, [f1, f3]);

    // The default naming another profile, or stored as a numeric text, is
    // judged by its value.
    store::set_kv(&pool, "settings", r#"{"chat_profile": "99"}"#)
        .await
        .unwrap();
    let gone = profiles::delete_chat_profile(&pool, other, AdminThreads::Shown, None)
        .await
        .unwrap()
        .unwrap();
    assert!(!gone.default_cleared);
    assert_eq!(gone.threads_cleared, 1);
}

#[tokio::test]
async fn a_refused_patch_changes_nothing() {
    let pool = db().await;
    let calm = create(&pool, "Calm", "x").await;
    let before = body_of(&pool, calm).await;
    let refused = patch(
        &pool,
        calm,
        ProfilePatch {
            name: Some("Concise".into()),
            persona: Some(Some("changed".into())),
            ..Default::default()
        },
    )
    .await;
    assert!(refused.is_err());
    assert_eq!(body_of(&pool, calm).await, before);
}

#[tokio::test]
async fn threads_take_a_profile_from_their_folder_and_keep_it() {
    let pool = db().await;
    let calm = create(&pool, "Calm", "x").await;
    let folder = store::create_chat_folder(
        &pool,
        "F",
        &ThreadDefaults {
            profile_id: Some(calm),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    let defaults = store::get_chat_folder(&pool, folder)
        .await
        .unwrap()
        .unwrap()
        .defaults;
    assert_eq!(defaults.profile_id, Some(calm));
    let mut t = store::ChatThread {
        model_alias: "m".into(),
        kind: "chat".into(),
        folder_id: Some(folder),
        ..Default::default()
    };
    defaults.apply(&mut t);
    let id = store::create_chat_thread_from(&pool, &t, None)
        .await
        .unwrap();
    assert_eq!(profile_of(&pool, id).await, Some(calm));

    // Keep: a temporary thread's profile comes along.
    t.id = -3;
    let kept = store::insert_kept_chat_thread(&pool, &t, &[], &[], None)
        .await
        .unwrap();
    assert_eq!(profile_of(&pool, kept).await, Some(calm));

    // A folder patch that names a profile carries it in its defaults.
    store::update_chat_folder(
        &pool,
        folder,
        &ChatFolderPatch {
            defaults: Some(ThreadDefaults::default()),
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    let d = store::get_chat_folder(&pool, folder)
        .await
        .unwrap()
        .unwrap()
        .defaults;
    assert_eq!(d.profile_id, None);
}

#[tokio::test]
async fn a_thread_write_naming_a_gone_profile_writes_none() {
    let pool = db().await;
    let calm = create(&pool, "Calm", "x").await;
    let id = thread_with(&pool, Some(calm)).await;
    // A copy read before the delete, written after it.
    let stale = store::get_chat_thread(&pool, id).await.unwrap().unwrap();
    profiles::delete_chat_profile(&pool, calm, AdminThreads::Shown, None)
        .await
        .unwrap()
        .unwrap();
    store::update_chat_thread_settings(&pool, &stale, SeedWrite::Keep, None)
        .await
        .unwrap();
    assert_eq!(profile_of(&pool, id).await, None);
    // The same for a new thread and a kept one (a temporary thread holds
    // its profile in memory, where a delete does not reach it).
    let made = store::create_chat_thread_from(&pool, &stale, None)
        .await
        .unwrap();
    assert_eq!(profile_of(&pool, made).await, None);
    let kept = store::insert_kept_chat_thread(&pool, &stale, &[], &[], None)
        .await
        .unwrap();
    assert_eq!(profile_of(&pool, kept).await, None);
}

#[tokio::test]
async fn the_snapshot_holds_every_profile_after_a_reload() {
    let state = AppState::init_for_tests().await.unwrap();
    let snap = state.snapshot();
    assert_eq!(snap.chat_profiles.len(), 1);
    let concise = snap.chat_profile_named("concise").unwrap();
    assert_eq!(concise.persona, CONCISE.persona);
    let id = create(&state.db, "Calm", "Speak calmly.").await;
    assert!(state.snapshot().chat_profile(id).is_none(), "not before");
    state.reload_snapshot().await.unwrap();
    let snap = state.snapshot();
    assert_eq!(snap.chat_profile(id).unwrap().persona, "Speak calmly.");
    assert_eq!(
        snap.chat_profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["Calm", "Concise"]
    );
    // A draft of a stored profile is its content fields.
    assert_eq!(
        snap.chat_profile(id).unwrap().draft(),
        ProfileDraft {
            persona: "Speak calmly.".into(),
            ..Default::default()
        }
    );
}
