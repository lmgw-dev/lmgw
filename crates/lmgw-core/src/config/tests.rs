use super::*;

/// `ApiKey` derives `Debug` and ends up inside a `Snapshot` that all sorts
/// of things `{:?}`-print, so the one field holding a live bearer must not
/// print itself (final review).
#[test]
fn an_agent_tokens_debug_output_never_shows_the_token() {
    let key = super::ApiKey {
        name: "agent:labeler".into(),
        kind: super::ApiKeyKind::Agent,
        key_plain: Some(super::Secret::new("lmgw-agent-deadbeef")),
        agent_id: Some("labeler".into()),
        ..Default::default()
    };
    let printed = format!("{key:?}");
    assert!(!printed.contains("deadbeef"), "{printed}");
    assert!(printed.contains("<redacted>"), "{printed}");
    // Serde already refused to write it; both doors, not one.
    assert!(!serde_json::to_string(&key).unwrap().contains("deadbeef"));
    // And the value is still exactly the value when it is asked for.
    assert_eq!(key.key_plain.unwrap().expose(), "lmgw-agent-deadbeef");
}

fn stdio(image: Option<&str>) -> McpServer {
    McpServer {
        id: 1,
        name: "github".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("mcp-server-github".into()),
        args: vec!["--verbose".into()],
        env: vec![("GITHUB_TOKEN".into(), "tok".into())],
        cwd: None,
        container_image: image.map(String::from),
        extra_run_args: vec!["-v".into(), "./data:/data:Z".into()],
        url: None,
        headers: vec![],
        tool_prefix: "gh".into(),
        timeout_ms: 60_000,
        autostart: true,
        idle_seconds: 0,
        allow_sampling: true,
        sampling_alias: None,
        agent_id: None,
    }
}

#[test]
fn stdio_argv_isolated_synthesizes_podman_run() {
    let (prog, argv) = stdio(Some("ghcr.io/acme/mcp-github")).stdio_argv();
    assert_eq!(prog, "podman");
    assert_eq!(
        argv,
        vec![
            "run",
            "--rm",
            "-i",
            "--quiet",
            "-v",
            "./data:/data:Z",
            "-e",
            "GITHUB_TOKEN=tok",
            "ghcr.io/acme/mcp-github",
            "mcp-server-github",
            "--verbose",
        ]
    );
}

#[test]
fn stdio_argv_bare_runs_command_verbatim() {
    // No container image ⇒ bare subprocess; env applied by the spawner.
    let (prog, argv) = stdio(None).stdio_argv();
    assert_eq!(prog, "mcp-server-github");
    assert_eq!(argv, vec!["--verbose"]);
}

/// The connect creates an isolated server's container before its handshake
/// (MCP gateway design §9, 2026-10-08): `podman create` with `run`'s every
/// flag, image and command, and the labels it is given. A bare server has
/// none.
#[test]
fn container_create_argv_is_the_run_argv_up_to_the_start() {
    let s = stdio(Some("ghcr.io/acme/mcp-github"));
    let (_, mut run) = s.stdio_argv();
    run[0] = "create".to_string();
    run.splice(4..4, ["--label".to_string(), "lmgw.mcp=1".to_string()]);
    let labels = [("lmgw.mcp".to_string(), "1".to_string())];
    assert_eq!(s.container_create_argv(&labels), Some(run));
    assert_eq!(stdio(None).container_create_argv(&labels), None);
    assert_eq!(stdio(Some("  ")).container_create_argv(&labels), None);
}

