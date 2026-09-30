//! The git layer of container builds (container-builds design §5, §7, §10,
//! §14.1): one object pool for every remote, ref resolution and fetch into
//! namespaced refs, a worktree per run, and **assemble** — base plus the
//! extras merged (or squash-applied) in order, with submodules updated in the
//! order the spike found to work.
//!
//! # Hardening (§10)
//!
//! Every call is the real `git` binary with argv, never a shell, and
//! [`Git::command`] is the one place that argv and environment are built:
//!
//! - `GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS=` and `SSH_ASKPASS=` (this
//!   session sets `ksshaskpass`), and the child gets its own session
//!   (`setsid`), so neither git nor ssh can reach a terminal to prompt on —
//!   a run that needs credentials fails, visibly, instead of hanging;
//! - `-c credential.helper=` (it also silences the URL-scoped
//!   `!gh auth git-credential` of the global config, verified in the spike)
//!   and the SSH→HTTPS rewrite for github.com, which the `-c` hands down to
//!   submodule clones too (audio.cpp's frontend submodule is an SSH URL);
//! - the user's own config otherwise stays in force (proxies, CA bundles),
//!   except what would change what a run does: hooks, commit signing,
//!   fsmonitor, `submodule.recurse`, rerere, colour, localized messages;
//! - a forge token reaches git only as `http.<url>.extraHeader` through
//!   `GIT_CONFIG_COUNT`/`KEY_0`/`VALUE_0` in the child's environment — never
//!   argv, which is what gets logged;
//! - `file://` (and plain-path) submodule URLs stay blocked, as git does by
//!   default: a PR's `.gitmodules` could otherwise copy any repository on
//!   this machine into the build context.
//!
//! # The pool (§14.1)
//!
//! One bare repository, `<builds_dir>/git/pool.git`, holds every remote's
//! objects under `refs/lmgw/<remote key>/{heads,tags,commits,pr}/…`, always
//! fetched `--no-tags`. ik_llama.cpp and llama.cpp then share blobs. Pool
//! mutations (fetch, ref updates, worktree add/remove/prune) take the **pool
//! lock**: an async mutex every [`Pool`] on the same directory in this
//! process shares, then a `flock` on `<builds_dir>/git/pool.lock` that every
//! process shares — so a Check merge or a Resolve of any instance waits for
//! a fetch, not for a whole build (which holds the machine-wide build lock
//! for its duration). A git killed mid-mutation (a canceled run drops its
//! future, and git with it) leaves its `*.lock` files; whoever takes the
//! pool lock next knows they are stale and removes them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use base64::Engine as _;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::model::Forge;
use super::presets;
use super::validate;

/// `merge-tree --write-tree --merge-base` (the squash-apply) needs 2.40.
pub const MIN_GIT_VERSION: (u32, u32) = (2, 40);

/// Author and committer of every commit lmgw makes (§5 step 3).
pub const LMGW_NAME: &str = "lmgw";
pub const LMGW_EMAIL: &str = "lmgw@localhost";

/// Where progress lines go: one line per call, already without its newline.
/// The run executor forwards them into the run log.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Configuration every call carries (see the module docs). `gc.autoDetach`
/// and `maintenance.autoDetach` keep an automatic gc inside the git call that
/// triggered it — under the pool lock, and killed with it — rather than in a
/// detached child that could hold `packed-refs.lock` after the lock is gone.
const HARDENING: [&str; 16] = [
    "credential.helper=",
    "url.https://github.com/.insteadOf=git@github.com:",
    "url.https://github.com/.insteadOf=ssh://git@github.com/",
    "core.hooksPath=/dev/null",
    "commit.gpgSign=false",
    "core.fsmonitor=false",
    "submodule.recurse=false",
    "rerere.enabled=false",
    "color.ui=false",
    "core.quotePath=false",
    "log.showSignature=false",
    "core.autocrlf=false",
    "advice.detachedHead=false",
    "advice.submoduleMergeConflict=false",
    "gc.autoDetach=false",
    "maintenance.autoDetach=false",
];

/// Inherited variables that would point git at another repository or inject
/// configuration — present when lmgw (or its test suite) runs under a git
/// hook, for one.
const SCRUBBED_ENV: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "LANGUAGE",
];

// ---------------------------------------------------------------------------
// Invocation
// ---------------------------------------------------------------------------

/// A forge token as git gets it: an `Authorization` header scoped to one
/// `scheme://host/`. `Debug` never prints it.
#[derive(Clone)]
pub struct GitAuth {
    url_prefix: String,
    header: SecretString,
}

impl std::fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitAuth")
            .field("url_prefix", &self.url_prefix)
            .field("header", &"[redacted]")
            .finish()
    }
}

impl GitAuth {
    /// The header for `url`'s host (§7 "Tokens"): HTTP Basic, user
    /// `x-access-token` on GitHub and `oauth2` elsewhere (GitLab, Gitea),
    /// the token as password. `None` for a URL git will not reach over
    /// HTTP(S) — an SSH remote other than github.com, `file://` — where a
    /// header means nothing.
    pub fn new(url: &str, token: &str, forge: Forge) -> Option<Self> {
        let url_prefix = http_prefix(url)?;
        let user = match forge {
            Forge::Github => "x-access-token",
            Forge::Gitlab | Forge::Plain => "oauth2",
        };
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
        Some(Self {
            url_prefix,
            header: SecretString::from(format!("Authorization: Basic {basic}")),
        })
    }

    /// `https://host/` — the `http.<url>` key the header is scoped to.
    pub fn url_prefix(&self) -> &str {
        &self.url_prefix
    }
}

/// `scheme://host[:port]/` of an HTTP(S) URL; github.com's SSH spellings
/// count, since [`HARDENING`] rewrites them to HTTPS.
fn http_prefix(url: &str) -> Option<String> {
    for scheme in ["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host = authority.rsplit('@').next().unwrap_or("");
            return (!host.is_empty()).then(|| format!("{scheme}{}/", host.to_ascii_lowercase()));
        }
    }
    let github_ssh = url.starts_with("git@github.com:") || url.starts_with("ssh://git@github.com/");
    github_ssh.then(|| "https://github.com/".to_string())
}

/// How lmgw runs git: which binary, and whether local-file submodule URLs are
/// allowed.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    allow_file_protocol: bool,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            allow_file_protocol: false,
        }
    }
}

/// One finished git call. `stdout` is exact (lossily decoded), not re-joined
/// lines, so a blob read through it is the blob.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    fn said(&self) -> &str {
        match self.stderr.trim() {
            "" => self.stdout.trim(),
            e => e,
        }
    }
}

impl Git {
    /// `git` from `PATH`.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Allow `file://` and plain-path URLs for submodules
    /// (`protocol.file.allow=always`). **Off in production** — see the module
    /// docs. For tests whose fixture repositories are local, and nothing else.
    pub fn allow_file_protocol(mut self) -> Self {
        self.allow_file_protocol = true;
        self
    }

