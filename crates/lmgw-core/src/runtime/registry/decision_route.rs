//! Whether a decision row's llama-server has `POST /v1/systemone`
//! (decision-models design §3.6, "at start").
//!
//! The route came with llama.cpp b11361, and the class default image's tag
//! moves, so an install's pulled copy may predate it. Nothing is keyed on a
//! build number: the server is asked. Once per start and adoption, right after
//! its `/props` and in what is left of the same readiness budget
//! (`llama_props.rs`), the registry reads the container's `GET /v1/models`.
//! A build that serves the route publishes `decisions` among its model's
//! `architecture.output_modalities` (Appendix A.2). When it does not, the
//! entry gets a warning that names the image and its `build_info`, shown with
//! the entry's other warnings on Status and the Models page. As with `/props`,
//! a failed read is a warning and never a failed start, and the local model
//! test reads it again ([`Registry::refresh_decision_route`]).

use std::time::{Duration, Instant};

use serde::Deserialize;

use super::*;
use crate::config::AuxKind;
use crate::runtime::argv::{EngineArgs, LlamaArgs};
use crate::runtime::Class;

/// What a decision row's `GET /v1/models` said (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionRoute {
    /// A listed model outputs `decisions`: the image has the route.
    Listed,
    /// None does: the warning names the image and its build.
    Missing(String),
    /// The read failed, or was not asked; the warning says so.
    Unread(String),
}

impl DecisionRoute {
    /// The entry's warning, when there is one.
    pub fn warning(&self) -> Option<&String> {
        match self {
            Self::Listed => None,
            Self::Missing(w) | Self::Unread(w) => Some(w),
        }
    }
}

/// Whether a start of `engine` runs a decision row, the starts that read
/// the route.
pub(super) fn serves_decisions(engine: &EngineArgs) -> bool {
    matches!(
        engine,
        EngineArgs::Llama(LlamaArgs::Aux {
            kind: AuxKind::Decision,
            ..
        })
    )
}

/// The part of llama-server's `GET /v1/models` this reads: each listed
/// model's `architecture.output_modalities`.
#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    #[serde(default)]
    architecture: Option<Architecture>,
}

#[derive(Deserialize)]
struct Architecture {
    #[serde(default)]
    output_modalities: Vec<String>,
}

/// What `body` says: does a listed model output `decisions`?
fn lists_decisions(body: &[u8]) -> Result<bool, String> {
    let list: ModelList =
        serde_json::from_slice(body).map_err(|e| format!("its body is not a model list ({e})"))?;
    Ok(list.data.iter().any(|m| {
        m.architecture
            .as_ref()
            .is_some_and(|a| a.output_modalities.iter().any(|o| o == "decisions"))
    }))
}

/// The check's outcome for a server running `image` (`build_info` from its
/// `/props`, when read), from what its `GET /v1/models` answered.
fn route_of(image: &str, build_info: Option<&str>, read: Result<bool, String>) -> DecisionRoute {
    let build = build_info.unwrap_or("build unknown");
    match read {
        Ok(true) => DecisionRoute::Listed,
        Ok(false) => DecisionRoute::Missing(format!(
            "the image '{image}' ({build}) lists no decisions among its model's output \
             modalities in GET /v1/models, so its llama-server has no POST /v1/systemone (it \
             came with llama.cpp b11361) and every decision request answers 501 — pick a newer \
             image for the row or the aux class on the Backends page"
        )),
        Err(why) => DecisionRoute::Unread(format!(
            "GET /v1/models could not be read ({why}), so whether the image '{image}' \
             ({build}) serves POST /v1/systemone is unknown until it starts again or a local \
             model test reads it"
        )),
    }
}

/// What [`Registry::refresh_decision_route`] did with a newer read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefresh {
    /// The entry holds the newer read.
    Stored,
    /// The read failed and the answer the entry had is kept, as a failed
    /// `/props` read keeps the facts read before.
    Kept,
    /// That container's entry is gone, or holds no decision row.
    Gone,
}