/// `podman create` refuses `--sig-proxy` and `--detach-keys`, which `podman
/// run` took, and `podman start` takes them (the review's R-1): they go to
/// the start, a `--detach-keys` value of its own token with it, and every
/// other flag stays with the create.
#[test]
fn the_run_flags_only_podman_start_takes_go_to_the_start() {
    let mut s = stdio(Some("ghcr.io/acme/mcp-github"));
    s.extra_run_args = [
        "--sig-proxy=false",
        "-v",
        "./data:/data:Z",
        "--detach-keys",
        "ctrl-x",
        "--detach-keys=ctrl-y",
        "--sig-proxy",
    ]
    .map(String::from)
    .to_vec();
    let create = s.container_create_argv(&[]).unwrap();
    assert_eq!(
        create,
        [
            "create",
            "--rm",
            "-i",
            "--quiet",
            "-v",
            "./data:/data:Z",
            "-e",
            "GITHUB_TOKEN=tok",
            "ghcr.io/acme/mcp-github",
            "mcp-server-github",
            "--verbose",
        ]
    );
    assert_eq!(
        s.container_start_argv("c0ffee"),
        [
            "start",
            "--attach",
            "--interactive",
            "--sig-proxy=false",
            "--detach-keys",
            "ctrl-x",
            "--detach-keys=ctrl-y",
            "--sig-proxy",
            "c0ffee",
        ]
    );
    assert_eq!(
        stdio(Some("img")).container_start_argv("c0ffee"),
        ["start", "--attach", "--interactive", "c0ffee"]
    );
}

/// The `podman run` flags neither `podman create` nor `podman start` takes
/// are refused by name, with why (R-1): `-d`/`--detach` (a cluster such as
/// `-dit` too), `--rmi`, `--preserve-fd`, `--preserve-fds` and `--passwd`.
/// The flags `podman start` takes, a value that merely spells one, and a
/// shorthand cluster whose `d` is a value are not.
#[test]
fn the_run_flags_no_connect_can_pass_are_refused_by_name() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for (extra, flag) in [
        (args(&["-d"]), "`-d`"),
        (args(&["-dit"]), "`-d`"),
        (args(&["--detach"]), "`--detach`"),
        (args(&["--detach=true"]), "`--detach`"),
        (args(&["--rmi"]), "`--rmi`"),
        (args(&["--preserve-fd", "3"]), "`--preserve-fd`"),
        (args(&["--preserve-fds=2"]), "`--preserve-fds`"),
        (args(&["--passwd=false"]), "`--passwd`"),
    ] {
        let why = run_only_refusal(&extra).unwrap_or_else(|| panic!("{extra:?} passed"));
        assert!(why.contains(flag), "{extra:?}: {why}");
        assert!(why.contains("only `podman run` takes"), "{why}");
        assert!(why.contains("`podman create`"), "{why}");
        assert!(why.contains("remove it from extra_run_args"), "{why}");
    }
    let both = run_only_refusal(&args(&["--rmi", "-v", "/a:/b", "-d", "--rmi"])).unwrap();
    assert!(both.contains("`--rmi`") && both.contains("`-d`"), "{both}");
    assert_eq!(both.matches("`--rmi`").count(), 1, "{both}");
    assert!(both.contains("remove them"), "{both}");
    for fine in [
        args(&["--sig-proxy=false", "--detach-keys", "ctrl-x"]),
        args(&["-it", "-v/data:/data:Z", "-edetach=1"]),
        args(&["--label=--rmi", "--device", "nvidia.com/gpu=all"]),
        args(&[]),
    ] {
        assert_eq!(run_only_refusal(&fine), None, "{fine:?}");
    }
}

#[test]
fn exposed_name_applies_prefix() {
    let s = stdio(None);
    assert_eq!(s.exposed_name("search"), "gh__search");
    let mut bare = stdio(None);
    bare.tool_prefix = String::new();
    assert_eq!(bare.exposed_name("search"), "search");
}

// -- Snapshot::hold_fallback_for (gpu-hold design §2) --------------------

fn local_row(mode: HoldFallbackMode, fallback: Option<&str>) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: "chat-a".into(),
        gguf_path: "chat-a.gguf".into(),
        params: LlamaParams::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: mode,
        hold_fallback: fallback.map(String::from),
        capabilities_override: None,
        ladder: vec![],
    }
}

fn aux_row(mode: HoldFallbackMode, fallback: Option<&str>) -> AuxModel {
    AuxModel {
        id: 1,
        model_id: "embed-a".into(),
        gguf_path: "embed-a.gguf".into(),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: mode,
        hold_fallback: fallback.map(String::from),
    }
}

