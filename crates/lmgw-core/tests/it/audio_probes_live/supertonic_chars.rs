//! **Supertonic characters** (review TC-3): the drift tripwire of
//! `audio::charset`. lmgw sends a Supertonic row the characters its
//! engine rewrites itself (`audio::families::char_vocabulary`, copied by
//! hand off audio.cpp's `preprocess`), what it decomposes (its NFKD table,
//! `families::charset::nfkd`) and what lmgw puts in place of a character
//! it cannot say (`audio::charset::replacements`) as they are. An engine
//! that stopped rewriting one, or regenerated its table, would fail such a
//! speech again with "Supertonic unicode indexer has no entry for codepoint
//! …", while every test with the stand-in stays green. Run this after an
//! audio.cpp image update.
//!
//! It sends them all to a real Supertonic container and expects a 200 with
//! no `x-lmgw-speech` (nothing was changed on the way), then a text of hard
//! characters lmgw has to fit (`chars=`, 200), then one of nothing the
//! engine says (400 `empty_input`, nothing started).
//!
//! The row runs on the CPU (4 threads): no GPU is needed. By default the
//! probe starts its own gateway and container like the other probes here.
//! With `LMGW_LIVE_AUDIO_GATEWAY` (a base URL) and
//! `LMGW_LIVE_SUPERTONIC_ALIAS` (an alias on it, `audio/<row>`) it asks a
//! running lmgw instead, a dev instance with a Supertonic row of its own;
//! `LMGW_LIVE_AUDIO_MODELS_DIR` is then where the package that row serves
//! lies at `audio-cpp/audio.cpp-gguf/Supertonic-3-GGUF`, which the probe
//! reads the package's vocabulary from:
//!
//! ```sh
//! LMGW_LIVE_AUDIO_PROBES=1 LMGW_LIVE_AUDIO_MODELS_DIR=<data dir>/models \
//!   LMGW_LIVE_AUDIO_GATEWAY=http://127.0.0.1:8897 \
//!   LMGW_LIVE_SUPERTONIC_ALIAS=audio/supertonic-cpu \
//!   cargo nextest run -p lmgw-core --test it --run-ignored only \
//!   -E 'test(supertonic_says_every_character)' --no-capture
//! ```

use lmgw_core::audio::charset::replacements;
use serde_json::{json, Value};

use super::{gateway, models_dir, row, runs, Sweep, SUPERTONIC};

/// Text around the probed characters, so none of them is at an edge the
/// engine trims.
fn framed(chars: &str) -> String {
    format!("Eins {chars} zwei.")
}

/// Characters kept as they are because the engine's NFKD makes them text
/// it has: ä, the ellipsis, a no-break and a narrow no-break space, the
/// fullwidth exclamation mark, one half.
const NFKD: &str = "ä … \u{00A0} \u{202F} \u{FF01} \u{00BD}";

/// What lmgw has to fit: a German quote pair, a dash, an ellipsis, an emoji
/// with its skin tone, the capital sharp s, the per-mille sign, an outlined
/// A (Unicode 16, newer than the engine's NFKD table) and the modifier
/// capital S (Unicode 17).
const HARD: &str = "„Gern“ – bis bald… 👍🏽 GROẞ 5‰ \u{1CCD6} \u{A7F1}";

#[tokio::test]
#[ignore = "runs a real audio.cpp container; set LMGW_LIVE_AUDIO_PROBES=1"]
async fn supertonic_says_every_character_lmgw_sends_it_as_it_is() {
    const TEST: &str = "supertonic_says_every_character_lmgw_sends_it_as_it_is";
    let running = std::env::var("LMGW_LIVE_AUDIO_GATEWAY")
        .ok()
        .filter(|g| !g.is_empty());
    if !runs(TEST, &[SUPERTONIC.1]) {
        return;
    }
    let _sweep = Sweep;
    let (base, alias, _state) = match running {
        Some(base) => {
            let alias = std::env::var("LMGW_LIVE_SUPERTONIC_ALIAS")
                .expect("LMGW_LIVE_SUPERTONIC_ALIAS: the running gateway's Supertonic alias");
            (base.trim_end_matches('/').to_string(), alias, None)
        }
        None => {
            let mut r = row(
                "supertonic-chars",
                SUPERTONIC.0,
                SUPERTONIC.1,
                "tts",
                "offline",
            );
            r.backend = Some("cpu".into());
            r.threads = Some(4);
            let (state, base) = gateway(&[r]).await;
            (base, "audio/supertonic-chars".to_string(), Some(state))
        }
    };

    // What lmgw reads from the package: the rewrites it leaves to the
    // engine are the ones whose output the vocabulary has.
    let models = models_dir().unwrap();
    let probe_row: lmgw_core::config::AudioModel = serde_json::from_value(json!({
        "id": 0, "model_id": "probe", "family": SUPERTONIC.0, "path": SUPERTONIC.1,
        "task": "tts", "mode": "offline", "load_options": {}, "session_options": {},
        "voice_presets": {}, "default_voice_preset": null, "enabled": true, "image": null,
        "extra_run_args": null, "warm_start": false
    }))
    .unwrap();
    let gguf =
        lmgw_core::audio::files::row_gguf(&models, &models.join(SUPERTONIC.1), None, SUPERTONIC.0)
            .map(|(p, _)| p);
    let profile = lmgw_core::audio::profile::compute(&probe_row, None, gguf.as_deref());
    assert!(profile.problems.is_empty(), "{:?}", profile.problems);
    // Text the engine writes itself that the package's vocabulary lacks is a
    // note, not a problem (review TC-23): the real package has none.
    assert!(profile.notes.is_empty(), "{:?}", profile.notes);
    let vocab = profile
        .char_vocab
        .clone()
        .expect("the package's unicode_indexer");
    let rewrites: String = vocab.rewrites().iter().collect();
    assert_eq!(
        vocab.rewrites().len(),
        28,
        "every rewrite's output is in the vocabulary"
    );
    // The engine's longer rewrites too (`e.g.,`, `i.e.,`).
    let plain = framed(&format!(
        "{rewrites} e.g., i.e., {} {NFKD}",
        replacements().join(" ")
    ));

    let client = reqwest::Client::new();
    let speak = |input: String| {
        client
            .post(format!("{base}/v1/audio/speech"))
            .json(&json!({"model": alias, "input": input, "response_format": "wav"}))
            .send()
    };
    let resp = speak(plain.clone()).await.unwrap();
    let (status, header) = (resp.status(), shaped(&resp));
    let body = resp.bytes().await.unwrap();
    eprintln!(
        "supertonic chars: as they are -> {status}, x-lmgw-speech {header:?}, {} bytes",
        body.len()
    );
    assert!(
        status.is_success(),
        "the engine refused what lmgw sends it as it is ({plain:?}): {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header, None, "lmgw changed {plain:?} on its way");

    let resp = speak(framed(HARD)).await.unwrap();
    let (status, header) = (resp.status(), shaped(&resp));
    let body = resp.bytes().await.unwrap();
    eprintln!("supertonic chars: fitted -> {status}, x-lmgw-speech {header:?}");
    assert!(
        status.is_success(),
        "the engine refused the fitted {HARD:?}: {}",
        String::from_utf8_lossy(&body)
    );
    let header = header.unwrap_or_default();
    assert!(
        header.contains("chars=replaced:U+201E/") && header.contains("dropped:U+1F44D/U+1F3FD"),
        "{header}"
    );

    let resp = speak("😊".into()).await.unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    eprintln!("supertonic chars: a lone emoji -> {status}, {body}");
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "empty_input");
}

fn shaped(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-lmgw-speech")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}
