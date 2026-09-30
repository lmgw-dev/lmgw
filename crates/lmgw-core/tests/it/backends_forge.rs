//! The forge clients of the Backends extras picker (container-builds design
//! §7, §10, §15 `forge_prs` / `forge_pr` / `forge_refs`) against mock GitHub
//! and GitLab APIs: paging, the client-side query filter, direct resolution
//! by number or pasted URL, the PR mapping with its state normalization, the
//! rate-limit sentence, and — the security half — that a token reaches its
//! own host and nothing else.
//!
//! GitHub's API base is pointed at a `wiremock` server; a GitLab repository
//! simply lives on one (`http://127.0.0.1:<port>/group/project`), since its
//! API is on its own host. Nothing here talks to a real forge.

use std::collections::BTreeMap;
use std::path::Path;

use lmgw_core::backends::forge::{self, ForgeClient, ForgeRepo, ForgeSession};
use lmgw_core::backends::Forge;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "https://github.com/ggml-org/llama.cpp";
const PULLS: &str = "/repos/ggml-org/llama.cpp/pulls";

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 40).collect()
}

fn gh_pull(number: u64, title: &str, login: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "user": {"login": login},
        "updated_at": "2026-09-25T10:00:00Z",
        "draft": false,
        "state": "open",
        "head": {"sha": sha('a'), "ref": "feature"},
        "base": {"sha": sha('b'), "ref": "master"},
        "merged_at": null,
        "html_url": format!("https://github.com/ggml-org/llama.cpp/pull/{number}"),
    })
}

fn gl_mr(iid: u64, title: &str, username: &str, state: &str) -> Value {
    json!({
        "iid": iid,
        "id": 90_000 + iid,
        "title": title,
        "author": {"username": username},
        "updated_at": "2026-09-24T08:30:00.000Z",
        "draft": false,
        "state": state,
        "sha": sha('c'),
        "merged_at": null,
        "web_url": format!("https://git.example/group/sub/proj/-/merge_requests/{iid}"),
    })
}

fn client(gh: &MockServer) -> ForgeClient {
    ForgeClient::new().unwrap().with_github_api(gh.uri())
}

/// `http://127.0.0.1:<port>` → `127.0.0.1:<port>`: the `forge_tokens` key of
/// a repository on that mock.
fn host_of(server: &MockServer) -> String {
    server.uri().trim_start_matches("http://").to_string()
}

fn auth_of(r: &Request) -> Option<String> {
    r.headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string())
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GitHub listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn github_lists_open_prs_newest_first_paged_by_link() {
    let gh = MockServer::start().await;
    let reset = chrono::Utc::now().timestamp() + 3600;
    let next = format!(
        "<{0}/repositories/612354784/pulls?state=open&page=2>; rel=\"next\", \
         <{0}/repositories/612354784/pulls?state=open&page=7>; rel=\"last\"",
        gh.uri()
    );
    Mock::given(method("GET"))
        .and(path(PULLS))
        .and(query_param("state", "open"))
        .and(query_param("sort", "updated"))
        .and(query_param("direction", "desc"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", next.as_str())
                .insert_header("x-ratelimit-limit", "60")
                .insert_header("x-ratelimit-remaining", "58")
                .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                .set_body_json(json!([
                    gh_pull(16500, "model : add Gemma 4 vision", "ngxson"),
                    gh_pull(
                        16391,
                        "CUDA: faster FA for small batches",
                        "JohannesGaessler"
                    ),
                ])),
        )
        .expect(1)
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([gh_pull(15000, "old one", "someone")])),
        )
        .expect(1)
        .mount(&gh)
        .await;

    let c = client(&gh);
    let first = c
        .list_prs(REPO, Forge::Github, None, None, None)
        .await
        .unwrap();
    assert_eq!(first.next_page, Some(2));
    assert_eq!(
        first.prs.iter().map(|p| p.number).collect::<Vec<_>>(),
        [16500, 16391]
    );
    let pr = &first.prs[0];
    assert_eq!(pr.title, "model : add Gemma 4 vision");
    assert_eq!(pr.author, "ngxson");
    assert_eq!(pr.state, "open");
    assert_eq!(pr.head_sha, sha('a'));
    assert_eq!(pr.base_sha, sha('b'));
    assert_eq!(pr.updated_at, "2026-09-25T10:00:00Z");
    assert_eq!(pr.url, "https://github.com/ggml-org/llama.cpp/pull/16500");
    assert!(!pr.draft && pr.merged_at.is_none());
    let rl = first.rate_limit.expect("GitHub sends its quota");
    assert_eq!(rl.remaining, 58);
    assert_eq!(
        rl.reset_at,
        chrono::DateTime::from_timestamp(reset, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );

    let second = c
        .list_prs(REPO, Forge::Github, None, Some(2), None)
        .await
        .unwrap();
    assert_eq!(second.prs.len(), 1);
    assert_eq!(second.next_page, None, "no Link rel=next on the last page");
    assert!(second.rate_limit.is_none(), "no quota headers, no quota");

    for r in requests(&gh).await {
        let ua = r.headers.get("user-agent").unwrap().to_str().unwrap();
        assert!(ua.starts_with("lmgw/"), "{ua}");
        assert_eq!(
            r.headers.get("accept").unwrap().to_str().unwrap(),
            "application/vnd.github+json"
        );
        assert_eq!(auth_of(&r), None, "no token configured, none sent");
    }
}