fn audio_row(mode: HoldFallbackMode, fallback: Option<&str>) -> AudioModel {
    AudioModel {
        id: 1,
        model_id: "tts-a".into(),
        family: "qwen3_tts".into(),
        path: "tts-a".into(),
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
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: mode,
        hold_fallback: fallback.map(String::from),
        residency: None,
    }
}

/// A snapshot with one row of each class and a settable global fallback,
/// for the `hold_fallback_for` matrix below.
fn snap_with(
    local: LocalModel,
    aux: AuxModel,
    audio: AudioModel,
    global: Option<&str>,
) -> Snapshot {
    Snapshot {
        local_models: vec![local],
        aux_models: vec![aux],
        audio_models: vec![audio],
        settings: Settings {
            hold: HoldSettings {
                active: true,
                fallback_alias: global.map(String::from),
            },
            ..Settings::default()
        },
        ..Snapshot::default()
    }
}

#[test]
fn hold_fallback_none_mode_refuses_even_with_a_global_set() {
    let snap = snap_with(
        local_row(HoldFallbackMode::None, None),
        aux_row(HoldFallbackMode::None, None),
        audio_row(HoldFallbackMode::None, None),
        Some("cloud-global"),
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Chat, "chat-a"),
        None
    );
}

#[test]
fn hold_fallback_alias_mode_uses_the_rows_own_alias() {
    let snap = snap_with(
        local_row(HoldFallbackMode::Alias, Some("cloud-row")),
        aux_row(HoldFallbackMode::Alias, Some("cloud-row-aux")),
        audio_row(HoldFallbackMode::Alias, Some("cloud-row-audio")),
        None,
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Chat, "chat-a"),
        Some("cloud-row".to_string())
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Aux, "embed-a"),
        Some("cloud-row-aux".to_string())
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Audio, "tts-a"),
        Some("cloud-row-audio".to_string())
    );
}

#[test]
fn hold_fallback_alias_mode_with_no_alias_set_counts_as_no_fallback() {
    let snap = snap_with(
        local_row(HoldFallbackMode::Alias, None),
        aux_row(HoldFallbackMode::Alias, Some("")),
        audio_row(HoldFallbackMode::Alias, None),
        Some("cloud-global"),
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Chat, "chat-a"),
        None
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Aux, "embed-a"),
        None
    );
}

#[test]
fn hold_fallback_inherit_chat_falls_through_to_the_global() {
    let snap = snap_with(
        local_row(HoldFallbackMode::Inherit, None),
        aux_row(HoldFallbackMode::Inherit, None),
        audio_row(HoldFallbackMode::Inherit, None),
        Some("cloud-global"),
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Chat, "chat-a"),
        Some("cloud-global".to_string())
    );
}

#[test]
fn hold_fallback_inherit_chat_with_no_global_refuses() {
    let snap = snap_with(
        local_row(HoldFallbackMode::Inherit, None),
        aux_row(HoldFallbackMode::Inherit, None),
        audio_row(HoldFallbackMode::Inherit, None),
        None,
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Chat, "chat-a"),
        None
    );
}

#[test]
fn hold_fallback_inherit_never_reaches_aux_or_audio() {
    let snap = snap_with(
        local_row(HoldFallbackMode::Inherit, None),
        aux_row(HoldFallbackMode::Inherit, None),
        audio_row(HoldFallbackMode::Inherit, None),
        Some("cloud-global"),
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Aux, "embed-a"),
        None,
        "an aux model must never silently inherit the chat fallback"
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Audio, "tts-a"),
        None,
        "an audio model must never silently inherit the chat fallback"
    );
}

#[test]
fn hold_fallback_unknown_model_id_refuses() {
    let snap = snap_with(
        local_row(HoldFallbackMode::Alias, Some("cloud-row")),
        aux_row(HoldFallbackMode::Alias, Some("cloud-row-aux")),
        audio_row(HoldFallbackMode::Alias, Some("cloud-row-audio")),
        Some("cloud-global"),
    );
    assert_eq!(
        snap.hold_fallback_for(crate::runtime::Class::Chat, "no-such-model"),
        None
    );
}

// -----------------------------------------------------------------------
// Unified-KV helpers (candidate-aliases/unified-KV design §3.1–§3.3)
// -----------------------------------------------------------------------

