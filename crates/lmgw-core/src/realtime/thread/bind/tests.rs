//! A folder's decision between `check` and `bind` (review of calls WP2,
//! should-fix 1): a kept current thread that a delete, a move or an archive
//! — writes that take no folder lock — ends after the check is not bound;
//! the bind decides the folder again and binds what it picks now.

use super::super::check::{check, Target};
use super::*;
use crate::config::ApiKeyKind;
use crate::principal::Principal;
use crate::state::AppState;
use crate::store::{self, FolderOptions, ThreadDefaults};

/// The owner, as an owner key presents it.
fn owner() -> RequestCtx {
    RequestCtx {
        principal: Principal::Key {
            id: 1,
            name: "owner".into(),
            kind: ApiKeyKind::Owner,
            agent_id: None,
            fingerprint: String::new(),
        },
        ..Default::default()
    }
}

/// An ongoing folder on `chatty`: its id.
async fn folder(state: &SharedState) -> i64 {
    let defaults = ThreadDefaults {
        model_alias: Some("chatty".into()),
        ..Default::default()
    };
    let opts = FolderOptions {
        ongoing_idle_minutes: Some(30),
        ..Default::default()
    };
    store::create_chat_folder_with(&state.db, "Assistant", &defaults, &opts, &[], None)
        .await
        .unwrap()
}

/// The thread `outcome` bound, and its bind.
fn bound(outcome: Outcome) -> (i64, Box<Bind>) {
    match outcome {
        Outcome::Bound(b) => (b.binding.thread_id, b),
        Outcome::Pending(p) => panic!("pending on {}", p.held_by),
    }
}

/// The folder's current thread, as stored.
async fn current(state: &SharedState, id: i64) -> Option<i64> {
    store::get_chat_folder(&state.db, id)
        .await
        .unwrap()
        .and_then(|f| f.current_thread_id())
}

#[tokio::test]
async fn a_kept_thread_that_left_the_folder_after_the_check_is_decided_again() {
    let state = AppState::init_for_tests().await.unwrap();
    let ctx = owner();
    let id = folder(&state).await;
    let target = Target::Folder { id, new: false };

    // The first bind makes the folder's thread.
    let checked = check(&state, &ctx, target, false, Takeover::Always)
        .await
        .unwrap();
    let (first, b) = bound(bind(&state, &ctx, checked, Takeover::Always).await.unwrap());
    assert_eq!(current(&state, id).await, Some(first));
    drop(b);

    // Checked with the first thread kept; archived before the bind.
    let checked = check(&state, &ctx, target, false, Takeover::Always)
        .await
        .unwrap();
    assert_eq!(checked.thread.id, first, "kept at the check");
    store::archive_chat_thread(&state.db, first, None)
        .await
        .unwrap();
    let (second, b) = bound(bind(&state, &ctx, checked, Takeover::Always).await.unwrap());
    assert_ne!(second, first, "not the archived thread");
    assert_eq!(current(&state, id).await, Some(second));
    drop(b);

    // Checked with the second kept; deleted before the bind: decided again,
    // not refused as a thread that is not there.
    let checked = check(&state, &ctx, target, false, Takeover::Always)
        .await
        .unwrap();
    assert_eq!(checked.thread.id, second);
    store::delete_chat_thread(&state.db, second, None)
        .await
        .unwrap();
    let (third, b) = bound(bind(&state, &ctx, checked, Takeover::Always).await.unwrap());
    assert!(third != first && third != second);
    assert_eq!(current(&state, id).await, Some(third));
    drop(b);

    // A kept thread still current is bound as it is.
    let checked = check(&state, &ctx, target, false, Takeover::Always)
        .await
        .unwrap();
    let (again, _b) = bound(bind(&state, &ctx, checked, Takeover::Always).await.unwrap());
    assert_eq!(again, third);
}

/// A move out of the folder clears its pointer as a delete does: the bind
/// that checked before it binds the folder's next thread, and the moved one
/// stays where it went.
#[tokio::test]
async fn a_kept_thread_moved_out_after_the_check_is_not_bound() {
    let state = AppState::init_for_tests().await.unwrap();
    let ctx = owner();
    let id = folder(&state).await;
    let elsewhere = store::create_chat_folder(&state.db, "Elsewhere", &Default::default(), None)
        .await
        .unwrap();
    let target = Target::Folder { id, new: false };
    let checked = check(&state, &ctx, target, false, Takeover::Always)
        .await
        .unwrap();
    let (first, b) = bound(bind(&state, &ctx, checked, Takeover::Always).await.unwrap());
    drop(b);

    let checked = check(&state, &ctx, target, false, Takeover::Always)
        .await
        .unwrap();
    assert_eq!(checked.thread.id, first);
    store::set_chat_thread_folder(&state.db, first, Some(elsewhere), None)
        .await
        .unwrap();
    let (next, _b) = bound(bind(&state, &ctx, checked, Takeover::Always).await.unwrap());
    assert_ne!(next, first);
    assert_eq!(current(&state, id).await, Some(next));
    let moved = store::get_chat_thread(&state.db, first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.folder_id, Some(elsewhere));
}
