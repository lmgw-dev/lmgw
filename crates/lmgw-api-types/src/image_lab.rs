//! The Image lab's request contract (image-generation design §8): the form the
//! page fills in, and the one function that turns it into the body
//! `/v1/images/generations` receives.
//!
//! It lives in the shared crate rather than in either half because both halves
//! need the *same* document. The page's "Request" panel is only worth reading
//! if it shows what will actually be sent, and what is actually sent is built
//! on the server (`web::image_lab` → `proxy::handle_image_generation`), which
//! is where the lab's calls are dispatched in process. Two builders — one for
//! the preview, one for the dispatch — would drift the day one of them learned
//! a field, which is exactly the drift the Audio lab avoids by building its
//! preview with the function it dispatches with.
//!
//! Everything the OpenAI route does not read (§2.3 — it reads `prompt`, `n`,
//! `size`, `output_format`, `output_compression` and nothing else) goes into
//! sd.cpp's own escape hatch: a JSON block appended to the prompt, whose schema
//! is the native `img_gen` body. lmgw adds no field of its own on either side
//! of that line.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The route a generation is dispatched to.
pub const GENERATIONS_ENDPOINT: &str = "/v1/images/generations";
/// The route an edit is dispatched to.
pub const EDITS_ENDPOINT: &str = "/v1/images/edits";

/// sd.cpp's prompt extension (§2.3). The server strips the block before
/// generating; a build that does not know it leaves it in the prompt, which is
/// why it is only ever appended when it carries something.
pub const EXTRA_OPEN: &str = "<sd_cpp_extra_args>";
/// Closing half of [`EXTRA_OPEN`].
pub const EXTRA_CLOSE: &str = "</sd_cpp_extra_args>";

/// One row of the optional LoRA table. `<lora:…>` prompt tags are refused by
/// every sd.cpp family (§2.3), so a LoRA is this structured field or nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageLoraRow {
    /// A name from the probed `loras` list, or a path the server can resolve
    /// under its `--lora-model-dir`.
    pub path: String,
    /// Blank leaves the server's own multiplier in place.
    pub multiplier: String,
}

/// Everything the lab's two panels collect, as typed. Numbers travel as the
/// strings the inputs hold: an empty field means "say nothing about this", not
/// zero, and a malformed one has to be reportable as a message rather than
/// silently become a default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageGenForm {
    /// The public alias — `image/<id>` for a local row, the alias for a cloud
    /// one. Rewritten to the concrete upstream id by the route itself.
    pub model: String,
    pub prompt: String,
    pub negative_prompt: String,
    pub width: String,
    pub height: String,
    pub n: String,
    pub steps: String,
    pub cfg_scale: String,
    /// Blank says nothing (sd-server's own default is a fixed 42); `-1` is its
    /// spelling for "random each run".
    pub seed: String,
    pub sampler: String,
    pub scheduler: String,
    pub output_format: String,
    pub output_compression: String,
    pub loras: Vec<ImageLoraRow>,
}

/// Parse a number out of a form field, naming the field when it does not.
fn num<T: std::str::FromStr>(label: &str, raw: &str) -> Result<Option<T>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<T>()
        .map(Some)
        .map_err(|_| format!("{label}: '{raw}' is not a number"))
}

fn text(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    (!raw.is_empty()).then_some(raw)
}

/// Refuse a field that carries one of the extension block's own delimiters.
///
/// The format has no escaping — the server finds the block by searching the
/// prompt for [`EXTRA_OPEN`] and [`EXTRA_CLOSE`] — so a prompt containing
/// either one either closes the block early (everything after it becomes
/// prompt text, including the JSON lmgw wrote) or opens a second one. There
/// are exactly two honest answers, and stripping the delimiter silently is not
/// one of them: the request is refused, by field name.
fn no_delimiters(label: &str, raw: &str) -> Result<(), String> {
    for marker in [EXTRA_OPEN, EXTRA_CLOSE] {
        if raw.contains(marker) {
            return Err(format!(
                "{label} contains '{marker}', which is how sd.cpp delimits its own extension \
                 block — the format has no escape for it, so a request carrying one cannot be \
                 sent as written. Remove it."
            ));
        }
    }
    Ok(())
}

impl ImageGenForm {
    /// `WIDTHxHEIGHT` for the `size` field, or `None` when the form says
    /// nothing. Both halves or neither: sd-server falls back to *its* `-W/-H`
    /// pair, and lmgw has no third number to fill a missing half with.
    pub fn size(&self) -> Result<Option<String>, String> {
        let w: Option<i64> = num("width", &self.width)?;
        let h: Option<i64> = num("height", &self.height)?;
        match (w, h) {
            (Some(w), Some(h)) => Ok(Some(format!("{w}x{h}"))),
            (None, None) => Ok(None),
            _ => Err("width and height are one field (`size`) — set both or neither".into()),
        }
    }

