//! `image__generate` and `image__edit` (chat-media-tools design §3.3,
//! §3.5): the arguments read into a `proxy::image_call` call, and the
//! pictures it made said in words a model can relay.

use lmgw_api_types::chat_media::MediaFacts;
use serde_json::{Map, Value};

use crate::proxy::image_call::{
    self, Action, EditKind, ImageCall, ImageOutput, InputImage, Params,
};
use crate::state::SharedState;

use super::args::{int_arg, model, num_arg, required, str_arg};
use super::list::Offer;
use super::result::{secs, Outcome, Produced};
use super::{refs, CallCx, IMAGE_EDIT};

pub(super) async fn run(
    state: &SharedState,
    name: &str,
    args: &Map<String, Value>,
    offer: &Offer,
    cx: &CallCx<'_>,
) -> Result<Outcome, String> {
    if name == IMAGE_EDIT {
        super::edit_rows::refuse_non_edit(args, offer)?;
    }
    let alias = model(args, offer, name)?;
    let params = params(args)?;
    let mut parent = None;
    let action = if name == IMAGE_EDIT {
        let raw = required(args, "image")?;
        let src = refs::resolve(cx.refs, raw, "image", "image").await?;
        parent = src.id;
        Action::Edit {
            image: InputImage {
                mime: src.mime,
                bytes: src.bytes,
            },
        }
    } else {
        Action::Generate
    };
    // A call that sets no seed gets one lmgw draws on a local row
    // (`image_call::job`), which the picture's facts then name.
    let call = ImageCall {
        alias,
        action,
        params: params.clone(),
    };
    let out = image_call::run(state, cx.ctx, cx.proto, call, cx.hooks, cx.stop)
        .await
        .map_err(|e| e.to_string())?;
    let mut said = said(name, alias, &params, out);
    // The picture an edit started from, when the thread holds it: what a
    // card's Re-roll edits again (design §8.3).
    for f in &mut said.files {
        f.facts.parent = parent;
    }
    Ok(said)
}

/// The tool's flat parameters as the call's.
fn params(args: &Map<String, Value>) -> Result<Params, String> {
    let size = match str_arg(args, "size")? {
        Some(s) => Some(image_call::parse_size(s)?),
        None => None,
    };
    // No `strength`: neither tool makes a variation (`edit_rows`).
    Ok(Params {
        prompt: required(args, "prompt")?.to_string(),
        negative_prompt: str_arg(args, "negative_prompt")?.map(str::to_string),
        size,
        seed: int_arg(args, "seed")?,
        steps: int_arg(args, "steps")?,
        cfg: num_arg(args, "cfg")?,
        strength: None,
    })
}

