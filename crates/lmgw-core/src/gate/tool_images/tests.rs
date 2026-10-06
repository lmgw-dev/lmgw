//! The predicate condition by condition, and `tool_media`.

use super::*;
use crate::ir::{Message, Params};

/// Every condition holding: a server with vision on an unguarded external
/// row, for a request that did not come through a candidate alias.
fn holding() -> ToolImageInputs<'static> {
    ToolImageInputs {
        vision: Some(true),
        ..ToolImageInputs::default()
    }
}

fn refused(why: &str) -> ToolImages {
    ToolImages::Refused(why.into())
}

#[test]
fn every_condition_holding_allows_tool_images() {
    assert_eq!(tool_image_predicate(&holding()), ToolImages::Allowed);
    assert_eq!(
        tool_image_predicate(&ToolImageInputs {
            guarded_bound_known: Some(true),
            candidate_vision: Some(true),
            ..holding()
        }),
        ToolImages::Allowed
    );
}

#[test]
fn a_server_without_vision_refuses() {
    let inputs = ToolImageInputs {
        vision: Some(false),
        ..holding()
    };
    assert_eq!(tool_image_predicate(&inputs), refused(NO_VISION));
    assert_eq!(NO_VISION, "this model's server has no vision");
}

/// A server that did not say whether it sees is today's bytes, with no
/// reason of its own (decision 14) — whatever else holds or fails.
#[test]
fn unknown_vision_is_today() {
    assert_eq!(
        tool_image_predicate(&ToolImageInputs::default()),
        ToolImages::Unknown
    );
    let otherwise_failing = ToolImageInputs {
        vision: None,
        ubatch_advisory: Some("advisory"),
        unread_projector_short: true,
        guarded_bound_known: Some(false),
        candidate_vision: Some(false),
    };
    assert_eq!(
        tool_image_predicate(&otherwise_failing),
        ToolImages::Unknown
    );
    assert_eq!(ToolImages::Unknown.refusal(), None);
    assert!(!ToolImages::Unknown.allowed());
}

#[test]
fn a_projector_ubatch_advisory_refuses() {
    let inputs = ToolImageInputs {
        ubatch_advisory: Some("llama.cpp decodes this model's images (gemma4v projector) …"),
        ..holding()
    };
    assert_eq!(tool_image_predicate(&inputs), refused(PROJECTOR_CAN_ABORT));
    assert_eq!(
        PROJECTOR_CAN_ABORT,
        "its projector can abort above the batch size"
    );
}

#[test]
fn a_projector_lmgw_cannot_read_refuses_below_an_images_batch() {
    let inputs = ToolImageInputs {
        unread_projector_short: true,
        ..holding()
    };
    assert_eq!(tool_image_predicate(&inputs), refused(PROJECTOR_UNREAD));
}

/// The started row of a managed chat container, with `params` and `args`.
fn started(params: crate::config::LlamaParams, args: &[&str]) -> GateFacts {
    GateFacts {
        model_id: "m".into(),
        models_dir: "/models".into(),
        gguf_path: "m.gguf".into(),
        params,
        args: args.iter().map(|a| a.to_string()).collect(),
        trained_context: None,
        rung: None,
    }
}

/// A projector lmgw never sees (`--mmproj-url`, `-hf`'s own): an unknown
/// projector needs Gemma 4's measured image ubatch in both batch sizes, or
/// the row's own `--image-max-tokens` when larger. A row whose projector
/// lmgw reads is its advisory's business, not this one's.
#[test]
fn an_unread_projector_needs_the_batch_an_unknown_one_needs() {
    use crate::config::LlamaParams;
    use crate::modelinfo::GEMMA4_IMAGE_UBATCH;
    let big = |b: i64| LlamaParams {
        batch_size: Some(b),
        ubatch_size: Some(b),
        ..LlamaParams::default()
    };
    let url = ["--mmproj-url", "https://example.org/mmproj.gguf"];
    let hf = ["-hf", "org/model-GGUF"];
    // llama.cpp's defaults (2048 and 512): the ubatch is short.
    assert!(unread_projector_short(&started(
        LlamaParams::default(),
        &url
    )));
    assert!(unread_projector_short(&started(
        LlamaParams::default(),
        &hf
    )));
    assert!(!unread_projector_short(&started(
        big(GEMMA4_IMAGE_UBATCH),
        &url
    )));
    assert!(unread_projector_short(&started(
        big(GEMMA4_IMAGE_UBATCH - 1),
        &hf
    )));
    // Only the ubatch big enough: the batch still splits an image.
    let ubatch_only = LlamaParams {
        batch_size: Some(512),
        ubatch_size: Some(4096),
        ..LlamaParams::default()
    };
    assert!(unread_projector_short(&started(ubatch_only, &url)));
    // The row's own image budget above Gemma 4's.
    let budget = ["--mmproj-url", "u", "--image-max-tokens", "4096"];
    assert!(unread_projector_short(&started(big(2048), &budget)));
    assert!(!unread_projector_short(&started(big(4096), &budget)));
    // A projector lmgw reads: judged by its advisory instead.
    let named = LlamaParams {
        mmproj_path: Some("m-mmproj.gguf".into()),
        ..LlamaParams::default()
    };
    assert!(!unread_projector_short(&started(named, &[])));
    assert!(!unread_projector_short(&started(
        LlamaParams::default(),
        &["--mmproj", "/models/p.gguf"]
    )));
}

