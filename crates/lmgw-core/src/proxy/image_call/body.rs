//! sd-server's native request body for one call (chat-media-tools design
//! §4.1), and the small parsers around it. Pure: everything here is
//! unit-tested without a server.
//!
//! | Call | Native field |
//! |---|---|
//! | `prompt`, `negative_prompt` | `prompt`, `negative_prompt` |
//! | `size` `WxH` | `width`, `height` |
//! | `seed` | `seed` (lmgw draws one when the call sets none) |
//! | `steps` | `sample_params.sample_steps` |
//! | `cfg` | `sample_params.guidance.txt_cfg` |
//! | an instruction edit's picture | `ref_images: [data URL]` |
//! | a variation's picture | `init_image` + `strength` |
//! | the preview mode | `preview`, `preview_interval: 1` |
//!
//! Unset fields are left out, so the server's own defaults (the row's
//! `args`) apply. `ref_images` goes only to a row whose `edit` column is
//! set: sd-server segfaults on it otherwise (image §12.8), and this builder
//! is where that is enforced.

use base64::Engine as _;
use lmgw_api_types::chat_media::PreviewMode;
use serde_json::{json, Map, Value};

use super::{EditKind, Effective, InputImage, Params};

/// A picture as the `data:` URL sd-server's image fields take.
pub(crate) fn data_url(image: &InputImage) -> String {
    format!(
        "data:{};base64,{}",
        image.mime,
        base64::engine::general_purpose::STANDARD.encode(&image.bytes)
    )
}

/// A seed for a call that set none, so every picture names the seed that
/// makes it again (design §12, decision 4): `0..=i32::MAX`, a range every
/// sd.cpp build reads.
pub(crate) fn draw_seed() -> i64 {
    i64::from(rand::random::<u32>() >> 1)
}

/// `WIDTHxHEIGHT` (`1024x768`, an `×` or `*` too) as pixels; the refusal
/// names the shape it wants.
pub(crate) fn parse_size(raw: &str) -> Result<(u32, u32), String> {
    let t = raw.trim().to_ascii_lowercase();
    let wrong = || format!("size '{raw}' is not WIDTHxHEIGHT in pixels, e.g. 1024x768");
    let (w, h) = t.split_once(['x', '×', '*']).ok_or_else(wrong)?;
    let w: u32 = w.trim().parse().map_err(|_| wrong())?;
    let h: u32 = h.trim().parse().map_err(|_| wrong())?;
    if w == 0 || h == 0 {
        return Err(wrong());
    }
    Ok((w, h))
}

/// The native body of one call. `edit`: the picture and how it goes, for an
/// edit; `seed`: the call's, or one [`draw_seed`] drew.
pub(crate) fn native(
    params: &Params,
    edit: Option<(&InputImage, EditKind)>,
    preview: Option<PreviewMode>,
    seed: i64,
) -> Value {
    let mut body = Map::new();
    body.insert("prompt".into(), json!(params.prompt));
    if let Some(n) = params.negative_prompt.as_deref().filter(|n| !n.is_empty()) {
        body.insert("negative_prompt".into(), json!(n));
    }
    if let Some((w, h)) = params.size {
        body.insert("width".into(), json!(w));
        body.insert("height".into(), json!(h));
    }
    body.insert("seed".into(), json!(seed));
    let mut sample = Map::new();
    if let Some(steps) = params.steps {
        sample.insert("sample_steps".into(), json!(steps));
    }
    if let Some(cfg) = params.cfg {
        sample.insert("guidance".into(), json!({ "txt_cfg": cfg }));
    }
    if !sample.is_empty() {
        body.insert("sample_params".into(), Value::Object(sample));
    }
    match edit {
        Some((image, EditKind::Instruction)) => {
            body.insert("ref_images".into(), json!([data_url(image)]));
        }
        Some((image, EditKind::Variation)) => {
            body.insert("init_image".into(), json!(data_url(image)));
            if let Some(s) = params.strength {
                body.insert("strength".into(), json!(s));
            }
        }
        // The cloud edit is the OpenAI route's multipart, not this body.
        Some((_, EditKind::Cloud)) | None => {}
    }
    if let Some(mode) = preview.filter(|m| *m != PreviewMode::None) {
        body.insert("preview".into(), json!(mode.as_str()));
        body.insert("preview_interval".into(), json!(1));
    }
    body.insert("output_format".into(), json!("png"));
    body.insert("batch_count".into(), json!(1));
    Value::Object(body)
}

