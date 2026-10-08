//! `settle` on both upstream write paths (llama.cpp egress design §5): the
//! tool plane's `upstream_set` and the dashboard's `upstream_set_full`
//! store the same protocol and kind for the same request, refuse the same
//! ones by name, and say the same about what they stored differently.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ops::{upstream_set, RowWriter, UpstreamPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewUpstream};
use serde_json::{json, Value};

use crate::common::{serve, Gw};

/// `(protocol, kind, supports_responses)` as stored.
type Shape = (Protocol, UpstreamKind, bool);

/// One request against an upstream that is `current` (`None`: a create).
struct Case {
    what: &'static str,
    current: Option<Shape>,
    protocol: Option<&'static str>,
    kind: Option<&'static str>,
    supports_responses: Option<bool>,
    /// What is stored, and a phrase the message must carry (`""`: none).
    want: Result<(Shape, &'static str), &'static str>,
}

use Protocol::{Anthropic, Gemini, LlamaCpp, Openai};
use UpstreamKind::{AudioCpp, Generic, LlamaServer};

fn cases() -> Vec<Case> {
    let case = |what, current, protocol, kind, want| Case {
        what,
        current,
        protocol,
        kind,
        supports_responses: None,
        want,
    };
    let llama = Some((LlamaCpp, LlamaServer, false));
    vec![
        case(
            "create llama_cpp",
            None,
            Some("llama_cpp"),
            None,
            Ok(((LlamaCpp, LlamaServer, false), "kind set to llama_server")),
        ),
        case(
            "create llama_cpp + llama_server",
            None,
            Some("llama_cpp"),
            Some("llama_server"),
            Ok(((LlamaCpp, LlamaServer, false), "")),
        ),
        case(
            "create llama_cpp + generic",
            None,
            Some("llama_cpp"),
            Some("generic"),
            Err("always kind llama_server"),
        ),
        case(
            "create the old spelling",
            None,
            Some("openai"),
            Some("llama_server"),
            Ok(((LlamaCpp, LlamaServer, false), "old spelling")),
        ),
        case(
            "create anthropic + llama_server",
            None,
            Some("anthropic"),
            Some("llama_server"),
            Ok(((Anthropic, LlamaServer, false), "")),
        ),
        case(
            "create gemini + llama_server",
            None,
            Some("gemini"),
            Some("llama_server"),
            Err("does not serve the Gemini API"),
        ),
        case(
            "create sd_cpp",
            None,
            Some("openai"),
            Some("sd_cpp"),
            Err("image class"),
        ),
        case(
            "create openai + audio_cpp",
            None,
            Some("openai"),
            Some("audio_cpp"),
            Ok(((Openai, AudioCpp, false), "")),
        ),
        case(
            "llama_cpp row to openai",
            llama,
            Some("openai"),
            None,
            Ok(((Openai, Generic, false), "kind set to generic")),
        ),
        case(
            "llama_cpp row to gemini",
            llama,
            Some("gemini"),
            None,
            Ok(((Gemini, Generic, false), "kind set to generic")),
        ),
        case(
            "llama_cpp row to anthropic",
            llama,
            Some("anthropic"),
            None,
            Ok(((Anthropic, LlamaServer, false), "")),
        ),
        case(
            "llama_cpp row, kind generic alone",
            llama,
            None,
            Some("generic"),
            Err("send its new protocol as well"),
        ),
        case(
            "llama_cpp row, the dashboard's reset",
            llama,
            Some("openai"),
            Some("generic"),
            Ok(((Openai, Generic, false), "")),
        ),
        case(
            "openai row, kind llama_server alone",
            Some((Openai, Generic, false)),
            None,
            Some("llama_server"),
            Ok(((LlamaCpp, LlamaServer, false), "old spelling")),
        ),
        case(
            "gemini row, kind llama_server alone",
            Some((Gemini, Generic, false)),
            None,
            Some("llama_server"),
            Err("does not serve the Gemini API"),
        ),
        case(
            "anthropic row, kind llama_server alone",
            Some((Anthropic, Generic, false)),
            None,
            Some("llama_server"),
            Ok(((Anthropic, LlamaServer, false), "")),
        ),
        case(
            "a rename touches neither",
            llama,
            None,
            None,
            Ok(((LlamaCpp, LlamaServer, false), "")),
        ),
        case(
            "openai row with native responses to llama_cpp",
            Some((Openai, Generic, true)),
            Some("llama_cpp"),
            None,
            Ok((
                (LlamaCpp, LlamaServer, false),
                "native /v1/responses switched off",
            )),
        ),
        Case {
            what: "llama_cpp asked for native responses",
            current: None,
            protocol: Some("llama_cpp"),
            kind: None,
            supports_responses: Some(true),
            want: Err("supports_responses cannot be on"),
        },
        Case {
            what: "the dashboard's llama_cpp save",
            current: llama,
            protocol: Some("llama_cpp"),
            kind: Some("llama_server"),
            supports_responses: Some(false),
            want: Ok(((LlamaCpp, LlamaServer, false), "")),
        },
    ]
}

async fn insert(state: &SharedState, name: &str, (protocol, kind, responses): Shape) -> i64 {
    store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol,
            kind,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 1_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: responses,
        },
    )
    .await
    .unwrap()
}