    /// The hardened command for `git [-C dir] <args>`, not yet spawned: stdin
    /// closed, stdout and stderr piped, killed when dropped. Public so the
    /// hardening can be inspected — nothing else should build a git command.
    pub fn command(&self, dir: Option<&Path>, args: &[&str], auth: Option<&GitAuth>) -> Command {
        let mut cmd = Command::new(&self.program);
        if let Some(d) = dir {
            cmd.arg("-C").arg(d);
        }
        for c in HARDENING {
            cmd.arg("-c").arg(c);
        }
        if self.allow_file_protocol {
            cmd.arg("-c").arg("protocol.file.allow=always");
        }
        cmd.args(args);
        for var in SCRUBBED_ENV {
            cmd.env_remove(var);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            // Messages are matched below ("couldn't find remote ref", …) and
            // the session's locale is German.
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", LMGW_NAME)
            .env("GIT_AUTHOR_EMAIL", LMGW_EMAIL)
            .env("GIT_COMMITTER_NAME", LMGW_NAME)
            .env("GIT_COMMITTER_EMAIL", LMGW_EMAIL);
        if let Some(a) = auth {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("http.{}.extraHeader", a.url_prefix),
                )
                .env("GIT_CONFIG_VALUE_0", a.header.expose_secret());
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // SAFETY: `setsid` is async-signal-safe and touches no memory of
            // the parent; it is the only thing run between fork and exec.
            // Detaching from the controlling terminal is what makes an ssh
            // host-key or passphrase prompt fail instead of blocking a run.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        cmd
    }

    /// Run `git [-C dir] <args>` to completion. Any exit status is `Ok`;
    /// `Err` only when git could not be started. With `echo`, the command
    /// line (never the token, which is not in it) and every output line go
    /// there as they arrive.
    pub async fn run(
        &self,
        dir: Option<&Path>,
        args: &[&str],
        auth: Option<&GitAuth>,
        echo: Option<Progress<'_>>,
    ) -> Result<GitOutput, String> {
        if let Some(p) = echo {
            // A URL in the argv never carries credentials (validate refuses
            // them), and is redacted anyway: this line goes into the run log.
            let shown: Vec<String> = args.iter().map(|a| validate::redact_url(a)).collect();
            p(&format!("$ git {}", shown.join(" ")));
        }
        let mut child = self.command(dir, args, auth).spawn().map_err(spawn_error)?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let (out, err, status) = tokio::join!(
            read_stream(stdout, echo),
            read_stream(stderr, echo),
            child.wait()
        );
        let status = status.map_err(|e| format!("waiting for git failed: {e}"))?;
        Ok(GitOutput {
            status: status.code().unwrap_or(-1),
            stdout: out,
            stderr: err,
        })
    }

    /// [`Self::run`], with a non-zero exit turned into an error that names
    /// the command and quotes git.
    pub async fn run_ok(
        &self,
        dir: Option<&Path>,
        args: &[&str],
        auth: Option<&GitAuth>,
        echo: Option<Progress<'_>>,
    ) -> Result<GitOutput, String> {
        let out = self.run(dir, args, auth, echo).await?;
        if !out.ok() {
            return Err(format!(
                "git {} failed (exit {}): {}",
                args.first().copied().unwrap_or(""),
                out.status,
                out.said()
            ));
        }
        Ok(out)
    }

    /// `git --version` as `(major, minor, patch)`.
    pub async fn version(&self) -> Result<(u32, u32, u32), String> {
        let out = self.run_ok(None, &["--version"], None, None).await?;
        parse_version(&out.stdout)
            .ok_or_else(|| format!("unexpected `git --version` output: {}", out.stdout.trim()))
    }

    /// Heads and tags of `url`, plus which branch its `HEAD` is (§4 "ref"
    /// suggestions, update checks). Two narrow requests rather than one bare
    /// `ls-remote`, which on llama.cpp lists tens of thousands of
    /// `refs/pull/*`.
    pub async fn ls_remote(&self, url: &str, auth: Option<&GitAuth>) -> Result<RemoteRefs, String> {
        validate::validate_repo_url("url", url)?;
        let heads_tags = ["ls-remote", "--heads", "--tags", "--", url];
        let symref = ["ls-remote", "--symref", "--", url, "HEAD"];
        let (a, b) = tokio::join!(
            self.run(None, &heads_tags, auth, None),
            self.run(None, &symref, auth, None)
        );
        let (a, b) = (a?, b?);
        for out in [&a, &b] {
            if !out.ok() {
                return Err(remote_error(url, "git ls-remote", out));
            }
        }
        Ok(RemoteRefs::parse(&a.stdout, &b.stdout))
    }

    /// Exactly the refs named by `patterns` on `url` (full ref names:
    /// `refs/heads/master`, `refs/tags/b7000`, `refs/tags/b7000^{}` for a
    /// tag's peeled commit, `refs/pull/9/head`), as `name → sha`. One
    /// `ls-remote` whose patterns become ref prefixes on the wire, so the
    /// remote sends only those refs — what the update check asks each remote
    /// (§8), rather than every head and tag of llama.cpp. A pattern that
    /// matches nothing is simply absent from the answer.
    pub async fn ls_remote_refs(
        &self,
        url: &str,
        patterns: &[String],
        auth: Option<&GitAuth>,
    ) -> Result<HashMap<String, String>, String> {
        validate::validate_repo_url("url", url)?;
        let mut args = vec!["ls-remote", "--", url];
        args.extend(patterns.iter().map(String::as_str));
        let out = self.run(None, &args, auth, None).await?;
        if !out.ok() {
            return Err(remote_error(url, "git ls-remote", &out));
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(_, name)| patterns.iter().any(|p| p == name))
            .map(|(sha, name)| (name.to_string(), sha.to_string()))
            .collect())
    }
}

fn spawn_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "git is not installed (or not on PATH) — the Backends page needs it: `sudo dnf install \
         git`"
            .into()
    } else {
        format!("git could not be started: {e}")
    }
}

/// Read a pipe to its end, keeping the bytes exact and echoing each line.
async fn read_stream<R: AsyncRead + Unpin>(r: R, echo: Option<Progress<'_>>) -> String {
    let mut reader = BufReader::new(r);
    let mut all: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                all.extend_from_slice(&line);
                if let Some(p) = echo {
                    let text = String::from_utf8_lossy(&line);
                    p(text.trim_end_matches(['\n', '\r']));
                }
            }
        }
    }
    String::from_utf8_lossy(&all).into_owned()
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let v = s.trim().strip_prefix("git version ")?;
    let mut nums = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u32>().ok());
    Some((
        nums.next()??,
        nums.next()??,
        nums.next().flatten().unwrap_or(0),
    ))
}

/// Whether git is there and new enough (§10 "git dependency"): the version
/// string, or the sentence the Backends page shows.
pub async fn git_available(git: &Git) -> Result<String, String> {
    let (ma, mi, pa) = git.version().await?;
    let (need_ma, need_mi) = MIN_GIT_VERSION;
    if (ma, mi) < MIN_GIT_VERSION {
        return Err(format!(
            "git {ma}.{mi}.{pa} is too old — container builds need git {need_ma}.{need_mi} or \
             newer (for `merge-tree --write-tree --merge-base`)"
        ));
    }
    Ok(format!("{ma}.{mi}.{pa}"))
}

/// A failed remote operation, worded for the owner. The URL is redacted, as
/// is git's own message (which may quote it).
fn remote_error(url: &str, what: &str, out: &GitOutput) -> String {
    let url = &validate::redact_url(url);
    let said = &redact_words(out.said());
    let lower = said.to_ascii_lowercase();
    if lower.contains("could not read username")
        || lower.contains("authentication failed")
        || lower.contains("terminal prompts disabled")
        || lower.contains("permission denied (publickey")
    {
        let host = validate::repo_host(url).unwrap_or_else(|| url.to_string());
        return format!(
            "{url} asks for credentials — if it is a private repository, add a forge token for \
             {host} (Settings); git said: {said}"
        );
    }
    format!("{what} {url} failed (exit {}): {said}", out.status)
}

