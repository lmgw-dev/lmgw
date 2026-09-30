//! The route-kind guards the request gate runs before dispatch
//! ([`crate::gate::open`]): a media model on a text route, a reranker
//! answering `/v1/embeddings`, an embedder answering `/v1/rerank`.

use crate::config::Route;
use crate::error::GatewayError;

/// Refuse a **media** model — image or audio — on one of the text routes,
/// **before admission**, because merely resolving one is enough for the next
/// line to start its container for a request that server has no handler for
/// (image-generation design §6).
///
/// The same shape [`resolve_image`] refuses a chat model with, from the other
/// side: one class's routes, named. Both media classes are in here rather than
/// one guard each because every call site wants both: a text route serves
/// neither, and the only thing that differs between them is which endpoints
/// the refusal points at.
///
/// Keyed on [`UpstreamKind`](crate::config::UpstreamKind), not on the
/// synthetic upstream's id, so an owner's own `sd_cpp`/`audio_cpp` upstream is
/// covered too — those servers have no text handler either, wherever they run.
pub(crate) fn refuse_media_route(
    route: &Route,
    alias: &str,
    endpoint: &str,
) -> Result<(), GatewayError> {
    let (what, send_it_to, server) = match route.upstream.kind {
        crate::config::UpstreamKind::SdCpp => (
            "an image",
            "/v1/images/generations, or to /v1/images/edits when its row is marked edit",
            "sd-server",
        ),
        crate::config::UpstreamKind::AudioCpp => (
            "an audio",
            "/v1/audio/speech, /v1/audio/transcriptions (or …/details), \
             /v1/audio/alignments, or to /v1/tasks/run (/v1/tasks/stream) for a task \
             with no OpenAI shape",
            "audio.cpp",
        ),
        _ => return Ok(()),
    };
    Err(GatewayError::Unsupported(format!(
        "model '{alias}' is {what} model, and {endpoint} does not serve one — send it to \
         {send_it_to}. (Refused here so its container is not started for a request \
         {server} cannot answer.)"
    )))
}

/// The reranker-section guard of `/v1/embeddings`
/// ([`crate::gate::RouteCheck::Embeddings`]).
///
/// A reranker section answers `/v1/embeddings` with HTTP 200 and an all-zero
/// vector — llama-server sets the embedding flag internally for reranking, so
/// nothing downstream can tell the difference (verified live). Silently
/// forwarding that fills a corpus with zero vectors that look fine until
/// every search returns noise, so the kind lmgw itself recorded is the gate,
/// and a misrouted alias fails loudly instead.
pub(crate) fn refuse_reranker_for_embeddings(
    snap: &crate::config::Snapshot,
    route: &Route,
    alias: &str,
) -> Result<(), GatewayError> {
    match snap.aux_model_for(route) {
        Some(m) if m.kind == crate::config::AuxKind::Rerank => {
            Err(GatewayError::BadRequest(format!(
                "'{alias}' resolves to the rerank model '{}' — rerankers cannot produce \
                 embeddings (llama-server would answer with an all-zero vector). Request an \
                 embedding model, or send this one to /v1/rerank.",
                m.model_id
            )))
        }
        _ => Ok(()),
    }
}

/// The embedding-section guard of `/v1/rerank`
/// ([`crate::gate::RouteCheck::Rerank`]) — the mirror image of
/// [`refuse_reranker_for_embeddings`]. A section without `reranking = true`
/// answers a rerank request with whatever its pooling produces, which is not
/// a relevance score and would silently reorder a corpus's answers. Models
/// lmgw has no kind for (a remote provider's) are forwarded — their kind is
/// theirs to know.
pub(crate) fn refuse_embedder_for_rerank(
    snap: &crate::config::Snapshot,
    route: &Route,
    alias: &str,
) -> Result<(), GatewayError> {
    match snap.aux_model_for(route) {
        Some(m) if m.kind == crate::config::AuxKind::Embed => {
            Err(GatewayError::BadRequest(format!(
                "'{alias}' resolves to the embedding model '{}' — an embedding section \
                 cannot rank (it has no cross-encoder head, and llama-server would answer \
                 with its pooling output instead of a relevance score). Request a rerank \
                 model, or send this one to /v1/embeddings.",
                m.model_id
            )))
        }
        _ => Ok(()),
    }
}
