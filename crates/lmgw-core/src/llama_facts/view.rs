//! The external rows' facts as the Upstreams page and `lmgw__upstreams` show
//! them (llama egress design §4.2, "Shown").

use lmgw_api_types::{LlamaProps, UpstreamLlamaFacts};

use super::{Answer, ExternalFacts};
use crate::egress::llama_cpp::props::LlamaFacts;

impl ExternalFacts {
    /// Row `row`'s server: its facts, when they were read, or why they are
    /// unknown — one entry for a server that is no router (`model` empty),
    /// and for a router one for the server and one per model it was asked
    /// about.
    pub fn view(&self, row: i64) -> Vec<UpstreamLlamaFacts> {
        self.row(row)
            .into_iter()
            .map(|s| {
                let answer = s.entry.as_ref().map(|e| &e.answer);
                let props = match answer {
                    Some(Answer::Facts(f)) => Some(props_view(f)),
                    // A router states its build and nothing about a model.
                    Some(Answer::Router(build)) => Some(LlamaProps {
                        build_info: build.clone(),
                        ..Default::default()
                    }),
                    _ => None,
                };
                UpstreamLlamaFacts {
                    model: s.key.model.unwrap_or_default(),
                    base_url: s.key.base_url,
                    router: matches!(answer, Some(Answer::Router(_))),
                    props,
                    unknown: answer.and_then(|a| a.why_unknown()).map(str::to_string),
                    cached: answer.is_some_and(|a| a.cached()),
                    read_at: s.entry.as_ref().map(|e| e.at.timestamp()),
                    probing: s.probing,
                }
            })
            .collect()
    }
}

/// The facts on the wire, as the container view sends them.
pub fn props_view(f: &LlamaFacts) -> LlamaProps {
    LlamaProps {
        vision: f.vision,
        audio: f.audio,
        video: f.video,
        caps: f.caps.clone(),
        n_ctx_slot: f.n_ctx_slot,
        build_info: f.build_info.clone(),
    }
}
