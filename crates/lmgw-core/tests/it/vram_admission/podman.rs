//! The fake podman

use super::*;

/// Drives [`World`] from the argv the registry renders. `run` marks the model
/// loaded, `stop` unloads it — so the fake driver's free figure moves for the
/// same reasons a real one would.
pub(super) struct Podman {
    world: Arc<Mutex<World>>,
    /// When set, every `podman run` waits here. A barrier of 2 only releases if
    /// two starts are genuinely in flight at the same moment.
    pub(super) barrier: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    /// When set, every `podman run` parks until the test flips it — the seam
    /// that holds a model in `starting` while something else asks for the GPU.
    pub(super) gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    /// When set, every `podman stop` exits non-zero and the model stays
    /// loaded — a container that outlives `vram.unload_timeout_seconds`, or a
    /// podman that cannot reach its own socket. The registry turns that into
    /// `RuntimeError::Stop` *and forgets the entry anyway*, which is why a
    /// hold sweep has to report it rather than let it vanish.
    pub(super) fail_stop: Mutex<bool>,
    /// When set, every `podman inspect` for PIDs parks here (after it is
    /// counted in `World::pid_inspects`) until the test flips it — podman
    /// queued behind a container lock (review finding 4).
    pub(super) inspect_gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    /// When set, every `podman wait` parks here until the test flips it — a
    /// stop in progress, its container already signalled.
    pub(super) wait_gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    /// When set, every `podman stop` parks here until the test flips it,
    /// before it takes effect.
    pub(super) stop_gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    /// When set, every `podman inspect --format json` parks here until the
    /// test flips it — a reconciliation pass held mid-adoption.
    pub(super) inspect_json_gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    /// When set, every `podman rm` exits non-zero and removes nothing.
    pub(super) fail_rm: Mutex<bool>,
}

impl Podman {
    pub(super) fn new(world: Arc<Mutex<World>>) -> Self {
        Self {
            world,
            barrier: Mutex::new(None),
            gate: Mutex::new(None),
            fail_stop: Mutex::new(false),
            inspect_gate: Mutex::new(None),
            wait_gate: Mutex::new(None),
            stop_gate: Mutex::new(None),
            inspect_json_gate: Mutex::new(None),
            fail_rm: Mutex::new(false),
        }
    }

    /// Hold every `podman wait` until the returned sender says go.
    pub(super) fn hold_waits(&self) -> tokio::sync::watch::Sender<bool> {
        hold(&self.wait_gate)
    }

    /// Hold every `podman stop` until the returned sender says go.
    pub(super) fn hold_stops(&self) -> tokio::sync::watch::Sender<bool> {
        hold(&self.stop_gate)
    }

    /// Hold every `podman inspect --format json` until the returned sender
    /// says go.
    pub(super) fn hold_inspects(&self) -> tokio::sync::watch::Sender<bool> {
        hold(&self.inspect_json_gate)
    }
}

fn hold(
    slot: &Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
) -> tokio::sync::watch::Sender<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    *slot.lock().unwrap() = Some(rx);
    tx
}

/// Now, in unix seconds — what podman's `Created` counts in.
pub(super) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The `podman ps --format json` rows of the containers `w` runs.
fn ps_rows(w: &World) -> String {
    let rows: Vec<Value> = w
        .names
        .iter()
        .filter(|(_, model)| w.loaded.contains(*model))
        .filter_map(|(name, model)| {
            let argv = w.argv.get(model)?;
            Some(json!({
                "Names": [name],
                "Labels": labels_of(argv),
                "State": "running",
                "Created": w.created.get(name).copied().unwrap_or(0),
            }))
        })
        .chain(w.extra_ps.iter().cloned())
        .collect();
    serde_json::to_string(&rows).unwrap()
}

