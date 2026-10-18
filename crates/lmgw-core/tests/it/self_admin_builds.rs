//! The text of the build tools, read into their types and written out again:
//! `lmgw__builds` (the list with the repository presets, or one build and its
//! runs), `lmgw__build_log`, `lmgw__forge_prs`, `lmgw__build_set` (with a
//! `next_step` unless the build was deleted), and the two writes that say what
//! to call next, `lmgw__build_run` and `lmgw__container_image_pull`.
//!
//! The text is the answer's pretty JSON with sorted keys, so a key a type
//! adds, drops, renames or turns from absent into `null` shows up as a
//! difference between the text and what the type writes back.

use lmgw_api_types as dto;
use lmgw_api_types::builds::{
    BuildGetResponse, BuildRunStarted, ContainerImagePullStarted, Engine, Forge, RunLogChunk,
};
use lmgw_api_types::self_admin::{BuildList, BuildSet};
use lmgw_core::backends::forge::ForgeClient;
use lmgw_core::state::AppState;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::backends_fake::{wait_run, Harness};
use crate::support::self_admin::{parse, read_back, Admin};

#[tokio::test]
async fn the_build_list_is_the_builds_op_and_the_repository_presets() {
    let h = Harness::new().await;
    h.build(&h.spec()).await;
    let a = Admin::new(&h.state).await;

    let text = a.text("lmgw__builds", json!({})).await;
    let list: BuildList = read_back(&text);
    assert_eq!(list.builds.builds.len(), 1);
    assert_eq!(list.builds.usage_error, None);
    assert!(parse(&text).get("usage_error").is_none(), "{text}");
    let ids: Vec<&str> = list.repo_presets.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["official", "ik", "audio", "sdcpp"]);
    let ik = &list.repo_presets[1];
    assert_eq!(ik.repo_url, "https://github.com/ikawrakow/ik_llama.cpp");
    assert_eq!((ik.engine, ik.forge), (Engine::Llama, Forge::Github));
    assert_eq!(ik.default_ref, "main");
    assert_eq!(list.repo_presets[2].engine, Engine::Audio);
    assert_eq!(list.repo_presets[3].engine, Engine::Sdcpp);
    // The same entry the Builds page's picker gets: the `id` is what
    // `lmgw__build_set preset=` takes.
    assert_eq!(parse(&text)["repo_presets"][0]["id"], "official", "{text}");
    assert!(
        parse(&text)["repo_presets"][0].get("preset").is_none(),
        "{text}"
    );
}

/// Nothing answers `podman images`, so who uses each image cannot be told: both
/// reads say so, and say it as a key that is otherwise absent.
#[tokio::test]
async fn both_reads_say_when_the_image_users_could_not_be_told() {
    let state = AppState::init_for_tests().await.unwrap();
    let a = Admin::new(&state).await;
    let made: BuildSet = read_back(
        &a.text(
            "lmgw__build_set",
            json!({"action": "create", "preset": "official", "slug": "official-master"}),
        )
        .await,
    );
    let id = made.answer.build.unwrap().id;

    let text = a.text("lmgw__builds", json!({})).await;
    let list: BuildList = read_back(&text);
    let why = list.builds.usage_error.expect("the list says why");
    assert!(why.contains("could not be determined"), "{why}");
    assert!(parse(&text)["usage_error"].is_string());

    let text = a.text("lmgw__builds", json!({"id": id})).await;
    let one: BuildGetResponse = read_back(&text);
    assert!(one.usage_error.is_some(), "{text}");
    assert_eq!(one.view.build.id, id);
    assert!(parse(&text).get("repo_presets").is_none(), "{text}");
}

#[tokio::test]
async fn a_save_says_what_to_call_next_unless_the_build_is_gone() {
    let h = Harness::new().await;
    let a = Admin::new(&h.state).await;

    let text = a
        .text(
            "lmgw__build_set",
            json!({"action": "create", "slug": "official-master", "engine": "llama",
                   "repo_url": h.upstream.url(), "ref": "master", "arch": "89"}),
        )
        .await;
    let made: BuildSet = read_back(&text);
    let id = made.answer.build.as_ref().unwrap().id;
    let next = made.next_step.expect("a create says what to call next");
    assert!(
        next.contains(&format!("lmgw__build_run id={id} starts a run")),
        "{next}"
    );
    assert!(
        next.contains(&format!("lmgw__build_check_merge id={id}")),
        "{next}"
    );

    let text = a
        .text(
            "lmgw__build_set",
            json!({"action": "update", "id": id, "keep_runs": 2}),
        )
        .await;
    let updated: BuildSet = read_back(&text);
    assert_eq!(
        updated.answer.build.as_ref().unwrap().spec.keep_runs,
        Some(2)
    );
    assert!(updated.next_step.is_some());

    let text = a
        .text(
            "lmgw__build_set",
            json!({"action": "duplicate", "id": id, "slug": "copy"}),
        )
        .await;
    let copy: BuildSet = read_back(&text);
    let copy_id = copy.answer.build.as_ref().unwrap().id;
    assert_ne!(copy_id, id);
    assert!(copy.next_step.unwrap().contains(&format!("id={copy_id}")));

    // A delete leaves no build to run: no `next_step` key, not a null one.
    let text = a
        .text(
            "lmgw__build_set",
            json!({"action": "delete", "id": copy_id}),
        )
        .await;
    let gone: BuildSet = read_back(&text);
    assert!(gone.answer.build.is_none());
    assert_eq!(gone.next_step, None);
    assert!(parse(&text).get("next_step").is_none(), "{text}");
    assert!(parse(&text)["build"].is_null(), "{text}");
}

