//! The row's engine settings and run args (the per-row CPU switch).

use super::*;
use crate::host::CoreSource;

fn row() -> AudioModel {
    serde_json::from_value(serde_json::json!({
        "id": 1, "model_id": "parakeet", "family": "parakeet_tdt", "path": "p",
        "task": "asr", "mode": "offline", "load_options": {}, "session_options": {},
        "voice_presets": {}, "default_voice_preset": null, "enabled": true,
        "image": null, "extra_run_args": null, "warm_start": false
    }))
    .unwrap()
}

fn host(cores: usize) -> HostCpu {
    HostCpu {
        physical_cores: cores,
        logical_cpus: cores * 2,
        source: CoreSource::Topology,
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn class() -> AudioSettings {
    AudioSettings {
        threads: 4,
        ..AudioSettings::default()
    }
}

#[test]
fn a_row_that_sets_nothing_renders_the_class_settings_unchanged() {
    let (m, s) = (row(), class());
    let e = engine_settings(&m, &s, host(16));
    assert_eq!((e.backend.as_str(), e.threads), ("cuda", 4));
    assert_eq!(
        super::super::render_single_model_config(&e, &m),
        super::super::render_single_model_config(&s, &m),
        "byte-identical server.json"
    );
    assert_eq!(placement(&m, &s), Placement::Gpu);
    assert_eq!(threads_in_effect(&m, &s, host(16)).1, ThreadsSource::Class);
    assert_eq!(run_args(&m, &s), s.extra_run_args);
}

#[test]
fn a_row_on_the_cpu_uses_the_physical_cores_unless_it_names_a_count() {
    let (mut m, s) = (row(), class());
    m.backend = Some("cpu".into());
    let e = engine_settings(&m, &s, host(16));
    assert_eq!((e.backend.as_str(), e.threads), ("cpu", 16));
    assert_eq!(placement(&m, &s), Placement::Cpu);
    assert_eq!(
        threads_in_effect(&m, &s, host(16)),
        (16, ThreadsSource::Cores)
    );
    // Where the topology is unreadable the figure is the logical count, and
    // the source says so.
    let logical = HostCpu {
        physical_cores: 32,
        logical_cpus: 32,
        source: CoreSource::Logical,
    };
    let (n, source) = threads_in_effect(&m, &s, logical);
    assert_eq!((n, source.as_str()), (32, "logical_cpus"));
    m.threads = Some(8);
    assert_eq!(threads_in_effect(&m, &s, host(16)), (8, ThreadsSource::Row));
    assert_eq!(engine_settings(&m, &s, host(16)).threads, 8);
    // A GPU row's own count wins over the class's too.
    let mut gpu = row();
    gpu.threads = Some(2);
    assert_eq!(
        threads_in_effect(&gpu, &s, host(16)),
        (2, ThreadsSource::Row)
    );
}

#[test]
fn a_row_on_a_cpu_class_keeps_the_class_threads() {
    let m = row();
    let s = AudioSettings {
        backend: "cpu".into(),
        threads: 6,
        ..AudioSettings::default()
    };
    assert_eq!(placement(&m, &s), Placement::Cpu);
    assert_eq!(
        threads_in_effect(&m, &s, host(16)),
        (6, ThreadsSource::Class)
    );
    assert_eq!(
        super::super::render_single_model_config(&engine_settings(&m, &s, host(16)), &m),
        super::super::render_single_model_config(&s, &m)
    );
}

/// audio.cpp matches the backend word exactly, and so does the predicate:
/// a padded class value is not taken for the CPU.
#[test]
fn a_padded_class_backend_is_not_the_cpu() {
    let s = AudioSettings {
        backend: "cpu ".into(),
        ..AudioSettings::default()
    };
    assert_eq!(placement(&row(), &s), Placement::Gpu);
    let mut m = row();
    m.backend = Some(" cpu".into());
    assert_eq!(placement(&m, &AudioSettings::default()), Placement::Cpu);
    assert_eq!(engine_settings(&m, &s, host(16)).backend, "cpu");
}

#[test]
fn the_gpu_flags_are_stripped_and_everything_else_stays() {
    let stripped = strip_gpu_devices(&args(&[
        "--device",
        "nvidia.com/gpu=all",
        "--security-opt",
        "label=disable",
        "--device=amd.com/gpu=0",
        "--device",
        "/dev/dri/renderD128:/dev/dri/renderD128:rwm",
        "--device=/dev/kfd",
        "--device",
        "/dev/nvidia0",
        "--gpus",
        "all",
        "--gpus=1",
        "--device",
        "/dev/snd",
        "--cpuset-cpus",
        "0-7",
    ]));
    assert_eq!(
        stripped,
        args(&[
            "--security-opt",
            "label=disable",
            "--device",
            "/dev/snd",
            "--cpuset-cpus",
            "0-7"
        ])
    );
    // A trailing flag with no value is kept as it was.
    assert_eq!(strip_gpu_devices(&args(&["--device"])), args(&["--device"]));
}

/// The other ways a container is handed a GPU: a GPU node as a volume or a
/// mount, the NVIDIA runtime, a CDI annotation, and the env the legacy
/// NVIDIA hook reads — while a volume, a mount, an env and an annotation
/// that name no GPU stay, and so does an env that names no device.
#[test]
fn the_volume_runtime_annotation_and_env_forms_are_stripped_too() {
    let stripped = strip_gpu_devices(&args(&[
        "-v",
        "/dev/nvidia0:/dev/nvidia0",
        "--volume=/dev/nvidiactl:/dev/nvidiactl:rw",
        "-v",
        "/srv/models:/models:ro,Z",
        "--mount",
        "type=bind,source=/dev/nvidia-uvm,target=/dev/nvidia-uvm",
        "--mount=type=bind,src=/srv/x,dst=/x",
        "--runtime",
        "nvidia",
        "--runtime=/usr/bin/nvidia-container-runtime",
        "--runtime=crun",
        "--annotation",
        "cdi.k8s.io/gpu=nvidia.com/gpu=all",
        "--annotation=run.oci.keep_original_groups=1",
        "-e",
        "NVIDIA_VISIBLE_DEVICES=all",
        "--env=NVIDIA_VISIBLE_DEVICES=0",
        "--env",
        "NVIDIA_VISIBLE_DEVICES",
        "-e",
        "NVIDIA_VISIBLE_DEVICES=void",
        "-e",
        "TZ=Europe/Berlin",
        "--hooks-dir",
        "/usr/share/containers/oci/hooks.d",
    ]));
    assert_eq!(
        stripped,
        args(&[
            "-v",
            "/srv/models:/models:ro,Z",
            "--mount=type=bind,src=/srv/x,dst=/x",
            "--runtime=crun",
            "--annotation=run.oci.keep_original_groups=1",
            "-e",
            "NVIDIA_VISIBLE_DEVICES=void",
            "-e",
            "TZ=Europe/Berlin",
            "--hooks-dir",
            "/usr/share/containers/oci/hooks.d",
        ])
    );
    assert!(passes_gpu(&args(&["-e", "NVIDIA_VISIBLE_DEVICES=all"])));
    assert!(!passes_gpu(&args(&["-e", "NVIDIA_VISIBLE_DEVICES=void"])));
}

#[test]
fn a_cpu_row_inherits_the_class_args_without_the_gpu_and_keeps_its_own_verbatim() {
    let (mut m, s) = (row(), class());
    m.backend = Some("cpu".into());
    assert_eq!(
        run_args(&m, &s),
        args(&[
            "--security-opt",
            "label=disable",
            "-e",
            "NVIDIA_VISIBLE_DEVICES=void"
        ])
    );
    assert!(!own_args_pass_gpu(&m, &s));
    m.extra_run_args = Some(args(&["--device", "nvidia.com/gpu=all"]));
    assert_eq!(
        run_args(&m, &s),
        args(&["--device", "nvidia.com/gpu=all"]),
        "the owner's own args are never rewritten"
    );
    assert!(own_args_pass_gpu(&m, &s), "but the surfaces say so");
    m.extra_run_args = Some(args(&["--security-opt", "label=disable"]));
    assert!(!own_args_pass_gpu(&m, &s));
    // A GPU row's own GPU args are what it is for.
    m.backend = None;
    m.extra_run_args = Some(args(&["--device", "nvidia.com/gpu=all"]));
    assert!(!own_args_pass_gpu(&m, &s));
}
