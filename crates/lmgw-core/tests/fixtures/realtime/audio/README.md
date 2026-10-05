# Realtime voice test fixtures

Synthetic only: Piper TTS voice `en_US-ljspeech-medium` (public domain dataset, trained from scratch,
see `LICENCES.json`) and seeded numpy noise. Piper is non-deterministic: treat the committed bytes as
the artefacts, do not expect regeneration to reproduce them.

## Audio (24 kHz mono PCM16 WAV, each with a `.json` sidecar: text, voice, licence, duration, ground-truth segments)
- `en_complete_short.wav`: one question; 300 ms lead, 800 ms trail. `turn_end_ms` = end of speech.
- `en_two_sentences_pause.wav`: two sentences, 700 ms pause, 800 ms trail (a 500 ms silence window splits, a longer one does not).
- `en_midsentence_pause.wav`: one natural-prosody utterance cut at a word boundary ("... report because | my boss ..."), exactly 400 ms digital silence inserted (`mid_pause_start_ms`..`mid_pause_end_ms`).
- `noise_only.wav`: 2 s seeded white+pink noise, -50 dBFS RMS (VAD negative).
Segment times come from the generation (where silence was placed), not from a VAD. Speech is amplitude-trimmed (2% of peak, 5 ms fades).

## Smart Turn parity (`smart-turn-v3.2-cpu.onnx`)
`smartturn_<name>.{audio16k_f32le.bin,features_f32le.bin,json}` for `en_complete_short` (cut at speech end, expected complete)
and `en_midsentence_pause` (cut 200 ms into the pause, expected incomplete).
- `.bin` files are little-endian float32, row-major.
- Audio: 16 kHz, unpadded; soxr VHQ resample of the cut 24 kHz WAV.
- Pipeline: left-pad zeros to 128000 samples (keep the last 128000 if longer), WhisperFeatureExtractor(chunk_length=8, do_normalize=True).
- Features: shape [1,80,800], mel-major, time fastest. The JSON has first/last 5 values, mean/std/min/max and the probability.
- Model input `input_features`; output (named `logits`) is already a probability.
- Suggested tolerance for a Rust mel implementation: compare features elementwise (~1e-3 abs) and the probability (~1e-2).
- Smart Turn is not a reliable boolean on TTS: appending 200 ms silence pushes most incomplete clips towards "complete".

## Silero CSVs (`silero_vad_16k_op15.onnx`)
`silero_<name>.csv` and `silero_smartturn_<name>_cutaudio.csv`, columns `frame,start_sample_16k,end_ms,prob`.
- Inputs `input[1,64+512]`, `state[2,1,128]`, int64 `sr=16000`; outputs `output`, `stateN`.
- Frame = 512 samples (32 ms); each input is the previous 64 samples of context (zeros at start) plus 512 new samples; the partial tail frame is dropped.
- The `cutaudio` CSVs use the exact `audio16k_f32le.bin` inputs: tolerance about 1e-3 across ORT builds.
- The full-file CSVs start from the 24 kHz WAV resampled with soxr VHQ; a different resampler can add up to about 0.02.
