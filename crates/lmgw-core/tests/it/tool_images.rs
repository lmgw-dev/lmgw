//! Tool-result images on a managed llama.cpp row (llama egress design §8,
//! §9.3), on the `gpu_world` containers: the decision across a ladder climb,
//! a dead-container retry and a candidate re-pick, each rechecked on the
//! container it lands on and posting what that container counted; an MCP tool loop whose tool returns an image to a
//! row that sees; the placeholders and their reasons where a condition
//! fails; the count that builds a body without a send; and known facts with
//! vision and no tool image giving today's bytes.
//!
//! A seeing row here loads a projector file lmgw cannot read (one byte), so
//! its own `--image-max-tokens` is its per-image bound, and runs batches big
//! enough that no ubatch advisory applies. Its container answers `/props`
//! with `vision: true` (`gpu_world::World::props`).

use std::time::Duration;

use lmgw_core::config::{HoldFallbackMode, LlamaParams, Snapshot};
use lmgw_core::ladder::Rung;
use lmgw_core::store::{NewCandidateAlias, NewLocalModel};
use serde_json::{json, Value};

use crate::chat_actions::post;
use crate::common::{serve, Gw};
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::mcp_stub;

/// A whole 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
/// `<svg/>`.
const SVG: &str = "PHN2Zy8+";
/// A seeing row's per-image bound, its own `--image-max-tokens`.
const BOUND: u64 = 300;
/// What every container's `/tokenize` counts for any prompt.
const PROMPT: u64 = 5;

// ---------------------------------------------------------------------------
// Rows, requests and what the containers got
// ---------------------------------------------------------------------------

/// A `/props` body in llama-server's shape.
fn props(vision: bool) -> Value {
    json!({"modalities": {"vision": vision, "audio": false},
           "default_generation_settings": {"n_ctx": 4096}, "build_info": "b1-test"})
}

/// A public chat row `id` on `<id>.gguf` with `params`, `args` and rungs.
fn row(id: &str, params: LlamaParams, args: Vec<String>, ladder: Vec<Rung>) -> NewLocalModel {
    NewLocalModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        params,
        args,
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder,
    }
}

/// A row that sees (module doc): `params` with its projector and batches
/// added, its bound in its args, and its container's `/props` saying
/// `vision: true`.
async fn seeing(gpu: &Gpu, id: &str, params: LlamaParams, ladder: Vec<Rung>) -> NewLocalModel {
    let mmproj = format!("{id}-mmproj.gguf");
    gpu.file(&mmproj, 1);
    gpu.world().props.insert(id.into(), props(true));
    row(
        id,
        LlamaParams {
            mmproj_path: Some(mmproj),
            batch_size: Some(2048),
            ubatch_size: Some(2048),
            ..params
        },
        vec!["--image-max-tokens".into(), BOUND.to_string()],
        ladder,
    )
}

/// A guarded shared pool of `ctx` tokens over two slots.
fn pool(ctx: i64) -> LlamaParams {
    LlamaParams {
        ctx_size: Some(ctx),
        parallel: Some(2),
        kv_unified: Some(true),
        n_predict: Some(16),
        ..Default::default()
    }
}

/// An Anthropic conversation on `model` whose tool result is `blocks`.
fn tool_turn(model: &str, blocks: Value) -> Value {
    json!({"model": model, "max_tokens": 16, "messages": [
        {"role": "user", "content": "Show me the chart."},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "chart", "input": {}}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": blocks}]},
    ]})
}

fn image(mime: &str, data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})
}

fn png() -> Value {
    image("image/png", PNG)
}

/// The `image_url` part a sendable PNG goes as.
fn png_part() -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG}")}})
}

/// The placeholder an image of `mime` and `data` becomes, for `why`.
fn placeholder(mime: &str, data: &str, why: &str) -> String {
    format!(
        "[{mime} image, {} base64 bytes — omitted: {why}]",
        data.len()
    )
}

