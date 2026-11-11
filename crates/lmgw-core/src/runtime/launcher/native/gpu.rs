//! Which card a native engine sees: the environment that stands in for the
//! container's device flags (Windows design §4.1).
//!
//! The source is the model's resolved `extra_run_args`, the one place a
//! model's card is chosen today, read here and nowhere else for a native
//! start:
//!
//! - **No card** — the row runs on the CPU ([`Placement::Cpu`]), or its run
//!   args would give a container none
//!   ([`passes_gpu`](crate::runtime::audio::passes_gpu)): every GPU backend
//!   is hidden. `CUDA_VISIBLE_DEVICES=-1` and `HIP_VISIBLE_DEVICES=-1` (an
//!   invalid first index makes the runtime list no device), and an empty
//!   `GGML_VK_VISIBLE_DEVICES` (ggml's Vulkan backend then takes no device:
//!   it reads the indices from a set variable and only enumerates every card
//!   when it is unset, read in `ggml-vulkan.cpp` at llama.cpp b12236).
//! - **NVIDIA cards by name** — `--device nvidia.com/gpu=<n>` (repeated, or
//!   `--gpus device=<n>,…`): `CUDA_VISIBLE_DEVICES=<n>,…` with
//!   `CUDA_DEVICE_ORDER=PCI_BUS_ID`, so CUDA counts the cards in the order
//!   CDI and `nvidia-smi` number them. Vulkan and HIP builds still see every
//!   card: their device order is not mapped to NVIDIA's yet, and a pinned
//!   Vulkan build started for a row that names cards says so in the start's
//!   warnings (`start.rs`).
//! - **Every card** — `nvidia.com/gpu=all`, `--gpus all`, `/dev/dri`,
//!   `/dev/kfd`: nothing is set.
//!
//! `-e`/`--env NAME=VALUE` in the run args is the process's environment as
//! written, after the variables above, so a row that sets one itself wins —
//! with one exception: when no card is passed, the hiding variables win over
//! the row's own `CUDA_VISIBLE_DEVICES`, `HIP_VISIBLE_DEVICES` and
//! `GGML_VK_VISIBLE_DEVICES` (a container with no device could not have
//! reached a card whatever the row set; a CPU row counts as no VRAM and
//! runs under the GPU hold). Those are dropped and the start's log names
//! them ([`ProcessEnv::dropped`]). Of `--device`, only a GPU node or a CDI
//! name selects; any other device (`/dev/snd`) is not applied. Every other
//! run arg means something to a container only; the start's log names them
//! ([`ProcessEnv::not_applied`]).

use crate::runtime::Placement;

/// What the native start sets in the engine's environment, and what of the
/// run args it does not apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessEnv {
    /// In order; a later name wins over an earlier one.
    pub vars: Vec<(String, String)>,
    /// The run args only a container reads, as written.
    pub not_applied: Vec<String>,
    /// The row's own `-e` settings of a variable that hides the cards, left
    /// out because no card is passed (module doc), as `NAME=VALUE`.
    pub dropped: Vec<String>,
}

/// The variables a no-card start sets to hide every backend.
const HIDING_VARS: [&str; 3] = [
    "CUDA_VISIBLE_DEVICES",
    "HIP_VISIBLE_DEVICES",
    "GGML_VK_VISIBLE_DEVICES",
];

/// A `--device` value that selects cards: a CDI name or a GPU node.
fn selects_cards(value: &str) -> bool {
    value.starts_with(lmgw_machine::cdi::NVIDIA_KIND)
        || lmgw_api_types::audio_engine::is_gpu_node(value.split(':').next().unwrap_or(value))
}

/// The card selection a native start applies (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    None,
    Nvidia(Vec<String>),
    All,
}

/// `run_args`' card selection for a model at `placement` (module doc).
pub fn selection(run_args: &[String], placement: Placement) -> Selection {
    if !placement.is_gpu() || !crate::runtime::audio::passes_gpu(run_args) {
        return Selection::None;
    }
    let mut names = Vec::new();
    let mut all = false;
    let mut it = run_args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = split_flag(a);
        let value = || {
            inline
                .map(str::to_string)
                .or_else(|| it.clone().next().cloned())
        };
        match flag {
            "--device" => {
                if let Some(v) = value() {
                    if let Some(name) = v.strip_prefix(lmgw_machine::cdi::NVIDIA_KIND) {
                        if let Some(name) = name.strip_prefix('=') {
                            all |= name == "all";
                            names.extend(name.split(',').map(str::to_string));
                        }
                    } else if selects_cards(&v) {
                        // A GPU node (`/dev/dri`, `/dev/kfd`, `/dev/nvidia0`):
                        // nothing narrower than every card is known of it.
                        all = true;
                    }
                }
            }
            "--gpus" => match value().as_deref().map(|v| v.trim_matches('"')) {
                Some(v) if v.starts_with("device=") => names.extend(
                    v.trim_start_matches("device=")
                        .split(',')
                        .map(str::to_string),
                ),
                _ => all = true,
            },
            _ => {}
        }
    }
    if all || names.is_empty() {
        Selection::All
    } else {
        Selection::Nvidia(names)
    }
}

