//! The renderer per block type, array against string, today's bytes when
//! nothing goes as an image, and the format check (§8.1, §8.2).

use serde_json::{json, Value};

use super::*;
use crate::egress::openai_wire::FlattenToolResults;

/// A whole 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
/// A whole 1x1 GIF.
const GIF: &str = "R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==";
/// A whole 8x8 grey baseline JPEG.
const JPEG: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/wAALCAAIAAgBAREA/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/9oACAEBAAA/ACv/2Q==";
/// A whole 2x2 24-bit BMP.
const BMP: &str = "Qk1GAAAAAAAAADYAAAAoAAAAAgAAAAIAAAABABgAAAAAABAAAADEDgAAxA4AAAAAAAAAAAAAKB7IKB7IAAAoHsgoHsgAAA==";
/// A whole 1x1 lossless WebP.
const WEBP: &str = "UklGRiAAAABXRUJQVlA4TBQAAAAvAAAAAAdQgVQIIAAKmv7HiIj+Bw==";
/// The PNG signature alone: what the egress corpus's png case carries.
const PNG_SIGNATURE: &str = "iVBORw0KGgo=";
/// `<svg/>`.
const SVG: &str = "PHN2Zy8+";

const SEND: LlamaToolResults<'static> = LlamaToolResults(ToolImageDecision::Send { webp: false });
const SEND_WEBP: LlamaToolResults<'static> =
    LlamaToolResults(ToolImageDecision::Send { webp: true });
const NO_VISION: LlamaToolResults<'static> = LlamaToolResults(ToolImageDecision::Placeholder {
    reason: "this model's server has no vision",
});

fn image(mime: &str, data: &str) -> ToolResultBlock {
    ToolResultBlock::Image {
        mime: mime.into(),
        data: data.into(),
    }
}

fn text(s: &str) -> ToolResultBlock {
    ToolResultBlock::text(s)
}

fn url(mime: &str, data: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}})
}

fn text_part(s: &str) -> Value {
    json!({"type": "text", "text": s})
}

/// Every block kind but an image, alone and together.
fn imageless_results() -> Vec<Vec<ToolResultBlock>> {
    let json_block = ToolResultBlock::Json {
        value: json!({"temp": 12, "sky": ["sun"]}),
    };
    let resources = vec![
        ToolResultBlock::Resource {
            uri: "file:///notes.md".into(),
            mime: Some("text/markdown".into()),
            text: Some("# Notes".into()),
        },
        ToolResultBlock::Resource {
            uri: "file:///chart.pdf".into(),
            mime: Some("application/pdf".into()),
            text: None,
        },
        ToolResultBlock::Resource {
            uri: "file:///blob".into(),
            mime: None,
            text: None,
        },
    ];
    let audio = ToolResultBlock::Audio {
        mime: "audio/wav".into(),
        data: "UklGRg==".into(),
    };
    let mut all = vec![text("12°C"), json_block.clone(), audio.clone(), text("")];
    all.extend(resources.clone());
    vec![
        vec![],
        vec![text("12°C, sunny")],
        vec![text("")],
        vec![json_block],
        resources,
        vec![audio],
        all,
    ]
}

#[test]
fn a_result_without_images_is_todays_string_byte_for_byte() {
    for blocks in imageless_results() {
        let today = FlattenToolResults.render("call_1", &blocks);
        for renderer in [SEND, SEND_WEBP, NO_VISION] {
            let ours = renderer.render("call_1", &blocks);
            assert_eq!(
                serde_json::to_string(&ours).unwrap(),
                serde_json::to_string(&today).unwrap(),
                "{blocks:?} under {renderer:?}"
            );
        }
    }
}

