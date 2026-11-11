//! `Snapshot`: the immutable in-memory view built from the store, and its
//! lookup / routing-resolution methods.

use std::collections::{HashMap, HashSet};

use crate::error::GatewayError;
use crate::ir::Params;

use super::routes::UNHELD_BASE_URL;
use super::*;

/// Immutable point-in-time view of all gateway config.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub upstreams: HashMap<i64, Upstream>,
    /// alias, as written (case-sensitive) -> ModelAlias. Every stored row,
    /// enabled or not; [`Snapshot::resolve`] routes the enabled ones.
    pub aliases: HashMap<String, ModelAlias>,
    /// alias (lowercased) -> CandidateAlias (candidate-aliases design §4.1).
    /// Every stored row, enabled or not — same convention as `aliases` —
    /// so a name check sees a disabled candidate alias too; use
    /// [`Snapshot::candidate_alias`] for the routable (enabled-only) lookup.
    pub candidate_aliases: HashMap<String, CandidateAlias>,
    pub local_models: Vec<LocalModel>,
    /// Aux-class models (embedders + rerankers). In the snapshot because
    /// `/v1/embeddings` has to know a resolved model's kind on the hot path —
    /// see [`Snapshot::aux_model_for`].
    pub aux_models: Vec<AuxModel>,
    /// Audio models (audio.cpp `server.json` entries). Unlike the two llama
    /// classes above, no hot path reads this today — it exists so
    /// [`crate::runtime::descriptor::model_runtimes`] can derive audio's
    /// runtime descriptor the same uniform way it derives chat/aux, from one
    /// `Snapshot`, rather than every caller that wants all three classes
    /// together needing a separate `store::list_audio_models` round trip.
    pub audio_models: Vec<AudioModel>,
    /// Image models (sd-server pipelines). On the snapshot for the same
    /// reason `audio_models` is: it is what lets
    /// [`crate::runtime::descriptor::model_runtimes`] derive all four classes'
    /// runtime descriptors from one `Snapshot`.
    pub image_models: Vec<ImageModel>,
    pub api_keys: Vec<ApiKey>,
    /// Price sheets (usage-analytics §2.2). On the snapshot because pricing
    /// happens on the logging path of every request; a DB round trip per
    /// request to read four numbers that change once a month would be absurd.
    pub prices: Vec<PriceRow>,
    pub settings: Settings,
    /// (upstream_id, upstream_model_id) pairs hidden from /v1/models and UI.
    pub hidden_passthrough: HashSet<(i64, String)>,
    /// Upstreams whose owner turned `catalog_all_modalities` off: their
    /// catalog request goes without `output_modalities=all`
    /// ([`crate::catalog::request`], decision-models design §6.2). Every
    /// other upstream, a row not stored too, asks with it.
    pub catalog_bare: HashSet<i64>,
    /// Registered MCP servers (config plane), keyed by `mcp_servers.id`.
    pub mcp_servers: HashMap<i64, McpServer>,
    /// (server_id, upstream tool name) → override (hide / rename).
    pub mcp_tool_overrides: HashMap<(i64, String), McpToolOverride>,
    /// Fully-qualified tool name → the owner's disable record (`tool_disabled`).
    /// Presence means disabled; an empty map is "everything is offered", which
    /// is where every install starts.
    pub disabled_tools: HashMap<String, DisabledTool>,
    /// Every personality profile (personality-profiles design D10), in name
    /// order: voice resolution and prompt assembly read a thread's profile
    /// synchronously, and an edit reaches a bound session's cached stages
    /// as any config change does. Looked up with [`Snapshot::chat_profile`].
    pub chat_profiles: Vec<ChatProfile>,
    /// A benchmark run's GPU lease — runtime state, never loaded from the
    /// store: carried over by every publish and read through
    /// [`Snapshot::gpu_block`] (see [`crate::bench::lease`]).
    pub gpu_lease: Option<std::sync::Arc<crate::bench::lease::GpuLease>>,
    /// The main TLS listener while one serves (headless server design §4.3):
    /// runtime state like `gpu_lease`, never loaded from the store, set and
    /// cleared by the start that serves it
    /// ([`crate::state::AppState::set_serving_tls`]) and carried over by every
    /// publish. Every URL builder reads its TLS part from here, so a link
    /// names a listener that answers.
    pub serving_tls: Option<crate::net::ServingTls>,
    /// `settings.trusted_proxies` read as networks (headless server design
    /// §4.5): derived from the settings at every publish
    /// ([`crate::state::AppState`]'s), so the main router's edge reads
    /// them on every request without parsing the list each time.
    pub trusted_nets: Vec<TrustedNet>,
    /// Every `accounts` row (headless server design §5.1), loaded with the
    /// keys: resolution names a disabled account from here.
    pub accounts: Vec<crate::accounts::Account>,
    /// Accounts on (D25): container mode, or `settings.accounts.enabled`.
    /// Derived at every publish ([`crate::state::AppState`]'s).
    pub accounts_on: bool,
    /// The live sessions (§5.2): runtime state, the gateway's one desk,
    /// set at every publish; a snapshot built by hand has an empty one.
    pub sessions: std::sync::Arc<crate::accounts::sessions::SessionDesk>,
}

