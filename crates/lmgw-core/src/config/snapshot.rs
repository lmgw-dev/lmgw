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
    /// alias (lowercased) -> ModelAlias
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
    /// Registered MCP servers (config plane), keyed by `mcp_servers.id`.
    pub mcp_servers: HashMap<i64, McpServer>,
    /// (server_id, upstream tool name) → override (hide / rename).
    pub mcp_tool_overrides: HashMap<(i64, String), McpToolOverride>,
    /// Fully-qualified tool name → the owner's disable record (`tool_disabled`).
    /// Presence means disabled; an empty map is "everything is offered", which
    /// is where every install starts.
    pub disabled_tools: HashMap<String, DisabledTool>,
    /// A benchmark run's GPU lease — runtime state, never loaded from the
    /// store: carried over by every publish and read through
    /// [`Snapshot::gpu_block`] (see [`crate::bench::lease`]).
    pub gpu_lease: Option<std::sync::Arc<crate::bench::lease::GpuLease>>,
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

/// `<prefix>/<id>`, or bare `<id>` when the class configures no prefix.
fn prefixed(prefix: &str, id: &str) -> String {
    match prefix.trim_matches('/') {
        "" => id.to_string(),
        p => format!("{p}/{id}"),
    }
}

/// The model id inside a class-prefixed public name, or `None` when the name
/// is not under that class's prefix at all.
fn strip_prefix<'a>(prefix: &str, name: &'a str) -> Option<&'a str> {
    match prefix.trim_matches('/') {
        "" => Some(name),
        p => name.strip_prefix(p)?.strip_prefix('/'),
    }
}

