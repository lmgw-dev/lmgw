//! Update detection (container-builds design §8, §15 `build_updates_check`,
//! `container_image_pull`): builds against real git and local fixture
//! remotes (a branch, an annotated tag, a ref extra, a commit ref), PR/MR
//! state against mock GitHub and GitLab APIs (and a rate limit that becomes
//! a sentence, not an update), registry digests against a mock registry
//! speaking the anonymous bearer-token flow (index versus manifest), the
//! schedule honouring `0`, and **Pull update** through the fake podman of
//! `support/backends_fake.rs`. Nothing here reaches a real forge, registry
//! or podman.

use crate::common;
use crate::support::backends_fake as support;

use std::sync::Arc;
use std::time::Duration;

use lmgw_api_types::builds::{
    BuildRunInputs, ResolvedExtra, ResolvedInputs, UpdateStatus, UpdatesSummary,
};
use lmgw_core::backends::forge::{ForgeClient, GatewayForge};
use lmgw_core::backends::git::{FetchTarget, Git, Pool};
use lmgw_core::backends::oci::{ImageRef, RegistryClient};
use lmgw_core::backends::updates::{self, Cadence};
use lmgw_core::backends::{
    BuildExtra, BuildRunPatch, BuildRunStatus, BuildSpec, BuildTrigger, Forge, NewBuildRun,
};
use lmgw_core::store;
use lmgw_core::telemetry::Event;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::*;
use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

/// A verified, promoted run of `build_id` that built `base` (+ `extras`),
/// recorded as the executor records one.
async fn seed_verified(h: &Harness, build_id: i64, base: &str, extras: Vec<ResolvedExtra>) -> i64 {
    let build = store::get_build(&h.state.db, build_id)
        .await
        .unwrap()
        .unwrap();
    let run_id =
        store::insert_build_run(&h.state.db, &NewBuildRun::of(&build, BuildTrigger::Manual))
            .await
            .unwrap();
    store::update_build_run(
        &h.state.db,
        run_id,
        &BuildRunPatch {
            base_sha: Some(base.into()),
            inputs: Some(BuildRunInputs {
                config: build.spec.clone(),
                resolved: Some(ResolvedInputs {
                    base_sha: base.into(),
                    extras,
                    ..ResolvedInputs::default()
                }),
                cfg_hash: None,
            }),
            ..BuildRunPatch::default()
        },
    )
    .await
    .unwrap();
    store::finish_build_run(&h.state.db, run_id, BuildRunStatus::Succeeded, None)
        .await
        .unwrap();
    store::promote_build_run(&h.state.db, run_id).await.unwrap();
    run_id
}

async fn check(h: &Harness, id: i64) -> UpdateStatus {
    updates::check_build(&h.state, id)
        .await
        .unwrap()
        .expect("a build with a verified run is checked")
}

fn push_all(h: &Harness) {
    h.work.push(&h.upstream, "refs/heads/*:refs/heads/*");
}

async fn op(gw: &common::Gw, name: &str, args: Value) -> Value {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "{name}: {body}");
    body
}

async fn op_err(gw: &common::Gw, name: &str, args: Value) -> String {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    body["message"].as_str().unwrap().to_string()
}