/// One row of `tool_disabled`: the owner switched this tool off, and where it
/// came from at the time.
#[derive(Debug, Clone, PartialEq)]
pub struct DisabledTool {
    /// Source label recorded when the switch was flipped (`lmgw`, `docs`, or a
    /// server's label). Kept so a row outliving its source can still say what
    /// it belonged to instead of becoming an anonymous name.
    pub source: String,
    pub disabled_at: String,
}

/// `<prefix>/<id>`, or bare `<id>` when the class configures no prefix: the
/// rule the aux editor names a draft by ([`lmgw_api_types::public_names`]).
use lmgw_api_types::public_names::prefixed;

/// The model id inside a class-prefixed public name, or `None` when the name
/// is not under that class's prefix at all.
fn strip_prefix<'a>(prefix: &str, name: &'a str) -> Option<&'a str> {
    match prefix.trim_matches('/') {
        "" => Some(name),
        p => name.strip_prefix(p)?.strip_prefix('/'),
    }
}

mod disabled;
mod hops;
pub use hops::*;
mod sheet;

impl Snapshot {
    /// Is this upstream id one of ours — a container on this machine?
    ///
    /// The four local classes route through **synthetic** upstreams
    /// ([`ROUTER_UPSTREAM_ID`], [`AUX_UPSTREAM_ID`], [`AUDIO_UPSTREAM_ID`],
    /// [`IMAGE_UPSTREAM_ID`])
    /// that are built on the fly and never stored in `upstreams`, so a plain
    /// `upstreams.get(id)` misses every local request. Checking the kind alone
    /// priced all local traffic as *unpriced* rather than free — the one
    /// mistake this feature cannot afford, because it turns the honest
    /// "unknown is not zero" rule into a permanent false alarm over exactly
    /// the traffic that is genuinely free.
    pub fn is_local_upstream(&self, id: i64) -> bool {
        if matches!(
            id,
            ROUTER_UPSTREAM_ID | AUX_UPSTREAM_ID | AUDIO_UPSTREAM_ID | IMAGE_UPSTREAM_ID
        ) {
            return true;
        }
        self.upstreams.get(&id).is_some_and(|u| {
            matches!(
                u.kind,
                UpstreamKind::LlamaServer | UpstreamKind::AudioCpp | UpstreamKind::SdCpp
            )
        })
    }

    /// The `api_keys.id` behind a `client_key` name, including the synthetic
    /// `internal:*` identities.
    pub fn key_id_for_name(&self, name: &str) -> Option<i64> {
        self.api_keys.iter().find(|k| k.name == name).map(|k| k.id)
    }

