//! Anthropic's paging of `GET /v1/models` (`limit`, `after_id`, `before_id`)
//! over the real router, and the OpenAI shape staying unpaged.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::Params;
use lmgw_core::server::build_router;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::Value;

const ALIASES: usize = 45;

/// A live gateway with `ALIASES` enabled aliases and the base URL to ask it.
async fn gateway() -> String {
    let state = AppState::init_for_tests().await.unwrap();
    let upstream = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "paging".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: false,
        },
    )
    .await
    .unwrap();
    for i in 0..ALIASES {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: format!("page-{i:02}"),
                upstream_id: upstream,
                upstream_model_id: "m".into(),
                param_overrides: Params::default(),
                enabled: true,
                capabilities_override: None,
                fallback: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { crate::common::serve_on(listener, app).await });
    base
}

async fn get(base: &str, query: &str, anthropic: bool) -> (u16, Value) {
    let mut rb = reqwest::Client::new().get(format!("{base}/v1/models{query}"));
    if anthropic {
        rb = rb.header("anthropic-version", "2023-06-01");
    }
    let resp = rb.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let v = serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"));
    (status, v)
}

fn ids(page: &Value) -> Vec<String> {
    page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_anthropic_list_pages_like_anthropics() {
    let base = gateway().await;
    let (_, everything) = get(&base, "", false).await;
    let all = ids_openai(&everything);
    assert!(all.len() >= ALIASES);

    // Default page: 20, with more to come.
    let (status, first) = get(&base, "", true).await;
    assert_eq!(status, 200);
    assert_eq!(ids(&first), all[..20]);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["first_id"], all[0].as_str());
    assert_eq!(first["last_id"], all[19].as_str());
    assert!(first["lmgw"].is_object());

    let (_, two) = get(&base, "?limit=2", true).await;
    assert_eq!(ids(&two), all[..2]);
    assert_eq!(two["has_more"], true);

    // Forwards with after_id = last_id: every id exactly once.
    let mut seen = Vec::new();
    let mut query = "?limit=7".to_string();
    loop {
        let (status, page) = get(&base, &query, true).await;
        assert_eq!(status, 200);
        let got = ids(&page);
        assert!(got.len() <= 7);
        seen.extend(got);
        if page["has_more"] == false {
            break;
        }
        query = format!("?limit=7&after_id={}", page["last_id"].as_str().unwrap());
    }
    assert_eq!(seen, all);

    // Backwards with before_id = first_id, from the end.
    let (_, last) = get(
        &base,
        &format!("?limit=7&after_id={}", all[all.len() - 8]),
        true,
    )
    .await;
    assert_eq!(ids(&last), all[all.len() - 7..]);
    assert_eq!(last["has_more"], false);
    let mut back: Vec<String> = ids(&last);
    let mut cursor = last["first_id"].as_str().unwrap().to_string();
    loop {
        let (status, page) = get(&base, &format!("?limit=7&before_id={cursor}"), true).await;
        assert_eq!(status, 200);
        let mut got = ids(&page);
        got.extend(back);
        back = got;
        if page["has_more"] == false {
            break;
        }
        cursor = page["first_id"].as_str().unwrap().to_string();
    }
    assert_eq!(back, all);

    // Past the end and before the start: an empty page, null cursors.
    let (_, end) = get(&base, &format!("?after_id={}", all[all.len() - 1]), true).await;
    assert_eq!(ids(&end), Vec::<String>::new());
    assert_eq!(end["has_more"], false);
    assert!(end["first_id"].is_null() && end["last_id"].is_null());
}

#[tokio::test]
async fn bad_paging_parameters_are_400_in_the_anthropic_envelope() {
    let base = gateway().await;
    for (query, names) in [
        ("?limit=0", "limit"),
        ("?limit=1001", "limit"),
        ("?limit=abc", "limit"),
        ("?after_id=page-01&before_id=page-03", "together"),
        ("?after_id=no-such-model", "no-such-model"),
        ("?before_id=no-such-model", "no-such-model"),
    ] {
        let (status, body) = get(&base, query, true).await;
        assert_eq!(status, 400, "{query}");
        assert_eq!(body["type"], "error", "{query}");
        assert_eq!(body["error"]["type"], "invalid_request_error", "{query}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(names), "{query}: {message}");
    }
}

#[tokio::test]
async fn the_openai_list_ignores_paging() {
    let base = gateway().await;
    let (_, plain) = get(&base, "", false).await;
    let (status, paged) = get(&base, "?limit=2&after_id=nope&before_id=nope", false).await;
    assert_eq!(status, 200);
    assert_eq!(ids_openai(&paged), ids_openai(&plain));
    assert!(ids_openai(&paged).len() >= ALIASES);
    assert!(paged.get("has_more").is_none());
}

fn ids_openai(list: &Value) -> Vec<String> {
    ids(list)
}
