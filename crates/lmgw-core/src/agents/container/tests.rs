//! The pure half of the container runner: the argv, the two documents it
//! writes, the run directory and the image-reference rules.
//!
//! Everything here runs without podman, without a gateway and without a job —
//! `tests/it/agents_container.rs` drives the executor end to end against a fake
//! spawner, and the real-podman legs are `#[ignore]`d there.

use super::*;
use crate::store::AgentRow;

fn limits() -> Limits {
    Limits::default()
}

fn agent(manifest_text: &str, config: &str) -> Agent {
    Agent::from_row(AgentRow {
        id: "labeler".into(),
        manifest: manifest_text.into(),
        config: config.into(),
        enabled: true,
        source: "authored".into(),
        provenance: String::new(),
        dev_url: None,
        created_at: String::new(),
        updated_at: String::new(),
    })
    .expect("the fixture manifest parses")
}

const DOC: &str = r#"{
  "schema_version": 1,
  "id": "labeler",
  "name": "Labeler",
  "model": { "alias": "{{config.model}}" },
  "config": { "schema": { "type": "object", "properties": {
    "model": { "type": "string", "format": "model_alias" },
    "label_prefix": { "type": "string" },
    "api_token": { "type": "string", "format": "secret" }
  } } },
  "run": { "kind": "container", "image": "localhost/labeler:1",
           "columns": ["subject"], "phases": ["run", "apply"] }
}"#;

/// The same agent with two slots: a required-nothing `rw` directory and a `ro`
/// file, for everything mounts changes about the argv and the two documents.
const MOUNT_DOC: &str = r#"{
  "schema_version": 1,
  "id": "labeler",
  "name": "Labeler",
  "model": { "alias": "{{config.model}}" },
  "config": { "schema": { "type": "object", "properties": {
    "model": { "type": "string", "format": "model_alias" },
    "notes": { "type": "string", "format": "directory", "access": "rw" },
    "rules": { "type": "string", "format": "file" }
  } } },
  "run": { "kind": "container", "image": "localhost/labeler:1",
           "columns": ["subject"], "phases": ["run", "apply"] }
}"#;

// ---------------------------------------------------------------------------
// The argv (§6.1)
// ---------------------------------------------------------------------------

fn argv_with(limits: &Limits) -> Vec<String> {
    argv_full(limits, &[], false)
}

/// The argv with whatever mounts and `keep_id` a test wants beyond lmgw's own
/// `input.json`.
fn argv_full(limits: &Limits, extra: &[Mount], keep_id: bool) -> Vec<String> {
    let env = vec![("LMGW_PHASE".to_string(), "run".to_string())];
    let mut mounts = vec![Mount::lmgw_file(
        PathBuf::from("/run/user/1000/lmgw/lmgw/run-7/input.json"),
        "/lmgw/input.json",
    )];
    mounts.extend(extra.iter().cloned());
    run_argv(&RunSpecArgs {
        name: "lmgw-agent-labeler-7",
        prefix: "lmgw",
        agent_id: "labeler",
        run: "7",
        image: "localhost/labeler:1",
        pull: PullPolicy::Never,
        entrypoint: None,
        args: &[],
        limits,
        env: &env,
        mounts: &mounts,
        keep_id,
        network: &[],
        service: None,
    })
}

#[test]
fn the_argv_is_the_one_the_design_prints() {
    let argv = argv_with(&limits());
    assert_eq!(
        argv,
        [
            "run",
            "--rm",
            "--replace",
            "--name",
            "lmgw-agent-labeler-7",
            "--label",
            "lmgw.instance=lmgw",
            "--label",
            "lmgw.kind=agent",
            "--label",
            "lmgw.agent=labeler",
            "--label",
            "lmgw.run=7",
            "--memory",
            "512m",
            "--cpus",
            "2",
            "--pids-limit",
            "256",
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--cap-drop=ALL",
            "--security-opt",
            "no-new-privileges",
            "--pull=never",
            "-e",
            "LMGW_PHASE=run",
            "-v",
            "/run/user/1000/lmgw/lmgw/run-7/input.json:/lmgw/input.json:ro,Z",
            "localhost/labeler:1",
        ]
        .map(String::from)
    );
}

