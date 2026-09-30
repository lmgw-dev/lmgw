//! Phases 1–2 of a run as reusable steps (container-builds §5, §14.1): what
//! every "auto" and every "follow the head" of a build becomes right now.
//! The run executor walks them one by one (so each shows as its own phase);
//! the editor's **Resolve** ([`resolve_preview`]) and **Check merge**
//! ([`check_merge`]) walk the same ones without building, so a preview can
//! never disagree with the run that follows it.

use std::path::PathBuf;

use lmgw_api_types::builds::{
    BuildCheckMergeArgs, BuildEnv, CheckMergeReport, CheckMergeStep, EditApplied, MergeOutcome,
    RepoPreset as RepoPresetView, ResolvedPreview,
};

use crate::backends::git::{
    self, AssembleOpts, AssembleStep, FetchTarget, Fetched, Git, GitAuth, Pool, Progress,
    ResolvedExtraSpec,
};
use crate::backends::model::{BuildExtra, BuildSpec, Engine, Forge, ResolvedExtra, ResolvedInputs};
use crate::backends::presets::{self, DockerfileChoice, DockerfileInfo, SourceFacts, TemplateVars};
use crate::backends::{forge, paths, tags, validate, ForgePr};
use crate::state::SharedState;
use crate::store;

/// What the host and the forge say, before anything is fetched: the CUDA
/// version and arch list the build targets, and each extra's forge state.
pub(crate) struct HostFacts {
    pub cuda: Option<String>,
    pub arch: Vec<String>,
    /// One slot per `spec.extras` entry: the forge's answer for a `pr`
    /// extra, `None` for a `ref` extra or when the forge could not say.
    pub prs: Vec<Option<ForgePr>>,
    /// Why a PR's state is unknown, one sentence each.
    pub notes: Vec<String>,
}

/// §5 phase 1, the part that needs no git: CUDA version, arch (the build's
/// own, else the host GPUs'), and the forge state of every `pr` extra.
pub(crate) async fn resolve_host(
    state: &SharedState,
    spec: &BuildSpec,
    log: Progress<'_>,
) -> Result<HostFacts, String> {
    let cuda = presets::resolve_cuda_version(spec);
    let runner = state.runtime().runner();
    let arch = presets::resolve_arch(spec.backend, spec.arch.as_deref(), runner.as_ref()).await?;
    log(&format!(
        "GPU backend {}, CUDA {}, arch {}{}",
        spec.backend.as_str(),
        cuda.as_deref().unwrap_or("n/a"),
        if arch.is_empty() {
            "(the Dockerfile's default)".to_string()
        } else {
            arch.join(";")
        },
        if spec.arch.as_ref().is_some_and(|a| !a.is_empty()) {
            ""
        } else {
            " (auto)"
        }
    ));
    let (prs, notes) = lookup_prs(state, spec, log).await;
    Ok(HostFacts {
        cuda,
        arch,
        prs,
        notes,
    })
}

/// The forge state of each `pr` extra (§5 phase 1: `merged_at`, closed,
/// draft). A lookup that fails leaves the PR's state unknown — merged, not
/// skipped — and says so; it never fails the run (§14.1: the forge is the
/// primary signal, git's empty-merge check the backstop).
pub(crate) async fn lookup_prs(
    state: &SharedState,
    spec: &BuildSpec,
    log: Progress<'_>,
) -> (Vec<Option<ForgePr>>, Vec<String>) {
    let forge = state.builds.forge();
    let mut prs = Vec::with_capacity(spec.extras.len());
    let mut notes = Vec::new();
    for extra in &spec.extras {
        let BuildExtra::Pr { number, .. } = extra else {
            prs.push(None);
            continue;
        };
        match forge.pr(&spec.repo_url, spec.forge, *number).await {
            Ok(pr) => {
                log(&format!("PR #{number}: {}", forge_state(&pr)));
                prs.push(Some(pr));
            }
            Err(e) => {
                let note = format!(
                    "PR #{number}: forge state unknown ({e}) — it is merged rather than skipped \
                     as merged upstream"
                );
                log(&note);
                notes.push(note);
                prs.push(None);
            }
        }
    }
    (prs, notes)
}