    /// Resolve a client-supplied model name (§4). Precedence: explicit alias,
    /// then local models of any class, then expose-all upstream passthrough.
    ///
    /// Aux and audio models sat in the third tier until per-model containers
    /// §5: they were reached through a managed `expose_all` upstream row, so a
    /// cloud catalog sharing their prefix could win. Those rows are gone and
    /// the three classes now resolve alike, from their own tables — which
    /// means a local model of a given name beats a passthrough catalog entry
    /// of the same name. Deliberate: prefixes are named by the owner.
    ///
    /// **A candidate alias never resolves here (candidate-aliases design
    /// §4.1, §12).** There is no one static [`Route`] to hand back — which of
    /// its candidates answers is decided per request by the gate worker,
    /// which does not call this function for the alias name itself (it looks
    /// the candidates up directly and builds their route with
    /// [`Self::chat_local_route`]). Every caller here that needs one fixed
    /// model instead — quickdoc's ingest/golden plans, corpus creation, the
    /// dashboard chat's model picker, [`Self::hop`] — gets a
    /// named refusal instead of `UnknownAlias`, so "this name exists but
    /// picks a model per request" reads differently from "no such name".
    pub fn resolve(&self, alias: &str) -> Result<Route, GatewayError> {
        if let Some(ca) = self.candidate_alias(alias) {
            return Err(GatewayError::BadRequest(format!(
                "'{alias}' is a candidate alias — it picks a model per request rather than \
                 naming one fixed route; name one of its candidates directly instead ({})",
                ca.candidates.join(", ")
            )));
        }
        if let Some(entry) = self.aliases.get(alias).filter(|a| a.enabled) {
            let upstream = self
                .upstreams
                .get(&entry.upstream_id)
                .filter(|u| u.enabled)
                .ok_or_else(|| {
                    GatewayError::Internal(format!(
                        "alias '{alias}' points at missing/disabled upstream {}",
                        entry.upstream_id
                    ))
                })?;
            return Ok(Route {
                upstream: upstream.clone(),
                upstream_model: entry.upstream_model_id.clone(),
                param_defaults: entry.param_overrides.clone(),
                fallback: None,
            });
        }
        self.resolve_local(alias)
            .or_else(|| self.resolve_passthrough(alias))
            .ok_or_else(|| GatewayError::UnknownAlias(alias.to_string()))
    }

