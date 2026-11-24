//! A request's way to a chosen model (the page-reading model, or the
//! picture-description model, `super::Purpose`), and its model-call pool
//! (design §7.5, §7.7, D14, D20, D27). The words below say pages; a
//! picture's description takes the same way, a picture where a page is.
//!
//! **Opened by the first page that needs it, held only while a file has
//! pages for it.** The first page that needs the model opens the route
//! through the gate ([`crate::gate::open`]: the hold's swap along the
//! fallback chain, a candidate alias's pick, GPU admission). What was
//! opened (for a local model its claim on the container) belongs to the
//! file stages that use it: the request keeps only a [`Weak`] to it, and
//! each file's stage ([`Keep`]) holds it while that file has pages waiting
//! or in flight. When the last such stage ends, the claim is given back, so
//! a job that read one scan early and then converts office files for
//! twenty minutes blocks neither the GPU hold nor another model's
//! eviction; the next file with pages for the model opens again. A
//! conversion with no page for the model never opens anything.
//!
//! **The GPU hold.** What was opened is used only while the hold stands as
//! it stood then: a hold switched on or released mid-request makes the next
//! page open again, so pages not yet sent take the held path (the fallback,
//! or `model_held`) and, once it is released, go to the model again (§7.7).
//! Calls already running finish or fail as the hold handles them. A refusal
//! is kept the same way for as long as the stage that met it runs, so a
//! held model costs a file one look, not one admission per page.
//!
//! **Who answers is asked before any page is rendered.** A route that is a
//! fallback whose capabilities say it takes no pictures cannot read a page:
//! the gate would send it the prompt with a placeholder for the image
//! (`gate::fallback_images`), and its answer would be text about nothing.
//! Such a route is refused here ([`Stop::Blind`]), with the fallback's
//! name. So is a route that prices nothing a budgeted key is billed in
//! ([`Stop::NotAllowed`]): that refusal is the same for every page, so it
//! is judged once and no page is rendered for it.
//!
//! **The pool rule (D20).** Each opened route has its own bound on calls in
//! flight, the answering model's slots:
//! - a local llama.cpp model: the slots its container was started with
//!   ([`LlamaParams::effective_slots`](crate::config::LlamaParams::effective_slots):
//!   `-np`, or llama-server's own 4 when the row leaves it to the server);
//! - any other route (a cloud alias, a fallback, an llama-server lmgw does
//!   not run): the tool pool's size, as no upstream has a concurrency
//!   setting yet.
//!
//! A page takes one of them before its image is rendered and gives it back
//! when its answer is read, so at most that many page images exist at once.
//! Pages wait in the order they asked (the files of a request in request
//! order, a file's pages in page order). Across requests and Chat the calls
//! queue where every model call queues, at the model. Dropping the
//! conversion (the client gone, a cancelled job) drops the calls in flight
//! with their slots; each leaves its `499` usage row
//! ([`crate::proxy::sample_once_dropping`]).

use std::sync::{Arc, Weak};

use lmgw_api_types::documents::model_help::BoundFrom;
use tokio::sync::{Mutex, Semaphore};

use super::{Chosen, ENDPOINT};
use crate::error::GatewayError;
use crate::extract::convert::pool;
use crate::gate::{Opened, RouteCheck};

/// Why no page can go to the model now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The GPU hold (or a benchmark's lease) pauses the model and no
    /// fallback answers for it.
    Held(String),
    /// The model cannot run now and the fallback that answers for it takes
    /// no pictures: its name.
    Blind(String),
    /// The key may not be billed on the route that answers (a budgeted key
    /// on a route without a price).
    NotAllowed(String),
    /// The model cannot be opened: its name does not resolve, it cannot
    /// generate text, admission refused it.
    Failed(String),
}

/// The route a request's pages go to.
pub(crate) struct Open {
    pub opened: Opened,
    /// The model that answers, by its lmgw name: the chosen one, or the
    /// fallback the gate handed the request to (`x-lmgw-fallback`'s name).
    pub answered_by: String,
    /// Another model answers for the chosen one.
    pub fallback: bool,
    /// The most calls in flight, and where the number comes from.
    pub bound: u32,
    pub bound_from: BoundFrom,
    pub calls: Arc<Semaphore>,
    /// The answering model's context in tokens, as its running server
    /// reports it or as lmgw's model list has it; `None`: not known.
    pub context: Option<u64>,
    /// Its largest answer in tokens, where its catalog says one.
    pub max_output: Option<u64>,
}

/// One file stage's hold on what the request opened (module doc): alive
/// while the file has pages waiting or in flight.
#[derive(Default)]
pub(crate) struct Keep {
    route: std::sync::Mutex<Option<Arc<Open>>>,
    /// What a refusal met by this stage lives as long as.
    alive: Arc<()>,
}

impl Keep {
    fn hold(&self, open: &Arc<Open>) {
        *self.route.lock().unwrap_or_else(|e| e.into_inner()) = Some(open.clone());
    }
}

