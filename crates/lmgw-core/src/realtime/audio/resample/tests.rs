use super::*;

fn sine(rate: u32, hz: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
        .collect()
}

/// Largest error over the whole clip except `edge` samples at each end.
fn max_err(a: &[f32], b: &[f32], edge: usize) -> f32 {
    assert_eq!(a.len(), b.len());
    a[edge..a.len() - edge]
        .iter()
        .zip(&b[edge..b.len() - edge])
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

/// Feeds `x` in awkward chunk sizes and flushes.
pub(crate) fn stream(rs: &mut StreamResampler, x: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    let mut i = 0;
    for size in [1usize, 479, 960, 7, 2400].iter().cycle() {
        if i >= x.len() {
            break;
        }
        let end = (i + size).min(x.len());
        rs.push(&x[i..end], &mut out).unwrap();
        i = end;
    }
    rs.flush(&mut out).unwrap();
    out
}

/// The error of a 0.5-amplitude sine shifted by half a sample: what
/// "aligned within half a sample" allows.
fn half_sample(hz: f32, rate: u32) -> f32 {
    0.5 * std::f32::consts::PI * hz / rate as f32
}

#[test]
fn stream_is_aligned_and_exact_length() {
    let x = sine(24_000, 440.0, 24_000);
    let mut rs = StreamResampler::new().unwrap();
    assert_eq!(rs.delay_input_samples(), 126);
    assert_eq!(rs.block_input_samples(), 240);
    let y = stream(&mut rs, &x);
    assert_eq!(y.len(), 16_000);
    // Edges only lose the filter's ramp (the clip starts mid-wave).
    let err = max_err(&y, &sine(16_000, 440.0, 16_000), 200);
    assert!(
        err < half_sample(440.0, 16_000),
        "aligned to the input timeline: {err}"
    );
    // Flush reset the stream; a second pass gives the same result.
    assert_eq!(stream(&mut rs, &x), y);
    assert_eq!(StreamResampler::output_to_input(512), 768);
}

#[test]
fn stream_latency_is_delay_plus_block_rounding() {
    let mut rs = StreamResampler::new().unwrap();
    let mut out = Vec::new();
    rs.push(&[0.1; 239], &mut out).unwrap();
    assert!(out.is_empty(), "nothing before a full block");
    // The first Silero frame (output 0..512) covers input 0..768; it is
    // complete once 768 + 126 inputs arrived, rounded up to 240s: 960.
    rs.push(&[0.1; 1], &mut out).unwrap();
    while rs.emitted() < 512 {
        rs.push(&[0.1; 240], &mut out).unwrap();
    }
    assert_eq!(rs.0.pushed, 960);
    assert_eq!(out.len() as u64, rs.emitted());
}

#[test]
fn one_shot_is_clean_from_the_first_block() {
    // The whole clip is checked, including the first block that
    // rubato's own `process_all` corrupts.
    for (from, to) in [
        (22_050, 24_000),
        (48_000, 24_000),
        (16_000, 24_000),
        (44_100, 24_000),
    ] {
        let y = resample(&sine(from, 300.0, from as usize), from, to, usize::MAX).unwrap();
        let err = max_err(&y, &sine(to, 300.0, to as usize), 200);
        assert!(err < half_sample(300.0, to), "{from} -> {to}: {err}");
    }
}

#[test]
fn one_shot_bounds() {
    let x = sine(22_050, 300.0, 22_050);
    // Pass-through, but still bounded.
    assert_eq!(resample(&x, 24_000, 24_000, 22_050).unwrap(), x);
    assert!(resample(&x, 24_000, 24_000, 100).is_err());
    assert_eq!(resample(&x, 0, 24_000, 1), Err(ResampleError::ZeroRate));
    // A 1 Hz header asks for 24000x the input: fine within the bound,
    // refused up front beyond it.
    assert_eq!(
        resample(&x[..10], 1, 24_000, 240_000).unwrap().len(),
        240_000
    );
    assert_eq!(
        resample(&x[..100], 1, 24_000, 1_000),
        Err(ResampleError::OutputTooLong {
            needed: 2_400_000,
            max: 1_000
        })
    );
    // A coprime rate costs no more memory than a friendly one, and the
    // length is the exact rational one.
    assert_eq!(
        resample(&x[..997], 997, 24_000, usize::MAX).unwrap().len(),
        24_000
    );
    assert!(resample(&[], 16_000, 24_000, 0).unwrap().is_empty());
}
