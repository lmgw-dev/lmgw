# Backends — building your own llama.cpp / ik_llama.cpp / audio.cpp / stable-diffusion.cpp images

The **Backends** page (`/backends`, *Serve* group in the nav) builds the
engine images lmgw runs from git — any repo, any ref, PRs/MRs or branches
from other remotes merged on top — tags them itself, and knows where every
tag came from. It covers all three engines lmgw runs: **llama.cpp** and
**ik_llama.cpp** (chat + aux classes), **audio.cpp** (audio class) and
**stable-diffusion.cpp** (image class).

Design doc:
[docs/design/2026-09-26-container-builds-design.md](design/2026-09-26-container-builds-design.md).
This page is the user-facing summary; the spec's §14 (measured facts) and §15
(API contract) are authoritative where the two differ.

Not "Images" — that name is already the image-generation (sd.cpp) class. Not
"Recipes" either — that already names the sd.cpp model recipes
(`lmgw__image_recipes`).

---

## 1. Concepts

- **Build** — a saved, editable definition: which repo/ref, what's merged on
  top, which GPU backend, and the build args. Editing a build and running it
  again is how you "edit an image"; an image's contents can't be changed in
  place.
- **Run** — one execution of a build: fetch → assemble → prepare → podman
  build → verify → promote. Runs are append-only history; a build keeps every
  run it ever produced, even across edits to the build itself.
- **Moving tag** — `localhost/lmgw-<engine-repo>:<slug>`. Picking it in a
  class-image field or a model's image override means "follow this build":
  it repoints to whatever run was last verified.
- **Immutable tag** — `localhost/lmgw-<engine-repo>:<slug>-<base7>-<cfg6>`,
  one per set of inputs. Picking it means "pin this exact image": it only
  ever moves to a **verified** image (a verified Rebuild anyway). A run is
  built as `<immutable>-r<run id>` first; after verify it takes the
  immutable tag when it is verified or when nothing holds the tag yet (the
  first image of those inputs), otherwise — a broken or unverified rebuild
  while the tag names a verified image — it keeps only its `-r<run id>` tag,
  so a model pinned to the immutable tag keeps the good image. `keep_runs`
  and build delete handle `-r` tags like any run's tag.
  `<engine-repo>` is `lmgw-llama-server`, `lmgw-audio-cpp` or `lmgw-sd-server`
  (`Engine::image_repo`). `cfg6` hashes every input that changes the image
  (resolved SHAs, backend, CUDA version, arch, Dockerfile, target, edits,
  build args, ccache settings) — same inputs, same tag, which is what makes
  the "already built" short-circuit (§6) sound.
- **Provenance labels.** Every image lmgw builds carries `dev.lmgw.instance`,
  `.run`, `.build`, `.slug`, `.engine`, `.repo` (credentials never — such a
  URL is refused), `.ref`, `.base`, `.extras` (JSON, resolved SHAs),
  `.backend`, `.arch`, `.cuda`, `.cfg`, plus the standard
  `org.opencontainers.image.{source,revision,version,created}`. The Images
  tab and the image picker read only these labels, so an image stays
  self-describing after its `builds`/`build_runs` rows are gone. An image
  with none of them (a hand-built tag, a plain registry pull) shows as
  **external**.
