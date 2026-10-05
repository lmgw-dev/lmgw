//! The presets against the real upstream Dockerfiles.
//!
//! The fixtures in `tests/fixtures/dockerfiles/` are copies of the upstream
//! files the WP0 spike built (official 171e884, ik 1aaf710, audio 955c872,
//! sd 2f88688), plus the four Containerfiles the spike built from them
//! (`*.spike.Containerfile`) — the measured, working results. The tested
//! profiles must reproduce those exactly (modulo the templated cache ids);
//! every profile's edits must match today's upstream text.

use super::*;
use crate::runtime::registry::CmdOutput;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/dockerfiles/",
            $name
        ))
    };
}

/// Every profile with its upstream fixture.
fn upstream(id: &str) -> &'static str {
    match id {
        "llama-official-cuda" => fixture!("official-cuda.Dockerfile"),
        "llama-official-vulkan" => fixture!("official-vulkan.Dockerfile"),
        "llama-official-rocm" => fixture!("official-rocm.Dockerfile"),
        "llama-official-cpu" => fixture!("official-cpu.Dockerfile"),
        "llama-ik-cuda" => fixture!("ik-cuda.Dockerfile"),
        "llama-ik-vulkan" => fixture!("ik-vulkan.Dockerfile"),
        "audio-cuda" => fixture!("audio-cuda.Dockerfile"),
        "audio-vulkan" => fixture!("audio-vulkan.Dockerfile"),
        "audio-cpu" => fixture!("audio-cpu.Dockerfile"),
        "sdcpp-cuda" => fixture!("sd-cuda.Dockerfile"),
        "sdcpp-vulkan" => fixture!("sd-vulkan.Dockerfile"),
        "sdcpp-cpu" => fixture!("sd-cpu.Dockerfile"),
        other => panic!("no fixture for profile {other}"),
    }
}

fn profile(id: &str) -> &'static DockerfileProfile {
    PROFILES.iter().find(|p| p.id == id).expect("known profile")
}

/// The ids the spike hardcoded (`lmgw-llama-npm` predates the per-engine npm
/// id), so "modulo templating" is an exact comparison.
fn spike_vars(ccache_id: &str) -> TemplateVars {
    TemplateVars {
        ccache_id: ccache_id.into(),
        npm_cache_id: "lmgw-llama-npm".into(),
        ccache_max_size: "10G".into(),
        ccache_shared_id: None,
        npm_shared_id: None,
    }
}