async fn wait_job(h: &Harness, id: i64) -> store::JobRow {
    for _ in 0..800 {
        let row = store::get_job(&h.state.db, id).await.unwrap().unwrap();
        if matches!(row.status.as_str(), "done" | "failed" | "canceled") {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} never finished");
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_moved_branch_is_counted_only_from_the_pool_and_a_new_run_clears_it() {
    let h = Harness::new().await;
    let spec = h.spec();
    let id = h.build(&spec).await;

    // No run to compare with: nothing is asked, nothing is badged.
    assert_eq!(updates::check_build(&h.state, id).await.unwrap(), None);

    let a = h.base_sha();
    seed_verified(&h, id, &a, vec![]).await;
    let u = check(&h, id).await;
    assert!(!u.has_update() && u.errors.is_empty(), "{u:?}");
    assert!(!u.checked_at.is_empty());

    h.work.write("a.txt", "one\ntwo\nthree\nfour\n");
    let b = h.work.commit("four");
    push_all(&h);
    let u = check(&h, id).await;
    assert!(u.ref_moved);
    assert_eq!(
        u.reasons,
        vec![format!("master moved ({} → {})", &a[..7], &b[..7])],
        "the pool holds neither commit: no count, and nothing is fetched to get one"
    );
    assert!(
        !Pool::exists(&h.builds_dir()),
        "the check created no pool, let alone fetched into one"
    );

    // The same badge over the ops, from the stored facts — no new check.
    let gw = common::serve(h.state.clone()).await;
    let listed = op(&gw, "builds", json!({})).await;
    assert_eq!(
        listed["builds"][0]["update"]["reasons"][0],
        u.reasons[0].as_str()
    );
    assert_eq!(listed["builds"][0]["update"]["ref_moved"], true);

    // Once the pool holds both commits (a run fetched them), the count is
    // cheap and the badge says it.
    let pool = Pool::open(Git::new(), &h.builds_dir()).await.unwrap();
    pool.resolve_and_fetch(
        &h.upstream.url(),
        &FetchTarget::Ref("master".into()),
        None,
        Forge::Plain,
        None,
        None,
    )
    .await
    .unwrap();
    let u = check(&h, id).await;
    assert_eq!(u.reasons, vec!["master +1 commit"]);

    // A run that builds the new head clears the badge at once, from the same
    // stored facts.
    seed_verified(&h, id, &b, vec![]).await;
    let got = op(&gw, "build_get", json!({"id": id})).await;
    assert_eq!(got["view"]["update"]["reasons"], json!([]), "{got}");

    // And an edit shows at once too.
    let mut edited = spec.clone();
    edited.build_args = "GGML_CUDA_FA_ALL_QUANTS=OFF".into();
    edited.notes = "notes are not the image".into();
    store::update_build(&h.state.db, id, &edited).await.unwrap();
    let build = store::get_build(&h.state.db, id).await.unwrap().unwrap();
    let u = updates::status_for(&h.state, &build).await.unwrap();
    assert_eq!(
        u.reasons,
        vec!["definition changed since last run (build args)"]
    );
    assert!(!u.ref_moved);
}

#[tokio::test]
async fn a_moved_tag_is_peeled_and_a_commit_ref_never_updates() {
    let h = Harness::new().await;
    let a = h.base_sha();
    h.work.git(&["tag", "-a", "v1", "-m", "v1"]);
    h.work.push(&h.upstream, "refs/tags/*:refs/tags/*");

    let tagged = h
        .build(&BuildSpec {
            slug: "official-v1".into(),
            git_ref: "v1".into(),
            ..h.spec()
        })
        .await;
    let pinned = h
        .build(&BuildSpec {
            slug: "official-at".into(),
            git_ref: a.clone(),
            ..h.spec()
        })
        .await;
    seed_verified(&h, tagged, &a, vec![]).await;
    seed_verified(&h, pinned, &a, vec![]).await;
    let u = check(&h, tagged).await;
    assert!(
        !u.has_update(),
        "the annotated tag is compared by the commit it names: {u:?}"
    );

    h.work.write("b.txt", "b\n");
    let b = h.work.commit("b");
    h.work.git(&["tag", "-f", "-a", "v1", "-m", "v1 moved"]);
    push_all(&h);
    h.work.push(&h.upstream, "refs/tags/*:refs/tags/*");

    let resp = updates::check_all(&h.state).await.unwrap();
    let of = |id: i64| {
        resp.updates
            .iter()
            .find(|e| e.build_id == id)
            .and_then(|e| e.update.clone())
            .unwrap()
    };
    let u = of(tagged);
    assert_eq!(
        u.reasons,
        vec![format!("tag v1 moved ({} → {})", &a[..7], &b[..7])]
    );
    assert!(u.ref_moved);
    assert!(!of(pinned).has_update(), "a commit ref never updates");
    assert!(
        updates::summary(&h.state).await.checked_at.is_some(),
        "a full check records when it ended"
    );
}

#[tokio::test]
async fn ref_extras_move_pinned_ones_do_not_and_a_vanished_one_is_an_error() {
    let h = Harness::new().await;
    let a = h.base_sha();
    h.work.git(&["branch", "feature"]);
    h.work.git(&["branch", "other"]);
    push_all(&h);
    let url = h.upstream.url();
    let spec = BuildSpec {
        slug: "official-extras".into(),
        extras: vec![
            BuildExtra::Ref {
                remote_url: url.clone(),
                git_ref: "feature".into(),
                pin: None,
            },
            BuildExtra::Ref {
                remote_url: url.clone(),
                git_ref: "other".into(),
                pin: Some(a.clone()),
            },
        ],
        ..h.spec()
    };
    let id = h.build(&spec).await;
    let resolved = spec
        .extras
        .iter()
        .map(|e| ResolvedExtra {
            extra: e.clone(),
            sha: a.clone(),
        })
        .collect();
    seed_verified(&h, id, &a, resolved).await;
    assert!(!check(&h, id).await.has_update());

    h.work.git(&["checkout", "--quiet", "feature"]);
    h.work.write("f.txt", "feature\n");
    let f = h.work.commit("feature work");
    h.work.git(&["checkout", "--quiet", "other"]);
    h.work.write("o.txt", "other\n");
    h.work.commit("other work");
    h.work.git(&["checkout", "--quiet", "master"]);
    push_all(&h);
    let u = check(&h, id).await;
    assert!(!u.ref_moved, "master did not move");
    assert_eq!(
        u.reasons,
        vec![format!("{url} feature moved ({} → {})", &a[..7], &f[..7])],
        "the pinned extra is not reported"
    );
    assert_eq!(u.extras.len(), 1);
    assert_eq!(u.extras[0].label, format!("{url} feature"));

    h.work
        .git(&["push", "--quiet", &url, ":refs/heads/feature"]);
    let u = check(&h, id).await;
    assert!(!u.has_update());
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(
        u.errors[0].starts_with("check failed: ") && u.errors[0].contains("no longer exists"),
        "{}",
        u.errors[0]
    );
}

#[tokio::test]
async fn an_unreachable_remote_is_an_error_not_an_update() {
    let h = Harness::new().await;
    let spec = BuildSpec {
        repo_url: format!("file://{}", h.root.join("nowhere.git").display()),
        ..h.spec()
    };
    let id = h.build(&spec).await;
    seed_verified(&h, id, &h.base_sha(), vec![]).await;
    let u = check(&h, id).await;
    assert!(!u.has_update());
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(u.errors[0].contains("git ls-remote"), "{}", u.errors[0]);
}

// ---------------------------------------------------------------------------
// Forges
// ---------------------------------------------------------------------------

const GH_REPO: &str = "https://github.com/o/r";

fn gh_pull(number: u64, state: &str, head: char, merged: bool) -> Value {
    json!({
        "number": number,
        "title": format!("PR {number}"),
        "user": {"login": "someone"},
        "updated_at": "2026-09-25T10:00:00Z",
        "draft": false,
        "state": state,
        "head": {"sha": sha(head)},
        "base": {"sha": sha('0')},
        "merged_at": if merged { json!("2026-09-25T09:00:00Z") } else { Value::Null },
        "html_url": format!("{GH_REPO}/pull/{number}"),
    })
}

/// A build of `repo_url` on a commit (so no remote is `ls-remote`d) with
/// these PR extras, and a verified run that merged `merged` at their SHAs.
async fn pr_build(
    h: &Harness,
    slug: &str,
    repo_url: &str,
    forge: Forge,
    numbers: &[u64],
    merged: &[(u64, char)],
) -> i64 {
    let spec = BuildSpec {
        slug: slug.into(),
        repo_url: repo_url.into(),
        forge,
        git_ref: sha('a'),
        extras: numbers
            .iter()
            .map(|n| BuildExtra::Pr {
                number: *n,
                pin: None,
            })
            .collect(),
        ..h.spec()
    };
    let id = h.build(&spec).await;
    let resolved = merged
        .iter()
        .map(|(n, c)| ResolvedExtra {
            extra: BuildExtra::Pr {
                number: *n,
                pin: None,
            },
            sha: sha(*c),
        })
        .collect();
    seed_verified(h, id, &sha('a'), resolved).await;
    id
}

fn install_github(h: &Harness, gh: &MockServer) {
    h.state
        .builds
        .set_forge_client(ForgeClient::new().unwrap().with_github_api(gh.uri()));
    h.state
        .builds
        .set_forge(Arc::new(GatewayForge::new(&h.state)));
}

#[tokio::test]
async fn github_pr_states_become_the_spec_badges() {
    let h = Harness::new().await;
    let gh = MockServer::start().await;
    install_github(&h, &gh);
    for (n, body) in [
        (1, gh_pull(1, "open", 'f', false)),
        (2, gh_pull(2, "closed", 'e', true)),
        (3, gh_pull(3, "closed", 'd', false)),
        (4, gh_pull(4, "open", 'c', false)),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/pulls/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&gh)
            .await;
    }
    // PR 2 was merged upstream before the run (skipped, no SHA); 1, 3 and 4
    // were merged in at b, d and c.
    let id = pr_build(
        &h,
        "official-prs",
        GH_REPO,
        Forge::Github,
        &[1, 2, 3, 4],
        &[(1, 'b'), (3, 'd'), (4, 'c')],
    )
    .await;
    let u = check(&h, id).await;
    // The build's base is a bare commit ref (`sha('a')`) and the pool never
    // fetched anything: PR #2's merge/head commit is not something the pool
    // can prove is (or isn't) in the base, so the badge is non-committal.
    let merged_noncommittal = "merged upstream at 2026-09-25T09:00:00Z; your base doesn't \
                                contain it yet — it's still merged into your build";
    assert_eq!(
        u.reasons,
        vec![
            "PR #1 pushed".to_string(),
            format!("PR #2 {merged_noncommittal}"),
            "PR #3 closed unmerged".to_string(),
        ]
    );
    let changes: Vec<&str> = u.extras.iter().map(|e| e.change.as_str()).collect();
    assert_eq!(changes, ["pushed", merged_noncommittal, "closed unmerged"]);
    assert!(!u.ref_moved && u.errors.is_empty());
    assert_eq!(
        gh.received_requests().await.unwrap().len(),
        4,
        "one forge call per unpinned PR"
    );
}

#[tokio::test]
async fn a_rate_limit_is_recorded_with_its_reset_and_the_forge_is_not_asked_again() {
    let h = Harness::new().await;
    let gh = MockServer::start().await;
    install_github(&h, &gh);
    let reset = chrono::Utc::now().timestamp() + 23 * 60;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-limit", "60")
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                .set_body_json(json!({"message": "API rate limit exceeded for 203.0.113.7."})),
        )
        .mount(&gh)
        .await;
    let first = pr_build(&h, "first", GH_REPO, Forge::Github, &[1], &[(1, 'b')]).await;
    let second = pr_build(
        &h,
        "second",
        "https://github.com/o/other",
        Forge::Github,
        &[9],
        &[(9, 'b')],
    )
    .await;

    let resp = updates::check_all(&h.state).await.unwrap();
    for id in [first, second] {
        let u = resp
            .updates
            .iter()
            .find(|e| e.build_id == id)
            .and_then(|e| e.update.clone())
            .unwrap();
        assert!(!u.has_update(), "a rate limit is not an update: {u:?}");
        assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
        let e = &u.errors[0];
        assert!(e.starts_with("check failed: "), "{e}");
        assert!(e.contains("GitHub API rate limit reached"), "{e}");
        assert!(e.contains("resets at") && e.contains("(in 23 min)"), "{e}");
    }
    assert_eq!(
        gh.received_requests().await.unwrap().len(),
        1,
        "after the quota ran out the check asked github.com nothing more"
    );
    assert_eq!(updates::summary(&h.state).await.builds_with_updates, 0);
}

