//! The prompt count (ladder design §3.3, unified-KV design §3.3 step 2):
//! exact text tokens from the running server, plus an upper bound per media
//! part, since "neither counts media" (ladder design §2.1 fact 5) and a
//! ceiling has to come from somewhere else.
//!
//! Measured on this machine's llama-server image: `/tokenize`
//! with `parse_special` left at its default (`true`) equalled a real
//! completion's `usage.prompt_tokens` exactly; `parse_special: false`
//! overcounted by nearly double. So neither helper here ever sets it.
//!
//! Both helpers **do** set `add_special: true`, which `/tokenize` defaults to
//! `false`: the completion path itself tokenizes every prompt with
//! `tokenize_input_prompts(vocab, mctx, prompt, /*add_special*/ true,
//! /*parse_special*/ true)` (`server-context.cpp` — read in the b062ba735
//! checkout on this machine, and the same `true, true` arguments in 171e884's
//! own copy), while `/tokenize` reads `json_value(body, "add_special", false)`.
//! So a model whose vocab adds a BOS token gets one on the completion path and
//! must be counted with it. On a vocab that adds nothing the flag changes nothing — which is why
//! the measurement above, taken on such a model, could not tell the two
//! apart. An off-by-one under the pool's arithmetic is exactly the overflow
//! the ledger exists to prevent.
//!
//! **What counts as a transport failure.** Only a failure to reach the
//! container or read its answer is [`GatewayError::Transport`] — the shape the
//! gate hands to the dead-container recovery
//! ([`crate::vram::retry_dead_container`]). An HTTP error status or a
//! malformed answer is the container *answering*, so it is an
//! [`GatewayError::Upstream`] and never makes lmgw stop and restart a live
//! container.

use serde_json::{json, Value};

use crate::config::{LlamaParams, LocalModel};
use crate::error::GatewayError;
use crate::modelinfo::{self, ImageAttention, ImageTokens};

/// One request's counted prompt (ladder design §3.3 step 4's `tokens`, split
/// out so a caller — and a test — can see what each part contributed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCount {
    /// What `/tokenize` counted on the rendered chat prompt.
    pub text_tokens: u64,
    /// How many image parts the request carried.
    pub images: u64,
    /// The per-image token bound used for every one of them
    /// ([`image_token_bound`]'s `tokens`). `0` when the request carried none.
    pub per_image_bound: u64,
    /// `text_tokens + images * per_image_bound` — what the fit check compares
    /// against the per-request context (ladder design §3.3 step 4).
    /// Saturating: every term is client-controlled (a request's image count,
    /// a row's own `--image-max-tokens`), so an absurd one has to end in a
    /// clean refusal against the limit, never wrap below it or panic under
    /// overflow checks — the same rule as every other reservation sum.
    pub total: u64,
}

/// How many media parts a chat request carries, by kind — [`count_chat_prompt`]
/// reads only the counts, never the bytes, since counting is about bounding
/// tokens, not decoding media.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaParts {
    pub images: u64,
    pub audio: u64,
}

/// Count a request's media parts, by kind (§3.3).
pub fn media_parts(ir: &crate::ir::ChatRequest) -> MediaParts {
    let mut out = MediaParts::default();
    for m in &ir.messages {
        for p in &m.content {
            match p {
                crate::ir::ContentPart::Image { .. } => out.images += 1,
                crate::ir::ContentPart::Audio { .. } => out.audio += 1,
                _ => {}
            }
        }
    }
    out
}

/// A projector's per-image token ceiling, and why lmgw believes it
/// (ladder design §3.3 step 3, §4.3 rule 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBound {
    pub tokens: u64,
    pub basis: String,
}

/// One step of [`count_chat_prompt`], named so a transport failure or a
/// malformed response says which call it was that failed.
async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
    step: &str,
) -> Result<Value, GatewayError> {
    let resp = http.post(url).json(body).send().await.map_err(|e| {
        if e.is_timeout() {
            GatewayError::Timeout
        } else {
            GatewayError::Transport(format!("{step}: {e}"))
        }
    })?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Transport(format!("{step}: {e}")))?;
    if !status.is_success() {
        // Review finding 4: keep the container's own status. `/apply-template`
        // or `/tokenize` rejecting a request (a `400`, most commonly) is that
        // request's own fault, not lmgw's upstream trouble — the client must
        // see the same 4xx an unguarded row would give it, never a retryable
        // 502 that invites a pointless retry.
        return Err(answered(
            status.as_u16(),
            format!(
                "{step} failed ({status}): {}",
                String::from_utf8_lossy(&bytes)
            ),
        ));
    }
    // Past this point the container answered 2xx, so a malformed body from
    // here on is lmgw's own confusion about the shape, never the client's
    // fault — `502` regardless of the (successful) status above.
    serde_json::from_slice(&bytes).map_err(|e| answered(502, format!("{step}: invalid JSON: {e}")))
}