/// Every URL-looking word of `text` passed through
/// [`validate::redact_url`].
fn redact_words(text: &str) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    text.split(' ')
        .map(|w| {
            let quoted = w.trim_matches(|c| c == '\'' || c == '"');
            if quoted.contains("://") && quoted.contains('@') {
                w.replace(quoted, &validate::redact_url(quoted))
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Remote refs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    pub name: String,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTag {
    pub name: String,
    /// What the tag ref points at: the tag object for an annotated tag.
    pub sha: String,
    /// The commit (`^{}`, peeled); equal to `sha` for a lightweight tag.
    pub commit: String,
}

impl RemoteTag {
    pub fn annotated(&self) -> bool {
        self.sha != self.commit
    }
}

/// What `ls-remote` says about a remote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRefs {
    /// The branch `HEAD` points at: the remote's default branch.
    pub default_branch: Option<String>,
    pub head: Option<String>,
    pub heads: Vec<RemoteRef>,
    pub tags: Vec<RemoteTag>,
}

impl RemoteRefs {
    /// From `ls-remote --heads --tags` and `ls-remote --symref <url> HEAD`.
    pub fn parse(heads_tags: &str, symref: &str) -> Self {
        let mut refs = Self::default();
        let mut peeled: HashMap<String, String> = HashMap::new();
        for line in heads_tags.lines() {
            let Some((sha, name)) = line.split_once('\t') else {
                continue;
            };
            if let Some(b) = name.strip_prefix("refs/heads/") {
                refs.heads.push(RemoteRef {
                    name: b.into(),
                    sha: sha.into(),
                });
            } else if let Some(t) = name.strip_prefix("refs/tags/") {
                match t.strip_suffix("^{}") {
                    Some(t) => {
                        peeled.insert(t.into(), sha.into());
                    }
                    None => refs.tags.push(RemoteTag {
                        name: t.into(),
                        sha: sha.into(),
                        commit: sha.into(),
                    }),
                }
            }
        }
        for t in &mut refs.tags {
            if let Some(c) = peeled.remove(&t.name) {
                t.commit = c;
            }
        }
        for line in symref.lines() {
            if let Some(rest) = line.strip_prefix("ref: ") {
                if let Some((target, "HEAD")) = rest.split_once('\t') {
                    refs.default_branch = target.strip_prefix("refs/heads/").map(str::to_string);
                }
            } else if let Some((sha, "HEAD")) = line.split_once('\t') {
                refs.head = Some(sha.into());
            }
        }
        refs
    }

    pub fn branch(&self, name: &str) -> Option<&str> {
        self.heads
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.sha.as_str())
    }

    pub fn tag(&self, name: &str) -> Option<&RemoteTag> {
        self.tags.iter().find(|t| t.name == name)
    }

    /// The distinct commits at a branch or tag tip whose SHA starts with
    /// `prefix` — how a short SHA is resolved (§14.1: short SHAs cannot be
    /// fetched).
    pub fn tips_starting_with(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        let mut out: Vec<String> = Vec::new();
        let tips = self
            .heads
            .iter()
            .map(|b| &b.sha)
            .chain(self.tags.iter().map(|t| &t.commit));
        for sha in tips {
            if sha.starts_with(&prefix) && !out.contains(sha) {
                out.push(sha.clone());
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// A remote's namespace in the pool: the first 16 hex of sha256 over its
/// URL, normalized so `git@github.com:o/r.git` and `https://GitHub.com/o/r/`
/// share one — they are the same remote.
pub fn remote_key(url: &str) -> String {
    let web = presets::web_url(url);
    let normalized = match web.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            format!(
                "{}://{}/{path}",
                scheme.to_ascii_lowercase(),
                authority.to_ascii_lowercase()
            )
        }
        None => web,
    };
    hex::encode(Sha256::digest(normalized.as_bytes()))[..16].to_string()
}

fn commit_ref(key: &str, sha: &str) -> String {
    format!("refs/lmgw/{key}/commits/{sha}")
}

/// `refs/lmgw/<key>/` of a pool ref, `None` for one outside the namespaces.
fn remote_namespace(local: &str) -> Option<&str> {
    let rest = local.strip_prefix("refs/lmgw/")?;
    let key_len = rest.find('/')?;
    Some(&local[.."refs/lmgw/".len() + key_len + 1])
}

/// Whether the existing ref `existing` stands in the way of writing `local`:
/// one is a path prefix of the other at a `/` (`…/heads/a` vs `…/heads/a/b`).
fn ref_conflict(existing: &str, local: &str) -> bool {
    let under =
        |a: &str, b: &str| a.len() > b.len() && a.starts_with(b) && a.as_bytes()[b.len()] == b'/';
    under(local, existing) || under(existing, local)
}

pub(crate) fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && is_hex(s)
}

/// The first 7 characters of a SHA, for messages and tags.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// What to resolve: the build's `ref` (or a `ref` extra's), or a PR/MR number
/// of the build's own repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    /// A branch, tag, full commit SHA, a short SHA of a branch or tag tip, or
    /// a full ref (`refs/pull/123/head`).
    Ref(String),
    /// GitHub `refs/pull/N/head`, GitLab `refs/merge-requests/N/head`.
    Pr(u64),
}

/// What a target turned out to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefKind {
    Branch {
        name: String,
    },
    Tag {
        name: String,
        annotated: bool,
    },
    Commit,
    Pr {
        number: u64,
    },
    /// A full ref name that is neither a head nor a tag.
    Ref {
        name: String,
    },
    /// A pin: exactly the given commit, whatever the target is now.
    Pinned,
}

/// A resolved, fetched target: its commit and the pool ref that keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetched {
    pub sha: String,
    pub local_ref: String,
    pub kind: RefKind,
}

/// A target, resolved to what to fetch.
struct TargetPlan {
    kind: RefKind,
    /// Refspec source: a ref name or a full SHA.
    source: String,
    local: String,
    /// The commit, when already known from `ls-remote` (lets a fetch be
    /// skipped when the pool has it).
    known: Option<String>,
    /// How to name it in an error.
    what: String,
}

type PoolLock = Arc<tokio::sync::Mutex<()>>;

/// The held pool lock (see the module docs): the process's mutex, then the
/// cross-process flock. Dropping it releases both.
pub struct PoolGuard {
    _flock: std::fs::File,
    _mutex: tokio::sync::OwnedMutexGuard<()>,
}

static POOL_LOCKS: LazyLock<Mutex<HashMap<PathBuf, PoolLock>>> = LazyLock::new(Default::default);

/// The process-wide mutex of the pool at `dir`, shared by every [`Pool`] on it.
fn pool_lock(dir: &Path) -> PoolLock {
    let mut map = POOL_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    map.entry(dir.to_path_buf()).or_default().clone()
}

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// A directory name no other run or check in this or an earlier process uses.
fn unique_name(prefix: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "{prefix}-{}-{millis}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

fn path_str(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("{} is not a UTF-8 path", p.display()))
}

/// The builds' shared object pool (§14.1).
#[derive(Debug, Clone)]
pub struct Pool {
    git: Git,
    dir: PathBuf,
    work: PathBuf,
    lock: PoolLock,
    /// The gateway's instance id, in the names of its Check merge trees
    /// (`check-<instance>-<pid>-…`), so the boot sweep collects only its
    /// own. Empty until [`Self::with_instance`].
    instance: String,
}

impl Pool {
    /// The pool under `builds_dir` (`git/pool.git`), created on first use.
    pub async fn open(git: Git, builds_dir: &Path) -> Result<Self, String> {
        let dir = builds_dir.join("git").join("pool.git");
        let pool = Self {
            git,
            lock: pool_lock(&dir),
            work: builds_dir.join("work"),
            dir,
            instance: String::new(),
        };
        let _guard = pool.lock_pool().await?;
        if !pool.dir.join("HEAD").exists() {
            std::fs::create_dir_all(&pool.dir)
                .map_err(|e| format!("could not create {}: {e}", pool.dir.display()))?;
            let dir = path_str(&pool.dir)?;
            pool.git
                .run_ok(
                    None,
                    &[
                        "init",
                        "--bare",
                        "--quiet",
                        "--initial-branch=lmgw-pool",
                        "--",
                        dir,
                    ],
                    None,
                    None,
                )
                .await?;
        }
        drop(_guard);
        Ok(pool)
    }

    /// Name this gateway's instance in the pool's throwaway trees.
    pub fn with_instance(mut self, instance: &str) -> Self {
        self.instance = instance.to_string();
        self
    }

    /// `<builds_dir>/git/pool.lock`.
    fn lock_file(&self) -> PathBuf {
        self.dir.with_file_name("pool.lock")
    }

