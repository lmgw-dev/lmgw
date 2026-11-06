//! `image__upscale` (chat-media-tools design §3.2, §3.3, §9): an ESRGAN
//! upscale of a picture on a local image row, through
//! `proxy::image_call::upscale`. No `model` parameter — which row runs it is
//! the gateway's rule (§9) — and an `upscaler` enum of the file stems the
//! image models dir's `upscalers/` holds; the tool is listed only when it
//! holds one.

use lmgw_api_types::chat_media::MediaFacts;
use serde_json::{json, Map, Value};

use crate::proxy::image_call::upscale::{self, UpscaleCall, UpscaleOutput};
use crate::proxy::image_call::InputImage;
use crate::state::SharedState;

use super::args::{int_arg, required, str_arg};
use super::list::Offer;
use super::result::{secs, Outcome, Produced};
use super::{refs, CallCx, IMAGE_UPSCALE};

/// sd-server's own bound on `repeats` (`POST /sdcpp/v1/upscale`).
pub(crate) const MAX_REPEATS: i64 = 4;

pub(super) async fn run(
    state: &SharedState,
    args: &Map<String, Value>,
    offer: &Offer,
    cx: &CallCx<'_>,
) -> Result<Outcome, String> {
    if !offer.offers_upscale() {
        return Err(format!(
            "{IMAGE_UPSCALE} has nothing to run with right now: it needs an upscaler file in \
             the image models dir's upscalers/ and an image model this caller may use"
        ));
    }
    let raw = required(args, "image")?;
    let upscaler = upscaler_arg(args, offer)?;
    let repeats = repeats_arg(args)?;
    let src = refs::resolve(cx.refs, raw, "image", "image").await?;
    let call = UpscaleCall {
        image: InputImage {
            mime: src.mime.clone(),
            bytes: src.bytes.clone(),
        },
        upscaler,
        repeats,
        made_by: src.made_by.as_deref(),
        rows: Some(&offer.upscale_rows),
    };
    let out = upscale::run(state, cx.ctx, cx.proto, call, cx.hooks, cx.stop)
        .await
        .map_err(|e| e.to_string())?;
    Ok(said(&src.name, src.id, out))
}

/// The `upscaler` argument: one of the names on disk.
pub(crate) fn upscaler_arg<'a>(
    args: &'a Map<String, Value>,
    offer: &Offer,
) -> Result<Option<&'a str>, String> {
    let Some(u) = str_arg(args, "upscaler")? else {
        return Ok(None);
    };
    check_upscaler(u, offer)?;
    Ok(Some(u))
}

/// `u` is an upscaler on disk, or the refusal naming those that are.
pub(crate) fn check_upscaler(u: &str, offer: &Offer) -> Result<(), String> {
    if offer.upscalers.iter().any(|n| n == u) {
        return Ok(());
    }
    Err(if offer.upscalers.is_empty() {
        "there is no upscaler in the image models dir's upscalers/".to_string()
    } else {
        format!(
            "upscaler '{u}' is not one this gateway has; pick one of: {}",
            offer.upscalers.join(", ")
        )
    })
}

/// The `repeats` argument, 1 to 4.
fn repeats_arg(args: &Map<String, Value>) -> Result<Option<u32>, String> {
    let Some(r) = int_arg(args, "repeats")? else {
        return Ok(None);
    };
    check_repeats(r).map(Some)
}

/// `r` within sd-server's 1 to 4.
pub(crate) fn check_repeats(r: i64) -> Result<u32, String> {
    if (1..=MAX_REPEATS).contains(&r) {
        Ok(r as u32)
    } else {
        Err(format!("repeats {r} is outside 1 to {MAX_REPEATS}"))
    }
}