#[test]
fn a_zero_limit_omits_its_flag_rather_than_inventing_a_number() {
    let none = Limits {
        memory_mb: 0,
        cpus: 0.0,
        pids: 0,
        deadline_seconds: 0,
        stop_grace_seconds: 0,
        read_only: false,
    };
    let argv = argv_with(&none);
    for flag in [
        "--memory",
        "--cpus",
        "--pids-limit",
        "--read-only",
        "--tmpfs",
    ] {
        assert!(
            !argv.iter().any(|a| a == flag),
            "{flag} is still on the argv at 0: {argv:?}"
        );
    }
    // What is not negotiable stays on regardless.
    assert!(argv.iter().any(|a| a == "--cap-drop=ALL"));
    assert!(argv.iter().any(|a| a == "no-new-privileges"));
}

/// The owner's folder is `:z` — shared — and lmgw's own files stay `:Z`
/// (mounts §5.5). The two are on the same argv, which is the whole point: a
/// run directory nobody else will ever hold, and a folder two containers may.
#[test]
fn a_bound_mount_is_shared_and_lmgws_own_files_stay_private() {
    let notes = Mount {
        host: PathBuf::from("/home/alice/Notes"),
        inside: "/lmgw/mounts/notes".to_string(),
        access: Access::Rw,
        label: Label::Shared,
    };
    let argv = argv_full(&limits(), &[notes], true);
    let flags: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(i, _)| i > &0 && argv[i - 1] == "-v")
        .map(|(_, a)| a)
        .collect();
    assert_eq!(
        flags,
        [
            "/run/user/1000/lmgw/lmgw/run-7/input.json:/lmgw/input.json:ro,Z",
            "/home/alice/Notes:/lmgw/mounts/notes:rw,z",
        ],
        "{argv:?}"
    );
}

/// `--userns=keep-id` is the manifest's property, and it lands where §5.5
/// prints it: after the labels, before the limits.
#[test]
fn keep_id_is_off_by_default_and_lands_after_the_labels() {
    let plain = argv_with(&limits());
    assert!(
        !plain.iter().any(|a| a == "--userns=keep-id"),
        "an agent with no mount field sees no change: {plain:?}"
    );
    let argv = argv_full(&limits(), &[], true);
    let at = |s: &str| argv.iter().position(|a| a == s).unwrap();
    assert!(at("lmgw.run=7") < at("--userns=keep-id"), "{argv:?}");
    assert!(at("--userns=keep-id") < at("--memory"), "{argv:?}");
}

/// The uid on the start line is the one this process really runs as — read
/// from `/proc/self`, never a number lmgw picked (§5.5).
#[test]
fn the_keep_id_line_names_the_uid_the_process_actually_has() {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").unwrap().uid();
    assert_eq!(process_uid(), Some(uid));
    assert_eq!(
        keep_id_note(),
        format!(
            "this manifest declares mounts: the container runs as uid {uid} (--userns=keep-id)"
        )
    );
}

