use lmgw_core::voice_lab::save::check_name;
use lmgw_core::voice_lab::sentences::{self, TABLE};
use lmgw_core::voice_lab::wav::{self, pcm16_mono, Wav};

use super::padded_tone;

#[test]
fn the_table_ships_english_and_german_with_a_check_line_each() {
    let langs: Vec<&str> = TABLE.iter().map(|e| e.language).collect();
    assert_eq!(langs, ["en", "de"]);
    for e in TABLE {
        assert!(e.sentence.split_whitespace().count() > 30, "{}", e.language);
        assert!(!e.check_line.is_empty(), "{}", e.language);
        assert!(
            !e.sentence.contains("  "),
            "{}: a line break left two spaces",
            e.language
        );
    }
    assert!(
        sentences::builtin("de-at").is_some(),
        "a region reads its language"
    );
    assert!(sentences::builtin("fr").is_none());
}

#[test]
fn an_owners_sentence_is_saved_per_language_and_survives_a_fresh_read() {
    let dir = tempfile::tempdir().unwrap();
    let lab = dir.path();
    assert_eq!(
        sentences::sentence_for(lab, "de").as_deref(),
        Some(TABLE[1].sentence)
    );
    assert_eq!(
        sentences::sentence_for(lab, "fr"),
        None,
        "no entry: empty box"
    );

    assert_eq!(
        sentences::save(lab, " DE ", " Mein eigener Satz. ").unwrap(),
        "de"
    );
    assert_eq!(
        sentences::save(lab, "fr", "Une phrase à moi.").unwrap(),
        "fr"
    );
    // What a restart reads: the file, nothing in memory.
    assert_eq!(
        sentences::sentence_for(lab, "de").as_deref(),
        Some("Mein eigener Satz.")
    );
    let t = sentences::table(lab);
    let codes: Vec<&str> = t.languages.iter().map(|l| l.language.as_str()).collect();
    assert_eq!(
        codes,
        ["en", "de", "fr"],
        "built-in ones first, then the owner's"
    );
    let de = &t.languages[1];
    assert!(de.saved);
    assert_eq!(de.name, "German");
    assert_eq!(de.builtin.as_deref(), Some(TABLE[1].sentence));
    let fr = &t.languages[2];
    assert!(fr.saved && fr.builtin.is_none() && fr.check_line.is_empty());

    // Empty drops the owner's: the built-in one is the prefill again.
    sentences::save(lab, "de", "  ").unwrap();
    assert_eq!(
        sentences::sentence_for(lab, "de").as_deref(),
        Some(TABLE[1].sentence)
    );
    assert!(!sentences::table(lab).languages[1].saved);

    assert!(sentences::save(lab, "deutsch!", "x").is_err());
    assert!(sentences::normalize("pt_BR").unwrap() == "pt-br");
}

#[test]
fn trimming_cuts_only_the_silence_around_the_speech_and_keeps_its_samples() {
    let src = padded_tone();
    let (out, trim) = wav::trim_silence(&src);
    assert_eq!(trim.note, None);
    // 500 ms of silence less the 40 ms margin; 300 ms less 120 ms.
    assert_eq!((trim.lead_ms, trim.tail_ms, trim.kept_ms), (460, 180, 1160));
    let a = Wav::parse(&src).unwrap();
    let b = Wav::parse(&out).unwrap();
    assert_eq!((b.rate, b.channels, b.bits), (a.rate, a.channels, a.bits));
    assert_eq!(b.duration_ms(), 1160);
    // The samples kept are the source's own, byte for byte: no gain.
    let start = 11_040 * 2;
    assert_eq!(b.data, &a.data[start..start + b.data.len()]);
    // Trimming what is trimmed cuts nothing more than the margins allow.
    let (_, again) = wav::trim_silence(&out);
    assert_eq!((again.lead_ms, again.tail_ms), (0, 0));
}

#[test]
fn what_cannot_be_trimmed_is_kept_as_it_came_with_the_reason() {
    let silent = pcm16_mono(16_000, &[0; 16_000]);
    let (out, trim) = wav::trim_silence(&silent);
    assert_eq!(out, silent);
    assert!(trim.note.unwrap().contains("silence floor"));
    assert_eq!(trim.kept_ms, 1000);

    let mp3 = b"ID3\x04\0\0\0\0\0\0not a wav".to_vec();
    let (out, trim) = wav::trim_silence(&mp3);
    assert_eq!(out, mp3);
    assert!(trim.note.unwrap().contains("not a PCM WAV"));
    assert_eq!(wav::duration_ms(&mp3), None);
}