/// `open, draft` / `closed, merged 2026-09-20T…` / `open, head abc1234`.
fn forge_state(pr: &ForgePr) -> String {
    let mut s = if pr.state.is_empty() {
        "state unknown".to_string()
    } else {
        pr.state.clone()
    };
    if pr.draft {
        s.push_str(", draft");
    }
    match &pr.merged_at {
        Some(at) => s.push_str(&format!(", merged {at}")),
        None if !pr.head_sha.is_empty() => {
            s.push_str(&format!(", head {}", git::short_sha(&pr.head_sha)))
        }
        None => {}
    }
    s
}

/// The forge token configured for `url`'s host, as git gets it (§7
/// "Tokens"): only ever to that exact host (the canonical token host,
/// [`forge::token_host`]), and never over plain `http://` to another machine
/// — [`forge::git_auth`]'s rules, the same the update check and the forge ops
/// follow.
pub(crate) fn auth_for(
    state: &SharedState,
    url: &str,
    forge: Forge,
) -> Result<Option<GitAuth>, String> {
    forge::git_auth(&state.snapshot().settings.forge_tokens, url, forge)
}

/// The base and every extra, resolved and fetched into the pool.
pub(crate) struct FetchedInputs {
    pub base: Fetched,
    /// What assemble takes, in order — merged-upstream PRs included, flagged,
    /// so the log and Check merge can say they were skipped.
    pub assemble: Vec<ResolvedExtraSpec>,
    /// What the image is built from: the extras minus the merged-upstream
    /// ones, which do not change it (`ResolvedInputs::extras`).
    pub extras: Vec<ResolvedExtra>,
}

/// §5 phase 1, the git part (§14.1): resolve the ref and each extra to a
/// commit and fetch exactly those into the pool. An extra that shares no
/// history with the base also gets what a squash-apply needs: the forge PR's
/// fork point, or its remote's default branch to compute one against.
pub(crate) async fn fetch_inputs(
    state: &SharedState,
    spec: &BuildSpec,
    pool: &Pool,
    prs: &[Option<ForgePr>],
    progress: Progress<'_>,
) -> Result<FetchedInputs, String> {
    let base_auth = auth_for(state, &spec.repo_url, spec.forge)?;
    let base = pool
        .resolve_and_fetch(
            &spec.repo_url,
            &FetchTarget::Ref(spec.git_ref.clone()),
            None,
            spec.forge,
            base_auth.as_ref(),
            Some(progress),
        )
        .await?;
    progress(&format!(
        "base: {} {} is {}",
        spec.repo_url,
        spec.git_ref,
        git::short_sha(&base.sha)
    ));
    let mut assemble = Vec::with_capacity(spec.extras.len());
    let mut extras = Vec::with_capacity(spec.extras.len());
    for (i, extra) in spec.extras.iter().enumerate() {
        let pr = prs.get(i).cloned().flatten();
        let (url, forge, target, auth) = match extra {
            BuildExtra::Pr { number, .. } => (
                spec.repo_url.clone(),
                spec.forge,
                FetchTarget::Pr(*number),
                base_auth.clone(),
            ),
            BuildExtra::Ref {
                remote_url,
                git_ref,
                ..
            } => {
                let snap = state.snapshot();
                // The token is looked up under the canonical token host, as
                // `forge::token_for` does — `host:443` and `host` are one.
                let token_host = forge::token_host(remote_url);
                let forge = validate::default_forge(remote_url, |_| {
                    token_host
                        .as_ref()
                        .is_some_and(|h| snap.settings.forge_tokens.contains_key(h))
                });
                (
                    remote_url.clone(),
                    forge,
                    FetchTarget::Ref(git_ref.clone()),
                    auth_for(state, remote_url, forge)?,
                )
            }
        };
        let fetched = pool
            .resolve_and_fetch(
                &url,
                &target,
                extra.pin(),
                forge,
                auth.as_ref(),
                Some(progress),
            )
            .await?;
        let merged_upstream = match pr.as_ref().filter(|p| p.merged_at.is_some()) {
            Some(p) => merged_into(pool, &base.sha, p, &fetched.sha).await?,
            None => false,
        };
        progress(&format!(
            "{}: {}{}{}",
            extra.label(),
            git::short_sha(&fetched.sha),
            if extra.pin().is_some() {
                " (pinned)"
            } else {
                ""
            },
            if merged_upstream {
                " — merged upstream, will be skipped"
            } else if pr.as_ref().is_some_and(|p| p.merged_at.is_some()) {
                " — merged upstream, but not into this base (neither its merge commit nor its \
                 head is in the base's history), so it is merged in as usual"
            } else {
                ""
            }
        ));
        // The forge PR's own fork point, when the pool has its base commit
        // (it normally does: the PR targets the branch being built).
        let fork_point = match pr.as_ref().map(|p| p.base_sha.as_str()) {
            Some(b) if !b.is_empty() && pool.has_commit(b).await => {
                pool.merge_base(None, b, &fetched.sha).await?
            }
            _ => None,
        };
        // Unrelated to the base: a squash-apply will need a fork point. Fetch
        // the extra's remote's default branch to compute one against — but
        // only then, since for llama.cpp on ik that is a whole history.
        let upstream_tip = if !merged_upstream
            && fork_point.is_none()
            && pool
                .merge_base(None, &base.sha, &fetched.sha)
                .await?
                .is_none()
        {
            match pool
                .fetch_default_branch(&url, auth.as_ref(), Some(progress))
                .await
            {
                Ok(f) => Some(f.sha),
                Err(e) => {
                    progress(&format!(
                        "{}: shares no history with the base, and {url}'s default branch could \
                         not be fetched to find its fork point: {e}",
                        extra.label()
                    ));
                    None
                }
            }
        } else {
            None
        };
        assemble.push(ResolvedExtraSpec {
            label: extra.label(),
            sha: fetched.sha.clone(),
            merged_upstream,
            fork_point,
            upstream_tip,
        });
        if !merged_upstream {
            extras.push(ResolvedExtra {
                extra: extra.clone(),
                sha: fetched.sha,
            });
        }
    }
    Ok(FetchedInputs {
        base,
        assemble,
        extras,
    })
}