/// The bodies posted to `route` on the live containers, with each one's port.
async fn bodies(gpu: &Gpu, route: &str) -> Vec<(u16, Value)> {
    gpu.posted()
        .await
        .into_iter()
        .filter(|r| r.url.path() == route)
        .map(|r| {
            // The container's port, as the request addressed it.
            let port = r
                .headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.rsplit(':').next())
                .and_then(|p| p.parse().ok())
                .expect("a container port in the Host header");
            (port, serde_json::from_slice(&r.body).expect("a JSON body"))
        })
        .collect()
}

/// The bodies posted to `route` on the container at `port`.
async fn bodies_on(gpu: &Gpu, route: &str, port: u16) -> Vec<Value> {
    bodies(gpu, route)
        .await
        .into_iter()
        .filter(|(p, _)| *p == port)
        .map(|(_, b)| b)
        .collect()
}

/// The `content` of the one `tool` message in a chat body.
fn tool_content(body: &Value) -> Value {
    let tools: Vec<&Value> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tools.len(), 1, "one tool message: {body}");
    tools[0]["content"].clone()
}

/// The running container of `model`.
fn view(gpu: &Gpu, model: &str) -> lmgw_core::runtime::registry::RuntimeView {
    gpu.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model)
        .unwrap_or_else(|| panic!("'{model}' runs"))
}

/// What the running container of `model` said about whether it sees.
fn vision_of(gpu: &Gpu, model: &str) -> Option<bool> {
    view(gpu, model).llama_props.and_then(|f| f.vision)
}

/// Start `model` as the owner would, and leave it resident and idle.
async fn load(gpu: &Gpu, model: &str) -> u16 {
    let hold = lmgw_core::vram::admit(&gpu.state, &gpu.route(model), model)
        .await
        .unwrap()
        .expect("a local model");
    hold.port()
}

/// A gateway on a card of 16 GiB with `containers` ports for starts.
async fn world(containers: usize) -> (Gpu, Gw) {
    let gpu = Gpu::new(16 * GIB, containers, 5).await;
    gpu.world().prompt_tokens = PROMPT;
    let gw = serve(gpu.state.clone()).await;
    (gpu, gw)
}

// ---------------------------------------------------------------------------
// The rendering, and today's bytes
// ---------------------------------------------------------------------------