/// The container answered, but not with a count — see the module doc's "what
/// counts as a transport failure". `status` is what the client sees: the
/// container's own HTTP status when it gave one (review finding 4), else
/// `502` for an lmgw-side reading problem on an otherwise-successful response.
fn answered(status: u16, message: String) -> GatewayError {
    GatewayError::Upstream {
        status,
        provider_type: None,
        message,
    }
}

/// Count a chat-shaped prompt on the running server (ladder design §3.3 steps
/// 1–3): `POST /apply-template` with the exact body egress is about to send,
/// then `POST /tokenize` on the rendered prompt, at the llama-server *root*
/// (not under `/v1` — the same root `build_count_tokens`
/// (`egress/openai.rs`) already derives for its native `/tokenize` call).
///
/// **Decision (the owner's, binding): audio cannot be bounded in v1.** A
/// request with any audio part on a counted route is refused before either
/// HTTP call, since lmgw has no per-audio-part token ceiling to add. An image
/// request is refused the same way when `image_bound` is `None` — which
/// means either the caller found no bound (see [`image_token_bound`]'s `Err`,
/// which should be surfaced to the client directly and never reach here) or
/// simply passed none for a request that turned out to carry images anyway.
/// Both refusals are `400`s naming what to do about it, not a silent guess.
pub async fn count_chat_prompt(
    http: &reqwest::Client,
    server_root: &str,
    body: &Value,
    media: MediaParts,
    image_bound: Option<ImageBound>,
) -> Result<PromptCount, GatewayError> {
    let per_image_bound = media_bound(media, image_bound.as_ref())?;

    let template = post_json(
        http,
        &format!("{server_root}/apply-template"),
        body,
        "counting the prompt: apply-template",
    )
    .await?;
    let prompt = template
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            answered(
                502,
                "counting the prompt: apply-template response carried no 'prompt'".into(),
            )
        })?;

    let text_tokens = tokenize(http, server_root, &Value::String(prompt.to_string())).await?;

    Ok(PromptCount {
        text_tokens,
        images: media.images,
        per_image_bound,
        total: text_tokens.saturating_add(media.images.saturating_mul(per_image_bound)),
    })
}

/// The per-image token bound [`count_chat_prompt`] adds for each image part,
/// or its refusal of media it cannot bound — the checks that need no server,
/// so a ladder row (whose count runs beside the send, ladder design §12
/// entry 7) makes them before anything is sent.
pub(crate) fn media_bound(
    media: MediaParts,
    image_bound: Option<&ImageBound>,
) -> Result<u64, GatewayError> {
    if media.audio > 0 {
        return Err(GatewayError::Unsupported(
            "this request carries audio input, and lmgw has no per-audio-part token bound to \
             count it against (v1) — this model enforces a context guard (a ladder rung or a \
             guarded shared KV pool), which cannot promise the request fits without one; use a \
             model without a context guard for audio requests"
                .into(),
        ));
    }
    match (media.images, image_bound) {
        (0, _) => Ok(0),
        (_, Some(b)) => Ok(b.tokens),
        (_, None) => Err(GatewayError::BadRequest(
            "this request carries an image, and this model's per-image token bound is not \
             known — see the model's own problem (a projector needs a measured or configured \
             --image-max-tokens before a context guard can count against it), or use a model \
             without a context guard for images"
                .into(),
        )),
    }
}

/// Count one raw prompt for the legacy `/v1/completions` path: `/tokenize`
/// alone, since there is no chat template to render (ladder design §3.3,
/// "`POST /v1/count_tokens` on a ladder model counts on the running rung").
///
/// `content` is one prompt exactly as the completion path would read it — a
/// string, or a mixed array of strings and token ids. `/tokenize` feeds it
/// through the same `tokenize_mixed` the completion path uses, so the count
/// is the completion's own, BOS rule included (a BOS only where the prompt
/// starts with text).
pub async fn count_text_prompt(
    http: &reqwest::Client,
    server_root: &str,
    content: &Value,
) -> Result<u64, GatewayError> {
    tokenize(http, server_root, content).await
}

