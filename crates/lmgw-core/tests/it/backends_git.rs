//! The container-builds git layer against real git (container-builds design
//! §5, §7, §10, §14.1): local bare "remotes" in temp dirs, the pool, run
//! worktrees and assemble. Real git is both fast here (milliseconds per call)
//! and the only faithful fake of merge-ort's submodule handling.
//!
//! Fixture repositories are made with an isolated git (`GIT_CONFIG_GLOBAL`
//! pointed at /dev/null, fixed identity and dates); the code under test runs
//! as it does in production — the owner's global config and all — except for
//! `allow_file_protocol`, which the local submodule fixtures need.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lmgw_core::backends::git::{
    git_available, remote_key, AssembleOpts, AssembleReport, ExtraResult, FetchTarget, Fetched,
    Git, GitAuth, MergeMode, Pool, RefKind, ResolvedExtraSpec,
};
use lmgw_core::backends::Forge;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture_git(dir: &Path, args: &[&str], date: &str) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "init.defaultBranch=master",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "advice.detachedHead=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "fixture git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A fixture repository with its own clock, so no two commits share a date
/// (identical content, parents and dates would be the same commit).
struct Repo {
    dir: PathBuf,
    clock: Cell<u64>,
}

impl Repo {
    fn init(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).unwrap();
        let r = Self {
            dir,
            clock: Cell::new(1_790_000_000),
        };
        r.git(&["init", "--quiet"]);
        r
    }

    fn bare(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).unwrap();
        let r = Self {
            dir,
            clock: Cell::new(1_790_000_000),
        };
        r.git(&["init", "--quiet", "--bare"]);
        r
    }

    fn date(&self) -> String {
        let t = self.clock.get() + 60;
        self.clock.set(t);
        format!("@{t} +0000")
    }

    fn git(&self, args: &[&str]) -> String {
        fixture_git(&self.dir, args, &self.date())
    }

    fn url(&self) -> String {
        format!("file://{}", self.dir.display())
    }

    fn write(&self, path: &str, content: &str) {
        let p = self.dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn commit_at(&self, msg: &str, date: &str) -> String {
        self.git(&["add", "-A"]);
        fixture_git(&self.dir, &["commit", "--quiet", "-m", msg], date);
        self.git(&["rev-parse", "HEAD"])
    }

    fn checkout(&self, what: &[&str]) {
        let mut args = vec!["checkout", "--quiet"];
        args.extend_from_slice(what);
        self.git(&args);
    }

    /// Push every branch and tag to `remote`.
    fn push_all(&self, remote: &Repo) {
        let url = remote.url();
        self.git(&[
            "push",
            "--quiet",
            "--force",
            &url,
            "refs/heads/*:refs/heads/*",
        ]);
        self.git(&[
            "push",
            "--quiet",
            "--force",
            &url,
            "refs/tags/*:refs/tags/*",
        ]);
    }
}

const FIVE: &str = "one\ntwo\nthree\nfour\nfive\n";

fn lines_with(n: usize, text: &str) -> String {
    FIVE.lines()
        .enumerate()
        .map(|(i, l)| if i + 1 == n { text } else { l })
        .map(|l| format!("{l}\n"))
        .collect()
}

struct Env {
    _tmp: TempDir,
    root: PathBuf,
    pool: Pool,
}

impl Env {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let pool = Pool::open(Git::new().allow_file_protocol(), &root.join("builds"))
            .await
            .expect("pool opens");
        Self {
            _tmp: tmp,
            root,
            pool,
        }
    }

    fn repo(&self, name: &str) -> Repo {
        Repo::init(self.root.join(name))
    }

    fn remote(&self, name: &str) -> Repo {
        Repo::bare(self.root.join(format!("{name}.git")))
    }

    async fn fetch(&self, url: &str, r: &str) -> String {
        self.pool
            .resolve_and_fetch(
                url,
                &FetchTarget::Ref(r.into()),
                None,
                Forge::Plain,
                None,
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("fetch {r}: {e}"))
            .sha
    }

    /// A worktree of `base`, assembled with `extras`.
    async fn assemble(
        &self,
        name: &str,
        base: &str,
        extras: &[ResolvedExtraSpec],
    ) -> (PathBuf, AssembleReport, Vec<String>) {
        let wt = self.pool.work_dir().join(name);
        self.pool.worktree_add(base, &wt, None).await.unwrap();
        let (cap, lines) = capture();
        let report = self
            .pool
            .assemble(&wt, base, extras, AssembleOpts::default(), Some(&*cap))
            .await
            .unwrap_or_else(|e| panic!("assemble: {e}"));
        let lines = lines.lock().unwrap().clone();
        (wt, report, lines)
    }
}

type Lines = Arc<Mutex<Vec<String>>>;
type Sink = Box<dyn Fn(&str) + Send + Sync>;

