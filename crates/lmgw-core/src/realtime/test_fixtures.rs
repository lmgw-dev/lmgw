//! Test-only access to the committed realtime fixtures (realtime §16):
//! synthetic Piper `en_US-ljspeech-medium` speech and seeded noise, with
//! the Silero reference CSVs beside them (see the folder's `README.md`).

use std::path::PathBuf;

pub fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/realtime/audio")
        .join(name)
}

/// A committed 24 kHz WAV as mono float samples.
pub fn wav_24k(name: &str) -> Vec<f32> {
    let bytes = std::fs::read(path(name)).expect("fixture present");
    let wav = crate::realtime::audio::pcm::parse_wav(&bytes).expect("fixture parses");
    assert_eq!(wav.rate, 24_000);
    wav.samples
}

/// A committed little-endian float32 dump.
pub fn f32le(name: &str) -> Vec<f32> {
    let bytes = std::fs::read(path(name)).expect("fixture present");
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().map(|&b| f32::from_le_bytes(b)).collect()
}

/// `silero_*.csv` probabilities, indexed by frame (`frame,start_sample_16k,end_ms,prob`).
pub fn silero_csv(name: &str) -> Vec<f32> {
    let text = std::fs::read_to_string(path(name)).expect("fixture present");
    text.lines()
        .skip(1)
        .enumerate()
        .map(|(i, line)| {
            let cols: Vec<&str> = line.split(',').collect();
            assert_eq!(cols[0].parse::<usize>().unwrap(), i, "frames are dense");
            cols[3].parse().unwrap()
        })
        .collect()
}