/// Known facts with vision and no tool image: the body is byte for byte what
/// the llama.cpp egress renders with nothing known.
#[tokio::test]
async fn known_vision_and_no_tool_image_give_todays_bytes() {
    let (gpu, gw) = world(1).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    let request = json!({"model": "eye", "max_tokens": 16, "messages": [
        {"role": "user", "content": "Weather?"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
            "function": {"name": "get_weather", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "c1", "content": "12°C"},
    ]});
    let resp = post(&gw, "/v1/chat/completions", request.clone()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(vision_of(&gpu, "eye"), Some(true), "the facts were known");

    let ir = lmgw_core::ingress::openai::parse_chat_request(&request).unwrap();
    let today = lmgw_core::egress::llama_cpp::chat_body(
        &ir,
        "eye",
        &ir.params,
        false,
        &Snapshot::default().router_upstream(),
    );
    let posted = bodies(&gpu, "/v1/chat/completions").await;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        serde_json::to_string(&posted[0].1).unwrap(),
        serde_json::to_string(&today).unwrap()
    );
}

/// On a row that sees, a tool result's PNG goes as an image beside its
/// text, and an SVG beside it as its placeholder naming the format.
#[tokio::test]
async fn a_tool_image_goes_to_a_row_that_sees() {
    let (gpu, gw) = world(1).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    let blocks =
        json!([{"type": "text", "text": "The chart:"}, png(), image("image/svg+xml", SVG)]);
    let resp = post(&gw, "/v1/messages", tool_turn("eye", blocks)).await;
    assert_eq!(resp.status(), 200);

    let posted = bodies(&gpu, "/v1/chat/completions").await;
    assert_eq!(
        tool_content(&posted[0].1),
        json!([
            {"type": "text", "text": "The chart:"},
            png_part(),
            {"type": "text",
             "text": placeholder("image/svg+xml", SVG, "svg is not a format llama.cpp decodes")},
        ])
    );
}

/// Where a condition fails, every tool image is its placeholder naming the
/// condition: a server without vision, a projector that can abort above the
/// batch size. A server that said nothing gets today's bytes.
#[tokio::test]
async fn a_failed_condition_is_the_placeholders_reason_and_unknown_is_today() {
    let (gpu, gw) = world(3).await;
    // Says it does not see.
    gpu.row(row("blind", Default::default(), vec![], vec![]), GIB)
        .await;
    gpu.world().props.insert("blind".into(), props(false));
    // Sees, at llama.cpp's default ubatch: its projector can abort.
    let mut abort = seeing(&gpu, "abort", Default::default(), vec![]).await;
    abort.params.ubatch_size = None;
    gpu.row(abort, GIB).await;
    // Says nothing: no `/props` at all.
    gpu.row(row("mute", Default::default(), vec![], vec![]), GIB)
        .await;

    for (model, why) in [
        ("blind", "this model's server has no vision"),
        ("abort", "its projector can abort above the batch size"),
        ("mute", "this upstream's tool-result slot is text-only"),
    ] {
        let resp = post(&gw, "/v1/messages", tool_turn(model, json!([png()]))).await;
        assert_eq!(resp.status(), 200, "{model}");
        let port = view(&gpu, model).port;
        let posted = bodies_on(&gpu, "/v1/chat/completions", port).await;
        assert_eq!(
            tool_content(&posted[0]),
            json!(placeholder("image/png", PNG, why)),
            "{model}"
        );
    }
    assert_eq!(vision_of(&gpu, "mute"), None, "nothing was known");
}

/// A server that sees, on a row whose projector lmgw cannot read: one loaded
/// by `--mmproj-url`, or named but not in the models dir. Whether its
/// images are decoded non-causally is unknown, so below an unknown
/// projector's batch (Gemma 4's measured image ubatch) a tool image is its
/// placeholder; at or above it, it goes.
#[tokio::test]
async fn a_projector_lmgw_cannot_read_needs_an_images_batch() {
    let (gpu, gw) = world(3).await;
    let url = || {
        vec![
            "--mmproj-url".to_string(),
            "https://example.org/mmproj.gguf".to_string(),
        ]
    };
    gpu.row(row("url", Default::default(), url(), vec![]), GIB)
        .await;
    let big = LlamaParams {
        batch_size: Some(2048),
        ubatch_size: Some(2048),
        ..Default::default()
    };
    gpu.row(row("urlbig", big, url(), vec![]), GIB).await;
    let ghost = LlamaParams {
        mmproj_path: Some("ghost-mmproj.gguf".into()),
        ..Default::default()
    };
    gpu.row(row("ghost", ghost, vec![], vec![]), GIB).await;
    for model in ["url", "urlbig", "ghost"] {
        gpu.world().props.insert(model.into(), props(true));
    }

    for (model, sent) in [
        (
            "url",
            json!(placeholder(
                "image/png",
                PNG,
                "lmgw cannot read the projector it loads, and its batch size is below a whole \
                 image"
            )),
        ),
        ("urlbig", json!([png_part()])),
        (
            "ghost",
            json!(placeholder(
                "image/png",
                PNG,
                "its projector can abort above the batch size"
            )),
        ),
    ] {
        let resp = post(&gw, "/v1/messages", tool_turn(model, json!([png()]))).await;
        assert_eq!(resp.status(), 200, "{model}");
        assert_eq!(vision_of(&gpu, model), Some(true), "{model} says it sees");
        let port = view(&gpu, model).port;
        let posted = bodies_on(&gpu, "/v1/chat/completions", port).await;
        assert_eq!(tool_content(&posted[0]), sent, "{model}");
    }
}

/// The count that builds a body without a send decides as the send would:
/// a tool image that goes is counted at the row's bound and rendered as an
/// image; one kept back is its placeholder and adds nothing.
#[tokio::test]
async fn the_count_decides_as_the_send_does() {
    let (gpu, gw) = world(2).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    let mut abort = seeing(&gpu, "abort", Default::default(), vec![]).await;
    abort.params.ubatch_size = None;
    gpu.row(abort, GIB).await;

    for (model, images) in [("eye", 1), ("abort", 0)] {
        let resp = post(
            &gw,
            "/v1/messages/count_tokens",
            tool_turn(model, json!([png()])),
        )
        .await;
        assert_eq!(resp.status(), 200, "{model}");
        let got: Value = resp.json().await.unwrap();
        assert_eq!(
            got,
            json!({"input_tokens": PROMPT + images * BOUND}),
            "{model}"
        );
        let port = view(&gpu, model).port;
        let templated = bodies_on(&gpu, "/apply-template", port).await;
        let content = tool_content(&templated[0]);
        if images == 1 {
            assert_eq!(content, json!([png_part()]), "{model}");
        } else {
            assert_eq!(
                content,
                json!(placeholder(
                    "image/png",
                    PNG,
                    "its projector can abort above the batch size"
                )),
                "{model}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The frozen decision: decided once, posting what was counted
// ---------------------------------------------------------------------------

/// The decision is made on the rung that runs, and every attempt holds it
/// against the container it goes to: the rung the request climbs to says it
/// does not see, so the count and the send to it both carry the
/// placeholder, never the image the first rung's decision let go. The count
/// on the first rung held the image at its bound; the second rung's count
/// and send agree.
#[tokio::test]
async fn a_ladder_climb_to_a_rung_that_does_not_see_posts_the_placeholder() {
    let (gpu, gw) = world(2).await;
    gpu.file("lad-2.gguf", GIB);
    let base = LlamaParams {
        ctx_size: Some(64),
        parallel: Some(1),
        n_predict: Some(16),
        ..Default::default()
    };
    let rungs = vec![Rung {
        gguf_path: "lad-2.gguf".into(),
        ctx_size: 4096,
    }];
    gpu.row(seeing(&gpu, "lad", base, rungs).await, GIB).await;
    // Rung 1 runs, and sees.
    let resp = post(
        &gw,
        "/v1/messages",
        tool_turn("lad", json!([{"type": "text", "text": "12°C"}])),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(vision_of(&gpu, "lad"), Some(true));
    let rung1 = view(&gpu, "lad").port;
    // Whatever starts next says it does not.
    gpu.world().props.insert("lad".into(), props(false));

    // 5 + 300 for the image + 16 of output: past rung 1's 64.
    let resp = post(&gw, "/v1/messages", tool_turn("lad", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    let rung2 = view(&gpu, "lad").port;
    assert_ne!(rung2, rung1, "the request climbed");
    assert_eq!(gpu.runs(), ["lad", "lad"]);
    assert_eq!(
        vision_of(&gpu, "lad"),
        Some(false),
        "the rung that answered says it does not see"
    );

    let first = bodies_on(&gpu, "/apply-template", rung1).await;
    assert_eq!(
        tool_content(first.last().expect("counted on rung 1")),
        json!([png_part()]),
        "rung 1 counted the image its decision let go"
    );
    let posted = bodies_on(&gpu, "/v1/chat/completions", rung2).await;
    let counted = bodies_on(&gpu, "/apply-template", rung2).await;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        tool_content(&posted[0]),
        json!(placeholder(
            "image/png",
            PNG,
            "this model's server has no vision"
        ))
    );
    assert_eq!(counted, posted, "rung 2 posted what it counted");
}

/// A dead container is recovered during the count: the decision made on
/// the container the request was admitted to is held against the one that
/// replaced it, which says it does not see. The retried count and the send
/// both carry the placeholder, and agree.
#[tokio::test]
async fn a_dead_container_retry_onto_one_that_does_not_see_posts_the_placeholder() {
    let (gpu, gw) = world(2).await;
    gpu.row(seeing(&gpu, "pool", pool(8192), vec![]).await, GIB)
        .await;
    let first = load(&gpu, "pool").await;
    assert_eq!(vision_of(&gpu, "pool"), Some(true));
    gpu.world().props.insert("pool".into(), props(false));
    gpu.kill(first).await;

    let resp = post(&gw, "/v1/messages", tool_turn("pool", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    let second = view(&gpu, "pool").port;
    assert_ne!(second, first, "recovered on a new container");
    assert_eq!(vision_of(&gpu, "pool"), Some(false));

    let posted = bodies_on(&gpu, "/v1/chat/completions", second).await;
    let counted = bodies_on(&gpu, "/apply-template", second).await;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        tool_content(&posted[0]),
        json!(placeholder(
            "image/png",
            PNG,
            "this model's server has no vision"
        ))
    );
    assert_eq!(counted, posted, "posted what the retried count counted");
}

/// The row is edited while its container runs (the ubatch dropped to
/// llama.cpp's default), then the container dies: the send's retry lands on
/// a container re-admitted from the edited row, which carries the projector
/// advisory. The image the first container's decision let go is its
/// placeholder there.
#[tokio::test]
async fn a_dead_container_retry_onto_an_edited_row_posts_the_placeholder() {
    let (gpu, gw) = world(2).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    let first = load(&gpu, "eye").await;
    let id = gpu
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == "eye")
        .expect("the row")
        .id;
    let mut edited = seeing(&gpu, "eye", Default::default(), vec![]).await;
    edited.params.ubatch_size = None;
    lmgw_core::store::update_local_model(&gpu.state.db, id, &edited)
        .await
        .unwrap();
    gpu.state.reload_snapshot().await.unwrap();
    gpu.kill(first).await;

    let resp = post(&gw, "/v1/messages", tool_turn("eye", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    let second = view(&gpu, "eye").port;
    assert_ne!(second, first, "re-admitted on a new container");
    assert_eq!(vision_of(&gpu, "eye"), Some(true), "it still sees");

    let posted = bodies_on(&gpu, "/v1/chat/completions", second).await;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        tool_content(&posted[0]),
        json!(placeholder(
            "image/png",
            PNG,
            "its projector can abort above the batch size"
        ))
    );
}

/// A complete capability object that says the row sees — the sparse weights
/// carry no GGUF header, so a candidate alias's Vision facet needs it.
fn sees() -> Value {
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": ["text", "image"],
        "vision": true,
        "source": "owner",
    }})
}

/// A guest's candidate counts the tool image its decision lets go, does not
/// fit, and is picked again: the next candidate decides for itself — its
/// server says it does not see, so the image goes as that placeholder — and
/// posts exactly what it counted. The first never got the request.
#[tokio::test]
async fn a_candidate_repick_decides_for_itself() {
    let (gpu, gw) = world(2).await;
    let mut p = seeing(&gpu, "p", pool(256), vec![]).await;
    p.capabilities_override = Some(sees());
    gpu.row(p, GIB).await;
    let mut a = row("a", pool(8192), vec![], vec![]);
    a.capabilities_override = Some(sees());
    gpu.row(a, GIB).await;
    gpu.world().props.insert("a".into(), props(false));
    gpu.candidate(NewCandidateAlias {
        alias: "jobs".into(),
        candidates: vec!["p".into(), "a".into()],
        background: true,
        fallback_mode: HoldFallbackMode::None,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec!["vision".into()],
        enabled: true,
        notes: String::new(),
    })
    .await;
    let p_port = load(&gpu, "p").await;
    let a_port = load(&gpu, "a").await;

    // On p: 5 + 300 + 16 does not fit its 256.
    let resp = post(&gw, "/v1/messages", tool_turn("jobs", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-candidate")
            .and_then(|v| v.to_str().ok()),
        Some("a")
    );

    let p_counted = bodies_on(&gpu, "/apply-template", p_port).await;
    assert_eq!(
        tool_content(&p_counted[0]),
        json!([png_part()]),
        "p's decision let the image go, and its count held it"
    );
    assert!(
        bodies_on(&gpu, "/v1/chat/completions", p_port)
            .await
            .is_empty(),
        "p never got the request"
    );
    let posted = bodies_on(&gpu, "/v1/chat/completions", a_port).await;
    let counted = bodies_on(&gpu, "/apply-template", a_port).await;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        tool_content(&posted[0]),
        json!(placeholder(
            "image/png",
            PNG,
            "this model's server has no vision"
        ))
    );
    assert_eq!(counted, posted, "posted exactly what was counted");
}

/// A candidate alias that does not enable Vision keeps tool images back on
/// a model that sees, naming the alias.
#[tokio::test]
async fn a_candidate_alias_without_vision_keeps_tool_images_back() {
    let (gpu, gw) = world(1).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    gpu.candidate(NewCandidateAlias {
        alias: "plain".into(),
        candidates: vec!["eye".into()],
        background: false,
        fallback_mode: HoldFallbackMode::None,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    })
    .await;
    let resp = post(&gw, "/v1/messages", tool_turn("plain", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    let posted = bodies(&gpu, "/v1/chat/completions").await;
    assert_eq!(
        tool_content(&posted[0].1),
        json!(placeholder(
            "image/png",
            PNG,
            "the candidate alias does not enable Vision"
        ))
    );
}

// ---------------------------------------------------------------------------
// End to end: an MCP tool returns an image
// ---------------------------------------------------------------------------

/// The Chat's tool loop on a row that sees: the MCP tool answers with text
/// and a PNG, and the model's next call gets both, the image as an image.
#[tokio::test]
async fn an_mcp_tool_loop_sends_the_tools_image_to_a_row_that_sees() {
    let (gpu, gw) = world(1).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    {
        let mut w = gpu.world();
        w.calls_tool.insert("eye".into(), "stub__shot".into());
        w.thinking.insert("eye".into());
    }
    let stub = mcp_stub::answering(
        json!([{"name": "shot", "description": "a screenshot",
                "inputSchema": {"type": "object", "properties": {}}}]),
        false,
        mcp_stub::answer(|_, _| async {
            json!({"content": [{"type": "text", "text": "the screen:"},
                               {"type": "image", "data": PNG, "mimeType": "image/png"}],
                   "isError": false})
        }),
    )
    .await;
    mcp_stub::register(&gpu.state, "stub-server", "stub", &stub.url, true, None).await;

    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "eye"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "stub"}]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "what is on the screen?"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let _ = tokio::time::timeout(Duration::from_secs(10), r.text())
        .await
        .expect("the turn ends");

    assert_eq!(stub.calls().len(), 1, "the tool was called once");
    let after_tool: Vec<Value> = bodies(&gpu, "/v1/chat/completions")
        .await
        .into_iter()
        .map(|(_, b)| b)
        .filter(|b| {
            b["messages"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m["role"] == "tool"))
        })
        .collect();
    assert_eq!(after_tool.len(), 1, "one model call after the tool");
    assert_eq!(
        tool_content(&after_tool[0]),
        json!([{"type": "text", "text": "the screen:"}, png_part()])
    );
}

/// A guard against the world's own drift: a seeing row's container really
/// was started with its projector and said it sees.
#[tokio::test]
async fn a_seeing_row_starts_with_its_projector() {
    let (gpu, _gw) = world(1).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    load(&gpu, "eye").await;
    let w = gpu.world();
    assert!(w.projectors.contains("eye"));
    assert_eq!(w.props_reads, ["eye"]);
    drop(w);
    let v = view(&gpu, "eye");
    assert_eq!(v.llama_props.and_then(|f| f.vision), Some(true));
    assert!(
        v.warnings.is_empty(),
        "no ubatch advisory, no failed read: {:?}",
        v.warnings
    );
}

// ---------------------------------------------------------------------------
// An external row
// ---------------------------------------------------------------------------

/// An external `llama_cpp` row `name` whose server (a wiremock) sees and
/// answers every chat request, stored with an alias `alias` on its model:
/// the mock, and the row's id.
async fn external_llama(
    state: &lmgw_core::state::SharedState,
    name: &str,
    alias: &str,
) -> (wiremock::MockServer, i64) {
    use lmgw_core::config::{Protocol, UpstreamKind};
    use lmgw_core::store::{self, NewAlias, NewUpstream};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(props(true)))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 0, "model": "gguf",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "a chart"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 1, "total_tokens": 8},
        })))
        .mount(&mock)
        .await;
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol: Protocol::LlamaCpp,
            kind: UpstreamKind::LlamaServer,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: alias.into(),
            upstream_id: id,
            upstream_model_id: "chat-gguf".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    (mock, id)
}