    /// The `img_gen`-shaped object that goes inside the prompt block, or `None`
    /// when the form set none of it.
    ///
    /// Key spellings are the native body's, read off the `defaults` this class
    /// probes out of every running container
    /// (`GET /sdcpp/v1/capabilities`) — including the one that is not obvious:
    /// CFG is `sample_params.guidance.txt_cfg`, which is what the server
    /// serializes and therefore what it parses.
    pub fn extra_args(&self) -> Result<Option<Value>, String> {
        let mut extra = Map::new();
        if let Some(np) = text(&self.negative_prompt) {
            no_delimiters("the negative prompt", np)?;
            extra.insert("negative_prompt".into(), json!(np));
        }
        if let Some(seed) = num::<i64>("seed", &self.seed)? {
            extra.insert("seed".into(), json!(seed));
        }

        let mut sample = Map::new();
        if let Some(steps) = num::<i64>("steps", &self.steps)? {
            sample.insert("sample_steps".into(), json!(steps));
        }
        if let Some(m) = text(&self.sampler) {
            no_delimiters("the sampler", m)?;
            sample.insert("sample_method".into(), json!(m));
        }
        if let Some(s) = text(&self.scheduler) {
            no_delimiters("the scheduler", s)?;
            sample.insert("scheduler".into(), json!(s));
        }
        if let Some(cfg) = num::<f64>("cfg scale", &self.cfg_scale)? {
            sample.insert("guidance".into(), json!({ "txt_cfg": cfg }));
        }
        if !sample.is_empty() {
            extra.insert("sample_params".into(), Value::Object(sample));
        }

        let mut loras: Vec<Value> = Vec::new();
        for row in &self.loras {
            let Some(path) = text(&row.path) else {
                continue;
            };
            no_delimiters("a LoRA path", path)?;
            let mut l = Map::new();
            l.insert("path".into(), json!(path));
            if let Some(mult) = num::<f64>("LoRA multiplier", &row.multiplier)? {
                l.insert("multiplier".into(), json!(mult));
            }
            loras.push(Value::Object(l));
        }
        if !loras.is_empty() {
            extra.insert("lora".into(), Value::Array(loras));
        }

        Ok((!extra.is_empty()).then_some(Value::Object(extra)))
    }

