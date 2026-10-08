//! `stream_options.include_usage` on the managed local models and across the
//! swaps (the top-level `stream_usage` suite has the rule and one case per
//! upstream protocol): a model's own llama-server container, a hold's
//! fallback to a cloud alias, and a ladder's climb. Each answers the client by
//! OpenAI's rule, and records the tokens in its row either way.

use super::*;
use crate::stream_usage::{
    assert_usage_rule, chunks, last_post, sse, text_of, Ask, LLAMA_SSE, OPENAI_SSE,
};

/// The model's own container streams llama-server's real shape, usage chunk
/// and timings included: the client sees that chunk only when it asked.
#[tokio::test]
async fn a_local_models_stream_shows_usage_only_when_asked() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    // The first model to start gets the first container.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(LLAMA_SSE))
        .with_priority(1)
        .mount(&f.first)
        .await;
    for ask in Ask::ALL {
        let before = log_count(&f, "chat-model").await;
        let resp = chat_body(&f.gateway, ask.body("chat-model", "hi")).await;
        assert_eq!(resp.status(), 200, "{ask:?}");
        let chunks = chunks(&resp.text().await.unwrap());
        assert_usage_rule(&chunks, ask, (7, 2));
        assert_eq!(text_of(&chunks), "Hello", "{ask:?}");
        assert_eq!(chat_port(&f), f.first.address().port());
        assert_eq!(
            last_post(&f.first).await["stream_options"],
            json!({"include_usage": true}),
            "{ask:?}: llama-server is asked for usage regardless"
        );
        let row = newest_log(&f, "chat-model", before).await;
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens),
            (Some(7), Some(2)),
            "{ask:?}: {row:?}"
        );
    }
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// Under a hold the request goes to the fallback alias before anything is
/// sent; the client's ask goes with it.
#[tokio::test]
async fn a_held_models_fallback_stream_shows_usage_only_when_asked() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = cloud_upstream(&f, "cloud-chat").await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_partial_json(
            json!({"stream": true}),
        ))
        .respond_with(sse(OPENAI_SSE))
        .with_priority(1)
        .mount(&cloud)
        .await;
    set_global_fallback(&f, "cloud-chat").await;
    engage_hold(&f).await;

    for ask in Ask::ALL {
        let before = log_count(&f, "chat-model").await;
        let resp = chat_body(&f.gateway, ask.body("chat-model", "hi")).await;
        assert_eq!(resp.status(), 200, "{ask:?}");
        assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
        let chunks = chunks(&resp.text().await.unwrap());
        assert_usage_rule(&chunks, ask, (7, 2));
        assert_eq!(text_of(&chunks), "Hello", "{ask:?}");
        assert!(chunks.iter().all(|c| c["model"] == "chat-model"), "{ask:?}");
        let row = newest_log(&f, "chat-model", before).await;
        assert_eq!(row.upstream_name.as_deref(), Some("cloud"), "{row:?}");
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens),
            (Some(7), Some(2)),
            "{ask:?}: {row:?}"
        );
    }
    assert!(f.runs().is_empty(), "nothing local started under the hold");
}

/// A prompt too big for rungs 1 and 2 climbs to rung 3 before its answer
/// streams: the answer the client gets is rung 3's, under the client's ask.
/// A fresh ladder per ask, so each one climbs.
#[tokio::test]
async fn a_streamed_climb_shows_usage_only_when_asked() {
    for ask in Ask::ALL {
        let f = ladder_fixture(16 * GIB, 0).await;
        let before = log_count(&f, LADDER).await;
        // 201 prompt tokens + 16: past rung 2's 128, within rung 3's 512.
        let resp = chat_body(&f.gateway, ask.body(LADDER, &words("w", 200))).await;
        assert_eq!(resp.status(), 200, "{ask:?}");
        assert_eq!(
            header(&resp, "x-lmgw-rung"),
            Some("3/3; ctx=512; gguf=ladder-top.gguf"),
            "{ask:?}"
        );
        let chunks = chunks(&resp.text().await.unwrap());
        assert_usage_rule(&chunks, ask, (201, 1));
        assert_eq!(text_of(&chunks), served_by(ladder_view(&f).port), "{ask:?}");
        let row = newest_log(&f, LADDER, before).await;
        assert_eq!(row.rung, Some(3), "{ask:?}: {row:?}");
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens),
            (Some(201), Some(1)),
            "{ask:?}: {row:?}"
        );
    }
}
