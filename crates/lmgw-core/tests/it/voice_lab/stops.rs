//! A round that is stopped — its caller gone (the page's Stop, the page
//! left, an MCP client that hung up) — or discarded while it renders.

use std::time::Duration;

use lmgw_api_types::voice_lab::VoiceGenerateArgs;
use lmgw_core::state::SharedState;
use lmgw_core::voice_lab::{generate, lab_dir, rounds};
use serde_json::json;

use super::{get, lab_world_answering_after, post};
use crate::support::audio_world::World;

/// What each candidate takes the container stand-in.
const SLOW: Duration = Duration::from_millis(300);

fn asked(count: u32) -> VoiceGenerateArgs {
    VoiceGenerateArgs {
        alias: "audio/design".into(),
        language: Some("en".into()),
        description: Some("a warm, calm voice".into()),
        count: Some(count),
        seed: Some(1),
        ..Default::default()
    }
}

type Row = (i64, Option<String>, Option<i64>);

/// The design alias's `request_logs` rows (status, error kind, characters
/// sent), once there are `n`.
async fn rows(state: &SharedState, n: usize) -> Vec<Row> {
    for _ in 0..500 {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT status, error_kind, chars_in FROM request_logs \
             WHERE requested_alias = 'audio/design' ORDER BY id",
        )
        .fetch_all(&state.db)
        .await
        .unwrap();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fewer than {n} rows for audio/design");
}

/// The id of the round rendering in `w` once its first candidate is in.
async fn rendering(w: &World) -> String {
    let lab = lab_dir(&w.state);
    for _ in 0..500 {
        let l = lab.clone();
        let list = tokio::task::spawn_blocking(move || rounds::list(&l))
            .await
            .unwrap();
        if let Some(r) = list.rounds.first() {
            if !r.candidates.is_empty() {
                assert!(rounds::is_active(&lab, &r.id));
                return r.id.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no round started rendering");
}

#[tokio::test]
async fn a_round_whose_caller_went_away_stops_and_still_writes_its_row() {
    let w = lab_world_answering_after(SLOW).await;
    let state = w.state.clone();
    // The caller: the HTTP handler or the MCP tool, either way this future.
    let call = tokio::spawn(async move { generate::generate(&state, &asked(50)).await });
    let id = rendering(&w).await;
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());

    // The round ends at its next await and writes its one row: a stop is a
    // 200 `canceled`, as a realtime answer's.
    let row = rows(&w.state, 1).await;
    assert_eq!(row.len(), 1, "{row:?}");
    assert_eq!(row[0].0, 200);
    assert_eq!(row[0].1.as_deref(), Some("canceled"));
    // The candidate in flight at the stop may be in the provider's hands:
    // its characters, and so the round's, are unknown rather than short.
    // (A stop that lands between two candidates leaves them known.)
    assert!(row[0].2.is_none_or(|c| c > 0), "{row:?}");

    // And the round is kept with what it made, saying it was stopped.
    let lab = lab_dir(&w.state);
    let mut round = None;
    for _ in 0..500 {
        if !rounds::is_active(&lab, &id) {
            round = Some(rounds::read_round(&lab, &id).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let round = round.expect("the round ended");
    let stopped = round.stopped.as_deref().unwrap();
    assert!(stopped.starts_with("stopped ("), "{stopped}");
    assert!(!round.candidates.is_empty() && round.candidates.len() < 50);
    assert!(
        w.sent().await.len() < 50,
        "nothing more is sent after the stop"
    );
}

#[tokio::test]
async fn a_round_discarded_while_it_renders_stops_first_and_stays_gone() {
    let w = lab_world_answering_after(SLOW).await;
    let state = w.state.clone();
    let call = tokio::spawn(async move { generate::generate(&state, &asked(50)).await });
    let id = rendering(&w).await;

    let (status, left) = post(&w, &format!("/voice-lab/api/rounds/{id}/delete"), json!({})).await;
    assert_eq!(status, 200, "{left}");
    assert_eq!(left["rounds"], json!([]));
    let lab = lab_dir(&w.state);
    assert!(!rounds::is_active(&lab, &id), "stopped before it went");

    // The renderer has ended and answered with what it made; nothing it
    // wrote after the stop brought the directory back.
    let made = call.await.unwrap().unwrap();
    assert!(made.stopped.is_some());
    tokio::time::sleep(SLOW * 2).await;
    assert!(!rounds::dir(&lab).join(&id).exists());
    let (_, list) = get(&w, "/voice-lab/api/rounds").await;
    assert_eq!(list["rounds"], json!([]));
    let row = rows(&w.state, 1).await;
    assert_eq!(row[0].1.as_deref(), Some("canceled"), "{row:?}");
}

#[tokio::test]
async fn clear_stops_a_rendering_round_and_takes_the_orphans_it_counts() {
    let w = lab_world_answering_after(SLOW).await;
    let lab = lab_dir(&w.state);
    // A directory with no readable round.json: counted, and Clear's.
    let orphan = rounds::dir(&lab).join("20260101-000000-000-abcdef");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("1.wav"), vec![0u8; 1000]).unwrap();
    let (_, list) = get(&w, "/voice-lab/api/rounds").await;
    assert_eq!(list["rounds"], json!([]));
    assert_eq!(list["bytes"], 1000);

    let state = w.state.clone();
    let call = tokio::spawn(async move { generate::generate(&state, &asked(50)).await });
    let id = rendering(&w).await;
    let (status, left) = post(&w, "/voice-lab/api/rounds/clear", json!({})).await;
    assert_eq!(status, 200, "{left}");
    assert_eq!(left["rounds"], json!([]));
    assert_eq!(left["bytes"], 0);
    assert!(!orphan.exists());
    assert!(call.await.unwrap().unwrap().stopped.is_some());
    tokio::time::sleep(SLOW * 2).await;
    assert!(!rounds::dir(&lab).join(&id).exists());
}