#[test]
fn a_guarded_row_without_a_per_image_bound_refuses() {
    let inputs = ToolImageInputs {
        guarded_bound_known: Some(false),
        ..holding()
    };
    assert_eq!(tool_image_predicate(&inputs), refused(NO_IMAGE_BOUND));
    assert_eq!(NO_IMAGE_BOUND, "no per-image bound");
}

#[test]
fn a_candidate_alias_without_vision_refuses() {
    let inputs = ToolImageInputs {
        candidate_vision: Some(false),
        ..holding()
    };
    assert_eq!(
        tool_image_predicate(&inputs),
        refused(CANDIDATE_WITHOUT_VISION)
    );
}

/// The first failing condition names the refusal, in the spec's order.
#[test]
fn the_first_failing_condition_is_the_reason() {
    let all_failing = ToolImageInputs {
        vision: Some(false),
        ubatch_advisory: Some("advisory"),
        unread_projector_short: true,
        guarded_bound_known: Some(false),
        candidate_vision: Some(false),
    };
    assert_eq!(tool_image_predicate(&all_failing), refused(NO_VISION));
    let from_advisory = ToolImageInputs {
        vision: Some(true),
        ..all_failing
    };
    assert_eq!(
        tool_image_predicate(&from_advisory),
        refused(PROJECTOR_CAN_ABORT)
    );
    let from_unread = ToolImageInputs {
        ubatch_advisory: None,
        ..from_advisory
    };
    assert_eq!(
        tool_image_predicate(&from_unread),
        refused(PROJECTOR_UNREAD)
    );
    let from_bound = ToolImageInputs {
        unread_projector_short: false,
        ..from_unread
    };
    assert_eq!(tool_image_predicate(&from_bound), refused(NO_IMAGE_BOUND));
    let from_candidate = ToolImageInputs {
        guarded_bound_known: Some(true),
        ..from_bound
    };
    assert_eq!(
        tool_image_predicate(&from_candidate),
        refused(CANDIDATE_WITHOUT_VISION)
    );
}

/// A whole 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
/// A whole 1x1 lossless WebP.
const WEBP: &str = "UklGRiAAAABXRUJQVlA4TBQAAAAvAAAAAAdQgVQIIAAKmv7HiIj+Bw==";

fn image(mime: &str, data: &str) -> ToolResultBlock {
    ToolResultBlock::Image {
        mime: mime.into(),
        data: data.into(),
    }
}