fn without_lines(text: &str, drop: &[&str]) -> String {
    text.lines()
        .filter(|l| !drop.contains(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_profile_has_a_fixture_and_every_edit_matches_it_today() {
    for p in &PROFILES {
        let report = apply_edits_report(upstream(p.id), &p.edits_for(true), &spike_vars("x"));
        for o in &report.outcomes {
            assert!(o.applied(), "{}: {}", p.id, o.log_line());
        }
        assert_eq!(report.check(), Ok(()), "{}", p.id);
        // No placeholder survives rendering.
        assert!(!report.text.contains("{{"), "{}", p.id);
    }
}

#[test]
fn the_tested_profiles_reproduce_the_spike_containerfiles() {
    let (official, _) = apply_edits(
        fixture!("official-cuda.Dockerfile"),
        &profile("llama-official-cuda").edits_for(true),
        &spike_vars("lmgw-llama-cuda"),
    )
    .unwrap();
    assert_eq!(official, fixture!("official-cuda.spike.Containerfile"));

    // audio: the spike's image had no eSpeak NG (`runtime-espeak` came with
    // the Kokoro fix); without that edit the result is its file byte for
    // byte.
    let mut audio_vars = spike_vars("lmgw-audio-cuda");
    audio_vars.npm_cache_id = "unused".into();
    let audio_edits: Vec<BuildEdit> = profile("audio-cuda")
        .edits_for(true)
        .into_iter()
        .filter(|e| e.name != "runtime-espeak")
        .collect();
    let (audio, _) =
        apply_edits(fixture!("audio-cuda.Dockerfile"), &audio_edits, &audio_vars).unwrap();
    assert_eq!(audio, fixture!("audio-cuda.spike.Containerfile"));

    // sd.cpp: the spike did not inject build info (§14.2 "not verified");
    // without those two edits the result is the spike's file byte for byte.
    let sd_edits: Vec<BuildEdit> = profile("sdcpp-cuda")
        .edits_for(true)
        .into_iter()
        .filter(|e| e.role != EditRole::BuildInfo)
        .collect();
    let (sd, _) = apply_edits(
        fixture!("sd-cuda.Dockerfile"),
        &sd_edits,
        &spike_vars("lmgw-sdcpp-cuda"),
    )
    .unwrap();
    assert_eq!(sd, fixture!("sd-cuda.spike.Containerfile"));

    // ik: the build-number ARGs are anchored on the stage line instead of
    // the spike's `ARG CUDA_DOCKER_ARCH="86;90"` (the line most likely to
    // change). Same two lines, same stage, declared earlier.
    let (ik, _) = apply_edits(
        fixture!("ik-cuda.Dockerfile"),
        &profile("llama-ik-cuda").edits_for(true),
        &spike_vars("lmgw-llama-cuda"),
    )
    .unwrap();
    let args = ["ARG LLAMA_BUILD_NUMBER=0", "ARG LLAMA_BUILD_COMMIT=unknown"];
    assert_eq!(
        without_lines(&ik, &args),
        without_lines(fixture!("ik-cuda.spike.Containerfile"), &args)
    );
    let stage = ik.find("AS build\n").unwrap();
    let compile = ik.find("RUN --mount=type=cache").unwrap();
    for a in args {
        let at = ik.find(a).unwrap();
        assert!(
            stage < at && at < compile,
            "{a} is declared in the build stage"
        );
    }
}

#[test]
fn ccache_off_drops_the_cache_edits_and_keeps_the_build_info() {
    for p in &PROFILES {
        let edits = p.edits_for(false);
        assert!(edits.iter().all(|e| !e.role.needs_ccache()), "{}", p.id);
        let (text, _) = apply_edits(upstream(p.id), &edits, &spike_vars("x")).unwrap();
        assert!(!text.contains("--mount=type=cache"), "{}", p.id);
        assert!(!text.contains("ccache -s"), "{}", p.id);
    }
    let (ik, _) = apply_edits(
        fixture!("ik-cuda.Dockerfile"),
        &profile("llama-ik-cuda").edits_for(false),
        &spike_vars("x"),
    )
    .unwrap();
    assert!(ik
        .contains("RUN sed -i \"s/^set(BUILD_NUMBER 0)/set(BUILD_NUMBER ${LLAMA_BUILD_NUMBER})/;"));
    let (audio, _) = apply_edits(
        fixture!("audio-cuda.Dockerfile"),
        &profile("audio-cuda").edits_for(false),
        &spike_vars("x"),
    )
    .unwrap();
    assert!(audio.contains("RUN sed -i \"s/^set(AUDIOCPP_GIT_SHA"));
    assert!(!audio.contains("COMPILER_LAUNCHER"));
}

#[test]
fn every_audio_runtime_stage_gains_espeak_ng() {
    for id in ["audio-cuda", "audio-vulkan", "audio-cpu"] {
        let p = profile(id);
        let report = apply_edits_report(upstream(id), &p.edits_for(true), &spike_vars("x"));
        let espeak = report
            .outcomes
            .iter()
            .find(|o| o.name == "runtime-espeak")
            .unwrap_or_else(|| panic!("{id}: no runtime-espeak edit"));
        assert!(
            !espeak.required,
            "{id}: a reworded upstream must not fail the build"
        );
        // Exactly the runtime stage's install line, never the build stage's.
        assert_eq!(espeak.matches, 1, "{id}");
        let info = DockerfileInfo::parse(&report.text);
        let base = info
            .stages
            .iter()
            .position(|s| s.name.as_deref() == Some("base"))
            .unwrap();
        let at = report.text.find("libespeak-ng1 espeak-ng-data").unwrap();
        let base_from = report
            .text
            .match_indices("\nFROM ")
            .nth(base)
            .map(|(i, _)| i)
            .unwrap();
        assert!(at > base_from, "{id}: installed in the runtime stage");
        // The cache switch does not take it away: it is not a cache edit.
        let plain = apply_edits_report(upstream(id), &p.edits_for(false), &spike_vars("x"));
        assert!(plain.text.contains("libespeak-ng1 espeak-ng-data"), "{id}");
    }
}

#[test]
fn choose_dockerfile_carries_the_espeak_edit_for_audio() {
    let spec = BuildSpec {
        engine: Engine::Audio,
        backend: GpuBackend::Cuda,
        repo_url: "https://github.com/0xShug0/audio.cpp".into(),
        ccache: true,
        ..BuildSpec::default()
    };
    let choice = choose_dockerfile(
        &spec,
        ".devops/cuda.Dockerfile",
        fixture!("audio-cuda.Dockerfile"),
    )
    .unwrap();
    let names: Vec<&str> = choice.edits.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"runtime-espeak"), "{names:?}");
    // An explicit edit list (a build saved before the preset grew it) is the
    // owner's: nothing is appended behind their back.
    let own = BuildSpec {
        edits: Some(vec![profile("audio-cuda").edits[0].to_edit()]),
        ..spec
    };
    let choice = choose_dockerfile(
        &own,
        ".devops/cuda.Dockerfile",
        fixture!("audio-cuda.Dockerfile"),
    )
    .unwrap();
    assert!(choice.edits.iter().all(|e| e.name != "runtime-espeak"));
}

#[test]
fn the_sd_build_info_rewrites_the_cmake_fallbacks() {
    for id in ["sdcpp-cuda", "sdcpp-vulkan", "sdcpp-cpu"] {
        let (text, _) =
            apply_edits(upstream(id), &profile(id).edits_for(true), &spike_vars("x")).unwrap();
        let info = DockerfileInfo::parse(&text);
        assert!(info.declares_arg("SDCPP_BUILD_VERSION"), "{id}");
        assert!(info.declares_arg("SDCPP_BUILD_COMMIT"), "{id}");
        assert!(
            text.contains("CMakeLists.txt && cmake . -B ./build"),
            "{id}: the sed runs before configure"
        );
        assert!(profile(id)
            .edits
            .iter()
            .filter(|e| e.role == EditRole::BuildInfo)
            .all(|e| !e.required));
    }
    // What the sed looks for is what sd.cpp's CMakeLists.txt (2f88688) has,
    // indented inside `if(NOT …)`, which is why it is not anchored at `^`.
    assert!(sd_build_info_sed!().contains("s/set(SDCPP_BUILD_VERSION unknown)/"));
    assert!(sd_build_info_sed!().contains("s/set(SDCPP_BUILD_COMMIT unknown)/"));
}

#[test]
fn a_required_edit_that_misses_fails_naming_it() {
    let edits = vec![
        BuildEdit {
            name: "fine".into(),
            find: "FROM".into(),
            replace: "FROM".into(),
            required: true,
            ..BuildEdit::default()
        },
        BuildEdit {
            name: "gone".into(),
            find: "no such text".into(),
            replace: "x".into(),
            required: false,
            ..BuildEdit::default()
        },
        BuildEdit {
            name: "ccache-mount".into(),
            role: EditRole::Ccache,
            find: "RUN make".into(),
            replace: "x".into(),
            required: true,
        },
    ];
    let report = apply_edits_report("FROM a\nFROM b\n", &edits, &spike_vars("x"));
    assert_eq!(
        report.log_lines(),
        vec![
            "edit edits[0] fine (other): applied (2 matches)",
            "edit edits[1] gone (other): not matched",
            "edit edits[2] ccache-mount (ccache): NOT MATCHED (required)",
        ]
    );
    let err = apply_edits("FROM a\n", &edits, &spike_vars("x")).unwrap_err();
    assert!(err.contains("edits[2] ccache-mount"), "{err}");
    assert!(!err.contains("gone"), "{err}");
    assert!(err.contains("Customize edits"), "{err}");
}

#[test]
fn edits_apply_in_order_and_replace_every_occurrence() {
    let edits = vec![
        BuildEdit {
            find: "a".into(),
            replace: "b".into(),
            ..BuildEdit::default()
        },
        BuildEdit {
            find: "b".into(),
            replace: "{{ccache_id}}:{{ccache_max_size}}:{{npm_cache_id}}:{{other}}".into(),
            ..BuildEdit::default()
        },
    ];
    let vars = TemplateVars::new(Engine::Llama, GpuBackend::Cuda, "s", false, "5G");
    let (text, outcomes) = apply_edits("a a", &edits, &vars).unwrap();
    assert_eq!(
        text,
        "lmgw-llama-cuda:5G:lmgw-npm-llama:{{other}} lmgw-llama-cuda:5G:lmgw-npm-llama:{{other}}"
    );
    assert_eq!(outcomes[1].matches, 2);
}

#[test]
fn template_ids_split_per_slug_once_extras_are_merged() {
    let plain = TemplateVars::new(Engine::Audio, GpuBackend::Vulkan, "main", false, "10G");
    assert_eq!(plain.ccache_id, "lmgw-audio-vulkan");
    assert_eq!(plain.npm_cache_id, "lmgw-npm-audio");
    assert_eq!(plain.ccache_shared_id, None);
    let pr = TemplateVars::new(Engine::Llama, GpuBackend::Cuda, "master-pr1", true, "10G");
    assert_eq!(pr.ccache_id, "lmgw-llama-cuda-master-pr1");
    assert_eq!(pr.npm_cache_id, "lmgw-npm-llama-master-pr1");
    assert_eq!(pr.ccache_shared_id.as_deref(), Some("lmgw-llama-cuda"));
}

/// The WP0 spike filled these cache mounts; a plain build of each tested
/// profile must land in exactly them, or its first run compiles cold.
#[test]
fn plain_builds_use_the_spike_cache_ids() {
    for (engine, id) in [
        (Engine::Llama, "lmgw-llama-cuda"),
        (Engine::Audio, "lmgw-audio-cuda"),
        (Engine::Sdcpp, "lmgw-sdcpp-cuda"),
    ] {
        let vars = TemplateVars::new(engine, GpuBackend::Cuda, "whatever", false, "10G");
        assert_eq!(vars.ccache_id, id);
        let p = PROFILES
            .iter()
            .find(|p| p.engine == engine && p.backend == GpuBackend::Cuda)
            .unwrap();
        let (text, _) = apply_edits(upstream(p.id), &p.edits_for(true), &vars).unwrap();
        assert!(
            text.contains(&format!("--mount=type=cache,id={id},target=/ccache export")),
            "{}",
            p.id
        );
        assert!(!text.contains(CCACHE_SHARED_TARGET), "{}", p.id);
        assert!(!text.contains("cp -a"), "{}", p.id);
    }
}

/// §14.2 "PR builds read the shared cache": a build with extras compiles
/// into its own cache, seeded from the plain build's cache mounted
/// read-only — in every profile that mounts a ccache at all.
#[test]
fn builds_with_extras_seed_their_own_cache_from_the_shared_one_read_only() {
    for p in &PROFILES {
        let vars = TemplateVars::new(p.engine, p.backend, "master-pr7", true, "10G");
        let own = format!(
            "lmgw-{}-{}-master-pr7",
            p.engine.as_str(),
            p.backend.as_str()
        );
        let shared = format!("lmgw-{}-{}", p.engine.as_str(), p.backend.as_str());
        let (text, _) = apply_edits(upstream(p.id), &p.edits_for(true), &vars).unwrap();
        assert!(!text.contains("{{"), "{}", p.id);
        let runs: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("target=/ccache"))
            .collect();
        assert_eq!(runs.len(), 1, "{}: one compile step mounts the cache", p.id);
        let run = runs[0];
        let mount_own = format!("--mount=type=cache,id={own},target=/ccache ");
        let mount_shared = format!("--mount=type=cache,id={shared},target=/ccache-shared,ro ");
        assert!(run.starts_with("RUN "), "{}: {run}", p.id);
        assert!(run.contains(&mount_own), "{}: {run}", p.id);
        assert!(run.contains(&mount_shared), "{}: {run}", p.id);
        // Both mounts are flags of the RUN, before its command.
        let command = run
            .find(" export CCACHE_DIR=/ccache ")
            .expect("the command");
        assert!(run.find(&mount_shared).unwrap() < command, "{}", p.id);
        // Seeded before the stats are zeroed, so `ccache -s` counts only this
        // build's lookups; never the other way round (the shared cache is
        // never a copy target).
        let seed = run
            .find("cp -a -u --reflink=auto /ccache-shared/. /ccache/ && ")
            .unwrap_or_else(|| panic!("{}: no seed in {run}", p.id));
        assert!(seed < run.find("ccache -z").unwrap(), "{}", p.id);
        assert!(!text.contains("/ccache-shared/ &&"), "{}", p.id);
        assert!(
            !text.contains(&format!("id={shared},target=/ccache ")),
            "{}",
            p.id
        );
    }
    // ccache off: no cache mount of either kind.
    for p in &PROFILES {
        let vars = TemplateVars::new(p.engine, p.backend, "x", true, "10G");
        let (text, _) = apply_edits(upstream(p.id), &p.edits_for(false), &vars).unwrap();
        assert!(!text.contains("/ccache"), "{}", p.id);
    }
}

