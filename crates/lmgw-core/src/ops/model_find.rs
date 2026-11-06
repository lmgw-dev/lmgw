//! `model_find` (find-a-model design §9.1, WP4): a model by the name
//! someone said, sized to this computer's card. Shared by
//! `POST /api/op/model_find` and `lmgw__model_find`. It reads, and no
//! header (D123): the card's marks are by size, and `hf_repo` with `fit`
//! reads the header after.

use lmgw_api_types::model_find::{FindAnswer, ModelFind, ModelFindArgs};

use crate::find::{self, card::Inputs, Kind};
use crate::hf::search::LiveHub;
use crate::modelinfo::fit::CardNow;
use crate::state::SharedState;

/// §4 against the live hub with this install's token (D2), measured with
/// this install's card, download rate, engine and disk.
pub async fn model_find(state: &SharedState, args: ModelFindArgs) -> Result<ModelFind, String> {
    let snap = state.snapshot();
    let card = CardNow::of(state, &snap).await;
    let hub = LiveHub::of(state);
    let found = find::find(
        &hub,
        &args.query,
        args.cursor.as_deref(),
        Inputs::room(card.as_ref()),
    )
    .await?;
    let kind = found
        .origin
        .as_ref()
        .map(|o| o.kind.clone())
        .unwrap_or(Kind::Chat);
    let with_card = matches!(found.answer, FindAnswer::Card | FindAnswer::NothingFits)
        && found.chosen.is_some();
    // The engine and the rate only matter to a card with a file: the
    // engine's check asks Podman and maybe a registry.
    let (engine, downloads_rate) = match with_card {
        true => (
            crate::hf::rate::engine(state, kind.target()).await,
            crate::hf::rate::of_downloads(state).await,
        ),
        false => (
            crate::hf::rate::Engine::NotKnown("no file to fetch".into()),
            None,
        ),
    };
    let dir = crate::hf::models_dir_for_target(&snap.settings, kind.target());
    let disk_free = (!dir.trim().is_empty())
        .then(|| crate::backends::paths::free_bytes(std::path::Path::new(&dir)))
        .flatten();
    let inputs = Inputs {
        card,
        downloads_rate,
        engine,
        disk_free,
        token_set: !snap.settings.hf_token.trim().is_empty(),
    };
    Ok(find::card::answer(
        &found,
        &inputs,
        &find::card::Lists::of(&args),
    ))
}