/// The tool message of the last chat request `mock` got.
async fn last_tool_content(mock: &wiremock::MockServer) -> Value {
    let last = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .last()
        .expect("a chat request");
    tool_content(&serde_json::from_slice(&last.body).unwrap())
}

/// An external `llama_cpp` row whose server sees: its first send goes out
/// with nothing known, today's bytes (decision 14), and asks the server in
/// the background; the next one sends the tool image as an image.
#[tokio::test]
async fn an_external_row_sends_tool_images_once_its_facts_are_known() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let (mock, id) = external_llama(&state, "llama-ext", "ext").await;
    let gw = serve(state.clone()).await;

    let sent = || async {
        let resp = post(&gw, "/v1/messages", tool_turn("ext", json!([png()]))).await;
        assert_eq!(resp.status(), 200);
        last_tool_content(&mock).await
    };
    assert_eq!(
        sent().await,
        json!(placeholder(
            "image/png",
            PNG,
            "this upstream's tool-result slot is text-only"
        )),
        "nothing known yet: today's bytes"
    );
    state.llama_facts.settled(id).await;
    assert_eq!(sent().await, json!([png_part()]));
}

/// A candidate alias that does not enable Vision, answered by its alias
/// fallback under the GPU hold: an external row that sees, reached with no
/// hold at all. The alias still keeps the tool image back; the same row
/// named directly sends it.
#[tokio::test]
async fn a_candidate_alias_fallback_without_a_hold_keeps_tool_images_back() {
    let (gpu, gw) = world(1).await;
    gpu.row(seeing(&gpu, "eye", Default::default(), vec![]).await, GIB)
        .await;
    let (mock, id) = external_llama(&gpu.state, "llama-ext", "ext").await;
    gpu.candidate(NewCandidateAlias {
        alias: "plain".into(),
        candidates: vec!["eye".into()],
        background: false,
        fallback_mode: HoldFallbackMode::Alias,
        fallback: Some("ext".into()),
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    })
    .await;
    // The external row's facts, known: it sees.
    let resp = post(&gw, "/v1/messages", tool_turn("ext", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    gpu.state.llama_facts.settled(id).await;
    lmgw_core::ops::hold_set(&gpu.state, true).await.unwrap();

    let resp = post(&gw, "/v1/messages", tool_turn("plain", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("ext"),
        "the alias fallback answered"
    );
    assert_eq!(
        last_tool_content(&mock).await,
        json!(placeholder(
            "image/png",
            PNG,
            "the candidate alias does not enable Vision"
        ))
    );
    assert!(
        gpu.runs().is_empty(),
        "nothing local started under the hold"
    );

    let resp = post(&gw, "/v1/messages", tool_turn("ext", json!([png()]))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(last_tool_content(&mock).await, json!([png_part()]));
}