#[test]
fn effective_kv_unified_follows_the_tri_state_then_parallel() {
    // Auto default: unified exactly when `parallel` is unset (fact 1).
    assert!(LlamaParams::default().effective_kv_unified());
    assert!(!LlamaParams {
        parallel: Some(4),
        ..Default::default()
    }
    .effective_kv_unified());
    // Explicit `true` always wins, even split-shaped `parallel`.
    assert!(LlamaParams {
        parallel: Some(4),
        kv_unified: Some(true),
        ..Default::default()
    }
    .effective_kv_unified());
    // Explicit `false` only has a say once `parallel` names a real slot
    // count — with one set, split actually takes effect.
    assert!(!LlamaParams {
        parallel: Some(4),
        kv_unified: Some(false),
        ..Default::default()
    }
    .effective_kv_unified());
    // Review finding 3: llama-server forces unified whenever `parallel`
    // is auto (unset, or non-positive), *overriding* an explicit
    // `--no-kv-unified` — so `kv_unified: false` cannot win here, however
    // surprising that reads next to a config that says otherwise.
    assert!(LlamaParams {
        parallel: None,
        kv_unified: Some(false),
        ..Default::default()
    }
    .effective_kv_unified());
    assert!(LlamaParams {
        parallel: Some(0),
        kv_unified: Some(false),
        ..Default::default()
    }
    .effective_kv_unified());
}

#[test]
fn effective_slots_is_parallel_or_llama_servers_auto_of_four() {
    assert_eq!(LlamaParams::default().effective_slots(), 4);
    assert_eq!(
        LlamaParams {
            parallel: Some(1),
            ..Default::default()
        }
        .effective_slots(),
        1
    );
    assert_eq!(
        LlamaParams {
            parallel: Some(8),
            ..Default::default()
        }
        .effective_slots(),
        8
    );
    // A non-positive `parallel` cannot mean "fewer than one slot" — falls
    // back to the auto count rather than producing a division-by-zero
    // elsewhere.
    assert_eq!(
        LlamaParams {
            parallel: Some(0),
            ..Default::default()
        }
        .effective_slots(),
        4
    );
    assert_eq!(
        LlamaParams {
            parallel: Some(-1),
            ..Default::default()
        }
        .effective_slots(),
        4
    );
}

#[test]
fn pool_guarded_needs_an_explicit_on_more_than_one_slot_and_max_output() {
    // The auto default reaching a shared pool is left alone (spec §1):
    // never guarded, whatever `n_predict` says.
    assert!(!LlamaParams {
        n_predict: Some(4096),
        ..Default::default()
    }
    .pool_guarded());
    // Explicit unified, one slot: no pool to guard.
    assert!(!LlamaParams {
        kv_unified: Some(true),
        parallel: Some(1),
        n_predict: Some(4096),
        ..Default::default()
    }
    .pool_guarded());
    // Explicit unified, several slots, no bound: refused elsewhere
    // (`ops::validate_kv_unified`), and not guarded here either.
    assert!(!LlamaParams {
        kv_unified: Some(true),
        parallel: Some(4),
        ..Default::default()
    }
    .pool_guarded());
    assert!(!LlamaParams {
        kv_unified: Some(true),
        parallel: Some(4),
        n_predict: Some(0),
        ..Default::default()
    }
    .pool_guarded());
    // The one case that is guarded: explicit, shared, bounded.
    assert!(LlamaParams {
        kv_unified: Some(true),
        parallel: Some(4),
        n_predict: Some(4096),
        ..Default::default()
    }
    .pool_guarded());
    // Explicit split is never guarded — there is no shared pool.
    assert!(!LlamaParams {
        kv_unified: Some(false),
        parallel: Some(4),
        n_predict: Some(4096),
        ..Default::default()
    }
    .pool_guarded());
}

