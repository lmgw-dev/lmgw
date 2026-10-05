use super::*;
use crate::realtime::test_fixtures::f32le;

/// Max and mean |a - b|.
fn diff(a: &[f32], b: &[f32]) -> (f32, f64) {
    assert_eq!(a.len(), b.len());
    let max = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    let mean = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from((x - y).abs()))
        .sum::<f64>()
        / a.len() as f64;
    (max, mean)
}

/// The exact `audio16k` inputs through our front end against
/// `WhisperFeatureExtractor(chunk_length=8, do_normalize=True)` on the
/// same left-padded window (realtime §13 parity test).
#[test]
fn matches_the_python_extractor() {
    let fe = MelFrontEnd::new();
    for name in ["en_complete_short", "en_midsentence_pause"] {
        let audio = f32le(&format!("smartturn_{name}.audio16k_f32le.bin"));
        let want = f32le(&format!("smartturn_{name}.features_f32le.bin"));
        assert_eq!(want.len(), FEATURES, "{name}: fixture shape");
        let got = fe.features(&audio);
        let (max, mean) = diff(&got, &want);
        eprintln!("{name}: features max |d| {max:.3e}, mean |d| {mean:.3e}");
        assert!(max <= 1e-3, "{name}: max |d| {max}");
    }
}

#[test]
fn shared_front_end_matches_an_own_one() {
    let audio = f32le("smartturn_en_complete_short.audio16k_f32le.bin");
    assert_eq!(
        whisper_features(&audio),
        MelFrontEnd::new().features(&audio)
    );
}

/// Shorter turns are left-padded and longer ones keep their last 8 s, so
/// explicit padding or a longer prefix changes nothing.
#[test]
fn window_is_the_last_8_s_left_padded() {
    let fe = MelFrontEnd::new();
    let audio = f32le("smartturn_en_midsentence_pause.audio16k_f32le.bin");
    let mut padded = vec![0.0; WINDOW_SAMPLES - audio.len()];
    padded.extend_from_slice(&audio);
    assert_eq!(fe.features(&audio), fe.features(&padded));
    let mut longer: Vec<f32> = (0..50_000).map(|i| (i as f32 * 0.01).sin()).collect();
    longer.extend_from_slice(&padded);
    assert_eq!(fe.features(&longer), fe.features(&padded));
}

/// Empty and all-zero input stay finite (the 1e-7 epsilon and the 1e-10
/// floor): every value is the floor, (log10(1e-10) + 4) / 4.
#[test]
fn silence_is_finite_and_flat() {
    let fe = MelFrontEnd::new();
    for audio in [&[][..], &[0.0; 1000][..], &[0.0; WINDOW_SAMPLES + 7][..]] {
        let f = fe.features(audio);
        assert_eq!(f.len(), FEATURES);
        assert!(f.iter().all(|&v| v == -1.5), "{:?}", &f[..4]);
    }
}

/// A constant (DC) window is all zeros after normalisation; a full-scale
/// sine stays finite and puts its maximum in the right mel bin.
#[test]
fn tones_land_in_their_mel_bin() {
    let fe = MelFrontEnd::new();
    let tone: Vec<f32> = (0..WINDOW_SAMPLES)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 16_000.0).sin())
        .collect();
    let f = fe.features(&tone);
    assert!(f.iter().all(|v| v.is_finite()));
    let mid = FRAMES / 2;
    let loudest = (0..N_MELS)
        .max_by(|&a, &b| f[a * FRAMES + mid].total_cmp(&f[b * FRAMES + mid]))
        .unwrap();
    // 1000 Hz is mel 15; the centres are 81 equal steps up to mel(8000).
    let centre = |m: usize| mel_to_hz((m + 1) as f64 * hz_to_mel(8000.0) / 81.0);
    assert!(
        (centre(loudest) - 1000.0).abs() < 60.0,
        "loudest bin {loudest} centred at {} Hz",
        centre(loudest)
    );
}

#[test]
fn hann_is_periodic() {
    let w = periodic_hann(N_FFT);
    assert_eq!(w.len(), N_FFT);
    assert_eq!(w[0], 0.0);
    assert!((w[N_FFT / 2] - 1.0).abs() < 1e-15);
    for k in 1..N_FFT {
        assert!((w[k] - w[N_FFT - k]).abs() < 1e-15, "k {k}");
    }
}

#[test]
fn reflect_mirrors_without_repeating_the_edge() {
    assert_eq!(reflect(0), 200);
    assert_eq!(reflect(199), 1);
    assert_eq!(reflect(200), 0);
    assert_eq!(reflect(200 + WINDOW_SAMPLES - 1), WINDOW_SAMPLES - 1);
    assert_eq!(reflect(200 + WINDOW_SAMPLES), WINDOW_SAMPLES - 2);
    // The last sample the last kept frame reads.
    let last = (FRAMES - 1) * HOP + N_FFT - 1;
    assert_eq!(reflect(last), 2 * (WINDOW_SAMPLES - 1) - (last - 200));
}

/// Slaney filters: 80 non-empty triangles, peak `2 / (f[m+2] - f[m])`
/// scaled by how close a bin gets to the centre; the first starts above
/// 0 Hz (its lower edge is 0 Hz, where the weight is 0).
#[test]
fn filters_are_slaney_triangles() {
    let filters = slaney_filters();
    assert_eq!(filters.len(), N_MELS);
    assert!(filters.iter().all(|f| f.weights.iter().any(|&w| w > 0.0)));
    assert_eq!(filters[0].first, 1);
    assert!(filters.windows(2).all(|p| p[0].first <= p[1].first));
    let last = &filters[N_MELS - 1];
    assert!(last.first + last.weights.len() <= BINS);
    // Mel scale breakpoints.
    assert!((hz_to_mel(1000.0) - 15.0).abs() < 1e-12);
    assert!((mel_to_hz(hz_to_mel(6000.0)) - 6000.0).abs() < 1e-9);
}