/// A progress sink that keeps every line.
fn capture() -> (Sink, Lines) {
    let lines: Lines = Arc::default();
    let sink = lines.clone();
    (
        Box::new(move |l: &str| sink.lock().unwrap().push(l.to_string())),
        lines,
    )
}

fn extra(label: &str, sha: &str) -> ResolvedExtraSpec {
    ResolvedExtraSpec {
        label: label.into(),
        sha: sha.into(),
        merged_upstream: false,
        fork_point: None,
        upstream_tip: None,
    }
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

fn in_wt(dir: &Path, args: &[&str]) -> String {
    fixture_git(dir, args, "@1790000000 +0000")
}

/// The upstream every merge test starts from: `master` at c1 over c0, with
/// branches cut from c0.
struct Upstream {
    up: Repo,
    remote: Repo,
    c0: String,
}

fn upstream(env: &Env) -> Upstream {
    let up = env.repo("up");
    up.write("a.txt", FIVE);
    let c0 = up.commit("c0");
    up.write("other.txt", "other\n");
    up.commit("c1");
    let remote = env.remote("remote");
    Upstream { up, remote, c0 }
}

impl Upstream {
    /// A branch off c0 changing line `n` of a.txt.
    fn branch(&self, name: &str, n: usize, text: &str) -> String {
        self.up.checkout(&["-b", name, &self.c0]);
        self.up.write("a.txt", &lines_with(n, text));
        let sha = self.up.commit(name);
        self.up.checkout(&["master"]);
        sha
    }

    fn master_edits(&self, n: usize, text: &str) -> String {
        let current = std::fs::read_to_string(self.up.dir.join("a.txt")).unwrap();
        let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
        lines[n - 1] = text.to_string();
        self.up.write("a.txt", &(lines.join("\n") + "\n"));
        self.up.commit(&format!("master edits line {n}"))
    }

    fn publish(&self) {
        self.up.push_all(&self.remote);
    }
}

// ---------------------------------------------------------------------------
// Resolution and fetch
// ---------------------------------------------------------------------------

async fn resolve(env: &Env, url: &str, t: FetchTarget, forge: Forge) -> Result<Fetched, String> {
    env.pool
        .resolve_and_fetch(url, &t, None, forge, None, None)
        .await
}

#[tokio::test]
async fn branches_tags_commits_and_prs_resolve_against_a_remote() {
    let env = Env::new().await;
    let up = env.repo("up");
    up.write("a.txt", "1\n");
    let c1 = up.commit("c1");
    up.git(&["tag", "-a", "v1", "-m", "release 1"]);
    up.write("a.txt", "2\n");
    let c2 = up.commit("c2");
    // A tag that looks like a short SHA, as llama.cpp's all do.
    up.git(&["tag", "b6000"]);
    up.checkout(&["-b", "feature"]);
    up.write("f.txt", "f\n");
    let c3 = up.commit("c3");
    up.checkout(&["master"]);
    let remote = env.remote("remote");
    up.push_all(&remote);
    // What GitHub and GitLab advertise for a PR / MR.
    up.checkout(&["-b", "pr-src", &c1]);
    up.write("pr.txt", "pr\n");
    let c4 = up.commit("c4");
    up.checkout(&["master"]);
    up.git(&[
        "push",
        "--quiet",
        &remote.url(),
        &format!("{c4}:refs/pull/7/head"),
    ]);
    up.git(&[
        "push",
        "--quiet",
        &remote.url(),
        &format!("{c4}:refs/merge-requests/9/head"),
    ]);

    let url = remote.url();
    let key = remote_key(&url);
    let get = |t: FetchTarget, forge: Forge| resolve(&env, &url, t, forge);

    let m = get(FetchTarget::Ref("master".into()), Forge::Plain)
        .await
        .unwrap();
    assert_eq!(m.sha, c2);
    assert_eq!(
        m.kind,
        RefKind::Branch {
            name: "master".into()
        }
    );
    assert_eq!(m.local_ref, format!("refs/lmgw/{key}/heads/master"));

    let v1 = get(FetchTarget::Ref("v1".into()), Forge::Plain)
        .await
        .unwrap();
    assert_eq!(v1.sha, c1, "an annotated tag resolves to its commit");
    assert_eq!(
        v1.kind,
        RefKind::Tag {
            name: "v1".into(),
            annotated: true
        }
    );
    let b = get(FetchTarget::Ref("b6000".into()), Forge::Plain)
        .await
        .unwrap();
    assert_eq!(b.sha, c2);
    assert!(matches!(
        b.kind,
        RefKind::Tag {
            annotated: false,
            ..
        }
    ));

    let full = get(FetchTarget::Ref(c1.to_uppercase()), Forge::Plain)
        .await
        .unwrap();
    assert_eq!(
        (full.sha.as_str(), full.kind),
        (c1.as_str(), RefKind::Commit)
    );
    let short = get(FetchTarget::Ref(c3[..9].into()), Forge::Plain)
        .await
        .unwrap();
    assert_eq!(short.sha, c3, "a short SHA of a branch tip resolves");
    let err = get(FetchTarget::Ref(c4[..9].into()), Forge::Plain)
        .await
        .unwrap_err();
    assert!(err.contains("full 40-character SHA"), "{err}");
    let err = get(FetchTarget::Ref("no-such-branch".into()), Forge::Plain)
        .await
        .unwrap_err();
    assert!(err.contains("not a branch or tag"), "{err}");

    let pr = get(FetchTarget::Pr(7), Forge::Github).await.unwrap();
    assert_eq!(pr.sha, c4);
    assert_eq!(pr.local_ref, format!("refs/lmgw/{key}/pr/7"));
    let mr = get(FetchTarget::Pr(9), Forge::Gitlab).await.unwrap();
    assert_eq!(mr.sha, c4);
    let err = get(FetchTarget::Pr(8), Forge::Github).await.unwrap_err();
    assert!(
        err.contains("PR #8 (refs/pull/8/head) does not exist"),
        "{err}"
    );
    assert!(get(FetchTarget::Pr(7), Forge::Plain).await.is_err());
    // URLs are checked before git sees them (§10), `--` or not.
    let err = resolve(&env, "ext::sh%-c%touch", FetchTarget::Pr(7), Forge::Github)
        .await
        .unwrap_err();
    assert!(err.contains("not a git URL"), "{err}");
    let err = env
        .pool
        .git()
        .ls_remote("-upload-pack=x", None)
        .await
        .unwrap_err();
    assert!(err.contains("starts with '-'"), "{err}");
    let full_ref = get(FetchTarget::Ref("refs/pull/7/head".into()), Forge::Plain)
        .await
        .unwrap();
    assert_eq!(full_ref.sha, c4);
    assert_eq!(full_ref.local_ref, format!("refs/lmgw/{key}/pr/7"));

    let pinned = env
        .pool
        .resolve_and_fetch(
            &url,
            &FetchTarget::Pr(7),
            Some(&c1),
            Forge::Github,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        (pinned.sha.as_str(), pinned.kind),
        (c1.as_str(), RefKind::Pinned)
    );

    // Nothing moved: the ref is moved locally, nothing is fetched.
    let (cap, lines) = capture();
    let again = env
        .pool
        .resolve_and_fetch(
            &url,
            &FetchTarget::Ref("master".into()),
            None,
            Forge::Plain,
            None,
            Some(&*cap),
        )
        .await
        .unwrap();
    assert_eq!(again.sha, c2);
    let lines = lines.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("nothing to fetch")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("$ git fetch")),
        "{lines:?}"
    );

    // The remote moves: the next resolve fetches.
    up.write("a.txt", "3\n");
    let c5 = up.commit("c5");
    up.push_all(&remote);
    assert_eq!(env.fetch(&url, "master").await, c5);
}