#[tokio::test]
async fn a_gitlab_mr_merged_upstream_is_badged() {
    let h = Harness::new().await;
    let gl = MockServer::start().await;
    h.state.builds.set_forge_client(ForgeClient::new().unwrap());
    h.state
        .builds
        .set_forge(Arc::new(GatewayForge::new(&h.state)));
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/group%2Fproj/merge_requests/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "iid": 5,
            "title": "a feature",
            "author": {"username": "me"},
            "updated_at": "2026-09-25T08:00:00.000Z",
            "state": "merged",
            "sha": sha('e'),
            "merged_at": "2026-09-25T08:00:00.000Z",
            "web_url": format!("{}/group/proj/-/merge_requests/5", gl.uri()),
        })))
        .mount(&gl)
        .await;
    let repo = format!("{}/group/proj", gl.uri());
    let id = pr_build(&h, "gitlab-mr", &repo, Forge::Gitlab, &[5], &[(5, 'b')]).await;
    let u = check(&h, id).await;
    assert_eq!(
        u.reasons,
        vec![
            "PR #5 merged upstream at 2026-09-25T08:00:00.000Z; your base doesn't contain it \
             yet — it's still merged into your build"
        ],
        "no merge_commit_sha from this GitLab response and no pool: non-committal"
    );
}

