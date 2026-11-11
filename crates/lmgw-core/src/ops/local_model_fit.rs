//! What `local_model_get` shows of a chat row VRAM admission fits to the
//! card in llama.cpp's `--fit` place (`vram::partial::llama`): the
//! placement its command line is rendered with, and the sentence that says
//! where it comes from.

use crate::runtime::argv::Fitted;
use crate::runtime::registry::RuntimeState;
use crate::runtime::Class;
use crate::state::SharedState;

/// The placement `model_id`'s command line is shown with, and what to say
/// of it: the running container's own where admission fitted it, else what
/// a start on this card gets beside what other programs held at the newest
/// reading — decided again at each start against what the card can give
/// then. `(None, [])` for a row admission does not fit.
pub(super) async fn fitted(state: &SharedState, model_id: &str) -> (Option<Fitted>, Vec<String>) {
    let running = state.runtime().list().into_iter().find(|e| {
        e.class == Class::Chat
            && e.model_id == model_id
            && matches!(e.state, RuntimeState::Starting | RuntimeState::Ready)
    });
    if let Some(e) = running {
        let Some(g) = e.grant.as_ref() else {
            return (None, Vec::new());
        };
        let Some(p) = g.llama.filter(|p| !p.is_whole()) else {
            return (None, Vec::new());
        };
        return (
            Some(Fitted {
                bytes: g.bytes,
                placement: p,
            }),
            vec![format!(
                "The command line shows what its running container was told: VRAM admission \
                 fitted it to the card in llama.cpp's --fit place ({}). A later start is fitted \
                 again to what is free then",
                p.flags()
            )],
        );
    }
    let snap = state.snapshot();
    let Some(fp) = state.vram.footprint(&snap, Class::Chat, model_id).await else {
        return (None, Vec::new());
    };
    let Some(l) = fp.llama.as_ref() else {
        return (None, Vec::new());
    };
    if !l.fits_card {
        return (None, Vec::new());
    }
    if l.target.is_whole() {
        return (None, Vec::new());
    }
    (
        Some(Fitted {
            bytes: fp.total_bytes,
            placement: l.target,
        }),
        vec![format!(
            "The command line shows what a start on this card is told beside what other \
             programs held at the last reading: {}. The placement is decided at each start \
             against what the card can give it then (VRAM admission, in llama.cpp's --fit \
             place)",
            fp.note.as_deref().unwrap_or_default()
        )],
    )
}