impl Registry {
    /// Read `GET /v1/models` from the managed container published on `port`,
    /// `timeout` bounding the whole exchange, as the check of a server
    /// running `image` (module doc).
    pub async fn read_decision_route(
        &self,
        port: u16,
        timeout: Duration,
        image: &str,
        build_info: Option<&str>,
    ) -> DecisionRoute {
        let read = async {
            let resp = crate::egress::with_timeout(
                self.http.get(format!("http://127.0.0.1:{port}/v1/models")),
                Some(timeout),
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.bytes().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            lists_decisions(&body)
        };
        route_of(image, build_info, read.await)
    }

    /// [`Self::read_decision_route`] bounded by what is left until `deadline`,
    /// the readiness budget a start or an adoption began with. Not asked once
    /// the budget is spent.
    pub(super) async fn read_decision_route_by(
        &self,
        port: u16,
        deadline: Instant,
        image: &str,
        build_info: Option<&str>,
    ) -> DecisionRoute {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return route_of(
                image,
                build_info,
                Err(
                    "not asked: vram.load_timeout_seconds was spent by the time the server \
                     was up"
                        .into(),
                ),
            );
        }
        self.read_decision_route(port, left, image, build_info)
            .await
    }

    /// The check of a start or an adoption of `render` on `port` (module
    /// doc): `None` for every row but a decision row. `props` is the
    /// server's `/props` read just before, for its `build_info`.
    pub(super) async fn decision_route_of(
        &self,
        render: &crate::runtime::argv::RenderSpec,
        port: u16,
        deadline: Instant,
        props: Option<&Result<Arc<crate::egress::llama_cpp::props::LlamaFacts>, String>>,
    ) -> Option<DecisionRoute> {
        if !serves_decisions(&render.engine) {
            return None;
        }
        let build = props
            .and_then(|p| p.as_ref().ok())
            .and_then(|f| f.build_info.as_deref());
        Some(
            self.read_decision_route_by(port, deadline, &render.image, build)
                .await,
        )
    }

    /// Refresh what container `generation` of `(class, model_id)` said about
    /// its route with a newer read (the local model test's): an answer
    /// replaces what the entry had, a failed read replaces only a failed one.
    pub fn refresh_decision_route(
        &self,
        class: Class,
        model_id: &str,
        generation: u64,
        route: DecisionRoute,
    ) -> RouteRefresh {
        let mut map = self.map();
        let Some(slot) = map
            .get_mut(&(class, model_id.to_string()))
            .filter(|e| e.generation == generation)
            .and_then(|e| e.llama.as_mut())
            .and_then(|l| l.decision_route.as_mut())
        else {
            return RouteRefresh::Gone;
        };
        match (&*slot, &route) {
            (DecisionRoute::Listed | DecisionRoute::Missing(_), DecisionRoute::Unread(_)) => {
                RouteRefresh::Kept
            }
            _ => {
                *slot = route;
                RouteRefresh::Stored
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_decision_model_has_the_route_and_a_text_one_has_not() {
        // Appendix A.2, trimmed.
        let laya = br#"{"object": "list", "data": [{"id": "laya", "object": "model",
            "architecture": {"input_modalities": ["text"], "output_modalities": ["decisions"]}}],
            "models": [{"name": "laya", "capabilities": ["completion"]}]}"#;
        assert_eq!(lists_decisions(laya), Ok(true));
        let old = br#"{"object": "list", "data": [{"id": "laya", "object": "model"}]}"#;
        assert_eq!(lists_decisions(old), Ok(false));
        assert!(lists_decisions(b"File Not Found").is_err());

        let missing = route_of("img:old", Some("b11226-0c6a6a7"), Ok(false));
        let w = missing.warning().unwrap();
        assert!(w.contains("img:old") && w.contains("b11226-0c6a6a7"), "{w}");
        assert_eq!(route_of("img", None, Ok(true)).warning(), None);
    }
}