#[tokio::test]
async fn a_pin_that_no_ref_reaches_is_still_found_in_the_pool() {
    let env = Env::new().await;
    let u = upstream(&env);
    let old = u.branch("feature", 2, "old");
    u.publish();
    let url = u.remote.url();
    assert_eq!(env.fetch(&url, "feature").await, old);
    // Force-push the branch away from the pinned commit.
    u.up.checkout(&["feature"]);
    u.up.git(&["reset", "--quiet", "--hard", &u.c0]);
    u.up.write("a.txt", &lines_with(2, "new"));
    u.up.commit("rewritten");
    u.up.checkout(&["master"]);
    u.publish();
    let pinned = env
        .pool
        .resolve_and_fetch(
            &url,
            &FetchTarget::Ref("feature".into()),
            Some(&old),
            Forge::Plain,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(pinned.sha, old);
    let err = env
        .pool
        .resolve_and_fetch(
            &url,
            &FetchTarget::Ref("feature".into()),
            Some(&"e".repeat(40)),
            Forge::Plain,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(err.contains("could not be fetched"), "{err}");
}

#[tokio::test]
async fn ls_remote_reports_the_default_branch_and_fetch_default_branch_follows_it() {
    let env = Env::new().await;
    let u = upstream(&env);
    u.up.git(&["tag", "-a", "v2", "-m", "v2"]);
    u.publish();
    let refs = env
        .pool
        .git()
        .ls_remote(&u.remote.url(), None)
        .await
        .unwrap();
    assert_eq!(refs.default_branch.as_deref(), Some("master"));
    assert!(refs.tag("v2").unwrap().annotated());
    let tip = env
        .pool
        .fetch_default_branch(&u.remote.url(), None, None)
        .await
        .unwrap();
    assert_eq!(Some(tip.sha.as_str()), refs.branch("master"));
    let err = env
        .pool
        .git()
        .ls_remote(&format!("file://{}/nope.git", env.root.display()), None)
        .await
        .unwrap_err();
    assert!(err.contains("git ls-remote"), "{err}");
}

#[tokio::test]
async fn build_number_commit_date_and_files_come_from_the_pool() {
    let env = Env::new().await;
    let up = env.repo("up");
    up.write("Dockerfile", "FROM x AS server");
    up.commit("one");
    up.write("sub/Dockerfile", "FROM y\n");
    let two = up.commit_at("two", "2026-09-25T18:44:03-04:00");
    let remote = env.remote("remote");
    up.push_all(&remote);
    assert_eq!(env.fetch(&remote.url(), "master").await, two);

    assert_eq!(env.pool.build_number(&two).await.unwrap(), 2);
    let date = env.pool.commit_date(&two).await.unwrap();
    assert_eq!(date.to_rfc3339(), "2026-09-25T18:44:03-04:00");
    assert_eq!(
        env.pool
            .read_file(&two, "Dockerfile")
            .await
            .unwrap()
            .as_deref(),
        Some("FROM x AS server"),
        "exactly the blob — no newline added"
    );
    assert_eq!(
        env.pool
            .read_file(&two, "sub/Dockerfile")
            .await
            .unwrap()
            .as_deref(),
        Some("FROM y\n")
    );
    assert_eq!(env.pool.read_file(&two, "missing").await.unwrap(), None);
    assert!(env
        .pool
        .read_file(&"0".repeat(40), "Dockerfile")
        .await
        .is_err());
}

// ---------------------------------------------------------------------------
// Assemble
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extras_merge_in_order_as_lmgw() {
    let env = Env::new().await;
    let u = upstream(&env);
    let x1 = u.branch("x1", 1, "ONE");
    let x2 = u.branch("x2", 5, "FIVE");
    u.publish();
    let url = u.remote.url();
    let base = env.fetch(&url, "master").await;
    assert_eq!(env.fetch(&url, "x1").await, x1);
    assert_eq!(env.fetch(&url, "x2").await, x2);

    let (wt, report, lines) = env
        .assemble("run-1", &base, &[extra("x1", &x1), extra("x2", &x2)])
        .await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.base, base);
    assert!(matches!(
        report.extras[0].result,
        ExtraResult::Merged { .. }
    ));
    assert!(matches!(
        report.extras[1].result,
        ExtraResult::Merged { .. }
    ));
    assert_eq!(read(&wt, "a.txt"), "ONE\ntwo\nthree\nfour\nFIVE\n");
    assert_eq!(in_wt(&wt, &["rev-parse", "HEAD"]), report.head);
    // Two --no-ff merge commits on the base, committed by lmgw.
    assert_eq!(
        in_wt(&wt, &["log", "--format=%cn <%ce>|%s", "-2"]),
        "lmgw <lmgw@localhost>|lmgw: merge x2 (".to_string()
            + &x2[..7]
            + ")\nlmgw <lmgw@localhost>|lmgw: merge x1 ("
            + &x1[..7]
            + ")"
    );
    assert_eq!(in_wt(&wt, &["rev-parse", "HEAD~2"]), base);
    assert!(
        lines.iter().any(|l| l.starts_with("$ git merge --no-ff")),
        "{lines:?}"
    );
    env.pool.worktree_remove(&wt, None).await.unwrap();
    assert!(!wt.exists());
}

#[tokio::test]
async fn a_conflict_names_the_extra_its_files_and_what_was_merged_before() {
    let env = Env::new().await;
    let u = upstream(&env);
    let x1 = u.branch("x1", 1, "ONE");
    let x3 = u.branch("x3", 3, "three-from-pr");
    u.master_edits(3, "three-on-master");
    u.publish();
    let url = u.remote.url();
    let base = env.fetch(&url, "master").await;
    env.fetch(&url, "x1").await;
    env.fetch(&url, "x3").await;

    let (wt, report, _) = env
        .assemble(
            "run-c",
            &base,
            &[extra("x1", &x1), extra("PR #3", &x3), extra("never", &x1)],
        )
        .await;
    let c = report.conflict.as_ref().expect("a conflict");
    assert_eq!(c.label, "PR #3");
    assert_eq!(c.mode, MergeMode::Merge);
    assert_eq!(c.files, vec!["a.txt"]);
    assert_eq!(c.merged_before, vec!["x1"]);
    assert!(c
        .message()
        .contains("does not merge cleanly after x1: conflicts in a.txt"));
    assert_eq!(report.extras.len(), 1, "it stops at the conflict");
    let steps = report.steps();
    let outcomes: Vec<&str> = steps.iter().map(|s| s.outcome.as_str()).collect();
    assert_eq!(outcomes, vec!["merged", "conflict"]);
    assert_eq!(steps[1].files, vec!["a.txt"]);
    let json = serde_json::to_value(&report.extras[0]).unwrap();
    assert_eq!(json["outcome"], "merged");
    assert_eq!(json["label"], "x1");
    // The merge was aborted: the tree is x1's merge, clean.
    assert_eq!(in_wt(&wt, &["rev-parse", "HEAD"]), report.head);
    assert_eq!(in_wt(&wt, &["status", "--porcelain"]), "");
    env.pool.worktree_remove(&wt, None).await.unwrap();
}

#[tokio::test]
async fn a_squash_merged_pr_and_an_ancestor_are_already_in_base() {
    let env = Env::new().await;
    let u = upstream(&env);
    let pr = u.branch("pr", 2, "TWO");
    // Upstream squash-merges it: same change, a new commit on master.
    u.up.write("a.txt", &lines_with(2, "TWO"));
    u.up.commit("Squashed PR (#2)");
    u.publish();
    let url = u.remote.url();
    let base = env.fetch(&url, "master").await;
    env.fetch(&url, "pr").await;

    let (_, report, lines) = env
        .assemble("run-s", &base, &[extra("PR #2", &pr), extra("old", &u.c0)])
        .await;
    assert!(report.is_clean());
    match &report.extras[0].result {
        ExtraResult::AlreadyInBase { why } => assert!(why.contains("changed nothing"), "{why}"),
        other => panic!("{other:?}"),
    }
    match &report.extras[1].result {
        ExtraResult::AlreadyInBase { why } => assert!(why.contains("ancestor"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(lines
        .iter()
        .any(|l| l.contains("PR #2") && l.contains("already in base")));
}

#[tokio::test]
async fn a_pr_merged_upstream_is_skipped_on_the_forges_word() {
    let env = Env::new().await;
    let u = upstream(&env);
    let x3 = u.branch("x3", 3, "pr");
    u.master_edits(3, "master");
    u.publish();
    let url = u.remote.url();
    let base = env.fetch(&url, "master").await;
    env.fetch(&url, "x3").await;
    let mut merged = extra("PR #3", &x3);
    merged.merged_upstream = true;
    let (wt, report, _) = env.assemble("run-u", &base, &[merged]).await;
    assert!(report.is_clean(), "it would have conflicted");
    assert_eq!(report.extras[0].result, ExtraResult::MergedUpstream);
    assert_eq!(report.head, base);
    assert_eq!(in_wt(&wt, &["rev-parse", "HEAD"]), base);
}

/// An upstream llama.cpp PR on top of ik_llama.cpp (§14.1): no shared
/// history, so the PR's own changes since its fork point are applied.
#[tokio::test]
async fn an_extra_with_unrelated_history_is_squash_applied() {
    let env = Env::new().await;
    // "llama.cpp": a shared file, and a PR changing line 3 of it.
    let llama = env.repo("llama");
    llama.write("common.c", FIVE);
    let fork = llama.commit("llama base");
    llama.checkout(&["-b", "pr"]);
    llama.write("common.c", &lines_with(3, "THREE (pr)"));
    llama.write("new.c", "added by the pr\n");
    let pr = llama.commit("the pr");
    llama.checkout(&["master"]);
    llama.write("unrelated.c", "upstream moved on\n");
    llama.commit("llama moves on");
    let llama_remote = env.remote("llama");
    llama.push_all(&llama_remote);
    // "ik": the same file at the same content, no common commit.
    let ik = env.repo("ik");
    ik.write("common.c", FIVE);
    ik.write("ik.c", "ik only\n");
    ik.commit("ik base");
    let ik_remote = env.remote("ik");
    ik.push_all(&ik_remote);

    let base = env.fetch(&ik_remote.url(), "master").await;
    assert_eq!(env.fetch(&llama_remote.url(), "pr").await, pr);
    let tip = env
        .pool
        .fetch_default_branch(&llama_remote.url(), None, None)
        .await
        .unwrap();
    let mut x = extra("llama.cpp pr", &pr);
    x.upstream_tip = Some(tip.sha.clone());

    let (wt, report, lines) = env.assemble("run-sq", &base, &[x.clone()]).await;
    assert!(report.is_clean(), "{report:?}");
    match &report.extras[0].result {
        ExtraResult::SquashApplied { commit, fork_point } => {
            assert_eq!(fork_point, &fork);
            assert_eq!(commit, &report.head);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(read(&wt, "common.c"), lines_with(3, "THREE (pr)"));
    assert_eq!(read(&wt, "new.c"), "added by the pr\n");
    assert!(
        !wt.join("unrelated.c").exists(),
        "only the PR's own changes"
    );
    assert_eq!(read(&wt, "ik.c"), "ik only\n");
    assert_eq!(
        in_wt(&wt, &["rev-list", "--parents", "-1", "HEAD"]),
        format!("{} {base}", report.head),
        "one commit, on the base"
    );
    assert!(
        lines.iter().any(|l| l.contains("squash-applying")),
        "{lines:?}"
    );

    // The same with a fork point given (the forge PR's base).
    let mut given = extra("llama.cpp pr", &pr);
    given.fork_point = Some(fork.clone());
    let (_, report, _) = env.assemble("run-sq2", &base, &[given]).await;
    assert!(report.is_clean());

    // ik changed that line: a conflict, reported like a merge's.
    ik.write("common.c", &lines_with(3, "three (ik)"));
    ik.commit("ik edits line 3");
    ik.push_all(&ik_remote);
    let base2 = env.fetch(&ik_remote.url(), "master").await;
    let (wt2, report, _) = env.assemble("run-sq3", &base2, &[x]).await;
    let c = report.conflict.expect("a conflict");
    assert_eq!(c.mode, MergeMode::SquashApply);
    assert_eq!(c.files, vec!["common.c"]);
    assert!(c
        .message()
        .contains("does not squash-apply cleanly onto the base alone"));
    assert_eq!(in_wt(&wt2, &["status", "--porcelain"]), "");

    // No fork point to be had: said, not guessed.
    let (_, report, _) = env
        .assemble("run-sq4", &base, &[extra("orphan", &pr)])
        .await;
    let c = report.conflict.expect("refused");
    assert!(c.reason.unwrap().contains("no fork point"));

    // An extra already on its own remote's default branch is its own fork
    // point: nothing of its own to squash-apply — reported as such, never
    // as "already in base" (the tree never had its changes).
    let mut on_master = extra("llama.cpp master", &fork);
    on_master.upstream_tip = Some(tip.sha.clone());
    let (_, report, _) = env.assemble("run-sq5", &base, &[on_master]).await;
    let c = report.conflict.expect("refused");
    assert_eq!(c.mode, MergeMode::SquashApply);
    assert!(
        c.reason
            .as_deref()
            .unwrap()
            .contains("already contained in its own remote's default branch"),
        "{c:?}"
    );
}

/// sd.cpp's case (§14.1): the base bumped a submodule, a PR bumped it further
/// from an older base. merge-ort fast-forwards the gitlink only while the
/// submodule is checked out — hence submodules at base before the merges.
#[tokio::test]
async fn a_submodule_bump_fast_forwards_and_the_tree_is_updated_after_the_merges() {
    let env = Env::new().await;
    let sub = env.repo("ggml");
    sub.write("ggml.c", "s0\n");
    let s0 = sub.commit("s0");
    sub.write("ggml.c", "s1\n");
    let s1 = sub.commit("s1");
    sub.write("s2.c", "only in s2\n");
    let s2 = sub.commit("s2");
    let sub_remote = env.remote("ggml");
    sub.push_all(&sub_remote);

    let sd = env.repo("sd");
    sd.write("main.c", "main\n");
    sd.git(&["submodule", "add", "--quiet", &sub_remote.url(), "ggml"]);
    fixture_git(
        &sd.dir.join("ggml"),
        &["checkout", "--quiet", &s0],
        "@1790000000 +0000",
    );
    let base0 = sd.commit("base with ggml at s0");
    sd.checkout(&["-b", "pr"]);
    fixture_git(
        &sd.dir.join("ggml"),
        &["checkout", "--quiet", &s2],
        "@1790000000 +0000",
    );
    let pr = sd.commit("bump ggml to s2");
    sd.checkout(&["master"]);
    sd.git(&["submodule", "update", "--quiet"]);
    fixture_git(
        &sd.dir.join("ggml"),
        &["checkout", "--quiet", &s1],
        "@1790000000 +0000",
    );
    sd.commit("bump ggml to s1");
    let sd_remote = env.remote("sd");
    sd.push_all(&sd_remote);
    let _ = base0;

    let base = env.fetch(&sd_remote.url(), "master").await;
    env.fetch(&sd_remote.url(), "pr").await;
    let (wt, report, lines) = env.assemble("run-sub", &base, &[extra("PR #9", &pr)]).await;
    assert!(report.is_clean(), "{report:?}\n{lines:#?}");
    assert!(matches!(
        report.extras[0].result,
        ExtraResult::Merged { .. }
    ));
    assert!(
        in_wt(&wt, &["ls-tree", "HEAD", "ggml"]).contains(&s2),
        "the gitlink is the PR's"
    );
    assert_eq!(in_wt(&wt.join("ggml"), &["rev-parse", "HEAD"]), s2);
    assert_eq!(
        read(&wt, "ggml/s2.c"),
        "only in s2\n",
        "checked out after the merge"
    );

    // The submodule clone borrowed the pool's objects, and its commit was
    // kept there for the next run.
    let modules = env.pool.dir().join("worktrees/run-sub/modules/ggml");
    assert!(modules.join("objects/info/alternates").exists());
    let kept = format!("refs/lmgw/{}/commits/{s2}", remote_key(&sub_remote.url()));
    assert_eq!(
        fixture_git(env.pool.dir(), &["rev-parse", &kept], "@0 +0000"),
        s2
    );

    // A worktree with submodules is removed, registration and all.
    env.pool.worktree_remove(&wt, None).await.unwrap();
    assert!(!wt.exists());
    assert!(!env.pool.dir().join("worktrees/run-sub").exists());
    let listed = fixture_git(
        env.pool.dir(),
        &["worktree", "list", "--porcelain"],
        "@0 +0000",
    );
    assert_eq!(listed.matches("worktree ").count(), 1, "{listed}");

    // Check merge runs the same assemble, and leaves nothing behind.
    let report = env
        .pool
        .check_merge(&base, &[extra("PR #9", &pr)], AssembleOpts::default(), None)
        .await
        .unwrap();
    assert!(report.is_clean());
    let left: Vec<_> = std::fs::read_dir(env.pool.work_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn check_merge_reports_a_conflict_and_cleans_up() {
    let env = Env::new().await;
    let u = upstream(&env);
    let x3 = u.branch("x3", 3, "pr");
    u.master_edits(3, "master");
    u.publish();
    let url = u.remote.url();
    let base = env.fetch(&url, "master").await;
    env.fetch(&url, "x3").await;
    let report = env
        .pool
        .check_merge(&base, &[extra("PR #3", &x3)], AssembleOpts::default(), None)
        .await
        .unwrap();
    assert_eq!(report.conflict.unwrap().files, vec!["a.txt"]);
    assert_eq!(std::fs::read_dir(env.pool.work_dir()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_worktree_that_is_not_at_the_base_is_refused() {
    let env = Env::new().await;
    let u = upstream(&env);
    u.publish();
    let base = env.fetch(&u.remote.url(), "master").await;
    let wt = env.pool.work_dir().join("run-x");
    env.pool.worktree_add(&u.c0, &wt, None).await.unwrap();
    let err = env
        .pool
        .assemble(&wt, &base, &[], AssembleOpts::default(), None)
        .await
        .unwrap_err();
    assert!(err.contains("not at the base"), "{err}");
}

// ---------------------------------------------------------------------------
// Hardening
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_goes_through_the_environment_never_argv_or_the_log() {
    let token = "ghp_TOPSECRET123";
    let auth = GitAuth::new("https://github.com/o/r", token, Forge::Github).unwrap();
    let git = Git::new();
    let cmd = git.command(
        Some(Path::new("/nonexistent")),
        &[
            "fetch",
            "--no-tags",
            "--",
            "https://github.com/o/r",
            "+refs/heads/x:refs/y",
        ],
        Some(&auth),
    );
    let std = cmd.as_std();
    let args: Vec<String> = std
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
    };
    for a in &args {
        assert!(
            !a.contains(token) && !a.contains(&encoded),
            "argv carries the token: {a}"
        );
    }
    // The hardening itself.
    for want in [
        "credential.helper=",
        "url.https://github.com/.insteadOf=git@github.com:",
        "core.hooksPath=/dev/null",
        "commit.gpgSign=false",
    ] {
        assert!(
            args.iter().any(|a| a == want),
            "missing -c {want}: {args:?}"
        );
    }
    assert!(!args.iter().any(|a| a.starts_with("protocol.file.allow")));
    let envs: Vec<(String, Option<String>)> = std
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let env = |k: &str| envs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(env("GIT_TERMINAL_PROMPT"), Some(Some("0".into())));
    assert_eq!(env("GIT_ASKPASS"), Some(Some(String::new())));
    assert_eq!(env("SSH_ASKPASS"), Some(Some(String::new())));
    assert_eq!(
        env("GIT_DIR"),
        Some(None),
        "an inherited GIT_DIR is removed"
    );
    assert_eq!(env("GIT_CONFIG_COUNT"), Some(Some("1".into())));
    assert_eq!(
        env("GIT_CONFIG_KEY_0"),
        Some(Some("http.https://github.com/.extraHeader".into()))
    );
    assert_eq!(
        env("GIT_CONFIG_VALUE_0"),
        Some(Some(format!("Authorization: Basic {encoded}")))
    );

    // Git really reads it from there — and the echoed command line does not
    // carry it.
    let (cap, lines) = capture();
    let out = git
        .run(
            None,
            &["config", "--get", "http.https://github.com/.extraHeader"],
            Some(&auth),
            Some(&*cap),
        )
        .await
        .unwrap();
    assert_eq!(out.stdout.trim(), format!("Authorization: Basic {encoded}"));
    let first = lines.lock().unwrap()[0].clone();
    assert_eq!(
        first,
        "$ git config --get http.https://github.com/.extraHeader"
    );
    // Without auth no header is configured at all.
    let out = git
        .run(
            None,
            &["config", "--get", "http.https://github.com/.extraHeader"],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!out.ok());
}

#[tokio::test]
async fn a_file_url_submodule_is_blocked_unless_allowed() {
    let env = Env::new().await;
    let sub = env.repo("s");
    sub.write("x", "x\n");
    sub.commit("s");
    let sub_remote = env.remote("s");
    sub.push_all(&sub_remote);
    let sup = env.repo("sup");
    sup.write("m", "m\n");
    sup.git(&["submodule", "add", "--quiet", &sub_remote.url(), "s"]);
    sup.commit("with s");
    let sup_remote = env.remote("sup");
    sup.push_all(&sup_remote);

    // A production git: the local submodule URL must not be cloned.
    let strict = Pool::open(Git::new(), &env.root.join("strict"))
        .await
        .unwrap();
    let base = strict
        .resolve_and_fetch(
            &sup_remote.url(),
            &FetchTarget::Ref("master".into()),
            None,
            Forge::Plain,
            None,
            None,
        )
        .await
        .unwrap()
        .sha;
    let wt = strict.work_dir().join("w");
    strict.worktree_add(&base, &wt, None).await.unwrap();
    let err = strict
        .assemble(&wt, &base, &[], AssembleOpts::default(), None)
        .await
        .unwrap_err();
    assert!(err.contains("submodule"), "{err}");
    assert!(!wt.join("s/x").exists());
    strict.worktree_remove(&wt, None).await.unwrap();
}

#[tokio::test]
async fn git_is_checked_for_with_a_visible_error() {
    let v = git_available(&Git::new()).await.unwrap();
    assert!(v.starts_with("2."), "{v}");
    let err = git_available(&Git::with_program("/nonexistent/bin/git"))
        .await
        .unwrap_err();
    assert!(err.contains("not installed"), "{err}");
}

/// The remote deletes branch `a` and creates `a/b` (and later the reverse):
/// the pool's old ref stands where the new one needs a directory (or a
/// file), and is cleared out of the way instead of failing the fetch.
#[tokio::test]
async fn a_branch_renamed_across_a_slash_still_fetches() {
    let env = Env::new().await;
    let u = upstream(&env);
    let a = u.branch("a", 1, "A");
    u.publish();
    let url = u.remote.url();
    assert_eq!(env.fetch(&url, "a").await, a);

    u.up.git(&["push", "--quiet", &url, "--delete", "a"]);
    u.up.git(&["branch", "--quiet", "-D", "a"]);
    let ab = u.branch("a/b", 2, "AB");
    u.publish();
    assert_eq!(env.fetch(&url, "a/b").await, ab);

    u.up.git(&["push", "--quiet", &url, "--delete", "a/b"]);
    u.up.git(&["branch", "--quiet", "-D", "a/b"]);
    let a2 = u.branch("a", 3, "A2");
    u.publish();
    assert_eq!(env.fetch(&url, "a").await, a2);
}

/// A git killed mid-fetch (a canceled run) leaves its lock files in the
/// pool; the next holder of the pool lock removes them rather than failing
/// on "Unable to create '….lock': File exists".
#[tokio::test]
async fn stale_git_locks_in_the_pool_are_cleared_by_the_next_holder() {
    let env = Env::new().await;
    let u = upstream(&env);
    let a = u.branch("a", 1, "A");
    u.publish();
    let url = u.remote.url();
    assert_eq!(env.fetch(&url, "a").await, a);
    // A worktree's own lock is its run's business, not the pool's.
    let wt = env.pool.work_dir().join("run-x-1");
    env.pool.worktree_add(&a, &wt, None).await.unwrap();
    let wt_admin = env.pool.dir().join("worktrees").join("run-x-1");
    std::fs::write(wt_admin.join("index.lock"), "").unwrap();

    let key = lmgw_core::backends::git::remote_key(&url);
    let stale = [
        env.pool.dir().join(format!("refs/lmgw/{key}/heads/a.lock")),
        env.pool.dir().join("packed-refs.lock"),
        env.pool.dir().join("objects/pack/tmp_pack_x"),
    ];
    for f in &stale {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "").unwrap();
    }
    // The branch moved: the ref is updated in place, under the pool lock.
    u.up.git(&["branch", "--quiet", "-D", "a"]);
    let a2 = u.branch("a", 2, "A2");
    u.publish();
    assert_eq!(env.fetch(&url, "a").await, a2);
    for f in &stale {
        assert!(!f.exists(), "{} is gone", f.display());
    }
    assert!(wt_admin.join("index.lock").exists());
    assert!(env.root.join("builds/git/pool.lock").exists());
}

#[tokio::test]
async fn pools_on_one_directory_share_their_lock_and_fetch_concurrently() {
    let env = Env::new().await;
    let u = upstream(&env);
    let a = u.branch("a", 1, "A");
    let b = u.branch("b", 2, "B");
    u.publish();
    let url = u.remote.url();
    let second = Pool::open(Git::new().allow_file_protocol(), &env.root.join("builds"))
        .await
        .unwrap();
    let t_a = FetchTarget::Ref("a".into());
    let t_b = FetchTarget::Ref("b".into());
    let (ra, rb) = tokio::join!(
        env.pool
            .resolve_and_fetch(&url, &t_a, None, Forge::Plain, None, None),
        second.resolve_and_fetch(&url, &t_b, None, Forge::Plain, None, None),
    );
    assert_eq!(ra.unwrap().sha, a);
    assert_eq!(rb.unwrap().sha, b);
}
