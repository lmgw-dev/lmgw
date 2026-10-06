//! Router tests (§15): alias resolution + param-override merge precedence +
//! auto-exposure (public locals, passthrough upstreams).

use lmgw_core::config::{
    AudioModel, AuxKind, AuxModel, LlamaParams, LocalModel, ModelAlias, Protocol, Snapshot,
    Upstream, UpstreamKind, AUDIO_UPSTREAM_ID, AUX_UPSTREAM_ID, ROUTER_UPSTREAM_ID,
};
use lmgw_core::error::GatewayError;
use lmgw_core::ir::Params;

fn upstream(id: i64, enabled: bool) -> Upstream {
    Upstream {
        id,
        name: format!("up{id}"),
        protocol: Protocol::Openai,
        kind: UpstreamKind::Generic,
        base_url: "http://localhost:1234/v1".into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 1000,
        enabled,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn alias(name: &str, upstream_id: i64, enabled: bool, overrides: Params) -> ModelAlias {
    ModelAlias {
        id: 1,
        alias: name.into(),
        upstream_id,
        upstream_model_id: "real-model".into(),
        param_overrides: overrides,
        enabled,
        capabilities_override: None,
    }
}

fn snapshot(ups: Vec<Upstream>, aliases: Vec<ModelAlias>) -> Snapshot {
    Snapshot {
        upstreams: ups.into_iter().map(|u| (u.id, u)).collect(),
        aliases: aliases.into_iter().map(|a| (a.alias.clone(), a)).collect(),
        candidate_aliases: Default::default(),
        local_models: vec![],
        aux_models: vec![],
        audio_models: vec![],
        image_models: vec![],
        prices: vec![],
        api_keys: vec![],
        settings: Default::default(),
        hidden_passthrough: Default::default(),
        mcp_servers: Default::default(),
        mcp_tool_overrides: Default::default(),
        disabled_tools: Default::default(),
        gpu_lease: None,
    }
}

#[test]
fn resolves_alias_to_route() {
    let snap = snapshot(
        vec![upstream(1, true)],
        vec![alias("my-model", 1, true, Params::default())],
    );
    let route = snap.resolve("my-model").unwrap();
    assert_eq!(route.upstream.id, 1);
    assert_eq!(route.upstream_model, "real-model");
}

#[test]
fn unknown_alias_is_404() {
    let snap = snapshot(vec![upstream(1, true)], vec![]);
    assert!(matches!(
        snap.resolve("nope"),
        Err(GatewayError::UnknownAlias(_))
    ));
}

#[test]
fn disabled_alias_is_unknown() {
    let snap = snapshot(
        vec![upstream(1, true)],
        vec![alias("m", 1, false, Params::default())],
    );
    assert!(matches!(
        snap.resolve("m"),
        Err(GatewayError::UnknownAlias(_))
    ));
}

#[test]
fn disabled_upstream_is_internal_error() {
    let snap = snapshot(
        vec![upstream(1, false)],
        vec![alias("m", 1, true, Params::default())],
    );
    assert!(matches!(snap.resolve("m"), Err(GatewayError::Internal(_))));
}

#[test]
fn client_params_win_over_alias_defaults() {
    let defaults = Params {
        temperature: Some(0.2),
        top_p: Some(0.9),
        max_tokens: Some(1024),
        stop: vec!["<END>".into()],
        ..Default::default()
    };
    let client = Params {
        temperature: Some(1.0),
        ..Default::default()
    };
    let merged = client.with_defaults(&defaults);
    assert_eq!(merged.temperature, Some(1.0)); // client wins
    assert_eq!(merged.top_p, Some(0.9)); // filled from alias
    assert_eq!(merged.max_tokens, Some(1024));
    assert_eq!(merged.stop, vec!["<END>".to_string()]);
}

#[test]
fn empty_defaults_keep_client_params() {
    let client = Params {
        seed: Some(7),
        ..Default::default()
    };
    let merged = client.clone().with_defaults(&Params::default());
    assert_eq!(merged, client);
}

#[test]
fn api_key_verification() {
    let mut snap = snapshot(vec![], vec![]);
    let key = "lmgw-secret";
    snap.api_keys.push(lmgw_core::config::ApiKey {
        id: 1,
        name: "test".into(),
        key_hash: lmgw_core::config::hash_api_key(key),
        enabled: true,
        ..Default::default()
    });
    assert_eq!(snap.verify_api_key(key).unwrap().name, "test");
    assert!(snap.verify_api_key("wrong").is_none());
}

fn local(model_id: &str, enabled: bool, public: bool) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: LlamaParams::default(),
        args: vec![],
        idle_seconds: 0,
        enabled,
        public,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

#[test]
fn public_local_resolves_without_alias() {
    let mut snap = snapshot(vec![], vec![]);
    snap.local_models.push(local("gemma4-12b", true, true));
    let route = snap.resolve("gemma4-12b").unwrap();
    assert_eq!(route.upstream.id, ROUTER_UPSTREAM_ID);
    assert_eq!(route.upstream.kind, UpstreamKind::LlamaServer);
    assert_eq!(route.upstream_model, "gemma4-12b");
    // There is no class-wide router port to target any more: a local route is
    // unforwardable until `vram::admit` hands back a hold and the caller
    // overwrites this with the acquired container's endpoint (§5). Port 0
    // makes an unheld forward fail immediately instead of hitting whatever is
    // listening on 9292.
    assert_eq!(route.upstream.base_url, "http://127.0.0.1:0/v1");
    assert_eq!(route.upstream.base_url, snap.router_upstream().base_url);
    assert!(!route.upstream.expose_all);
}

fn aux(model_id: &str, kind: AuxKind, enabled: bool) -> AuxModel {
    AuxModel {
        id: 1,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        kind,
        pooling: None,
        ctx_size: None,
        args: vec![],
        idle_seconds: 0,
        enabled,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

fn audio(model_id: &str, enabled: bool) -> AudioModel {
    AudioModel {
        id: 1,
        model_id: model_id.into(),
        family: "pocket_tts".into(),
        path: format!("audio/{model_id}"),
        task: "tts".into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        backend: None,
        threads: None,
        load_options: Default::default(),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        residency: None,
    }
}

/// Aux and audio models used to reach the second tier only through a managed
/// `expose_all` upstream row that had to be provisioned first. Per-model
/// containers §5 deleted those rows: both classes now resolve exactly like
/// public chat locals do — from their own table, under their own class
/// prefix, onto their class's synthetic upstream.
#[test]
fn aux_and_audio_models_resolve_from_their_own_tables() {
    let mut snap = snapshot(vec![], vec![]);
    snap.aux_models.push(aux("bge-m3", AuxKind::Embed, true));
    snap.audio_models.push(audio("pocket-tts", true));

    let route = snap.resolve("embed/bge-m3").unwrap();
    assert_eq!(route.upstream.id, AUX_UPSTREAM_ID);
    assert_eq!(route.upstream.name, "llama-aux");
    assert_eq!(route.upstream.kind, UpstreamKind::LlamaServer);
    assert_eq!(route.upstream_model, "bge-m3");
    // …and the kind gate `/v1/embeddings` and `/v1/rerank` share reads the row
    // behind that route, which is what keeps the zero-vector guard working.
    assert_eq!(
        snap.aux_model_for(&route).map(|m| m.kind),
        Some(AuxKind::Embed)
    );

    let route = snap.resolve("audio/pocket-tts").unwrap();
    assert_eq!(route.upstream.id, AUDIO_UPSTREAM_ID);
    assert_eq!(route.upstream.name, "audiocpp");
    assert_eq!(route.upstream.kind, UpstreamKind::AudioCpp);
    assert_eq!(route.upstream_model, "pocket-tts");
    assert!(snap.aux_model_for(&route).is_none());

    // Outside the class prefix nothing routes, and a disabled row is unknown.
    assert!(snap.resolve("bge-m3").is_err());
    assert!(snap.resolve("pocket-tts").is_err());
}

#[test]
fn disabled_aux_and_audio_models_are_unknown() {
    let mut snap = snapshot(vec![], vec![]);
    snap.aux_models.push(aux("bge-m3", AuxKind::Embed, false));
    snap.audio_models.push(audio("pocket-tts", false));
    assert!(matches!(
        snap.resolve("embed/bge-m3"),
        Err(GatewayError::UnknownAlias(_))
    ));
    assert!(matches!(
        snap.resolve("audio/pocket-tts"),
        Err(GatewayError::UnknownAlias(_))
    ));
}

/// The precedence change §5 states plainly: aux/audio moved out of the
/// passthrough tier into the local one, so a cloud `expose_all` upstream
/// sharing a class prefix now loses to a local model of the same name.
/// Acceptable in a deployment that names its prefixes deliberately — and an
/// explicit alias still beats both.
#[test]
fn a_local_model_beats_a_passthrough_catalog_of_the_same_name() {
    let mut up = upstream(1, true);
    up.expose_all = true;
    up.expose_prefix = "embed".into();
    let mut snap = snapshot(vec![up], vec![]);
    snap.aux_models.push(aux("bge-m3", AuxKind::Embed, true));

    assert_eq!(
        snap.resolve("embed/bge-m3").unwrap().upstream.id,
        AUX_UPSTREAM_ID
    );
    // Anything the local table does not have still passes through.
    assert_eq!(
        snap.resolve("embed/text-embedding-3").unwrap().upstream.id,
        1
    );

    // An alias is still the first tier.
    let a = alias("embed/bge-m3", 1, true, Params::default());
    snap.aliases.insert(a.alias.clone(), a);
    assert_eq!(snap.resolve("embed/bge-m3").unwrap().upstream.id, 1);
}

#[test]
fn public_local_honors_prefix() {
    let mut snap = snapshot(vec![], vec![]);
    snap.settings.router.public_prefix = "local".into();
    snap.local_models.push(local("gemma4-12b", true, true));
    assert!(snap.resolve("gemma4-12b").is_err()); // bare name no longer routes
    let route = snap.resolve("local/gemma4-12b").unwrap();
    assert_eq!(route.upstream_model, "gemma4-12b");
}

#[test]
fn private_or_disabled_local_is_unknown() {
    let mut snap = snapshot(vec![], vec![]);
    snap.local_models.push(local("private-model", true, false));
    snap.local_models.push(local("disabled-model", false, true));
    assert!(matches!(
        snap.resolve("private-model"),
        Err(GatewayError::UnknownAlias(_))
    ));
    assert!(matches!(
        snap.resolve("disabled-model"),
        Err(GatewayError::UnknownAlias(_))
    ));
}

#[test]
fn explicit_alias_wins_over_public_local() {
    let mut snap = snapshot(
        vec![upstream(1, true)],
        vec![alias("gemma4-12b", 1, true, Params::default())],
    );
    snap.local_models.push(local("gemma4-12b", true, true));
    let route = snap.resolve("gemma4-12b").unwrap();
    assert_eq!(route.upstream.id, 1); // the alias's upstream, not the router
}

#[test]
fn prefixed_passthrough_strips_prefix() {
    let mut up = upstream(1, true);
    up.expose_all = true;
    up.expose_prefix = "groq".into();
    let snap = snapshot(vec![up], vec![]);
    let route = snap.resolve("groq/llama-3.3-70b").unwrap();
    assert_eq!(route.upstream.id, 1);
    assert_eq!(route.upstream_model, "llama-3.3-70b");
    // Outside the prefix nothing routes.
    assert!(snap.resolve("llama-3.3-70b").is_err());
}

#[test]
fn bare_passthrough_forwards_any_model() {
    let mut up = upstream(1, true);
    up.expose_all = true;
    let snap = snapshot(vec![up], vec![]);
    let route = snap.resolve("whatever-model").unwrap();
    assert_eq!(route.upstream_model, "whatever-model");
}

#[test]
fn passthrough_prefers_prefixed_over_bare() {
    let mut bare = upstream(1, true);
    bare.expose_all = true;
    let mut prefixed = upstream(2, true);
    prefixed.expose_all = true;
    prefixed.expose_prefix = "groq".into();
    let snap = snapshot(vec![bare, prefixed], vec![]);
    assert_eq!(snap.resolve("groq/m").unwrap().upstream.id, 2);
    assert_eq!(snap.resolve("m").unwrap().upstream.id, 1);
}

/// Exposure is table-driven for all three classes (§5): there is no always-on
/// class router left to HTTP-fetch a catalog from, so `GET /v1/models` reads
/// the rows. Chat still requires `public`; aux and audio have no such column
/// and key on `enabled` alone.
#[test]
fn exposed_models_lists_aliases_and_every_enabled_local_model() {
    let mut snap = snapshot(
        vec![upstream(1, true)],
        vec![alias("my-claude", 1, true, Params::default())],
    );
    snap.settings.router.public_prefix = "local".into();
    snap.local_models.push(local("gemma4-12b", true, true));
    snap.local_models.push(local("hidden", true, false));
    snap.aux_models.push(aux("bge-m3", AuxKind::Embed, true));
    snap.aux_models
        .push(aux("bge-reranker", AuxKind::Rerank, false));
    snap.audio_models.push(audio("pocket-tts", true));
    snap.audio_models.push(audio("off", false));
    let names: Vec<String> = snap.exposed_models().into_iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        vec![
            "audio/pocket-tts",
            "embed/bge-m3",
            "local/gemma4-12b",
            "my-claude",
        ]
    );

    // Every name is listed once, and each carries the class it came from.
    let sources: Vec<&str> = snap.exposed_models().iter().map(|e| e.source).collect();
    assert_eq!(sources, vec!["audio", "aux", "local", "alias"]);
}

/// An alias of the same name wins, and the model is not also listed under its
/// public name twice.
#[test]
fn exposed_models_dedupes_an_alias_over_a_local_of_the_same_name() {
    let mut snap = snapshot(
        vec![upstream(1, true)],
        vec![alias("embed/bge-m3", 1, true, Params::default())],
    );
    snap.aux_models.push(aux("bge-m3", AuxKind::Embed, true));
    let listed: Vec<(String, &str)> = snap
        .exposed_models()
        .into_iter()
        .map(|e| (e.name, e.source))
        .collect();
    assert_eq!(listed, vec![("embed/bge-m3".to_string(), "alias")]);
}

#[test]
fn snapshot_lists_only_enabled_aliases() {
    let mut snap = snapshot(
        vec![upstream(1, true)],
        vec![alias("b-on", 1, true, Params::default())],
    );
    let mut off = alias("a-off", 1, false, Params::default());
    off.id = 2;
    snap.aliases.insert(off.alias.clone(), off);
    let names: Vec<&str> = snap
        .enabled_aliases()
        .iter()
        .map(|a| a.alias.as_str())
        .collect();
    assert_eq!(names, vec!["b-on"]);
}

/// Each local class carries its **own** per-request ceiling onto its synthetic
/// upstream. They shared one 600 s constant until now, which was right for the
/// chat class and wrong for the other three in both directions: an embedding
/// call that hangs ten minutes should have failed in one, and a twenty-minute
/// music generation should never have been cut off at ten.
#[test]
fn each_class_carries_its_own_request_ceiling() {
    let mut snap = snapshot(vec![], vec![]);

    // The shipped defaults, per class.
    assert_eq!(snap.router_upstream().timeout_ms, 600_000, "chat");
    assert_eq!(snap.aux_upstream().timeout_ms, 60_000, "aux");
    assert_eq!(snap.audio_upstream().timeout_ms, 1_800_000, "audio");
    assert_eq!(snap.image_upstream().timeout_ms, 1_800_000, "image");

    // Chat and aux are two instances of one settings type, so the thing worth
    // proving is that they are read separately and do not shadow each other.
    snap.settings.router.request_timeout_seconds = 11;
    snap.settings.aux_router.request_timeout_seconds = 22;
    snap.settings.audio.request_timeout_seconds = 33;
    snap.settings.image.request_timeout_seconds = 44;
    assert_eq!(snap.router_upstream().timeout_ms, 11_000);
    assert_eq!(snap.aux_upstream().timeout_ms, 22_000);
    assert_eq!(snap.audio_upstream().timeout_ms, 33_000);
    assert_eq!(snap.image_upstream().timeout_ms, 44_000);
}

/// `0` is the maximum possible — no deadline of lmgw's own — and **not** the
/// one-millisecond deadline `timeout_ms.max(1)` used to turn it into. This is
/// the whole reason the field is read through one accessor.
#[test]
fn a_zero_ceiling_means_no_deadline_not_an_instant_one() {
    let mut snap = snapshot(vec![upstream(1, true)], vec![]);
    for s in [&mut snap.settings.router, &mut snap.settings.aux_router] {
        s.request_timeout_seconds = 0;
    }
    snap.settings.audio.request_timeout_seconds = 0;
    snap.settings.image.request_timeout_seconds = 0;

    for up in [
        snap.router_upstream(),
        snap.aux_upstream(),
        snap.audio_upstream(),
        snap.image_upstream(),
    ] {
        assert_eq!(up.timeout_ms, 0, "{}", up.name);
        assert_eq!(up.request_timeout(), None, "{}", up.name);
    }

    // Same rule for a stored upstream whose owner cleared the field — it is
    // one field with one meaning, not a local-only convention.
    let mut stored = upstream(1, true);
    stored.timeout_ms = 0;
    assert_eq!(stored.request_timeout(), None);
    stored.timeout_ms = 1;
    assert_eq!(
        stored.request_timeout(),
        Some(std::time::Duration::from_millis(1))
    );
}

/// A seconds value large enough to overflow the millisecond multiplication
/// saturates instead of wrapping to something short. An owner who types a
/// decade gets a decade; what they must never get is a deadline *shorter* than
/// the one they asked for.
#[test]
fn an_absurd_ceiling_saturates_rather_than_wrapping() {
    let mut snap = snapshot(vec![], vec![]);
    snap.settings.router.request_timeout_seconds = u64::MAX;
    assert_eq!(snap.router_upstream().timeout_ms, u64::MAX);
    assert!(snap.router_upstream().request_timeout().is_some());
}
