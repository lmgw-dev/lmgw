//! The fake containers

use super::*;

/// Hold a request's transient ([`Until`]): until a sampler has read the
/// driver after it came on, or for its time.
///
/// Two more samplers' reads than when it came on: the first may have begun
/// before the transient did and been counted after (a read and its count are
/// not one step), while the second began after the first ended.
///
/// A sampler that never comes is waited for as long as the test may wait
/// ([`common::patience`]): under nextest the harness's timeout ends the test
/// first, and what the hold waited for is on its stderr; under plain `cargo
/// test` the transient ends with the budget, and the test's own assertion
/// fails.
fn hold(world: &Arc<Mutex<World>>, until: Until) {
    match until {
        Until::For(lasts) => std::thread::sleep(lasts),
        Until::Sampled => {
            let samples = world
                .lock()
                .unwrap()
                .samples
                .clone()
                .expect("the fixture counts the samplers' reads");
            let enough = samples() + 2;
            let mut wait = common::patience::Wait::new("a sampler reads the transient");
            loop {
                let read = samples();
                let seen = format_args!("{read} samplers' reads, {enough} needed");
                if read >= enough || !wait.goes_on(Some(&seen)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// One model's container: `/health` for the readiness poll (§10.3), `/slots`
/// for the eviction busy probe (§10.7), and the inference routes a forwarded
/// request lands on.
///
/// `/slots` answers about whichever model the fake podman put on *this* port,
/// so a test states busyness per model and never has to know which container a
/// model was assigned.
pub(super) async fn container(world: Arc<Mutex<World>>) -> MockServer {
    // A *bare* server on a listener of its own, not `MockServer::start()`'s
    // pooled one: a pooled server survives its handle being dropped (it goes
    // back to the pool and keeps listening), and one of these containers has
    // to be killable — see `Fixture::kill_first_container`.
    let server = MockServer::builder()
        .listener(std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .start()
        .await;
    let port = server.address().port();

    let w = world.clone();
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(move |_: &Request| {
            // Still loading while the test holds it (`World::health_held`).
            if w.lock().unwrap().health_held {
                return ResponseTemplate::new(503)
                    .set_body_string(r#"{"error":{"message":"Loading model"}}"#);
            }
            ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#)
        })
        .mount(&server)
        .await;

    let w = world.clone();
    Mock::given(method("GET"))
        .and(path("/slots"))
        .respond_with(move |_: &Request| {
            let mut w = w.lock().unwrap();
            *w.slots_calls.entry(port).or_insert(0) += 1;
            let busy = w
                .ports
                .get(&port)
                .is_some_and(|model| w.busy.contains(model));
            // The shape llama-server actually serves (measured 2026-09-17 on
            // `official-latest`): `id_task` is present on an *idle* slot too,
            // carrying the id of the task it last finished — the server keeps
            // `task_prev` and `to_json` falls back to it. Only a slot that has
            // never run a task omits the key, and every victim these tests
            // evict has served at least one request. An idle mock without the
            // field would pass a probe that reads "has a task id" as "busy",
            // which is exactly the bug that let an idle model sit on the card
            // through a whole queue timeout.
            ResponseTemplate::new(200).set_body_json(json!([{
                "id": 0,
                "is_processing": busy,
                "id_task": 0,
            }]))
        })
        .mount(&server)
        .await;

    // The image class's readiness route *is* its capabilities route (§3), so
    // one 200 here is both "the pipeline is up" and "this is what it can do".
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "supported_modes": ["img_gen"],
            "limits": {"min_width": 64, "max_width": 4096},
        })))
        .mount(&server)
        .await;

    // An audio.cpp container (realtime design §9.4): its readiness route,
    // which answers before any model is loaded, and the two inference routes,
    // whose first answer is what loads the model — the driver's figure for
    // this port's model grows to `World::loaded_bytes` the moment one runs.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    for (route, wav) in [
        ("/v1/audio/speech", true),
        ("/v1/audio/transcriptions", false),
    ] {
        let w = world.clone();
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(move |req: &Request| {
                let world = w.clone();
                // `stream_format: audio` is answered with raw PCM, chunked
                // (audio-class gap 8).
                let raw_pcm = serde_json::from_slice::<Value>(&req.body)
                    .ok()
                    .is_some_and(|b| b["stream_format"] == "audio");
                let mut w = w.lock().unwrap();
                let model = w.ports.get(&port).cloned().unwrap_or_default();
                let loaded = w.loaded_bytes.get(&model).copied();
                if let Some(bytes) = loaded {
                    w.size.insert(model.clone(), bytes);
                }
                w.audio_calls.push(model.clone());
                let delay = Duration::ZERO;
                if let Some(&t) = w.transient.get(&model) {
                    // Held, then dropped back before the answer leaves:
                    // this blocks the fake container's own server, which
                    // nothing else needs meanwhile (the driver is read in
                    // process).
                    w.size.insert(model.clone(), t.bytes);
                    let back = loaded.unwrap_or(t.bytes);
                    drop(w);
                    hold(&world, t.until);
                    w = world.lock().unwrap();
                    w.size.insert(model.clone(), back);
                }
                if let Some(&n) = w.sse_bytes.get(&model).filter(|_| wav && raw_pcm) {
                    return ResponseTemplate::new(200)
                        .insert_header("content-type", "application/octet-stream")
                        .set_body_raw(vec![0u8; n], "application/octet-stream")
                        .set_delay(delay);
                }
                if let Some(&n) = w.sse_bytes.get(&model).filter(|_| wav) {
                    let frame = "data: {\"type\":\"speech.audio.delta\",\"audio\":\"AAAA\"}\n\n";
                    let body = frame.repeat(n / frame.len() + 1).into_bytes();
                    return ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_raw(body, "text/event-stream")
                        .set_delay(delay);
                }
                if wav {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "audio/wav")
                        .set_body_bytes(crate::support::realtime_tts::wav(
                            &crate::support::realtime_tts::speech(100),
                            24_000,
                        ))
                        .set_delay(delay)
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"text": "hello"}))
                        .set_delay(delay)
                }
            })
            .mount(&server)
            .await;
    }

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.5, 0.5]}],
            "usage": {"prompt_tokens": 3, "total_tokens": 3},
        })))
        .mount(&server)
        .await;

    // `/apply-template` + `/tokenize` (ladder design §3.3 / unified-KV design
    // §3.3 step 2): a deterministic stand-in for llama-server's real chat
    // template render and BPE count, so a test can craft a prompt of known
    // size just by choosing how many words it writes. `parse_special` is
    // never read here — the mock's whole vocabulary is "one token per
    // whitespace-separated word" regardless of what the request asked for,
    // matching the *shape* of the real endpoints without the real tokenizer.
    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/apply-template"))
        .respond_with(move |req: &Request| {
            *w.lock()
                .unwrap()
                .apply_template_calls
                .entry(port)
                .or_insert(0) += 1;
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let prompt = render_prompt(&body);
            let delay = {
                let w = w.lock().unwrap();
                let by_tag = w
                    .template_delay_by_tag
                    .iter()
                    .filter(|(tag, _)| prompt.split_whitespace().any(|word| word.starts_with(tag)))
                    .map(|(_, d)| *d);
                w.apply_template_delay
                    .get(&port)
                    .copied()
                    .into_iter()
                    .chain(by_tag)
                    .max()
                    .unwrap_or_default()
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"prompt": prompt}))
                .set_delay(delay)
        })
        .mount(&server)
        .await;

    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(move |req: &Request| {
            let undercount = {
                let mut w = w.lock().unwrap();
                *w.tokenize_calls.entry(port).or_insert(0) += 1;
                w.tokenize_undercount
            };
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let content = body
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let counted = (content.split_whitespace().count() as u64).saturating_sub(undercount);
            let tokens: Vec<u64> = (0..counted).collect();
            ResponseTemplate::new(200).set_body_json(json!({"tokens": tokens}))
        })
        .mount(&server)
        .await;

    // `/v1/chat/completions`: the plain body every existing test asserts on,
    // unless this port is in unified-pool mode (`World::pool_capacity`) — set
    // by a test that wants the mock itself to enforce fact 3's shared-pool
    // overflow. Every call is recorded into `chat_bodies` either way, so a
    // test can assert the `max_tokens` llama-server actually received.
    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let mut w = w.lock().unwrap();
            w.chat_bodies.entry(port).or_default().push(body.clone());
            let delay = w.chat_delay.get(&port).copied().unwrap_or_default();
            if let Some(&slot) = w.slot_ctx.get(&port) {
                let prompt = render_prompt(&body).split_whitespace().count() as u64;
                return context_rules(&mut w, port, slot, prompt, &body).set_delay(delay);
            }

            let ok = ResponseTemplate::new(200).set_body_json(json!({
                "id": "c1", "object": "chat.completion", "created": 1, "model": "chat-model",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }));

            let Some(&capacity) = w.pool_capacity.get(&port) else {
                return ok.set_delay(delay);
            };
            let need = chat_need(&body);
            let delay = w
                .pool_delay
                .get(&port)
                .copied()
                .unwrap_or(Duration::from_millis(200));
            let now = std::time::Instant::now();
            let windows = w.pool_windows.entry(port).or_default();
            windows.retain(|(_, end, _)| *end > now);
            let in_flight: u64 = windows.iter().map(|(_, _, n)| *n).sum();
            if in_flight + need > capacity {
                w.pool_overflows += 1;
                let streamed = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
                return if streamed {
                    // Fact 3, streaming shape: HTTP 200, one SSE error event,
                    // stream closes with no finish_reason.
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_raw(
                            "data: {\"error\":{\"code\":500,\"message\":\"Context size has \
                             been exceeded.\",\"type\":\"server_error\"}}\n\n"
                                .as_bytes()
                                .to_vec(),
                            "text/event-stream",
                        )
                } else {
                    // Fact 3, non-streaming shape: HTTP 500, the same body.
                    ResponseTemplate::new(500).set_body_json(json!({
                        "error": {
                            "code": 500,
                            "message": "Context size has been exceeded.",
                            "type": "server_error",
                        }
                    }))
                };
            }
            w.pool_windows
                .entry(port)
                .or_default()
                .push((now, now + delay, need));
            w.pool_served
                .entry(port)
                .or_default()
                .push((now, now + delay, need));
            ok.set_delay(delay)
        })
        .mount(&server)
        .await;

    // `/v1/completions`, for the ports that play llama-server's context rules
    // (`World::slot_ctx`): the legacy path's prompt is counted as its words.
    // The lowest priority, so a test that mounts its own answer still gets it,
    // and a 404 elsewhere — what an unmatched route answered before.
    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let mut w = w.lock().unwrap();
            w.chat_bodies.entry(port).or_default().push(body.clone());
            let Some(&slot) = w.slot_ctx.get(&port) else {
                return ResponseTemplate::new(404);
            };
            let prompt = body
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split_whitespace()
                .count() as u64;
            context_rules(&mut w, port, slot, prompt, &body)
        })
        .with_priority(u8::MAX)
        .mount(&server)
        .await;

    server
}