#[test]
fn a_sendable_image_turns_the_result_into_one_part_per_block() {
    let blocks = vec![
        text("Here is the chart:"),
        image("image/png", PNG),
        ToolResultBlock::Json {
            value: json!({"points": 3}),
        },
        ToolResultBlock::Resource {
            uri: "file:///notes.md".into(),
            mime: Some("text/markdown".into()),
            text: Some("# Notes".into()),
        },
        ToolResultBlock::Resource {
            uri: "file:///chart.pdf".into(),
            mime: Some("application/pdf".into()),
            text: None,
        },
        ToolResultBlock::Audio {
            mime: "audio/wav".into(),
            data: "UklGRg==".into(),
        },
    ];
    assert_eq!(
        SEND.render("call_1", &blocks),
        json!([
            text_part("Here is the chart:"),
            url("image/png", PNG),
            text_part(r#"{"points":3}"#),
            text_part("# Notes"),
            text_part("[application/pdf resource: file:///chart.pdf]"),
            text_part(
                "[audio/wav audio, 8 base64 bytes — omitted: this upstream's tool-result slot \
                 is text-only]"
            ),
        ])
    );
}

#[test]
fn an_image_alone_is_an_array_of_its_one_part() {
    assert_eq!(
        SEND.render("call_1", &[image("image/png", PNG)]),
        json!([url("image/png", PNG)])
    );
}

/// The `data:` URL carries the format's own lowercase mime: llama-server
/// matches `data:image/` case-sensitively.
#[test]
fn the_url_carries_the_formats_own_mime() {
    assert_eq!(
        SEND.render("c", &[image("IMAGE/JPG; q=1", JPEG)]),
        json!([url("image/jpeg", JPEG)])
    );
}

#[test]
fn every_format_llama_cpp_decodes_goes() {
    for (mime, data) in [
        ("image/png", PNG),
        ("image/jpeg", JPEG),
        ("image/gif", GIF),
        ("image/bmp", BMP),
    ] {
        assert_eq!(
            SEND.render("c", &[image(mime, data)]),
            json!([url(mime, data)]),
            "{mime}"
        );
    }
}

#[test]
fn a_non_sendable_image_beside_a_sendable_one_is_its_placeholder_with_why() {
    assert_eq!(
        SEND.render("c", &[image("image/svg+xml", SVG), image("image/png", PNG)]),
        json!([
            text_part(
                "[image/svg+xml image, 8 base64 bytes — omitted: svg is not a format llama.cpp \
                 decodes]"
            ),
            url("image/png", PNG),
        ])
    );
}

/// An SVG on a route that may send images: nothing goes as an image, so the
/// result stays a string, and its placeholder names the format.
#[test]
fn an_svg_alone_stays_a_string_naming_the_format() {
    assert_eq!(
        SEND.render("c", &[text("chart:"), image("image/svg+xml", SVG)]),
        json!(
            "chart:\n[image/svg+xml image, 8 base64 bytes — omitted: svg is not a format \
             llama.cpp decodes]"
        )
    );
}

#[test]
fn a_refused_decision_makes_every_image_its_placeholder_with_the_reason() {
    assert_eq!(
        NO_VISION.render("c", &[text("chart:"), image("image/png", PNG)]),
        json!(format!(
            "chart:\n[image/png image, {} base64 bytes — omitted: this model's server has no \
             vision]",
            PNG.len()
        ))
    );
}

#[test]
fn webp_goes_only_where_the_server_decodes_it() {
    assert_eq!(
        SEND_WEBP.render("c", &[image("image/webp", WEBP)]),
        json!([url("image/webp", WEBP)])
    );
    assert_eq!(
        SEND.render("c", &[image("image/webp", WEBP)]),
        json!(format!(
            "[image/webp image, {} base64 bytes — omitted: {WEBP_NEEDS_VIDEO}]",
            WEBP.len()
        ))
    );
}

#[test]
fn the_format_check_names_why() {
    let why = |mime: &str, data: &str| image_format(mime, data, false).unwrap_err();
    assert_eq!(
        why("image/svg+xml", SVG),
        "svg is not a format llama.cpp decodes"
    );
    assert_eq!(
        why("image/tiff", "SUkqAA=="),
        "tiff is not a format llama.cpp decodes"
    );
    assert_eq!(
        why("image/x-icon", "AAABAA=="),
        "icon is not a format llama.cpp decodes"
    );
    assert_eq!(
        why("application/pdf", "JVBERg=="),
        "application/pdf is not a format llama.cpp decodes"
    );
    assert_eq!(
        why("", PNG),
        "an image without a mime type is not a format llama.cpp decodes"
    );
    assert_eq!(why("image/webp", WEBP), WEBP_NEEDS_VIDEO);
    // An image block that carries no bytes (a client's malformed one).
    assert_eq!(why("image/png", ""), "it carries no image data");
    assert_eq!(
        why("image/png", "iVBORw0K\nGgo="),
        "its data is not plain base64"
    );
    assert_eq!(
        why("image/png", "iVBO=Rw0KGgo="),
        "its data is not plain base64"
    );
    assert_eq!(why("image/png", "iVBO-w0_"), "its data is not plain base64");
    // A length one past a whole group of four does not decode.
    assert_eq!(
        why("image/png", "iVBORw0KG"),
        "its data is not plain base64"
    );
    assert_eq!(why("image/png", SVG), "its bytes are not a png image");
    assert_eq!(
        why("image/png", JPEG),
        "its bytes are jpeg, not the png it says it is"
    );
    assert_eq!(
        why("image/jpeg", WEBP),
        "its bytes are webp, not the jpeg it says it is"
    );
    // The right magic is not enough: the file is walked as stb_image reads
    // it (the walk's own reasons are `stb`'s tests).
    assert_eq!(
        why("image/png", PNG_SIGNATURE),
        "it is not a png llama.cpp decodes (it ends before its IHDR chunk)"
    );
    assert_eq!(
        // Cut before its EOI's last byte.
        why("image/jpeg", &JPEG[..JPEG.len() - 4]),
        "it is not a jpeg llama.cpp decodes (it ends inside a scan, before its EOI marker)"
    );
    assert_eq!(
        image_format("image/webp", "UklGRhoAAABXRUJQVlA4TA0AAAA=", true),
        Err("it is not a webp llama.cpp decodes (its RIFF container is cut off)".into())
    );
}

/// A truncated image beside a whole one is its placeholder naming the
/// check it failed; the whole one goes.
#[test]
fn an_image_stb_image_cannot_decode_is_its_placeholder() {
    assert_eq!(
        SEND.render(
            "c",
            &[image("image/png", PNG_SIGNATURE), image("image/gif", GIF)]
        ),
        json!([
            text_part(
                "[image/png image, 12 base64 bytes — omitted: it is not a png llama.cpp decodes \
                 (it ends before its IHDR chunk)]"
            ),
            url("image/gif", GIF),
        ])
    );
}

#[test]
fn the_format_check_passes_what_llama_cpp_decodes() {
    for (mime, data, format) in [
        ("image/png", PNG, ImageFormat::Png),
        // Unpadded base64, as llama-server's decoder reads it.
        ("image/png", PNG.trim_end_matches('='), ImageFormat::Png),
        ("image/jpeg", JPEG, ImageFormat::Jpeg),
        ("image/jpg", JPEG, ImageFormat::Jpeg),
        ("image/gif", GIF, ImageFormat::Gif),
        ("image/bmp", BMP, ImageFormat::Bmp),
        ("image/x-ms-bmp", BMP, ImageFormat::Bmp),
        ("Image/PNG; charset=binary", PNG, ImageFormat::Png),
    ] {
        assert_eq!(image_format(mime, data, false), Ok(format), "{mime}");
    }
    assert_eq!(
        image_format("image/webp", WEBP, true),
        Ok(ImageFormat::Webp)
    );
}

fn route_with(tool_images: ToolImages, video: Option<bool>) -> LlamaRoute {
    LlamaRoute {
        facts: std::sync::Arc::new(crate::egress::llama_cpp::props::LlamaFacts {
            vision: Some(true),
            video,
            ..Default::default()
        }),
        tool_images,
    }
}

#[test]
fn the_decision_comes_from_the_routes_frozen_one() {
    let allowed = route_with(ToolImages::Allowed, None);
    assert_eq!(
        ToolImageDecision::of(&allowed),
        Some(ToolImageDecision::Send { webp: false })
    );
    let with_video = route_with(ToolImages::Allowed, Some(true));
    assert_eq!(
        ToolImageDecision::of(&with_video),
        Some(ToolImageDecision::Send { webp: true })
    );
    let refused = route_with(ToolImages::Refused("no per-image bound".into()), None);
    assert_eq!(
        ToolImageDecision::of(&refused),
        Some(ToolImageDecision::Placeholder {
            reason: "no per-image bound"
        })
    );
    assert_eq!(
        ToolImageDecision::of(&route_with(ToolImages::Unknown, None)),
        None
    );
}

/// `chat_body` renders tool results by the route's decision: today's text
/// with none, or with one made where vision is unknown; an array where
/// images go; the reason where a condition failed.
#[test]
fn chat_body_renders_by_the_routes_decision() {
    use crate::ir::{ChatRequest, ContentPart, Message, Params, Role};
    let ir = ChatRequest {
        model_alias: "m".into(),
        messages: vec![Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "c".into(),
                name: None,
                content: vec![text("chart:"), image("image/png", PNG)],
                is_error: false,
            }],
        }],
        params: Params::default(),
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let content = |decision: Option<LlamaRoute>| {
        let mut up = crate::config::Snapshot::default().router_upstream();
        up.llama = decision.map(std::sync::Arc::new);
        crate::egress::llama_cpp::chat_body(&ir, "m", &Params::default(), false, &up)["messages"][0]
            ["content"]
            .clone()
    };
    let today = json!(format!(
        "chart:\n[image/png image, {} base64 bytes — omitted: this upstream's tool-result slot \
         is text-only]",
        PNG.len()
    ));
    assert_eq!(content(None), today);
    assert_eq!(content(Some(route_with(ToolImages::Unknown, None))), today);
    assert_eq!(
        content(Some(route_with(ToolImages::Allowed, None))),
        json!([text_part("chart:"), url("image/png", PNG)])
    );
    assert_eq!(
        content(Some(route_with(
            ToolImages::Refused("this model's server has no vision".into()),
            None
        ))),
        json!(format!(
            "chart:\n[image/png image, {} base64 bytes — omitted: this model's server has no \
             vision]",
            PNG.len()
        ))
    );
}
