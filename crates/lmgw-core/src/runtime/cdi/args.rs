//! Which CDI devices a podman argv asks for (machine-check design §3.6.3).
//!
//! `--device <kind>=<name>` (or `--device=…`) names a CDI device when its
//! value is a qualified name — `vendor/class=name`, as opposed to a host path
//! like `/dev/dri` or `/dev/kfd:/dev/kfd:rwm`. `--gpus <which>` is Podman's
//! shorthand for `nvidia.com/gpu=<which>`.

use lmgw_machine::cdi::NVIDIA_KIND;

/// What a podman argv asks of CDI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CdiRequest {
    /// It names an `nvidia.com/gpu` device: lmgw's own spec has to be fresh.
    pub nvidia: bool,
    /// The other kinds it names, each once, in order: only the system's
    /// specs define them.
    pub other_kinds: Vec<String>,
}

/// The kind of a qualified CDI name (`nvidia.com/gpu=all` →
/// `nvidia.com/gpu`); `None` for a host path or anything else.
pub fn cdi_kind(device: &str) -> Option<&str> {
    if device.starts_with('/') {
        return None;
    }
    let (kind, name) = device.split_once('=')?;
    (kind.contains('/') && !name.is_empty()).then_some(kind)
}

/// What `args` ask of CDI.
pub fn requested(args: &[String]) -> CdiRequest {
    let mut req = CdiRequest::default();
    let mut note = |kind: &str| {
        if kind == NVIDIA_KIND {
            req.nvidia = true;
        } else if !req.other_kinds.iter().any(|k| k == kind) {
            req.other_kinds.push(kind.to_string());
        }
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        match flag {
            "--device" => {
                let value = inline.map(str::to_string).or_else(|| it.next().cloned());
                if let Some(kind) = value.as_deref().and_then(cdi_kind) {
                    note(kind);
                }
            }
            "--gpus" => {
                if inline.is_none() {
                    it.next();
                }
                note(NVIDIA_KIND);
            }
            _ => {}
        }
    }
    req
}

/// Long flags of `podman run` / `create` that take no separate value.
/// Anything not listed here is read as taking one, which errs toward reading
/// past the image and refusing more, never toward missing a device.
const BOOLEAN_FLAGS: &[&str] = &[
    "--detach",
    "--disable-content-trust",
    "--env-host",
    "--help",
    "--http-proxy",
    "--init",
    "--interactive",
    "--no-healthcheck",
    "--no-hosts",
    "--oom-kill-disable",
    "--passwd",
    "--privileged",
    "--publish-all",
    "--quiet",
    "--read-only",
    "--read-only-tmpfs",
    "--replace",
    "--rm",
    "--rmi",
    "--rootfs",
    "--sig-proxy",
    "--tls-verify",
    "--tty",
    "--unsetenv-all",
];

/// Short flags of `podman run` / `create` that take no value.
const BOOLEAN_SHORT: &str = "dihPqt";

/// The part of a podman argv in which a device can be named: what stands
/// between the verb and the image of a `run` or `create` (`container run`
/// too), the options of a `pod create`. What follows the image is the
/// container's own command, whatever it says (`--gpus` included). `None`
/// for a verb that cannot give a container a device. The options before the
/// verb (the globals) are skipped.
pub fn container_options(argv: &[String]) -> Option<&[String]> {
    let mut i = 0;
    while argv.get(i).is_some_and(|a| a.starts_with('-')) {
        i += if argv[i].contains('=') { 1 } else { 2 };
    }
    let verb = argv.get(i)?.as_str();
    i += 1;
    let verb = if matches!(verb, "container" | "pod") {
        let sub = argv.get(i)?.as_str();
        i += 1;
        match (verb, sub) {
            ("pod", "create") => return argv.get(i..),
            ("container", sub) => sub,
            _ => return None,
        }
    } else {
        verb
    };
    if !matches!(verb, "run" | "create") {
        return None;
    }
    let start = i;
    while let Some(tok) = argv.get(i) {
        if tok == "--" || tok == "-" || !tok.starts_with('-') {
            break;
        }
        i += if let Some(long) = tok.strip_prefix("--") {
            let flag = format!("--{}", long.split('=').next().unwrap_or(long));
            if tok.contains('=') || BOOLEAN_FLAGS.contains(&flag.as_str()) {
                1
            } else {
                2
            }
        } else {
            // A cluster of short flags: the first one that takes a value
            // ends it, its value attached or the next token.
            let letters = &tok[1..];
            match letters.find(|c| !BOOLEAN_SHORT.contains(c)) {
                None => 1,
                Some(at) if at + 1 < letters.len() => 1,
                Some(_) => 2,
            }
        };
    }
    argv.get(start..i.min(argv.len()))
}

/// Whether `policy` is no restart policy: `no`, or `never`, which Podman
/// takes for it.
fn is_no_policy(policy: &str) -> bool {
    matches!(policy, "no" | "never")
}