/// The npm cache mount exists only on official's four profiles (§14.2 "An
/// optional npm cache mount"); a plain build lands in exactly the id the
/// spike hardcoded before the per-engine id existed.
const OFFICIAL_PROFILES: [&str; 4] = [
    "llama-official-cuda",
    "llama-official-vulkan",
    "llama-official-rocm",
    "llama-official-cpu",
];

#[test]
fn plain_builds_use_the_spike_npm_cache_id() {
    for id in OFFICIAL_PROFILES {
        let p = profile(id);
        let vars = TemplateVars::new(p.engine, p.backend, "whatever", false, "10G");
        assert_eq!(vars.npm_cache_id, "lmgw-npm-llama");
        let (text, _) = apply_edits(upstream(id), &p.edits_for(true), &vars).unwrap();
        assert!(
            text.contains("--mount=type=cache,id=lmgw-npm-llama,target=/root/.npm npm ci"),
            "{id}: {text}"
        );
        assert!(!text.contains(NPM_CACHE_SHARED_TARGET), "{id}");
        assert!(!text.contains("cp -a"), "{id}");
    }
}

/// §14.2 "npm follows the same pattern": a build with extras seeds its own
/// npm cache from the plain build's, mounted read-only, the same way ccache
/// does — and a profile with no npm-cache edit at all never gains one.
#[test]
fn builds_with_extras_seed_their_own_npm_cache_from_the_shared_one_read_only() {
    for id in OFFICIAL_PROFILES {
        let p = profile(id);
        let vars = TemplateVars::new(p.engine, p.backend, "master-pr7", true, "10G");
        let (text, _) = apply_edits(upstream(id), &p.edits_for(true), &vars).unwrap();
        assert!(!text.contains("{{"), "{id}");
        let runs: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("target=/root/.npm"))
            .collect();
        assert_eq!(runs.len(), 1, "{id}: one npm ci step mounts the cache");
        let run = runs[0];
        let mount_own = "--mount=type=cache,id=lmgw-npm-llama-master-pr7,target=/root/.npm ";
        let mount_shared = "--mount=type=cache,id=lmgw-npm-llama,target=/npm-shared,ro ";
        assert!(run.starts_with("RUN "), "{id}: {run}");
        assert!(run.contains(mount_own), "{id}: {run}");
        assert!(run.contains(mount_shared), "{id}: {run}");
        // Both mounts are flags of the RUN, before `npm ci`.
        let command = run.find("npm ci").expect("the command");
        assert!(run.find(mount_shared).unwrap() < command, "{id}");
        let seed = run
            .find("cp -a -u --reflink=auto /npm-shared/. /root/.npm/ && ")
            .unwrap_or_else(|| panic!("{id}: no seed in {run}"));
        assert!(seed < command, "{id}");
        assert!(!text.contains("/npm-shared/ &&"), "{id}");
        assert!(
            !text.contains("id=lmgw-npm-llama,target=/root/.npm "),
            "{id}: the plain id must not be the one written to"
        );
    }
    // A build with no npm-cache edit at all (ik, audio, sd.cpp) never gains
    // an npm cache mount, extras or not — even though their upstream
    // Dockerfiles (sd.cpp's) legitimately mention npm for other reasons.
    for id in ["llama-ik-cuda", "audio-cuda", "sdcpp-cuda"] {
        let p = profile(id);
        let vars = TemplateVars::new(p.engine, p.backend, "master-pr7", true, "10G");
        let (text, _) = apply_edits(upstream(id), &p.edits_for(true), &vars).unwrap();
        assert!(!text.contains("--mount=type=cache,id=lmgw-npm"), "{id}");
        assert!(!text.contains(NPM_CACHE_SHARED_TARGET), "{id}");
    }
}