/// `POST /tokenize` with the completion path's own flags (module doc): the
/// server's `parse_special` default, and `add_special: true`.
async fn tokenize(
    http: &reqwest::Client,
    server_root: &str,
    content: &Value,
) -> Result<u64, GatewayError> {
    let tokenized = post_json(
        http,
        &format!("{server_root}/tokenize"),
        &json!({"content": content, "add_special": true}),
        "counting the prompt: tokenize",
    )
    .await?;
    tokenized
        .get("tokens")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .ok_or_else(|| {
            answered(
                502,
                "counting the prompt: tokenize response carried no 'tokens'".into(),
            )
        })
}

/// The parts of a chat row the per-image bound is read from, borrowed.
///
/// Not a `&LocalModel`, because the two callers hold different things: the
/// gate reads the row a container was **started** with
/// ([`super::facts::GateFacts`], second review, finding 1 — never the row as it has
/// been edited since), and the save-time advisory (`ops::model_warnings`)
/// reads the row being saved, which is not a `LocalModel` yet.
#[derive(Debug, Clone, Copy)]
pub struct ProjectorRow<'r> {
    pub model_id: &'r str,
    /// Relative to the models dir, like the row's own column.
    pub gguf_path: &'r str,
    pub params: &'r LlamaParams,
    pub args: &'r [String],
}

impl<'r> From<&'r LocalModel> for ProjectorRow<'r> {
    fn from(m: &'r LocalModel) -> Self {
        Self {
            model_id: &m.model_id,
            gguf_path: &m.gguf_path,
            params: &m.params,
            args: &m.args,
        }
    }
}