- **Instances.** Each data dir has an instance id (generated once, kept in
  the settings table, re-minted when the database turns up under another
  data dir — a dev copy of production's). Run ids are per database while
  the builds dir and the image store are shared, so the id goes into every
  per-run name (`work/run-<instance>-<id>`, `tmp/run-<instance>-<id>`,
  `logs/<instance>-<id>.log`, the verify container) and the
  `dev.lmgw.instance` label. The Images tab trusts run status, build name
  and build/run ids only for images of its own instance; another
  instance's image is described by its labels alone
  (`provenance.other_instance`).
- **Dev instances tag into their own namespace**:
  `localhost/lmgw-dev-<engine-repo>:…`, and refuse to tag, untag or remove
  any other name. `BuildView.moving_tag` is the moving tag on the answering
  instance. The headless runner (`examples/headless.rs`) is always a dev
  instance.
- **podman is the source of truth for images.** lmgw stores no image table;
  the Images tab joins `podman images` against the labels above and against
  `build_runs`.

## 2. Builds tab: fields

Repository preset (official llama.cpp, ik_llama.cpp, audio.cpp, sd.cpp, or a
custom URL), then:

| Field | Notes |
|---|---|
| `name`, `slug` | The slug becomes the tag: `[a-z0-9._-]`, and it's fixed once the build has a run. |
| `engine` | `llama` \| `audio` \| `sdcpp`. ik_llama.cpp is the `llama` engine with its own repo preset — it runs in the chat/aux classes like official llama.cpp. |
| `repo_url`, `forge` | `forge` is `github` (auto on github.com), `gitlab` (auto when a token is configured for that host), or `plain` (no PR list — refs from other remotes only). |
| `ref` | Branch, tag or full 40-hex commit. A branch/tag is re-resolved on every run; a commit never updates. |
| `extras` | Ordered list merged on top of `ref`: `{kind: pr, number, pin?}` (a PR/MR of `repo_url`) or `{kind: ref, remote_url, ref, pin?}` (a branch/tag/commit of another remote — a fork branch, or an upstream llama.cpp PR applied on top of ik via `refs/pull/N/head`). No pin follows the head; a pin is a full SHA. |
| `backend` | **GPU backend** in the UI (labelled that way so it doesn't collide with "backend" meaning the page's own subject): `cuda` (default) \| `vulkan` \| `rocm` \| `cpu`. |
| `cuda_version` | Default `13.0.0` for every engine (works on driver 615.71; shares base images across engines so audio/sd don't pull their own 10 GB CUDA base). The editor warns if you ask for more than the driver supports. |
| `arch` | Default **auto**: this host's GPUs' `compute_cap` values (`89` on an RTX 4090). Settable to another list for a different machine. |
| `dockerfile`, `target` | Default auto (the preset's candidates for the engine+backend). Chosen **at the base commit**, before any extras are merged — see §16. |
| `edits` | Starts as the preset's Dockerfile edits, shown and editable (find/replace, `required` flag, a `role` so switches like `ccache` off can drop every edit of that role even if you customized it). |
| `ccache` | On by default. `ccache_max_size` defaults to `10G`. |
| `cpus` | `--cpuset-cpus` range; empty = all cores (`-j$(nproc)` follows). |
| `build_args` | Extra `KEY=VALUE` lines. |
| `keep_layers` | Off by default (`--layers=false`) — no multi-GB intermediate stages left behind. |
| `keep_runs` | New builds default to **3** in the editor; empty/unset means keep every run's image forever. |
| `notes` | Free text. |

**Duplicate** copies a build under a new slug/name — the quick way to get
`official-master` and `official-master-pr16391` side by side. Custom
Dockerfile edits are dashboard-only (not exposed through `lmgw__build_set`).

## 3. The run pipeline

One run at a time, machine-wide (§7). Phases, each shown in the job detail
and the log with a `==> ` header:

1. **Resolve & fetch** — resolve `ref` and every unpinned extra to a SHA; a
   `pr` extra also gets forge state (`merged_at`, closed, draft). All repos
   share one object pool (`<builds_dir>/git/pool.git`, namespaced refs per
   remote), so an ik build fetching llama.cpp objects it already has
   transfers nothing.
2. **Short-circuit ("up to date").** If the computed immutable tag already
   belongs to a **succeeded run of this same build**, the run stops without
   building — this is stricter than "an image with that tag exists" (§16).
   **Rebuild anyway** forces a full build with `--pull=newer`.
3. **Assemble** — in a fresh `git worktree`, merge each extra in order.
   Submodules are only touched *after* every merge
   (`submodule sync --recursive` → `update --init --recursive --force` →
   `foreach --recursive git clean -ffdx`), so a PR that bumps a submodule
   pointer (sd.cpp's `ggml`) builds against the bumped submodule.
   - A PR the forge reports `merged_at` for is skipped as **merged
     upstream** — when the base contains it (its merge commit,
     `ForgePr.merge_commit_sha`, or else its head). Merged somewhere the
     base does not reach (an older ref, a tag, a fork) it is merged in as
     usual, and the log still notes it merged upstream.
   - A merge whose diff against its first parent is empty is **already in
     base** (catches a squash-merged PR that predates the primary
     `merged_at` signal).
   - An extra with no shared history with the assembled tree (an upstream
     llama.cpp PR stacked on ik) is **squash-applied**: its changes since its
     own fork point are taken as one commit, rather than a real merge. One
     whose head *is* its fork point (it is already on its remote's default
     branch) has no changes of its own to take and is reported as a
     conflict saying so.
   - A real conflict aborts the merge and fails the run, naming the extra,
     the conflicted files, and what was merged before it.
4. **Prepare** — the chosen Dockerfile is copied outside the tree and the
   edits applied to the copy; each is logged as applied (with a count) or not
   matched, and a `required` edit that doesn't match fails the run. An
   ignore file (`.git`, `.github`, `build/`, `.cache/`) goes with it via
   `--ignorefile`.
5. **Build** — `podman build --target <stage> --ignorefile <file>
   --layers=<keep_layers> --build-arg CUDA_VERSION=… --build-arg
   <arch-arg>=… --build-arg APP_VERSION=… --build-arg APP_REVISION=<base sha>
   --build-arg BUILD_DATE=<commit date> [--cpuset-cpus …] --label
   dev.lmgw.*=… --iidfile … -t <immutable>-r<run id> <worktree>`, with
   `TMPDIR=<builds_dir>/tmp/run-<instance>-<id>` (§7). The whole log goes to
   `<builds_dir>/logs/<instance>-<run id>.log`, never truncated, kept after
   the run.
   Progress climbs across podman's `STEP n/m:` stages and cmake/ninja's
   `[k/N]` lines and never restarts mid-build. The job detail
   (`BuildRunJobDetail`) carries `step` (`"n/m"` within the current stage —
   podman restarts its count in every stage of a multi-stage Dockerfile) and
   `stage` (`"k/N"`, the podman stage that step belongs to, from podman's
   `[k/N]` line prefix; `null` for a single-stage build and outside the
   build phase), so the run log can say which stage a step is in.
6. **Verify** (§4).
7. **Promote** — a verified run moves the build's moving tag, drops the old
   image's `--help` cache entries, applies `keep_runs` (removing old,
   unused immutable tags — each kept one logged with its reason), and
   records the image ID and size. A move that fails leaves the run
   `succeeded` but not current, with the error recorded — *Make current*
   retries it.

**Nothing is recreated automatically.** A run never touches a running
container; see §9.

## 4. Verify

Exit codes are useless (official and audio exit 0 with no GPU at all; ik's
`--help` exits 1), so verify **parses output**, always with the class's real
GPU run args:

| Engine | `--help` binary | Device probe | Success pattern |
|---|---|---|---|
| official llama | `/app/llama-server --help` | `/app/llama-server --list-devices` | `^\s+CUDA\d+:` (Vulkan/ROCm: backend name) |
| ik | `/llama-server --help` (rc 1 — expected) | `/llama-server -m /nonexistent.gguf` (no `--list-devices`) | `ggml_cuda_init: found [1-9]\d* CUDA devices` |
| audio | `server --help` via the dispatcher entrypoint | `server --list-devices` | `CUDA:\d+ ` |
| sd | `/sd-server --help` | `/sd-server --list-devices` | `^CUDA\d+\t` |

Without the GPU args, official/audio just list `(none)`/`CPU:0` and ik/sd
fail on `libcuda.so.1` — exactly the "broken backend" case verify exists to
catch (official builds use `GGML_BACKEND_DL=ON`, so `--help` alone succeeds
even when the CUDA backend can't actually load — wrong arch, or a CUDA
newer than the driver).

Outcomes:

- **succeeded** — built, verified, promoted.
- **unverified** — built, but not GPU-checked (GPU hold active, or a CUDA
  OOM while probing). Not promoted. **Verify now** (`build_verify`) retries
  later; if it then succeeds and no newer run of the build is already
  current, that run is promoted too, full retention included.
- **broken** — built, a verify check failed. Not promoted.

## 5. Caches and timings

- **ccache**, one cache-mount id per `(engine, backend)`: `lmgw-<engine>-<backend>`
  for a plain build, `lmgw-<engine>-<backend>-<slug>` for one with extras — so
  PR code never writes into the cache a master build reads. A build with
  extras also mounts the plain cache **read-only** and seeds its own from it
  first (`cp -a -u --reflink=auto`, ~0.2 s on btrfs for llama's 572 MB
  cache) — so a PR build isn't compiling cold, and it also picks up whatever
  later master builds added.
- **npm cache** follows the same pattern for llama's web-UI stage
  (`lmgw-npm-<engine>[-<slug>]`), seeded the same way.
- **`--layers=false` by default** — no multi-GB intermediate stages left
  behind; a rebuild's speed comes from ccache/npm-cache and the up-to-date
  short-circuit, not from podman's layer cache.
- **Measured (2026-09-26 spike + e2e, sm_89, RTX 4090):** cold builds ~350 s
  (official), ~404 s (ik), ~362 s (audio), ~248 s (sd) — the design doc's
  headline "~4–7 min" cold, "~40–130 s" cache-hit range. A cache-hit rebuild
  of official measured 123 s wall / 13 s compile (609/610 ccache hits); the
  rest was the web UI (47 s), apt (41 s) and the commit. Official master +
  a PR on the same shared cache: 608/610 hits (99.67%), 117 s wall against
  129 s for plain master.
- Peak RAM added per build: roughly 8–12 GB (minimum available 14–16 GB of
  62 during the spike).

## 6. The machine-wide build lock

One run builds at a time, everywhere — every instance of the user, whatever
its `builds_dir`. The lock file is `$XDG_RUNTIME_DIR/lmgw-build.lock` when
that directory exists, else `/tmp/lmgw-build-<uid>.lock`. A run waiting on
it still shows as running (the jobs system has no separate queued state);
its phase is `waiting`, naming the build it's behind.

The git pool has a lock of its own, held only while it is changed (a fetch,
a ref update, a worktree added or removed): the process's mutex plus a
flock on `<builds_dir>/git/pool.lock`. *Check merge* and *Resolve* of any
instance wait for a fetch, not for a whole build. Whoever takes it next
removes the `*.lock` files a killed git left in the pool.

## 7. Cancel and leftover cleanup

Cancel works in every phase: a fetch, merge or forge lookup in flight is
stopped (its git is killed), and `podman build` gets SIGTERM, SIGKILL after
a grace period. Either way podman leaves one buildah working container per
started stage (status `Storage`, named `<image>-working-container[-N]`,
unlabeled) and a `buildahNNN` scratch dir; SIGTERM alone leaves no compile
processes and commits nothing, SIGKILL additionally leaves stray overlay
mounts. Cleanup (under the build lock) removes **only this run's own
leftovers**: the working containers (state `Storage` and buildah's name)
that appeared while it ran — never a model container recreated meanwhile,
an agent's, or anything older — and its scratch dir, which is its own
`TMPDIR` (`<builds_dir>/tmp/run-<instance>-<id>`, removed whole after every
build). buildah also roots its cache mounts under `$TMPDIR`, so
`buildah-cache-<uid>` there is a symlink to the real cache root in
`/var/tmp`: ccache stays shared and survives. An image the build still
finished as the cancel went out is removed by its `-r<run id>` tag when
nothing else names or uses it. A run left `running` at boot is marked
failed, alongside the general orphaned-job sweep; the boot sweep removes
that instance's own run dirs only. The partial compile survives in ccache
either way.

## 8. Retention and delete

- **`keep_runs`** (per build, editor default 3, empty = keep forever): after
  a promotion, older unused immutable tags are removed and each one that's
  kept is logged with why.
- **Image delete** (`container_image_delete` / the Images tab / the run
  history's *Delete image…*) asks first while a class default, a model
  override or any container still uses it — stopped ones count too
  (exited, created, a buildah working container), since they block
  `podman rmi` just like running ones. The confirmation lists the users and
  what deleting anyway does; confirmed (`force: true`), every container on
  the image is stopped and removed first — a model's through the runtime,
  so a running one goes down even mid-request — and the class defaults and
  model overrides are left naming a missing image, which the answer lists
  (`still_named_by`) and the UI raises as a warning. Without `force` the op
  refuses and names the users. Refused outright on a dev instance, which
  shares production's image store.
- **Retag** (`container_image_tag`) checks everything before changing
  anything: adding a name that names another image (moving it) is refused
  while anything follows that name (a class default or model override
  naming it, a container running the image it names now); removing one a
  class default or model override names is refused; a dev instance may add
  or remove only `localhost/lmgw-dev-…` names.
- **Build delete** (`build_set` action `delete`) removes the definition; its
  run history stays (with `build_id` set to `NULL`). `delete_images=true`
  additionally removes the build's own images wherever nothing uses them,
  reporting `removed_images` and, for what it kept, `kept: [{tag, reason}]`.
  Either way, delete also removes **this build's own** per-build ccache and
  npm-cache directories — for its engine and backend now and every one its
  runs used (never the shared, non-suffixed ones, never another build's),
  reported in `removed_caches` — skipped on a dev instance, since
  its `/var/tmp` is production's, and refused outright while a run of the
  build is live.

## 9. After a run: recreating running containers

A run (or a registry **Pull update**, §10) never recreates a container on
its own. The success panel lists every user of the (old or moved) image —
class defaults, model overrides, running containers, matched by image ID —
and offers **Recreate N running containers**, which calls one per-model
`container apply` per affected model (never the group apply). When the
image isn't a class default yet, it also offers **Set as default for …**.

**Rollback.** Every run in the history has **Make current**
(`build_promote`): it moves the build's moving tag straight to that run's
image, with the same recreate offer. It does **no retention** — rolling
back must never prune the runs that are newer than the one you rolled back
to.

## 10. Update detection

A scheduled check (**Settings → Backends**, `build_update_check_hours`,
default 6 h, `0` = off, at most 8760 = a year) plus **Check now** per build
or globally (`build_updates_check {id?}`):

- For each build, it resolves `ref` and every unpinned extra and compares
  against the last succeeded/unverified run. A pinned commit never shows an
  update.
- Badge reasons: `"master +37 commits"`, `"master moved (abc1234 →
  def5678)"`, `"PR #1234 pushed"`, `"PR #1234 merged upstream, drop it?"`,
  `"PR #1234 closed unmerged"`, `"definition changed since last run"`. What
  couldn't be checked (a rate-limited forge, with its reset time) lands in
  `errors`, never in `reasons` — a failed check is not an update.
  `UpdateStatus::has_update()` (empty `reasons` = up to date) is what the nav
  badge counts.
- Each remote is asked with one narrow `git ls-remote`, all remotes side by
  side, each with a **60 s timeout** (`updates::REMOTE_TIMEOUT`). A remote
  that doesn't answer in time becomes `"check failed: <remote> timed out
  after 60s (git ls-remote)"` in the `errors` of every build using it — one
  stalled remote can't wedge the scheduled check or **Check now**.
- A check that **fails as a whole** (the database, a panic) is logged once
  and retried a tick (5 min) later, then 10, 20, … minutes — doubling per
  failure in a row, capped at the interval. It never re-asks in a loop.
- The result is persisted to one `settings` row, key `backends:update_check`
  (see §16) — but is also **re-derived live on every read** against the
  build's current definition and runs, so building the new head clears
  "master moved" immediately and editing a build shows "definition changed"
  immediately, neither waiting for the next scheduled tick. Writes to the
  row take turns and each writes the state as it is at its turn, so the row
  always ends as the newest state; a per-build **Check now** that finishes
  during a full check keeps its newer observation (merged per build by
  `checked_at`), and a pull's re-evaluation waits for a running full check
  instead of being overwritten by it.
- **Registry images** in active use under a non-`localhost/` tag (the
  ghcr.io audio/sd defaults) get the same check against the registry's
  manifest digest (anonymous bearer-token flow), surfaced as
  `registry_update` on `container_images`, with a **Pull update** action
  (`container_image_pull {image}`, a background `podman pull` job; it
  answers `{job_id, image}` at once). This is how a stale pulled audio.cpp
  image (predating a route it needs) gets flagged. A pull is allowed on a
  dev instance — it only adds an image to the shared store and moves a
  registry tag, deleting nothing production uses.
  - **Registry references only**: `registry/repository:tag`, optionally
    `docker://`-prefixed (the prefix is stripped). podman's other transports
    — `docker-archive:`, `oci-archive:`, `oci:`, `dir:`,
    `containers-storage:`, `docker-daemon:`, `sif:`, `tarball:` — read from
    this machine, not a registry, and are refused with a sentence saying so,
    as are other `scheme://` URLs, `localhost/` images (built here: run the
    build) and digest pins (nothing to update).
  - The reference is pulled and the job keyed in its **fully qualified**
    spelling (`busybox` → `docker.io/library/busybox:latest`, job key
    `image:docker.io/library/busybox:latest`), so two spellings of one image
    are one pull; a second pull of it while one runs answers with the
    running job.
  - `container_image_pull_status {job_id}` reads the job back:
    `{job_id, image, status, detail, result, error}` — `detail` is the live
    `ImagePullJobDetail` (`image`, `old_id`, `last_line`) while it runs,
    `result` the `ImagePullResult` (old/new ID and digests, `updated`,
    `used_by`, podman's log) once it is done, `error` podman's last lines
    once it failed. The jobs feed carries running jobs only and no job's
    result, so this is how the page learns what a finished pull did.

## 11. Images tab and the image picker

**Images tab**: every local image of the three engines — lmgw-built,
external (a hand-built tag, an unlabeled registry pull), or a registry
image in use — one row per image **ID** (every tag it carries listed
together, so two names for one image are one row). Columns: tags, engine,
GPU-backend label, size, created, provenance (build/run/repo/ref/base/extras
when lmgw built it), used-by. Actions: set as class default, delete (asks
first while in use — see above; refused on a dev instance), pull update. Footer: `podman system df`
totals and any orphaned `/var/tmp/buildah*` dirs — shown for information,
**nothing is pruned from here** (clean up untracked images and `/var/tmp`
orphans by hand).

**Image picker** replaces free text in the four Settings → Runtimes
class-image fields (chat/aux/audio/image) and the four per-model "Image
override" fields. Typing still works — anything typed is a valid value,
Enter/blur commits it, Esc restores the stored value. Opening it lists what's
on the machine for that class's engine: each build's moving tag first
("follows `<build>`"), then its pinned runs, then other local images of that
engine. Under the field, a status line says what the current value resolves
to on this machine right now — local / missing / broken / not GPU-verified —
and warns if the image's engine or GPU-backend label doesn't match the
class.

## 12. Forge integration (PRs/MRs)

- **GitHub** — open PRs paged 100 at a time, most-recently-updated first,
  filtered client-side; a numeric query or a pasted PR URL fetches that PR
  regardless of state. Token or not, 60 requests/h unauthenticated, 5,000
  with one.
- **GitLab** — `merge_requests?state=opened&search=…` server-side.
- **Plain** — no PR list; extras from other remotes only.
- **GitHub Enterprise** — set `forge=github`; its API is `/api/v3` on its
  own host. A host with a forge token defaults to `gitlab`, so a GHE host
  left on the default answers GitLab's `/api/v4` with a 404, and that error
  says: "If <host> is GitHub Enterprise rather than GitLab, set
  forge=github".
- **Check merge** (`build_check_merge`) fetches, then merges every extra in
  order into a **throwaway worktree** running the exact same assemble code a
  real run uses (not a bare `merge-tree` — sd.cpp's gitlink-bumping PRs need
  a checked-out submodule to fast-forward cleanly). Reports per extra:
  `merged`, `already_in_base`, `merged_upstream`, `squash_applied` or
  `conflict` (with the conflicting files). Takes seconds on a warm pool.

## 13. Settings → Backends

| Field | Notes |
|---|---|
| **Forge tokens** | One row per host — GitHub always shown, then every host you've added. A token goes only to its own host: as the forge API's bearer, and as a git `http.<url>.extraHeader` for private-repo fetches — never argv, never a log. GitHub's row talks to `api.github.com` specifically; every other forge (GitLab, Gitea, Forgejo — anything not `github.com`) is named by host, with a port when it isn't 443. The host a repository URL is looked up under is its **web host** (`forge::token_host`, one function for API calls, git and the update check): `https://git.example:443/…` is `git.example`, an SSH remote's port (`ssh://git@host:2222/…`) is the SSH daemon's and is dropped. **A token is never sent over plain `http://`** to another machine: the API call, `ls-remote` or fetch fails with "refusing to send the forge token over plain http to <host>; use https" (plain http to `localhost`/`127.0.0.0/8`/`::1` is allowed — it never leaves the machine). A remote that needs no token keeps working over http. The self-admin tools can neither read nor write these. |
| **Builds directory** | Where the git pool, worktrees, logs and per-run contexts live. Must not be on tmpfs — the field shows a warning naming this if it is (a dev instance's default data dir is tmpfs, so it **must** be pointed elsewhere before it can run a build at all). Placeholder shows the effective path in use right now. |
| **Update check interval (hours)** | `build_update_check_hours`, default 6, `0` disables the background check (Check now still works). Accepted range 0–8760 (a year); a value outside it is refused on save with a message stating the range (the dashboard save and `lmgw__settings_set` alike). |

## 14. MCP tools

Verified against `crates/lmgw-core/src/mcp/selfadmin/catalog/{reads,builds}.rs` (§9.2 of the spec
undercounts these by two — `build_log` and `container_image_pull` are also
exposed):

| Tool | Writes | Needs self-admin |
|---|---|---|
| `lmgw__builds` | no | read-only or full |
| `lmgw__build_log` | no | read-only or full |
| `lmgw__container_images` | no | read-only or full |
| `lmgw__forge_prs` | no | read-only or full |
| `lmgw__build_set` | yes | **full** |
| `lmgw__build_run` | yes | **full** |
| `lmgw__build_check_merge` | yes* | **full** |
| `lmgw__container_image_delete` | yes | **full** |
| `lmgw__container_image_pull` | yes | **full** |

\* `build_check_merge` changes nothing lasting — it's flagged `writes` because
it downloads repositories and runs git on the machine.

**Reading a run's log.** `build_run_log {run_id, offset}` returns the log
from byte `offset`, **at most 1 MiB per call** (`run::LOG_CHUNK_BYTES`),
with `next_offset` to continue from and `done` once the run has ended and
the reply reaches the end; the dashboard streams it this way.
`build_run_log {run_id, tail: N}` returns the last N lines instead (no size
cap — exactly the lines asked for), with `next_offset` at the end of the
file; a non-zero `offset` together with `tail` is refused. `lmgw__build_log`
takes the same `offset` / `tail`, and **with neither returns the last 200
lines** (`ops::backends::TOOL_LOG_TAIL`) — where a run's outcome is — rather
than the first megabyte.

**Arguments are checked by name.** Every Backends op's args struct refuses
an unknown field (`deny_unknown_fields`), and the Backends MCP tools enforce
their closed input schema: a typo (`offest`) is an error naming it and the
arguments the tool takes, never a silent default.

**Dashboard-only** (no MCP tool): `build_get`, `build_resolve`,
`build_promote` (*Make current*), `build_verify` (*Verify now*), `build_env`,
`forge_refs`, `forge_pr`, `container_image_tag` (*Retag*),
`build_updates_check`, `container_image_pull_status`.

With the write tools an agent can do "build master + PR 16391, test model X
on it, report back": `lmgw__build_set` → `lmgw__build_run` → poll
`lmgw__build_log` / `lmgw__builds id=` until `live_job_id` is null →
`lmgw__container model=<id> action=apply` to recreate → `lmgw__local_model_test`.

## 15. Host notes

- The ccache/npm cache mounts live under `/var/tmp/buildah-cache-<uid>/`.
  **`systemd-tmpfiles` removes an entry after 30 days idle** — a build you
  haven't touched in a month compiles cold next time, not a bug.
- lmgw does not prune what builds leave behind outside its own tags:
  dangling images, stale `Storage` containers and `/var/tmp/buildahNNN`
  directories (from other tools too) accumulate on a build host. A periodic
  `podman image prune`, plus `podman system prune` while no build is running,
  keeps them in check.
- git 2.55+ is assumed (`merge-tree --write-tree`); podman 5.8.7+ (`--ignorefile`,
  out-of-context `-f`, `--label`, `--iidfile`, `--cpuset-cpus`).

## 16. Known limits

- **Vulkan, ROCm and CPU profiles are untested.** Only the four CUDA
  profiles (official llama, ik, audio, sd) are `tested: true` in the preset
  table; the other eight compile from the upstream Dockerfile as-is but
  none has been built or run yet. See [amd-arch.md](amd-arch.md#building-your-own-images-vulkanrocm)
  for the ROCm arch-arg note.
- **ik_llama.cpp has no ROCm or CPU profile at all** (not just untested):
  its `llama-server.Dockerfile` (CPU) and `llama-server-rocm.Dockerfile`
  still `make`, and ik dropped its Makefile — they cannot build. Set
  `dockerfile` by hand if you want to try anyway.
- **No nginx front layer in the images** (design §11.1) — lmgw is already
  the single front door and proxies only the API, and holding the port closed
  until the model loads (or rewriting `--host`/`--port`) is part of lmgw's own
  start sequence.
- **audio.cpp and stable-diffusion.cpp default to CUDA 13.0.0**, not their
  own upstream Dockerfile defaults — chosen because 13.0.0 is what works on
  driver 615.71 for all four engines and shares a base image, at the cost of
  not matching what a `git clone && docker build` of those repos alone would
  give you.

---

### Deviations from the design doc's body (implementation notes)

A few things the running code does differently from the design doc's
prose (not already called out in the doc's own §14/§15):

- The Dockerfile/target candidate is chosen **at the base commit**, before
  any extra is merged, so it can be hashed as part of the immutable tag
  before anything is fetched or merged (§5 step 2's short-circuit needs the
  tag before assembling). If an extra removes the file or its target stage,
  *Prepare* fails visibly rather than silently building the base's file.
- The **up-to-date short-circuit** is stricter than "an image with that tag
  exists": it requires a **succeeded run of this same build** recorded
  against that exact image ID and tag, not merely any local image bearing
  it.
- **`build_verify` ("Verify now") can itself promote.** If a re-verified run
  turns `succeeded` and no newer run of its build is already current, it's
  promoted the same way a run's own step 7 would — moving tag, help-cache
  drop, retention. **`build_promote` ("Make current" / rollback) never runs
  retention** — moving back to an older run must not prune anything newer.
- The forge client (`GatewayForge`) holds a `Weak<AppState>` rather than an
  `Arc`, installed once at startup, so it can't keep the whole gateway alive
  from a background scheduler task.
- The update check's results are persisted to one `settings`-table row keyed
  `backends:update_check`, and re-evaluated against current state on every
  read rather than only reflecting the last scheduled tick.
- `build_get`'s `before` cursor is an **exclusive** run-id boundary and need
  not name a real run (`before = run_id + 1, limit = 1` fetches exactly
  `run_id`).
- `container_images` returns **one row per image ID**, not per tag — an
  image with several tags (e.g. a moving tag and its own immutable one still
  matching) is one row listing every tag.
- `container_images` takes a `disk: false` flag to skip the `podman system df`
  scan (the slowest part of the op) for a caller — the image picker — that
  never renders the footer anyway.