#[tokio::test]
async fn github_query_filters_the_page_client_side() {
    let gh = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    format!("<{}/x/pulls?page=2>; rel=\"next\"", gh.uri()).as_str(),
                )
                .set_body_json(json!([
                    gh_pull(3, "model : add Gemma 4 vision", "ngxson"),
                    gh_pull(2, "CUDA: faster FA", "JohannesGaessler"),
                    gh_pull(1, "server: fix gemma template", "ggerganov"),
                ])),
        )
        .mount(&gh)
        .await;
    let c = client(&gh);
    let numbers = |q: &'static str| {
        let c = c.clone();
        async move {
            let page = c
                .list_prs(REPO, Forge::Github, Some(q), None, None)
                .await
                .unwrap();
            (
                page.prs.iter().map(|p| p.number).collect::<Vec<_>>(),
                page.next_page,
            )
        }
    };
    assert_eq!(numbers("GEMMA").await, (vec![3, 1], Some(2)));
    assert_eq!(numbers("gemma ggerganov").await, (vec![1], Some(2)));
    assert_eq!(numbers("johannes").await, (vec![2], Some(2)));
    assert_eq!(numbers("  ").await.0, vec![3, 2, 1], "blank = no filter");
    assert_eq!(
        numbers("nothing matches").await,
        (vec![], Some(2)),
        "an empty filtered page still offers the next hundred"
    );
    // One request per page, never a detail call per PR (60 requests/h
    // unauthenticated).
    for r in requests(&gh).await {
        assert_eq!(r.url.path(), PULLS);
    }
}

