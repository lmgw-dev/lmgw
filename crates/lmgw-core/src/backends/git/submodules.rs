//! Submodules of a run's worktree (container-builds design §14.1).
//!
//! Run worktrees are linked worktrees of the one shared bare pool, and git
//! keeps a superproject's `submodule.<name>.*` settings in its **common**
//! config — the pool's `config`, shared by every worktree of every
//! repository. `submodule init` (and so `update --init`) writes a
//! submodule's URL there and never overwrites one that is already set;
//! `submodule sync` writes there too. A URL written by one run was therefore
//! reused by every later run of any repository with a submodule of that name:
//! a wrong one stuck for good (audio.cpp's relative frontends URL, resolved
//! against the worktree's path by a build before 55a2d4ba), two repositories
//! sharing a submodule name got whichever was initialised first, and two
//! worktrees raced on the one file.
//!
//! So the top level is never initialised. Every time, its URLs come from the
//! worktree's own `.gitmodules` ([`Pool::gitmodules`]), resolved here, and
//! reach git as per-invocation `-c submodule.<name>.url=… -c
//! submodule.<name>.active=true` on `submodule update` (without `--init`):
//! command-line config wins over anything in the pool's config, and nothing
//! is written. What git keeps per submodule — its clone, under the
//! worktree's own `worktrees/<name>/modules/`, and that clone's
//! `remote.origin.url` — is per worktree, and set here directly.
//!
//! The nested levels are driven from `submodule foreach` *without* those
//! `-c`s (they reach git's children through the environment, where a nested
//! submodule of the same name would take the top level's URL). There, the
//! superproject is a submodule's own clone, so `sync` and `update --init`
//! write only into this worktree's `modules/`, and a nested relative URL
//! resolves against its parent submodule's own `origin`.

use std::path::Path;

use super::{commit_ref, is_full_sha, path_str, remote_key, Pool, Progress};

/// One `.gitmodules` entry: its name, and its URL as this build clones it
/// ([`resolve_submodule_url`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Submodule {
    name: String,
    url: String,
}

/// What a `.gitmodules` URL means for a build whose repository is
/// `repo_url`. An absolute URL is kept as it is (the hardening of every git
/// call applies to it: the SSH→HTTPS rewrite, and `file://` and plain paths
/// blocked). A relative one (`./`, `../`) resolves against `repo_url` the way
/// git's `resolve_relative_url` resolves it against a clone's `origin`: each
/// leading `../` drops one path component, the rest is appended. Unlike git,
/// climbing past the repository URL's host (`https://host`, `git@host:`) is
/// refused rather than turned into a local path, and so is a `.`/`..`
/// anywhere after the leading ones, which an HTTP client would fold away.
pub(crate) fn resolve_submodule_url(repo_url: Option<&str>, url: &str) -> Result<String, String> {
    if url.starts_with('-') {
        return Err(format!(
            "the submodule URL '{url}' starts with '-', which git would read as an option"
        ));
    }
    if url.chars().any(char::is_control) {
        return Err(format!(
            "the submodule URL '{}' contains a control character",
            url.escape_debug()
        ));
    }
    if !(url.starts_with("./") || url.starts_with("../")) {
        return Ok(url.to_string());
    }
    let Some(base) = repo_url else {
        return Err(format!(
            "the submodule URL '{url}' is relative to the build's repository, and this assemble \
             was given no repository URL to resolve it against"
        ));
    };
    let base = base.trim_end_matches('/');
    let (root, path) = split_url_root(base);
    let mut parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut rest = url;
    loop {
        if let Some(r) = rest.strip_prefix("../") {
            if parts.pop().is_none() {
                return Err(format!(
                    "the submodule URL '{url}' climbs above '{root}', the host of the build's \
                     repository {base}"
                ));
            }
            rest = r;
        } else if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else {
            break;
        }
    }
    let tail: Vec<&str> = rest.split('/').filter(|c| !c.is_empty()).collect();
    if tail.is_empty() || tail.iter().any(|c| *c == "." || *c == "..") {
        return Err(format!(
            "the submodule URL '{url}' is not a repository path relative to the build's \
             repository {base} (only leading ./ and ../ are resolved)"
        ));
    }
    parts.extend(tail);
    let sep = if path.starts_with('/') || root.contains("://") {
        "/"
    } else {
        ""
    };
    Ok(format!("{root}{sep}{}", parts.join("/")))
}

/// `(root, path)` of a repository URL: `https://host[:port]` and
/// `/owner/repo`, `file://` and `/abs/path`, `git@host:` and `owner/repo`.
/// The root is what a relative submodule URL may never climb above.
fn split_url_root(url: &str) -> (&str, &str) {
    if let Some(i) = url.find("://") {
        let after = i + 3;
        return match url[after..].find('/') {
            Some(j) => url.split_at(after + j),
            None => (url, ""),
        };
    }
    match url.find(':') {
        Some(i) if !url[..i].contains('/') => url.split_at(i + 1),
        _ => ("", url),
    }
}