fn tool_result(id: &str, content: Vec<ToolResultBlock>) -> ContentPart {
    ContentPart::ToolResult {
        id: id.into(),
        name: None,
        content,
        is_error: false,
    }
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model_alias: "m".into(),
        messages,
        params: Params::default(),
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

#[test]
fn tool_media_counts_the_tool_images_that_pass_the_format_check() {
    let ir = request(vec![
        Message {
            role: Role::User,
            content: vec![
                ContentPart::text("look"),
                // User media is `media_parts`' business.
                ContentPart::Image {
                    mime: "image/png".into(),
                    source: crate::ir::ImageSource::Base64 { data: PNG.into() },
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![
                tool_result(
                    "a",
                    vec![
                        ToolResultBlock::text("chart:"),
                        image("image/png", PNG),
                        image("image/svg+xml", "PHN2Zy8+"),
                        image("image/webp", WEBP),
                        ToolResultBlock::Audio {
                            mime: "audio/wav".into(),
                            data: "UklGRg==".into(),
                        },
                    ],
                ),
                tool_result(
                    "b",
                    vec![
                        image("image/png", PNG),
                        image("image/png", ""),
                        // The signature alone: stb_image cannot decode it.
                        image("image/png", "iVBORw0KGgo="),
                    ],
                ),
            ],
        },
    ]);
    assert_eq!(tool_media(&ir, false), 2);
    assert_eq!(tool_media(&ir, true), 3);
    assert_eq!(crate::gate::media_parts(&ir).images, 1);
}

/// A tool result outside a `tool` message is never rendered as one, so it is
/// never counted as one.
#[test]
fn tool_media_reads_only_tool_messages() {
    let ir = request(vec![Message {
        role: Role::User,
        content: vec![tool_result("a", vec![image("image/png", PNG)])],
    }]);
    assert_eq!(tool_media(&ir, true), 0);
    assert_eq!(tool_media(&request(Vec::new()), true), 0);
}

// ---------------------------------------------------------------------------
// The per-attempt recheck
// ---------------------------------------------------------------------------

fn facts(
    vision: Option<bool>,
    video: Option<bool>,
) -> Arc<crate::egress::llama_cpp::props::LlamaFacts> {
    Arc::new(crate::egress::llama_cpp::props::LlamaFacts {
        vision,
        video,
        ..Default::default()
    })
}

fn allowed_on(f: &Arc<crate::egress::llama_cpp::props::LlamaFacts>) -> LlamaRoute {
    LlamaRoute {
        facts: f.clone(),
        tool_images: ToolImages::Allowed,
    }
}

fn entry(
    f: Option<Arc<crate::egress::llama_cpp::props::LlamaFacts>>,
    advisory: Option<&str>,
) -> LlamaEntry {
    LlamaEntry {
        props: f.ok_or_else(|| "GET /props could not be read".to_string()),
        ubatch_advisory: advisory.map(str::to_string),
    }
}

/// The verdict a recheck leaves on the route: `None` when the decision
/// stands, else the route's new `tool_images` (`None` for today's bytes).
fn verdict(
    decided: &LlamaRoute,
    on: Option<&LlamaEntry>,
    started: Option<&GateFacts>,
) -> Option<Option<ToolImages>> {
    rechecked(decided, on, started).map(|now| now.map(|l| l.tool_images.clone()))
}

/// The container an attempt goes to still sees, with no advisory: the
/// decision stands, also on another container's equal facts.
#[test]
fn a_recheck_on_a_container_that_still_sees_leaves_the_decision() {
    let seen = facts(Some(true), None);
    let decided = allowed_on(&seen);
    assert_eq!(
        verdict(&decided, Some(&entry(Some(seen), None)), None),
        None
    );
    let other = facts(Some(true), Some(false));
    assert_eq!(
        verdict(&decided, Some(&entry(Some(other), None)), None),
        None
    );
}

/// Every way the container an attempt goes to cannot take the image turns
/// the decision into that reason, or into today's bytes where it says
/// nothing: it does not see, it carries an advisory, it reads no projector
/// lmgw can read at a short batch, its facts are unknown, its entry is gone.
#[test]
fn a_recheck_only_takes_away() {
    let decided = allowed_on(&facts(Some(true), None));
    let refused_with = |why: &str| Some(Some(refused(why)));
    assert_eq!(
        verdict(
            &decided,
            Some(&entry(Some(facts(Some(false), None)), None)),
            None
        ),
        refused_with(NO_VISION)
    );
    assert_eq!(
        verdict(
            &decided,
            Some(&entry(Some(facts(Some(true), None)), Some("advisory"))),
            None
        ),
        refused_with(PROJECTOR_CAN_ABORT)
    );
    let url = started(Default::default(), &["--mmproj-url", "u"]);
    assert_eq!(
        verdict(
            &decided,
            Some(&entry(Some(facts(Some(true), None)), None)),
            Some(&url)
        ),
        refused_with(PROJECTOR_UNREAD)
    );
    assert_eq!(
        verdict(&decided, Some(&entry(Some(facts(None, None)), None)), None),
        Some(Some(ToolImages::Unknown))
    );
    assert_eq!(
        verdict(&decided, Some(&entry(None, None)), None),
        Some(None)
    );
    assert_eq!(verdict(&decided, None, None), Some(None));
}

/// webp goes only where both containers decode it: lost on the second, the
/// decision carries the second's facts; gained there, nothing changes.
#[test]
fn a_recheck_never_lets_webp_go_where_the_count_did_not() {
    let with_video = facts(Some(true), Some(true));
    let without = facts(Some(true), None);
    let lost = rechecked(
        &allowed_on(&with_video),
        Some(&entry(Some(without.clone()), None)),
        None,
    )
    .expect("changed")
    .expect("facts known");
    assert_eq!(lost.tool_images, ToolImages::Allowed);
    assert_eq!(lost.facts.video, None);
    assert_eq!(
        verdict(
            &allowed_on(&without),
            Some(&entry(Some(with_video), None)),
            None
        ),
        None
    );
}