/// The pictures as files, and the sentence about them (design §3.5): the
/// tools' and the card actions' (`web::chat_media::actions`).
pub(crate) fn said(tool: &str, alias: &str, params: &Params, out: ImageOutput) -> Outcome {
    let e = &out.effective;
    let answered = out.answered_by.clone();
    let stem = crate::web::model_stem(answered.as_deref().unwrap_or(alias));
    // The seed the picture was made with: the request's or the drawn one
    // on a local row; none on a route that takes no seed.
    let seed = e.seed;
    let files: Vec<Produced> = out
        .images
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (w, h) = p.png_size().unzip();
            let ext = p.mime.rsplit('/').next().unwrap_or("png");
            let n = if out.images.len() > 1 {
                format!("-{}", i + 1)
            } else {
                String::new()
            };
            Produced {
                kind: "image",
                mime: p.mime.clone(),
                name: match seed {
                    Some(s) => format!("{stem}-{s}{n}.{ext}"),
                    None => format!("{stem}{n}.{ext}"),
                },
                bytes: p.bytes.clone(),
                facts: MediaFacts {
                    tool: Some(tool.to_string()),
                    model: Some(alias.to_string()),
                    answered_by: answered.clone(),
                    prompt: Some(params.prompt.clone()),
                    negative_prompt: params.negative_prompt.clone(),
                    seed,
                    steps: e.steps,
                    cfg: e.cfg,
                    width: w.or(e.width),
                    height: h.or(e.height),
                    strength: e.strength,
                    job_ms: out.job_ms,
                    wall_ms: Some(out.wall_ms),
                    not_applied: out.not_applied.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            }
        })
        .collect();

    let size = files
        .first()
        .and_then(|f| f.facts.width.zip(f.facts.height))
        .map(|(w, h)| format!(" ({w}x{h})"))
        .unwrap_or_default();
    let by = match &answered {
        Some(fb) => format!(
            "{fb}, the fallback for {alias} while {}",
            match out.fallback {
                Some(crate::gate::FallbackReason::Hold) | None => "the GPU hold is on".to_string(),
                Some(r) => format!("its local model cannot run ({})", r.as_str()),
            }
        ),
        None => alias.to_string(),
    };
    let mut facts = Vec::new();
    if let Some(s) = files.first().and_then(|f| f.facts.seed) {
        facts.push(format!("seed {s}"));
    }
    if let Some(s) = e.steps {
        facts.push(format!("{s} steps"));
    }
    if let Some(c) = e.cfg {
        facts.push(format!("cfg {}", trim_float(c)));
    }
    if let Some(s) = e.strength {
        facts.push(format!("strength {}", trim_float(s)));
    }
    facts.push(match out.job_ms {
        Some(job) => format!("{} ({} of it drawing)", secs(out.wall_ms), secs(job)),
        None => secs(out.wall_ms),
    });
    let how = match out.edit {
        Some(EditKind::Instruction) => " as an edit",
        Some(EditKind::Variation) => " as a variation",
        Some(EditKind::Cloud) | None => "",
    };
    let not_applied = if out.not_applied.is_empty() {
        String::new()
    } else {
        format!(
            "; {} {} not applied",
            list(&out.not_applied),
            if out.not_applied.len() == 1 {
                "was"
            } else {
                "were"
            }
        )
    };
    let detail = format!("{size}{how} with {by}: {}{not_applied}.", facts.join(", "));
    let (noun, bare_noun) = if files.len() > 1 {
        ("images ", format!("Made {} images", files.len()))
    } else {
        ("image ", "Made an image".to_string())
    };
    Outcome::made(
        tool,
        files,
        (
            format!("Made {noun}"),
            format!("{detail} The user sees it."),
        ),
        format!("{bare_noun}{detail} It is in this result."),
    )
}

/// `seed, steps and cfg`.
fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `1` for `1.0`, `1.5` as it is.
fn trim_float(f: f64) -> String {
    if f.fract() == 0.0 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::image_call::{Effective, Picture};

    fn output() -> ImageOutput {
        ImageOutput {
            images: vec![Picture {
                mime: "image/png".into(),
                bytes: crate::bench::png::solid_red(),
            }],
            effective: Effective {
                seed: Some(1730094420),
                steps: Some(8),
                cfg: Some(1.0),
                width: Some(64),
                height: Some(64),
                strength: None,
            },
            job_ms: Some(1100),
            wall_ms: 4700,
            answered_by: None,
            fallback: None,
            not_applied: Vec::new(),
            edit: None,
        }
    }

    #[test]
    fn a_picture_is_said_with_its_ref_and_its_facts() {
        let p = Params {
            prompt: "a red square".into(),
            seed: Some(1730094420),
            ..Default::default()
        };
        let o = said("image__generate", "image/z-image-turbo", &p, output());
        assert_eq!(
            o.text_for(Some(&[57])),
            "Made image att:57 (64x64) with image/z-image-turbo: seed 1730094420, 8 steps, \
             cfg 1, 4.7 s (1.1 s of it drawing). The user sees it."
        );
        assert_eq!(o.files[0].name, "image-z-image-turbo-1730094420.png");
        assert_eq!(o.files[0].facts.seed, Some(1730094420));
    }

    #[test]
    fn a_fallback_says_who_answered_and_what_it_did_not_take() {
        let p = Params {
            prompt: "x".into(),
            seed: Some(5),
            steps: Some(4),
            ..Default::default()
        };
        let mut out = output();
        out.answered_by = Some("image-cloud.example".into());
        out.fallback = Some(crate::gate::FallbackReason::Hold);
        out.not_applied = vec!["seed", "steps"];
        out.effective = Effective {
            width: Some(64),
            height: Some(64),
            ..Default::default()
        };
        out.job_ms = None;
        let o = said("image__generate", "image/z-image-turbo", &p, out);
        let t = o.text_for(None);
        assert!(
            t.contains(
                "with image-cloud.example, the fallback for image/z-image-turbo while the GPU \
                 hold is on"
            ),
            "{t}"
        );
        assert!(t.contains("seed and steps were not applied"), "{t}");
        assert_eq!(o.files[0].facts.seed, None);
        assert_eq!(o.files[0].name, "image-cloud.example.png");
    }
}