/// Whether a PR the forge reports merged is in `base` — its merge (or squash)
/// commit is in the base's history, or failing that its head is. Merged
/// upstream is not the same as merged into what this build builds: a build of
/// an older ref, a fork's branch, a release tag. Only then is it skipped; a
/// merged PR the base lacks is merged in like any other (and an
/// already-applied one still shows as "already in base" by tree equality).
async fn merged_into(pool: &Pool, base: &str, pr: &ForgePr, head: &str) -> Result<bool, String> {
    if !pr.merge_commit_sha.is_empty() && pool.contains(base, &pr.merge_commit_sha).await? {
        return Ok(true);
    }
    pool.contains(base, head).await
}

/// The Dockerfile the run builds (§4 "dockerfile, target" auto): the first of
/// the build's candidates present **at the base**, with its target, edits and
/// verify probes. Chosen at the base rather than on the assembled tree so it
/// can be hashed before anything is merged (§5 step 2) — a function of the
/// inputs alone, which is what makes the short-circuit sound. Prepare then
/// reads the same path from the assembled tree and refuses, visibly, if an
/// extra removed it or its target stage.
pub(crate) async fn pick_dockerfile(
    pool: &Pool,
    spec: &BuildSpec,
    base: &str,
    log: Progress<'_>,
) -> Result<(DockerfileChoice, String), String> {
    for path in presets::dockerfiles_to_try(spec) {
        let Some(text) = pool.read_file(base, &path).await? else {
            continue;
        };
        let choice = presets::choose_dockerfile(spec, &path, &text)?;
        log(&format!(
            "Dockerfile {path} ({}), target {}",
            choice
                .profile
                .map_or("no preset knows it".to_string(), |p| format!(
                    "preset {}",
                    p.id
                )),
            if choice.target.is_empty() {
                "its last stage"
            } else {
                &choice.target
            }
        ));
        for n in &choice.notes {
            log(&format!("note: {n}"));
        }
        return Ok((choice, text));
    }
    Err(presets::no_dockerfile_error(spec, base))
}

/// The run's [`ResolvedInputs`] — what the config hash covers.
pub(crate) fn resolved_inputs(
    host: &HostFacts,
    inputs: &FetchedInputs,
    choice: &DockerfileChoice,
) -> ResolvedInputs {
    ResolvedInputs {
        base_sha: inputs.base.sha.clone(),
        extras: inputs.extras.clone(),
        cuda_version: host.cuda.clone(),
        arch: host.arch.clone(),
        dockerfile: choice.dockerfile.clone(),
        target: choice.target.clone(),
        edits: choice.edits.clone(),
    }
}

