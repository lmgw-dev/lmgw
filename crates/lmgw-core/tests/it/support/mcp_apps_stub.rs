//! A Streamable-HTTP MCP server with resources, for the suites of `/mcp`'s
//! resources and the MCP Apps metadata (client-apps design §7): the tools,
//! resources and templates it is given, every `initialize`'s params and
//! every `resources/read` URI kept, a read answered with an HTML document
//! naming the URI it was asked for — or `-32002` for a URI with `missing`
//! in it — and every `tools/call` with the result it was given.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{json, Value};

/// What the server offers.
#[derive(Clone, Default)]
pub struct Apps {
    pub tools: Value,
    pub resources: Value,
    pub templates: Value,
    /// Every `tools/call`'s answer.
    pub call_result: Value,
}

pub struct AppsStub {
    pub url: String,
    /// Every `initialize`'s `params`.
    pub inits: Arc<Mutex<Vec<Value>>>,
    /// Every `resources/read`'s URI, in arrival order.
    pub reads: Arc<Mutex<Vec<String>>>,
}

impl AppsStub {
    pub fn reads(&self) -> Vec<String> {
        self.reads.lock().unwrap().clone()
    }

    pub fn inits(&self) -> Vec<Value> {
        self.inits.lock().unwrap().clone()
    }
}

/// The text a read of `uri` answers with.
pub fn page(uri: &str) -> String {
    format!("<!doctype html><title>{uri}</title>")
}

pub async fn apps_stub(apps: Apps) -> AppsStub {
    let inits: Arc<Mutex<Vec<Value>>> = Arc::default();
    let reads: Arc<Mutex<Vec<String>>> = Arc::default();
    let (i, r) = (inits.clone(), reads.clone());
    let handler = move |body: String| {
        let (inits, reads, apps) = (i.clone(), r.clone(), apps.clone());
        async move {
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let answer = match req.get("method").and_then(Value::as_str).unwrap_or("") {
                "initialize" => {
                    inits.lock().unwrap().push(params.clone());
                    Ok(json!({
                        "protocolVersion": params["protocolVersion"],
                        "capabilities": {"tools": {}, "resources": {}},
                        "serverInfo": {"name": "apps-stub", "version": "0.1.0"},
                    }))
                }
                "tools/list" => Ok(json!({"tools": apps.tools})),
                "tools/call" => Ok(apps.call_result.clone()),
                "resources/list" => Ok(json!({"resources": apps.resources})),
                "resources/templates/list" => Ok(json!({"resourceTemplates": apps.templates})),
                "resources/read" => {
                    let uri = params["uri"].as_str().unwrap_or_default().to_string();
                    reads.lock().unwrap().push(uri.clone());
                    if uri.contains("missing") {
                        Err(json!({"code": -32002, "message": format!("no resource {uri}")}))
                    } else {
                        Ok(json!({"contents": [{
                            "uri": uri, "mimeType": "text/html;profile=mcp-app",
                            "text": page(&uri),
                            "_meta": {"ui": {"prefersBorder": true}}
                        }]}))
                    }
                }
                _ => Ok(json!({})),
            };
            let body = match answer {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
            };
            (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "apps-session"),
                ],
                body.to_string(),
            )
                .into_response()
        }
    };
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(handler).delete(|| async { StatusCode::OK }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    AppsStub {
        url: format!("http://{addr}/mcp"),
        inits,
        reads,
    }
}