#[tokio::test]
async fn numbers_and_pasted_urls_resolve_directly() {
    let gh = MockServer::start().await;
    let mut merged = gh_pull(16391, "CUDA: faster FA", "JohannesGaessler");
    merged["state"] = json!("closed");
    merged["merged_at"] = json!("2026-09-20T12:00:00Z");
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/16391")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-remaining", "41")
                .insert_header("x-ratelimit-reset", "1790000000")
                .set_body_json(merged),
        )
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&gh)
        .await;

    let c = client(&gh);
    for (repo, q) in [
        (REPO, "16391"),
        (REPO, "#16391"),
        (
            REPO,
            " https://github.com/ggml-org/llama.cpp/pull/16391/files ",
        ),
        (
            REPO,
            "https://github.com/GGML-org/Llama.cpp/pull/16391#issuecomment-1",
        ),
        (
            "git@github.com:ggml-org/llama.cpp.git",
            "https://github.com/ggml-org/llama.cpp/pull/16391",
        ),
    ] {
        let page = c
            .list_prs(repo, Forge::Github, Some(q), Some(3), None)
            .await
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(page.prs.len(), 1, "{q}");
        assert_eq!(page.prs[0].number, 16391);
        assert_eq!(page.prs[0].state, "merged", "closed + merged_at");
        assert_eq!(page.next_page, None);
        assert_eq!(page.rate_limit.unwrap().remaining, 41);
    }

    let other = c
        .list_prs(
            REPO,
            Forge::Github,
            Some("https://github.com/ikawrakow/ik_llama.cpp/pull/5"),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(other.contains("ikawrakow/ik_llama.cpp"), "{other}");
    assert!(other.contains("ref from another remote"), "{other}");
    assert!(other.contains("refs/pull/5/head"), "{other}");

    let issue = c
        .list_prs(
            REPO,
            Forge::Github,
            Some("https://github.com/ggml-org/llama.cpp/issues/5"),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(issue.contains("not a pull request"), "{issue}");

    let zero = c
        .list_prs(REPO, Forge::Github, Some("#0"), None, None)
        .await
        .unwrap_err();
    assert!(zero.contains("no PR #0"), "{zero}");
}

#[tokio::test]
async fn github_get_pr_maps_fields_and_normalizes_state() {
    let gh = MockServer::start().await;
    let mut open = gh_pull(1, "draft work", "alice");
    open["draft"] = json!(true);
    // An open PR's merge_commit_sha is GitHub's test merge: not kept.
    open["merge_commit_sha"] = json!(sha('9'));
    let mut merged = gh_pull(2, "landed", "bob");
    merged["state"] = json!("closed");
    merged["merged_at"] = json!("2026-09-20T12:00:00Z");
    merged["merge_commit_sha"] = json!(sha('f'));
    let mut closed = gh_pull(3, "abandoned", "carol");
    closed["state"] = json!("closed");
    let mut ghost = gh_pull(4, "author deleted", "x");
    ghost["user"] = Value::Null;
    ghost["draft"] = Value::Null;
    for (n, body) in [(1, open), (2, merged), (3, closed), (4, ghost)] {
        Mock::given(method("GET"))
            .and(path(format!("{PULLS}/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&gh)
            .await;
    }
    let c = client(&gh);
    let get = |n| c.get_pr(REPO, Forge::Github, n, None);

    let pr = get(1).await.unwrap();
    assert_eq!((pr.state.as_str(), pr.draft), ("open", true));
    assert_eq!(pr.author, "alice");
    assert_eq!((pr.head_sha, pr.base_sha), (sha('a'), sha('b')));
    assert_eq!(pr.merge_commit_sha, "");

    let pr = get(2).await.unwrap();
    assert_eq!(pr.state, "merged");
    assert_eq!(pr.merged_at.as_deref(), Some("2026-09-20T12:00:00Z"));
    assert_eq!(pr.merge_commit_sha, sha('f'));

    let pr = get(3).await.unwrap();
    assert_eq!(pr.state, "closed");
    assert_eq!(pr.merged_at, None);

    let pr = get(4).await.unwrap();
    assert_eq!((pr.author.as_str(), pr.draft), ("", false));

    let missing = get(5).await.unwrap_err();
    assert!(
        missing.starts_with("PR #5 of ggml-org/llama.cpp"),
        "{missing}"
    );
    assert!(missing.contains("404"), "{missing}");
    assert!(
        missing.contains("forge token for github.com"),
        "a private repository needs a token: {missing}"
    );
}

// ---------------------------------------------------------------------------
// GitLab
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gitlab_lists_mrs_of_a_nested_group() {
    let gl = MockServer::start().await;
    let repo = format!("{}/group/sub/proj.git", gl.uri());
    let mrs = "/api/v4/projects/group%2Fsub%2Fproj/merge_requests";
    let mut old_wip = gl_mr(12, "WIP: vision tower", "alice", "opened");
    old_wip.as_object_mut().unwrap().remove("draft");
    old_wip["work_in_progress"] = json!(true);
    Mock::given(method("GET"))
        .and(path(mrs))
        .and(query_param("state", "opened"))
        .and(query_param("order_by", "updated_at"))
        .and(query_param("sort", "desc"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .and(query_param_is_missing("search"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-next-page", "2")
                .insert_header("x-page", "1")
                .insert_header("ratelimit-limit", "2000")
                .insert_header("ratelimit-remaining", "1999")
                .insert_header("ratelimit-reset", "1790000000")
                .set_body_json(json!([
                    old_wip,
                    gl_mr(11, "server: add endpoint", "anna", "opened")
                ])),
        )
        .expect(1)
        .mount(&gl)
        .await;
    Mock::given(method("GET"))
        .and(path(mrs))
        .and(query_param("page", "2"))
        .and(query_param("search", "vision tower"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-next-page", "")
                .set_body_json(json!([gl_mr(3, "vision tower", "alice", "opened")])),
        )
        .expect(1)
        .mount(&gl)
        .await;

    let c = ForgeClient::new().unwrap();
    let first = c
        .list_prs(&repo, Forge::Gitlab, None, None, None)
        .await
        .unwrap();
    assert_eq!(first.next_page, Some(2));
    assert_eq!(
        first.rate_limit.unwrap(),
        lmgw_api_types::builds::RateLimit {
            remaining: 1999,
            reset_at: "2026-09-21T14:13:20Z".into()
        }
    );
    let mr = &first.prs[0];
    assert_eq!(mr.number, 12);
    assert_eq!(mr.state, "open", "opened → open");
    assert!(mr.draft, "work_in_progress is the old spelling of draft");
    assert_eq!(mr.author, "alice");
    assert_eq!(mr.head_sha, sha('c'));
    assert_eq!(mr.base_sha, "", "the list endpoint has no diff_refs");
    assert_eq!(
        mr.url,
        "https://git.example/group/sub/proj/-/merge_requests/12"
    );
    assert!(!first.prs[1].draft);

    // GitLab searches server-side; the page is taken as it comes.
    let second = c
        .list_prs(&repo, Forge::Gitlab, Some("vision tower"), Some(2), None)
        .await
        .unwrap();
    assert_eq!(second.prs.len(), 1);
    assert_eq!(second.next_page, None, "an empty x-next-page ends the list");
}

#[tokio::test]
async fn gitlab_get_mr_maps_fields_and_normalizes_state() {
    let gl = MockServer::start().await;
    let repo = format!("{}/group/sub/proj", gl.uri());
    let mut draft = gl_mr(7, "Draft: speculative", "anna", "opened");
    draft["draft"] = json!(true);
    draft["diff_refs"] = json!({"base_sha": sha('d'), "head_sha": sha('c'), "start_sha": sha('e')});
    let mut merged = gl_mr(8, "landed", "anna", "merged");
    merged["merged_at"] = json!("2026-09-22T09:00:00.000Z");
    merged["merge_commit_sha"] = json!(sha('f'));
    let closed = gl_mr(9, "abandoned", "anna", "closed");
    let mut old_merged = gl_mr(10, "merged on an old GitLab", "anna", "merged");
    // A fast-forward squash: no merge commit, only the squash commit.
    old_merged["merge_commit_sha"] = Value::Null;
    old_merged["squash_commit_sha"] = json!(sha('5'));
    for (n, body) in [(7, draft), (8, merged), (9, closed), (10, old_merged)] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v4/projects/group%2Fsub%2Fproj/merge_requests/{n}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&gl)
            .await;
    }
    let c = ForgeClient::new().unwrap();
    let get = |n| c.get_pr(&repo, Forge::Gitlab, n, None);

    let mr = get(7).await.unwrap();
    assert_eq!((mr.state.as_str(), mr.draft), ("open", true));
    assert_eq!((mr.head_sha, mr.base_sha), (sha('c'), sha('d')));
    assert_eq!(mr.updated_at, "2026-09-24T08:30:00.000Z");

    let mr = get(8).await.unwrap();
    assert_eq!(mr.state, "merged");
    assert_eq!(mr.merged_at.as_deref(), Some("2026-09-22T09:00:00.000Z"));
    assert_eq!(mr.merge_commit_sha, sha('f'));

    let mr = get(9).await.unwrap();
    assert_eq!((mr.state.as_str(), mr.merged_at), ("closed", None));

    let mr = get(10).await.unwrap();
    assert_eq!(mr.state, "merged");
    assert_eq!(mr.merge_commit_sha, sha('5'));
    assert!(
        mr.merged_at.is_some(),
        "state merged always comes with merged_at, the executor's signal"
    );

    let missing = get(11).await.unwrap_err();
    assert!(missing.starts_with("MR !11 of group/sub/proj"), "{missing}");
}

#[tokio::test]
async fn plain_repositories_have_no_pr_list() {
    let c = ForgeClient::new().unwrap();
    let err = c
        .list_prs("https://git.example/g/p", Forge::Plain, None, None, None)
        .await
        .unwrap_err();
    assert!(err.contains("plain git repository"), "{err}");
    let err = c
        .get_pr("https://git.example/g/p.git", Forge::Plain, 3, None)
        .await
        .unwrap_err();
    assert!(err.contains("plain git repository"), "{err}");
    let err = c
        .list_prs("https://github.com/a/b/c", Forge::Github, None, None, None)
        .await
        .unwrap_err();
    assert!(err.contains("not a GitHub repository"), "{err}");
}

// ---------------------------------------------------------------------------
// Rate limits and refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_exhausted_quota_says_when_it_resets() {
    let gh = MockServer::start().await;
    let reset = chrono::Utc::now().timestamp() + 1800;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-limit", "60")
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                .set_body_json(json!({
                    "message": "API rate limit exceeded for 203.0.113.7.",
                    "documentation_url": "https://docs.github.com/rest/overview/rate-limits"
                })),
        )
        .mount(&gh)
        .await;
    // Secondary limit: 403 with retry-after while quota remains.
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/1")))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "40")
                .insert_header("retry-after", "60")
                .set_body_json(json!({"message": "You have exceeded a secondary rate limit."})),
        )
        .mount(&gh)
        .await;
    // A plain permission refusal is not a rate limit.
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/2")))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "39")
                .set_body_json(json!({"message": "Resource not accessible by integration"})),
        )
        .mount(&gh)
        .await;

    let c = client(&gh);
    let now = chrono::Utc::now();
    let err = c
        .list_prs(REPO, Forge::Github, None, None, None)
        .await
        .unwrap_err();
    assert!(
        err.starts_with("open PRs of ggml-org/llama.cpp: GitHub API rate limit reached"),
        "{err}"
    );
    assert!(err.contains("all 60 requests used"), "{err}");
    assert!(
        err.contains(&forge::reset_phrase(reset, now)),
        "local reset time: {err}"
    );
    assert!(err.contains("(in 30 min)"), "{err}");
    assert!(err.contains("forge token for github.com"), "{err}");

    let err = c.get_pr(REPO, Forge::Github, 1, None).await.unwrap_err();
    assert!(err.contains("rate limit reached"), "{err}");
    assert!(err.contains("(in 1 min)"), "{err}");

    let err = c.get_pr(REPO, Forge::Github, 2, None).await.unwrap_err();
    assert!(!err.contains("rate limit"), "{err}");
    assert!(err.contains("403: Resource not accessible"), "{err}");

    let gl = MockServer::start().await;
    let gl_reset = chrono::Utc::now().timestamp() + 45;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("ratelimit-remaining", "0")
                .insert_header("ratelimit-reset", gl_reset.to_string().as_str())
                .set_body_string("Retry later"),
        )
        .mount(&gl)
        .await;
    let err = c
        .list_prs(
            &format!("{}/g/p", gl.uri()),
            Forge::Gitlab,
            None,
            None,
            Some("glpat-x"),
        )
        .await
        .unwrap_err();
    assert!(err.contains("GitLab API rate limit reached"), "{err}");
    assert!(err.contains("resets at"), "{err}");
    assert!(
        !err.contains("forge token for"),
        "a token was sent, so no token advice: {err}"
    );
}

#[tokio::test]
async fn refusals_are_worded_for_the_owner() {
    let gl = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
        )
        .mount(&gl)
        .await;
    let repo = format!("{}/g/p", gl.uri());
    let c = ForgeClient::new().unwrap();
    let with = c
        .get_pr(&repo, Forge::Gitlab, 4, Some("glpat-verysecret"))
        .await
        .unwrap_err();
    assert!(
        with.contains(&format!(
            "the forge token for {} was rejected",
            host_of(&gl)
        )),
        "{with}"
    );
    assert!(!with.contains("glpat-verysecret"), "never in a message");
    let without = c.get_pr(&repo, Forge::Gitlab, 4, None).await.unwrap_err();
    assert!(without.contains("wants authentication"), "{without}");

    let bad = c
        .get_pr(&repo, Forge::Gitlab, 4, Some("glpat bad\r\nX-Evil: 1"))
        .await
        .unwrap_err();
    assert!(bad.contains("whitespace or a control character"), "{bad}");
    assert!(!bad.contains("X-Evil"), "the token is not echoed: {bad}");
}