/// A submodule name git can take as `-c submodule.<name>.url=…`: the first
/// `=` ends the key, and a control character makes it invalid.
fn check_submodule_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('=') || name.chars().any(char::is_control) {
        return Err(format!(
            "the submodule name '{}' cannot be passed to git on the command line (it is empty \
             or holds '=' or a control character); lmgw does not build such a tree yet",
            name.escape_debug()
        ));
    }
    Ok(())
}

/// `-c submodule.<name>.url=<url> -c submodule.<name>.active=true` for each.
fn overrides(subs: &[Submodule]) -> Vec<String> {
    let mut args = Vec::with_capacity(subs.len() * 4);
    for s in subs {
        args.push("-c".to_string());
        args.push(format!("submodule.{}.url={}", s.name, s.url));
        args.push("-c".to_string());
        args.push(format!("submodule.{}.active=true", s.name));
    }
    args
}

/// `'…'` for a POSIX shell (`submodule foreach` runs its command through one).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// NUL-terminated fields, in groups of `n`.
fn nul_groups(out: &str, n: usize) -> Vec<Vec<&str>> {
    let fields: Vec<&str> = out.split('\0').collect();
    fields.chunks_exact(n).map(<[&str]>::to_vec).collect()
}

impl Pool {
    /// The worktree's `.gitmodules` (as checked out now), each URL resolved
    /// against `repo_url` ([`resolve_submodule_url`]). Empty without one.
    pub(super) async fn gitmodules(
        &self,
        worktree: &Path,
        repo_url: Option<&str>,
    ) -> Result<Vec<Submodule>, String> {
        if !worktree.join(".gitmodules").is_file() {
            return Ok(Vec::new());
        }
        let out = self
            .git
            .run(
                Some(worktree),
                &[
                    "config",
                    "--file",
                    ".gitmodules",
                    "-z",
                    "--get-regexp",
                    r"^submodule\..*\.(url|path)$",
                ],
                None,
                None,
            )
            .await?;
        // 1: no key matched.
        if out.status == 1 {
            return Ok(Vec::new());
        }
        if !out.ok() {
            return Err(format!(
                "could not read {}/.gitmodules: {}",
                worktree.display(),
                out.said()
            ));
        }
        // Git only looks at entries whose path is a gitlink in the index and
        // ignores the rest, so an odd entry nothing points at must not fail
        // the run: only those get validated and passed on.
        let gitlinks = self.gitlink_paths(worktree).await?;
        let mut urls: Vec<(&str, &str)> = Vec::new();
        let mut paths: Vec<(&str, &str)> = Vec::new();
        // `-z`: `<key>\n<value>\0` per entry.
        for entry in out.stdout.split('\0').filter(|e| !e.is_empty()) {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            let Some(rest) = key.strip_prefix("submodule.") else {
                continue;
            };
            if let Some(name) = rest.strip_suffix(".url") {
                urls.push((name, value));
            } else if let Some(name) = rest.strip_suffix(".path") {
                paths.push((name, value));
            }
        }
        let mut subs: Vec<Submodule> = Vec::new();
        for &(name, url) in &urls {
            // The last one wins, as in git.
            let Some(&(_, path)) = paths.iter().rev().find(|(n, _)| *n == name) else {
                continue;
            };
            if !gitlinks.iter().any(|g| g == path) {
                continue;
            }
            check_submodule_name(name)?;
            let url = resolve_submodule_url(repo_url, url)
                .map_err(|e| format!("submodule {name}: {e}"))?;
            subs.retain(|s| s.name != name);
            subs.push(Submodule {
                name: name.to_string(),
                url,
            });
        }
        Ok(subs)
    }

    /// Paths of the gitlinks (mode 160000) in the worktree's index.
    async fn gitlink_paths(&self, worktree: &Path) -> Result<Vec<String>, String> {
        let out = self
            .git
            .run_ok(Some(worktree), &["ls-files", "--stage", "-z"], None, None)
            .await?;
        // `<mode> <sha> <stage>\t<path>\0`
        Ok(out
            .stdout
            .split('\0')
            .filter_map(|e| e.split_once('\t'))
            .filter(|(meta, _)| meta.starts_with("160000 "))
            .map(|(_, path)| path.to_string())
            .collect())
    }