/// `args` without a restart policy when they give the container an NVIDIA
/// GPU and the call carries lmgw's own spec dir (`globals` holds
/// [`super::SPEC_DIR_OPTION`]), and the tokens taken out (D27). Podman
/// restarts such a container itself, from conmon, which passes the hooks dir
/// and no `--cdi-spec-dir` (`pkg/specgenutil/util.go`): the restarted
/// container would get the system's spec instead of lmgw's. lmgw starts a
/// model whose container ended again on its next request, so the policy adds
/// nothing such a row needs. Without lmgw's spec dir (Podman's default dirs,
/// or no managed spec) the container reads the system's spec on every start
/// anyway, so a restart policy changes nothing and stays. `--restart no`
/// (`never`, and the `=` forms) is no policy and stays. Args that name no
/// NVIDIA device come back whole.
pub fn strip_gpu_restart(args: &[String], globals: &[String]) -> (Vec<String>, Vec<String>) {
    let own_dir = globals
        .iter()
        .any(|g| g == super::SPEC_DIR_OPTION || g.starts_with("--cdi-spec-dir="));
    if !own_dir || !requested(args).nvidia {
        return (args.to_vec(), Vec::new());
    }
    let (mut kept, mut removed) = (Vec::new(), Vec::new());
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--restart" {
            match it.peek() {
                Some(v) if is_no_policy(v) => kept.push(a.clone()),
                Some(_) => {
                    removed.push(a.clone());
                    removed.extend(it.next().cloned());
                }
                None => removed.push(a.clone()),
            }
            continue;
        }
        if let Some(policy) = a.strip_prefix("--restart=") {
            if !is_no_policy(policy) {
                removed.push(a.clone());
                continue;
            }
        }
        kept.push(a.clone());
    }
    (kept, removed)
}

/// The note a start (and a row's check) gives when [`strip_gpu_restart`]
/// took `removed` out.
pub fn restart_note(removed: &[String]) -> String {
    format!(
        "`{}` is left out of this model's container: Podman restarts a container on its own \
         without the description of the graphics card that lmgw points it at and would give it \
         the system's, which can be out of date. lmgw starts the model again on its next \
         request instead.",
        removed.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_policy_goes_from_gpu_args_only() {
        let own = args(&["--cdi-spec-dir", "/run/user/1000/lmgw-machine/lmgw/cdi"]);
        let (kept, removed) = strip_gpu_restart(
            &args(&[
                "--device",
                "nvidia.com/gpu=all",
                "--restart",
                "always",
                "--restart=on-failure:3",
                "--security-opt",
                "label=disable",
            ]),
            &own,
        );
        assert_eq!(
            kept,
            args(&[
                "--device",
                "nvidia.com/gpu=all",
                "--security-opt",
                "label=disable"
            ])
        );
        assert_eq!(
            removed,
            args(&["--restart", "always", "--restart=on-failure:3"])
        );
        assert!(restart_note(&removed).starts_with("`--restart always --restart=on-failure:3`"));

        let no = args(&[
            "--gpus",
            "all",
            "--restart",
            "no",
            "--restart=no",
            "--restart",
            "never",
            "--restart=never",
        ]);
        assert_eq!(strip_gpu_restart(&no, &own), (no.clone(), vec![]));
        let cpu = args(&["--cpus", "4", "--restart", "always"]);
        assert_eq!(strip_gpu_restart(&cpu, &own), (cpu.clone(), vec![]));
        // Podman's default dirs (or no managed spec): the policy stays.
        let gpu = args(&["--gpus", "all", "--restart", "always"]);
        assert_eq!(strip_gpu_restart(&gpu, &[]), (gpu.clone(), vec![]));
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_nvidia_defaults_ask_for_lmgws_spec_only() {
        let req = requested(&args(&[
            "--device",
            "nvidia.com/gpu=all",
            "--security-opt",
            "label=disable",
        ]));
        assert_eq!(
            req,
            CdiRequest {
                nvidia: true,
                other_kinds: vec![]
            }
        );
        assert!(requested(&args(&["--device=nvidia.com/gpu=0"])).nvidia);
        assert!(requested(&args(&["--gpus", "all"])).nvidia);
        assert!(requested(&args(&["--gpus=all"])).nvidia);
    }

    #[test]
    fn host_device_paths_and_cpu_args_ask_for_nothing() {
        for a in [
            args(&["--device", "/dev/dri", "--device", "/dev/kfd:/dev/kfd:rwm"]),
            args(&["-e", "CUDA_VISIBLE_DEVICES=", "--cpus", "8"]),
            args(&[]),
            // A label value with `=` and `/` in it is not a device.
            args(&["--label", "a/b=c"]),
        ] {
            assert_eq!(requested(&a), CdiRequest::default(), "{a:?}");
        }
    }

    #[test]
    fn another_kind_is_named_once_beside_nvidia() {
        let req = requested(&args(&[
            "--device",
            "vendor.example/fpga=0",
            "--device=vendor.example/fpga=1",
            "--device",
            "nvidia.com/gpu=all",
        ]));
        assert!(req.nvidia);
        assert_eq!(req.other_kinds, vec!["vendor.example/fpga"]);
    }

    #[test]
    fn the_options_end_at_the_image() {
        let run = args(&[
            "--cdi-spec-dir",
            "/d",
            "run",
            "-d",
            "--rm",
            "--device",
            "nvidia.com/gpu=all",
            "-p",
            "127.0.0.1:1:2",
            "-it",
            "img",
            "serve",
            "--gpus",
            "all",
        ]);
        let opts = container_options(&run).unwrap();
        assert_eq!(opts.last().unwrap(), "-it");
        assert!(requested(opts).nvidia);
        let own = args(&["run", "--cpus", "4", "img", "--gpus", "all"]);
        assert!(!requested(container_options(&own).unwrap()).nvidia);
        assert!(container_options(&args(&["rm", "-f", "x"])).is_none());
        assert!(container_options(&args(&["exec", "x", "--gpus", "all"])).is_none());
        let cc = args(&["container", "create", "-e", "A=1", "--gpus=all", "img"]);
        assert!(requested(container_options(&cc).unwrap()).nvidia);
        let attached = args(&["run", "-eA=1", "--name=n", "img", "--gpus", "all"]);
        assert!(!requested(container_options(&attached).unwrap()).nvidia);
    }

    #[test]
    fn a_trailing_device_flag_without_a_value_is_ignored() {
        assert_eq!(requested(&args(&["--device"])), CdiRequest::default());
    }
}