#[test]
fn candidates_are_tried_official_first() {
    assert_eq!(
        dockerfile_candidates(Engine::Llama, GpuBackend::Cuda),
        vec![
            ".devops/cuda.Dockerfile",
            ".devops/llama-server-cuda.Dockerfile"
        ]
    );
    assert_eq!(
        dockerfile_candidates(Engine::Sdcpp, GpuBackend::Cuda),
        vec!["docker/Dockerfile.cuda"]
    );
    assert!(dockerfile_candidates(Engine::Audio, GpuBackend::Rocm).is_empty());
    // Same path, different engine: the key includes the engine.
    assert_eq!(
        profile_for(Engine::Audio, GpuBackend::Cuda, ".devops/cuda.Dockerfile")
            .unwrap()
            .id,
        "audio-cuda"
    );
    assert!(profile_for(Engine::Llama, GpuBackend::Vulkan, ".devops/cuda.Dockerfile").is_none());
    // Profile ids are unique, and exactly the four CUDA ones are tested.
    let mut ids: Vec<&str> = PROFILES.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), PROFILES.len());
    let tested: Vec<&str> = PROFILES.iter().filter(|p| p.tested).map(|p| p.id).collect();
    assert_eq!(
        tested,
        vec![
            "llama-official-cuda",
            "llama-ik-cuda",
            "audio-cuda",
            "sdcpp-cuda"
        ]
    );
}

#[test]
fn a_missing_dockerfile_is_worded_with_what_was_tried() {
    let mut spec = BuildSpec {
        engine: Engine::Llama,
        repo_url: "https://github.com/x/y".into(),
        ..BuildSpec::default()
    };
    assert_eq!(dockerfiles_to_try(&spec).len(), 2);
    let err = no_dockerfile_error(&spec, &"a".repeat(40));
    assert!(
        err.contains(".devops/llama-server-cuda.Dockerfile"),
        "{err}"
    );
    assert!(err.contains("set dockerfile"), "{err}");
    spec.dockerfile = Some("Containerfile".into());
    assert_eq!(dockerfiles_to_try(&spec), vec!["Containerfile".to_string()]);
    assert!(no_dockerfile_error(&spec, "abc").contains("Containerfile does not exist"));
}