enum Outcome {
    Open(Weak<Open>),
    /// A refusal, and the stage that met it.
    Stop(Stop, Weak<()>),
}

struct Cached {
    /// Whether the hold (or a benchmark's lease) stood when this was opened.
    held: bool,
    outcome: Outcome,
}

/// One request's route to the model (module doc).
#[derive(Default)]
pub(crate) struct Session {
    cached: Mutex<Option<Cached>>,
}

fn held_now(chosen: &Chosen) -> bool {
    chosen.state.snapshot().gpu_block().is_some()
}

impl Session {
    /// The route pages go to now, held by `keep`'s stage from here on: what
    /// a stage of the request still holds open, or a new open when none
    /// does or the hold changed since (module doc).
    pub(crate) async fn current(&self, chosen: &Chosen, keep: &Keep) -> Result<Arc<Open>, Stop> {
        let held = held_now(chosen);
        let mut cached = self.cached.lock().await;
        match cached
            .as_ref()
            .filter(|c| c.held == held)
            .map(|c| &c.outcome)
        {
            Some(Outcome::Open(route)) => {
                if let Some(open) = route.upgrade() {
                    keep.hold(&open);
                    return Ok(open);
                }
            }
            Some(Outcome::Stop(stop, by)) if by.strong_count() > 0 => return Err(stop.clone()),
            _ => {}
        }
        *cached = None;
        // Boxed: the gate's open is large, and this runs under a file's
        // conversion on the request's small stack.
        let outcome = Box::pin(open(chosen)).await.map(Arc::new);
        *cached = Some(Cached {
            held,
            outcome: match &outcome {
                Ok(open) => {
                    keep.hold(open);
                    Outcome::Open(Arc::downgrade(open))
                }
                Err(stop) => Outcome::Stop(stop.clone(), Arc::downgrade(&keep.alive)),
            },
        });
        outcome
    }

    /// Whether `open` is still what pages go to: asked by a page that
    /// waited for its turn, so one that waited through a hold's change
    /// takes the new path.
    pub(crate) async fn is_current(&self, open: &Arc<Open>, chosen: &Chosen) -> bool {
        let held = held_now(chosen);
        let cached = self.cached.lock().await;
        cached.as_ref().is_some_and(|c| {
            c.held == held
                && matches!(&c.outcome, Outcome::Open(o) if std::ptr::eq(o.as_ptr(), Arc::as_ptr(open)))
        })
    }

    /// Whether anything was opened or refused for the request yet (a
    /// test's question).
    #[cfg(test)]
    pub(crate) async fn was_asked(&self) -> bool {
        self.cached.lock().await.is_some()
    }

    /// Whether the request holds a route open now (a test's question).
    #[cfg(test)]
    pub(crate) async fn holds_a_route(&self) -> bool {
        let cached = self.cached.lock().await;
        matches!(cached.as_ref().map(|c| &c.outcome), Some(Outcome::Open(o)) if o.strong_count() > 0)
    }
}