#[test]
fn pool_unguarded_shared_is_the_editors_note_condition() {
    // Auto default: shared, more than one slot, never guarded — fires.
    assert!(LlamaParams::default().pool_unguarded_shared());
    // Explicit unified with no bound: also fires.
    assert!(LlamaParams {
        kv_unified: Some(true),
        parallel: Some(4),
        ..Default::default()
    }
    .pool_unguarded_shared());
    // Guarded (bound present): does not fire.
    assert!(!LlamaParams {
        kv_unified: Some(true),
        parallel: Some(4),
        n_predict: Some(4096),
        ..Default::default()
    }
    .pool_unguarded_shared());
    // Split: no shared pool, nothing to warn about. `parallel` has to be
    // a real slot count for this — review finding 3/`effective_kv_unified`
    // forces unified when `parallel` is auto even with `kv_unified:
    // false`, so *that* combination is a shared pool after all (below).
    assert!(!LlamaParams {
        kv_unified: Some(false),
        parallel: Some(4),
        ..Default::default()
    }
    .pool_unguarded_shared());
    // `kv_unified: false` with `parallel` left on auto does not actually
    // split anything (fact 1) — this is the same "shared and unguarded"
    // case as the auto default, just spelled with an explicit `false`
    // that has no effect.
    assert!(LlamaParams {
        kv_unified: Some(false),
        ..Default::default()
    }
    .pool_unguarded_shared());
    // A single effective slot is never a "shared" pool worth a note.
    assert!(!LlamaParams {
        kv_unified: Some(true),
        parallel: Some(1),
        ..Default::default()
    }
    .pool_unguarded_shared());
}

#[test]
fn pool_tokens_prefers_ctx_size_then_the_per_slot_cap_never_trained() {
    // Nothing known.
    assert_eq!(LlamaParams::default().pool_tokens(), None);
    // `ctx_size` wins outright when set.
    assert_eq!(
        LlamaParams {
            ctx_size: Some(131_072),
            ..Default::default()
        }
        .pool_tokens(),
        Some(131_072)
    );
    // No `ctx_size`: the per-slot cap sizes the pool to `np * N`
    // (`server.cpp:164-172`).
    assert_eq!(
        LlamaParams {
            kv_unified_per_slot: Some(16_384),
            parallel: Some(4),
            ..Default::default()
        }
        .pool_tokens(),
        Some(65_536)
    );
    // Review finding 2: `--fit` can shrink an unset `ctx_size` to
    // whatever memory allows, so there is no trained-context fallback —
    // neither `ctx_size` nor the per-slot cap set means `None`, never a
    // guess from the GGUF header.
    assert_eq!(
        LlamaParams {
            ctx_size: None,
            kv_unified_per_slot: None,
            ..Default::default()
        }
        .pool_tokens(),
        None
    );
    // `ctx_size ≤ 0` reads as "from model" (llama-server's own sentinel
    // for unset), never a pool of zero or negative size — falls through
    // to the per-slot cap exactly like an absent `ctx_size` would.
    assert_eq!(
        LlamaParams {
            ctx_size: Some(0),
            kv_unified_per_slot: Some(4_096),
            ..Default::default()
        }
        .pool_tokens(),
        Some(4_096 * 4)
    );
    assert_eq!(
        LlamaParams {
            ctx_size: Some(-1),
            ..Default::default()
        }
        .pool_tokens(),
        None
    );
}

#[test]
fn per_request_ctx_unified_is_the_min_of_pool_cap_and_trained() {
    // Auto row (today's shape): `ctx_size` alone, no cap, no trained
    // context known — unchanged from before this toggle existed.
    assert_eq!(
        LlamaParams {
            ctx_size: Some(131_072),
            ..Default::default()
        }
        .per_request_ctx(None),
        Some(131_072)
    );
    // Same row, but the trained context is smaller: the new formula must
    // not publish more than the model actually supports.
    assert_eq!(
        LlamaParams {
            ctx_size: Some(8_192),
            ..Default::default()
        }
        .per_request_ctx(Some(4_096)),
        Some(4_096)
    );
    // The per-slot cap can bind tighter than the pool it also sizes.
    // `parallel` being set would otherwise mean split (fact 1), so this
    // needs `kv_unified` explicitly on to reach the unified branch at all.
    assert_eq!(
        LlamaParams {
            kv_unified: Some(true),
            kv_unified_per_slot: Some(4_096),
            parallel: Some(4),
            ..Default::default()
        }
        .per_request_ctx(Some(262_144)),
        Some(4_096)
    );
    // Review finding 2: a row that only reaches unified through the auto
    // default, with nothing configured at all, now gets `None` — the
    // pool itself is unknown (`pool_tokens` has no trained-context
    // fallback any more), so the trained context can no longer stand in
    // for it here either. This is the exact "auto row with `ctx_size`
    // unset publishes nothing" behaviour from before this toggle existed.
    assert_eq!(LlamaParams::default().per_request_ctx(Some(32_768)), None);
}

