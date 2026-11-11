//! A native start: `prepare` resolves the engine and the environment, each
//! `launch` spawns one attempt.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use super::process::Proc;
use super::ring::Ring;
use super::{engines, gpu, limits, Native, Resolved};
use crate::runtime::argv::{engine_argv, EngineArgs, EngineView, RenderSpec};
use crate::runtime::engine_assets::pins::Variant;
use crate::runtime::launcher::types::{Launch, LaunchFailure, Launched, Prepare, Prepared};
use crate::runtime::registry::STOP_GRACE_SECONDS;

/// The directory beside a model's container `server.json` that holds the
/// one a native audio.cpp reads (host paths, the host port). The container's
/// stays where it is: the stale check compares the row with it.
pub const NATIVE_CONFIG_DIR: &str = "native";

impl Native {
    pub(super) async fn resolve(&self, p: Prepare<'_>) -> Result<Prepared, String> {
        // A CUDA build that ended an earlier run here without a kernel image
        // is marked before the choice reads the marks.
        self.mark_failed_cuda_builds();
        let install = engines::resolve(&self.engines, p.class, p.watch).await?;
        // From here on only the run args without the limits are read.
        let (limits, run_args) = limits::take(p.run_args);
        if !limits.is_empty() {
            tracing::info!(
                "a native {} process gets the run args' limits {} (a job object of its own)",
                engines::executable_stem(p.class),
                limits.describe()
            );
        }
        let mut env = gpu::process_env(&run_args, p.placement);
        if !env.not_applied.is_empty() {
            tracing::info!(
                "run args that only a container reads are not applied to a native {} process: {}",
                engines::executable_stem(p.class),
                env.not_applied.join(" ")
            );
        }
        if !env.dropped.is_empty() {
            tracing::info!(
                "a native {} process is started with no card (the row is on the CPU, or its run \
                 args pass none), so its own {} are left out: the variables that hide the cards win",
                engines::executable_stem(p.class),
                env.dropped.join(" ")
            );
        }
        let mut warnings = Vec::new();
        if p.class == crate::runtime::Class::Audio {
            // The phonemizing audio.cpp families (Kokoro, Piper, SanoTTS, …)
            // load eSpeak NG by name and read its data from
            // `ESPEAK_DATA_PATH`; the row's own `-e` still wins. lmgw has no
            // per-family "phonemizes" fact yet, so a failed install is a
            // warning for every audio row and the start goes on without it.
            match self.engines.ensure_espeak(p.watch).await {
                Ok(Some(home)) => {
                    env.vars.splice(0..0, espeak_env(&home));
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!(
                    "eSpeak NG could not be installed ({e}), so this audio.cpp process starts \
                     without it (no ESPEAK_DATA_PATH): a voice that phonemizes (Kokoro, Piper, …) \
                     fails at its first request, the others are not affected"
                )),
            }
        }
        let selection = gpu::selection(&run_args, p.placement);
        if let gpu::Selection::Nvidia(cards) = &selection {
            if install.variant == Some(Variant::Vulkan) {
                warnings.push(format!(
                    "the run args select NVIDIA card {} (--device nvidia.com/gpu=…), but the card \
                     selection does not reach the Vulkan build yet: the native {} process ({}) \
                     sees every card",
                    cards.join(", "),
                    engines::executable_stem(p.class),
                    install.build
                ));
            }
        }
        let gpu_row_without_card = p.placement.is_gpu() && selection == gpu::Selection::None;
        Ok(Prepared {
            native: Some(Arc::new(Resolved {
                class: p.class,
                install,
                env: env.vars,
                budget: p.budget,
                gpu_row_without_card,
                warnings,
                limits,
            })),
            ..Prepared::default()
        })
    }