// ---------------------------------------------------------------------------
// Registries
// ---------------------------------------------------------------------------

const INDEX_MT: &str = "application/vnd.oci.image.index.v1+json";

/// A registry behind the anonymous bearer flow: `HEAD`/`GET` of any of
/// `repo`'s manifests without the token → 401 with a challenge naming
/// `realm_path` on the same server.
async fn challenge(reg: &MockServer, repo: &str, realm_path: &str, service: &str) {
    Mock::given(path_regex(format!(
        "^/v2/{}/manifests/",
        regex_escape(repo)
    )))
    .respond_with(
        ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(
                "Bearer realm=\"{}{realm_path}\",service=\"{service}\",\
                 scope=\"repository:{repo}:pull\"",
                reg.uri()
            )
            .as_str(),
        ),
    )
    .with_priority(10)
    .mount(reg)
    .await;
    Mock::given(method("GET"))
        .and(path(realm_path))
        .and(query_param("scope", format!("repository:{repo}:pull")))
        .and(query_param("service", service))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "anon"})))
        .mount(reg)
        .await;
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_' {
                c.to_string()
            } else {
                format!("\\{c}")
            }
        })
        .collect()
}

/// `repo:tag` behind the token: an index of `members`, digest `index`.
async fn serve_index(reg: &MockServer, repo: &str, tag: &str, index: char, members: &[char]) {
    let body = json!({
        "schemaVersion": 2,
        "mediaType": INDEX_MT,
        "manifests": members.iter().map(|m| json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest(*m),
            "size": 1234,
            "platform": {"architecture": "amd64", "os": "linux"},
        })).collect::<Vec<_>>(),
    });
    for m in ["HEAD", "GET"] {
        Mock::given(method(m))
            .and(path(format!("/v2/{repo}/manifests/{tag}")))
            .and(header("authorization", "Bearer anon"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("docker-content-digest", digest(index).as_str())
                    .insert_header("content-type", INDEX_MT)
                    .set_body_json(body.clone()),
            )
            .with_priority(1)
            .mount(reg)
            .await;
    }
}