/// The values a native call was made with: the request's, else what the
/// probed `defaults_by_mode.img_gen` says the server used (design §4.1).
pub(crate) fn effective(
    params: &Params,
    seed: i64,
    variation: bool,
    defaults: Option<&Map<String, Value>>,
) -> Effective {
    let d = |path: &[&str]| -> Option<&Value> {
        let mut v = defaults?.get(path[0])?;
        for k in &path[1..] {
            v = v.get(k)?;
        }
        Some(v)
    };
    let size = |k: &str| d(&[k]).and_then(Value::as_u64).map(|v| v as u32);
    Effective {
        seed: Some(seed),
        steps: params
            .steps
            .or_else(|| d(&["sample_params", "sample_steps"]).and_then(Value::as_i64)),
        cfg: params
            .cfg
            .or_else(|| d(&["sample_params", "guidance", "txt_cfg"]).and_then(Value::as_f64)),
        width: params.size.map(|s| s.0).or_else(|| size("width")),
        height: params.size.map(|s| s.1).or_else(|| size("height")),
        strength: variation
            .then(|| {
                params
                    .strength
                    .or_else(|| d(&["strength"]).and_then(Value::as_f64))
            })
            .flatten(),
    }
}

/// `(width, height)` from a PNG's IHDR; `None` for anything else.
pub(crate) fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture() -> InputImage {
        InputImage {
            mime: "image/png".into(),
            bytes: vec![1, 2, 3],
        }
    }

    #[test]
    fn every_parameter_maps_and_unset_ones_are_left_out() {
        let p = Params {
            prompt: "a red bicycle".into(),
            negative_prompt: Some("blur".into()),
            size: Some((768, 512)),
            seed: None,
            steps: Some(8),
            cfg: Some(1.5),
            strength: None,
        };
        let b = native(&p, None, None, 42);
        assert_eq!(
            b,
            json!({
                "prompt": "a red bicycle",
                "negative_prompt": "blur",
                "width": 768,
                "height": 512,
                "seed": 42,
                "sample_params": {"sample_steps": 8, "guidance": {"txt_cfg": 1.5}},
                "output_format": "png",
                "batch_count": 1,
            })
        );
        let bare = native(
            &Params {
                prompt: "x".into(),
                ..Default::default()
            },
            None,
            Some(PreviewMode::None),
            7,
        );
        assert_eq!(
            bare,
            json!({"prompt": "x", "seed": 7, "output_format": "png", "batch_count": 1})
        );
    }

    #[test]
    fn ref_images_go_only_to_an_instruction_edit() {
        let p = Params {
            prompt: "make it blue".into(),
            strength: Some(0.4),
            ..Default::default()
        };
        let pic = picture();
        let edit = native(&p, Some((&pic, EditKind::Instruction)), None, 1);
        assert_eq!(edit["ref_images"], json!(["data:image/png;base64,AQID"]));
        assert!(edit.get("init_image").is_none() && edit.get("strength").is_none());

        let variation = native(&p, Some((&pic, EditKind::Variation)), None, 1);
        assert!(variation.get("ref_images").is_none(), "{variation}");
        assert_eq!(variation["init_image"], json!("data:image/png;base64,AQID"));
        assert_eq!(variation["strength"], json!(0.4));
    }

    #[test]
    fn a_preview_asks_for_every_step() {
        let p = Params {
            prompt: "x".into(),
            ..Default::default()
        };
        let b = native(&p, None, Some(PreviewMode::Proj), 1);
        assert_eq!(b["preview"], json!("proj"));
        assert_eq!(b["preview_interval"], json!(1));
    }

    #[test]
    fn a_drawn_seed_stays_in_the_positive_i32_range() {
        for _ in 0..1000 {
            let s = draw_seed();
            assert!((0..=i64::from(i32::MAX)).contains(&s), "{s}");
        }
    }

    #[test]
    fn sizes_parse_and_a_wrong_one_names_the_shape() {
        assert_eq!(parse_size("1024x768"), Ok((1024, 768)));
        assert_eq!(parse_size(" 512 × 512 "), Ok((512, 512)));
        assert_eq!(parse_size("640*480"), Ok((640, 480)));
        for bad in ["1024", "x768", "0x5", "big"] {
            let e = parse_size(bad).unwrap_err();
            assert!(e.contains("WIDTHxHEIGHT"), "{e}");
        }
    }

    #[test]
    fn effective_values_fall_back_to_the_servers_defaults() {
        let caps: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/sdcpp/capabilities-z-image-turbo-0518b34.json"
        ))
        .unwrap();
        let defaults = caps["defaults_by_mode"]["img_gen"].as_object().unwrap();
        let p = Params {
            prompt: "x".into(),
            size: Some((512, 512)),
            ..Default::default()
        };
        let e = effective(&p, 9, false, Some(defaults));
        assert_eq!(
            e,
            Effective {
                seed: Some(9),
                steps: Some(8),
                cfg: Some(1.0),
                width: Some(512),
                height: Some(512),
                strength: None,
            }
        );
        let unknown = effective(&p, 9, true, None);
        assert_eq!(unknown.steps, None);
        assert_eq!(unknown.strength, None);
    }

    #[test]
    fn a_png_header_gives_its_size() {
        let png = crate::bench::png::solid_red();
        assert_eq!(png_size(&png), Some((64, 64)));
        assert_eq!(png_size(b"GIF89a"), None);
    }
}