impl Snapshot {
    /// The price sheet that applies to one resolved route, or `None` when the
    /// scope has no price at all — which is *not* zero (§2.3).
    ///
    /// Order: the alias is more specific than the upstream model it resolves
    /// to, and within one scope the owner's manual row beats the catalog's.
    pub fn prices_for(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
    ) -> Option<crate::pricing::Prices> {
        use crate::pricing::{PriceSource, Prices};

        // Anything served out of a container on this machine is free, and no
        // price row can make it otherwise. What a local request really costs is
        // GPU time, which is recorded as the llama.cpp timings instead (§2.4).
        if upstream_id.is_some_and(|id| self.is_local_upstream(id)) {
            return Some(Prices::free_local());
        }

        let alias_key = alias.to_lowercase();
        let up_key = match (upstream_id, upstream_model) {
            (Some(id), Some(m)) => Some(upstream_scope_key(id, m)),
            _ => None,
        };

        let pick = |scope: PriceScope, key: &str, source: PriceSource| {
            self.prices.iter().find(|p| {
                p.scope_kind == scope
                    && p.source == source
                    && p.unit == "per_mtok"
                    && p.scope_key.eq_ignore_ascii_case(key)
            })
        };

        // Usability is part of *selecting* a row, not a test applied after. An
        // owner who "undoes" a manual override by clearing its fields instead
        // of deleting it would otherwise have an all-`None` row shadow a
        // perfectly good catalog row, and every request on that alias would
        // record `price_source='unknown'` — a silent hole in the spend total
        // that reads as "the catalog stopped publishing".
        let usable = |row: &PriceRow| {
            let p = Prices {
                price_in: row.price_in,
                price_out: row.price_out,
                price_cache_read: row.price_cache_read,
                price_cache_write: row.price_cache_write,
                source: row.source,
            };
            p.is_usable().then_some(p)
        };

        pick(PriceScope::Alias, &alias_key, PriceSource::Manual)
            .and_then(usable)
            .or_else(|| pick(PriceScope::Alias, &alias_key, PriceSource::Catalog).and_then(usable))
            .or_else(|| {
                up_key
                    .as_deref()
                    .and_then(|k| pick(PriceScope::UpstreamModel, k, PriceSource::Manual))
                    .and_then(usable)
            })
            .or_else(|| {
                up_key
                    .as_deref()
                    .and_then(|k| pick(PriceScope::UpstreamModel, k, PriceSource::Catalog))
                    .and_then(usable)
            })
    }

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
    /// dashboard chat's model picker, [`Self::usable_fallback`] — gets a
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
            });
        }
        self.resolve_local(alias)
            .or_else(|| self.resolve_passthrough(alias))
            .ok_or_else(|| GatewayError::UnknownAlias(alias.to_string()))
    }

    /// [`Snapshot::resolve`] plus the GPU-hold rule (gpu-hold design §4): a
    /// route that lands on a local container while `hold.active` becomes its
    /// fallback's route, or [`GatewayError::GpuHold`] when it has no usable
    /// one. Cloud routes and an inactive hold return exactly what `resolve`
    /// returned. A benchmark's lease ([`Self::gpu_block`], benchmark design
    /// §3.2) is answered the same way, refusing with
    /// [`GatewayError::GpuBenchmark`].
    ///
    /// **Why the swap lives here and not inside
    /// [`vram::admit`](crate::vram::admit).** Four call sites read the route
    /// *before* they admit: legacy `/v1/completions` and the audio handlers
    /// refuse a non-`openai` upstream protocol, the dashboard chat injects a
    /// llama.cpp-only passthrough field keyed on `upstream.kind`, and the two
    /// quickdoc plans capture the route into an immutable plan struct.
    /// Swapping the route inside `admit` would leave every one of those
    /// decisions made against the *local* route while the bytes went to a
    /// cloud one — a llama.cpp-only field posted to Anthropic, a protocol
    /// check that passed for the wrong upstream. Swapping at the moment the
    /// route is born means everything downstream sees a cloud route exactly as
    /// if the client had asked for the fallback alias by name.
    ///
    /// The fallback is re-validated here, not trusted from set time
    /// ([`crate::ops::validate_fallback_alias`]): rows and aliases change
    /// between the two, and a fallback that has since become local or been
    /// deleted must surface as a named refusal rather than silently start the
    /// container the hold exists to prevent.
    pub fn resolve_for_request(&self, alias: &str) -> Result<Resolved, GatewayError> {
        let route = self.resolve(alias)?;
        let Some(target) = crate::vram::classify(&route) else {
            // A cloud route is untouched by the hold — it costs no VRAM.
            return Ok(Resolved {
                route,
                fallback: None,
            });
        };
        // Per model: an audio row on the CPU is not held, and its fallback
        // goes unused (`gpu_block_for`).
        let Some(block) = self.gpu_block_for(target.class, &target.model_id) else {
            return Ok(Resolved {
                route,
                fallback: None,
            });
        };
        match self.fallback_route(&target) {
            // Named as `alias`, the name the client asked for, not
            // `target.model_id`: for aux, audio and image those differ by the
            // class's public prefix (`embed/`, …), and a refusal naming the
            // bare row id reads as if it named a different model than the one
            // the request actually used.
            FallbackRoute::None => Err(block.refusal(alias, "")),
            // Named rather than downgraded to a bare refusal, because it is a
            // misconfiguration the owner can fix. A local fallback in
            // particular would put the GPU straight back to work, which is the
            // one thing the hold is for.
            FallbackRoute::Unusable { alias: fb, why } => {
                Err(block.refusal(alias, format!(" (fallback '{fb}' {why})")))
            }
            FallbackRoute::Usable { alias: fb, route } => Ok(Resolved {
                route,
                fallback: Some(fb),
            }),
        }
    }

    /// The fallback of the local model `target`, as far as it can stand in
    /// for it (gpu-hold design §2, "usable fallback"): the row's lookup
    /// ([`Self::hold_fallback_for`]), then [`Self::usable_fallback`].
    ///
    /// The one lookup every fallback goes through — the hold's swap here in
    /// [`Self::resolve_for_request`], the outside-VRAM swap at admission
    /// (candidate-aliases design §4.7, `gate::open::usable_fallback`) — so
    /// "which alias answers for this model, and can it" has one answer.
    /// Re-validated per call, never trusted from set time: rows and aliases
    /// change in between.
    pub fn fallback_route(&self, target: &crate::vram::Target) -> FallbackRoute {
        let Some(alias) = self.hold_fallback_for(target.class, &target.model_id) else {
            return FallbackRoute::None;
        };
        match self.usable_fallback(&alias) {
            Ok(route) => FallbackRoute::Usable { alias, route },
            Err(why) => FallbackRoute::Unusable { alias, why },
        }
    }

    /// Whether the alias `fallback` can answer for a local model: it resolves,
    /// to a route that is not itself local, and is not itself another
    /// candidate alias (candidate-aliases design §4.1 — a candidate alias has
    /// no static route [`resolve`](Self::resolve) could return anyway, since
    /// which of its candidates answers is decided per request; naming the
    /// clause here rather than letting it fall through to "does not resolve"
    /// gives the owner the actual reason). `Err` is why not, as the clause a
    /// refusal names it with.
    pub fn usable_fallback(&self, fallback: &str) -> Result<Route, &'static str> {
        if self
            .candidate_aliases
            .contains_key(&fallback.to_lowercase())
        {
            return Err("is itself a candidate alias");
        }
        let route = self.resolve(fallback).map_err(|_| "does not resolve")?;
        if crate::vram::classify(&route).is_some() {
            return Err("is itself a local model");
        }
        Ok(route)
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

    /// Whether any enabled candidate alias has `background` set — the
    /// runtime worker's (C1) gate for arming draining/ownership machinery at
    /// all: with none, background traffic cannot exist, so there is nothing
    /// to drain for (candidate-aliases design §4.4–§4.5).
    pub fn any_background_alias(&self) -> bool {
        self.candidate_aliases
            .values()
            .any(|c| c.enabled && c.background)
    }

    /// The alias fallback of one candidate alias (design §4.1–§4.3): the
    /// single fallback that answers for the **whole** alias — a candidate's
    /// own row fallback is never consulted through it. Same three modes as a
    /// row's GPU-hold fallback: `inherit` reads `settings.hold.fallback_alias`
    /// unconditionally (a candidate alias's candidates are chat-class rows
    /// only, so there is exactly one global to inherit — unlike
    /// [`Self::hold_fallback_for`]'s per-class table, which only chat
    /// inherits), `none` never leaves the machine, `alias` is the row's own
    /// named fallback. Re-validated per call through [`Self::usable_fallback`],
    /// never trusted from save time — rows and aliases change in between.
    pub fn alias_fallback(&self, alias: &CandidateAlias) -> FallbackRoute {
        let name = match alias.fallback_mode {
            HoldFallbackMode::None => None,
            HoldFallbackMode::Alias => alias.fallback.clone().filter(|a| !a.trim().is_empty()),
            HoldFallbackMode::Inherit => self
                .settings
                .hold
                .fallback_alias
                .clone()
                .filter(|a| !a.trim().is_empty()),
        };
        let Some(name) = name else {
            return FallbackRoute::None;
        };
        match self.usable_fallback(&name) {
            Ok(route) => FallbackRoute::Usable { alias: name, route },
            Err(why) => FallbackRoute::Unusable { alias: name, why },
        }
    }

    /// The one fallback lookup a request against `requested` takes, wherever
    /// one is needed (design §5's ordered gate): the hold swap, the
    /// outside-VRAM swap (§4.7), and the ladder's climb hook all call this
    /// instead of [`Self::fallback_route`] directly, so a candidate alias's
    /// row fallback is never consulted through it (§4.1) no matter which of
    /// the three triggered the fallback.
    ///
    /// `requested` naming an **enabled** candidate alias reads that alias's
    /// own fallback ([`Self::alias_fallback`]); anything else — a direct
    /// model name, a plain alias, a disabled candidate alias name (which
    /// [`Self::candidate_alias`] treats as unknown) — reads exactly
    /// [`Self::fallback_route`] as before phase 4.
    pub fn request_fallback(&self, requested: &str, target: &crate::vram::Target) -> FallbackRoute {
        match self.candidate_alias(requested) {
            Some(ca) => self.alias_fallback(ca),
            None => self.fallback_route(target),
        }
    }

    /// The synthetic upstream backing local chat models — never stored, and
    /// not reachable until a hold overwrites its `base_url` (§5).
    pub fn router_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            ROUTER_UPSTREAM_ID,
            ROUTER_UPSTREAM_NAME,
            UpstreamKind::LlamaServer,
            self.settings.router.request_timeout_seconds,
        )
    }

    /// The synthetic upstream backing aux models (embedders + rerankers).
    pub fn aux_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            AUX_UPSTREAM_ID,
            AUX_UPSTREAM_NAME,
            UpstreamKind::LlamaServer,
            self.settings.aux_router.request_timeout_seconds,
        )
    }

    /// The synthetic upstream backing audio models (audio.cpp).
    pub fn audio_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            AUDIO_UPSTREAM_ID,
            AUDIO_UPSTREAM_NAME,
            UpstreamKind::AudioCpp,
            self.settings.audio.request_timeout_seconds,
        )
    }

    /// The synthetic upstream backing image models (stable-diffusion.cpp).
    pub fn image_upstream(&self) -> Upstream {
        Self::synthetic_upstream(
            IMAGE_UPSTREAM_ID,
            IMAGE_UPSTREAM_NAME,
            UpstreamKind::SdCpp,
            self.settings.image.request_timeout_seconds,
        )
    }

    /// One of the four per-class synthetic upstreams (§5). Never persisted,
    /// never `expose_all` (the classes enumerate their own tables — see
    /// [`Snapshot::exposed_models`]), always `protocol = openai`.
    ///
    /// `timeout_seconds` is the caller's class setting, not a constant: the
    /// four classes used to share one 600 s ceiling, which was right for none
    /// of them but chat. A 0 travels through as a 0 and
    /// [`Upstream::request_timeout`] reads it as "no deadline".
    fn synthetic_upstream(
        id: i64,
        name: &str,
        kind: UpstreamKind,
        timeout_seconds: u64,
    ) -> Upstream {
        Upstream {
            id,
            name: name.into(),
            protocol: Protocol::Openai,
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

    /// Public client-facing name of an aux model
    /// (`embed/<id>` under the configured aux prefix).
    pub fn aux_public_name(&self, model_id: &str) -> String {
        prefixed(&self.settings.aux_router.public_prefix, model_id)
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

    fn resolve_aux_local(&self, name: &str) -> Option<Route> {
        let id = strip_prefix(&self.settings.aux_router.public_prefix, name)?;
        let m = self
            .aux_models
            .iter()
            .find(|m| m.enabled && m.model_id == id)?;
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
                    .map(|m| (self.aux_public_name(&m.model_id), "aux")),
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

    /// The GPU-hold fallback lookup for a held model (gpu-hold design §2): the
    /// row's own `hold_fallback_mode` first (`none` refuses outright, `alias`
    /// is the row's own `hold_fallback`), and only an `inherit`ing **chat**
    /// row falls through to `settings.hold.fallback_alias` — aux, audio and
    /// image never inherit the global, because a different embedding model
    /// silently corrupts a vector index (owner's decision, 2026-09-04) and the
    /// global fallback is a chat alias, which cannot draw a picture.
    ///
    /// `None` means "refuse this held request": no fallback configured, an
    /// explicit `none`, an `alias` row with nothing set, or a model id this
    /// snapshot does not know about at all (a row that vanished between
    /// resolve and this lookup should refuse, not silently inherit).
    pub fn hold_fallback_for(
        &self,
        class: crate::runtime::Class,
        model_id: &str,
    ) -> Option<String> {
        use crate::runtime::Class;

        let (mode, own_fallback) = match class {
            Class::Chat => self
                .local_models
                .iter()
                .find(|m| m.model_id == model_id)
                .map(|m| (m.hold_fallback_mode, m.hold_fallback.clone()))?,
            Class::Aux => self
                .aux_models
                .iter()
                .find(|m| m.model_id == model_id)
                .map(|m| (m.hold_fallback_mode, m.hold_fallback.clone()))?,
            Class::Audio => self
                .audio_models
                .iter()
                .find(|m| m.model_id == model_id)
                .map(|m| (m.hold_fallback_mode, m.hold_fallback.clone()))?,
            Class::Image => self
                .image_models
                .iter()
                .find(|m| m.model_id == model_id)
                .map(|m| (m.hold_fallback_mode, m.hold_fallback.clone()))?,
        };
        match mode {
            HoldFallbackMode::None => None,
            HoldFallbackMode::Alias => own_fallback.filter(|a| !a.trim().is_empty()),
            HoldFallbackMode::Inherit if class == Class::Chat => self
                .settings
                .hold
                .fallback_alias
                .clone()
                .filter(|a| !a.trim().is_empty()),
            HoldFallbackMode::Inherit => None,
        }
    }
}
