//! The scorer's own rules (fix package B6), with a stand-in for Smart Turn:
//! too little audio is not scored, and a request a newer one overtook while
//! it waited for the model is answered without running it.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use super::*;
use crate::state::AppState;

/// `ms` of 16 kHz audio, every sample `mark` (what the stand-in sees).
fn job(id: u64, ms: u32, mark: f32) -> ScoreJob {
    ScoreJob {
        id,
        samples: vec![mark; (ms * SAMPLE_RATE / 1000) as usize],
        pause_ms: 0,
    }
}

async fn next(rx: &mut mpsc::UnboundedReceiver<ScoreDone>) -> ScoreDone {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("an answer within 10 s")
        .expect("the scorer still sends")
}

#[tokio::test]
async fn too_little_audio_is_not_scored() {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_turn_score_for_tests(Some(Arc::new(|_: &[f32]| Ok(0.9))));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let scorer = Scorer::new(state, tx);
    scorer.score(job(0, MIN_SCORED_MS - 10, 0.0));
    let done = next(&mut rx).await;
    assert_eq!(done.id, 0);
    assert_eq!(
        done.result,
        Err(Unscored::Short(u64::from(MIN_SCORED_MS) - 10))
    );
    // An empty span, as a ring with nothing kept answers.
    scorer.score(job(1, 0, 0.0));
    assert_eq!(next(&mut rx).await.result, Err(Unscored::Short(0)));
    // At the bound it is scored.
    scorer.score(job(2, MIN_SCORED_MS, 0.0));
    assert_eq!(next(&mut rx).await.result, Ok(0.9));
}

#[tokio::test]
async fn a_request_overtaken_while_it_waited_is_not_scored() {
    let state = AppState::init_for_tests().await.unwrap();
    // The first score (its audio marked 1.0) holds the model until released.
    let (release, held) = std_mpsc::channel::<()>();
    let held = Mutex::new(held);
    let (started_tx, started) = std_mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    state.set_turn_score_for_tests(Some(Arc::new(move |audio: &[f32]| {
        if audio[0] == 1.0 {
            let _ = started_tx.lock().unwrap().send(());
            let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }
        Ok(f32::from(audio[0] == 3.0) * 0.5 + 0.25)
    })));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let scorer = Scorer::new(state, tx);
    scorer.score(job(5, 500, 1.0));
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the first score runs");
    // The voice resumed and paused twice while it ran.
    scorer.score(job(6, 500, 2.0));
    scorer.score(job(7, 500, 3.0));
    release.send(()).unwrap();
    let mut answers = Vec::new();
    for _ in 0..3 {
        let done = next(&mut rx).await;
        answers.push((done.id, done.result));
    }
    answers.sort_by_key(|(id, _)| *id);
    assert_eq!(
        answers,
        [(5, Ok(0.25)), (6, Err(Unscored::Overtaken)), (7, Ok(0.75)),]
    );
}