/// The run log's mount line, §5.6 verbatim — including the half an owner
/// cannot learn anywhere else: the relabel is recursive and permanent.
#[test]
fn the_mount_line_is_the_one_the_design_prints() {
    let a = agent(MOUNT_DOC, r#"{"notes":"/home/alice/Notes"}"#);
    let field = a.manifest.mount_fields().next().unwrap();
    let line = mount_note(&mounts::Binding {
        field,
        host: PathBuf::from("/home/alice/Notes"),
    });
    assert_eq!(
        line,
        "mount notes: /home/alice/Notes → /lmgw/mounts/notes (rw, directory; relabelled \
         container_file_t, recursive, permanent)"
    );
}

#[test]
fn no_env_flag_can_carry_a_secret() {
    // `podman inspect` prints a container's environment, so the token travels
    // in the secrets file (§3.1). The env builder is the only thing that
    // renders `-e`, and this is everything it renders.
    let env = env_for(
        "labeler",
        "http://host.containers.internal:8787",
        Phase::Run.as_str(),
        Some(7),
        600,
    );
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "LMGW_BASE_URL",
            "LMGW_API_BASE",
            "LMGW_MCP_URL",
            "LMGW_LEDGER_URL",
            "LMGW_AGENT",
            "LMGW_RUN",
            "LMGW_PHASE",
            "LMGW_INPUT",
            "LMGW_SECRETS",
            "LMGW_DEADLINE_SECONDS",
        ]
    );
    assert!(
        !names
            .iter()
            .any(|n| n.contains("TOKEN") || n.contains("SECRET") && *n != "LMGW_SECRETS"),
        "{names:?}"
    );
    let by = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
    assert_eq!(
        by("LMGW_API_BASE"),
        "http://host.containers.internal:8787/v1"
    );
    assert_eq!(
        by("LMGW_LEDGER_URL"),
        "http://host.containers.internal:8787/api/agents/runs/7/events"
    );
    assert_eq!(by("LMGW_SECRETS"), "/lmgw/secrets.json");
}

#[test]
fn an_entrypoint_and_args_land_around_the_image() {
    let args = vec!["--verbose".to_string()];
    let argv = run_argv(&RunSpecArgs {
        name: "n",
        prefix: "lmgw",
        agent_id: "a",
        run: "1",
        image: "img",
        pull: PullPolicy::Always,
        entrypoint: Some("/bin/sh"),
        args: &args,
        limits: &limits(),
        env: &[],
        mounts: &[],
        keep_id: false,
        network: &[],
        service: None,
    });
    let at = |s: &str| argv.iter().position(|a| a == s).unwrap();
    assert!(at("--entrypoint") < at("img"), "{argv:?}");
    assert_eq!(argv.last().unwrap(), "--verbose");
    assert!(argv.iter().any(|a| a == "--pull=always"));
}

#[test]
fn the_container_name_carries_the_prefix_and_the_run() {
    // The prefix is what keeps a dev instance off the real one's names, and the
    // run id is what keeps two runs of the same agent apart.
    assert_eq!(
        container_name("lmgw-dev", "Mail Labeler", 12),
        "lmgw-dev-agent-mail-labeler-12"
    );
    assert_ne!(
        container_name("lmgw", "a", 1),
        container_name("lmgw-dev", "a", 1)
    );
}

// ---------------------------------------------------------------------------
// The two documents (§6.2)
// ---------------------------------------------------------------------------

#[test]
fn input_carries_the_config_without_secrets_and_rows_only_for_apply() {
    let a = agent(
        DOC,
        r#"{"model":"qwen3.8","label_prefix":"ai","api_token":"sk-live"}"#,
    );
    let doc = input_document(&a, Phase::Run, 7, &[]);
    assert_eq!(doc["phase"], "run");
    assert_eq!(doc["agent"]["id"], "labeler");
    assert_eq!(doc["run"]["id"], 7);
    assert_eq!(doc["config"]["label_prefix"], "ai");
    assert!(
        doc["config"].get("api_token").is_none(),
        "a secret must never reach input.json: {doc}"
    );
    // Absent, not empty: a container can tell "no gate" from "an empty gate".
    assert!(doc.get("rows").is_none(), "{doc}");

    let rows = vec![Row {
        id: "m1".into(),
        output: serde_json::json!({ "category": "news" }),
        columns: serde_json::json!({ "subject": "Hi" })
            .as_object()
            .unwrap()
            .clone(),
        ..Default::default()
    }];
    let doc = input_document(&a, Phase::Apply, 7, &rows);
    assert_eq!(
        doc["rows"],
        serde_json::json!([{ "id": "m1", "category": "news" }]),
        "Row::for_apply's shape, review columns deliberately excluded"
    );
}