#[test]
fn repo_presets_are_found_by_any_spelling_of_their_url() {
    assert_eq!(
        repo_preset_for_url("https://github.com/ikawrakow/ik_llama.cpp.git/")
            .unwrap()
            .id,
        "ik"
    );
    assert_eq!(
        repo_preset_for_url("git@github.com:0xShug0/audio.cpp.git")
            .unwrap()
            .id,
        "audio"
    );
    assert_eq!(
        repo_preset_for_url("https://GitHub.com/ggml-org/llama.cpp")
            .unwrap()
            .flavor,
        Flavor::Official
    );
    assert!(repo_preset_for_url("https://github.com/someone/llama.cpp").is_none());
    assert_eq!(repo_preset("sdcpp").unwrap().default_ref, "master");
    assert_eq!(
        web_url("ssh://git@git.example.com:2222/p/x.git"),
        "https://git.example.com/p/x"
    );
    assert_eq!(web_url("file:///srv/x"), "file:///srv/x");
    assert_eq!(
        web_url("https://x-access-token:ghp_secret@github.com/o/r.git"),
        "https://github.com/o/r",
        "a label or IMAGE_URL never carries credentials"
    );
}

#[test]
fn stages_and_args_are_read_off_the_upstream_files() {
    let official = DockerfileInfo::parse(upstream("llama-official-cuda"));
    let names: Vec<Option<&str>> = official.stages.iter().map(|s| s.name.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            Some("web"),
            Some("build"),
            Some("base"),
            Some("full"),
            Some("light"),
            Some("server")
        ]
    );
    for a in [
        "CUDA_VERSION",
        "CUDA_DOCKER_ARCH",
        "APP_VERSION",
        "APP_REVISION",
        "BUILD_DATE",
        "IMAGE_URL",
    ] {
        assert!(official.declares_arg(a), "{a}");
    }
    assert!(
        !official.declares_arg("LLAMA_BUILD_NUMBER"),
        "only after the edits"
    );

    let ik = DockerfileInfo::parse(upstream("llama-ik-cuda"));
    assert!(ik.has_stage("RUNTIME"), "stage names are case-insensitive");
    assert!(!ik.declares_arg("APP_VERSION"));
    let sd = DockerfileInfo::parse(upstream("sdcpp-cuda"));
    assert!(sd.declares_arg("CUDA_ARCHITECTURES"));
    assert_eq!(
        sd.stages[0].from,
        "nvidia/cuda:${CUDA_VERSION}-cudnn-devel-ubuntu${UBUNTU_VERSION}"
    );

    for p in &PROFILES {
        let info = DockerfileInfo::parse(upstream(p.id));
        assert_eq!(
            info.pick_target(p.targets).as_deref(),
            Some(p.targets[0]),
            "{}",
            p.id
        );
        if let Some(a) = p.arch_arg {
            assert!(info.declares_arg(a), "{} declares {a}", p.id);
        }
    }
}

#[test]
fn the_parser_handles_continuations_comments_flags_and_quotes() {
    let info = DockerfileInfo::parse(
        "# syntax=docker/dockerfile:1\n\
         arg A=1 B=\"x y\" \\\n\
         # a comment inside the continuation\n\
             C\n\
         FROM --platform=$BUILDPLATFORM docker.io/x:1 as Build\n\
         FROM build\n",
    );
    assert_eq!(info.args, vec!["A", "B", "C"]);
    assert_eq!(info.stages.len(), 2);
    assert_eq!(info.stages[0].name.as_deref(), Some("Build"));
    assert_eq!(info.stages[0].from, "docker.io/x:1");
    assert_eq!(info.stages[1].name, None);
    assert!(info.has_stage("build"));
}

fn spec_for(engine: Engine, backend: GpuBackend) -> BuildSpec {
    BuildSpec {
        slug: "s".into(),
        engine,
        backend,
        repo_url: "https://github.com/ggml-org/llama.cpp".into(),
        ..BuildSpec::default()
    }
}

#[test]
fn choosing_picks_the_preset_target_and_edits() {
    let spec = spec_for(Engine::Llama, GpuBackend::Cuda);
    let c = choose_dockerfile(
        &spec,
        ".devops/cuda.Dockerfile",
        upstream("llama-official-cuda"),
    )
    .unwrap();
    assert_eq!(c.profile.unwrap().id, "llama-official-cuda");
    assert_eq!(c.target, "server");
    assert_eq!(c.edits, profile("llama-official-cuda").edits_for(true));
    assert_eq!(c.verify.entrypoint, "/app/llama-server");
    assert!(c.notes.is_empty(), "{:?}", c.notes);

    let mut vk = spec_for(Engine::Llama, GpuBackend::Vulkan);
    vk.target = Some("full".into());
    let c = choose_dockerfile(
        &vk,
        ".devops/vulkan.Dockerfile",
        upstream("llama-official-vulkan"),
    )
    .unwrap();
    assert_eq!(c.target, "full");
    assert!(c.notes[0].contains("never been built"), "{:?}", c.notes);

    vk.target = Some("nope".into());
    let err = choose_dockerfile(
        &vk,
        ".devops/vulkan.Dockerfile",
        upstream("llama-official-vulkan"),
    )
    .unwrap_err();
    assert!(
        err.contains("web, build, base, full, light, server"),
        "{err}"
    );
}

#[test]
fn customized_edits_are_used_as_given_but_still_obey_the_ccache_switch() {
    let mut spec = spec_for(Engine::Llama, GpuBackend::Cuda);
    let mine = vec![
        BuildEdit {
            name: "mount".into(),
            role: EditRole::Ccache,
            find: "a".into(),
            ..BuildEdit::default()
        },
        BuildEdit {
            name: "mine".into(),
            find: "b".into(),
            ..BuildEdit::default()
        },
    ];
    spec.edits = Some(mine.clone());
    let text = upstream("llama-official-cuda");
    let c = choose_dockerfile(&spec, ".devops/cuda.Dockerfile", text).unwrap();
    assert_eq!(c.edits, mine);
    spec.ccache = false;
    let c = choose_dockerfile(&spec, ".devops/cuda.Dockerfile", text).unwrap();
    assert_eq!(c.edits, vec![mine[1].clone()]);
    spec.edits = Some(Vec::new());
    let c = choose_dockerfile(&spec, ".devops/cuda.Dockerfile", text).unwrap();
    assert!(c.edits.is_empty(), "an explicit empty list means no edits");
}