async fn gets(reg: &MockServer, of: &str) -> usize {
    reg.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path().contains(of))
        .count()
}

#[tokio::test]
async fn ghcr_digests_compare_by_index_or_by_its_member_manifest() {
    let reg = MockServer::start().await;
    challenge(&reg, "o/r", "/token", "ghcr.io").await;
    serve_index(&reg, "o/r", "tag", 'i', &['m', 'n']).await;
    let client = RegistryClient::new()
        .unwrap()
        .with_endpoint("ghcr.io", reg.uri());
    let image = ImageRef::parse("ghcr.io/o/r:tag").unwrap();

    // podman recorded the index digest: equal on the HEAD alone.
    let m = client
        .remote_manifest(&image, &[digest('i')])
        .await
        .unwrap();
    assert_eq!(m.digest, digest('i'));
    assert!(m.matches(&[digest('i')]));
    assert_eq!(gets(&reg, "/manifests/").await, 0, "no manifest read");

    // Only the platform manifest's digest: the index is read, and its member
    // matches — not an update.
    let m = client
        .remote_manifest(&image, &[digest('m')])
        .await
        .unwrap();
    assert_eq!(m.members, vec![digest('m'), digest('n')]);
    assert!(m.matches(&[digest('m')]));

    // Neither: an update.
    let m = client
        .remote_manifest(&image, &[digest('o')])
        .await
        .unwrap();
    assert!(!m.matches(&[digest('o')]));

    // The token was asked for anonymously, and sent only to the registry.
    let reqs = reg.received_requests().await.unwrap();
    let token_reqs: Vec<_> = reqs.iter().filter(|r| r.url.path() == "/token").collect();
    assert!(!token_reqs.is_empty());
    assert!(token_reqs
        .iter()
        .all(|r| !r.headers.contains_key("authorization")));
}