/// `input.json` under a bound slot (§5.6): the container path in `config`, the
/// `mounts` array beside it, and the host path nowhere in the document.
#[test]
fn input_substitutes_the_container_path_and_lists_the_mounts() {
    let a = agent(
        MOUNT_DOC,
        r#"{"model":"qwen3.8","notes":"/home/alice/Notes"}"#,
    );
    let doc = input_document(&a, Phase::Run, 7, &[]);
    assert_eq!(doc["config"]["notes"], "/lmgw/mounts/notes");
    assert_eq!(
        doc["mounts"],
        serde_json::json!([
            { "field": "notes", "path": "/lmgw/mounts/notes",
              "kind": "directory", "access": "rw" }
        ])
    );
    // An unbound optional slot is absent from both, rather than present and
    // empty — "not given" is not "given as nothing".
    assert!(doc["config"].get("rules").is_none(), "{doc}");
    // The whole document, not just the field: this is the invariant, and a
    // host path smuggled into any corner of it is the thing that breaks it.
    assert!(
        !doc.to_string().contains("/home/alice/Notes"),
        "a host path reached input.json: {doc}"
    );
}

/// A schema with no slot in it still says so, with `[]` rather than by being
/// absent: a container can enumerate without knowing whether it has any.
#[test]
fn input_carries_an_empty_mounts_array_when_the_schema_declares_none() {
    let a = agent(DOC, r#"{"model":"qwen3.8"}"#);
    assert_eq!(
        input_document(&a, Phase::Run, 7, &[])["mounts"],
        serde_json::json!([])
    );
}

/// `{{config.notes}}` renders the container path (§5.6) — the template is read
/// on the far side of the wall, so it must be the far side's path.
#[test]
fn a_template_resolves_a_mount_to_the_container_path() {
    let a = agent(
        MOUNT_DOC,
        r#"{"model":"qwen3.8","notes":"/home/alice/Notes"}"#,
    );
    let ctx = crate::agents::template::Ctx {
        config: a.effective_config(),
        ..Default::default()
    };
    assert_eq!(
        crate::agents::template::render_text("read {{config.notes}}/today.md", &ctx),
        "read /lmgw/mounts/notes/today.md"
    );
}

#[test]
fn secrets_carry_the_token_and_the_secret_fields_and_nothing_else() {
    let a = agent(
        DOC,
        r#"{"model":"qwen3.8","label_prefix":"ai","api_token":"sk-live"}"#,
    );
    let doc = secrets_document(&a, "lmgw-agent-deadbeef", true);
    assert_eq!(doc["token"], "lmgw-agent-deadbeef");
    assert_eq!(doc["config"], serde_json::json!({ "api_token": "sk-live" }));
    // A **script** run gets the token and nothing else (§4.2): a script is the
    // deterministic half of an agent and has no business holding a credential.
    let scripted = secrets_document(&a, "lmgw-agent-deadbeef", false);
    assert_eq!(scripted["token"], "lmgw-agent-deadbeef");
    assert_eq!(scripted["config"], serde_json::json!({}));
}

#[test]
fn a_secret_nobody_filled_in_is_absent_rather_than_null() {
    let a = agent(DOC, r#"{"model":"qwen3.8"}"#);
    assert_eq!(
        secrets_document(&a, "t", true)["config"],
        serde_json::json!({})
    );
}

// ---------------------------------------------------------------------------
// The run directory (§6.2)
// ---------------------------------------------------------------------------

#[test]
fn the_run_directory_is_0700_its_files_0600_and_it_is_gone_afterwards() {
    use std::os::unix::fs::PermissionsExt;
    // An explicit root, so this writes into the tempdir rather than into the
    // real `$XDG_RUNTIME_DIR` the way a derived one would.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lmgw").join("lmgw-unit-test");
    let path = {
        let dir = RunDir::create(&root, 4242).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.path()), 0o700);
        // Every level this call created, not only the leaf: an intermediate
        // directory left at the umask would be world-readable.
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&tmp.path().join("lmgw")), 0o700);
        let f = dir.write("secrets.json", "{}").unwrap();
        assert_eq!(mode(&f), 0o600);
        dir.path().to_path_buf()
    };
    assert!(
        !path.exists(),
        "the run directory has to go with the run: {}",
        path.display()
    );
}