#[test]
fn an_unknown_dockerfile_gets_no_edits_a_fallback_target_and_the_repo_flavor() {
    let mut spec = spec_for(Engine::Llama, GpuBackend::Cuda);
    spec.repo_url = "https://github.com/ikawrakow/ik_llama.cpp".into();
    spec.dockerfile = Some("docker/custom.Containerfile".into());
    let text = "FROM x AS build\nFROM y AS runtime\nFROM z AS extra\n";
    let c = choose_dockerfile(&spec, "docker/custom.Containerfile", text).unwrap();
    assert!(c.profile.is_none());
    assert!(c.edits.is_empty());
    assert_eq!(c.target, "runtime");
    assert_eq!(c.verify.entrypoint, "/llama-server", "ik's probes");
    assert!(
        c.notes.iter().any(|n| n.contains("no edits")),
        "{:?}",
        c.notes
    );

    // No preferred stage: the last one, said out loud.
    let c = choose_dockerfile(&spec, "x", "FROM a AS one\nFROM b AS two\n").unwrap();
    assert_eq!(c.target, "two");
    assert!(c.notes.iter().any(|n| n.contains("last stage, two")));
    let c = choose_dockerfile(&spec, "x", "FROM a\n").unwrap();
    assert_eq!(c.target, "");
    assert!(choose_dockerfile(&spec, "x", "# empty\n").is_err());
}

fn facts(repo: &str, sha: &str, n: u64, date: &str) -> SourceFacts {
    SourceFacts {
        repo_url: repo.into(),
        sha: sha.into(),
        build_number: n,
        commit_date: chrono::DateTime::parse_from_rfc3339(date).unwrap(),
    }
}

