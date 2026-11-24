//! The `podman run` wrapper around a model's engine argv: the Podman
//! launcher's half of the render ([`crate::runtime::launcher::Podman`]).
//! Everything after the image name is the shared engine half
//! ([`super::render_engine_args`], in the container's [`EngineView`]).

use super::{render_engine_args, EngineView, RenderSpec};

/// The label that records which `extra_run_args` a container was started
/// with, as [`run_args_digest`]. Boot adoption compares it with what the
/// model renders now: podman keeps a label exactly as written, where it
/// normalizes the flags themselves, so the flags cannot be compared as text.
pub const RUN_ARGS_LABEL: &str = "lmgw.run_args";

/// The label naming the lmgw that started a container: its data dir's
/// instance id (`backends::run::load_instance_id`). Two instances can share a
/// `container_prefix` — every dev copy is `lmgw-dev` — and so the instance
/// label; the reconciliation pass after boot (`registry/unheld.rs`) acts only
/// on containers carrying its own owner, never on another instance's.
pub const OWNER_LABEL: &str = "lmgw.owner";

/// `argv` (a [`podman_run_argv`]) with [`OWNER_LABEL`]`=<owner>` after its
/// other labels.
pub fn stamp_owner(mut argv: Vec<String>, owner: &str) -> Vec<String> {
    let at = argv
        .iter()
        .rposition(|a| a == "--label")
        .map_or(argv.len().min(1), |i| i + 2);
    argv.splice(
        at..at,
        ["--label".to_string(), format!("{OWNER_LABEL}={owner}")],
    );
    argv
}

/// `argv` (a [`podman_run_argv`]) with `--pull=never` right after its `run`
/// verb (after any of Podman's global options before it):
/// the registry's start of a model and a benchmark's container (machine-check
/// design §5). Each downloaded a missing image first — admission before the
/// gate, a benchmark before its GPU lease — so the start never fetches bytes
/// while it holds a reservation or the card; an image gone in between fails
/// the start with podman's own "image not known" instead of a download
/// nobody can see. Not in [`podman_run_argv`] itself, which the rendered
/// command line also uses: a line copied from it pulls what it needs.
///
/// # Panics
///
/// When `argv` has no `run` verb: every caller hands it a
/// [`podman_run_argv`], and inserting the flag anywhere else would make it
/// another command.
pub fn never_pull(mut argv: Vec<String>) -> Vec<String> {
    let at = argv
        .iter()
        .position(|a| a == "run")
        .unwrap_or_else(|| panic!("never_pull takes a podman run argv, with its verb: {argv:?}"));
    argv.insert(at + 1, "--pull=never".into());
    argv
}

/// `run_args` with the NVIDIA cards they pass replaced by `cards`, the cards
/// lmgw placed the model on (device-ledger design §7.1): every CDI NVIDIA
/// device (`--device nvidia.com/gpu=…`) and `--gpus` goes, and so do the
/// row's own `NVIDIA_VISIBLE_DEVICES`, `CUDA_VISIBLE_DEVICES` and
/// `CUDA_DEVICE_ORDER` (CUDA's own numbering differs from every other on
/// unlike cards, §8.1); then one `--device nvidia.com/gpu=<uuid>` per card,
/// `-e NVIDIA_VISIBLE_DEVICES=<uuid,…>` (so a host with NVIDIA's legacy
/// hook confines the container too) and `-e CUDA_DEVICE_ORDER=PCI_BUS_ID`
/// (the engine numbers the cards in the order lmgw knows them by). Every
/// other argument stays as written, a GPU node (`/dev/dri`) included.
pub fn hand_cards(run_args: &[String], cards: &[crate::vram::nvml::DeviceId]) -> Vec<String> {
    const ENV: [&str; 3] = [
        "NVIDIA_VISIBLE_DEVICES",
        "CUDA_VISIBLE_DEVICES",
        "CUDA_DEVICE_ORDER",
    ];
    let nvidia = |v: &str| v.starts_with(lmgw_machine::cdi::NVIDIA_KIND);
    let env = |v: &str| {
        let name = v.split_once('=').map_or(v, |(n, _)| n);
        ENV.contains(&name)
    };
    let mut out = Vec::with_capacity(run_args.len() + 2 * cards.len() + 4);
    let mut it = run_args.iter().peekable();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        let drops: Option<&dyn Fn(&str) -> bool> = match flag {
            "--device" => Some(&nvidia),
            "--gpus" => Some(&|_: &str| true),
            "-e" | "--env" => Some(&env),
            _ => None,
        };
        let drop = drops.is_some_and(|d| match inline {
            Some(v) => d(v),
            None => it.peek().is_some_and(|v| d(v)),
        });
        if !drop {
            out.push(a.clone());
            continue;
        }
        if inline.is_none() {
            it.next();
        }
    }
    let ids: Vec<&str> = cards.iter().map(|c| c.as_str()).collect();
    for id in &ids {
        out.push("--device".into());
        out.push(format!("{}={id}", lmgw_machine::cdi::NVIDIA_KIND));
    }
    out.extend([
        "-e".into(),
        format!("NVIDIA_VISIBLE_DEVICES={}", ids.join(",")),
        "-e".into(),
        "CUDA_DEVICE_ORDER=PCI_BUS_ID".into(),
    ]);
    out
}

