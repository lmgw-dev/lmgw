//! The media relay (speech, transcription, images): a client that goes
//! while the engine sends it nothing is heard at once, and the model's
//! claim goes with it.

use super::*;

#[tokio::test]
async fn speech_the_engine_is_still_working_on_stops_when_its_client_goes() {
    let g = Gpu::new(24 * GIB, 0, 30).await;
    g.audio("m", "tts", 4 * GIB).await;
    let (port, model) = model(Says::Silence).await;
    g.next_start_on(port);
    let gw = common::serve(g.state.clone()).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/audio/speech"))
        .json(&json!({"model": "audio/m", "input": "Hallo.", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    drop(resp);
    model_stops(&model, Instant::now()).await;
    claim_released(&g).await;
    let row = newest_row_of(&g.state, "audio/m").await;
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
}
