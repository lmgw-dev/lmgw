//! Image build and container-image mutations: `lmgw__build_set`
//! through `lmgw__container_image_pull`.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__build_set",
            writes: true,
            description:
                "Create, update, delete or duplicate a container build — the definition of an \
                 engine image lmgw builds from git (see lmgw__builds). create: name a slug and \
                 either a preset (official = ggml-org/llama.cpp, ik = ik_llama.cpp, audio = \
                 audio.cpp, sdcpp = stable-diffusion.cpp; the list with their repos is \
                 'repo_presets' in lmgw__builds) or engine + repo_url + ref; everything else \
                 has a working default (CUDA, the preset's CUDA version, arch auto-detected \
                 from this machine's GPUs, the preset's Dockerfile and edits, ccache on, keep \
                 every run). update is partial: pass only what changes. Empty strings mean \
                 'auto' for cuda_version, arch, dockerfile, target and cpus. duplicate copies \
                 id under slug/name (default '<slug>-copy'); it is the quick way to get \
                 'official-master' and 'official-master-pr16391' side by side. delete removes \
                 the definition (its run history stays) and, with delete_images=true, its \
                 images where nothing uses them — the answer lists what was removed and what \
                 was kept, and why. A run in progress blocks delete. Every field is checked \
                 and a refusal names it. Custom Dockerfile edits are dashboard-only. Building \
                 a PR runs that PR's build code on this machine (rootless podman, no GPU).",
            props: vec![
                (
                    "action",
                    enum_p("What to do.", &["create", "update", "delete", "duplicate"]),
                ),
                (
                    "id",
                    int_p("The build (lmgw__builds). Required except on create."),
                ),
                (
                    "preset",
                    enum_p(
                        "Fill engine, repo_url, forge and ref from a repository preset. Fields \
                         you also pass win.",
                        &["official", "ik", "audio", "sdcpp"],
                    ),
                ),
                (
                    "slug",
                    str_p(
                        "The image tag: lowercase letters, digits, '.', '_', '-' (e.g. \
                         'official-master'). Fixed once the build has a run. On duplicate: the \
                         copy's slug.",
                    ),
                ),
                (
                    "name",
                    str_p("Display name. Default: the slug. On duplicate: the copy's name."),
                ),
                (
                    "engine",
                    enum_p(
                        "Which server the image is: llama (llama.cpp and ik_llama.cpp — chat \
                         and aux classes), audio (audio.cpp) or sdcpp (stable-diffusion.cpp — \
                         image class).",
                        &["llama", "audio", "sdcpp"],
                    ),
                ),
                (
                    "repo_url",
                    str_p("Git URL (https://, ssh://, git@host:path) of the repository to build."),
                ),
                (
                    "forge",
                    enum_p(
                        "Where the repository's PRs live, which is how pr extras resolve. \
                         Default when repo_url is given: github for github.com, gitlab for a \
                         host with a configured forge token, else plain (no pr extras).",
                        &["github", "gitlab", "plain"],
                    ),
                ),
                (
                    "ref",
                    str_p(
                        "Branch, tag or full 40-hex commit to build on (e.g. master). A branch \
                         or tag is re-resolved on every run.",
                    ),
                ),
                (
                    "extras",
                    str_p(
                        "What to merge on top of ref, in order, one per line: 'pr <number>' for \
                         a PR/MR of this repository (lmgw__forge_prs finds numbers), or \
                         'ref <remote_url> <ref>' for a branch/tag/commit of another remote (a \
                         fork's branch, or an upstream llama.cpp PR on ik via \
                         refs/pull/<n>/head). Append a full commit SHA to pin that exact commit; \
                         unpinned extras follow their head. Replaces the whole list; '' clears \
                         it. lmgw__build_check_merge tests whether they merge.",
                    ),
                ),
                (
                    "backend",
                    enum_p(
                        "GPU backend the image is compiled for. Default cuda.",
                        &["cuda", "vulkan", "rocm", "cpu"],
                    ),
                ),
                (
                    "cuda_version",
                    str_p(
                        "CUDA toolkit of the build image, e.g. 13.0.0. '' = the preset default. \
                         cuda backend only.",
                    ),
                ),
                (
                    "arch",
                    str_p(
                        "GPU architectures to compile for, comma-separated (e.g. '89' or \
                         '86,89'; 'gfx1100' for ROCm). '' or 'auto' = this machine's GPUs.",
                    ),
                ),
                (
                    "dockerfile",
                    str_p("Repo-relative Dockerfile path. '' = the preset's candidates."),
                ),
                ("target", str_p("Final build stage. '' = the preset's.")),
                (
                    "reset_edits",
                    bool_p(
                        "On update: drop customized Dockerfile edits back to the preset's \
                         (they are otherwise kept).",
                    ),
                ),
                (
                    "ccache",
                    bool_p(
                        "Compiler cache across runs (default on) — a rebuild recompiles only \
                         what changed.",
                    ),
                ),
                (
                    "ccache_max_size",
                    str_p("CCACHE_MAXSIZE, e.g. 10G (default), 500M; 0 = no limit."),
                ),
                (
                    "cpus",
                    str_p(
                        "Cores the build may use (--cpuset-cpus), e.g. 0-15. '' = all. Needs \
                         podman's cpuset cgroup controller (rootless: delegated to \
                         user@.service) — without it the save is refused, naming the fix; CPUs \
                         the host does not have online are refused too. When podman cannot be \
                         asked, the save goes through and its 'notes' say so.",
                    ),
                ),
                (
                    "build_args",
                    str_p(
                        "Extra --build-arg lines, KEY=VALUE one per line (e.g. \
                         GGML_CUDA_FA_ALL_QUANTS=ON). Replaces the list.",
                    ),
                ),
                (
                    "keep_layers",
                    bool_p(
                        "Keep podman's layer cache (default off: no multi-GB intermediates \
                         left behind).",
                    ),
                ),
                (
                    "keep_runs",
                    int_p(
                        "After a successful run, keep this many previous runs' images and \
                         remove older unused ones. Negative = keep every run (the default).",
                    ),
                ),
                ("notes", str_p("Free text.")),
                (
                    "delete_images",
                    bool_p(
                        "On delete: also remove this build's images that nothing uses. \
                         Default false.",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__build_run",
            writes: true,
            description:
                "Start a run of a build: fetch its ref and extras, merge, podman build, verify \
                 the image on the GPU, and — when verification passes — move the build's \
                 moving tag to it. Returns job_id and run_id at once; the run takes minutes \
                 (a cold llama.cpp CUDA build about 6, a cached rebuild about 2). Follow it \
                 with lmgw__build_log run_id=<run_id> and lmgw__builds id=<build id> until \
                 live_job_id is null, then read the run's status. One run per machine builds \
                 at a time; a queued one shows phase 'waiting'. If the exact inputs were \
                 already built and verified, the run ends 'up_to_date' without building \
                 (rebuild=true builds anyway). Nothing running is restarted: models whose \
                 image names the moving tag (used_by on lmgw__builds) keep the old image until \
                 recreated with lmgw__container model=<id> action=apply — then \
                 lmgw__local_model_test proves the new build serves. Cancel from the \
                 dashboard's Jobs.",
            props: vec![
                ("id", int_p("The build to run (lmgw__builds).")),
                (
                    "rebuild",
                    bool_p(
                        "Build even when these exact inputs were already built and verified, \
                         pulling newer base images. Default false.",
                    ),
                ),
            ],
            required: &["id"],
        },
        Builtin {
            name: "lmgw__build_check_merge",
            writes: true,
            description:
                "Check whether a build's extras merge cleanly onto its ref, without building: \
                 fetches the ref and every extra into lmgw's git pool, merges them in order in \
                 a throwaway worktree with the same code a run uses, and reports per extra: \
                 merged, already_in_base, merged_upstream (the forge says the PR is merged, so \
                 it is skipped), squash_applied (no shared history — applied as a squash) or \
                 conflict (with the conflicting files). ok=true when nothing conflicts. Takes \
                 seconds on a warm pool; the first fetch of a large repository downloads it. \
                 Listed with the writes because it downloads repositories to disk and runs \
                 git on this machine.",
            props: vec![("id", int_p("The build to check (lmgw__builds)."))],
            required: &["id"],
        },
        Builtin {
            name: "lmgw__container_image_delete",
            writes: true,
            description:
                "Delete a local image by tag or ID (lmgw__container_images). A tag of an image \
                 that has other tags is only untagged; the last tag, or an ID, removes the \
                 image. While a class default, a model override or any container (running or \
                 stopped) uses it, it is refused with the users named and what force would do; \
                 show that to the user and pass force=true only once they confirm. Forced, \
                 every container on the image is stopped (a running model mid-request too) \
                 and removed first, and the class defaults / model overrides naming it are \
                 left naming a missing image (listed in still_named_by). Refused on a dev \
                 instance, which shares production's image store.",
            props: vec![
                (
                    "image",
                    str_p("Image tag (repo:tag) or ID (at least 12 hex)."),
                ),
                (
                    "force",
                    bool_p(
                        "Delete even though it is in use — only after the user confirmed the \
                         users the refusal named. Default false.",
                    ),
                ),
            ],
            required: &["image"],
        },
        Builtin {
            name: "lmgw__container_image_pull",
            writes: true,
            description:
                "Pull a registry image's update: runs `podman pull <image>` as a background job \
                 and returns job_id at once (a CUDA image is gigabytes; the pull takes minutes). \
                 Use the 'reference' of a registry_update with update_available=true from \
                 lmgw__container_images; afterwards that image shows the new ID and \
                 update_available=false. Nothing running is restarted: containers of the old \
                 image keep it until recreated with lmgw__container model=<id> action=apply. \
                 localhost/ images are built here (lmgw__build_run), not pulled. Only registry \
                 references are accepted (optionally docker://-prefixed); podman's archive, \
                 dir and containers-storage transports are refused. The image is pulled under \
                 its fully qualified name (busybox is docker.io/library/busybox:latest), and a \
                 second pull of the same image while one runs returns the running job. Cancel \
                 from the dashboard's Jobs.",
            props: vec![(
                "image",
                str_p(
                    "The registry reference to pull, e.g. ghcr.io/0xshug0/audio.cpp:full-cuda12 \
                     (registry_update.reference on lmgw__container_images).",
                ),
            )],
            required: &["image"],
        },
    ]
}