/// `podman inspect --format json <name>` for a container `w` knows: its
/// command, its published port and its state. `None` for a name it never ran.
fn inspect_row(w: &World, name: &str) -> Option<String> {
    let model = w.names.get(name)?;
    let argv = w.argv.get(model)?;
    let running = w.loaded.contains(model);
    let port = flag_value(argv, "-p")
        .and_then(|p| p.rsplit(':').nth(1))
        .unwrap_or_default();
    // The image follows the last read-only mount (and an `--entrypoint`, when
    // there is one); the command is everything after it.
    let mut image_at = argv.iter().rposition(|a| a.ends_with(":ro"))? + 1;
    if argv.get(image_at).map(String::as_str) == Some("--entrypoint") {
        image_at += 2;
    }
    Some(
        json!([{
            "Config": {"Cmd": argv[image_at + 1..].to_vec(), "Labels": labels_of(argv)},
            "NetworkSettings": {"Ports": {
                "8080/tcp": [{"HostIp": "127.0.0.1", "HostPort": port}]
            }},
            "State": {
                "Status": if running { "running" } else { "exited" },
                "Running": running,
                "ExitCode": 0,
            },
        }])
        .to_string(),
    )
}

/// The `--label k=v` pairs of a `podman run` argv.
fn labels_of(argv: &[String]) -> HashMap<String, String> {
    argv.iter()
        .zip(argv.iter().skip(1))
        .filter(|(f, _)| *f == "--label")
        .filter_map(|(_, kv)| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Park until `gate` says go (or its sender is gone).
async fn parked(gate: Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(mut rx) = gate {
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn label_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().find_map(|a| a.strip_prefix(key))
}

/// sd-server's own `--help`, as the committed build prints it — what
/// `ops::image_model_set` checks a row's keys against.
const SD_HELP: &str = include_str!("../../fixtures/sdcpp/sd-server-help-c678dfe.txt");

fn is_sdcpp_help(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "--help")
        && argv
            .windows(2)
            .any(|w| w[0] == "--entrypoint" && w[1] == "/sd-server")
}

#[async_trait::async_trait]
impl CommandRunner for Podman {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman", "the registry only ever shells podman");
        self.world.lock().unwrap().calls.push(args.to_vec());
        let ok = CmdOutput {
            status: 0,
            stdout: "c0ffee\n".into(),
            stderr: String::new(),
        };
        match args[0].as_str() {
            // Reconciliation's enumeration: an empty JSON list, which is what
            // podman answers on a box with no managed containers — unless the
            // test has the world report what it runs (`World::reports`).
            "ps" => {
                let w = self.world.lock().unwrap();
                Ok(CmdOutput {
                    status: 0,
                    stdout: if w.reports { ps_rows(&w) } else { "[]".into() },
                    stderr: String::new(),
                })
            }
            "inspect"
                if args.get(2).map(String::as_str) == Some("json")
                    && self.world.lock().unwrap().reports =>
            {
                let gate = self.inspect_json_gate.lock().unwrap().clone();
                parked(gate).await;
                let name = args.last().cloned().unwrap_or_default();
                let w = self.world.lock().unwrap();
                Ok(match inspect_row(&w, &name) {
                    Some(json) => CmdOutput {
                        status: 0,
                        stdout: json,
                        stderr: String::new(),
                    },
                    None => CmdOutput {
                        status: 125,
                        stdout: String::new(),
                        stderr: format!("Error: no such container {name}\n"),
                    },
                })
            }
            "wait" => {
                let gate = self.wait_gate.lock().unwrap().clone();
                parked(gate).await;
                Ok(ok)
            }
            // The image class's flag-vocabulary probe (image-generation §2.6):
            // a throwaway `--entrypoint /sd-server … --help` container. It is a
            // `run` too, so it has to be matched before the real one — it
            // carries no `--name` and no model label, and a start that reads it
            // as a container start would assert on their absence.
            "run" if is_sdcpp_help(args) => Ok(CmdOutput {
                status: 0,
                stdout: SD_HELP.into(),
                stderr: String::new(),
            }),
            "run" => {
                let barrier = self.barrier.lock().unwrap().clone();
                if let Some(b) = barrier {
                    b.wait().await;
                }
                let gate = self.gate.lock().unwrap().clone();
                if let Some(mut rx) = gate {
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                }
                let name = flag_value(args, "--name").unwrap_or_default().to_string();
                let model = label_value(args, "lmgw.model=")
                    .expect("every managed container is labelled with its model")
                    .to_string();
                let port: u16 = flag_value(args, "-p")
                    .and_then(|p| p.rsplit(':').nth(1))
                    .and_then(|p| p.parse().ok())
                    .expect("every managed container publishes a host port");
                let file = flag_value(args, "-m")
                    .and_then(|m| m.rsplit('/').next())
                    .unwrap_or_default()
                    .to_string();
                let ctx = flag_value(args, "--ctx-size").map(str::to_string);
                let slots = flag_value(args, "--parallel")
                    .and_then(|p| p.parse::<u64>().ok())
                    .unwrap_or(1)
                    .max(1);
                let mut w = self.world.lock().unwrap();
                match ctx.as_deref().and_then(|c| c.parse::<u64>().ok()) {
                    Some(c) if w.context_rules => w.slot_ctx.insert(port, c / slots),
                    _ => w.slot_ctx.remove(&port),
                };
                w.run_files.push((model.clone(), file.clone(), ctx));
                if w.fail_run_file.as_deref() == Some(file.as_str()) {
                    return Ok(CmdOutput {
                        status: 125,
                        stdout: String::new(),
                        stderr: format!("Error: /models/{file}: no such file or directory\n"),
                    });
                }
                if let Some(bytes) = w.files.get(&file).copied() {
                    w.size.insert(model.clone(), bytes);
                }
                let pid = 5_000_000 + w.runs.len() as u32;
                w.pids.insert(model.clone(), pid);
                w.created.insert(name.clone(), unix_now());
                w.names.insert(name, model.clone());
                w.ports.insert(port, model.clone());
                w.loaded.insert(model.clone());
                w.argv.insert(model.clone(), args.to_vec());
                w.runs.push(model);
                Ok(ok)
            }
            // Per-process attribution's `State.Pid` read (§4.7): one line per
            // running container it was asked about, and podman's own exit 125
            // plus a stderr line for any name it does not know.
            "inspect" if args.get(2).map(String::as_str) == Some(HOST_PID_FORMAT) => {
                let names = args[3..].to_vec();
                self.world.lock().unwrap().pid_inspects.push(names.clone());
                let gate = self.inspect_gate.lock().unwrap().clone();
                parked(gate).await;
                let w = self.world.lock().unwrap();
                let (mut stdout, mut stderr) = (String::new(), String::new());
                for n in &names {
                    let pid = w
                        .names
                        .get(n)
                        .filter(|m| w.loaded.contains(*m))
                        .and_then(|m| w.pids.get(m));
                    match pid {
                        Some(pid) => stdout.push_str(&format!("{n} {pid}\n")),
                        None => stderr.push_str(&format!("Error: no such object: \"{n}\"\n")),
                    }
                }
                Ok(CmdOutput {
                    status: if stderr.is_empty() { 0 } else { 125 },
                    stdout,
                    stderr,
                })
            }
            "rm" if *self.fail_rm.lock().unwrap() => Ok(CmdOutput {
                status: 1,
                stdout: String::new(),
                stderr: "Error: cannot remove container: it is in use\n".into(),
            }),
            "stop" => {
                let gate = self.stop_gate.lock().unwrap().clone();
                parked(gate).await;
                if *self.fail_stop.lock().unwrap() {
                    // Non-zero and the model stays in `loaded`: the container
                    // really is still on the GPU afterwards, which is the
                    // whole point of the seam.
                    return Ok(CmdOutput {
                        status: 1,
                        stdout: String::new(),
                        stderr: "Error: container is not stopping\n".into(),
                    });
                }
                let name = args.last().cloned().unwrap_or_default();
                let mut w = self.world.lock().unwrap();
                if let Some(model) = w.names.get(&name).cloned() {
                    w.loaded.remove(&model);
                    w.stops.push(model);
                }
                Ok(ok)
            }
            _ => Ok(ok),
        }
    }
}
