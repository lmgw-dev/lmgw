# Container Builds from Git (lmgw) — Design

**Date:** 2026-09-26
**Status:** Implemented 2026-09-26 on branch `feat/backends` (WP0–WP7). v2
folds in an adversarial review against the code and podman 5.8.7; §13 lists
what it changed. §14/§15 are measured/binding against the shipped code; §16
lists what else the implementation did differently from the body.

> Companion to [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
> (per-model containers, per-model image override) and
> [2026-09-21-image-generation-design.md](2026-09-21-image-generation-design.md)
> (the sd.cpp class). Replaces a manual `build.sh` workflow (two hardcoded
> repos, hand-tagged images) with a dashboard feature that covers all
> three engines lmgw runs: llama.cpp (chat + aux), audio.cpp and
> stable-diffusion.cpp.

## 1. Summary

Today every class image is a string someone types into Settings. New
llama.cpp images come from a shell script: it builds one of two hardcoded
repos, tags `localhost/llama-server-cuda:<variant>-latest`, and layers nginx
on top. lmgw can't see where an image came from, can't list the images it
could use, and doesn't notice when the tag it runs changes under it.

The decision: **lmgw gets a Backends page** (the owner's name for it; route `/backends`).

- A **build** is a saved, editable definition: repository (upstream or any
  fork), ref (branch, tag or commit), an ordered list of extras merged on top
  (PRs/MRs of that repo, or branches/commits/PRs from other remotes),
  backend, and build args.
- Running a build is a background job, a **build run**: fetch, merge,
  `podman build`, verify, tag. Its log streams live. The run history is
  kept.
- Every image lmgw builds carries its full provenance in OCI labels.
- The four Settings class-image fields and the four per-model "Image
  override" fields become an **image picker**. Free text still works, and
  the picker suggests the images on the machine, grouped by build.
- Images can be listed, inspected, retagged, deleted (with an in-use check)
  and rebuilt.
- An update check flags builds whose branch or PRs have moved since the
  last run.

The stored value stays a plain image string, so settings and model rows keep
their schema. Picking a build's **moving tag** means "follow this build".
Picking a run's **immutable tag** means "pin this exact image".

"Recipe" is deliberately not used. It already names sd.cpp model recipes
(`image_recipes.rs`, `lmgw__image_recipes`).

## 2. Facts this design rests on (checked 2026-09-26)

### 2.1 The upstream Dockerfiles

| Engine | Repo (default branch) | CUDA Dockerfile | Final stage | Arch build-arg | Submodules |
|---|---|---|---|---|---|
| llama.cpp | `ggml-org/llama.cpp` (`master`) | `.devops/cuda.Dockerfile` (also `vulkan`, `rocm`, `cpu`, …) | `server` (entrypoint `/app/llama-server`) | `CUDA_DOCKER_ARCH` (default `default`) | none |
| ik_llama.cpp | `ikawrakow/ik_llama.cpp` (`main`) | `.devops/llama-server-cuda.Dockerfile` (+ `-vulkan`, `-rocm`, `-intel`) | `runtime` (entrypoint `/llama-server`, not on PATH; ninja; no `GGML_BACKEND_DL`) | `CUDA_DOCKER_ARCH` (default `"86;90"`, **no 89**) | none |
| audio.cpp | `0xShug0/audio.cpp` (`main`) | `.devops/cuda.Dockerfile` (+ `vulkan`, `cpu`) | `full` | `CUDA_DOCKER_ARCH` | `external/audio.cpp-server-frontends` over **SSH** (`git@github.com:…`) |
| sd.cpp | `leejet/stable-diffusion.cpp` (`master`) | `docker/Dockerfile.cuda` (+ `.vulkan`, `.musa`, `.sycl`, plain) | `runtime` (entrypoint `/sd-cli`; lmgw already overrides to `/sd-server`) | `CUDA_ARCHITECTURES` (default empty) | `ggml` (bumped often), `examples/server/frontend`, `thirdparty/libwebp`, `thirdparty/libwebm` (HTTPS) |

Consequences:
- audio.cpp's SSH submodule fails to clone without a GitHub SSH key. Every
  git call gets `-c url.https://github.com/.insteadOf=git@github.com:`.
- ik's default arch list leaves out the 4090 (sm_89), so the arch arg must
  always be passed. The other engines default to "all archs" (slow) or a
  guess.
- Official llama.cpp and audio.cpp accept `APP_VERSION`/`APP_REVISION`/`BUILD_DATE`.
  `build.sh` passes none, so today's `official-latest` says `revision: N/A`.
  `BUILD_DATE` is declared before the runtime `apt-get` layer. A
  wall-clock value would bust that layer on every build, so it gets the
  **commit date**.
- llama.cpp needs `.git` to embed its build number. Excluding `.git` from the
  context (as `build.sh` does, and as layer caching needs) makes
  `--version` report build 0. `CMakeLists.txt:153-157` honours
  `-DLLAMA_BUILD_NUMBER`/`-DLLAMA_BUILD_COMMIT`, which the llama preset
  injects.
- Official llama.cpp builds its web UI in a `node:24` stage. That stage and
  the CUDA build stage are the untagged images each build leaves behind
  (today's: 8.72 GB + 1.99 GB).
- `build.sh`'s sed edits (zlib, ccache on `RUN if …`) only match official
  llama.cpp. ik uses `ninja-build`, `RUN cmake …`, and has no `rm -rf` in its
  runtime stage. The Dockerfile edits are therefore defined per engine
  preset and per Dockerfile, never shared.

### 2.2 lmgw today

- **Class images.** Chat, aux, audio and image each have their own image
  field: `router.image`, `aux_router.image` (its own stored value;
  `default_aux()` only seeds it), `audio.image`, `image.image`. They live in
  the single settings JSON blob. The per-model override is `image TEXT NULL`
  on all four model tables. `ModelRuntime.image` (`runtime/descriptor.rs`)
  resolves it fresh on every start.
- **No reconciler.** A changed image takes effect at the next start/apply.
  Group apply (`recreate_running`, `ops.rs:5566`) recreates every running
  model of the class, so the post-build flow calls `model_apply` per affected
  model instead.
- **Two latent bugs in `--help` probing** (fixed by WP1 on its own):
  1. `Registry::help_text` (`registry.rs:1847`) and `sdcpp_help`
     (`registry.rs:583`) cache by image **string** for the life of the
     process. A rebuilt moving tag keeps the old flag vocabulary until lmgw
     restarts. This is live right now: `router.image` and `aux_router.image`
     are `localhost/llama-server-cuda:official-latest`, lmgw started at
     01:34, and `build.sh` retagged that ref at 01:57.
  2. `help_text` passes no run args (`&[]`, `registry.rs:1866`) and only
     tries `llama-server` and `/app/llama-server`. ik installs
     `/llama-server`, so probing fails for ik images, and `config_warnings`
     (`ops.rs:1497`) swallows the failure.
- **Podman seams.** `runtime::registry::CommandRunner` runs a command to
  completion. `agents::container::Spawner` can also `spawn` with
  line-streamed stdout/stderr, but the only kill it has is dropping the
  status future (`kill_on_drop`, i.e. SIGKILL) (`agents/container.rs:124-131`).
  Both have test fakes.
- **Jobs.** `jobs/mod.rs` has `JobKind`, `JobExecutor`,
  `JobCtx::progress_detail` (throttled to 500 ms), cooperative cancel via
  an `AtomicBool`, the `/api/events` `jobs` frame, and
  `fail_orphaned_jobs` at boot. It has **no queued-waiting state**: `spawn`
  marks the row running immediately (`jobs/mod.rs:476-481`).
- **Dev instances** have no explicit marker. `dev-instance.sh` only sets
  `LMGW_DATA_DIR`, which is a `mktemp -p /tmp` dir (tmpfs), and
  `headless.rs` rewrites `container_prefix`.
- **RPM deps.** `tauri.conf.json:23` `bundle.linux.rpm.depends` is `[]`;
  podman isn't declared there either.

### 2.3 Host facts

Assumptions about the build host, kept generic:

- `/tmp` may be a tmpfs, so `build.sh`'s `/tmp/llama-cpp-build-*` clone can
  live in RAM.
- The existing ccache is a buildah cache mount under `/var/tmp`.
  **systemd-tmpfiles deletes it after 30 days idle.**
- An interrupted build can leave a large `/var/tmp/buildah<N>` orphan behind.
- The podman store can hold hundreds of GB of images, most of it reclaimable
  and some dangling.
- git with `git merge-tree --write-tree`, and a podman new enough for
  `--ignorefile`, out-of-context `-f`, `--label`, `--iidfile` and
  `--cpuset-cpus`.
- Builds need a CUDA-capable driver on the host.
- A desktop session may set `SSH_ASKPASS` (for example to `ksshaskpass`), and
  global git config may carry a credential helper for GitHub. A background git
  call would pick up both. §10 neutralises them.

## 3. Concepts

- **Engine preset** (static data in code, one per engine: `llama`, `audio`,
  `sdcpp`) holds:
  - repo presets;
  - per repo and backend: Dockerfile candidates, final-stage candidates, arch
    arg name, version args, Dockerfile edits (ccache wiring, zlib, build
    number), and smoke-test binary candidates;
  - which lmgw classes may use the image: `llama` → chat + aux, `audio` →
    audio, `sdcpp` → image.

  ik_llama.cpp is the `llama` engine with its own repo preset and its own
  edits.
- **Build** (`builds` row, editable): what to build. §4.
- **Build run** (`build_runs` row, append-only): one execution. It records
  the resolved inputs (base SHA, each extra's resolved SHA, config hash),
  status, image ID, tags, size, duration and log path. A deleted build keeps
  its runs, with `build_id` set to NULL.
- **Image**: podman is the source of truth. lmgw stores no image table. The
  Images view joins `podman images` with runs through the `dev.lmgw.run`
  label. Unlabeled images (the `build.sh` ones, ghcr pulls) show as
  *external*.

## 4. Build fields

| Field | Notes |
|---|---|
| `name`, `slug` | The slug becomes the tag. It is validated visibly (tag charset `[a-z0-9._-]`, and the full tag must fit 128 chars) and is **immutable once the build has a run** |
| `engine` | `llama` \| `audio` \| `sdcpp` |
| `repo_url`, `forge` | A preset or any git URL. `forge` is `github` (auto for github.com), `gitlab` (auto when a token is configured for the host, otherwise chosen) or `plain` |
| `ref` | Branch, tag or commit. Suggestions come from `git ls-remote --heads --tags`; annotated tags compare the peeled `^{}` SHA. The bare `ls-remote` would list tens of thousands of `refs/pull/*` |
| `extras` | Ordered list; each entry is one of:<br>• `{kind: pr, number, pin?}`, a PR/MR of `repo_url`<br>• `{kind: ref, remote_url, ref, pin?}`, a branch or commit of another remote (a fork branch, an upstream llama.cpp PR on top of ik via `refs/pull/N/head`, …)<br>With no pin, a run follows the head. A pin builds that exact SHA |
| `backend` | `cuda` (default) \| `vulkan` \| `rocm` \| `cpu`. Picks the Dockerfile candidate |
| `cuda_version` | Prefilled from the preset (`13.0.0` for llama, matching `build.sh`; upstream default for the others). The editor shows the driver's max CUDA version and warns if the build asks for more |
| `arch` | Default **auto**: the unique `compute_cap` values of the host GPUs (`89` here), shown resolved. Can be set to a list for another machine's card |
| `dockerfile`, `target` | Default auto (the preset's candidates). The override field appears, prefilled, when auto finds nothing |
| `edits` | Starts as the preset's edits for that Dockerfile, shown and editable. Each edit is a find/replace applied to a **copy**, with a `required` flag |
| `ccache` | Default on. `CCACHE_MAXSIZE` is a visible field (default `10G`, as `build.sh`). The cache id is `lmgw-<engine>-<backend>` for plain builds and `lmgw-<engine>-<backend>-<slug>` for builds with extras, so PR code never writes into the cache master builds read |
| `cpus` | `--cpuset-cpus` range. Empty means all cores. This caps `nproc` inside the build, so `-j$(nproc)` follows |
| `build_args` | Extra `KEY=VALUE` lines (e.g. `GGML_CUDA_FA_ALL_QUANTS=ON` for sd.cpp) |
| `keep_layers` | Default off (`--layers=false`): no multi-GB intermediates are left behind. Rebuild speed comes from ccache and the §5 "already built" check. On: keep the layer cache |
| `keep_runs` | How many previous runs' immutable tags to keep. Empty means keep all. The editor shows a default of 3 |
| `notes` | Free text |

"Edit an image" means edit its build and run it again, or retag or annotate
a run. Image contents can't be edited in place.

**Duplicate** copies a build. That's the quick way to get `official-master`
and `official-master-pr16391` side by side.

## 5. The run pipeline (`JobKind::BuildRun`, key = build id)

**Serialization.** One run at a time per machine, enforced by a `flock` on
`$XDG_RUNTIME_DIR/lmgw-build.lock`, so it also covers a dev instance. A run
that has to wait still shows as running (the jobs system has no queued
state); its detail says "waiting for <build>", and it polls the cancel flag
while it waits.

**Workspace.** A persistent bare mirror per remote URL lives in
`<builds_dir>/git/<hash(url)>.git`. Each run gets its own
`git worktree add --detach <builds_dir>/work/<run-id>`, which is removed when
the run ends. A run therefore never shares a tree with another run or with
**Check merge**. `builds_dir` is a visible setting, default
`<data_dir>/builds`. A dev instance must set it (§10), because its data dir
is on tmpfs.

Phases (each one is shown in the job detail and the log):

1. **Resolve and fetch.** Resolve the ref and each unpinned extra to a SHA;
   `pr` extras also get forge state: `merged_at`, closed, draft. Then
   `git fetch` exactly those refs (`refs/pull/<n>/head` or
   `refs/merge-requests/<n>/head` into `refs/lmgw/…`; other remotes by URL).
2. **Short-circuit.** Compute the immutable tag (below). If an image with that
   tag already exists and is verified, stop with **up to date**, unless
   **Rebuild anyway** was pressed. A rebuild uses `--pull=newer`, because
   `node:24` and the CUDA base tags move.
3. **Assemble.** In the worktree:
   - Merge the extras in order (`merge --no-ff --no-edit`, committer
     `lmgw <lmgw@localhost>`). A `pr` extra with `merged_at` set is skipped
     as "merged upstream". llama.cpp squash-merges, so its head is never an
     ancestor of master.
   - A merge whose diff against its first parent is empty
     (`git diff --quiet HEAD^1 HEAD`) is logged as "already in base".
   - On a conflict: `merge --abort`, then fail, naming the extra, the
     conflicted files and the extras merged before it.
   - **Only after all merges:** `submodule sync --recursive`,
     `submodule update --init --recursive --force`, and
     `submodule foreach --recursive git clean -ffdx`. A PR that bumps a
     submodule pointer (sd.cpp's `ggml`) builds against the bumped
     submodule.
4. **Prepare.** Copy the Dockerfile to `<builds_dir>/work/<run-id>.ctx/Containerfile`
   (outside the tree) and apply the edits there. Each edit is logged as
   applied or not matched; a `required` edit that doesn't match fails the
   run. An ignore file (`.git`, `.github`, `build/`, `.cache/`) goes next to
   it and is passed with `--ignorefile`.
5. **Build.** Runs through `Spawner::spawn`:

   ```
   podman build --target <stage> -f <copy> --ignorefile <file> --layers=<keep_layers>
     --build-arg CUDA_VERSION=… --build-arg <arch-arg>=…
     --build-arg APP_VERSION=… --build-arg APP_REVISION=<base sha> --build-arg BUILD_DATE=<commit date>
     [--cpuset-cpus …] --label dev.lmgw.*=… --iidfile … -t <immutable> <worktree>
   ```

   Every line goes to `<builds_dir>/logs/<run-id>.log`: the whole log,
   never truncated, and kept after the run. Progress comes from podman's
   `STEP n/m:` lines and from cmake/ninja's `[ 45%]` / `[123/456]`.
6. **Verify.** Two checks, both through `Registry::run_throwaway` with the
   class's `extra_run_args`:
   - `--help` from the preset's binary candidates (the image's Entrypoint
     first). This primes the flag-vocabulary cache for the new image ID.
   - `--list-devices` (llama; the equivalent where the engine has one), which
     must report a device of the build's backend. This matters because
     official builds use `GGML_BACKEND_DL=ON`: `--help` succeeds even when
     the CUDA backend can't load (CUDA newer than the driver, wrong arch).

   A failed check marks the run **built, broken**, and it is not promoted.
   Under GPU hold, or on a CUDA OOM while probing, the run is marked **not
   GPU-verified** instead, with a **Verify now** action for later.
7. **Promote.** A verified run moves the build's moving tag. Then:
   - drop the old image ID's help-cache entries;
   - apply `keep_runs`: old immutable tags that nothing uses get `podman rmi`,
     and each one kept is logged with the reason;
   - record the image ID and size.

**Cancel and failure cleanup.**
- `Spawner` gains `kill(signal)`. Cancel sends SIGTERM to `podman build`,
  and SIGKILL after a grace period.
- After a cancel or failure, the run removes its buildah working containers
  (`buildah`/`podman ps --external`, filtered to this run), the untagged
  intermediates it created (by image ID from the build log) and its
  worktree.
- At boot, runs left `running` are marked failed, next to
  `fail_orphaned_jobs`.
- Only this run's own leftovers are removed. The existing 35 GB orphan (§2.3)
  is reported, not touched.

**Tags.**
- Moving: `localhost/lmgw-<engine>:<slug>`.
- Immutable: `localhost/lmgw-<engine>:<slug>-<base7>-<cfg6>`. `cfg6` hashes
  every input that changes the image: the resolved SHA of each extra,
  backend, CUDA version, resolved arch list, Dockerfile path, target, edits,
  build args and ccache settings. The same inputs always give the same tag,
  which is what makes the step-2 short-circuit sound.
- `<engine>` is `llama-server`, `audio-cpp` or `sd-server`.
- The existing `localhost/llama-server-cuda:*` tags are not touched.

**Labels.** Each run is stamped with `dev.lmgw.run`, `.build`, `.engine`,
`.repo`, `.ref`, `.base`, `.extras` (JSON with resolved SHAs), `.backend`,
`.arch`, `.cuda` and `.cfg`, plus `org.opencontainers.image.{source,revision,version,created}`.
The Images view and the picker read only the labels, so an image stays
self-describing even after its row is gone.

## 6. After a run: the running models

A run never recreates a running container on its own. The success panel
lists who uses the moving tag: class defaults (all four fields), model
overrides, and running containers (`podman ps --filter ancestor=<old id>`).
It offers **Recreate N running containers**, which calls `model_apply` once
per affected model, never group apply. When the image isn't a default yet,
it also offers **Set as default for …**.

**Rollback.** Every run in the history has **Make current**. It moves the
moving tag back to that run's image, with the same recreate offer.

## 7. Forge integration (the extras picker)

- **GitHub:**
  - Open PRs: `GET /repos/{o}/{r}/pulls?state=open&sort=updated&per_page=100`,
    paged on scroll and filtered client-side.
  - Details (head SHA, state, `merged_at`, draft): `GET /repos/{o}/{r}/pulls/{n}`.
  - Adding by number or pasted URL always works.
- **GitLab** (e.g. `git.example.com`):
  `GET /api/v4/projects/<path>/merge_requests?state=opened&search=` and
  `…/merge_requests/<iid>`.
- **Plain:** no PR list. Refs from other remotes only.
- **Tokens:** a new `forge_tokens: {host → token}` setting, redacted like
  `hf_token`, with a `github.com` slot.
  - API calls: the token goes only to that exact host.
  - Fetching private repos: it is passed as `http.<url>.extraHeader`
    through `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n` env vars, never in argv
    or logs.
  - Without a token GitHub allows 60 requests/h. When the limit is hit, the
    picker shows the reset time instead of an empty list.
- **Check merge** (editor button): fetches, then runs
  `git merge-tree --write-tree` for each extra in order against the base. It
  touches no worktree and takes seconds. It returns "merges cleanly" or the
  conflict report, plus forge state per PR.

## 8. Update detection

- A periodic check runs at a visible interval setting (default 6 h, 0 = off),
  plus **Check now** per build and globally.
- For each build it resolves the ref and each unpinned extra, then compares
  them with the last verified run. A commit ref never updates.
- The build row gets a badge with the reason:
  - "master +37 commits"
  - "PR #1234 pushed"
  - "PR #1234 merged upstream, drop it?"
  - "PR #1234 closed unmerged"
- The nav item shows the count, following the Downloads badge pattern.
- Registry images in use (the ghcr audio/sd defaults) get the same check
  against the registry manifest digest (anonymous token flow), with a
  **Pull update** action. This covers "the pulled audio.cpp image predates
  the new routes".

## 9. Surfaces

### 9.1 UI

- **Nav:** "Backends" in the *Serve* group, route `/backends` (the owner's
  choice, 2026-09-26). Not "Images", because "Image" is the sd.cpp class.
  On this page the per-build `backend` field is labelled **GPU backend**,
  so it doesn't read as the page's own subject.
- **Backends page** (`PageMode::Fill`, two tabs):
  - **Builds tab.** One row per build:
    - Columns: engine chip, repo, ref, extras chips, last run status, age,
      update badge, used-by count.
    - Actions: Run, Edit, Duplicate, Check merge, Delete (`ConfirmButton`,
      with the choice to delete its images too).
    - Expanding a row shows the run history: status, duration, base SHA,
      extras, size, tag. Per run: View log / Make current / Verify now /
      Retag / Delete image.
  - **Images tab.** Every local image of the three engines (lmgw-built,
    external, registry):
    - Columns: ref, engine, backend, size, created, provenance, used by.
    - Actions: set as class default; delete (while in use: a confirmation
      listing the users and what deleting anyway does, then a forced
      delete); pull update (registry refs).
    - Footer: `podman system df` totals and any orphaned `/var/tmp/buildah*`
      dirs, shown for information. Nothing system-wide is pruned from here.
- **Build editor** (Modal):
  - Repo preset (official llama.cpp, ik_llama.cpp, audio.cpp, sd.cpp,
    custom URL).
  - Ref combobox.
  - Extras picker: searchable PR list plus "add ref from another remote".
    Chips are reorderable and show state and resolved SHA, with a pin
    toggle. A note next to it: building a PR runs that PR's build code.
  - Backend, CUDA/arch (with the auto values shown).
  - Advanced: the rest of §4.
  - Tag preview, free disk space next to the last run's footprint (a
    warning only, never a block), and Check merge.
- **Run log panel:**
  - Live tail via `GET /api/builds/runs/<id>/log?offset=`, polled only while
    the run is live.
  - Phase and progress bar, Cancel.
  - The §6 panel when the run finishes.
- **ImagePicker**: a new small widget on `popover::Popover` + `filter_words`.
  `ModelPicker` is tied to the model catalog, so it isn't generalized.
  - Used in the four Settings class fields (a new `Ctl` variant in
    `settings.rs`) and the four per-model override fields.
  - Suggestions: builds (moving tag first, labelled "follows build", then
    pinned runs), then external/registry images filtered to the class's
    engine.
  - Free text still works.
  - Each value shows local / missing / broken / not GPU-verified, and warns
    when the image's backend label doesn't match the class (e.g.
    `audio.backend`).

### 9.2 Ops, API, MCP

- **Ops** (each an `op()` arm plus an `ops.rs` function): `builds`,
  `build_set` (create/update/delete/duplicate), `build_run`, `build_runs`,
  `build_run_log`, `build_check_merge`, `build_promote`, `build_verify`,
  `build_updates_check`, `container_images`, `container_image_delete`,
  `container_image_tag`, `forge_prs`, `forge_refs`. Cancel reuses
  `job_cancel`.
- **MCP:** `lmgw__builds` (read), `lmgw__build_set`, `lmgw__build_run`,
  `lmgw__container_images` (read), `lmgw__container_image_delete`,
  `lmgw__forge_prs` (read). Mutating tools are gated by `self_admin=full`.
  With these an agent can do "build master + PR 16391, test model X on it,
  report back".

## 10. Security and robustness

- Building a PR runs that PR's CMake and Dockerfile, with the same trust as
  `build.sh` (rootless podman, no GPU, network on). What `build.sh` didn't
  have, a shared ccache, is split per build (§4).
- git and podman always get argv, never a shell. URLs and refs are validated
  (no leading `-`, no whitespace, known schemes only).
- Every git call runs with `GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS=` (empty),
  `SSH_ASKPASS=` (empty), `-c credential.helper=` and the SSH→HTTPS rewrite.
  No KDE password dialogs, no implicit `gh` credentials.
- **Dev instance.** An explicit `AppState` dev flag, set by
  `dev-instance.sh`:
  - it refuses image deletion and `keep_runs` pruning, because the image
    store is shared with prod;
  - it refuses to run builds until `builds_dir` points off tmpfs.
- **git dependency.** Add `git` to `bundle.linux.rpm.depends`
  (`tauri.conf.json:23`), with podman while at it. There is also a runtime
  `git --version` check with a visible error, because AppImage can't declare
  dependencies.

## 11. Decisions to take (recommendations)

1. **nginx front layer: drop it, and add no generic "extra layer" field.**
   lmgw is already the single front door and proxies only the API. The
   wrapper holds its port closed until the model has loaded and rewrites
   `--host`/`--port`.
2. **Tag namespace: new `localhost/lmgw-<engine>:*`.** The
   `localhost/llama-server-cuda:*` tags stay as they are until the owner
   switches the default via the picker.
3. **Pruning the existing 450 GB and the 35 GB `/var/tmp` orphan: not part
   of this feature.** The Images tab shows both. Cleaning them is a one-off
   done by hand, on the owner's say-so.

## 12. Scope and work packages

**v1 (proposed):** WP0–WP5 plus WP7. **v1.1:** WP6.

**Later (optional):** per-build auto-apply; scheduled nightly runs; a
canary test ("run model X on the new image before promoting"); export/push
to another host (push only ever by explicit click); Vulkan/ROCm builds for
the AMD laptop (the presets allow it, untested); a tray notification when a
run ends.

- **WP0 spike** (heavy CPU/RAM, needs a go). Hand-run §5 for official
  master and confirm:
  - `--label` reaches the final image;
  - `id=` on cache mounts;
  - `--layers=false` leaves no stage images;
  - what SIGTERM leaves behind;
  - build-number injection;
  - the audio SSH rewrite;
  - sd.cpp submodules after a gitlink-bumping merge;
  - ik with CUDA 13 on its ubuntu 22.04 base;
  - `--list-devices` output.

  Record the RAM peak at `-j32`.
- **WP1** help probing:
  - key the cache by image **ID**, for both `help_text` and `sdcpp_help`;
  - take binary candidates from the image's Entrypoint;
  - pass the class's run args;
  - surface probe failures instead of swallowing them.

  Ships first, on its own.
- **WP2** core:
  - migration `0040_container_builds.sql` (`builds`, `build_runs`);
  - engine presets;
  - git ops (mirror, worktrees, merge, submodules, env hardening);
  - the `BuildRun` executor with flock, logs, verify, promote, retention,
    cleanup and the boot sweep;
  - `Spawner::kill`;
  - delete with the in-use check;
  - the dev flag.
- **WP3** forge: GitHub/GitLab list and get, `ls-remote` refs,
  merge-tree check-merge, `forge_tokens`.
- **WP4** ops, API and MCP tools.
- **WP5** UI: Backends page, build editor, extras picker, log panel,
  ImagePicker in the eight fields, the §6 panel.
- **WP6** update detection (git and registry digests), nav badge.
- **WP7** docs (a user doc page, a `docs/amd-arch.md` note, release notes),
  RPM depends, and a `ui-matrix`/`webkit-check` pass on `/backends`.

## 13. What the review changed (v1 → v2)

- Submodules now update **after** the merges.
- "Already in base" is detected via forge `merged_at` and an empty merge
  diff, not ancestry.
- The immutable tag hashes the full config; slugs are validated.
- Each run gets its own worktree, and check-merge uses `merge-tree`.
- Cancel is SIGTERM via a new `Spawner::kill`, followed by cleanup and a
  boot sweep.
- Verify adds `--list-devices`; WP1 also fixes the ik probe.
- `BUILD_DATE` is the commit date; build number is injected.
- Edits are per Dockerfile; the ccache size is visible and PR builds get
  their own ccache.
- A flock serializes runs instead of an in-process queue.
- The git environment is hardened.
- The dev flag is explicit.
- `model_apply` is called per model instead of group apply.
- The ImagePicker is a new widget.
- Extras can come from other remotes.
- Cut: `cpus` replaces `memory_limit`; the `extra_layer` field, the GitHub
  search API, GitLab auto-probing and the system-wide prune button are gone.

## 14. WP0 spike results (measured 2026-09-26) — authoritative where they contradict the body

Raw evidence (logs, `free -m` samples, the exact Containerfiles and
`edits.json` per engine) is in `~/.cache/lmgw-spike/{logs,ctx}`. The four
spike images are kept as `localhost/lmgw-spike:{official,ik,audio,sd}` for
dev testing. Cold build wall times at sm_89: official about 350 s, ik 404 s,
audio 362 s, sd 248 s. Peak RAM added about 8–12 GB each (minimum available
14–16 GB of 62). A cache-hit rebuild of official took 123 s, of which the
compile was 13 s (609/610 ccache hits). The rest was the web UI (47 s), apt
(41 s) and commit.

### 14.1 Git

- **One object pool, not a mirror per URL.** All remotes are fetched into a
  single bare repo, `<builds_dir>/git/pool.git`, with namespaced refs
  `refs/lmgw/<hash(url)>/{heads,tags,commits,pr}/…`. ik and llama.cpp
  then share blobs, so fetching llama.cpp objects for an ik build
  transfers 0 objects. It replaces §5's "bare mirror per remote URL".
- **Refspecs:**
  - branch: `+refs/heads/B:refs/lmgw/<h>/heads/B`
  - annotated tag: compare `rev-parse 'ref^{}'`
  - commit: the full 40-hex SHA only (short SHAs can't be fetched, so resolve
    them via `ls-remote` or reject them visibly)
  - GitHub PR: `+refs/pull/N/head:…`; GitLab: `refs/merge-requests/N/head`

  Always fetch with `--no-tags`. `ls-remote --heads --tags` takes 0.5 s for
  8435 refs.
- **Build number** (llama engines) = `rev-list --count <base>`. For official
  llama.cpp this equals the upstream `bNNNN` tag. `APP_VERSION=b<number>`.
- **Unrelated histories.** An upstream llama.cpp PR on top of ik can't be
  merged ("refusing to merge unrelated histories"). An extra whose head
  shares no merge base with the assembled HEAD is applied as a
  **squash-apply**: `merge-tree --write-tree --merge-base=<PR fork point in its
  own repo> HEAD <head>`, then `commit-tree`. The fork point is the forge
  PR's `base.sha` merge-base, or `merge-base <head> <remote default
  branch>` for plain refs. The mode is chosen automatically and shown in the
  log. It can conflict like a merge.
- **Squash-merged PRs.** Merging a recently merged PR gives a tree identical
  to base ("already in base"). An older one **conflicts**. So the forge's
  `merged_at` is the primary signal. `merged_at` set means skip, with an
  update badge "merged upstream". `tree == base^{tree}` after a merge catches
  the rest.
- **Submodules, corrected order:** worktree at base →
  `submodule update --init --recursive` **at base** → merges → `submodule sync
  --recursive` → `submodule update --init --recursive --force` → `submodule
  foreach --recursive git clean -ffdx`. merge-ort can fast-forward a bumped
  gitlink (sd.cpp's ggml) only while the submodule is checked out. In a bare
  repo every real ggml-bumping sd.cpp PR reports a gitlink conflict.
  **Check merge therefore uses a throwaway worktree too, running the same
  code as the run's assemble phase, never bare `merge-tree`.** This
  supersedes §7.
- Per-worktree submodule clones live in `pool.git/worktrees/<wt>/modules`.
  They are re-cloned every run (sd.cpp: 7 s, about 50 MB). Pass
  `--reference` to a persistent submodule pool where cheap.
  `worktree remove --force` is needed when submodules exist.
- `-c url.https://github.com/.insteadOf=git@github.com:` on the parent
  command propagates to submodule clones. `-c credential.helper=`
  suppresses the URL-scoped `gh` helper too (verified with GIT_TRACE).

### 14.2 Podman build

- `--ignorefile` **replaces** the in-repo `.dockerignore`. Patterns must be
  `**/.git` (sd.cpp's submodules each have a `.git` file), plus `.github`,
  `build/` and `.cache/`.
- **Labels** and `APP_*`/`BUILD_DATE` land on the `--target` image.
  `--layers=false` leaves only the final image; intermediate stages are
  removed at the end.
- **Same inputs give a new image ID** (the build isn't reproducible). A
  "Rebuild anyway" onto the same immutable tag therefore leaves the old image
  dangling. The run removes it when nothing uses it.
- **ccache:** `id=` works. The cache dir is
  `/var/tmp/buildah-cache-<uid>/<first16hex(sha256("<id>:0:0"))>`. Being under
  `/var/tmp`, an idle one is removed by the host's `systemd-tmpfiles` after 30
  days, same as any other `/var/tmp` entry — worth surfacing next to a cache
  dir's size in the UI.
  Per engine:
  - official: the ccache package plus a mount on `RUN if [ "${CUDA_DOCKER_ARCH}"…`
  - ik: the package in `apt-get install -y build-essential git`, plus the
    mount on `RUN cmake -S . -B build -G Ninja`
  - audio: forces `GGML_CCACHE OFF`, so it additionally needs
    `-DCMAKE_{C,CXX,CUDA}_COMPILER_LAUNCHER=ccache`
  - sd: already installs ccache; only the mount on `RUN cmake --build ./build …`

  An optional npm cache mount (`id=lmgw-llama-npm`) on official's
  `RUN npm ci` shortens the web UI stage.
- **Build-info injection** differs per engine:
  - official: `ARG LLAMA_BUILD_NUMBER/COMMIT` plus `cmake -B build
    -DLLAMA_BUILD_NUMBER=… -DLLAMA_BUILD_COMMIT=…` (verified:
    `version: 0.5.0-dev (build 11192, commit 171e884)`)
  - ik: `-D` is ignored (`CMakeLists.txt:169` sets it unconditionally), so an
    in-container `sed` of `cmake/build-info.cmake` goes in the compile RUN
    (verified: `version: 4960 (1aaf710)`)
  - audio: `sed` of `set(AUDIOCPP_GIT_SHA/DATE "unknown")` in `CMakeLists.txt`
  - sd: same kind of `sed` (verified in the e2e of 2026-09-26: `version
    b920, commit 2f88688`)

  The exact edits are in `~/.cache/lmgw-spike/ctx/<engine>/edits.json`.
  Edits are replace-all. Presets template the ccache id and max size
  instead of hardcoding them. `build.sh`'s **zlib seds are dead**: they no
  longer match, and CMake gzips the UI itself. Drop them.
- **Short names:** ik and sd use `nvidia/cuda:…`. That only resolves here
  through a cached user alias; elsewhere short-name mode "enforcing" with no
  TTY fails. Every preset with a bare base image carries a (non-required)
  `docker.io/` qualify edit.
- **CUDA version:** 13.0.0 works for all four engines on driver 615.71.
  Using it everywhere shares base images: no 10 GB pull of 12.9.2 for audio,
  and no 12.6.3-cudnn for sd. sd.cpp needs no cuDNN, so its preset carries
  a non-required `-cudnn-` → `-` edit. Preset default: `13.0.0` for all
  engines, with a visible field and the driver-max warning.
- **Cancel:**
  - SIGTERM: podman exits within 0.5 s (rc 1), no compile processes survive
    and nothing is committed.
  - SIGKILL (rc 137): the same, plus overlay mounts left in the rootless
    namespace.
  - Leftovers in both cases: one buildah working container per started stage
    (status `Storage`, unlabeled) and `/var/tmp/buildahNNN`.
  - Cleanup (under the flock): diff `podman ps -a --external -q` before and
    after, `podman rm -f` the new ones (inspect fails on them, rm works), then
    `podman unshare rm -rf` the new `/var/tmp/buildah*` dirs.
  - The partial compile stays in ccache.
- **PR builds read the shared cache** (measured 2026-09-26, supersedes the
  "PR builds compile cold" consequence of §4). A build with extras still
  compiles into its own `lmgw-<engine>-<backend>-<slug>` cache, but its
  compile `RUN` also mounts the plain `lmgw-<engine>-<backend>` cache
  read-only at `/ccache-shared` and runs `cp -a -u --reflink=auto
  /ccache-shared/. /ccache/` before `ccache -z` (template keys
  `{{ccache_shared_mount}}` and `{{ccache_seed}}`, which render empty for
  plain builds, so those stay byte-identical to the spike's).
  - buildah honours `ro` on cache mounts (a write fails), and an absent
    shared id mounts as an empty dir, so the seed is a no-op.
  - The copy reflinks across the two mounts on this btrfs `/var/tmp`: 0.23 s
    for the 572 MB, 2531-file llama cache and 0.03 s for a refresh with
    nothing new, on both coreutils 8.32 (ik's 22.04) and 9.4. It runs every
    run, so a PR build also picks up what later master builds added.
  - Official master + PR #29448: 608/610 hits (99.67 %), 117 s wall, against
    129 s for plain master on the same cache. The PR run wrote its 4 new
    entries to its own cache, and the shared cache's path/size/mtime digest
    was unchanged.
  - Rejected: ccache's remote `file:` backend (`CCACHE_REMOTE_STORAGE` /
    `secondary_storage`) with `read-only`. It cannot read the local cache
    layout (`a/b/<key>R|M`): 0 hits with `layout=subdirs` and `layout=flat`
    (ccache 4.12), so it would need the master builds to keep a second copy
    in the remote layout.
  - **npm follows the same pattern** (implemented alongside this note, not
    measured in the spike itself): a build with extras still compiles its web
    UI stage into its own `lmgw-npm-<engine>-<slug>` cache, but that `RUN npm
    ci` also mounts the plain `lmgw-npm-<engine>` cache read-only at
    `/npm-shared` and seeds its own from it first — same `cp -a -u
    --reflink=auto`, same template shape (`{{npm_shared_mount}}` /
    `{{npm_seed}}`, empty for a plain build). Supersedes the spike's "not
    changed", which only measured ccache; the npm mount is the same mount
    type, so the same seed is safe.
  - **Per-build cache dirs are cleaned up on delete.** `build_set` delete
    removes the deleted build's own ccache and npm cache directories (the
    slug-suffixed ids only — never the shared, non-suffixed ones, never
    another build's), reported as `removed_caches` in the response. Skipped
    on a dev instance: its `/var/tmp` is production's.

### 14.3 Verify (supersedes §5 step 6)

Exit codes are useless here: official and audio exit 0 with no GPU, and ik's
`--help` exits 1. **Verify parses output, always with the class's GPU run args.**

| Engine | Help | Device probe | Success pattern |
|---|---|---|---|
| official llama | `/app/llama-server --help` | `/app/llama-server --list-devices` | `^\s+CUDA\d+:` (Vulkan/ROCm: backend name) |
| ik | `/llama-server --help` (rc 1) | `/llama-server -m /nonexistent.gguf` (no `--list-devices`) | `ggml_cuda_init: found [1-9]\d* CUDA devices` |
| audio | `server --help` via the dispatcher entrypoint | `server --list-devices` | `CUDA:\d+ ` |
| sd | `/sd-server --help` | `/sd-server --list-devices` | `^CUDA\d+\t` |

Without GPU args, official and audio list only `(none)`/`CPU:0`, and ik
and sd fail on `libcuda.so.1` (rc 127). This is the "broken backend" case
verify exists to catch.

### 14.4 Host

The pre-existing `/var/tmp` orphans, buildah `Storage` working containers
and dangling images were removed by hand on 2026-09-26. A periodic prune
outside lmgw is the recommended upkeep: dangling images always, and, when no
build is running, `Storage` containers and `/var/tmp/buildahNNN` dirs older
than a day. The ccache cache mounts are never touched.

## 15. API contract (binding for WP2b, WP4 and WP5)

All ops are `POST /api/op/<name>` with a JSON body. Types come from
`lmgw-api-types` (`builds.rs`; new response types are added there too).
Errors use the existing op error envelope. MCP tools wrap the same ops
(§9.2).

| Op | Args | Returns |
|---|---|---|
| `builds` | `{}` | `{builds: [BuildView]}`. `BuildView = {build: Build, last_run: Option<BuildRun>, current_run: Option<BuildRun>, live_job_id: Option<i64>, used_by: [ImageUse], update: Option<UpdateStatus>}` (`current_run` is the promoted one) |
| `build_get` | `{id, limit?, before?}` | `{view: BuildView, runs: [BuildRun], more: bool}`, newest first. `limit` is explicit and the UI pages with ShowMore |
| `build_set` | `{action: create\|update\|delete\|duplicate, id?, spec?, slug?, name?, delete_images?}` | `{build: Option<Build>, removed_images: [String], kept: [{tag, reason}], removed_caches: [String]}` |
| `build_resolve` | `{spec}` | `ResolvedPreview {base_sha, build_number?, dockerfile, target, profile, edits: [BuildEdit + role], build_args: [(k,v)], moving_tag, immutable_tag, warnings: [String]}`. Editor "Resolve" button; fetches but doesn't build |
| `build_check_merge` | `{id? , spec?}` | `CheckMergeReport {base_sha, ok, steps: [{label, outcome: merged\|already_in_base\|merged_upstream\|squash_applied\|conflict, sha, files: [String], note}]}` |
| `build_run` | `{id, rebuild?: bool}` | `{job_id, run_id}` |
| `build_run_log` | `{run_id, offset}` | `{text, next_offset, done}`. Byte offset into the run's log file; the UI polls only while `done=false` |
| `build_promote` | `{run_id}` | `{moving_tag, from_image: Option<String>, to_image, used_by: [ImageUse]}` ("Make current") |
| `build_verify` | `{run_id}` | `VerifyReport {help_ok, devices: [String], gpu_verified, notes: [String]}` and updates the run's status |
| `build_env` | `{}` | `{repo_presets, cuda_default, arch_auto: [String], driver_cuda_max: Option<String>, git_ok, builds_dir, builds_dir_warning}` for the editor |
| `forge_refs` | `{repo_url}` | `{default_branch, heads: [{name, sha}], tags: [{name, sha}]}` |
| `forge_prs` | `{repo_url, forge, query?, page?}` | `{prs: [ForgePr], next_page: Option<u32>, rate_limit: Option<{remaining, reset_at}>}`. `ForgePr {number, title, author, updated_at, draft, state, head_sha, base_sha, merged_at, url}` |
| `forge_pr` | `{repo_url, forge, number}` | `ForgePr` |
| `container_images` | `{engine?}` | `{images: [ContainerImage], disk: {images_total, reclaimable, buildah_orphans: [String]}}`. `ContainerImage {id, tags, engine: Option<Engine>, backend: Option<String>, size, created, provenance: Option<{build_id, run_id, slug, repo, git_ref, base, extras}>, external, used_by: [ImageUse], run_status: Option<BuildRunStatus>}` |
| `container_image_delete` | `{image}` (tag or ID), `force?: bool` | `{removed: [String], removed_containers: [String], still_named_by: [ImageUse]}`. While `used_by` is non-empty: refused without `force` (the error lists the users and what forcing does); with `force` (the user confirmed) every container on the image is stopped and removed first and config users are left naming a missing image (`still_named_by`). Refused on a dev instance either way |
| `container_image_tag` | `{image, add?: String, remove?: String}` | `{tags}` |
| `build_updates_check` | `{id?}` | `{updates: [{build_id, update: Option<UpdateStatus>}]}` (WP6) |

- `ImageUse = {kind: class_default|model_override|running_container|stopped_container, class, model_id?, container?}`.
  `stopped_container` is any container `podman ps -a --external` lists that is not running (exited, created, a buildah working container): it blocks `podman rmi` just like a running one.
- `UpdateStatus = {checked_at, reasons: [String], ref_moved: bool, extras: [{label, change}]}`.
- **Jobs feed:** `JobRow.kind = "build_run"`, `key = "build:<id>"`,
  `detail = {run_id, build_id, phase, step: Option<"n/m">, percent, last_line, waiting_for: Option<String>}`.
  The phases are `waiting | resolve | fetch | assemble | prepare | build |
  verify | promote | cleanup`.

## 16. Implementation notes (what shipped differs from the body, beyond §14/§15)

The build's Dockerfile/target is chosen **at the base commit**, before any
extra is merged (not on the assembled tree), so the immutable tag can be
hashed — and the step-2 short-circuit checked — before anything is fetched or
merged; *Prepare* then re-reads the same path from the assembled tree and
fails visibly if an extra moved or removed it or its target stage. The
up-to-date short-circuit is stricter than "an image with that tag exists": it
requires a **succeeded run of this same build** recorded against that exact
image ID and tag. `build_verify` ("Verify now") itself promotes — moving tag,
help-cache drop, retention — when the re-verified run turns `succeeded` and no
newer run of its build is already current; `build_promote` ("Make current" /
rollback) runs **no retention**, so rolling back can never prune a newer run.
The forge client (`GatewayForge`) holds a `Weak<AppState>`, installed once at
startup, rather than an `Arc`. The update check's results are persisted to one
`settings` row (key `backends:update_check`) but re-evaluated live against
current state on every read, not only on the scheduled tick. `build_get`'s
`before` is an exclusive run-id cursor that need not name an existing run
(`before = run_id + 1, limit = 1` fetches exactly `run_id`). `container_images`
returns one row per image **ID** (every tag it carries listed together, not
one row per tag) and takes a `disk: false` flag so a caller that never renders
the disk footer (the image picker) can skip the `podman system df` scan.
