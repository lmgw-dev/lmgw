use super::*;
use crate::realtime::audio::resample::tests::stream;
use crate::realtime::audio::resample::StreamResampler;
use crate::realtime::test_fixtures::{f32le, path, wav_24k};

/// A number field of a fixture sidecar (no JSON dependency for two keys).
fn json_number(name: &str, key: &str) -> f32 {
    let text = std::fs::read_to_string(path(name)).expect("fixture present");
    let at = text.find(&format!("\"{key}\":")).expect("key present") + key.len() + 3;
    let rest = text[at..].trim_start();
    let end = rest.find([',', '\n', '}']).expect("value ends");
    rest[..end].trim().parse().expect("a number")
}

fn reference(name: &str) -> f32 {
    json_number(
        &format!("smartturn_{name}.json"),
        "smart_turn_v3_2_cpu_probability",
    )
}

/// The reference features through our ORT session reproduce the Python
/// onnxruntime probability (realtime §22: onnxruntime 1.30). The output is
/// int8: ORT 1.28 lands one logit step (0.0394) lower on
/// `en_complete_short`, 8.4e-4 in p at 0.978. Near p 0.5 one step is
/// ~0.01, so this tolerance only holds for confident clips.
#[test]
fn matches_reference_probability_on_reference_features() {
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, 1).unwrap();
    for name in ["en_complete_short", "en_midsentence_pause"] {
        let features = f32le(&format!("smartturn_{name}.features_f32le.bin"));
        let (got, want) = (st.probability(&features).unwrap(), reference(name));
        eprintln!("{name}: reference features p {got:.6} (Python {want:.6})");
        assert!((got - want).abs() <= 1e-3, "{name}: {got} vs {want}");
    }
}

/// Our own features from the same audio land on the same probability.
#[test]
fn own_features_reproduce_the_probability() {
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, 1).unwrap();
    for name in ["en_complete_short", "en_midsentence_pause"] {
        let audio = f32le(&format!("smartturn_{name}.audio16k_f32le.bin"));
        let (got, want) = (st.score(&audio).unwrap(), reference(name));
        eprintln!("{name}: own features p {got:.6} (Python {want:.6})");
        assert!((got - want).abs() <= 1e-3, "{name}: {got} vs {want}");
    }
}

/// The whole Rust path from the 24 kHz WAV (rubato instead of soxr) at
/// the sidecars' cut points. Smart Turn is weak on TTS speech (§6.3), so
/// only the two clear cases are asserted; the rest is printed for
/// INTEGRATION.md.
#[test]
fn whole_rust_path_on_the_wavs() {
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, 1).unwrap();
    let mut at = |wav: &str, cut_ms: usize| {
        let x = wav_24k(wav);
        let y = stream(&mut StreamResampler::new().unwrap(), &x[..cut_ms * 24]);
        st.score(&y).unwrap()
    };
    let complete = at("en_complete_short.wav", 1738);
    let paused = at("en_midsentence_pause.wav", 2688);
    let at_pause = at("en_midsentence_pause.wav", 2488);
    let sentence_end = at("en_midsentence_pause.wav", 4399);
    eprintln!(
        "rubato path: complete_short@1738 {complete:.4}, midsentence@2488 (pause start) \
         {at_pause:.4}, @2688 (+200 ms) {paused:.4}, @4399 (sentence end) {sentence_end:.4}"
    );
    assert!(complete >= 0.5 && sentence_end >= 0.5, "complete turns");
    assert!(paused < 0.5, "mid-sentence pause");
}

#[test]
fn rejects_bad_input() {
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, 1).unwrap();
    assert_eq!(
        st.probability(&[0.0; 10]),
        Err(SmartTurnError::FeatureLength(10))
    );
    let mut f = vec![0.0; FEATURES];
    f[3] = f32::INFINITY;
    assert_eq!(st.probability(&f), Err(SmartTurnError::NonFinite));
    assert_eq!(
        st.score(&[0.1, f32::NAN, 0.2]),
        Err(SmartTurnError::NonFinite)
    );
    // A NaN older than the last 8 s does not count.
    let mut long = vec![f32::NAN];
    long.extend(std::iter::repeat_n(
        0.0,
        crate::realtime::turn::mel::WINDOW_SAMPLES,
    ));
    assert!(st.score(&long).is_ok());
    assert!(matches!(
        SmartTurn::from_bytes(b"not a model", 1),
        Err(SmartTurnError::Ort(_))
    ));
}

/// Empty and silent turns score without error (a probability, whatever
/// it is; the caller's policy decides).
#[test]
fn silence_scores() {
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, 2).unwrap();
    for audio in [&[][..], &[0.0; 16_000][..]] {
        let p = st.score(audio).unwrap();
        assert!((0.0..=1.0).contains(&p), "{p}");
    }
}
