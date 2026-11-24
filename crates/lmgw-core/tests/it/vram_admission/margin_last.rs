//! The start margin given up last (device-ledger design WP13, the owner's
//! decision of 2026-11-17), end to end on one card in fixed mode, where no
//! learned room is kept and only the margin can be given up:
//!
//! - a voice model whose files fit the card but whose files twice over do
//!   not is not refused as too large: it starts beside the floor, warned,
//!   and its first request teaches its residency;
//! - a chat model that fits only without its margin starts, warned, and
//!   says that an estimate that runs low comes out of the floor;
//! - while lmgw can make room another way — an idle model of its own to
//!   evict — the margin is kept, and nothing is said.

use super::audio_residency::{add_audio_model, learned_now, speak_and_read};
use super::*;
use lmgw_core::vram::headroom::{GROWTH_LET_GO, LLAMA_MARGIN, MARGIN_LET_GO};

const MIB: u64 = 1024 * 1024;

/// Headroom in fixed mode: `headroom_mb` kept free, no learned room.
async fn fixed(f: &Fixture) {
    let mut s = f.state.snapshot().settings.clone();
    s.vram.headroom_mode = lmgw_api_types::HeadroomMode::Fixed;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// The warnings `model_id`'s registry entry carries.
fn warnings(f: &Fixture, model_id: &str) -> Vec<String> {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model_id)
        .map(|v| v.warnings)
        .unwrap_or_else(|| panic!("{model_id} is up"))
}

/// Before: an unlearned voice model's margin is as much as its files, and
/// the too-large test counted it, so 5 GiB of files on an 8 GiB card with
/// 1 GiB kept free was refused and never measured. Now it starts beside the
/// floor, warned, and its first request teaches what it holds.
#[tokio::test]
async fn a_voice_model_whose_files_fit_starts_and_is_measured() {
    let f = fixture(8 * GIB, GIB, GIB, 1024).await;
    fixed(&f).await;
    f.attribute(0);
    add_audio_model(&f, "voice", 5 * GIB, 6 * GIB, None).await;
    speak_and_read(&f, "voice").await;
    assert_eq!(
        learned_now(&f, "voice"),
        Some(6 * GIB),
        "its first request taught its residency"
    );
    let w = warnings(&f, "voice");
    assert!(
        w.iter().any(|w| w.starts_with(MARGIN_LET_GO)
            && w.contains("keeps a margin of 5.0 GiB beside audio/voice")
            && w.contains("comes out of the 1.0 GiB kept free for other programs")),
        "{w:?}"
    );
    assert!(
        !w.iter().any(|w| w.contains(GROWTH_LET_GO)),
        "no learned room in fixed mode: {w:?}"
    );
}

/// A chat model of 6 GiB on a card that holds it beside the 512 MiB kept
/// free but not beside its 327 MiB margin as well: it starts, warned.
#[tokio::test]
async fn a_chat_model_that_fits_only_without_its_margin_starts_warned() {
    let f = fixture(6 * GIB + 512 * MIB + 100 * MIB, 6 * GIB, GIB, 512).await;
    fixed(&f).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let w = warnings(&f, "chat-model");
    assert!(
        w.iter().any(|w| w.starts_with(MARGIN_LET_GO)
            && w.contains("keeps a margin of 327 MiB beside chat/chat-model")
            && w.contains("with the 512 MiB always kept free, not with that margin")),
        "{w:?}"
    );
}

/// With an idle model of lmgw's on the card the margin is kept: the chat
/// model would fit beside the idle embedder and the 512 MiB kept free, but
/// not with its margin as well, so the embedder is evicted for it, and the
/// start says nothing of a margin.
#[tokio::test]
async fn the_margin_is_kept_while_an_idle_model_can_be_evicted() {
    let total = 6 * GIB + 512 * MIB + LLAMA_MARGIN + 50 * MIB;
    let f = fixture(total, 6 * GIB, 200 * MIB, 512).await;
    fixed(&f).await;
    assert_eq!(embed(&f.gateway).await.status(), 200);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(
        f.stops(),
        vec!["embed-model".to_string()],
        "evicted for the margin"
    );
    let w = warnings(&f, "chat-model");
    assert!(
        !w.iter().any(|w| w.contains(MARGIN_LET_GO)),
        "its margin was kept: {w:?}"
    );
}