#[test]
fn run_dirs_are_scoped_by_container_prefix() {
    // `$XDG_RUNTIME_DIR` is shared by every lmgw on the box and job ids are
    // per-database, so without the prefix a dev instance and the real one would
    // fight over `run-7`.
    let tmp = tempfile::tempdir().unwrap();
    let (a, _) = runs_root(tmp.path(), "lmgw");
    let (b, _) = runs_root(tmp.path(), "lmgw-dev");
    assert_ne!(a, b);
}

// ---------------------------------------------------------------------------
// Small rules
// ---------------------------------------------------------------------------

#[test]
fn a_local_image_reference_is_recognised_as_local() {
    for local in [
        "localhost/mail-labeler:1",
        "mail-labeler:1",
        "library/alpine",
        "mail-labeler",
    ] {
        assert!(is_local_image(local), "{local}");
    }
    for remote in [
        "docker.io/library/alpine",
        "registry.fedoraproject.org/fedora-minimal:44",
        "ghcr.io/acme/agent:2",
        "registry:5000/acme/agent",
    ] {
        assert!(!is_local_image(remote), "{remote}");
    }
}

#[test]
fn the_base_url_takes_the_port_from_the_bind_addr_and_never_guesses() {
    // A gateway on every interface is on the one `host.containers.internal`
    // names, and needs nothing on the argv.
    assert_eq!(
        gateway_access("0.0.0.0:8787").unwrap(),
        ("http://host.containers.internal:8787".to_string(), vec![])
    );
    assert_eq!(
        gateway_access("10.0.0.5:9000").unwrap(),
        ("http://host.containers.internal:9000".to_string(), vec![])
    );
    // A loopback-bound gateway — lmgw's own default — is *not* reachable at
    // `host.containers.internal` (connection refused, measured on podman
    // 5.8.4/pasta). pasta forwards exactly the one port instead.
    for addr in ["127.0.0.1:8001", "localhost:8001", "[::1]:8001"] {
        assert_eq!(
            gateway_access(addr).unwrap(),
            (
                "http://127.0.0.1:8001".to_string(),
                vec!["--network".to_string(), "pasta:-T,8001".to_string()]
            ),
            "{addr}"
        );
    }
    // No fallback port: a bind addr with no port is a config problem the run
    // has to say out loud, not one it papers over with 8787.
    let err = base_url("localhost").unwrap_err();
    assert!(err.contains("names no port"), "{err}");
}

/// The shim is embedded and written **per run**, never into a shared cache:
/// `:Z` rewrites the source file's MCS label, so a second run mounting the same
/// path would revoke the first container's read access mid-run.
#[test]
fn the_shim_is_the_embedded_module_and_is_written_per_run() {
    assert!(
        SHIM.contains("export"),
        "the shim does not import an ES module"
    );
    assert!(SHIM.contains("tools/call"), "the shim makes no tool calls");
    assert!(SHIM.contains("SIGTERM"), "the shim has no cancel handler");
    assert!(
        SHIM.contains("process.exit(1)"),
        "the shim never leaves on its own after a cancel"
    );
    assert!(
        SHIM.contains("console.log = "),
        "the shim lets a script write raw lines to the ledger"
    );

    // It lands in the run directory at `0600`, beside the secrets, and goes
    // with them.
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = RunDir::create(tmp.path(), 3).unwrap();
    let path = dir.write("shim.mjs", SHIM).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), SHIM);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the shim is not 0600");
    drop(dir);
    assert!(!path.exists(), "the shim outlived its run");
}