#[tokio::test]
async fn docker_hub_names_qualify_and_a_missing_digest_header_is_the_body_hash() {
    let reg = MockServer::start().await;
    challenge(&reg, "library/busybox", "/auth/token", "registry.docker.io").await;
    let body = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"digest": digest('c')},
        "layers": [],
    });
    let bytes = serde_json::to_vec(&body).unwrap();
    for m in ["HEAD", "GET"] {
        Mock::given(method(m))
            .and(path("/v2/library/busybox/manifests/tag"))
            .and(header("authorization", "Bearer anon"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .with_priority(1)
            .mount(&reg)
            .await;
    }
    let client = RegistryClient::new()
        .unwrap()
        .with_endpoint("docker.io", reg.uri());
    let image = ImageRef::parse("busybox:tag").unwrap();
    assert_eq!(image.reference(), "docker.io/library/busybox:tag");
    let m = client.remote_manifest(&image, &[]).await.unwrap();
    assert_eq!(
        m.digest,
        format!("sha256:{}", hex::encode(Sha256::digest(&bytes)))
    );
    assert!(m.members.is_empty(), "a single-platform manifest has none");
}

#[tokio::test]
async fn registry_refusals_are_sentences() {
    let reg = MockServer::start().await;
    Mock::given(path("/v2/o/private/manifests/tag"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("www-authenticate", "Basic realm=\"reg\""),
        )
        .mount(&reg)
        .await;
    Mock::given(path("/v2/o/gone/manifests/tag"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&reg)
        .await;
    Mock::given(path("/v2/o/busy/manifests/tag"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
        .mount(&reg)
        .await;
    let client = RegistryClient::new()
        .unwrap()
        .with_endpoint("ghcr.io", reg.uri());
    let err = |r: &str| {
        let client = client.clone();
        let image = ImageRef::parse(r).unwrap();
        async move { client.remote_manifest(&image, &[]).await.unwrap_err() }
    };
    assert!(err("ghcr.io/o/private:tag")
        .await
        .contains("public images only"));
    assert!(err("ghcr.io/o/gone:tag")
        .await
        .contains("no such tag (404)"));
    let e = err("ghcr.io/o/busy:tag").await;
    assert!(e.contains("rate-limited") && e.contains("60 s"), "{e}");
}

const AUDIO: &str = "ghcr.io/0xshug0/audio.cpp:full-cuda12";

#[tokio::test]
async fn a_registry_image_in_use_is_badged_and_pull_update_clears_it() {
    let h = Harness::new().await;
    let reg = MockServer::start().await;
    challenge(&reg, "0xshug0/audio.cpp", "/token", "ghcr.io").await;
    serve_index(&reg, "0xshug0/audio.cpp", "full-cuda12", 'n', &['p']).await;
    h.state.builds.set_registry_client(
        RegistryClient::new()
            .unwrap()
            .with_endpoint("ghcr.io", reg.uri()),
    );
    let old = "1".repeat(64);
    let new = "2".repeat(64);
    let repo_digest = |c: char| format!("ghcr.io/0xshug0/audio.cpp@{}", digest(c));
    h.podman
        .add_pulled(&old, &[AUDIO], &[&repo_digest('o'), &repo_digest('q')]);
    // A localhost image in use is never asked about.
    h.podman.add_image(
        &"3".repeat(64),
        &["localhost/llama-server-cuda:official-latest"],
        &[],
    );
    h.settings(|s| {
        s.audio.image = AUDIO.into();
        s.router.image = "localhost/llama-server-cuda:official-latest".into();
    })
    .await;
    let mut events = h.state.telemetry.subscribe();

    let gw = common::serve(h.state.clone()).await;
    let checked = op(&gw, "build_updates_check", json!({})).await;
    assert_eq!(checked["updates"], json!([]), "no builds");
    let summary = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an updates frame")
        {
            Ok(Event::Updates(s)) => break s,
            _ => continue,
        }
    };
    assert_eq!(
        summary,
        UpdatesSummary {
            builds_with_updates: 0,
            images_with_updates: 1,
            checked_at: summary.checked_at.clone(),
        }
    );
    assert!(summary.checked_at.is_some());

    let images = op(&gw, "container_images", json!({})).await;
    let audio = images["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == old.as_str())
        .unwrap()
        .clone();
    let ru = &audio["registry_update"];
    assert_eq!(ru["reference"], AUDIO);
    assert_eq!(ru["remote_digest"], digest('n').as_str());
    assert_eq!(ru["local_digests"], json!([digest('o'), digest('q')]));
    assert_eq!(ru["update_available"], true);
    assert!(ru["error"].is_null());
    let local = images["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "3".repeat(64).as_str())
        .unwrap();
    assert!(local["registry_update"].is_null());

    // Pull update.
    assert!(op_err(
        &gw,
        "container_image_pull",
        json!({"image": "localhost/x:y"})
    )
    .await
    .contains("built on this machine"));
    h.podman
        .world()
        .remote
        .insert(AUDIO.into(), (new.clone(), vec![repo_digest('n')]));
    let started = op(&gw, "container_image_pull", json!({"image": AUDIO})).await;
    let row = wait_job(&h, started["job_id"].as_i64().unwrap()).await;
    assert_eq!(row.kind, "image_pull");
    assert_eq!(row.key.as_deref(), Some(format!("image:{AUDIO}").as_str()));
    assert_eq!(row.status, "done", "{:?}", row.error);
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["updated"], true);
    // The page reads the same result back through the status op.
    let st = op(
        &gw,
        "container_image_pull_status",
        json!({"job_id": started["job_id"]}),
    )
    .await;
    assert_eq!(st["status"], "done");
    assert_eq!(st["image"], AUDIO);
    assert_eq!(st["result"], result);
    assert!(st["error"].is_null());
    assert_eq!(result["old_id"], old.as_str());
    assert_eq!(result["new_id"], new.as_str());
    assert_eq!(result["new_digests"], json!([repo_digest('n')]));
    assert_eq!(result["used_by"][0]["kind"], "class_default");
    assert_eq!(result["used_by"][0]["class"], "audio");
    assert!(result["log"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("Writing manifest")));
    assert!(h
        .podman
        .calls_of("pull")
        .iter()
        .any(|c| c == &["pull", "--", AUDIO]));

    // The badge went without asking the registry again.
    let before = reg.received_requests().await.unwrap().len();
    let images = op(&gw, "container_images", json!({})).await;
    let pulled = images["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == new.as_str())
        .unwrap()
        .clone();
    assert_eq!(pulled["registry_update"]["update_available"], false);
    assert_eq!(updates::summary(&h.state).await.images_with_updates, 0);
    assert_eq!(reg.received_requests().await.unwrap().len(), before);

    // A pull that fails says what podman said.
    let started = op(
        &gw,
        "container_image_pull",
        json!({"image": "ghcr.io/o/nope:x"}),
    )
    .await;
    let row = wait_job(&h, started["job_id"].as_i64().unwrap()).await;
    assert_eq!(row.status, "failed");
    assert!(row.error.unwrap().contains("manifest unknown"));
    let st = op(
        &gw,
        "container_image_pull_status",
        json!({"job_id": started["job_id"]}),
    )
    .await;
    assert_eq!(st["status"], "failed");
    assert!(st["result"].is_null());
    assert!(st["error"].as_str().unwrap().contains("manifest unknown"));
    // A job that is not there says so.
    assert!(op_err(
        &gw,
        "container_image_pull_status",
        json!({"job_id": 999_999})
    )
    .await
    .contains("no job with id"));
}

// ---------------------------------------------------------------------------
// Remotes that stall, and tokens that must not travel
// ---------------------------------------------------------------------------

/// A git remote that accepts connections and never answers: `ls-remote`
/// against it hangs until killed. It used to wedge the check (and Check now)
/// for as long as it hung.
#[tokio::test]
async fn a_stalled_remote_times_out_and_a_token_is_not_sent_over_plain_http() {
    let h = Harness::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _)) = listener.accept().await {
            open.push(conn);
        }
    });
    let stalled = format!("http://127.0.0.1:{port}/g/p.git");
    let plain = "http://git.example/g/p.git";
    h.settings(|s| {
        s.forge_tokens
            .insert("git.example".into(), "glpat-x".into());
    })
    .await;
    let mut spec = h.spec();
    spec.extras = vec![
        BuildExtra::Ref {
            remote_url: stalled.clone(),
            git_ref: "feature".into(),
            pin: None,
        },
        BuildExtra::Ref {
            remote_url: plain.into(),
            git_ref: "feature".into(),
            pin: None,
        },
    ];
    let id = h.build(&spec).await;
    let recorded = spec
        .extras
        .iter()
        .map(|e| ResolvedExtra {
            extra: e.clone(),
            sha: sha('c'),
        })
        .collect();
    seed_verified(&h, id, &h.base_sha(), recorded).await;
    h.state
        .builds
        .updates()
        .set_remote_timeout_for_tests(Duration::from_millis(300));

    let started = std::time::Instant::now();
    let u = check(&h, id).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the check waited {:?}",
        started.elapsed()
    );
    assert!(!u.has_update(), "{u:?}");
    assert!(
        u.errors.contains(&format!(
            "check failed: {stalled} timed out after 300ms (git ls-remote)"
        )),
        "{:?}",
        u.errors
    );
    assert!(
        u.errors.contains(&format!(
            "check failed: {plain}: refusing to send the forge token over plain http to \
             git.example; use https"
        )),
        "{:?}",
        u.errors
    );
    // The base remote answered all the same: its ref was compared.
    let stored = h.state.builds.updates().loaded(&h.state).await;
    assert!(stored.builds[&id]
        .refs
        .iter()
        .any(|r| r.name == "master" && matches!(r.seen, updates::Seen::Branch { .. })));
    held.abort();
}

// ---------------------------------------------------------------------------
// The schedule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_scheduler_honours_zero_and_runs_when_turned_on() {
    let h = Harness::new().await;
    h.settings(|s| s.build_update_check_hours = 0).await;
    let task = tokio::spawn(updates::run_scheduler(
        h.state.clone(),
        Cadence {
            boot_delay: Duration::from_millis(5),
            tick: Duration::from_millis(20),
        },
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        updates::summary(&h.state).await.checked_at,
        None,
        "0 is off: no check ran"
    );

    h.settings(|s| s.build_update_check_hours = 6).await;
    let mut checked = None;
    for _ in 0..500 {
        checked = updates::summary(&h.state).await.checked_at;
        if checked.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(checked.is_some(), "turned on, the first check ran");

    // Persisted: a restarted gateway would read when it last ran.
    let raw = store::get_kv(&h.state.db, updates::KV_KEY)
        .await
        .unwrap()
        .unwrap();
    let stored: updates::Stored = serde_json::from_str(&raw).unwrap();
    assert_eq!(stored.last_all, checked);

    // Not due again for six hours: the loop keeps waking without checking.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(updates::summary(&h.state).await.checked_at, checked);
    task.abort();
}