    /// `submodule update [--force] [--reference=…]` of the top level with
    /// `subs`' URLs as per-invocation config (module docs), then
    /// `sync` + `update --init` with the same flags inside each submodule,
    /// level by level.
    pub(super) async fn submodules_update(
        &self,
        worktree: &Path,
        subs: &[Submodule],
        reference: Option<&str>,
        force: bool,
        progress: Option<Progress<'_>>,
    ) -> Result<(), String> {
        let wt = Some(worktree);
        let mut flags = Vec::new();
        if force {
            flags.push("--force");
        }
        if let Some(r) = reference {
            flags.push(r);
        }
        let config = overrides(subs);
        let mut top: Vec<&str> = config.iter().map(String::as_str).collect();
        top.extend(["submodule", "update"]);
        top.extend(&flags);
        self.git.run_ok(wt, &top, None, progress).await?;
        // `foreach --recursive` runs the command in a submodule and only then
        // descends, so each level syncs its children's URLs from its
        // `.gitmodules` as just updated (an extra may have moved a nested URL,
        // and `init` never overwrites one) before it updates them.
        let nested = format!(
            "git submodule sync && git submodule update --init {}",
            flags
                .iter()
                .map(|f| shell_quote(f))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let foreach = [
            "submodule",
            "foreach",
            "--quiet",
            "--recursive",
            nested.as_str(),
        ];
        self.git
            .run_ok(wt, &foreach, None, progress)
            .await
            .map(drop)
    }

    /// Point each checked-out top-level submodule's `origin` at its URL in
    /// `subs` — what `submodule sync` does, without the superproject half that
    /// writes the pool's common config. An extra may have moved a submodule
    /// to another URL; `update` fetches a missing commit from the
    /// submodule's own `origin`, and nested relative URLs resolve against it.
    /// The nested levels sync in [`Self::submodules_update`], after the update
    /// has moved their parents.
    pub(super) async fn submodules_sync(
        &self,
        worktree: &Path,
        subs: &[Submodule],
        progress: Option<Progress<'_>>,
    ) -> Result<(), String> {
        let wt = Some(worktree);
        // Only populated submodules are listed, so `-C <path>` is the
        // submodule's own repository, never the superproject.
        let listed = self
            .git
            .run_ok(
                wt,
                &[
                    "submodule",
                    "foreach",
                    "--quiet",
                    r#"printf '%s\0%s\0' "$name" "$sm_path""#,
                ],
                None,
                None,
            )
            .await?;
        for pair in nul_groups(&listed.stdout, 2) {
            let (name, path) = (pair[0], pair[1]);
            let Some(sub) = subs.iter().find(|s| s.name == name) else {
                continue;
            };
            let dir = worktree.join(path);
            if !dir.join(".git").exists() {
                continue;
            }
            self.git
                .run_ok(
                    Some(&dir),
                    &["config", "remote.origin.url", &sub.url],
                    None,
                    progress,
                )
                .await?;
        }
        Ok(())
    }

    /// Keep this run's submodule commits in the pool, so the next run's
    /// `submodule update --reference` finds their objects locally instead of
    /// re-cloning (§14.1: sd.cpp's four submodules are about 50 MB and 7 s a
    /// run). Best effort: a failure costs only the next run's download.
    ///
    /// Listed with `foreach`, which walks every checked-out submodule;
    /// `submodule status` would call the top level's uninitialised (the pool
    /// holds no `submodule.*` config, module docs).
    pub(super) async fn harvest_submodules(&self, worktree: &Path, progress: Option<Progress<'_>>) {
        let note = |line: String| {
            if let Some(p) = progress {
                p(&line);
            }
        };
        let listed = match self
            .git
            .run_ok(
                Some(worktree),
                &[
                    "submodule",
                    "foreach",
                    "--quiet",
                    "--recursive",
                    r#"printf '%s\0%s\0%s\0' "$(git rev-parse HEAD)" "$displaypath" "$(git config --get remote.origin.url)""#,
                ],
                None,
                None,
            )
            .await
        {
            Ok(o) => o.stdout,
            Err(e) => return note(format!("note: submodule objects not kept for reuse: {e}")),
        };
        for fields in nul_groups(&listed, 3) {
            let (sha, path, url) = (fields[0], fields[1], fields[2]);
            if !is_full_sha(sha) || url.is_empty() {
                continue;
            }
            let local = commit_ref(&remote_key(url), sha);
            if self.ref_exists(&local).await {
                continue;
            }
            let sub = worktree.join(path);
            let Ok(sub_path) = path_str(&sub) else {
                continue;
            };
            let refspec = format!("+{sha}:{local}");
            let _guard = match self.lock_pool().await {
                Ok(g) => g,
                Err(e) => {
                    note(format!("note: submodule {path} not kept for reuse: {e}"));
                    continue;
                }
            };
            let fetched = self
                .git
                .run_ok(
                    Some(&self.dir),
                    &[
                        "fetch",
                        "--no-tags",
                        "--no-write-fetch-head",
                        "--quiet",
                        "--",
                        sub_path,
                        &refspec,
                    ],
                    None,
                    None,
                )
                .await;
            if let Err(e) = fetched {
                note(format!("note: submodule {path} not kept for reuse: {e}"));
            }
        }
    }

    /// Remove every `submodule.*` section from the pool's config. Builds
    /// before this module wrote them there (`update --init`, `sync`), where
    /// they would sit unused now; removed so an existing install heals and
    /// the config says what is in force. Called under the pool lock. Best
    /// effort: they no longer change what a run clones.
    pub(super) async fn clear_submodule_config(&self) {
        let out = match self
            .in_pool(&[
                "config",
                "--local",
                "-z",
                "--name-only",
                "--get-regexp",
                r"^submodule\.",
            ])
            .await
        {
            Ok(o) if o.ok() => o,
            // 1: none there.
            Ok(o) if o.status == 1 => return,
            Ok(o) => {
                return tracing::warn!(
                    "build pool: could not read its config for old submodule entries: {}",
                    o.said()
                )
            }
            Err(e) => {
                return tracing::warn!(
                    "build pool: could not read its config for old submodule entries: {e}"
                )
            }
        };
        let mut sections: Vec<&str> = Vec::new();
        for key in out.stdout.split('\0').filter(|k| !k.is_empty()) {
            let section = key.rsplit_once('.').map_or(key, |(s, _)| s);
            if !sections.contains(&section) {
                sections.push(section);
            }
        }
        for section in sections {
            match self
                .in_pool_ok(&["config", "--local", "--remove-section", section])
                .await
            {
                Ok(_) => tracing::info!(
                    "build pool: removed [{section}] from its config, left by an older build \
                     (submodule URLs now come from each run's own .gitmodules)"
                ),
                Err(e) => tracing::warn!("build pool: could not remove [{section}]: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls_resolve_like_git_against_the_repository_url() {
        let audio = Some("https://github.com/0xShug0/audio.cpp");
        assert_eq!(
            resolve_submodule_url(audio, "../../0xShug0/audio.cpp-server-frontends.git").unwrap(),
            "https://github.com/0xShug0/audio.cpp-server-frontends.git"
        );
        assert_eq!(
            resolve_submodule_url(audio, "../frontends.git").unwrap(),
            "https://github.com/0xShug0/frontends.git"
        );
        assert_eq!(
            resolve_submodule_url(audio, "./sub").unwrap(),
            "https://github.com/0xShug0/audio.cpp/sub"
        );
        assert_eq!(
            resolve_submodule_url(Some("https://github.com/a/b.git/"), "../c.git").unwrap(),
            "https://github.com/a/c.git"
        );
        assert_eq!(
            resolve_submodule_url(Some("git@github.com:a/b"), "../../x/y.git").unwrap(),
            "git@github.com:x/y.git"
        );
        assert_eq!(
            resolve_submodule_url(Some("ssh://git@host:2222/a/b"), "../c").unwrap(),
            "ssh://git@host:2222/a/c"
        );
        assert_eq!(
            resolve_submodule_url(Some("file:///srv/git/owner/super.git"), "../../o/s.git")
                .unwrap(),
            "file:///srv/git/o/s.git"
        );
    }

    #[test]
    fn absolute_urls_are_kept_and_bad_ones_refused() {
        assert_eq!(
            resolve_submodule_url(None, "git@github.com:leejet/ggml.git").unwrap(),
            "git@github.com:leejet/ggml.git"
        );
        assert!(resolve_submodule_url(None, "-uhttps://x").is_err());
        assert!(resolve_submodule_url(None, "https://x/\ny").is_err());
        assert!(
            resolve_submodule_url(None, "../x.git").is_err(),
            "relative needs a base"
        );
    }

    #[test]
    fn a_relative_url_never_climbs_past_the_host_into_a_local_path() {
        let base = Some("https://github.com/a/b");
        assert_eq!(
            resolve_submodule_url(base, "../../c/d.git").unwrap(),
            "https://github.com/c/d.git"
        );
        for bad in [
            "../../../d.git",
            "../../../../etc",
            "../x/../../../y",
            "../",
            "../..",
        ] {
            assert!(
                resolve_submodule_url(base, bad).is_err(),
                "{bad} must be refused"
            );
        }
        assert!(resolve_submodule_url(Some("git@github.com:a/b"), "../../../x").is_err());
        assert!(resolve_submodule_url(Some("file:///r"), "../../x").is_err());
    }

    #[test]
    fn names_that_break_a_dash_c_are_refused() {
        assert!(check_submodule_name("examples/server/frontend").is_ok());
        assert!(check_submodule_name("a.b").is_ok());
        assert!(check_submodule_name("a=b").is_err());
        assert!(check_submodule_name("a\nb").is_err());
        assert!(check_submodule_name("").is_err());
    }
}