/// What the version build args are derived from (§2.1, §14.1).
pub(crate) async fn source_facts(
    pool: &Pool,
    repo_url: &str,
    base: &str,
) -> Result<SourceFacts, String> {
    Ok(SourceFacts {
        repo_url: repo_url.to_string(),
        sha: base.to_string(),
        build_number: pool.build_number(base).await?,
        commit_date: pool.commit_date(base).await?,
    })
}

/// The per-run edit template values (§14.2).
pub(crate) fn template_vars(spec: &BuildSpec) -> TemplateVars {
    TemplateVars::new(
        spec.engine,
        spec.backend,
        &spec.slug,
        !spec.extras.is_empty(),
        &spec.ccache_max_size,
    )
}

/// The refusals every git-touching entry point gives first: a builds dir in
/// RAM (§10), git missing or too old (§10 "git dependency"). The pool, when
/// both are fine.
pub(crate) async fn open_pool(state: &SharedState) -> Result<Pool, String> {
    let builds_dir = state.builds_dir();
    if let Some(why) = paths::tmpfs_refusal(&builds_dir) {
        return Err(why);
    }
    let git = Git::new();
    git::git_available(&git).await?;
    Pool::open(git, &builds_dir)
        .await
        .map(|p| p.with_instance(state.builds.instance_id()))
}

/// `build_resolve` (§15): what `spec` resolves to right now — base SHA,
/// Dockerfile, target, edits, build args and both tags — fetched but not
/// built. The same steps as a run's phases 1–2; the edits and build args are
/// checked against the **base**'s Dockerfile, since nothing is merged here.
/// Everything a run would only note (a required edit that does not match, an
/// arch with nowhere to go, a CUDA newer than the driver) is a warning.
pub async fn resolve_preview(
    state: &SharedState,
    spec: BuildSpec,
) -> Result<ResolvedPreview, String> {
    let spec = validate::validate_build(spec)?;
    let pool = open_pool(state).await?;
    let quiet = |l: &str| tracing::debug!("build resolve: {l}");
    let host = resolve_host(state, &spec, &quiet).await?;
    let inputs = fetch_inputs(state, &spec, &pool, &host.prs, &quiet).await?;
    let base = inputs.base.sha.clone();
    let (choice, text) = pick_dockerfile(&pool, &spec, &base, &quiet).await?;
    let resolved = resolved_inputs(&host, &inputs, &choice);
    let cfg = tags::cfg_hash(&spec, &resolved)?;
    let dev = state.dev();
    let immutable_tag = tags::immutable_tag_for(spec.engine, &spec.slug, &base, &cfg, dev)?;
    let facts = source_facts(&pool, &spec.repo_url, &base).await?;

    let mut warnings = host.notes.clone();
    warnings.extend(choice.notes.iter().cloned());
    let report = presets::apply_edits_report(&text, &choice.edits, &template_vars(&spec));
    if let Err(e) = report.check() {
        warnings.push(e);
    }
    let edit_outcomes = report
        .outcomes
        .iter()
        .map(|o| EditApplied {
            index: u32::try_from(o.index).unwrap_or(u32::MAX),
            name: o.name.clone(),
            role: o.role,
            required: o.required,
            applied: u32::try_from(o.matches).unwrap_or(u32::MAX),
        })
        .collect();
    let args = presets::build_args(
        &choice,
        spec.backend,
        &DockerfileInfo::parse(&report.text),
        &facts,
        host.cuda.as_deref(),
        &host.arch,
        &spec.build_args,
    )?;
    warnings.extend(args.notes.iter().cloned());
    if let Some(cuda) = &host.cuda {
        let runner = state.runtime().runner();
        if let Ok(max) = presets::detect_driver_cuda_version(runner.as_ref()).await {
            warnings.extend(presets::cuda_driver_warning(cuda, &max));
        }
    }
    Ok(ResolvedPreview {
        base_sha: base,
        build_number: (spec.engine == Engine::Llama).then_some(facts.build_number),
        dockerfile: choice.dockerfile.clone(),
        target: choice.target.clone(),
        profile: choice.profile.map(|p| p.id.to_string()).unwrap_or_default(),
        edits: choice.edits.clone(),
        build_args: args.args,
        moving_tag: tags::moving_tag_for(spec.engine, &spec.slug, dev),
        immutable_tag,
        warnings,
        edit_outcomes,
    })
}