// ---------------------------------------------------------------------------
// Token scoping (§7, §10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tokens_go_only_to_their_own_host() {
    let gh = MockServer::start().await;
    let gl = MockServer::start().await;
    let gl_untokened = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([gh_pull(5, "t", "u")])))
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/5")))
        .respond_with(ResponseTemplate::new(200).set_body_json(gh_pull(5, "t", "u")))
        .mount(&gh)
        .await;
    for server in [&gl, &gl_untokened] {
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/g%2Fp/merge_requests"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([gl_mr(1, "t", "u", "opened")])),
            )
            .mount(server)
            .await;
    }

    let tokens = BTreeMap::from([
        ("github.com".to_string(), "ghp_githubsecret".to_string()),
        (host_of(&gl), "glpat-gitlabsecret".to_string()),
    ]);
    let session = ForgeSession::new(client(&gh), tokens.clone());
    assert!(
        !format!("{session:?}").contains("secret"),
        "Debug is redacted"
    );

    session.prs(REPO, Forge::Github, None, None).await.unwrap();
    session.pr(REPO, Forge::Github, 5).await.unwrap();
    // github.com is GitHub whatever the build says; the token follows the
    // host, not the chosen forge.
    session
        .prs(
            "git@github.com:ggml-org/llama.cpp.git",
            Forge::Gitlab,
            None,
            None,
        )
        .await
        .unwrap();
    let gl_repo = format!("{}/g/p.git", gl.uri());
    session
        .prs(&gl_repo, Forge::Gitlab, None, None)
        .await
        .unwrap();
    let other_repo = format!("{}/g/p", gl_untokened.uri());
    session
        .prs(&other_repo, Forge::Gitlab, None, None)
        .await
        .unwrap();

    let gh_seen = requests(&gh).await;
    assert_eq!(gh_seen.len(), 3);
    for r in &gh_seen {
        assert_eq!(auth_of(r).as_deref(), Some("Bearer ghp_githubsecret"));
    }
    let gl_seen = requests(&gl).await;
    assert_eq!(gl_seen.len(), 1);
    assert_eq!(
        auth_of(&gl_seen[0]).as_deref(),
        Some("Bearer glpat-gitlabsecret")
    );
    let other_seen = requests(&gl_untokened).await;
    assert_eq!(other_seen.len(), 1);
    assert_eq!(auth_of(&other_seen[0]), None, "no token for that host");
    for r in gh_seen.iter().chain(&gl_seen).chain(&other_seen) {
        assert!(
            !r.url.as_str().contains("secret"),
            "never in a URL: {}",
            r.url
        );
    }

    assert_eq!(
        forge::token_for(&tokens, "https://GitHub.com/a/b.git"),
        Some("ghp_githubsecret")
    );
    assert_eq!(forge::token_for(&tokens, "file:///srv/git/a"), None);
    assert_eq!(forge::token_for(&tokens, "https://gitlab.com/a/b"), None);
}