#[tokio::test]
async fn a_run_says_where_to_follow_it_and_its_log_is_one_chunk() {
    let h = Harness::new().await;
    let id = h.build(&h.spec()).await;
    let a = Admin::new(&h.state).await;

    let text = a.text("lmgw__build_run", json!({"id": id})).await;
    let started: dto::WithNextStep<BuildRunStarted> = read_back(&text);
    let run_id = started.answer.run_id;
    assert!(started.answer.job_id > 0);
    assert!(
        started
            .next_step
            .contains(&format!("lmgw__build_log run_id={run_id} offset=0")),
        "{}",
        started.next_step
    );
    assert!(started.next_step.contains(&format!("lmgw__builds id={id}")));
    wait_run(&h.state, run_id).await;

    let text = a.text("lmgw__build_log", json!({"run_id": run_id})).await;
    let chunk: RunLogChunk = read_back(&text);
    assert!(chunk.done);
    assert!(chunk.text.contains("(mcp)"), "{}", chunk.text);
    assert!(chunk.next_offset as usize >= chunk.text.len());

    let text = a.text("lmgw__builds", json!({"id": id})).await;
    let one: BuildGetResponse = read_back(&text);
    assert_eq!(one.runs.len(), 1);
    assert_eq!(one.runs[0].id, run_id);
    assert!(!one.more);
}

#[tokio::test]
async fn a_pull_says_how_to_follow_it() {
    let h = Harness::new().await;
    let a = Admin::new(&h.state).await;
    let text = a
        .text(
            "lmgw__container_image_pull",
            json!({"image": "ghcr.io/o/x:y"}),
        )
        .await;
    let started: dto::WithNextStep<ContainerImagePullStarted> = read_back(&text);
    assert!(started.answer.job_id > 0);
    assert_eq!(started.answer.image, "ghcr.io/o/x:y");
    assert!(
        started.next_step.contains(&format!(
            "the pull runs in the background (job {})",
            started.answer.job_id
        )),
        "{}",
        started.next_step
    );
}

#[tokio::test]
async fn the_pr_list_is_a_page_with_its_quota() {
    let h = Harness::new().await;
    let a = Admin::new(&h.state).await;
    let gh = MockServer::start().await;
    h.state
        .builds
        .set_forge_client(ForgeClient::new().unwrap().with_github_api(gh.uri()));
    let pull = |n: u64| {
        json!({
            "number": n, "title": format!("pr {n}"), "user": {"login": "someone"},
            "updated_at": "2026-09-25T10:00:00Z", "draft": n == 2, "state": "open",
            "head": {"sha": "a".repeat(40), "ref": "feature"},
            "base": {"sha": "b".repeat(40), "ref": "main"},
            "merged_at": null,
            "html_url": format!("https://github.com/o/r/pull/{n}"),
        })
    };
    let next = format!(
        "<{}/repositories/1/pulls?state=open&page=2>; rel=\"next\"",
        gh.uri()
    );
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", next.as_str())
                .insert_header("x-ratelimit-limit", "60")
                .insert_header("x-ratelimit-remaining", "58")
                .insert_header("x-ratelimit-reset", "1790000000")
                .set_body_json(json!([pull(1), pull(2)])),
        )
        .mount(&gh)
        .await;

    let text = a
        .text(
            "lmgw__forge_prs",
            json!({"repo_url": "https://github.com/o/r"}),
        )
        .await;
    let page: dto::builds::ForgePrPage = read_back(&text);
    assert_eq!(
        page.prs.iter().map(|p| p.number).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(page.next_page, Some(2));
    assert_eq!(page.rate_limit.unwrap().remaining, 58);
    assert!(parse(&text)["prs"][1]["draft"].as_bool().unwrap());

    // No quota headers on the last page: `null`, not an absent key.
    gh.reset().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&gh)
        .await;
    let text = a
        .text(
            "lmgw__forge_prs",
            json!({"repo_url": "https://github.com/o/r", "page": 2}),
        )
        .await;
    let empty: dto::builds::ForgePrPage = read_back(&text);
    assert!(empty.prs.is_empty() && empty.next_page.is_none() && empty.rate_limit.is_none());
    assert!(parse(&text)["next_page"].is_null() && parse(&text)["rate_limit"].is_null());
}