/// `run_args` hand NVIDIA cards to a container through CDI or `--gpus`,
/// which lmgw can replace with the cards it chose (§7.1); a GPU node
/// (`/dev/dri`, `/dev/nvidia0`) cannot be.
pub fn passes_cdi(run_args: &[String]) -> bool {
    for (i, a) in run_args.iter().enumerate() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let value = || inline.clone().or_else(|| run_args.get(i + 1).cloned());
        match flag {
            "--gpus" => return true,
            "--device"
                if value().is_some_and(|v| v.starts_with(lmgw_machine::cdi::NVIDIA_KIND)) =>
            {
                return true
            }
            _ => {}
        }
    }
    false
}

/// A digest of `podman run` flags, for [`RUN_ARGS_LABEL`]: the first eight
/// bytes of the SHA-256 of their JSON list, in hex.
pub fn run_args_digest(args: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(args).unwrap_or_default();
    hex::encode(&Sha256::digest(json.as_bytes())[..8])
}

/// Render one model's full container command line as a single copyable
/// string: `podman run … <image> <engine args…>`, shell-quoted.
///
/// The honest answer to "what will actually be run", which is what the
/// deleted `preset_section` used to be for (§8: local_edit's "Rendered
/// preset" panel becomes "Rendered command line").
pub fn command_line(spec: &RenderSpec) -> String {
    std::iter::once("podman".to_string())
        .chain(podman_run_argv(spec))
        .map(|tok| {
            shlex::try_quote(&tok)
                .map(|q| q.into_owned())
                .unwrap_or(tok)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the full `podman run` argv for one model's container (§3.3 + §3.6 +
/// the prod template, §10.4). Pure; unit-tested.
///
/// No `--rm`: a stopped container keeps its logs for post-mortem, and
/// `--replace` collects it on the next start (§3.6). Everything after the
/// image name is [`render_engine_args`]'s output, handed straight to the
/// image's entrypoint. [`RenderSpec::globals`] come first, before `run`:
/// Podman takes a global option only there.
pub fn podman_run_argv(spec: &RenderSpec) -> Vec<String> {
    let mut args: Vec<String> = spec.globals.clone();
    args.extend(["run".into(), "-d".into(), "--replace".into()]);
    if spec.init {
        args.push("--init".into());
    }
    args.extend([
        "--name".into(),
        spec.container_name.clone(),
        "--label".into(),
        format!("lmgw.instance={}", spec.container_prefix),
        "--label".into(),
        format!("lmgw.class={}", spec.class.as_str()),
        "--label".into(),
        format!("lmgw.model={}", spec.model_id),
        "--label".into(),
        format!("lmgw.engine={}", spec.class.engine()),
        "--label".into(),
        format!("{RUN_ARGS_LABEL}={}", run_args_digest(&spec.extra_run_args)),
    ]);
    // A partial start's grant, so boot adopts the container it renders.
    if let super::EngineArgs::Image(img) = &spec.engine {
        if let Some(held) = &img.held {
            args.extend([
                "--label".into(),
                format!("{}={}", super::GRANT_LABEL, held.label()),
            ]);
        }
    }
    // A fitted llama-server start's placement, likewise (`llama_fit`).
    if let Some(f) = spec.fitted.filter(|f| !f.placement.is_whole()) {
        args.extend([
            "--label".into(),
            format!("{}={}", super::FITTED_LABEL, f.label()),
        ]);
    }
    // What admission charged it on the cards (device-ledger design §3.3).
    if let Some(label) = spec.charge.as_ref().and_then(|c| c.label()) {
        args.extend([
            "--label".into(),
            format!("{}={label}", crate::vram::charge::PLACEMENT_LABEL),
        ]);
    }
    // The cards admission placed it on, where lmgw hands the engine fewer
    // than its arguments pass (device-ledger design §7.1, D17), recorded so
    // boot adopts the command it renders for them (`engine_cards`); the
    // arguments as written everywhere else, and in the digest above.
    match spec.charge.as_ref().and_then(|c| c.handed_cards()) {
        Some(cards) => {
            let ids: Vec<&str> = cards.iter().map(|c| c.as_str()).collect();
            args.extend([
                "--label".into(),
                format!("{}={}", super::HANDED_LABEL, ids.join(",")),
            ]);
            // Handed for the row's own split, which it is started with as
            // written (device-ledger design §7.2).
            if spec.charge.as_ref().is_some_and(|c| c.keeps_own_split()) {
                args.extend(["--label".into(), format!("{}=kept", super::OWN_SPLIT_LABEL)]);
            }
            args.extend(hand_cards(&spec.extra_run_args, &cards))
        }
        None => args.extend(spec.extra_run_args.iter().cloned()),
    }
    // Loopback only: every client reaches a model through lmgw's own
    // routes, which carry its auth, policy and logging — a port published on
    // every interface would hand the LAN an unauthenticated llama-server.
    // The mount targets and the container port are the container's
    // [`EngineView`], the one the engine argv after the image is rendered in.
    let view = EngineView::container();
    args.extend([
        "-p".into(),
        format!("127.0.0.1:{}:{}", spec.host_port, view.listen_port),
        "-v".into(),
        format!(
            "{}:{}:ro",
            spec.models_dir.trim_end_matches('/'),
            view.models_dir
        ),
    ]);
    if let Some(cfg) = &spec.config_mount {
        args.push("-v".into());
        args.push(format!("{}:{}:ro", cfg.display(), view.config_dir));
    }
    // Last of the podman-side flags, immediately before the image: the
    // image's own ENTRYPOINT is what this replaces, and putting it anywhere
    // else in the line would still work but would read as if it belonged to
    // the mount above it.
    if let Some(bin) = &spec.entrypoint {
        args.push("--entrypoint".into());
        args.push(bin.clone());
    }
    args.push(spec.image.clone());
    args.extend(render_engine_args(spec));
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vram::nvml::DeviceId;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// The placement's cards by UUID in place of the NVIDIA devices the row
    /// passes, with the two variables; every other argument as written, a
    /// GPU node and another device included (device-ledger design §7.1).
    #[test]
    fn the_placed_cards_replace_the_nvidia_devices() {
        let row = args(&[
            "--device",
            "nvidia.com/gpu=all",
            "--shm-size=1g",
            "--device=/dev/snd",
            "-e",
            "CUDA_VISIBLE_DEVICES=1",
            "--env=NVIDIA_VISIBLE_DEVICES=all",
            "-e",
            "HF_HOME=/x",
            "--gpus",
            "all",
            "--device",
            "/dev/dri",
        ]);
        let cards = [DeviceId::new("GPU-b"), DeviceId::new("GPU-a")];
        assert_eq!(
            hand_cards(&row, &cards),
            args(&[
                "--shm-size=1g",
                "--device=/dev/snd",
                "-e",
                "HF_HOME=/x",
                "--device",
                "/dev/dri",
                "--device",
                "nvidia.com/gpu=GPU-b",
                "--device",
                "nvidia.com/gpu=GPU-a",
                "-e",
                "NVIDIA_VISIBLE_DEVICES=GPU-b,GPU-a",
                "-e",
                "CUDA_DEVICE_ORDER=PCI_BUS_ID",
            ])
        );
    }

    #[test]
    fn cdi_and_gpus_hand_out_a_node_does_not() {
        assert!(passes_cdi(&args(&["--device", "nvidia.com/gpu=all"])));
        assert!(passes_cdi(&args(&["--device=nvidia.com/gpu=1"])));
        assert!(passes_cdi(&args(&["--gpus", "all"])));
        assert!(!passes_cdi(&args(&["--device", "/dev/dri"])));
        assert!(!passes_cdi(&args(&["--device", "/dev/kfd"])));
        assert!(!passes_cdi(&[]));
    }
}