    /// Take the pool lock (module docs), then clear what a git killed while
    /// holding it left behind ([`Self::clear_stale_locks`]).
    pub async fn lock_pool(&self) -> Result<PoolGuard, String> {
        let mutex = self.lock.clone().lock_owned().await;
        let path = self.lock_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("could not open the pool lock {}: {e}", path.display()))?;
        // Blocking, so off the async threads; at most one waiter per pool
        // per process, since the mutex is held.
        let flock = tokio::task::spawn_blocking(move || file.lock().map(|()| file))
            .await
            .map_err(|e| format!("waiting for the pool lock: {e}"))?
            .map_err(|e| format!("could not take the pool lock {}: {e}", path.display()))?;
        self.clear_stale_locks();
        Ok(PoolGuard {
            _flock: flock,
            _mutex: mutex,
        })
    }

    /// Remove git's lock files from the pool's shared parts — its top level
    /// (`packed-refs.lock`, `config.lock`, `shallow.lock`, …) and every ref
    /// under `refs/` — and the temporary packs of an interrupted fetch.
    /// Called with the pool lock just taken: every git that mutates these
    /// holds it, so any lock file there now belongs to one that was killed.
    /// A worktree's own files (`worktrees/<name>/…`) are left alone: those
    /// belong to its run or check, which is not under the pool lock while
    /// it merges.
    fn clear_stale_locks(&self) {
        let mut stale: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            stale.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "lock")),
            );
        }
        let mut dirs = vec![self.dir.join("refs")];
        while let Some(d) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.filter_map(Result::ok) {
                let p = e.path();
                match e.file_type() {
                    Ok(t) if t.is_dir() => dirs.push(p),
                    Ok(_) if p.extension().is_some_and(|x| x == "lock") => stale.push(p),
                    _ => {}
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.dir.join("objects").join("pack")) {
            stale.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("tmp_"))
                    }),
            );
        }
        for p in stale {
            match std::fs::remove_file(&p) {
                Ok(()) => tracing::info!(
                    "build pool: removed {}, left by a git that was killed",
                    p.display()
                ),
                Err(e) => tracing::warn!("build pool: could not remove {}: {e}", p.display()),
            }
        }
    }

    /// Delete the pool refs that would stop `local` from being written — a
    /// directory/file conflict. The remote deleted branch `a` and created
    /// `a/b`: the pool's `…/heads/a` is a file where `…/heads/a/b` needs a
    /// directory (and the reverse: `…/heads/a/b` blocks a new `…/heads/a`).
    /// Only refs of `local`'s own remote namespace are ever touched, and the
    /// pool lock must be held.
    async fn clear_ref_conflicts(&self, local: &str) -> Result<(), String> {
        let Some(ns) = remote_namespace(local) else {
            return Ok(());
        };
        let out = self
            .in_pool_ok(&["for-each-ref", "--format=%(refname)", ns])
            .await?;
        for existing in out.stdout.lines().map(str::trim) {
            if ref_conflict(existing, local) {
                self.in_pool_ok(&["update-ref", "-d", existing]).await?;
                tracing::info!(
                    "build pool: deleted {existing}, which stood in the way of {local} (the \
                     remote renamed a branch or tag across a '/')"
                );
            }
        }
        Ok(())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<builds_dir>/work` — where run worktrees and check-merge trees go.
    pub fn work_dir(&self) -> &Path {
        &self.work
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    async fn in_pool(&self, args: &[&str]) -> Result<GitOutput, String> {
        self.git.run(Some(&self.dir), args, None, None).await
    }

    async fn in_pool_ok(&self, args: &[&str]) -> Result<GitOutput, String> {
        self.git.run_ok(Some(&self.dir), args, None, None).await
    }

    /// Whether the pool holds `sha` as a commit.
    pub async fn has_commit(&self, sha: &str) -> bool {
        let obj = format!("{sha}^{{commit}}");
        self.in_pool(&["cat-file", "-e", &obj])
            .await
            .is_ok_and(|o| o.ok())
    }

    async fn ref_exists(&self, name: &str) -> bool {
        self.in_pool(&["rev-parse", "--verify", "--quiet", "--end-of-options", name])
            .await
            .is_ok_and(|o| o.ok())
    }

    /// `rev-parse --verify` in `dir` (the pool when `None`).
    async fn rev_parse(&self, dir: Option<&Path>, rev: &str) -> Result<String, String> {
        let dir = dir.unwrap_or(&self.dir);
        let out = self
            .git
            .run(
                Some(dir),
                &["rev-parse", "--verify", "--quiet", "--end-of-options", rev],
                None,
                None,
            )
            .await?;
        if !out.ok() {
            return Err(format!("{rev} does not resolve to an object"));
        }
        Ok(out.stdout.trim().to_string())
    }

    async fn update_ref(&self, name: &str, sha: &str) -> Result<(), String> {
        let _guard = self.lock_pool().await?;
        self.clear_ref_conflicts(name).await?;
        self.in_pool_ok(&["update-ref", name, sha])
            .await
            .map(|_| ())
    }

    async fn fetch(
        &self,
        url: &str,
        refspecs: &[String],
        what: &str,
        auth: Option<&GitAuth>,
        progress: Option<Progress<'_>>,
    ) -> Result<(), String> {
        let mut args = vec!["fetch", "--no-tags", "--no-write-fetch-head", "--", url];
        args.extend(refspecs.iter().map(String::as_str));
        let _guard = self.lock_pool().await?;
        for spec in refspecs {
            if let Some((_, local)) = spec.split_once(':') {
                self.clear_ref_conflicts(local).await?;
            }
        }
        let out = self.git.run(Some(&self.dir), &args, auth, progress).await?;
        if out.ok() {
            return Ok(());
        }
        let said = out.said();
        if said.contains("couldn't find remote ref") {
            return Err(format!("{what} does not exist on {url}"));
        }
        if said.contains("not our ref") || said.contains("unadvertised object") {
            return Err(format!(
                "{url} will not serve {what} on its own (it is not reachable from any of its \
                 branches or PRs, or the server does not allow fetching commits by SHA)"
            ));
        }
        Err(remote_error(url, &format!("git fetch {what} from"), &out))
    }

    /// Resolve `target` on `url` to a commit and fetch it into the pool
    /// (§5 phase 1, §14.1). With `pin`, the result is exactly that commit:
    /// taken from the pool when it is there, else fetched with the target
    /// (which normally contains it), else fetched by SHA.
    ///
    /// Branches win over tags of the same name. A short SHA resolves only
    /// against branch and tag tips. A fetch is skipped when `ls-remote`
    /// named a commit the pool already has — the ref is moved locally.
    pub async fn resolve_and_fetch(
        &self,
        url: &str,
        target: &FetchTarget,
        pin: Option<&str>,
        forge: Forge,
        auth: Option<&GitAuth>,
        progress: Option<Progress<'_>>,
    ) -> Result<Fetched, String> {
        validate::validate_repo_url("url", url)?;
        let key = remote_key(url);
        match pin {
            None => {
                let plan = self.plan(url, &key, target, forge, auth).await?;
                let sha = self.fetch_plan(url, &plan, auth, progress).await?;
                Ok(Fetched {
                    sha,
                    local_ref: plan.local,
                    kind: plan.kind,
                })
            }
            Some(pin) => {
                let pin = validate::validate_pin("pin", pin)?;
                let local = commit_ref(&key, &pin);
                if !self.has_commit(&pin).await {
                    // The target first: it normally contains the pin. Its own
                    // failure only matters if the pin cannot be had either.
                    let via_target = match self.plan(url, &key, target, forge, auth).await {
                        Ok(plan) => self
                            .fetch_plan(url, &plan, auth, progress)
                            .await
                            .map(|_| ()),
                        Err(e) => Err(e),
                    };
                    if !self.has_commit(&pin).await {
                        let what = format!("commit {pin}");
                        self.fetch(url, &[format!("+{pin}:{local}")], &what, auth, progress)
                            .await
                            .map_err(|e| {
                                let target_err = via_target
                                    .err()
                                    .map(|t| format!(" ({t})"))
                                    .unwrap_or_default();
                                format!(
                                    "the pinned commit {pin} could not be fetched: {e}{target_err} \
                                     — unpin it, or pin a commit that is still on {url}"
                                )
                            })?;
                    }
                }
                self.update_ref(&local, &pin).await?;
                Ok(Fetched {
                    sha: pin,
                    local_ref: local,
                    kind: RefKind::Pinned,
                })
            }
        }
    }

    /// Fetch `url`'s default branch (what its `HEAD` points at): the upstream
    /// tip a plain-ref extra's fork point is computed against (§14.1).
    pub async fn fetch_default_branch(
        &self,
        url: &str,
        auth: Option<&GitAuth>,
        progress: Option<Progress<'_>>,
    ) -> Result<Fetched, String> {
        let refs = self.git.ls_remote(url, auth).await?;
        let name = refs
            .default_branch
            .clone()
            .ok_or_else(|| format!("{url} does not say which branch is its default (no HEAD)"))?;
        let target = FetchTarget::Ref(name);
        self.resolve_and_fetch(url, &target, None, Forge::Plain, auth, progress)
            .await
    }

    async fn plan(
        &self,
        url: &str,
        key: &str,
        target: &FetchTarget,
        forge: Forge,
        auth: Option<&GitAuth>,
    ) -> Result<TargetPlan, String> {
        match target {
            FetchTarget::Pr(n) => {
                let source = match forge {
                    Forge::Github => format!("refs/pull/{n}/head"),
                    Forge::Gitlab => format!("refs/merge-requests/{n}/head"),
                    Forge::Plain => {
                        return Err(format!(
                            "PR #{n} needs a forge (github or gitlab) to be found on {url}; add \
                             it as a ref extra instead"
                        ))
                    }
                };
                Ok(TargetPlan {
                    kind: RefKind::Pr { number: *n },
                    what: format!("PR #{n} ({source})"),
                    local: format!("refs/lmgw/{key}/pr/{n}"),
                    source,
                    known: None,
                })
            }
            FetchTarget::Ref(r) => {
                validate::validate_ref("ref", r)?;
                if is_full_sha(r) {
                    let sha = r.to_ascii_lowercase();
                    return Ok(TargetPlan {
                        kind: RefKind::Commit,
                        what: format!("commit {sha}"),
                        local: commit_ref(key, &sha),
                        source: sha.clone(),
                        known: Some(sha),
                    });
                }
                if let Some(rest) = r.strip_prefix("refs/") {
                    let mapped = match rest.split('/').collect::<Vec<_>>().as_slice() {
                        ["pull" | "merge-requests", n, "head"] => format!("pr/{n}"),
                        _ => rest.to_string(),
                    };
                    return Ok(TargetPlan {
                        kind: match rest.strip_prefix("heads/") {
                            Some(b) => RefKind::Branch { name: b.into() },
                            None => RefKind::Ref { name: r.clone() },
                        },
                        what: r.clone(),
                        local: format!("refs/lmgw/{key}/{mapped}"),
                        source: r.clone(),
                        known: None,
                    });
                }
                let refs = self.git.ls_remote(url, auth).await?;
                if let Some(sha) = refs.branch(r) {
                    return Ok(TargetPlan {
                        kind: RefKind::Branch { name: r.clone() },
                        what: format!("branch {r}"),
                        local: format!("refs/lmgw/{key}/heads/{r}"),
                        source: format!("refs/heads/{r}"),
                        known: Some(sha.to_string()),
                    });
                }
                if let Some(t) = refs.tag(r) {
                    return Ok(TargetPlan {
                        kind: RefKind::Tag {
                            name: r.clone(),
                            annotated: t.annotated(),
                        },
                        what: format!("tag {r}"),
                        local: format!("refs/lmgw/{key}/tags/{r}"),
                        source: format!("refs/tags/{r}"),
                        known: Some(t.commit.clone()),
                    });
                }
                if r.len() >= 4 && is_hex(r) {
                    let tips = refs.tips_starting_with(r);
                    return match tips.as_slice() {
                        [sha] => Ok(TargetPlan {
                            kind: RefKind::Commit,
                            what: format!("commit {sha}"),
                            local: commit_ref(key, sha),
                            source: sha.clone(),
                            known: Some(sha.clone()),
                        }),
                        [] => Err(format!(
                            "'{r}' is not a branch or tag of {url}, and no branch or tag tip \
                             starts with it — a commit that is not a tip must be given as its \
                             full 40-character SHA (a short SHA cannot be fetched)"
                        )),
                        many => Err(format!(
                            "'{r}' is ambiguous on {url}: {} branch or tag tips start with it — \
                             give the full 40-character SHA",
                            many.len()
                        )),
                    };
                }
                Err(format!(
                    "'{r}' is not a branch or tag of {url} (a commit must be given as its full \
                     40-character SHA)"
                ))
            }
        }
    }

    async fn fetch_plan(
        &self,
        url: &str,
        plan: &TargetPlan,
        auth: Option<&GitAuth>,
        progress: Option<Progress<'_>>,
    ) -> Result<String, String> {
        if let Some(sha) = &plan.known {
            if self.has_commit(sha).await {
                if let Some(p) = progress {
                    p(&format!(
                        "{} is at {}, already in the pool — nothing to fetch",
                        plan.what,
                        short_sha(sha)
                    ));
                }
                self.update_ref(&plan.local, sha).await?;
                return Ok(sha.clone());
            }
        }
        let refspec = format!("+{}:{}", plan.source, plan.local);
        self.fetch(url, &[refspec], &plan.what, auth, progress)
            .await?;
        self.rev_parse(None, &format!("{}^{{commit}}", plan.local))
            .await
    }

    /// Whether a pool exists under `builds_dir` — without creating one, which
    /// [`Self::open`] would.
    pub fn exists(builds_dir: &Path) -> bool {
        builds_dir
            .join("git")
            .join("pool.git")
            .join("HEAD")
            .is_file()
    }

    /// How many commits `new` is ahead of `old` (`rev-list --count
    /// old..new`), when the pool already holds both and `old` is an ancestor
    /// of `new` — `None` otherwise: a commit the pool lacks, or a history
    /// that was rewritten. Never fetches (§8: the count is only cheap when
    /// both commits are already here).
    pub async fn ahead(&self, old: &str, new: &str) -> Result<Option<u64>, String> {
        if !(self.has_commit(old).await && self.has_commit(new).await) {
            return Ok(None);
        }
        let ancestor = self
            .in_pool(&["merge-base", "--is-ancestor", "--end-of-options", old, new])
            .await?;
        if !ancestor.ok() {
            return Ok(None);
        }
        let range = format!("{old}..{new}");
        let out = self
            .in_pool_ok(&["rev-list", "--count", "--end-of-options", &range])
            .await?;
        out.stdout
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| format!("rev-list --count printed {:?}", out.stdout.trim()))
    }

    /// `rev-list --count <sha>`: the build number of the llama engines
    /// (§14.1). For official llama.cpp it equals the upstream `bNNNN` tag.
    pub async fn build_number(&self, sha: &str) -> Result<u64, String> {
        let out = self
            .in_pool_ok(&["rev-list", "--count", "--end-of-options", sha])
            .await?;
        out.stdout
            .trim()
            .parse()
            .map_err(|_| format!("rev-list --count printed {:?}", out.stdout.trim()))
    }

    /// The committer date of `sha` — what `BUILD_DATE` is (§2.1).
    pub async fn commit_date(
        &self,
        sha: &str,
    ) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
        let out = self
            .in_pool_ok(&["show", "-s", "--format=%cI", "--end-of-options", sha])
            .await?;
        chrono::DateTime::parse_from_rfc3339(out.stdout.trim())
            .map_err(|e| format!("commit date {:?} of {sha}: {e}", out.stdout.trim()))
    }

    /// The text of `path` at `commit` (the base, or an assembled HEAD — every
    /// worktree writes into the pool's object store), `None` when the path
    /// does not exist there. How a run picks its Dockerfile before (and
    /// without) a worktree.
    pub async fn read_file(&self, commit: &str, path: &str) -> Result<Option<String>, String> {
        let c = format!("{commit}^{{commit}}");
        self.rev_parse(None, &c)
            .await
            .map_err(|_| format!("commit {commit} is not in the pool"))?;
        let spec = format!("{commit}:{path}");
        if !self.in_pool(&["cat-file", "-e", &spec]).await?.ok() {
            return Ok(None);
        }
        Ok(Some(
            self.in_pool_ok(&["cat-file", "blob", &spec]).await?.stdout,
        ))
    }

    /// The merge base of `a` and `b` in `dir` (the pool when `None`), `None`
    /// when they share no history.
    pub async fn merge_base(
        &self,
        dir: Option<&Path>,
        a: &str,
        b: &str,
    ) -> Result<Option<String>, String> {
        let dir = dir.unwrap_or(&self.dir);
        let out = self
            .git
            .run(
                Some(dir),
                &["merge-base", "--end-of-options", a, b],
                None,
                None,
            )
            .await?;
        match out.status {
            0 => Ok(Some(out.stdout.trim().to_string())),
            1 => Ok(None),
            s => Err(format!(
                "git merge-base {a} {b} failed (exit {s}): {}",
                out.said()
            )),
        }
    }

    /// Whether `base`'s history contains `commit` — `false` for a commit the
    /// pool does not hold (the base's history is in the pool, so a commit
    /// that is not cannot be in it).
    pub async fn contains(&self, base: &str, commit: &str) -> Result<bool, String> {
        if !is_hex(commit) || !self.has_commit(commit).await {
            return Ok(false);
        }
        self.is_ancestor(&self.dir, commit, base).await
    }

    async fn is_ancestor(&self, dir: &Path, a: &str, b: &str) -> Result<bool, String> {
        let out = self
            .git
            .run(
                Some(dir),
                &["merge-base", "--is-ancestor", "--end-of-options", a, b],
                None,
                None,
            )
            .await?;
        match out.status {
            0 => Ok(true),
            1 => Ok(false),
            s => Err(format!(
                "git merge-base --is-ancestor failed (exit {s}): {}",
                out.said()
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Worktrees
    // -----------------------------------------------------------------------

    /// A detached worktree of `sha` at `dir` (§5 "Workspace"), which must not
    /// exist or be empty. Stale registrations (a crashed run's) are pruned
    /// first.
    pub async fn worktree_add(
        &self,
        sha: &str,
        dir: &Path,
        progress: Option<Progress<'_>>,
    ) -> Result<(), String> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let path = path_str(dir)?;
        let _guard = self.lock_pool().await?;
        self.in_pool_ok(&["worktree", "prune"]).await?;
        self.git
            .run_ok(
                Some(&self.dir),
                &["worktree", "add", "--detach", "--", path, sha],
                None,
                progress,
            )
            .await
            .map(|_| ())
    }

    /// Remove the worktree at `dir`, submodules and all (`--force`, which git
    /// needs for a tree with submodules, §14.1). Falls back to deleting the
    /// directory and pruning when git will not; `Ok` once both the directory
    /// and its registration are gone.
    pub async fn worktree_remove(
        &self,
        dir: &Path,
        progress: Option<Progress<'_>>,
    ) -> Result<(), String> {
        let path = path_str(dir)?;
        let _guard = self.lock_pool().await?;
        if dir.exists() {
            let out = self
                .git
                .run(
                    Some(&self.dir),
                    // Twice: a registration a killed `worktree add` left
                    // locked ("initializing") is removed too — it is ours.
                    &["worktree", "remove", "--force", "--force", "--", path],
                    None,
                    progress,
                )
                .await?;
            if !out.ok() && dir.exists() {
                if let Some(p) = progress {
                    p(&format!(
                        "git worktree remove failed ({}); deleting {path} directly",
                        out.said()
                    ));
                }
                std::fs::remove_dir_all(dir)
                    .map_err(|e| format!("could not delete the worktree {path}: {e}"))?;
            }
        }
        self.in_pool_ok(&["worktree", "prune"]).await.map(|_| ())
    }

    /// Drop the registrations of worktrees whose directory is gone.
    pub async fn prune(&self) -> Result<(), String> {
        let _guard = self.lock_pool().await?;
        self.in_pool_ok(&["worktree", "prune"]).await.map(|_| ())
    }

    /// [`Self::prune`], first unlocking the registrations `own` claims whose
    /// worktree is gone: a `worktree add` killed half-way (a canceled run)
    /// leaves its registration locked ("initializing"), and `prune` never
    /// drops a locked one. `own` is asked with the worktree path the
    /// registration points at; one that is not ours keeps its lock.
    pub async fn prune_stale(&self, own: impl Fn(&Path) -> bool) -> Result<(), String> {
        let _guard = self.lock_pool().await?;
        if let Ok(entries) = std::fs::read_dir(self.dir.join("worktrees")) {
            for entry in entries.filter_map(Result::ok) {
                let admin = entry.path();
                let locked = admin.join("locked");
                if !locked.exists() {
                    continue;
                }
                // `gitdir` holds `<worktree>/.git`.
                let Ok(gitdir) = std::fs::read_to_string(admin.join("gitdir")) else {
                    continue;
                };
                let dot_git = PathBuf::from(gitdir.trim());
                let Some(worktree) = dot_git.parent() else {
                    continue;
                };
                if !worktree.exists() && own(worktree) {
                    if let Err(e) = std::fs::remove_file(&locked) {
                        tracing::warn!(
                            "could not unlock the stale worktree registration {}: {e}",
                            admin.display()
                        );
                    }
                }
            }
        }
        self.in_pool_ok(&["worktree", "prune"]).await.map(|_| ())
    }

    // -----------------------------------------------------------------------
    // Assemble
    // -----------------------------------------------------------------------

    /// Assemble the build's tree in `worktree`, which is checked out at
    /// `base` (§5 phase 3 as corrected by §14.1):
    ///
    /// 1. submodules initialized **at base** — merge-ort fast-forwards a
    ///    bumped gitlink (sd.cpp's `ggml`) only while the submodule is
    ///    checked out;
    /// 2. each extra in order: skipped when the forge says it was merged
    ///    upstream; "already in base" when its head is an ancestor of HEAD;
    ///    else merged (`merge --no-ff`) when it shares history with HEAD —
    ///    "already in base" again when that changed nothing (a squash-merged
    ///    PR) — or **squash-applied** when it does not (an upstream llama.cpp
    ///    PR on ik): `merge-tree --merge-base=<fork point>` + `commit-tree`;
    /// 3. on a conflict: the merge is aborted and the report names the
    ///    extra, the files and the extras merged before it — and stops;
    /// 4. after all of them: `submodule sync`, `update --init --recursive
    ///    --force`, `foreach git clean -ffdx`, and a top-level `clean -ffdx`
    ///    (a submodule an extra removed would otherwise stay in the context).
    ///
    /// `Err` is for git failing; a conflict is an `Ok` report.
    pub async fn assemble(
        &self,
        worktree: &Path,
        base: &str,
        extras: &[ResolvedExtraSpec],
        opts: AssembleOpts,
        progress: Option<Progress<'_>>,
    ) -> Result<AssembleReport, String> {
        let wt = Some(worktree);
        let head0 = self.rev_parse(wt, "HEAD").await?;
        if !head0.eq_ignore_ascii_case(base) {
            return Err(format!(
                "the worktree {} is at {}, not at the base {}",
                worktree.display(),
                short_sha(&head0),
                short_sha(base)
            ));
        }
        let say = |line: &str| {
            if let Some(p) = progress {
                p(line);
            }
        };
        let reference = format!("--reference={}", path_str(&self.dir)?);
        let mut update_at_base = vec!["submodule", "update", "--init", "--recursive"];
        if opts.share_submodule_objects {
            update_at_base.push(&reference);
        }
        self.git.run_ok(wt, &update_at_base, None, progress).await?;

        let mut report = AssembleReport {
            base: base.to_string(),
            head: head0,
            extras: Vec::new(),
            conflict: None,
        };
        for x in extras {
            let head = report.head.clone();
            let done = |result: ExtraResult| ExtraOutcome {
                label: x.label.clone(),
                sha: x.sha.clone(),
                result,
            };
            if x.merged_upstream {
                say(&format!("{}: merged upstream — skipped", x.label));
                report.extras.push(done(ExtraResult::MergedUpstream));
                continue;
            }
            if self.is_ancestor(worktree, &x.sha, &head).await? {
                say(&format!(
                    "{} ({}): already in base — its head is an ancestor",
                    x.label,
                    short_sha(&x.sha)
                ));
                report.extras.push(done(ExtraResult::AlreadyInBase {
                    why: "its head is already an ancestor of the tree".into(),
                }));
                continue;
            }
            let step = match self.merge_base(wt, &head, &x.sha).await? {
                Some(_) => self.merge_one(worktree, x, &head, progress).await?,
                None => self.squash_one(worktree, x, &head, progress).await?,
            };
            match step {
                Step::Done(result, new_head) => {
                    if let ExtraResult::AlreadyInBase { why } = &result {
                        say(&format!(
                            "{} ({}): already in base — {why}",
                            x.label,
                            short_sha(&x.sha)
                        ));
                    }
                    report.head = new_head;
                    report.extras.push(done(result));
                }
                Step::Conflict {
                    mode,
                    files,
                    reason,
                } => {
                    let conflict = MergeConflict {
                        label: x.label.clone(),
                        sha: x.sha.clone(),
                        mode,
                        files,
                        reason,
                        merged_before: report.merged_labels(),
                    };
                    say(&conflict.message());
                    report.conflict = Some(conflict);
                    return Ok(report);
                }
            }
        }

        let mut update_final = vec!["submodule", "update", "--init", "--recursive", "--force"];
        if opts.share_submodule_objects {
            update_final.push(&reference);
        }
        for args in [
            &["submodule", "sync", "--recursive"][..],
            update_final.as_slice(),
            &[
                "submodule",
                "foreach",
                "--recursive",
                "git",
                "clean",
                "-ffdx",
            ][..],
            &["clean", "-ffdx", "--quiet"][..],
        ] {
            self.git.run_ok(wt, args, None, progress).await?;
        }
        if opts.share_submodule_objects {
            self.harvest_submodules(worktree, progress).await;
        }
        say(&format!(
            "assembled {} on {}",
            short_sha(&report.head),
            short_sha(base)
        ));
        Ok(report)
    }

    async fn merge_one(
        &self,
        worktree: &Path,
        x: &ResolvedExtraSpec,
        head: &str,
        progress: Option<Progress<'_>>,
    ) -> Result<Step, String> {
        let wt = Some(worktree);
        let msg = format!("lmgw: merge {} ({})", x.label, short_sha(&x.sha));
        let out = self
            .git
            .run(
                wt,
                &["merge", "--no-ff", "--no-edit", "-m", &msg, "--", &x.sha],
                None,
                progress,
            )
            .await?;
        if !out.ok() {
            let files = self.unmerged_files(worktree).await?;
            if files.is_empty() {
                return Err(format!(
                    "git merge of {} failed (exit {}): {}",
                    x.label,
                    out.status,
                    out.said()
                ));
            }
            self.git
                .run_ok(wt, &["merge", "--abort"], None, progress)
                .await?;
            return Ok(Step::Conflict {
                mode: MergeMode::Merge,
                files,
                reason: None,
            });
        }
        let new_head = self.rev_parse(wt, "HEAD").await?;
        let tree = self.rev_parse(wt, "HEAD^{tree}").await?;
        let before = self.rev_parse(wt, &format!("{head}^{{tree}}")).await?;
        let result = if tree == before {
            ExtraResult::AlreadyInBase {
                why: "merging it changed nothing (squash-merged upstream?)".into(),
            }
        } else {
            ExtraResult::Merged {
                commit: new_head.clone(),
            }
        };
        Ok(Step::Done(result, new_head))
    }

    async fn squash_one(
        &self,
        worktree: &Path,
        x: &ResolvedExtraSpec,
        head: &str,
        progress: Option<Progress<'_>>,
    ) -> Result<Step, String> {
        let wt = Some(worktree);
        let say = |line: &str| {
            if let Some(p) = progress {
                p(line);
            }
        };
        let fork = match (&x.fork_point, &x.upstream_tip) {
            (Some(f), _) => Some(f.clone()),
            (None, Some(tip)) => self.merge_base(wt, &x.sha, tip).await?,
            (None, None) => None,
        };
        let Some(fork) = fork else {
            return Ok(Step::Conflict {
                mode: MergeMode::SquashApply,
                files: Vec::new(),
                reason: Some(
                    "it shares no history with the tree, and no fork point is known to take its \
                     changes from (its remote's default branch shares none with it either)"
                        .into(),
                ),
            });
        };
        if fork.eq_ignore_ascii_case(&x.sha) {
            // Its head is its own fork point: it is already part of the
            // branch its changes would be measured against, so there are no
            // changes of its own to take — which says nothing about whether
            // the tree has them (it shares no history with it).
            return Ok(Step::Conflict {
                mode: MergeMode::SquashApply,
                files: Vec::new(),
                reason: Some(
                    "the extra is already contained in its own remote's default branch (its \
                     head is its fork point), so no squash base can be computed — merge that \
                     branch as an extra instead, or pick a commit that is not on it"
                        .into(),
                ),
            });
        }
        say(&format!(
            "{} ({}) shares no history with the tree — squash-applying its changes since {}",
            x.label,
            short_sha(&x.sha),
            short_sha(&fork)
        ));
        let merge_base = format!("--merge-base={fork}");
        let out = self
            .git
            .run(
                wt,
                &[
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    "--no-messages",
                    &merge_base,
                    "--end-of-options",
                    head,
                    &x.sha,
                ],
                None,
                None,
            )
            .await?;
        let mut lines = out.stdout.lines();
        let tree = lines.next().unwrap_or("").trim().to_string();
        match out.status {
            0 if is_hex(&tree) => {}
            1 => {
                let mut files: Vec<String> = Vec::new();
                for f in lines.map(str::trim).filter(|l| !l.is_empty()) {
                    if !files.iter().any(|g| g == f) {
                        files.push(f.to_string());
                    }
                }
                return Ok(Step::Conflict {
                    mode: MergeMode::SquashApply,
                    files,
                    reason: None,
                });
            }
            s => {
                return Err(format!(
                    "git merge-tree for {} failed (exit {s}): {}",
                    x.label,
                    out.said()
                ))
            }
        }
        let msg = format!(
            "lmgw: squash-apply {} ({}), changes since {}",
            x.label,
            short_sha(&x.sha),
            short_sha(&fork)
        );
        let commit = self
            .git
            .run_ok(
                wt,
                &["commit-tree", "-p", head, "-m", &msg, &tree],
                None,
                None,
            )
            .await?
            .stdout
            .trim()
            .to_string();
        self.git
            .run_ok(
                wt,
                &["checkout", "--detach", "--quiet", &commit],
                None,
                progress,
            )
            .await?;
        let before = self.rev_parse(wt, &format!("{head}^{{tree}}")).await?;
        let result = if tree == before {
            ExtraResult::AlreadyInBase {
                why: "applying it changed nothing".into(),
            }
        } else {
            ExtraResult::SquashApplied {
                commit: commit.clone(),
                fork_point: fork,
            }
        };
        Ok(Step::Done(result, commit))
    }

    async fn unmerged_files(&self, worktree: &Path) -> Result<Vec<String>, String> {
        let out = self
            .git
            .run_ok(
                Some(worktree),
                &["diff", "--name-only", "--diff-filter=U"],
                None,
                None,
            )
            .await?;
        let mut files: Vec<String> = Vec::new();
        for f in out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !files.iter().any(|g| g == f) {
                files.push(f.to_string());
            }
        }
        Ok(files)
    }

    /// Keep this run's submodule commits in the pool, so the next run's
    /// `submodule update --reference` finds their objects locally instead of
    /// re-cloning (§14.1: sd.cpp's four submodules are about 50 MB and 7 s a
    /// run). Best effort: a failure costs only the next run's download.
    async fn harvest_submodules(&self, worktree: &Path, progress: Option<Progress<'_>>) {
        let note = |line: String| {
            if let Some(p) = progress {
                p(&line);
            }
        };
        let status = match self
            .git
            .run_ok(
                Some(worktree),
                &["submodule", "status", "--recursive"],
                None,
                None,
            )
            .await
        {
            Ok(o) => o.stdout,
            Err(e) => return note(format!("note: submodule objects not kept for reuse: {e}")),
        };
        for line in status.lines() {
            let Some((sha, path)) = parse_submodule_status(line) else {
                continue;
            };
            let sub = worktree.join(path);
            let url = match self
                .git
                .run_ok(
                    Some(&sub),
                    &["config", "--get", "remote.origin.url"],
                    None,
                    None,
                )
                .await
            {
                Ok(o) => o.stdout.trim().to_string(),
                Err(_) => continue,
            };
            let local = commit_ref(&remote_key(&url), sha);
            if self.ref_exists(&local).await {
                continue;
            }
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

    /// [`Self::assemble`] in a throwaway worktree under
    /// [`Self::work_dir`], removed afterwards (§7 "Check merge", superseded
    /// by §14.1: the same code as a run, never a bare `merge-tree`, because
    /// only a checked-out submodule lets a bumped gitlink fast-forward).
    pub async fn check_merge(
        &self,
        base: &str,
        extras: &[ResolvedExtraSpec],
        opts: AssembleOpts,
        progress: Option<Progress<'_>>,
    ) -> Result<AssembleReport, String> {
        let prefix = if self.instance.is_empty() {
            "check".to_string()
        } else {
            format!("check-{}", self.instance)
        };
        let dir = self.work.join(unique_name(&prefix));
        self.worktree_add(base, &dir, progress).await?;
        let assembled = self.assemble(&dir, base, extras, opts, progress).await;
        let removed = self.worktree_remove(&dir, progress).await;
        let report = assembled?;
        removed?;
        Ok(report)
    }
}

/// ` <sha> <path> (<describe>)` (also `+`/`U`-prefixed) → `(sha, path)`;
/// `None` for an uninitialized (`-`) submodule.
fn parse_submodule_status(line: &str) -> Option<(&str, &str)> {
    let flag = line.chars().next()?;
    if flag == '-' {
        return None;
    }
    let rest = &line[flag.len_utf8()..];
    let (sha, path) = rest.split_once(' ')?;
    if !is_hex(sha) {
        return None;
    }
    let path = match path.rsplit_once(" (") {
        Some((p, _)) if path.ends_with(')') => p,
        _ => path,
    };
    Some((sha, path))
}

enum Step {
    Done(ExtraResult, String),
    Conflict {
        mode: MergeMode,
        files: Vec<String>,
        reason: Option<String>,
    },
}

/// One extra as assemble takes it: resolved, with what the caller knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedExtraSpec {
    /// How the extra is named in the log and the report (`PR #1234`,
    /// `https://github.com/fork/llama.cpp feature-x` — `BuildExtra::label`).
    pub label: String,
    /// The commit to merge (fetched into the pool).
    pub sha: String,
    /// The forge reports the PR merged (`merged_at` set, §14.1) **and** the
    /// base contains it (its merge commit or its head): skip it.
    pub merged_upstream: bool,
    /// Where the extra's own changes start, for a squash-apply: the forge
    /// PR's `base.sha` merge base. When `None`, the merge base of the extra
    /// and `upstream_tip` is used.
    pub fork_point: Option<String>,
    /// The tip of the extra's remote's default branch (fetched — see
    /// [`Pool::fetch_default_branch`]).
    pub upstream_tip: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembleOpts {
    /// Clone submodules `--reference` the pool, and keep their commits in it
    /// afterwards, so the next run fetches only what changed.
    pub share_submodule_objects: bool,
}

impl Default for AssembleOpts {
    fn default() -> Self {
        Self {
            share_submodule_objects: true,
        }
    }
}

/// How an extra was taken in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    Merge,
    SquashApply,
}

/// What happened to one extra. The tags are the `outcome` values of the
/// `build_check_merge` contract (§15), which adds `conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExtraResult {
    Merged {
        commit: String,
    },
    SquashApplied {
        commit: String,
        fork_point: String,
    },
    /// Nothing to add: `why` says how that was established.
    AlreadyInBase {
        why: String,
    },
    /// Skipped on the forge's word.
    MergedUpstream,
}

impl ExtraResult {
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Merged { .. } => "merged",
            Self::SquashApplied { .. } => "squash_applied",
            Self::AlreadyInBase { .. } => "already_in_base",
            Self::MergedUpstream => "merged_upstream",
        }
    }

    fn note(&self) -> String {
        match self {
            Self::Merged { commit } => format!("merge commit {}", short_sha(commit)),
            Self::SquashApplied { commit, fork_point } => format!(
                "no shared history — its changes since {} applied as {}",
                short_sha(fork_point),
                short_sha(commit)
            ),
            Self::AlreadyInBase { why } => why.clone(),
            Self::MergedUpstream => "the forge reports it merged upstream — skipped".into(),
        }
    }
}