    /// [`Snapshot::resolve`] plus the GPU-hold rule (gpu-hold design §4),
    /// along the chain (openai egress design §14.3): a route that lands on a
    /// local container while the hold is on (or a benchmark holds the card,
    /// [`Self::gpu_block`]) goes on to that model's own fallback, hop by hop,
    /// until a hop can answer — a cloud model, a row on the CPU — or one has
    /// none that can, which refuses with [`GatewayError::GpuHold`] /
    /// [`GatewayError::GpuBenchmark`] naming that hop and the chain. A
    /// disabled model or alias, asked for or met as a fallback, goes on to
    /// its own fallback the same way ([`Self::disabled_key`],
    /// [`Self::past_disabled`]).
    ///
    /// The static part of the gate's walk ([`crate::gate::resolve`] runs the
    /// same steps with each hop's route check): for the readers that only
    /// show what a request would get — a realtime warm-up, the Chat's
    /// attachment and voice checks. It starts nothing and asks no upstream.
    ///
    /// **Why the swap is at resolve and not inside
    /// [`vram::admit`](crate::vram::admit).** Four call sites read the route
    /// *before* they admit: legacy `/v1/completions` and the audio handlers
    /// refuse a non-`openai` upstream protocol, the dashboard chat injects a
    /// llama.cpp-only passthrough field keyed on `upstream.kind`, and the two
    /// quickdoc plans capture the route into an immutable plan struct.
    /// Swapping the route inside `admit` would leave every one of those
    /// decisions made against the held route while the bytes went to another
    /// one. Swapping at the moment the route is born means everything
    /// downstream sees the hop that answers exactly as if the client had
    /// asked for it by name.
    pub fn resolve_for_request(&self, alias: &str) -> Result<Resolved, GatewayError> {
        let mut chain: Vec<String> = Vec::new();
        let mut seen: Vec<HopKey> = Vec::new();
        let (mut route, mut name) = match self.resolve(alias) {
            Ok(route) => (route, alias.to_string()),
            Err(e) => {
                let Some(key) = self.disabled_key(alias) else {
                    return Err(e);
                };
                seen.push(key.clone());
                let (passed, next) = self.past_disabled(self.next_hop(&key), |k| *k == key);
                for (n, k) in passed {
                    chain.push(n);
                    seen.push(k);
                }
                match next {
                    FallbackRoute::Usable {
                        alias: fb,
                        key: next,
                        route,
                    } if !seen.contains(&next) => {
                        seen.push(next);
                        chain.push(fb.clone());
                        (route, fb)
                    }
                    _ => return Err(e),
                }
            }
        };
        while let Some((block, key)) = self.blocked_at_resolve(&route) {
            seen.push(key.clone());
            let (passed, next) = self.past_disabled(self.next_hop(&key), |k| seen.contains(k));
            for (n, k) in passed {
                chain.push(n);
                seen.push(k);
            }
            let tried = tried_clause(alias, &chain);
            match next {
                FallbackRoute::None => return Err(block.refusal(&name, tried)),
                FallbackRoute::Unusable { alias: fb, why, .. } => {
                    return Err(block.refusal(&name, format!(" (fallback '{fb}' {why}){tried}")))
                }
                FallbackRoute::Usable {
                    alias: fb,
                    key: next,
                    route: next_route,
                } => {
                    if seen.contains(&next) {
                        return Err(block.refusal(&name, already_tried(&fb, alias, &chain)));
                    }
                    chain.push(fb.clone());
                    name = fb;
                    route = next_route;
                }
            }
        }
        Ok(Resolved {
            route,
            fallback: chain.last().cloned(),
            chain,
        })
    }

    /// Whether the hop `route` lands on cannot answer now for a reason the
    /// snapshot decides alone (§14.3): a local model on the GPU while the
    /// hold is on or a benchmark holds the card. `None` for every hop that
    /// can be tried — a cloud route, a row on the CPU (the hold has no claim
    /// on it), anything with no block.
    pub fn blocked_at_resolve(
        &self,
        route: &Route,
    ) -> Option<(crate::bench::lease::GpuBlock, HopKey)> {
        let target = crate::vram::classify(route)?;
        let block = self.gpu_block_for(target.class, &target.model_id)?;
        Some((block, HopKey::Local(target.class, target.model_id)))
    }

    /// The enabled candidate alias named `name` (case-insensitive) —
    /// candidate-aliases design §4.1. `None` for an unknown name *or* a
    /// disabled row: a disabled candidate alias is not routable, exactly like
    /// a disabled plain alias in [`Self::resolve`]. Save-time uniqueness
    /// checks read `candidate_aliases` directly instead, because a disabled
    /// row still holds its name.
    pub fn candidate_alias(&self, name: &str) -> Option<&CandidateAlias> {
        self.candidate_aliases
            .get(&name.to_lowercase())
            .filter(|c| c.enabled)
    }

    /// The fallback the request for `requested` takes from the local model
    /// `target` it holds (design §5's ordered gate): a candidate alias's own
    /// when `requested` names an enabled one — a candidate's own row
    /// fallback is never consulted through it (candidate-aliases §4.1) —
    /// else the row's. The ladder's climb hook and the Chat's voice checks
    /// read it.
    pub fn request_fallback(&self, requested: &str, target: &crate::vram::Target) -> FallbackRoute {
        match self.candidate_alias(requested) {
            Some(ca) => self.fallback_named(ca.fallback.as_deref()),
            None => self.next_hop(&HopKey::Local(target.class, target.model_id.clone())),
        }
    }