#[test]
fn a_streamed_header_and_other_sample_formats_read() {
    // A streamed WAV: data size all ones, the samples running to the end.
    let mut streamed = pcm16_mono(8_000, &[100; 8_000]);
    let at = streamed.len() - 16_000 - 4;
    streamed[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(wav::duration_ms(&streamed), Some(1000));

    // 24-bit stereo at 48 kHz, half a second: 6 bytes a frame.
    let frames = 24_000u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + frames * 6).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&(48_000u32 * 6).to_le_bytes());
    b.extend_from_slice(&6u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(frames * 6).to_le_bytes());
    b.resize(b.len() + (frames * 6) as usize, 0);
    assert_eq!(wav::duration_ms(&b), Some(500));
}

#[test]
fn a_sample_name_is_a_plain_voice_name() {
    assert_eq!(check_name(" anna.wav ").unwrap(), "anna");
    assert_eq!(check_name("Anna 2").unwrap(), "Anna 2");
    assert_eq!(check_name("calm.v2").unwrap(), "calm.v2");
    for bad in ["", ".wav", "../x", "a/b", ".hidden", "a|b"] {
        assert!(check_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_count_of_billions_is_checked_without_laying_out_a_seed_each() {
    use lmgw_api_types::voice_lab::VoiceGenerateArgs;
    use lmgw_core::voice_lab::generate::check;
    let dir = tempfile::tempdir().unwrap();
    let args = VoiceGenerateArgs {
        alias: "audio/x".into(),
        text: Some("Hi.".into()),
        count: Some(4_000_000_000),
        seed: Some(u32::MAX),
        ..Default::default()
    };
    // Nothing per candidate up front: the answer is the count and the
    // first seed, each candidate's drawn as it starts.
    let asked = check(dir.path(), &args).unwrap();
    assert_eq!(asked.count, 4_000_000_000);
    assert!(std::mem::size_of_val(&asked) < 256);
    assert_eq!(
        (asked.seed(0), asked.seed(1), asked.seed(2)),
        (u32::MAX, 0, 1)
    );
    let random = check(dir.path(), &VoiceGenerateArgs { seed: None, ..args }).unwrap();
    assert_eq!(random.first_seed, None);
}

#[test]
fn a_sentences_file_lmgw_cannot_read_is_reported_and_never_written_over() {
    let dir = tempfile::tempdir().unwrap();
    let lab = dir.path();
    let file = lab.join("sentences.json");
    std::fs::write(&file, b"{\"de\": \"Mein Satz.\",}").unwrap();

    let refused = sentences::save(lab, "en", "My own.").unwrap_err();
    assert_eq!(refused.status.as_u16(), 409);
    assert_eq!(refused.code, "unreadable_file");
    assert!(refused.message.contains("sentences.json"), "{refused:?}");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"{\"de\": \"Mein Satz.\",}",
        "the owner's file is left as it is"
    );

    // Reading shows the built-in sentences and says why.
    let t = sentences::table(lab);
    assert_eq!(t.languages.len(), TABLE.len());
    assert!(t.error.as_deref().unwrap().contains("sentences.json"));
    assert_eq!(
        sentences::sentence_for(lab, "de").as_deref(),
        Some(TABLE[1].sentence)
    );
    // Once fixed, it is read and written as ever.
    std::fs::write(&file, b"{\"de\": \"Mein Satz.\"}").unwrap();
    assert_eq!(sentences::table(lab).error, None);
    sentences::save(lab, "en", "My own.").unwrap();
    assert_eq!(
        sentences::sentence_for(lab, "de").as_deref(),
        Some("Mein Satz.")
    );
}

#[test]
fn a_records_file_lmgw_cannot_read_is_reported_and_never_written_over() {
    use lmgw_api_types::voice_lab::VoiceProvenance;
    use lmgw_core::voice_lab::provenance;
    let dir = tempfile::tempdir().unwrap();
    let lab = dir.path();
    let file = lab.join("samples.json");
    std::fs::write(&file, b"not json").unwrap();
    let refused =
        provenance::record(lab, "anna.wav", &VoiceProvenance::default(), &[]).unwrap_err();
    assert_eq!(refused.code, "unreadable_file");
    assert!(refused.message.contains("samples.json"), "{refused:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"not json");
    let (samples, why) = provenance::samples(lab, lab, Vec::new());
    assert!(samples.is_empty());
    assert!(why.unwrap().contains("samples.json"));

    std::fs::remove_file(&file).unwrap();
    provenance::record(lab, "anna.wav", &VoiceProvenance::default(), &[]).unwrap();
    assert_eq!(provenance::read(lab).unwrap().len(), 1);
}

#[test]
fn only_audio_files_are_a_voice_name_a_save_replaces() {
    use lmgw_core::voice_lab::save::is_audio;
    for yes in ["anna.wav", "anna.mp3", "anna.flac", "anna.ogg", "Anna.WAV"] {
        assert!(is_audio(yes), "{yes}");
    }
    for no in ["anna.txt", "anna.json", "anna", "prompt_text"] {
        assert!(!is_audio(no), "{no}");
    }
}

#[test]
fn temporary_names_are_hidden_and_never_shared() {
    let a = lmgw_core::voice_lab::temp_name("prompt_text");
    let b = lmgw_core::voice_lab::temp_name("prompt_text");
    assert_ne!(a, b);
    assert!(
        a.starts_with(".prompt_text.") && a.ends_with(".part"),
        "{a}"
    );
    assert!(a.contains(&std::process::id().to_string()), "{a}");
}

/// A WAV of `chunks` (id, declared size, body) after a RIFF header that
/// declares `riff`.
fn riff(riff: u32, chunks: &[(&[u8; 4], u32, &[u8])]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&riff.to_le_bytes());
    b.extend_from_slice(b"WAVE");
    for (id, size, body) in chunks {
        b.extend_from_slice(*id);
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(body);
    }
    b
}

fn fmt16(format: u16, rate: u32, bits: u16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&format.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * u32::from(bits / 8)).to_le_bytes());
    f.extend_from_slice(&(bits / 8).to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f
}

#[test]
fn a_data_size_of_zero_means_none_unless_the_header_was_never_patched() {
    let fmt = fmt16(1, 8_000, 16);
    let samples = vec![1u8; 16_000];
    // The RIFF size is set: a data chunk of 0 is empty, whatever follows it.
    let mut set = riff(4 + 24 + 8, &[(b"fmt ", 16, &fmt), (b"data", 0, &[])]);
    set.extend_from_slice(&samples);
    assert_eq!(wav::duration_ms(&set), Some(0));
    // A header written before the sizes were known (RIFF 0 or all ones):
    // the data runs to the end.
    for unknown in [0, u32::MAX] {
        let streamed = riff(unknown, &[(b"fmt ", 16, &fmt), (b"data", 0, &samples)]);
        assert_eq!(wav::duration_ms(&streamed), Some(1000), "riff {unknown:#x}");
    }
    // All ones in the data size is unknown whatever the RIFF size says.
    let ones = riff(
        4 + 24 + 8 + 16_000,
        &[(b"fmt ", 16, &fmt), (b"data", u32::MAX, &samples)],
    );
    assert_eq!(wav::duration_ms(&ones), Some(1000));
}

#[test]
fn a_trimmed_float_wav_keeps_its_fact_chunk_counting_what_it_kept() {
    let rate = 16_000u32;
    let fmt = fmt16(3, rate, 32);
    // 0.5 s silence, 1 s of tone, 0.5 s silence, as 32-bit floats.
    let mut s = vec![0f32; 8_000];
    s.extend((0..16_000).map(|i| ((i as f32) * 0.2).sin() * 0.5));
    s.extend(vec![0f32; 8_000]);
    let data: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
    let frames = s.len() as u32;
    let src = riff(
        4 + 24 + 12 + 8 + data.len() as u32,
        &[
            (b"fmt ", 16, &fmt),
            (b"fact", 4, &frames.to_le_bytes()),
            (b"data", data.len() as u32, &data),
        ],
    );
    let parsed = Wav::parse(&src).unwrap();
    assert!(parsed.fact);
    let (out, trim) = wav::trim_silence(&src);
    assert_eq!(trim.note, None);
    let w = Wav::parse(&out).unwrap();
    assert!(w.fact, "the trimmed copy has its fact chunk");
    let at = out.windows(4).position(|c| c == b"fact").unwrap();
    let counted = u32::from_le_bytes(out[at + 8..at + 12].try_into().unwrap());
    assert_eq!(counted as usize, w.frames());
    assert!(w.frames() < s.len());
    assert_eq!(w.duration_ms(), trim.kept_ms);
    // The RIFF size covers the chunks written.
    let riff_size = u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize;
    assert_eq!(riff_size, out.len() - 8);
    // A PCM WAV without one gets none.
    let (pcm, _) = wav::trim_silence(&padded_tone());
    assert!(!Wav::parse(&pcm).unwrap().fact);
}