/// One extra's line in a report, flat: the `steps` rows of the
/// `build_check_merge` contract (§15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembleStep {
    pub label: String,
    /// `merged` | `already_in_base` | `merged_upstream` | `squash_applied` |
    /// `conflict`.
    pub outcome: String,
    pub sha: String,
    /// Conflicted paths (`conflict` only).
    pub files: Vec<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraOutcome {
    pub label: String,
    pub sha: String,
    #[serde(flatten)]
    pub result: ExtraResult,
}

/// The extra assemble stopped at (§5 phase 3 "On a conflict").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflict {
    pub label: String,
    pub sha: String,
    pub mode: MergeMode,
    /// Conflicted paths; empty when `reason` says why there was nothing to
    /// merge with.
    pub files: Vec<String>,
    pub reason: Option<String>,
    /// The extras merged or squash-applied before it, in order.
    pub merged_before: Vec<String>,
}

impl MergeConflict {
    /// The sentence a failed run or a Check merge shows.
    pub fn message(&self) -> String {
        let verb = match self.mode {
            MergeMode::Merge => "merge",
            MergeMode::SquashApply => "squash-apply",
        };
        let what = match (&self.reason, self.files.is_empty()) {
            (Some(r), _) => r.clone(),
            (None, false) => format!("conflicts in {}", self.files.join(", ")),
            (None, true) => "conflicts".into(),
        };
        let before = if self.merged_before.is_empty() {
            " onto the base alone".to_string()
        } else {
            format!(" after {}", self.merged_before.join(", "))
        };
        format!(
            "{} ({}) does not {verb} cleanly{before}: {what}",
            self.label,
            short_sha(&self.sha)
        )
    }
}