#[tokio::test]
async fn redirects_never_carry_a_token_to_another_host() {
    let gh = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    // A renamed repository: GitHub redirects within its own API host.
    Mock::given(method("GET"))
        .and(path("/repos/old-owner/llama.cpp/pulls"))
        .respond_with(
            ResponseTemplate::new(301)
                .insert_header("location", format!("{}{PULLS}?page=1", gh.uri()).as_str()),
        )
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([gh_pull(9, "t", "u")])))
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/evil/llama.cpp/pulls"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/steal", elsewhere.uri()).as_str()),
        )
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&elsewhere)
        .await;

    let c = client(&gh);
    let renamed = c
        .list_prs(
            "https://github.com/old-owner/llama.cpp",
            Forge::Github,
            None,
            None,
            Some("ghp_x"),
        )
        .await
        .unwrap();
    assert_eq!(renamed.prs[0].number, 9, "same-origin redirect followed");

    let err = c
        .list_prs(
            "https://github.com/evil/llama.cpp",
            Forge::Github,
            None,
            None,
            Some("ghp_x"),
        )
        .await
        .unwrap_err();
    assert!(err.contains("302"), "{err}");
    assert!(err.contains("redirecting to"), "{err}");
    assert!(
        requests(&elsewhere).await.is_empty(),
        "the cross-host redirect was not followed at all"
    );
}