    /// The synthetic upstream backing local chat models — never stored, and
    /// not reachable until a hold overwrites its `base_url` (§5).
    pub fn router_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            ROUTER_UPSTREAM_ID,
            ROUTER_UPSTREAM_NAME,
            Protocol::LlamaCpp,
            UpstreamKind::LlamaServer,
            self.settings.router.request_timeout_seconds,
        )
    }

    /// The synthetic upstream backing aux models (embedders + rerankers).
    pub fn aux_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            AUX_UPSTREAM_ID,
            AUX_UPSTREAM_NAME,
            Protocol::LlamaCpp,
            UpstreamKind::LlamaServer,
            self.settings.aux_router.request_timeout_seconds,
        )
    }

    /// The synthetic upstream backing audio models (audio.cpp).
    pub fn audio_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            AUDIO_UPSTREAM_ID,
            AUDIO_UPSTREAM_NAME,
            Protocol::OpenaiCompatible,
            UpstreamKind::AudioCpp,
            self.settings.audio.request_timeout_seconds,
        )
    }

    /// The synthetic upstream backing image models (stable-diffusion.cpp).
    pub fn image_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            IMAGE_UPSTREAM_ID,
            IMAGE_UPSTREAM_NAME,
            Protocol::OpenaiCompatible,
            UpstreamKind::SdCpp,
            self.settings.image.request_timeout_seconds,
        )
    }

    /// One of the four per-class synthetic upstreams (§5). Never persisted,
    /// never `expose_all` (the classes enumerate their own tables — see
    /// [`Snapshot::exposed_models`]). `protocol` is the caller's:
    /// `llama_cpp` for the two llama.cpp classes, `openai_compatible` for
    /// audio.cpp and sd-server (llama.cpp egress design §5, openai egress
    /// design decision 9).
    ///
    /// `timeout_seconds` is the caller's class setting, not a constant: the
    /// four classes used to share one 600 s ceiling, which was right for none
    /// of them but chat. A 0 travels through as a 0 and
    /// [`Upstream::request_timeout`] reads it as "no deadline".
    fn synthetic_upstream(
        id: i64,
        name: &str,
        protocol: Protocol,
        kind: UpstreamKind,
        timeout_seconds: u64,
    ) -> Upstream {
        Upstream {
            id,
            name: name.into(),
            protocol,
            kind,
            // Overwritten with the acquired container's endpoint before any
            // request is built — see [`UNHELD_BASE_URL`].
            base_url: UNHELD_BASE_URL.into(),
            api_key: None,
            extra_headers: vec![],
            // The class's own ceiling (`<class>.request_timeout_seconds`),
            // bounding the *request*: the container start in front of it is
            // bounded separately by `vram.load_timeout_seconds`, and by the
            // time a request is built the model is already up. Saturating, so
            // an owner who types a decade gets a decade rather than a wrap to
            // something short.
            timeout_ms: timeout_seconds.saturating_mul(1_000),
            enabled: true,
            // Neither llama-server nor audiocpp_server has /v1/responses
            // (ggml-org/llama.cpp#19138); synthesizing it from
            // chat/completions is the whole point of §21.
            supports_responses: false,
            expose_all: false,
            expose_prefix: String::new(),
            llama: None,
        }
    }

    /// The aux model a resolved route lands on, if it lands on one.
    ///
    /// Keyed on the synthetic upstream's *id*, not its name: an owner may have
    /// an upstream of their own called `llama-aux` (migration 0018's repair
    /// exists precisely because that is legal), and a passthrough route to it
    /// must not be mistaken for a local aux route and put through the
    /// embed/rerank cross-guard. The sentinel id cannot be forged by a stored
    /// row. Since §5 local models are not aliasable, this now has exactly one
    /// spelling to cover: the table-driven `<prefix>/<model id>`.
    pub fn aux_model_for(&self, route: &Route) -> Option<&AuxModel> {
        if route.upstream.id != AUX_UPSTREAM_ID {
            return None;
        }
        self.aux_models
            .iter()
            .find(|m| m.model_id == route.upstream_model)
    }

    /// Public client-facing name of an aux row: `decide/<id>` under the
    /// decision prefix for a decision row, `embed/<id>` under the aux prefix
    /// for an embedder or a reranker (decision-models design §3.10). One name
    /// per row: the other prefix does not reach it.
    pub fn aux_public_name(&self, m: &AuxModel) -> String {
        self.aux_name_for(m.kind, &m.model_id)
    }

    /// [`Self::aux_public_name`] of a row of `kind` called `model_id`, for a
    /// row that is not stored yet (a create's name checks).
    pub fn aux_name_for(&self, kind: AuxKind, model_id: &str) -> String {
        prefixed(self.aux_prefix(kind), model_id)
    }

    /// The prefix the aux rows of `kind` are exposed under.
    fn aux_prefix(&self, kind: AuxKind) -> &str {
        let aux = &self.settings.aux_router;
        lmgw_api_types::public_names::aux_prefix(
            kind.as_str(),
            &aux.public_prefix,
            &aux.decision_prefix,
        )
    }

    /// Public client-facing name of a local chat model
    /// (`local/<id>` when a public prefix is configured).
    pub fn local_public_name(&self, model_id: &str) -> String {
        prefixed(&self.settings.router.public_prefix, model_id)
    }

    /// Public client-facing name of an audio model
    /// (`audio/<id>` under the configured audio prefix).
    pub fn audio_public_name(&self, model_id: &str) -> String {
        prefixed(&self.settings.audio.public_prefix, model_id)
    }

    /// Public client-facing name of an image model
    /// (`image/<id>` under the configured image prefix).
    pub fn image_public_name(&self, model_id: &str) -> String {
        prefixed(&self.settings.image.public_prefix, model_id)
    }

    /// The route a local chat model is served on, whether or not it is public.
    ///
    /// Split out of [`Snapshot::resolve_public_local`] so in-process callers
    /// that address a model by its *row* rather than by a client-facing name
    /// (`modelinfo::local_model_test`) build the same route the proxy does,
    /// and therefore go through the same admission and the same container.
    pub fn chat_local_route(&self, model_id: &str) -> Route {
        Route {
            upstream: self.router_upstream(),
            upstream_model: model_id.to_string(),
            param_defaults: Params::default(),
            fallback: None,
        }
    }

    /// The route an aux model (embedder or reranker) is served on — the aux
    /// twin of [`Snapshot::chat_local_route`], for the same in-process callers
    /// (`modelinfo::local_model_test`) that address a row rather than a
    /// client-facing name and must still go through admission and land on
    /// the model's own container.
    pub fn aux_local_route(&self, model_id: &str) -> Route {
        Route {
            upstream: self.aux_upstream(),
            upstream_model: model_id.to_string(),
            param_defaults: Params::default(),
            fallback: None,
        }
    }

    /// Second-tier resolution (§5): the three local classes, each from its own
    /// table under its own class prefix, each onto its class's synthetic
    /// upstream. Chat additionally requires `public`; aux and audio have no
    /// such column and key on `enabled` alone.
    fn resolve_local(&self, name: &str) -> Option<Route> {
        self.resolve_public_local(name)
            .or_else(|| self.resolve_aux_local(name))
            .or_else(|| self.resolve_audio_local(name))
            .or_else(|| self.resolve_image_local(name))
    }

    fn resolve_public_local(&self, name: &str) -> Option<Route> {
        let id = strip_prefix(&self.settings.router.public_prefix, name)?;
        let m = self
            .local_models
            .iter()
            .find(|m| m.enabled && m.public && m.model_id == id)?;
        Some(self.chat_local_route(&m.model_id))
    }

    /// An aux row by its one public name (decision-models design §3.10): the
    /// decision prefix reaches decision rows only, the aux prefix embedders
    /// and rerankers only, so `embed/laya` does not resolve the decision row
    /// `laya`. With both prefixes set alike, both kinds are under it.
    fn resolve_aux_local(&self, name: &str) -> Option<Route> {
        let m =
            self.aux_models.iter().filter(|m| m.enabled).find(|m| {
                strip_prefix(self.aux_prefix(m.kind), name) == Some(m.model_id.as_str())
            })?;
        Some(self.aux_local_route(&m.model_id))
    }

    fn resolve_audio_local(&self, name: &str) -> Option<Route> {
        let id = strip_prefix(&self.settings.audio.public_prefix, name)?;
        let m = self
            .audio_models
            .iter()
            .find(|m| m.enabled && m.model_id == id)?;
        Some(Route {
            upstream: self.audio_upstream(),
            upstream_model: m.model_id.clone(),
            param_defaults: Params::default(),
            fallback: None,
        })
    }

    /// The image class's tier of [`Self::resolve_local`], mirroring
    /// [`Self::resolve_audio_local`]: enabled rows only, under the class
    /// prefix, onto the id -4 synthetic upstream.
    fn resolve_image_local(&self, name: &str) -> Option<Route> {
        let id = strip_prefix(&self.settings.image.public_prefix, name)?;
        let m = self
            .image_models
            .iter()
            .find(|m| m.enabled && m.model_id == id)?;
        Some(Route {
            upstream: self.image_upstream(),
            upstream_model: m.model_id.clone(),
            param_defaults: Params::default(),
            fallback: None,
        })
    }

    fn resolve_passthrough(&self, name: &str) -> Option<Route> {
        // Prefixed catalogs win over bare ones; longest prefix, then upstream
        // name keeps multi-upstream resolution deterministic.
        let mut prefixed: Vec<(&Upstream, &str)> = Vec::new();
        let mut bare: Vec<&Upstream> = Vec::new();
        for u in self
            .upstreams
            .values()
            .filter(|u| u.enabled && u.expose_all)
        {
            match u.prefix() {
                "" => bare.push(u),
                p => {
                    if let Some(rest) = name.strip_prefix(p).and_then(|r| r.strip_prefix('/')) {
                        if !rest.is_empty() {
                            prefixed.push((u, rest));
                        }
                    }
                }
            }
        }
        prefixed.sort_by(|(a, _), (b, _)| {
            b.prefix()
                .len()
                .cmp(&a.prefix().len())
                .then(a.name.cmp(&b.name))
        });
        bare.sort_by(|a, b| a.name.cmp(&b.name));
        let (u, model) = prefixed
            .first()
            .map(|(u, rest)| (*u, rest.to_string()))
            .or_else(|| bare.first().map(|u| (*u, name.to_string())))?;
        Some(Route {
            upstream: u.clone(),
            upstream_model: model,
            param_defaults: Params::default(),
            fallback: None,
        })
    }

    /// Whether the owner has switched this fully-qualified tool name off.
    ///
    /// The one predicate every tool-serving path consults, so "disabled" cannot
    /// mean one thing on `/mcp` and another in a chat thread.
    pub fn tool_disabled(&self, name: &str) -> bool {
        self.disabled_tools.contains_key(name)
    }

    /// Enabled aliases, sorted, for `GET /v1/models`.
    pub fn enabled_aliases(&self) -> Vec<&ModelAlias> {
        let mut v: Vec<&ModelAlias> = self.aliases.values().filter(|a| a.enabled).collect();
        v.sort_by(|a, b| a.alias.cmp(&b.alias));
        v
    }

    /// Enabled + public local chat models (exposed without an alias).
    pub fn public_locals(&self) -> impl Iterator<Item = &LocalModel> {
        self.local_models.iter().filter(|m| m.enabled && m.public)
    }

    /// Enabled aux models. There is no `public` column on the aux table and §5
    /// deliberately does not add one: an aux model is exposed exactly when it
    /// is enabled.
    pub fn enabled_aux_models(&self) -> impl Iterator<Item = &AuxModel> {
        self.aux_models.iter().filter(|m| m.enabled)
    }

    /// Enabled audio models — same rule as [`Snapshot::enabled_aux_models`].
    pub fn enabled_audio_models(&self) -> impl Iterator<Item = &AudioModel> {
        self.audio_models.iter().filter(|m| m.enabled)
    }

    /// Enabled image models — same rule as [`Snapshot::enabled_aux_models`].
    pub fn enabled_image_models(&self) -> impl Iterator<Item = &ImageModel> {
        self.image_models.iter().filter(|m| m.enabled)
    }

    /// Statically known names for `GET /v1/models`: explicit aliases plus
    /// every enabled local model of all four classes (deduped; aliases win).
    /// Passthrough upstream catalogs are appended by the handler, which can
    /// await HTTP.
    ///
    /// Aux and audio used to arrive through the handler's expose-all fan-out —
    /// an HTTP `GET {base}/models` against the class router's always-on port.
    /// Per-model containers have no such port (§5), so the tables are the
    /// catalog, which also makes exposure answerable without a network call.
    pub fn exposed_models(&self) -> Vec<ExposedModel> {
        let mut out: Vec<ExposedModel> = self
            .enabled_aliases()
            .iter()
            .map(|a| ExposedModel {
                name: a.alias.clone(),
                source: "alias",
            })
            .collect();
        // Candidate aliases (candidate-aliases design §4.1, §4.6): every
        // enabled one publishes a chat model entry the same way a plain
        // alias does — `capabilities::exposed::entry_for`'s
        // `"candidate_alias"` arm decides what its capabilities/context
        // actually say. Listed right after plain aliases and before every
        // local class, so name uniqueness (enforced at save time) is backed
        // up here too: neither kind of alias can be shadowed by a local
        // row's public name.
        let mut candidate_names: Vec<&str> = self
            .candidate_aliases
            .values()
            .filter(|c| c.enabled)
            .map(|c| c.alias.as_str())
            .collect();
        candidate_names.sort_unstable();
        for name in candidate_names {
            if !out.iter().any(|e| e.name == name) {
                out.push(ExposedModel {
                    name: name.to_string(),
                    source: "candidate_alias",
                });
            }
        }
        let locals = self
            .public_locals()
            .map(|m| (self.local_public_name(&m.model_id), "local"))
            .chain(
                self.enabled_aux_models()
                    .map(|m| (self.aux_public_name(m), "aux")),
            )
            .chain(
                self.enabled_audio_models()
                    .map(|m| (self.audio_public_name(&m.model_id), "audio")),
            )
            .chain(
                self.enabled_image_models()
                    .map(|m| (self.image_public_name(&m.model_id), "image")),
            );

        for (name, source) in locals {
            if !out.iter().any(|e| e.name == name) {
                out.push(ExposedModel { name, source });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The plaintext of one **enabled owner** row by name (principals §3.1),
    /// or `None` when the row is missing or switched off.
    ///
    /// The one place lmgw reads its own door key back out: the login link the
    /// process log prints, the shell's window, and the MCP transport dialling
    /// lmgw's own `/agents/<id>/mcp` (§10 Part 2) all need the live value, and
    /// none of them may cache it — a rotation has to take effect on the next
    /// use. Reading it from the snapshot is what makes that free.
    ///
    /// `enabled` is part of the lookup, not a caller's job: a disabled row
    /// authenticates nothing, so handing its plaintext out would only produce
    /// a request that is refused one hop later, with the confusing message.
    pub fn owner_key(&self, name: &str) -> Option<&str> {
        self.api_keys
            .iter()
            .find(|k| k.kind == ApiKeyKind::Owner && k.enabled && k.name == name)
            .and_then(|k| k.key_plain.as_ref())
            .map(|s| s.expose())
    }

    /// Verify a presented gateway API key against stored hashes; returns the
    /// matching key (for logging its *name*, never the key).
    pub fn verify_api_key(&self, presented: &str) -> Option<&ApiKey> {
        let hash = hash_api_key(presented);
        self.api_keys
            .iter()
            .find(|k| k.enabled && k.key_hash == hash)
    }
}