/// llama-server's context rules on a port started with a known per-slot
/// context (ladder design §2.1): a prompt longer than the slot is refused
/// before any work with `exceed_context_size_error` — a plain JSON 400,
/// streaming or not, exactly the shape WP0 measured — and a prompt whose
/// `max_tokens` would overrun the slot is answered, truncated. The answer
/// names the port that gave it, in both shapes, and on the legacy path too.
fn context_rules(
    w: &mut World,
    port: u16,
    slot: u64,
    prompt: u64,
    body: &Value,
) -> ResponseTemplate {
    if prompt > slot {
        w.exceeded += 1;
        return ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": 400,
                "message": format!(
                    "request ({prompt} tokens) exceeds the available context size ({slot} \
                     tokens), try increasing it"
                ),
                "type": "exceed_context_size_error",
                "n_prompt_tokens": prompt,
                "n_ctx": slot,
            }
        }));
    }
    let max_tokens = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
    let finish = if prompt + max_tokens > slot {
        w.truncations += 1;
        "length"
    } else {
        "stop"
    };
    let text = format!("served by port {port}");
    if body.get("prompt").is_some() {
        return ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "text_completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "text": text, "finish_reason": finish}],
            "usage": {"prompt_tokens": prompt, "completion_tokens": 1, "total_tokens": prompt + 1},
        }));
    }
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let mut frames = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
        );
        // llama-server's usage chunk, `choices: []` after the finish, comes
        // only when the request asks for it — as the gateway always does.
        if body.pointer("/stream_options/include_usage") == Some(&json!(true)) {
            let usage = json!({"prompt_tokens": prompt, "completion_tokens": 1,
                               "total_tokens": prompt + 1});
            frames.push_str(&format!(
                "data: {}\n\n",
                json!({"choices": [], "usage": usage})
            ));
        }
        frames.push_str("data: [DONE]\n\n");
        return ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(frames.into_bytes(), "text/event-stream");
    }
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": finish}],
        "usage": {"prompt_tokens": prompt, "completion_tokens": 1, "total_tokens": prompt + 1},
    }))
}

/// A deterministic stand-in for llama-server's chat-template render: every
/// message becomes `"role: content"`, joined by newlines — enough that
/// `/tokenize`'s word count is exactly what a test expects from the messages
/// it wrote, without needing a real Jinja template.
fn render_prompt(body: &Value) -> String {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter()
                .map(|m| {
                    let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
                    let content = m.get("content").and_then(Value::as_str).unwrap_or_default();
                    format!("{role}: {content}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// One request's reservation, the same arithmetic the unified-KV pool ledger
/// will use (design §3.3 step 3): the rendered prompt's word count plus
/// `max_tokens`, or a small stand-in when the request set none — the mock's
/// job is to prove the overlap math, not to model lmgw's own clamp/default.
fn chat_need(body: &Value) -> u64 {
    let words = render_prompt(body).split_whitespace().count() as u64;
    let max_tokens = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(16);
    words + max_tokens
}