/// The environment and the unapplied run args for a native start of a model
/// with `run_args` at `placement` (module doc).
pub fn process_env(run_args: &[String], placement: Placement) -> ProcessEnv {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: &str| vars.push((k.to_string(), v.to_string()));
    let hide = selection(run_args, placement) == Selection::None;
    match selection(run_args, placement) {
        Selection::None => {
            set("CUDA_VISIBLE_DEVICES", "-1");
            set("HIP_VISIBLE_DEVICES", "-1");
            set("GGML_VK_VISIBLE_DEVICES", "");
        }
        Selection::Nvidia(names) => {
            set("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
            set("CUDA_VISIBLE_DEVICES", &names.join(","));
        }
        Selection::All => {}
    }
    let mut not_applied = Vec::new();
    let mut dropped = Vec::new();
    let mut it = run_args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = split_flag(a);
        match flag {
            "-e" | "--env" => {
                let value = match inline {
                    Some(v) => Some(v.to_string()),
                    None => it.next().cloned(),
                };
                // `NAME` alone passes the host's value, which a child
                // inherits anyway.
                if let Some((k, v)) = value.as_deref().and_then(|v| v.split_once('=')) {
                    if hide && HIDING_VARS.contains(&k) {
                        dropped.push(format!("{k}={v}"));
                    } else {
                        vars.push((k.to_string(), v.to_string()));
                    }
                }
            }
            // Read above, through the selection.
            "--gpus" => {
                if inline.is_none() {
                    it.next();
                }
            }
            "--device" => {
                let value = match inline {
                    Some(v) => Some(v.to_string()),
                    None => it.next().cloned(),
                };
                if let Some(v) = value.filter(|v| !selects_cards(v)) {
                    not_applied.push(a.clone());
                    if inline.is_none() {
                        not_applied.push(v);
                    }
                }
            }
            _ => not_applied.push(a.clone()),
        }
    }
    ProcessEnv {
        vars,
        not_applied,
        dropped,
    }
}

/// `--flag=value` as `("--flag", Some("value"))`; anything else whole.
fn split_flag(a: &str) -> (&str, Option<&str>) {
    match a.split_once('=') {
        Some((f, v)) if f.starts_with("--") => (f, Some(v)),
        _ => (a, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn var<'a>(env: &'a ProcessEnv, k: &str) -> Option<&'a str> {
        env.vars
            .iter()
            .rev()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_class_default_shows_every_card_and_names_the_container_flags() {
        let args = v(&[
            "--device",
            "nvidia.com/gpu=all",
            "--security-opt",
            "label=disable",
        ]);
        assert_eq!(selection(&args, Placement::Gpu), Selection::All);
        let env = process_env(&args, Placement::Gpu);
        assert!(env.vars.is_empty(), "{env:?}");
        assert_eq!(env.not_applied, ["--security-opt", "label=disable"]);
    }

    #[test]
    fn named_cards_become_cuda_indices_in_bus_order() {
        let args = v(&["--device", "nvidia.com/gpu=1", "--device=nvidia.com/gpu=0"]);
        let env = process_env(&args, Placement::Gpu);
        assert_eq!(var(&env, "CUDA_VISIBLE_DEVICES"), Some("1,0"));
        assert_eq!(var(&env, "CUDA_DEVICE_ORDER"), Some("PCI_BUS_ID"));
        assert!(env.not_applied.is_empty(), "{env:?}");
        assert_eq!(
            selection(&v(&["--gpus", "device=2"]), Placement::Gpu),
            Selection::Nvidia(vec!["2".into()])
        );
    }

    #[test]
    fn no_card_hides_every_backend_and_the_rows_own_env_wins() {
        for (args, placement) in [
            (v(&["--device", "nvidia.com/gpu=all"]), Placement::Cpu),
            (v(&["--cpus", "8"]), Placement::Gpu),
        ] {
            let env = process_env(&args, placement);
            assert_eq!(var(&env, "CUDA_VISIBLE_DEVICES"), Some("-1"), "{args:?}");
            assert_eq!(var(&env, "HIP_VISIBLE_DEVICES"), Some("-1"));
            assert_eq!(var(&env, "GGML_VK_VISIBLE_DEVICES"), Some(""));
        }
        // The hiding wins over the row's own device variables on the CPU.
        let env = process_env(
            &v(&["-e", "CUDA_VISIBLE_DEVICES=0", "--env=X=1"]),
            Placement::Cpu,
        );
        assert_eq!(var(&env, "CUDA_VISIBLE_DEVICES"), Some("-1"));
        assert_eq!(var(&env, "X"), Some("1"));
        assert_eq!(env.dropped, ["CUDA_VISIBLE_DEVICES=0"]);
        // With a card passed, the row's value still wins.
        let env = process_env(
            &v(&[
                "--device",
                "nvidia.com/gpu=all",
                "-e",
                "CUDA_VISIBLE_DEVICES=0",
            ]),
            Placement::Gpu,
        );
        assert_eq!(var(&env, "CUDA_VISIBLE_DEVICES"), Some("0"));
        assert!(env.dropped.is_empty());
    }

    #[test]
    fn only_a_gpu_device_selects_a_card() {
        let args = v(&["--device", "nvidia.com/gpu=1", "--device", "/dev/snd"]);
        assert_eq!(
            selection(&args, Placement::Gpu),
            Selection::Nvidia(vec!["1".into()])
        );
        let env = process_env(&args, Placement::Gpu);
        assert_eq!(env.not_applied, ["--device", "/dev/snd"]);
        assert_eq!(var(&env, "CUDA_VISIBLE_DEVICES"), Some("1"));
        let args = v(&["--device=nvidia.com/gpu=0", "--device=/dev/fuse"]);
        assert_eq!(
            process_env(&args, Placement::Gpu).not_applied,
            ["--device=/dev/fuse"]
        );
    }

    #[test]
    fn a_device_node_is_every_card() {
        let args = v(&["--device", "/dev/kfd", "--device", "/dev/dri"]);
        assert_eq!(selection(&args, Placement::Gpu), Selection::All);
    }
}