fn args_for(id: &str, f: &SourceFacts, arch: &[&str], user: &str) -> Result<BuildArgs, String> {
    let p = profile(id);
    let spec = spec_for(p.engine, p.backend);
    let choice = choose_dockerfile(&spec, p.dockerfile, upstream(id)).unwrap();
    let (text, _) = apply_edits(upstream(id), &choice.edits, &spike_vars("x")).unwrap();
    let arch: Vec<String> = arch.iter().map(|a| a.to_string()).collect();
    build_args(
        &choice,
        p.backend,
        &DockerfileInfo::parse(&text),
        f,
        resolve_cuda_version(&spec).as_deref(),
        &arch,
        user,
    )
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The build args the spike passed (from a local spike checkout),
/// derived from the same commits.
#[test]
fn the_build_args_are_what_the_spike_passed() {
    let official = facts(
        "https://github.com/ggml-org/llama.cpp",
        "171e8846b4af9766c354064cb776cb34a50f053f",
        11192,
        "2026-09-26T01:55:18+02:00",
    );
    let a = args_for("llama-official-cuda", &official, &["89"], "").unwrap();
    assert_eq!(
        a.args,
        pairs(&[
            ("CUDA_VERSION", "13.0.0"),
            ("CUDA_DOCKER_ARCH", "89"),
            ("APP_VERSION", "b11192"),
            ("APP_REVISION", "171e8846b4af9766c354064cb776cb34a50f053f"),
            ("BUILD_DATE", "2026-09-25T23:55:18Z"),
            ("IMAGE_URL", "https://github.com/ggml-org/llama.cpp"),
            ("IMAGE_SOURCE", "https://github.com/ggml-org/llama.cpp"),
            ("LLAMA_BUILD_NUMBER", "11192"),
            ("LLAMA_BUILD_COMMIT", "171e884"),
        ])
    );
    assert!(a.notes.is_empty(), "{:?}", a.notes);

    let ik = facts(
        "https://github.com/ikawrakow/ik_llama.cpp",
        "1aaf7105be6e55a97fa4a9fd6f5bd362b08436dc",
        4960,
        "2026-09-25T16:06:19+02:00",
    );
    let a = args_for("llama-ik-cuda", &ik, &["86", "89"], "GGML_X=1").unwrap();
    assert_eq!(
        a.args,
        pairs(&[
            ("CUDA_VERSION", "13.0.0"),
            ("CUDA_DOCKER_ARCH", "86;89"),
            ("LLAMA_BUILD_NUMBER", "4960"),
            ("LLAMA_BUILD_COMMIT", "1aaf710"),
            ("GGML_X", "1"),
        ])
    );

    let audio = facts(
        "https://github.com/0xShug0/audio.cpp",
        "955c8725c611d511774e6be132aff6609163b2d2",
        812,
        "2026-09-25T18:44:03-04:00",
    );
    let a = args_for("audio-cuda", &audio, &["89"], "").unwrap();
    assert_eq!(
        a.args,
        pairs(&[
            ("CUDA_VERSION", "13.0.0"),
            ("CUDA_DOCKER_ARCH", "89"),
            ("APP_VERSION", "b812"),
            ("APP_REVISION", "955c8725c611d511774e6be132aff6609163b2d2"),
            ("BUILD_DATE", "2026-09-25T22:44:03Z"),
            ("IMAGE_URL", "https://github.com/0xShug0/audio.cpp"),
            ("IMAGE_SOURCE", "https://github.com/0xShug0/audio.cpp"),
            ("AUDIOCPP_VERSION", "b812"),
            ("AUDIOCPP_GIT_SHA", "955c872"),
            ("AUDIOCPP_GIT_DATE", "2026-09-25"),
        ])
    );

    let sd = facts(
        "https://github.com/leejet/stable-diffusion.cpp",
        "2f886889e6e8b78738d6b87f7191f6018557c551",
        920,
        "2026-09-26T02:03:05+08:00",
    );
    let a = args_for("sdcpp-cuda", &sd, &["89"], "").unwrap();
    assert_eq!(
        a.args,
        pairs(&[
            ("CUDA_VERSION", "13.0.0"),
            ("CUDA_ARCHITECTURES", "89"),
            ("SDCPP_BUILD_VERSION", "b920"),
            ("SDCPP_BUILD_COMMIT", "2f88688"),
        ])
    );
}

#[test]
fn an_arch_with_nowhere_to_go_is_a_note_and_a_reserved_arg_an_error() {
    let f = facts("https://x/y", &"a".repeat(40), 1, "2026-01-01T00:00:00Z");
    let a = args_for("audio-vulkan", &f, &["89"], "").unwrap();
    assert!(!a.args.iter().any(|(k, _)| k.contains("ARCH")));
    assert!(
        a.notes[0].contains("arch 89 is not applied"),
        "{:?}",
        a.notes
    );
    let err = args_for("audio-vulkan", &f, &[], "LLAMA_BUILD_NUMBER=5").unwrap_err();
    assert!(err.contains("passed by lmgw itself"), "{err}");
}

#[test]
fn verify_uses_the_measured_probes() {
    let official = verify_spec(Flavor::Official, GpuBackend::Cuda);
    assert_eq!(official.device_args, Some(&["--list-devices"][..]));
    let ik = verify_spec(Flavor::Ik, GpuBackend::Cuda);
    assert_eq!(ik.entrypoint, "/llama-server");
    assert_eq!(ik.device_args, Some(&["-m", "/nonexistent.gguf"][..]));
    let audio = verify_spec(Flavor::Audio, GpuBackend::Cuda);
    assert_eq!(audio.entrypoint, "/app/entrypoint.sh");
    assert_eq!(audio.help_args, &["server", "--help"]);
    assert_eq!(audio.device_args, Some(&["server", "--list-devices"][..]));
    let sd = verify_spec(Flavor::Sdcpp, GpuBackend::Cuda);
    assert_eq!(sd.entrypoint, "/sd-server");
    let cpu = verify_spec(Flavor::Official, GpuBackend::Cpu);
    assert_eq!(cpu.device_args, None);
    assert_eq!(devices_found(&cpu, "anything"), Ok(vec![]));
    // Every pattern compiles.
    for f in [Flavor::Official, Flavor::Ik, Flavor::Audio, Flavor::Sdcpp] {
        for b in [
            GpuBackend::Cuda,
            GpuBackend::Vulkan,
            GpuBackend::Rocm,
            GpuBackend::Cpu,
        ] {
            let v = verify_spec(f, b);
            let _ = help_ok(&v, "");
            let _ = devices_found(&v, "");
            assert_eq!(v.devices.is_some_and(|d| d.measured), b == GpuBackend::Cuda);
        }
    }
}

#[test]
fn devices_are_read_from_the_output_and_their_absence_is_the_error() {
    let official = verify_spec(Flavor::Official, GpuBackend::Cuda);
    let ok = "ggml_cuda_init: found 1 CUDA devices:\nAvailable devices:\n  CUDA0: NVIDIA GeForce \
              RTX 4090 (24080 MiB, 23500 MiB free)\n";
    assert_eq!(
        devices_found(&official, ok).unwrap(),
        vec!["CUDA0: NVIDIA GeForce RTX 4090 (24080 MiB, 23500 MiB free)"]
    );
    // Without the GPU run args (§14.3): exit 0, and no device.
    let err = devices_found(&official, "Available devices:\n  (none)\n").unwrap_err();
    assert!(err.contains("did not load"), "{err}");

    let ik = verify_spec(Flavor::Ik, GpuBackend::Cuda);
    let ik_out = "ggml_cuda_init: found 1 CUDA devices:\n  Device 0: NVIDIA GeForce RTX 4090, \
                  compute capability 8.9, VMM: yes, VRAM: 24080 MiB\nllama_model_load: error \
                  loading model: failed to open /nonexistent.gguf\n";
    assert_eq!(
        devices_found(&ik, ik_out).unwrap(),
        vec![
            "Device 0: NVIDIA GeForce RTX 4090, compute capability 8.9, VMM: yes, VRAM: 24080 MiB"
        ]
    );
    assert_eq!(
        devices_found(&ik, "ggml_cuda_init: found 2 CUDA devices:\n").unwrap(),
        vec!["ggml_cuda_init: found 2 CUDA devices:"]
    );
    assert!(devices_found(
        &ik,
        "ggml_cuda_init: found 0 CUDA devices:\n  Device 0: x\n"
    )
    .is_err());
    let no_lib = "/llama-server: error while loading shared libraries: libcuda.so.1: cannot open \
                  shared object file: No such file or directory\n";
    assert!(devices_found(&ik, no_lib).is_err());
    assert!(help_ok(&ik, no_lib).is_err());

    let audio = verify_spec(Flavor::Audio, GpuBackend::Cuda);
    let a_ok = "available_devices=2\nCPU:0 \"AMD Ryzen\" [cpu]\nCUDA:0 \"NVIDIA GeForce RTX \
                4090\" [gpu]\nselect with: --backend <cuda|hip|vulkan|metal|cpu> --device <index>\n";
    assert_eq!(
        devices_found(&audio, a_ok).unwrap(),
        vec!["CUDA:0 \"NVIDIA GeForce RTX 4090\" [gpu]"]
    );
    assert!(devices_found(&audio, "available_devices=1\nCPU:0 \"AMD Ryzen\" [cpu]\n").is_err());

    let sd = verify_spec(Flavor::Sdcpp, GpuBackend::Cuda);
    let sd_ok = "CPU\tAMD Ryzen 9\nCUDA0\tNVIDIA GeForce RTX 4090\n";
    assert_eq!(
        devices_found(&sd, sd_ok).unwrap(),
        vec!["CUDA0\tNVIDIA GeForce RTX 4090"]
    );
    assert!(devices_found(&sd, "CPU\tAMD Ryzen 9\n").is_err());
}

#[test]
fn help_is_recognized_by_its_port_flag() {
    // First lines of the spike's captured help outputs.
    let official = "0.00.000.349 I srv  llama_server: initializing ...\n--port PORT        \
                    port to listen (default: 8080)\n";
    assert!(help_ok(&verify_spec(Flavor::Official, GpuBackend::Cuda), official).is_ok());
    let ik = "usage: /llama-server [options]\n         --port PORT              port to listen\n";
    assert!(help_ok(&verify_spec(Flavor::Ik, GpuBackend::Cuda), ik).is_ok());
    let audio = "audiocpp_server [--config <server.json>] [--ui] [--host <ip>] [--port <port>]\n";
    assert!(help_ok(&verify_spec(Flavor::Audio, GpuBackend::Cuda), audio).is_ok());
    let sd = "Usage: /sd.cpp/bin/sd-server [options]\n  --listen-port <int>  server listen port\n";
    let sd_spec = verify_spec(Flavor::Sdcpp, GpuBackend::Cuda);
    assert!(help_ok(&sd_spec, sd).is_ok());
    let err = help_ok(&sd_spec, "Unknown command: --help\n").unwrap_err();
    assert!(err.contains("/sd-server --help"), "{err}");
}

#[test]
fn compute_capabilities_become_a_sorted_unique_arch_list() {
    assert_eq!(parse_compute_caps("8.9\n").unwrap(), vec!["89"]);
    assert_eq!(parse_compute_caps("8.9\n8.9\n").unwrap(), vec!["89"]);
    assert_eq!(
        parse_compute_caps("12.0\n8.6\n8.9\n8.6\n").unwrap(),
        vec!["86", "89", "120"]
    );
    assert!(parse_compute_caps("").unwrap_err().contains("no GPU"));
    assert!(parse_compute_caps("[N/A]\n").unwrap_err().contains("[N/A]"));
}

#[test]
fn the_driver_cuda_version_comes_from_the_nvidia_smi_header() {
    let header = "+-----------------------------------------------------------------------------+\n\
                  | NVIDIA-SMI 615.71        Driver Version: 615.71        CUDA Version: 13.1     |\n";
    assert_eq!(parse_driver_cuda_version(header).as_deref(), Some("13.1"));
    assert_eq!(parse_driver_cuda_version("no header"), None);
    // Driver 615.71.09 relabelled it (found in the container-builds e2e:
    // build_env reported no driver maximum, so the CUDA warning never fired).
    let umd = "+-----------------------------------------------------------------------------------------+\n\
               | NVIDIA-SMI 615.71.09              KMD Version: 615.71.09     CUDA UMD Version: 13.4     |\n";
    assert_eq!(parse_driver_cuda_version(umd).as_deref(), Some("13.4"));
    assert_eq!(cuda_driver_warning("13.0.0", "13.1"), None);
    assert_eq!(cuda_driver_warning("13.1.0", "13.1"), None);
    let w = cuda_driver_warning("13.2.0", "13.1").unwrap();
    assert!(w.contains("pick 13.1 or older"), "{w}");
    assert!(cuda_driver_warning("12.9.1", "13.0").is_none());
}

/// A canned `nvidia-smi`.
struct FakeSmi(Result<CmdOutput, String>);

#[async_trait::async_trait]
impl CommandRunner for FakeSmi {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "nvidia-smi");
        if !args.is_empty() {
            assert_eq!(args, ["--query-gpu=compute_cap", "--format=csv,noheader"]);
        }
        self.0
            .clone()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))
    }
}