    /// The prompt as it leaves lmgw: the typed text, plus sd.cpp's extension
    /// block **only** when something is in it. An empty block is not harmless
    /// — on a build that does not strip it, it is prompt text.
    pub fn full_prompt(&self) -> Result<String, String> {
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err("a prompt is required".into());
        }
        no_delimiters("the prompt", prompt)?;
        match self.extra_args()? {
            None => Ok(prompt.to_string()),
            Some(extra) => {
                let json = serde_json::to_string(&extra).map_err(|e| e.to_string())?;
                Ok(format!("{prompt} {EXTRA_OPEN}{json}{EXTRA_CLOSE}"))
            }
        }
    }

    /// The exact `POST /v1/images/generations` document.
    pub fn generation_body(&self) -> Result<Value, String> {
        if self.model.trim().is_empty() {
            return Err("pick a model first".into());
        }
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model.trim()));
        body.insert("prompt".into(), json!(self.full_prompt()?));
        if let Some(n) = num::<i64>("n", &self.n)? {
            body.insert("n".into(), json!(n));
        }
        if let Some(size) = self.size()? {
            body.insert("size".into(), json!(size));
        }
        if let Some(f) = text(&self.output_format) {
            body.insert("output_format".into(), json!(f));
        }
        if let Some(c) = num::<i64>("output compression", &self.output_compression)? {
            body.insert("output_compression".into(), json!(c));
        }
        Ok(Value::Object(body))
    }

    /// The text fields of a `POST /v1/images/edits` multipart, in the order
    /// they are written. The two file parts (`image`, and optionally `mask`)
    /// are the browser's upload and are appended after these.
    pub fn edit_fields(&self) -> Result<Vec<(String, String)>, String> {
        let body = self.generation_body()?;
        let mut out = Vec::new();
        for key in [
            "model",
            "prompt",
            "n",
            "size",
            "output_format",
            "output_compression",
        ] {
            if let Some(v) = body.get(key) {
                let text = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.push((key.to_string(), text));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ImageGenForm {
        ImageGenForm {
            model: "image/z-image-turbo".into(),
            prompt: "a lovely cat".into(),
            ..Default::default()
        }
    }

    /// The plain case: five known fields, no extension block, because nothing
    /// asked for one.
    #[test]
    fn a_bare_form_sends_no_extension_block() {
        let body = base().generation_body().unwrap();
        assert_eq!(body["prompt"], "a lovely cat");
        assert_eq!(body["model"], "image/z-image-turbo");
        assert!(body.get("n").is_none());
        assert!(body.get("size").is_none());
        assert!(!body["prompt"].as_str().unwrap().contains(EXTRA_OPEN));
    }

    #[test]
    fn every_extra_lands_in_the_block_under_its_native_key() {
        let f = ImageGenForm {
            negative_prompt: "blurry".into(),
            seed: "42".into(),
            steps: "8".into(),
            cfg_scale: "1.5".into(),
            sampler: "euler".into(),
            scheduler: "karras".into(),
            width: "512".into(),
            height: "768".into(),
            n: "2".into(),
            output_format: "webp".into(),
            output_compression: "80".into(),
            loras: vec![
                ImageLoraRow {
                    path: "add_detail".into(),
                    multiplier: "0.8".into(),
                },
                ImageLoraRow {
                    path: "  ".into(),
                    multiplier: "1".into(),
                },
            ],
            ..base()
        };
        let body = f.generation_body().unwrap();
        assert_eq!(body["size"], "512x768");
        assert_eq!(body["n"], 2);
        assert_eq!(body["output_format"], "webp");
        assert_eq!(body["output_compression"], 80);

        let prompt = body["prompt"].as_str().unwrap();
        let start = prompt.find(EXTRA_OPEN).unwrap() + EXTRA_OPEN.len();
        let end = prompt.find(EXTRA_CLOSE).unwrap();
        let extra: Value = serde_json::from_str(&prompt[start..end]).expect("valid JSON block");
        assert_eq!(extra["negative_prompt"], "blurry");
        assert_eq!(extra["seed"], 42);
        assert_eq!(extra["sample_params"]["sample_steps"], 8);
        assert_eq!(extra["sample_params"]["sample_method"], "euler");
        assert_eq!(extra["sample_params"]["scheduler"], "karras");
        assert_eq!(extra["sample_params"]["guidance"]["txt_cfg"], 1.5);
        // The blank row is not a LoRA with an empty path — it is no row.
        assert_eq!(extra["lora"].as_array().unwrap().len(), 1);
        assert_eq!(extra["lora"][0]["path"], "add_detail");
        assert_eq!(extra["lora"][0]["multiplier"], 0.8);
        // Width/height are `size`, never a second copy inside the block.
        assert!(extra.get("width").is_none());
    }

    #[test]
    fn half_a_size_and_a_bad_number_are_named_not_guessed() {
        let f = ImageGenForm {
            width: "512".into(),
            ..base()
        };
        assert!(f.size().unwrap_err().contains("both or neither"));
        let f = ImageGenForm {
            steps: "eight".into(),
            ..base()
        };
        assert!(f.generation_body().unwrap_err().contains("steps"));
        let f = ImageGenForm {
            prompt: "   ".into(),
            ..base()
        };
        assert!(f.generation_body().unwrap_err().contains("prompt"));
    }

    /// Neither delimiter may ride in on a field. The block has no escaping,
    /// so a prompt carrying `</sd_cpp_extra_args>` would close lmgw's block
    /// early and hand the rest of the JSON to the model as prompt text — and
    /// one carrying the opening tag would nest a block the server reads
    /// instead. Both are refused by name rather than stripped.
    #[test]
    fn a_field_carrying_the_block_delimiters_is_refused_by_name() {
        for marker in [EXTRA_OPEN, EXTRA_CLOSE] {
            let f = ImageGenForm {
                prompt: format!("a cat {marker}{{\"seed\":1}}"),
                ..base()
            };
            let e = f.generation_body().unwrap_err();
            assert!(e.contains("the prompt"), "{e}");
            assert!(e.contains(marker), "{e}");

            let f = ImageGenForm {
                negative_prompt: format!("blurry {marker}"),
                ..base()
            };
            let e = f.generation_body().unwrap_err();
            assert!(e.contains("the negative prompt"), "{e}");
            assert!(e.contains(marker), "{e}");

            let f = ImageGenForm {
                loras: vec![ImageLoraRow {
                    path: format!("detail{marker}"),
                    multiplier: String::new(),
                }],
                ..base()
            };
            assert!(
                f.generation_body().unwrap_err().contains("a LoRA path"),
                "a LoRA path is written into the block too"
            );
        }
        // The edits multipart is built from the same document, so it inherits
        // the refusal rather than re-checking for it.
        let f = ImageGenForm {
            negative_prompt: format!("x{EXTRA_CLOSE}"),
            ..base()
        };
        assert!(f.edit_fields().is_err());
    }

    /// The multipart carries the same values as the JSON body, as text.
    #[test]
    fn edit_fields_mirror_the_generation_body() {
        let f = ImageGenForm {
            seed: "7".into(),
            width: "1024".into(),
            height: "1024".into(),
            n: "1".into(),
            output_format: "png".into(),
            ..base()
        };
        let fields = f.edit_fields().unwrap();
        let get = |k: &str| {
            fields
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("model"), "image/z-image-turbo");
        assert_eq!(get("size"), "1024x1024");
        assert_eq!(get("n"), "1");
        assert_eq!(get("output_format"), "png");
        assert!(get("prompt").contains("\"seed\":7"));
    }
}