#[test]
fn the_failure_excerpt_is_the_last_lines_newline_joined_and_empty_when_there_are_none() {
    assert_eq!(excerpt(&std::collections::VecDeque::new()), "");
    // The tail is bounded where it is *collected* — the deque never grows past
    // the excerpt — so the full stderr lives in the run log and nowhere else.
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for i in 0..30 {
        if tail.len() == STDERR_EXCERPT_LINES {
            tail.pop_front();
        }
        tail.push_back(format!("line {i}"));
    }
    assert_eq!(tail.len(), STDERR_EXCERPT_LINES);
    let e = excerpt(&tail);
    assert!(e.starts_with("\nline 18\nline 19"), "{e:?}");
    assert!(e.ends_with("line 29"), "{e:?}");
    assert_eq!(e.lines().count(), STDERR_EXCERPT_LINES + 1, "{e:?}");
}

#[test]
fn a_child_killed_by_a_signal_is_not_reported_as_an_exit_status() {
    assert_eq!(
        Exit::Code(3).describe(),
        "the container exited with status 3"
    );
    assert_eq!(
        Exit::Signal(9).describe(),
        "podman run was killed by signal 9"
    );
}

// ---------------------------------------------------------------------------
// The signal seam (container-builds §5): against real children, since what is
// under test is exactly the process plumbing a fake would stand in for.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_spawned_child_can_be_sent_sigterm_and_says_it_was_a_signal() {
    let mut spawned = TokioSpawner
        .spawn("sleep", &["30".to_string()])
        .await
        .unwrap();
    assert!(spawned.kill.send(Signal::Term), "the child is running");
    let exit = tokio::time::timeout(Duration::from_secs(10), &mut spawned.status)
        .await
        .expect("SIGTERM ends a sleep")
        .unwrap();
    assert_eq!(exit, Exit::Signal(libc::SIGTERM));
    drop(spawned.status);
    assert!(
        !spawned.kill.send(Signal::Kill),
        "an ended child has nothing left to signal"
    );
}

#[tokio::test]
async fn terminate_escalates_to_sigkill_when_sigterm_is_ignored() {
    // SIG_IGN survives exec, so `sleep` itself ignores SIGTERM — the shape of
    // a build step that does not stop when asked.
    let mut spawned = TokioSpawner
        .spawn(
            "sh",
            &[
                "-c".to_string(),
                "trap '' TERM; echo ready; exec sleep 30".to_string(),
            ],
        )
        .await
        .unwrap();
    // Wait for the trap, or the SIGTERM could land before it and just work.
    assert_eq!(spawned.stdout.recv().await.as_deref(), Some("ready"));
    let started = std::time::Instant::now();
    let exit = terminate(
        &spawned.kill,
        &mut spawned.status,
        Duration::from_millis(300),
    )
    .await
    .unwrap();
    assert_eq!(exit, Exit::Signal(libc::SIGKILL));
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "SIGKILL came only after the grace"
    );
}

#[tokio::test]
async fn terminating_a_child_that_already_exited_reports_its_exit() {
    let mut spawned = TokioSpawner
        .spawn("sh", &["-c".to_string(), "exit 3".to_string()])
        .await
        .unwrap();
    // Let it exit on its own first; it is a zombie until `status` reaps it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let exit = terminate(&spawned.kill, &mut spawned.status, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(exit, Exit::Code(3));
}

/// What a fake spawner does with the seam: drain the receiving end and end
/// the fake child the way the real one would.
#[tokio::test]
async fn a_fake_child_can_honour_the_signal_line() {
    let (kill, mut signals) = KillHandle::channel();
    let mut status: BoxFuture<'static, std::io::Result<Exit>> = Box::pin(async move {
        match signals.recv().await {
            Some(sig) => Ok(Exit::Signal(sig.raw())),
            None => Ok(Exit::Code(0)),
        }
    });
    let exit = terminate(&kill, &mut status, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(exit, Exit::Signal(libc::SIGTERM));
    assert!(!KillHandle::detached().send(Signal::Term));
}