async fn stored(state: &SharedState, id: i64) -> Shape {
    let u = store::get_upstream(&state.db, id).await.unwrap().unwrap();
    (u.protocol, u.kind, u.supports_responses)
}

async fn stored_by_name(state: &SharedState, name: &str) -> Option<Shape> {
    store::get_upstream_by_name(&state.db, name)
        .await
        .unwrap()
        .map(|u| (u.protocol, u.kind, u.supports_responses))
}

/// The request as the tool plane takes it: `Ok(message)` or the refusal.
async fn via_tool_plane(
    state: &SharedState,
    c: &Case,
    name: &str,
    id: Option<i64>,
) -> Result<String, String> {
    let patch = UpstreamPatch {
        action: if id.is_some() { "update" } else { "create" }.into(),
        id,
        name: Some(name.into()),
        base_url: Some("http://127.0.0.1:9/v1".into()),
        protocol: c.protocol.map(String::from),
        kind: c.kind.map(String::from),
        supports_responses: c.supports_responses,
        ..Default::default()
    };
    upstream_set(state, patch, RowWriter::Tool)
        .await
        .map(|v| v["message"].as_str().unwrap().to_string())
}

/// The request as the dashboard posts it.
async fn via_dashboard(gw: &Gw, c: &Case, name: &str, id: Option<i64>) -> Result<String, String> {
    let mut body = json!({
        "action": if id.is_some() { "update" } else { "create" },
        "name": name,
        "base_url": "http://127.0.0.1:9/v1",
    });
    if let Some(id) = id {
        body["id"] = json!(id);
    }
    for (key, value) in [("protocol", c.protocol), ("kind", c.kind)] {
        if let Some(v) = value {
            body[key] = json!(v);
        }
    }
    if let Some(r) = c.supports_responses {
        body["supports_responses"] = json!(r);
    }
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/upstream_set_full"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap();
    match status {
        200 => Ok(v["message"].as_str().unwrap().to_string()),
        400 => Err(v["message"].as_str().unwrap().to_string()),
        other => panic!("{}: status {other}: {v}", c.what),
    }
}

#[tokio::test]
async fn both_write_paths_settle_protocol_and_kind_row_by_row() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    for (n, c) in cases().iter().enumerate() {
        for dashboard in [false, true] {
            let path = if dashboard { "dashboard" } else { "tool plane" };
            let what = format!("{} ({path})", c.what);
            // Unique per case and path; a create stores it, an update renames.
            let name = format!("up-{n}-{dashboard}");
            let id = match c.current {
                Some(shape) => Some(insert(&state, &name, shape).await),
                None => None,
            };
            let got = if dashboard {
                via_dashboard(&gw, c, &name, id).await
            } else {
                via_tool_plane(&state, c, &name, id).await
            };
            let now = match id {
                Some(id) => Some(stored(&state, id).await),
                None => stored_by_name(&state, &name).await,
            };
            // What each path answers when there is nothing to add.
            let plain = match (id, dashboard) {
                (None, _) => format!("upstream '{name}' created"),
                (Some(id), false) => format!("upstream {id} updated"),
                (Some(_), true) => "upstream updated".to_string(),
            };
            match c.want {
                Ok((shape, said)) => {
                    let message = got.unwrap_or_else(|e| panic!("{what}: refused: {e}"));
                    assert_eq!(now, Some(shape), "{what}");
                    if said.is_empty() {
                        assert_eq!(message, plain, "{what}");
                    } else {
                        assert!(message.starts_with(&plain), "{what}: {message}");
                        assert!(message.contains(said), "{what}: {message}");
                    }
                }
                Err(why) => {
                    let e = got.expect_err(&what);
                    assert!(e.contains(why), "{what}: {e}");
                    assert_eq!(now, c.current, "{what}: a refusal stores nothing");
                }
            }
        }
    }
}