    pub(super) async fn start(&self, launch: Launch<'_>) -> Result<Launched, LaunchFailure> {
        let failed = |message: String| LaunchFailure {
            message,
            port_in_use: false,
        };
        let render = launch.render;
        let name = render.container_name.as_str();
        let resolved = launch.prepared.native.as_ref().ok_or_else(|| {
            failed("this start was not prepared by the native launcher".to_string())
        })?;
        // What runs under the name goes first, as `--replace` does.
        self.kill_and_forget(name).await.map_err(failed)?;

        let view = host_view(render, resolved.install.variant).map_err(failed)?;
        let argv = engine_argv(render, &view).map_err(failed)?;
        let exe = resolved
            .install
            .exe
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut cmd = tokio::process::Command::new(&resolved.install.exe);
        cmd.args(&argv)
            .current_dir(&resolved.install.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A supervisor that is torn down (its runtime shut down) takes
            // the engine with it.
            .kill_on_drop(true);
        let removed = set_env(
            cmd.as_std_mut(),
            &resolved.env,
            std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()),
        );
        if !removed.is_empty() {
            tracing::info!(
                container = %name,
                "left {} out of the environment of the native {exe} process: llama-server reads \
                 LLAMA_ARG_* for every flag its argv does not set, so an inherited one would \
                 change this start unseen",
                removed.join(", ")
            );
        }
        crate::child::stdio_only(cmd.as_std_mut());
        #[cfg(unix)]
        super::unix::configure(&mut cmd);
        #[cfg(windows)]
        super::windows::configure(&mut cmd);

        let child = self.spawner.spawn(cmd).await.map_err(|e| {
            failed(format!(
                "{} could not be started: {e}",
                resolved.install.exe.display()
            ))
        })?;
        #[cfg(windows)]
        if let Err(e) = self.job.assign(&child, &resolved.limits) {
            let mut child = child;
            let _ = child.start_kill();
            return Err(failed(format!("{exe} was killed before it ran: {e}")));
        }
        let ring = Ring::new(name);
        let proc = Proc::supervise(
            name,
            exe.clone(),
            render.host_port,
            child,
            ring,
            Duration::from_secs(STOP_GRACE_SECONDS),
        )
        .ok_or_else(|| failed(format!("{exe} exited before its process id was read")))?;
        let pid = proc.pid;
        // The launch record first: a stop that finds the child in the table
        // finds its record too, so its `stop_asked` is never lost.
        self.remember_launch(name, pid, resolved.clone());
        self.table().insert(name.to_string(), proc);
        tracing::info!(
            container = %name,
            "started {} (build {}{}) as a native process, pid {pid}, on 127.0.0.1:{}; it has the \
             load timeout ({:?}, {}) to answer{}",
            exe,
            resolved.install.build,
            resolved
                .install
                .why
                .as_deref()
                .map(|w| format!(", {w}"))
                .unwrap_or_default(),
            render.host_port,
            resolved.budget,
            crate::config::LOAD_TIMEOUT_LABEL,
            if resolved.install.jit {
                ", which this first start may spend compiling its kernels from PTX (minutes on an \
                 older card; later starts read the driver's cache)"
            } else {
                ""
            }
        );
        let mut warnings = Vec::new();
        for w in &resolved.warnings {
            tracing::warn!(container = %name, "{w}");
            warnings.push(w.clone());
        }
        if resolved.gpu_row_without_card {
            let msg = format!(
                "{name} is placed on the GPU, but its run args pass no GPU, so the native {exe} \
                 process runs on the CPU (every GPU backend is hidden from it)"
            );
            tracing::warn!(container = %name, "{msg}");
            warnings.push(msg);
        }
        Ok(Launched { warnings })
    }
}

/// The child's environment on top of lmgw's: no `LD_LIBRARY_PATH` (the
/// engine resolves its libraries from its own directory — llama.cpp's Linux
/// builds: RUNPATH `$ORIGIN` — never from what a `cargo run` or an AppImage
/// put on lmgw's loader path) and none of `inherited`'s `LLAMA_ARG_*`
/// (llama-server reads one for every flag its argv does not set); then the
/// row's `env`, which wins over both. Returns the inherited `LLAMA_ARG_*`
/// names removed.
fn set_env(
    cmd: &mut std::process::Command,
    env: &[(String, String)],
    inherited: impl Iterator<Item = String>,
) -> Vec<String> {
    cmd.env_remove("LD_LIBRARY_PATH");
    let mut removed: Vec<String> = inherited
        .filter(|k| k.to_ascii_uppercase().starts_with("LLAMA_ARG_"))
        .filter(|k| !env.iter().any(|(r, _)| r.eq_ignore_ascii_case(k)))
        .collect();
    removed.sort();
    removed.dedup();
    for k in &removed {
        cmd.env_remove(k);
    }
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    removed
}

/// The variables that let audio.cpp find eSpeak NG in `home`: the
/// directory first on `PATH` (it loads `libespeak-ng.dll` by name) and as
/// `ESPEAK_DATA_PATH` (the parent of `espeak-ng-data`, which eSpeak reads
/// when it is given no data path).
fn espeak_env(home: &std::path::Path) -> Vec<(String, String)> {
    let mut path = vec![home.to_path_buf()];
    if let Some(inherited) = std::env::var_os("PATH") {
        path.extend(std::env::split_paths(&inherited));
    }
    let path = std::env::join_paths(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| home.display().to_string());
    vec![
        ("PATH".to_string(), path),
        ("ESPEAK_DATA_PATH".to_string(), home.display().to_string()),
    ]
}