/// `build_check_merge` (§15, §7 as superseded by §14.1): fetch, then assemble
/// base + extras in a throwaway worktree with the run's own code, and report
/// each extra's outcome plus the forge's state of each PR. Nothing is built;
/// the worktree is removed afterwards. `id` checks a saved build; `spec` an
/// unsaved one straight from the editor.
pub async fn check_merge(
    state: &SharedState,
    args: BuildCheckMergeArgs,
) -> Result<CheckMergeReport, String> {
    let spec = match (args.id, args.spec) {
        (Some(id), _) => {
            store::get_build(&state.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no build with id {id}"))?
                .spec
        }
        (None, Some(spec)) => validate::validate_build(spec)?,
        (None, None) => return Err("check merge needs a build id or a spec".into()),
    };
    let pool = open_pool(state).await?;
    let quiet = |l: &str| tracing::debug!("check merge: {l}");
    let (prs, _) = lookup_prs(state, &spec, &quiet).await;
    let inputs = fetch_inputs(state, &spec, &pool, &prs, &quiet).await?;
    let report = pool
        .check_merge(
            &inputs.base.sha,
            &inputs.assemble,
            AssembleOpts::default(),
            Some(&quiet),
        )
        .await?;
    let forge_notes: Vec<(String, String)> = spec
        .extras
        .iter()
        .zip(&prs)
        .filter_map(|(x, pr)| pr.as_ref().map(|p| (x.label(), forge_state(p))))
        .collect();
    let steps = report
        .steps()
        .into_iter()
        .map(|s| check_step(s, &forge_notes))
        .collect();
    Ok(CheckMergeReport {
        base_sha: report.base.clone(),
        ok: report.is_clean(),
        steps,
    })
}

fn check_step(s: AssembleStep, forge_notes: &[(String, String)]) -> CheckMergeStep {
    let outcome = match s.outcome.as_str() {
        "already_in_base" => MergeOutcome::AlreadyInBase,
        "merged_upstream" => MergeOutcome::MergedUpstream,
        "squash_applied" => MergeOutcome::SquashApplied,
        "conflict" => MergeOutcome::Conflict,
        _ => MergeOutcome::Merged,
    };
    let note = match forge_notes.iter().find(|(label, _)| *label == s.label) {
        Some((_, state)) => format!("{} (forge: {state})", s.note),
        None => s.note,
    };
    CheckMergeStep {
        label: s.label,
        outcome,
        sha: s.sha,
        files: s.files,
        note,
    }
}

/// `build_env` (§15): what the build editor needs when it opens — the repo
/// presets, the CUDA default, the host's arch and driver CUDA, whether git
/// works, and where builds go (with the tmpfs warning). Every probe that
/// fails leaves its field empty rather than failing the call.
pub async fn build_env(state: &SharedState) -> BuildEnv {
    let runner = state.runtime().runner();
    let git = Git::new();
    let (arch, driver, git_ok) = tokio::join!(
        presets::detect_cuda_arch(runner.as_ref()),
        presets::detect_driver_cuda_version(runner.as_ref()),
        git::git_available(&git),
    );
    let builds_dir: PathBuf = state.builds_dir();
    BuildEnv {
        repo_presets: presets::REPO_PRESETS
            .iter()
            .map(|p| RepoPresetView {
                id: p.id.into(),
                name: p.name.into(),
                engine: p.engine,
                repo_url: p.repo_url.into(),
                forge: p.forge,
                default_ref: p.default_ref.into(),
            })
            .collect(),
        cuda_default: presets::DEFAULT_CUDA_VERSION.into(),
        arch_auto: arch.unwrap_or_default(),
        driver_cuda_max: driver.ok(),
        git_ok: git_ok.is_ok(),
        builds_dir: builds_dir.display().to_string(),
        builds_dir_warning: paths::tmpfs_refusal(&builds_dir),
        builds_dir_free_bytes: paths::free_bytes(&builds_dir),
    }
}