/// The per-image token bound a chat row's projector gives the fit check
/// (ladder design §3.3 step 3, §4.3 rule 7): `Ok(None)` when the row loads no
/// projector at all — there is nothing to bound. `Err(reason)` names the fix
/// when a projector is loaded and no bound can be had, the same refusal
/// §4.3's own validation uses at save time.
///
/// **Deliberately does not reuse [`modelinfo::projector_ubatch_floor`]'s
/// substitution of Gemma 4's measured ceiling for an unmeasured or unknown
/// projector.** That number is a *batch-size* heuristic (how big a physical
/// batch has to be so decoding a non-causal image never aborts) — it is not,
/// and was never meant to be, an upper bound on how many tokens an image can
/// cost. Using it as one here would silently under-count a request whose
/// projector is neither Gemma 3 nor Gemma 4, and let it through a fit check
/// that then still overflows. So the unmeasured/unknown cases below only ever
/// trust the row's own explicit `--image-max-tokens`, and refuse otherwise.
pub async fn image_token_bound(
    models_dir: &str,
    row: ProjectorRow<'_>,
) -> Result<Option<ImageBound>, String> {
    if !modelinfo::row_loads_projector(row.params, row.args) {
        return Ok(None);
    }

    // The conditional gemma4v case needs the *weights'* embedding width
    // (`row_image_attention`'s `text_width`), read the same way
    // `ops::model_warnings` does for the projector-batch advisory.
    let weights_path = std::path::Path::new(models_dir).join(row.gguf_path);
    let text_width = tokio::task::spawn_blocking(move || crate::gguf::summarize(&weights_path))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|s| s.embedding_length);

    let Some(attention) =
        modelinfo::row_image_attention(models_dir, row.params, row.args, text_width).await
    else {
        return Err(format!(
            "'{}' loads a projector file that is not on disk under the models dir — fix \
             mmproj_path (or the --mmproj argument) before this model can take images",
            row.model_id
        ));
    };

    let configured_bound = || {
        modelinfo::arg_value(row.args, &["image-max-tokens"]).and_then(|v| v.parse::<u64>().ok())
    };
    let need_configured_bound = |why: &str| {
        Err(format!(
            "'{}' loads a projector and lmgw has no per-image token ceiling for it ({why}) — \
             set --image-max-tokens in the row's args",
            row.model_id
        ))
    };

    match attention {
        // Every image is resized to one fixed size: the exact count, never
        // raised by --image-max-tokens (projector_ubatch_floor's own doc
        // comment: a fixed-size projector "ignores that flag").
        ImageAttention::NonCausal {
            ty,
            tokens: ImageTokens::Fixed(n),
        } => Ok(Some(ImageBound {
            tokens: n as u64,
            basis: format!(
                "every {ty} image is resized to one fixed size and comes to exactly \
                             {n} tokens"
            ),
        })),
        // Gemma 4's measured ceiling, raised by the row's own
        // --image-max-tokens when that is larger.
        ImageAttention::NonCausal {
            ty,
            tokens: ImageTokens::Measured(n),
        } => {
            let measured = n as u64;
            match configured_bound() {
                Some(b) if b > measured => Ok(Some(ImageBound {
                    tokens: b,
                    basis: format!(
                        "the row's own --image-max-tokens ({b}) raises {ty}'s measured ceiling \
                         of {measured}"
                    ),
                })),
                _ => Ok(Some(ImageBound {
                    tokens: measured,
                    basis: format!(
                        "{ty}'s measured ceiling: Gemma 4's images stop growing at about 1100 \
                         tokens (its own largest budget is 1120), plus a margin"
                    ),
                })),
            }
        }
        // Any other projector — variable-size and unmeasured, or causal, or
        // lmgw could not even tell — trusts only the row's own explicit
        // --image-max-tokens, never a substituted heuristic.
        ImageAttention::NonCausal {
            ty,
            tokens: ImageTokens::Unmeasured,
        } => match configured_bound() {
            Some(b) => Ok(Some(ImageBound {
                tokens: b,
                basis: format!(
                    "the row's own --image-max-tokens ({b}); {ty} has no measured \
                                 per-image ceiling of its own"
                ),
            })),
            None => need_configured_bound(&format!("{ty} has no measured per-image ceiling")),
        },
        ImageAttention::Causal => match configured_bound() {
            Some(b) => Ok(Some(ImageBound {
                tokens: b,
                basis: format!("the row's own --image-max-tokens ({b})"),
            })),
            None => need_configured_bound(
                "its projector's image-token cost is not fixed and \
                                            nothing bounds it",
            ),
        },
        ImageAttention::Unknown(why) => match configured_bound() {
            Some(b) => Ok(Some(ImageBound {
                tokens: b,
                basis: format!("the row's own --image-max-tokens ({b}); {why}"),
            })),
            None => need_configured_bound(&why),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn chat_body() -> Value {
        json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "one two three four five"},
            ],
        })
    }

    /// A minimal stand-in for the llama-server mock in
    /// `tests/it/vram_admission/containers.rs`: renders a deterministic prompt
    /// from the messages, then counts it by whitespace-separated words — enough
    /// to prove the two-call sequence and the arithmetic without pulling in the
    /// whole VRAM fixture.
    async fn server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apply-template"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let rendered = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["content"].as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(" ");
                ResponseTemplate::new(200).set_body_json(json!({"prompt": rendered}))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let n = body["content"]
                    .as_str()
                    .unwrap_or_default()
                    .split_whitespace()
                    .count();
                let tokens: Vec<u64> = (0..n as u64).collect();
                ResponseTemplate::new(200).set_body_json(json!({"tokens": tokens}))
            })
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_text_only_prompt_counts_its_words() {
        let s = server().await;
        let http = reqwest::Client::new();
        let count = count_chat_prompt(&http, &s.uri(), &chat_body(), MediaParts::default(), None)
            .await
            .unwrap();
        assert_eq!(count.text_tokens, 5);
        assert_eq!(count.images, 0);
        assert_eq!(count.total, 5);
    }

    #[tokio::test]
    async fn images_add_their_bound_times_their_count() {
        let s = server().await;
        let http = reqwest::Client::new();
        let bound = ImageBound {
            tokens: 300,
            basis: "test".into(),
        };
        let media = MediaParts {
            images: 2,
            ..Default::default()
        };
        let count = count_chat_prompt(&http, &s.uri(), &chat_body(), media, Some(bound))
            .await
            .unwrap();
        assert_eq!(count.text_tokens, 5);
        assert_eq!(count.per_image_bound, 300);
        assert_eq!(count.total, 5 + 300 * 2);
    }

    /// Review finding 12/7: an absurd image count times an absurd configured
    /// bound saturates — a clean refusal against the per-request limit
    /// follows, never a wrap below it or an overflow panic.
    #[tokio::test]
    async fn an_absurd_image_bound_saturates_instead_of_wrapping() {
        let s = server().await;
        let http = reqwest::Client::new();
        let bound = ImageBound {
            tokens: u64::MAX / 2,
            basis: "test".into(),
        };
        let media = MediaParts {
            images: 3,
            ..Default::default()
        };
        let count = count_chat_prompt(&http, &s.uri(), &chat_body(), media, Some(bound))
            .await
            .unwrap();
        assert_eq!(count.total, u64::MAX);
    }

    #[tokio::test]
    async fn audio_is_refused_before_any_http_call() {
        let s = server().await;
        let http = reqwest::Client::new();
        let media = MediaParts {
            audio: 1,
            ..Default::default()
        };
        let err = count_chat_prompt(&http, &s.uri(), &chat_body(), media, None)
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::Unsupported(_)));
    }

    #[tokio::test]
    async fn an_unknown_image_bound_is_refused() {
        let s = server().await;
        let http = reqwest::Client::new();
        let media = MediaParts {
            images: 1,
            ..Default::default()
        };
        let err = count_chat_prompt(&http, &s.uri(), &chat_body(), media, None)
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::BadRequest(_)));
    }

    /// Review finding 4: a rejection from `/apply-template` (a `400`, the
    /// shape a bad request actually gets) must reach the caller as that same
    /// `400`, not the hardcoded `502` a transport failure would carry — a
    /// retryable 502 would tell a client to retry a request that can never
    /// succeed.
    #[tokio::test]
    async fn an_apply_template_rejection_keeps_its_own_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apply-template"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error": "context too long"})),
            )
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let err = count_chat_prompt(
            &http,
            &server.uri(),
            &chat_body(),
            MediaParts::default(),
            None,
        )
        .await
        .unwrap_err();
        match &err {
            GatewayError::Upstream { status, .. } => assert_eq!(*status, 400, "{err:?}"),
            other => panic!("expected Upstream{{status: 400, ..}}, got {other:?}"),
        }
        assert_eq!(
            err.http_status().as_u16(),
            400,
            "the client must see the same 4xx an unguarded row would get, not a retryable 502"
        );
    }

    #[tokio::test]
    async fn count_text_prompt_is_tokenize_only() {
        let s = server().await;
        let http = reqwest::Client::new();
        let n = count_text_prompt(&http, &s.uri(), &json!("a b c"))
            .await
            .unwrap();
        assert_eq!(n, 3);
    }

    // -----------------------------------------------------------------------
    // image_token_bound
    // -----------------------------------------------------------------------

    fn local_model(gguf_path: &str, mmproj_path: Option<&str>, args: Vec<String>) -> LocalModel {
        LocalModel {
            id: 1,
            model_id: "m".into(),
            gguf_path: gguf_path.into(),
            params: crate::config::LlamaParams {
                mmproj_path: mmproj_path.map(String::from),
                ..Default::default()
            },
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
            ladder: vec![],
        }
    }

    /// A models dir with a plain (non-mmproj) weights file at `weights.gguf`,
    /// so `text_width` reads `None` — irrelevant to every case below, since
    /// none of them uses the conditional `gemma4v` (only its unconditional
    /// `gemma4uv` sibling).
    fn models_dir_with_weights() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::gguf::synth::chat("qwen35", 4096).write_to(&dir.path().join("weights.gguf"));
        dir
    }

    #[tokio::test]
    async fn a_row_with_no_projector_has_no_bound_to_report() {
        let dir = models_dir_with_weights();
        let row = local_model("weights.gguf", None, vec![]);
        let bound = image_token_bound(&dir.path().display().to_string(), (&row).into())
            .await
            .unwrap();
        assert_eq!(bound, None);
    }

    #[tokio::test]
    async fn a_fixed_size_projector_gives_its_exact_computed_count() {
        let dir = models_dir_with_weights();
        let mut h = crate::gguf::synth::Header::default();
        h.str("clip.projector_type", "gemma3")
            .u32("clip.vision.image_size", 896)
            .u32("clip.vision.patch_size", 14)
            .u32("clip.vision.projector.scale_factor", 4);
        h.write_to(&dir.path().join("proj.gguf"));

        let row = local_model("weights.gguf", Some("proj.gguf"), vec![]);
        let bound = image_token_bound(&dir.path().display().to_string(), (&row).into())
            .await
            .unwrap()
            .expect("gemma3 has a computable fixed size");
        // (896 / 14)^2 / 4^2 = 64^2 / 16 = 256.
        assert_eq!(bound.tokens, 256);

        // A fixed-size projector ignores --image-max-tokens (it is exact,
        // never a heuristic to raise).
        let row_with_arg = local_model(
            "weights.gguf",
            Some("proj.gguf"),
            vec!["--image-max-tokens".into(), "9999".into()],
        );
        let bound2 = image_token_bound(&dir.path().display().to_string(), (&row_with_arg).into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bound2.tokens, 256,
            "a fixed size is never raised by the flag"
        );
    }

    #[tokio::test]
    async fn gemma4s_measured_ceiling_is_raised_only_when_the_rows_own_flag_is_larger() {
        let dir = models_dir_with_weights();
        let mut h = crate::gguf::synth::Header::default();
        h.str("clip.projector_type", "gemma4uv");
        h.write_to(&dir.path().join("proj.gguf"));
        let models_dir = dir.path().display().to_string();

        let unset = local_model("weights.gguf", Some("proj.gguf"), vec![]);
        let bound = image_token_bound(&models_dir, (&unset).into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bound.tokens, 1280,
            "Gemma 4's measured ceiling (GEMMA4_IMAGE_UBATCH)"
        );

        let smaller_arg = local_model(
            "weights.gguf",
            Some("proj.gguf"),
            vec!["--image-max-tokens".into(), "500".into()],
        );
        let bound = image_token_bound(&models_dir, (&smaller_arg).into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bound.tokens, 1280,
            "a smaller flag never lowers the measured ceiling"
        );

        let larger_arg = local_model(
            "weights.gguf",
            Some("proj.gguf"),
            vec!["--image-max-tokens".into(), "2000".into()],
        );
        let bound = image_token_bound(&models_dir, (&larger_arg).into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bound.tokens, 2000,
            "a larger flag raises the measured ceiling"
        );
    }

    #[tokio::test]
    async fn an_unmeasured_non_causal_projector_trusts_only_the_rows_own_flag() {
        let dir = models_dir_with_weights();
        let mut h = crate::gguf::synth::Header::default();
        // Non-causal (NON_CAUSAL_PROJECTORS), but not gemma3/gemma4v/gemma4uv —
        // `image_tokens` has no formula for it, so it is Unmeasured.
        h.str("clip.projector_type", "deepseek4v");
        h.write_to(&dir.path().join("proj.gguf"));
        let models_dir = dir.path().display().to_string();

        let no_flag = local_model("weights.gguf", Some("proj.gguf"), vec![]);
        let err = image_token_bound(&models_dir, (&no_flag).into())
            .await
            .unwrap_err();
        assert!(err.contains("--image-max-tokens"), "names the fix: {err}");

        let with_flag = local_model(
            "weights.gguf",
            Some("proj.gguf"),
            vec!["--image-max-tokens".into(), "900".into()],
        );
        let bound = image_token_bound(&models_dir, (&with_flag).into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound.tokens, 900);
    }

    #[tokio::test]
    async fn an_unknown_projector_type_also_trusts_only_the_rows_own_flag() {
        let dir = models_dir_with_weights();
        let mut h = crate::gguf::synth::Header::default();
        h.str("clip.projector_type", "lmgw-test-unknown-projector");
        h.write_to(&dir.path().join("proj.gguf"));
        let models_dir = dir.path().display().to_string();

        let no_flag = local_model("weights.gguf", Some("proj.gguf"), vec![]);
        assert!(image_token_bound(&models_dir, (&no_flag).into())
            .await
            .is_err());

        let with_flag = local_model(
            "weights.gguf",
            Some("proj.gguf"),
            vec!["--image-max-tokens".into(), "777".into()],
        );
        let bound = image_token_bound(&models_dir, (&with_flag).into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound.tokens, 777);
    }

    #[tokio::test]
    async fn a_missing_projector_file_is_a_named_error_not_a_panic() {
        let dir = models_dir_with_weights();
        let row = local_model("weights.gguf", Some("nowhere.gguf"), vec![]);
        let err = image_token_bound(&dir.path().display().to_string(), (&row).into())
            .await
            .unwrap_err();
        assert!(
            err.contains("mmproj_path") || err.contains("not on disk"),
            "{err}"
        );
    }
}