fn smi(stdout: &str) -> FakeSmi {
    FakeSmi(Ok(CmdOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }))
}

#[tokio::test]
async fn arch_auto_detects_for_cuda_only_and_a_set_arch_wins() {
    assert_eq!(
        resolve_arch(GpuBackend::Cuda, None, &smi("8.9\n"))
            .await
            .unwrap(),
        vec!["89"]
    );
    let mine = vec!["86".to_string()];
    assert_eq!(
        resolve_arch(
            GpuBackend::Cuda,
            Some(&mine),
            &FakeSmi(Err("unused".into()))
        )
        .await
        .unwrap(),
        mine
    );
    assert!(
        resolve_arch(GpuBackend::Rocm, None, &FakeSmi(Err("unused".into())))
            .await
            .unwrap()
            .is_empty()
    );
    let err = resolve_arch(GpuBackend::Cuda, None, &FakeSmi(Err("no such file".into())))
        .await
        .unwrap_err();
    assert!(err.contains("nvidia-smi"), "{err}");
    assert!(err.contains("set arch"), "{err}");
    let err = detect_cuda_arch(&FakeSmi(Ok(CmdOutput {
        status: 9,
        stdout: String::new(),
        stderr: "NVIDIA-SMI has failed".into(),
    })))
    .await
    .unwrap_err();
    assert!(err.contains("exited 9"), "{err}");
    assert_eq!(
        detect_driver_cuda_version(&smi("| CUDA Version: 13.0 |"))
            .await
            .unwrap(),
        "13.0"
    );
}

#[test]
fn the_cuda_version_defaults_to_13_for_cuda_only() {
    let mut s = spec_for(Engine::Sdcpp, GpuBackend::Cuda);
    assert_eq!(resolve_cuda_version(&s).as_deref(), Some("13.0.0"));
    s.cuda_version = Some("12.8.1".into());
    assert_eq!(resolve_cuda_version(&s).as_deref(), Some("12.8.1"));
    s.backend = GpuBackend::Vulkan;
    assert_eq!(resolve_cuda_version(&s), None);
}