#[test]
fn per_request_ctx_split_is_unchanged_ctx_size_over_parallel() {
    // Today's exact behaviour, trained context deliberately ignored.
    assert_eq!(
        LlamaParams {
            kv_unified: Some(false),
            ctx_size: Some(131_072),
            parallel: Some(4),
            ..Default::default()
        }
        .per_request_ctx(Some(4_096)),
        Some(32_768)
    );
    // `ctx_size ≤ 0` is unset, same as split's existing "no guess" rule.
    assert_eq!(
        LlamaParams {
            kv_unified: Some(false),
            ctx_size: Some(0),
            parallel: Some(4),
            ..Default::default()
        }
        .per_request_ctx(None),
        None
    );
    // Today's handling of an unset `ctx_size`: `None`, not a guess.
    assert_eq!(
        LlamaParams {
            kv_unified: Some(false),
            parallel: Some(4),
            ..Default::default()
        }
        .per_request_ctx(Some(262_144)),
        None
    );
}

/// Every `Protocol` round-trips through `as_str`, `parse` and serde, and the
/// three agree: `parse` is a string match the compiler does not check, and
/// serde's spelling comes from `rename_all` unless a variant overrides it —
/// `llama_cpp` does (llama.cpp egress design, decision 10).
#[test]
fn every_protocol_round_trips_through_as_str_parse_and_serde() {
    // Exhaustive on purpose: a new variant fails to compile here until it is
    // listed below as well.
    let index = |p: Protocol| match p {
        Protocol::Openai => 0,
        Protocol::Anthropic => 1,
        Protocol::Gemini => 2,
        Protocol::LlamaCpp => 3,
    };
    let all = [
        Protocol::Openai,
        Protocol::Anthropic,
        Protocol::Gemini,
        Protocol::LlamaCpp,
    ];
    assert_eq!(all.map(index), [0, 1, 2, 3], "every variant is listed once");
    for p in all {
        assert_eq!(Protocol::parse(p.as_str()), Some(p), "{p:?}");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, format!("\"{}\"", p.as_str()), "{p:?}");
        assert_eq!(serde_json::from_str::<Protocol>(&json).unwrap(), p);
    }
    assert_eq!(Protocol::LlamaCpp.as_str(), "llama_cpp");
    assert_eq!(Protocol::parse("llamacpp"), None);
    assert_eq!(
        Protocol::parse("llama_server"),
        None,
        "a kind, not a protocol"
    );
}

/// The OpenAI-shaped HTTP surface: OpenAI's own and llama.cpp's.
#[test]
fn openai_http_is_spoken_by_openai_and_llama_cpp_only() {
    assert!(Protocol::Openai.speaks_openai_http());
    assert!(Protocol::LlamaCpp.speaks_openai_http());
    assert!(!Protocol::Anthropic.speaks_openai_http());
    assert!(!Protocol::Gemini.speaks_openai_http());
}

/// The synthetic upstreams of the two llama.cpp classes speak `llama_cpp`;
/// audio.cpp and sd-server stay on `openai` (design §5).
#[test]
fn the_llama_cpp_classes_synthetic_upstreams_speak_llama_cpp() {
    let snap = Snapshot::default();
    for u in [snap.router_upstream(), snap.aux_upstream()] {
        assert_eq!(
            (u.protocol, u.kind),
            (Protocol::LlamaCpp, UpstreamKind::LlamaServer),
            "{}",
            u.name
        );
    }
    assert_eq!(snap.audio_upstream().protocol, Protocol::Openai);
    assert_eq!(snap.image_upstream().protocol, Protocol::Openai);
}
