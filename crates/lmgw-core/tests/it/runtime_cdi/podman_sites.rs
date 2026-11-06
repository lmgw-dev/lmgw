//! Every place the workspace's source starts podman (machine-check design
//! §3.6.4), on a list with the reason it cannot give a container the
//! graphics card without lmgw's CDI spec dir: it takes the option from
//! `runtime::cdi`, the run-time check (`OwnSpec::refuse_unpointed`) sees its
//! argv, or it runs nothing that has a device. A new site fails this test
//! until it is listed with its reason, so whoever adds one has to answer that
//! question; a site that went or moved fails it too, so the list stays true.
//!
//! The source is read as text: `Command::new("podman")`, `.run("podman"`,
//! `.spawn("podman"` and `.spawn_env("podman"`, under every member's `src/`
//! (unit tests included), not in the integration tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PATTERNS: [&str; 4] = [
    "Command::new(\"podman\")",
    ".run(\"podman\"",
    ".spawn(\"podman\"",
    ".spawn_env(\"podman\"",
];

/// A file, a pattern, how often it is there, and why each one is safe.
struct Site {
    file: &'static str,
    pattern: &'static str,
    count: usize,
    why: &'static str,
}

const AGENT_SPAWNER: &str = "through the agent spawner, which AppState::agent_spawner hands out \
                             behind CdiChecked, the run-time check; agents and their packages \
                             get no graphics card";

const SITES: &[Site] = &[
    Site {
        file: "crates/lmgw-core/src/agents/container.rs",
        pattern: ".spawn(\"podman\"",
        count: 1,
        why: AGENT_SPAWNER,
    },
    Site {
        file: "crates/lmgw-core/src/agents/container.rs",
        pattern: ".run(\"podman\"",
        count: 5,
        why: AGENT_SPAWNER,
    },
    Site {
        file: "crates/lmgw-core/src/agents/package.rs",
        pattern: ".run(\"podman\"",
        count: 5,
        why: AGENT_SPAWNER,
    },
    Site {
        file: "crates/lmgw-core/src/agents/service.rs",
        pattern: ".run(\"podman\"",
        count: 2,
        why: AGENT_SPAWNER,
    },
    Site {
        file: "crates/lmgw-core/src/backends/pull.rs",
        pattern: ".spawn(\"podman\"",
        count: 1,
        why: "podman pull, through the checked agent spawner: it starts no container",
    },
    Site {
        file: "crates/lmgw-core/src/backends/pull/service.rs",
        pattern: "Command::new(\"podman\")",
        count: 1,
        why: "podman system service on this instance's private socket (machine-check design §5): \
              it starts no container itself, and lmgw asks it for nothing but libpod image pulls",
    },
    Site {
        file: "crates/lmgw-core/src/backends/run/mod.rs",
        pattern: ".spawn_env(\"podman\"",
        count: 1,
        why: "podman build, through the checked agent spawner: its argv (build_argv) names no \
              device",
    },
    Site {
        file: "crates/lmgw-core/src/backends/run/podman.rs",
        pattern: ".run(\"podman\"",
        count: 1,
        why: "Podman::run, the one runner call of the builds' and the Images view's verbs (info, \
              images, system df, image inspect, tag, untag, rmi, ps, rm, unshare rm): none starts \
              a container, and refuse_unpointed checks every argv first",
    },
    Site {
        file: "crates/lmgw-core/src/bench/launcher.rs",
        pattern: ".run(\"podman\"",
        count: 1,
        why: "PodmanLauncher, the benchmark's container: its argv takes cdi::ready_globals, and \
              refuse_unpointed checks every call first",
    },
    Site {
        file: "crates/lmgw-core/src/llama_caps.rs",
        pattern: "Command::new(\"podman\")",
        count: 1,
        why: "a unit test that reads --help from a local image when one is there, with no device",
    },
    Site {
        file: "crates/lmgw-core/src/machine/gather.rs",
        pattern: ".run(\"podman\"",
        count: 1,
        why: "the machine check's own reads (--version, --help, info --format json): none starts \
              a container",
    },
    Site {
        file: "crates/lmgw-core/src/mcp/connect.rs",
        pattern: "Command::new(\"podman\")",
        count: 1,
        why: "an isolated MCP server's podman start --attach: it names no device (start takes \
              none; the create it follows took podman_globals and passed refuse_unpointed), so \
              the check has nothing to refuse here and the globals are passed for symmetry",
    },
    Site {
        file: "crates/lmgw-core/src/mcp/container.rs",
        pattern: "Command::new(\"podman\")",
        count: 3,
        why: "an isolated MCP server's podman create, which takes podman_globals and passes \
              refuse_unpointed first, and two podman rm --force of its container",
    },
    Site {
        file: "crates/lmgw-core/src/runtime/cdi.rs",
        pattern: ".run(\"podman\"",
        count: 1,
        why: "podman --help, which asks whether this Podman takes --cdi-spec-dir",
    },
    Site {
        file: "crates/lmgw-core/src/runtime/registry/podman_ops.rs",
        pattern: ".run(\"podman\"",
        count: 1,
        why: "Registry::podman_argv, the registry's one runner call: model starts and throwaway \
              probes take cdi::ready_globals, and refuse_unpointed checks every argv first",
    },
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.rs` file under `dir`, skipping integration-test dirs (`tests/`
/// directly under a member).
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let name = entry.file_name();
            let member_tests = name == "tests" && dir.join("Cargo.toml").exists();
            if !member_tests && name != "target" && name != "node_modules" {
                sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_podman_spawn_site_is_listed_with_its_reason() {
    let root = workspace();
    let mut files = Vec::new();
    for dir in ["crates", "src-tauri", "examples"] {
        sources(&root.join(dir), &mut files);
    }
    assert!(
        files.len() > 100,
        "the scan found {} source files under {}: is it reading the workspace?",
        files.len(),
        root.display()
    );
    let mut found: BTreeMap<(String, &str), usize> = BTreeMap::new();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for pattern in PATTERNS {
            let n = text.matches(pattern).count();
            if n > 0 {
                found.insert((rel.clone(), pattern), n);
            }
        }
    }
    let listed: BTreeMap<(String, &str), (usize, &str)> = SITES
        .iter()
        .map(|s| ((s.file.to_string(), s.pattern), (s.count, s.why)))
        .collect();
    let mut wrong = Vec::new();
    for ((file, pattern), n) in &found {
        match listed.get(&(file.clone(), *pattern)) {
            None => wrong.push(format!(
                "{file}: {n} × `{pattern}`, not listed. Give it the option from \
                 runtime::cdi (or route it through a checked runner), then list it here \
                 with the reason it cannot start a container with the graphics card \
                 without lmgw's spec dir"
            )),
            Some((count, _)) if count != n => wrong.push(format!(
                "{file}: {n} × `{pattern}`, listed as {count}: a site was added or removed; \
                 say why the new one is safe, or lower the count"
            )),
            Some(_) => {}
        }
    }
    for s in SITES {
        assert!(!s.why.trim().is_empty(), "{}: no reason", s.file);
        if !found.contains_key(&(s.file.to_string(), s.pattern)) {
            wrong.push(format!(
                "{}: listed with {} × `{}`, and there is none: drop it from the list",
                s.file, s.count, s.pattern
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "podman spawn sites:\n{}",
        wrong.join("\n")
    );
}