async fn open(chosen: &Chosen) -> Result<Open, Stop> {
    let state = &chosen.state;
    let opened = crate::gate::open(
        state,
        &chosen.alias,
        RouteCheck::Text(ENDPOINT),
        chosen.ctx.origin(),
    )
    .await
    .map_err(|f| match f.error {
        e @ (GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }) => {
            Stop::Held(e.to_string())
        }
        e => Stop::Failed(e.to_string()),
    })?;
    // A fallback that takes no pictures cannot read a page (module doc).
    // `opened`, and with it any claim, is given back by the return.
    if let Some(unseen) =
        crate::gate::fallback_images::blind_fallback(state, &opened.route, &chosen.alias).await
    {
        return Err(Stop::Blind(unseen.fallback.to_string()));
    }
    // What every page's call would be refused for on this route.
    crate::proxy::priced_refusal(
        state,
        &chosen.ctx.key_ref(),
        chosen.purpose.proto(),
        &chosen.alias,
        &opened.route,
        crate::telemetry::RequestClass::Chat,
        None,
    )
    .map_err(|e| Stop::NotAllowed(e.to_string()))?;
    let fallback = opened.headers.fallback().is_some();
    let answered_by = opened
        .headers
        .fallback()
        .unwrap_or(&chosen.alias)
        .to_string();
    let started = opened.hold.as_ref().and_then(|h| h.gate_facts());
    let (bound, bound_from) = match &started {
        Some(f) => (
            u32::try_from(f.params.effective_slots()).unwrap_or(1),
            BoundFrom::RowSlots,
        ),
        None => (pool::global().facts().slots, BoundFrom::ToolPool),
    };
    let bound = bound.max(1);
    let server = crate::llama_facts::resolve(state, opened.hold.as_ref(), &opened.route).facts;
    let listed = crate::capabilities::exposed::exposed_entry(state, &answered_by).await;
    let context = server
        .and_then(|f| f.n_ctx_slot)
        .or_else(|| listed.as_ref().and_then(|e| e.context_length));
    Ok(Open {
        opened,
        answered_by,
        fallback,
        bound,
        bound_from,
        calls: Arc::new(Semaphore::new((bound as usize).min(Semaphore::MAX_PERMITS))),
        context,
        max_output: listed.and_then(|e| e.max_output_tokens),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protocol, UpstreamKind};
    use crate::extract::convert::model_help::Reader;
    use crate::proxy::RequestCtx;
    use crate::state::AppState;
    use crate::store::{self, NewAlias, NewUpstream};

    /// A request's reader whose page-reading model is an alias on an
    /// upstream outside lmgw (nothing is ever sent to it here).
    async fn chosen() -> Arc<Chosen> {
        let state = AppState::init_for_tests().await.unwrap();
        let up = store::insert_upstream(
            &state.db,
            &NewUpstream {
                name: "example-up".into(),
                protocol: Protocol::OpenaiCompatible,
                kind: UpstreamKind::Generic,
                // Nothing listens there: a catalog read fails at once.
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                extra_headers: vec![],
                timeout_ms: 1_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
                catalog_all_modalities: true,
            },
        )
        .await
        .unwrap();
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: "example-vision".into(),
                upstream_id: up,
                upstream_model_id: "vision-tgt".into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
                fallback: None,
            },
        )
        .await
        .unwrap();
        let mut s = state.snapshot().settings.clone();
        s.documents.page_reader = "example-vision".into();
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();
        Reader::of(&state, &RequestCtx::default()).pages.unwrap()
    }

    /// S9: what the request opened (a local model's claim with it) lives
    /// only while a file's stage has pages for it, not until the request
    /// ends.
    #[tokio::test]
    async fn the_route_is_held_only_while_a_file_s_stage_holds_it() {
        let chosen = chosen().await;
        let session = &chosen.session;
        assert!(!session.holds_a_route().await);
        // Two files' stages share what the first opened.
        let (first, second) = (Keep::default(), Keep::default());
        let a = session.current(&chosen, &first).await.unwrap();
        let b = session.current(&chosen, &second).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "one open for both files");
        assert!(session.is_current(&a, &chosen).await);
        let route = Arc::downgrade(&a);
        // Their pages are answered: the stages alone hold it now.
        drop((a, b));
        drop(first);
        assert!(session.holds_a_route().await, "one file still has pages");
        drop(second);
        // No file has pages for the model: the route, and with it a local
        // model's claim, is given back while the request goes on.
        assert!(!session.holds_a_route().await);
        assert_eq!(route.strong_count(), 0);
        // The next file with pages for the model opens again.
        let third = Keep::default();
        let c = session.current(&chosen, &third).await.unwrap();
        assert!(session.holds_a_route().await);
        assert!(session.is_current(&c, &chosen).await);
    }

    /// WP11: the picture-description model shares the page-reading model's
    /// route and pool when both name the same model, and has its own when
    /// not.
    #[tokio::test]
    async fn one_model_chosen_for_pages_and_pictures_is_one_session() {
        let chosen = chosen().await;
        let state = chosen.state.clone();
        let mut s = state.snapshot().settings.clone();
        s.documents.picture_alias = "example-vision".into();
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();
        let r = Reader::of(&state, &RequestCtx::default());
        let (pages, pictures) = (r.pages.unwrap(), r.pictures.unwrap());
        assert!(Arc::ptr_eq(&pages.session, &pictures.session));
        assert_eq!(pictures.purpose, super::super::Purpose::Pictures);
        assert!(!pictures.doctags);
        s.documents.picture_alias = "example-other".into();
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();
        let r = Reader::of(&state, &RequestCtx::default());
        assert!(!Arc::ptr_eq(
            &r.pages.unwrap().session,
            &r.pictures.unwrap().session
        ));
    }

    /// A refusal is kept for the stage that met it, and asked again by the
    /// next file.
    #[tokio::test]
    async fn a_refusal_lasts_as_long_as_the_stage_that_met_it() {
        let chosen = chosen().await;
        let state = chosen.state.clone();
        sqlx::query("UPDATE models SET enabled = 0 WHERE alias = 'example-vision'")
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        let first = Keep::default();
        let Err(Stop::Failed(why)) = chosen.session.current(&chosen, &first).await else {
            panic!("a model that does not resolve was opened");
        };
        // The model is back; the stage that met the refusal keeps it...
        sqlx::query("UPDATE models SET enabled = 1 WHERE alias = 'example-vision'")
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        let again = chosen.session.current(&chosen, &first).await;
        assert_eq!(again.err(), Some(Stop::Failed(why)));
        // ...and the next file's stage asks anew.
        drop(first);
        let second = Keep::default();
        assert!(chosen.session.current(&chosen, &second).await.is_ok());
    }
}