// ---------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------

#[test]
fn repository_urls_parse_to_origin_and_path() {
    let ok = [
        (
            "https://github.com/ggml-org/llama.cpp",
            "https",
            "github.com",
            "ggml-org/llama.cpp",
        ),
        (
            "https://github.com/ggml-org/llama.cpp.git/",
            "https",
            "github.com",
            "ggml-org/llama.cpp",
        ),
        (
            "https://GitHub.com/ggml-org/llama.cpp.git",
            "https",
            "github.com",
            "ggml-org/llama.cpp",
        ),
        (
            "git@github.com:ggml-org/llama.cpp.git",
            "https",
            "github.com",
            "ggml-org/llama.cpp",
        ),
        (
            "ssh://git@github.com/ikawrakow/ik_llama.cpp.git",
            "https",
            "github.com",
            "ikawrakow/ik_llama.cpp",
        ),
        (
            "ssh://git@git.example.com:2222/kai/sub/lmgw.git",
            "https",
            "git.example.com",
            "kai/sub/lmgw",
        ),
        (
            "https://git.example.com/kai/sub/deeper/lmgw",
            "https",
            "git.example.com",
            "kai/sub/deeper/lmgw",
        ),
        (
            "http://127.0.0.1:8929/group/proj",
            "http",
            "127.0.0.1:8929",
            "group/proj",
        ),
    ];
    for (url, scheme, host, path) in ok {
        let r = ForgeRepo::parse(url).unwrap_or_else(|e| panic!("{url}: {e}"));
        assert_eq!(
            (r.scheme.as_str(), r.host.as_str(), r.path.as_str()),
            (scheme, host, path),
            "{url}"
        );
    }
    let bad = [
        ("file:///srv/git/llama.cpp", "local repository"),
        ("https://github.com/", "names no repository"),
        (
            "https://git.example.com/g/p/-/merge_requests/3",
            "page inside a project",
        ),
        ("ftp://github.com/o/r", "not a git URL"),
        ("", "cannot be empty"),
        ("https://github.com/o/r%20x", "path segment"),
        ("git@github.com", "not a git@host:path"),
        // Credentials belong in the forge token setting, never in the URL.
        (
            "https://oauth2:hunter2@git.example/g/p.git",
            "carries credentials",
        ),
    ];
    for (url, needle) in bad {
        let e = ForgeRepo::parse(url).unwrap_err();
        assert!(e.contains(needle), "{url}: {e}");
    }
    assert_eq!(
        forge::effective_forge("git@github.com:a/b.git", Forge::Gitlab),
        Forge::Github
    );
    assert_eq!(
        forge::effective_forge("https://git.example.com/a/b", Forge::Gitlab),
        Forge::Gitlab
    );
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

fn fixture_git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "init.defaultBranch=master",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .env("GIT_AUTHOR_DATE", "@1790000000 +0000")
        .env("GIT_COMMITTER_DATE", "@1790000000 +0000")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "fixture git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn refs_come_from_ls_remote_default_branch_first_tags_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture_git(dir, &["init", "--quiet"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    fixture_git(dir, &["add", "-A"]);
    fixture_git(dir, &["commit", "--quiet", "-m", "one"]);
    let one = fixture_git(dir, &["rev-parse", "HEAD"]);
    fixture_git(dir, &["tag", "b699"]);
    fixture_git(dir, &["tag", "v0.5.0"]);
    fixture_git(dir, &["tag", "-a", "b7000", "-m", "annotated"]);
    fixture_git(dir, &["branch", "a-feature"]);
    std::fs::write(dir.join("a"), "2").unwrap();
    fixture_git(dir, &["commit", "--quiet", "-am", "two"]);
    let two = fixture_git(dir, &["rev-parse", "HEAD"]);
    fixture_git(dir, &["tag", "b1000"]);

    let url = format!("file://{}", dir.display());
    let session = ForgeSession::new(ForgeClient::new().unwrap(), BTreeMap::new());
    let refs = session.refs(&url).await.unwrap();
    assert_eq!(refs.default_branch, "master");
    let heads: Vec<_> = refs
        .heads
        .iter()
        .map(|h| (h.name.as_str(), h.sha.as_str()))
        .collect();
    assert_eq!(
        heads,
        [("master", two.as_str()), ("a-feature", one.as_str())]
    );
    let tags: Vec<_> = refs
        .tags
        .iter()
        .map(|t| (t.name.as_str(), t.sha.as_str()))
        .collect();
    assert_eq!(
        tags,
        [
            ("b7000", one.as_str()),
            ("b1000", two.as_str()),
            ("b699", one.as_str()),
            ("v0.5.0", one.as_str()),
        ],
        "numbers first, newest first — not every v… above every b…; the annotated tag \
         peeled to its commit"
    );

    let err = session.refs("https://").await.unwrap_err();
    assert!(err.contains("names no host"), "{err}");
}