/// The `server.json` backend a pinned build computes with: a row on the
/// GPU gets the build's own backend (a CUDA class setting on a machine whose
/// audio.cpp is the Vulkan build runs Vulkan); a row on the CPU stays there.
pub(super) fn native_backend(configured: &str, variant: Option<Variant>) -> String {
    match variant {
        Some(_) if configured.trim() == "cpu" => "cpu".into(),
        Some(Variant::Cuda13 | Variant::Cuda12) => "cuda".into(),
        Some(Variant::Vulkan) => "vulkan".into(),
        Some(Variant::Cpu) => "cpu".into(),
        None => configured.to_string(),
    }
}

/// The engine's view of the host: the host's models dir, the port on the
/// host's loopback, and for audio.cpp its `server.json` rendered in that
/// view into [`NATIVE_CONFIG_DIR`], with the backend of the build that runs
/// it ([`native_backend`]).
fn host_view(render: &RenderSpec, variant: Option<Variant>) -> Result<EngineView, String> {
    let config_dir = render
        .config_mount
        .as_ref()
        .map(|dir| dir.join(NATIVE_CONFIG_DIR));
    let view = EngineView::host(
        &render.models_dir,
        &config_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        render.host_port,
    );
    if let EngineArgs::Audio(audio) = &render.engine {
        let dir = config_dir.ok_or("an audio model's render names no config directory")?;
        let mut settings = audio.settings.clone();
        let backend = native_backend(&settings.backend, variant);
        if backend != settings.backend {
            tracing::info!(
                container = %render.container_name,
                "audio.cpp's server.json backend is {backend} rather than the configured {} \
                 (the build this machine runs)",
                settings.backend
            );
        }
        settings.backend = backend;
        let json =
            crate::runtime::audio::render_single_model_config_in(&settings, &audio.model, &view);
        crate::runtime::audio::write_config(&dir, &json).map_err(|e| {
            format!(
                "writing the native server.json into {} failed: {e}",
                dir.display()
            )
        })?;
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::{native_backend, set_env, Variant};
    use std::ffi::OsStr;

    fn get<'a>(cmd: &'a std::process::Command, k: &str) -> Option<Option<&'a OsStr>> {
        cmd.get_envs()
            .find(|(n, _)| *n == OsStr::new(k))
            .map(|(_, v)| v)
    }

    #[test]
    fn the_rows_env_wins_and_stray_llama_args_are_removed() {
        let mut cmd = std::process::Command::new("x");
        let row = vec![
            ("LD_LIBRARY_PATH".to_string(), "/row/lib".to_string()),
            ("LLAMA_ARG_THREADS".to_string(), "3".to_string()),
        ];
        let inherited = ["LLAMA_ARG_N_PARALLEL", "LLAMA_ARG_THREADS", "PATH"]
            .into_iter()
            .map(String::from);
        let removed = set_env(&mut cmd, &row, inherited);
        assert_eq!(removed, ["LLAMA_ARG_N_PARALLEL"]);
        assert_eq!(
            get(&cmd, "LD_LIBRARY_PATH"),
            Some(Some(OsStr::new("/row/lib")))
        );
        assert_eq!(get(&cmd, "LLAMA_ARG_THREADS"), Some(Some(OsStr::new("3"))));
        assert_eq!(get(&cmd, "LLAMA_ARG_N_PARALLEL"), Some(None));
        assert_eq!(get(&cmd, "PATH"), None);

        let mut cmd = std::process::Command::new("x");
        set_env(&mut cmd, &[], std::iter::empty());
        assert_eq!(get(&cmd, "LD_LIBRARY_PATH"), Some(None));
    }

    #[test]
    fn audio_runs_the_backend_of_its_build_and_a_cpu_row_stays_on_the_cpu() {
        assert_eq!(native_backend("cuda", Some(Variant::Vulkan)), "vulkan");
        assert_eq!(native_backend("vulkan", Some(Variant::Cuda12)), "cuda");
        assert_eq!(native_backend("cpu", Some(Variant::Vulkan)), "cpu");
        assert_eq!(native_backend("cuda", Some(Variant::Cpu)), "cpu");
        assert_eq!(native_backend("cuda", None), "cuda");
    }
}
