//! The per-row CPU switch's shared rule: which `podman run` args hand a
//! container a GPU. The gateway strips them from the class args a row on the
//! CPU inherits; the audio editor shows the args such a row would run with,
//! and both warn when a CPU row's own args still pass one.

/// The env a row on the CPU runs with on top of its stripped class args.
/// The legacy nvidia-container-toolkit OCI hook acts on
/// `NVIDIA_VISIBLE_DEVICES`, and the CUDA images set it to `all` in their
/// own env — so on a host with that hook installed, a container gets the GPU
/// with no flag asking for it. `void` exposes neither devices nor driver
/// libraries; where there is no such hook (CDI only) the variable is read by
/// nothing.
pub const NO_GPU_ENV: [&str; 2] = ["-e", "NVIDIA_VISIBLE_DEVICES=void"];

/// `args` without the flags that hand a container a GPU:
/// - `--device X` / `--device=X` where `X` is a CDI GPU
///   (`nvidia.com/gpu=all`) or a GPU node (`/dev/nvidia*`, `/dev/dri*`,
///   `/dev/kfd`, optionally with `:container-path:perms`);
/// - `--gpus X` / `--gpus=X`;
/// - `-v`/`--volume` with a GPU node as its source, and `--mount` with one
///   as `source=`/`src=`;
/// - `-e`/`--env NVIDIA_VISIBLE_DEVICES[=…]` naming devices (what the
///   legacy hook reads);
/// - `--runtime` naming the NVIDIA runtime;
/// - `--annotation` whose value is a CDI GPU (`cdi.k8s.io/x=nvidia.com/gpu=all`).
///
/// Every other flag stays, another `--device` included. A `--hooks-dir` is
/// left alone: it may carry other hooks, and [`NO_GPU_ENV`] is what keeps
/// the NVIDIA one from acting.
pub fn strip_gpu_devices(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        let names_gpu: Option<fn(&str) -> bool> = match flag {
            "--gpus" => Some(|_| true),
            "--device" => Some(is_gpu_device),
            "-v" | "--volume" => Some(is_gpu_volume),
            "--mount" => Some(is_gpu_mount),
            "-e" | "--env" => Some(is_gpu_env),
            "--runtime" => Some(is_gpu_runtime),
            "--annotation" => Some(is_cdi_annotation),
            _ => None,
        };
        let gpu = names_gpu.is_some_and(|names| match inline {
            Some(v) => names(v),
            None => it.peek().is_some_and(|v| names(v)),
        });
        if !gpu {
            out.push(a.clone());
            continue;
        }
        if inline.is_none() {
            // The value is the next arg.
            it.next();
        }
    }
    out
}

/// The class args a row on the CPU inherits: [`strip_gpu_devices`], then
/// [`NO_GPU_ENV`].
pub fn cpu_run_args(class_args: &[String]) -> Vec<String> {
    let mut out = strip_gpu_devices(class_args);
    out.extend(NO_GPU_ENV.map(String::from));
    out
}

/// `args` hand the container a GPU in one of [`strip_gpu_devices`]'s forms
/// — said of a CPU row's own args, which are used verbatim.
pub fn passes_gpu(args: &[String]) -> bool {
    strip_gpu_devices(args).len() != args.len()
}

/// A host path that is a GPU node.
fn is_gpu_node(path: &str) -> bool {
    path.starts_with("/dev/nvidia") || path.starts_with("/dev/dri") || path == "/dev/kfd"
}

/// A `--device` value that names a GPU (see [`strip_gpu_devices`]).
fn is_gpu_device(value: &str) -> bool {
    let host = value.split(':').next().unwrap_or(value);
    host.contains("/gpu=") || is_gpu_node(host)
}

/// A `-v`/`--volume` value whose source is a GPU node.
fn is_gpu_volume(value: &str) -> bool {
    is_gpu_node(value.split(':').next().unwrap_or(value))
}

/// A `--mount` value whose source is a GPU node.
fn is_gpu_mount(value: &str) -> bool {
    value.split(',').any(|kv| {
        kv.split_once('=')
            .is_some_and(|(k, v)| matches!(k, "source" | "src") && is_gpu_node(v))
    })
}

/// An env setting `NVIDIA_VISIBLE_DEVICES` to devices, or passing the
/// host's value through. `void`, `none` and empty expose no device, so they
/// stay.
fn is_gpu_env(value: &str) -> bool {
    match value.split_once('=') {
        Some((k, v)) => k == "NVIDIA_VISIBLE_DEVICES" && !matches!(v, "void" | "none" | ""),
        None => value == "NVIDIA_VISIBLE_DEVICES",
    }
}

/// A `--runtime` naming the NVIDIA container runtime.
fn is_gpu_runtime(value: &str) -> bool {
    value.contains("nvidia")
}

/// An `--annotation key=value` whose value is a CDI GPU.
fn is_cdi_annotation(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(_, v)| v.contains("/gpu="))
}
