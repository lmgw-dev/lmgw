//! Which alias the barge-in word check may transcribe with (realtime design
//! §6.4, §12; B5 review, fix package B6): a speech-to-text one — what
//! `chat_stt_alias` must be (`asr::is_asr_alias`). A chat alias there failed
//! every check, so every barge-in fell back to the duration rule, and
//! nothing said why.
//!
//! - **A client's** `session.lmgw.barge_in_check_alias` that is not one:
//!   the `session.update` is refused, `invalid_value` on that parameter, as
//!   an unusable transcription model is.
//! - **The owner's** `realtime.barge_in_check_alias` that is not one: said
//!   at WARN when a session starts, and the session checks with its own ASR
//!   alias instead — its echo says `""`, the setting's "use the session's".
//!   A settings save is to refuse it (the realtime settings' patch, WP8).

use super::super::asr::is_asr_alias;
use super::super::protocol::ErrorObject;
use super::super::session::Core;
use crate::state::SharedState;

/// The parameter a client's word-check alias is refused on.
const PARAM: &str = "session.lmgw.barge_in_check_alias";

/// A client's word-check alias, at its `session.update` (module doc).
pub(in crate::realtime) async fn vet_client(
    state: &SharedState,
    alias: &str,
) -> Result<(), ErrorObject> {
    if is_asr_alias(state, alias).await {
        return Ok(());
    }
    Err(ErrorObject::invalid(
        "invalid_value",
        format!(
            "'{alias}' is not a speech-to-text (asr) alias this gateway serves; the barge-in word \
             check transcribes with one (empty: the session's ASR alias)"
        ),
    )
    .with_param(PARAM))
}

impl Core {
    /// The owner's word-check alias, before `session.created` (module doc).
    pub(in crate::realtime) async fn vet_owner_check_alias(&mut self) {
        let alias = self
            .state
            .snapshot()
            .settings
            .realtime
            .barge_in_check_alias
            .trim()
            .to_string();
        if alias.is_empty() || is_asr_alias(&self.state, &alias).await {
            return;
        }
        tracing::warn!(
            "realtime {}: realtime.barge_in_check_alias names '{alias}', which is not a \
             speech-to-text (asr) alias this gateway serves; this session's word check uses its \
             ASR alias instead — set the setting to an ASR alias, or empty it",
            self.id()
        );
        let lmgw = self.session.lmgw.get_or_insert_with(Default::default);
        lmgw.barge_in_check_alias = Some(String::new());
    }
}