/// What assemble did (§5 phase 3, §7 "Check merge").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembleReport {
    pub base: String,
    /// The assembled commit — or, after a conflict, the last one assembled
    /// before it.
    pub head: String,
    /// Every extra handled, in order, up to (not including) a conflict.
    pub extras: Vec<ExtraOutcome>,
    pub conflict: Option<MergeConflict>,
}

impl AssembleReport {
    pub fn is_clean(&self) -> bool {
        self.conflict.is_none()
    }

    /// Every extra handled, then the conflict if there was one, as flat rows.
    pub fn steps(&self) -> Vec<AssembleStep> {
        let mut steps: Vec<AssembleStep> = self
            .extras
            .iter()
            .map(|e| AssembleStep {
                label: e.label.clone(),
                outcome: e.result.outcome().into(),
                sha: e.sha.clone(),
                files: Vec::new(),
                note: e.result.note(),
            })
            .collect();
        if let Some(c) = &self.conflict {
            steps.push(AssembleStep {
                label: c.label.clone(),
                outcome: "conflict".into(),
                sha: c.sha.clone(),
                files: c.files.clone(),
                note: c.message(),
            });
        }
        steps
    }

    fn merged_labels(&self) -> Vec<String> {
        self.extras
            .iter()
            .filter(|e| {
                matches!(
                    e.result,
                    ExtraResult::Merged { .. } | ExtraResult::SquashApplied { .. }
                )
            })
            .map(|e| e.label.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_remote_output_parses_into_branches_peeled_tags_and_head() {
        let heads_tags = "\
1111111111111111111111111111111111111111\trefs/heads/master
2222222222222222222222222222222222222222\trefs/heads/feature/x
3333333333333333333333333333333333333333\trefs/tags/b6000
4444444444444444444444444444444444444444\trefs/tags/v1
1111111111111111111111111111111111111111\trefs/tags/v1^{}
";
        let symref =
            "ref: refs/heads/master\tHEAD\n1111111111111111111111111111111111111111\tHEAD\n";
        let r = RemoteRefs::parse(heads_tags, symref);
        assert_eq!(r.default_branch.as_deref(), Some("master"));
        assert_eq!(r.head.as_deref(), Some(&*"1".repeat(40)));
        assert_eq!(r.branch("feature/x"), Some(&*"2".repeat(40)));
        assert!(!r.tag("b6000").unwrap().annotated());
        let v1 = r.tag("v1").unwrap();
        assert!(v1.annotated());
        assert_eq!(v1.commit, "1".repeat(40));
        assert_eq!(r.tips_starting_with("1111"), vec!["1".repeat(40)]);
        assert_eq!(
            r.tips_starting_with("4444"),
            Vec::<String>::new(),
            "tag objects are not tips"
        );
    }

    #[test]
    fn a_remote_key_ignores_spelling_but_not_the_repository() {
        let k = remote_key("https://github.com/ggml-org/llama.cpp");
        assert_eq!(k.len(), 16);
        assert_eq!(k, remote_key("https://GitHub.com/ggml-org/llama.cpp.git/"));
        assert_eq!(k, remote_key("git@github.com:ggml-org/llama.cpp.git"));
        assert_ne!(k, remote_key("https://github.com/ikawrakow/ik_llama.cpp"));
    }

    #[test]
    fn submodule_status_lines_parse() {
        let sha = "a".repeat(40);
        assert_eq!(
            parse_submodule_status(&format!(" {sha} ggml (heads/master)")),
            Some((sha.as_str(), "ggml"))
        );
        assert_eq!(
            parse_submodule_status(&format!("+{sha} thirdparty/lib webp")),
            Some((sha.as_str(), "thirdparty/lib webp"))
        );
        assert_eq!(parse_submodule_status(&format!("-{sha} ggml")), None);
    }

    #[test]
    fn versions_parse_and_old_ones_are_named() {
        assert_eq!(parse_version("git version 2.55.0\n"), Some((2, 55, 0)));
        assert_eq!(
            parse_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
        assert_eq!(parse_version("git version 2.40"), Some((2, 40, 0)));
        assert_eq!(parse_version("nope"), None);
    }

    #[test]
    fn auth_is_scoped_to_the_host_and_never_printed() {
        let a = GitAuth::new("https://user@GitHub.com/o/r", "ghp_secret", Forge::Github).unwrap();
        assert_eq!(a.url_prefix(), "https://github.com/");
        assert!(!format!("{a:?}").contains("ghp_secret"));
        let expected =
            base64::engine::general_purpose::STANDARD.encode("x-access-token:ghp_secret");
        assert_eq!(
            a.header.expose_secret(),
            format!("Authorization: Basic {expected}")
        );
        assert_eq!(
            GitAuth::new("git@github.com:o/r.git", "t", Forge::Github)
                .unwrap()
                .url_prefix(),
            "https://github.com/"
        );
        let gl = GitAuth::new("https://git.example.com:8443/p/x", "glpat", Forge::Gitlab).unwrap();
        assert_eq!(gl.url_prefix(), "https://git.example.com:8443/");
        assert!(GitAuth::new("git@git.example.com:p/x", "t", Forge::Gitlab).is_none());
        assert!(GitAuth::new("file:///srv/x", "t", Forge::Plain).is_none());
    }

    #[test]
    fn a_ref_conflicts_only_across_a_slash_in_its_own_namespace() {
        let a = "refs/lmgw/0123456789abcdef/heads/a";
        let ab = "refs/lmgw/0123456789abcdef/heads/a/b";
        assert!(ref_conflict(a, ab));
        assert!(ref_conflict(ab, a));
        assert!(
            !ref_conflict(a, a),
            "the same ref is an update, not a conflict"
        );
        assert!(!ref_conflict("refs/lmgw/0123456789abcdef/heads/ab", ab));
        assert!(!ref_conflict("refs/lmgw/0123456789abcdef/heads/a-b", ab));
        assert_eq!(remote_namespace(ab), Some("refs/lmgw/0123456789abcdef/"));
        assert_eq!(remote_namespace("refs/heads/a"), None);
    }

    #[test]
    fn remote_errors_never_repeat_credentials() {
        let out = GitOutput {
            status: 128,
            stdout: String::new(),
            stderr: "fatal: unable to access 'https://oauth2:glpat-secret@git.local/o/r/': \
                     Could not resolve host"
                .into(),
        };
        let e = remote_error(
            "https://oauth2:glpat-secret@git.local/o/r",
            "git fetch",
            &out,
        );
        assert!(!e.contains("glpat-secret"), "{e}");
        assert!(e.contains("https://***@git.local/o/r"), "{e}");
    }

    #[test]
    fn a_conflict_reads_as_a_sentence() {
        let c = MergeConflict {
            label: "PR #7".into(),
            sha: "abcdef0123".into(),
            mode: MergeMode::Merge,
            files: vec!["a.c".into(), "b.h".into()],
            reason: None,
            merged_before: vec!["PR #3".into()],
        };
        assert_eq!(
            c.message(),
            "PR #7 (abcdef0) does not merge cleanly after PR #3: conflicts in a.c, b.h"
        );
    }
}