/// The upscaled picture as a file, and the sentence about it: the tool's
/// and the card's Upscale's (`web::chat_media::actions`). `source_name`
/// and `source_id` are the picture it started from.
pub(crate) fn said(source_name: &str, source_id: Option<i64>, out: UpscaleOutput) -> Outcome {
    let (w, h) = match (out.width, out.height) {
        (Some(w), Some(h)) => (Some(w), Some(h)),
        _ => {
            let (w, h) = out.picture.png_size().unzip();
            (w, h)
        }
    };
    let stem = std::path::Path::new(source_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("image");
    let ext = out.picture.mime.rsplit('/').next().unwrap_or("png");
    let times = match (out.scale, out.repeats) {
        (Some(s), Some(r)) if r > 1 => Some(s.pow(r)),
        (Some(s), _) => Some(s),
        _ => None,
    };
    let name = match times {
        Some(t) => format!("{stem}-x{t}.{ext}"),
        None => format!("{stem}-upscaled.{ext}"),
    };
    let size = w
        .zip(h)
        .map(|(w, h)| format!(" ({w}x{h})"))
        .unwrap_or_default();
    let with = match (&out.upscaler, times) {
        (Some(u), Some(t)) => format!(" with {u} ({t}x)"),
        (Some(u), None) => format!(" with {u}"),
        (None, Some(t)) => format!(" ({t}x)"),
        (None, None) => String::new(),
    };
    let what = source_id
        .map(|id| format!("att:{id}"))
        .unwrap_or_else(|| "the picture".to_string());
    let detail = format!(
        "{size} by upscaling {what}{with} on {}: {} ({} of it upscaling).",
        out.model,
        secs(out.wall_ms),
        secs(out.upscale_ms)
    );
    let file = Produced {
        kind: "image",
        mime: out.picture.mime.clone(),
        name,
        facts: MediaFacts {
            tool: Some(IMAGE_UPSCALE.to_string()),
            model: Some(out.model.clone()),
            width: w,
            height: h,
            parent: source_id,
            upscaler: out.upscaler.clone(),
            scale: times.map(f64::from),
            wall_ms: Some(out.wall_ms),
            job_ms: Some(out.upscale_ms),
            ..Default::default()
        },
        bytes: out.picture.bytes,
    };
    Outcome::made(
        IMAGE_UPSCALE,
        vec![file],
        (
            "Made image ".to_string(),
            format!("{detail} The user sees it."),
        ),
        format!("Made an image{detail} It is in this result."),
    )
}

/// Its `tools/list` entry.
pub(super) fn tool(offer: &Offer) -> Value {
    let limit = offer.upscale_limit_said();
    let lines: Vec<String> = offer.upscalers.iter().map(|u| format!("- {u}")).collect();
    json!({
        "name": IMAGE_UPSCALE,
        "description": format!(
            "Upscale a picture with an ESRGAN upscaler on one of this gateway's image models: \
             each run makes each side as many times larger as the upscaler's scale (4 for an \
             x4 model). No prompt and no sampling, so it is quick, but it runs on an image \
             model's server and may load one onto the GPU. In a Chat conversation the user \
             sees the result at once and it is named att:N; elsewhere it comes back in the \
             result.{limit} Upscalers:\n{}",
            lines.join("\n")
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "image": {"type": "string", "description": "The picture to upscale: att:N from this conversation, or a data: URL."},
                "upscaler": {"type": "string", "enum": offer.upscalers, "description": "Which upscaler (see the list above). Omit it for the first one the image server can run."},
                "repeats": {"type": "integer", "minimum": 1, "maximum": MAX_REPEATS, "description": "How many times to run it; each run multiplies the size again. Omit it for one."},
            },
            "required": ["image"],
            "additionalProperties": false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::image_call::Picture;

    fn output() -> UpscaleOutput {
        UpscaleOutput {
            picture: Picture {
                mime: "image/png".into(),
                bytes: crate::bench::png::solid_red(),
            },
            model: "image/z-image-turbo".into(),
            upscaler: Some("RealESRGAN_x4plus".into()),
            scale: Some(4),
            repeats: Some(1),
            width: Some(256),
            height: Some(256),
            upscale_ms: 900,
            wall_ms: 3100,
        }
    }

    #[test]
    fn an_upscale_is_said_with_both_refs_and_its_facts() {
        let o = said("image-z-image-turbo-7.png", Some(57), output());
        assert_eq!(
            o.text_for(Some(&[58])),
            "Made image att:58 (256x256) by upscaling att:57 with RealESRGAN_x4plus (4x) on \
             image/z-image-turbo: 3.1 s (0.9 s of it upscaling). The user sees it."
        );
        let f = &o.files[0];
        assert_eq!(f.name, "image-z-image-turbo-7-x4.png");
        assert_eq!(f.facts.parent, Some(57));
        assert_eq!(f.facts.scale, Some(4.0));
        let mut twice = output();
        twice.repeats = Some(2);
        assert_eq!(said("a.png", None, twice).files[0].name, "a-x16.png");
    }

    #[test]
    fn its_arguments_are_bounded_by_the_server_and_the_disk() {
        assert_eq!(check_repeats(4), Ok(4));
        assert!(check_repeats(5).unwrap_err().contains("outside 1 to 4"));
        let offer = Offer {
            upscalers: vec!["RealESRGAN_x4plus".into()],
            upscale_rows: vec!["image/z".into()],
            ..Default::default()
        };
        assert!(check_upscaler("RealESRGAN_x4plus", &offer).is_ok());
        let e = check_upscaler("other", &offer).unwrap_err();
        assert!(e.contains("pick one of: RealESRGAN_x4plus"), "{e}");
        let t = tool(&offer);
        assert!(!t["description"].as_str().unwrap().contains("larger than"));
        let one = Offer {
            upscale_limits: vec![("image/z".into(), Some((8192, 8192)))],
            ..offer.clone()
        };
        assert!(tool(&one)["description"]
            .as_str()
            .unwrap()
            .contains(" The result may not be larger than 8192x8192, the image server's limit."));
        let differ = Offer {
            upscale_rows: vec!["image/a".into(), "image/b".into(), "image/c".into()],
            upscale_limits: vec![
                ("image/a".into(), Some((8192, 8192))),
                ("image/b".into(), Some((4096, 4096))),
                ("image/c".into(), None),
            ],
            ..offer.clone()
        };
        assert!(
            tool(&differ)["description"].as_str().unwrap().contains(
                "which the gateway picks: 8192x8192 on image/a, 4096x4096 on image/b, not \
                 known yet on image/c."
            ),
            "{}",
            tool(&differ)["description"]
        );
        assert_eq!(
            t["inputSchema"]["properties"]["upscaler"]["enum"],
            json!(["RealESRGAN_x4plus"])
        );
        assert_eq!(t["inputSchema"]["required"], json!(["image"]));
    }
}
