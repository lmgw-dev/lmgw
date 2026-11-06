//! The media tools' flat arguments (chat-media-tools design §3.3): read and
//! checked one by one, each refusal naming the argument and what it wants —
//! the model reads it and corrects the call.

use serde_json::{Map, Value};

use super::list::Offer;

pub(super) fn str_arg<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

pub(super) fn required<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    str_arg(args, key)?.ok_or_else(|| format!("argument '{key}' is required"))
}

pub(super) fn int_arg(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .map(Some)
            .ok_or_else(|| format!("argument '{key}' must be a whole number")),
        // A small model writes `"8"` as often as `8`.
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| format!("argument '{key}' must be a whole number")),
        Some(_) => Err(format!("argument '{key}' must be a whole number")),
    }
}

pub(super) fn num_arg(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| format!("argument '{key}' must be a number")),
        Some(_) => Err(format!("argument '{key}' must be a number")),
    }
}

/// The `model` argument, which must be one of tool `name`'s choices; left
/// out, the tool's default model when one resolves (design §3.8).
pub(super) fn model<'a>(
    args: &'a Map<String, Value>,
    offer: &'a Offer,
    name: &str,
) -> Result<&'a str, String> {
    let m = match str_arg(args, "model")? {
        Some(m) => m,
        None => match (offer.default_for(name), offer.default_gone(name)) {
            (Some(d), _) => return Ok(d.name.as_str()),
            // The tool block promised it, and it went since (design §3.8).
            (None, Some(gone)) => {
                return Err(format!(
                    "the default model {gone} this conversation's tools named is not available \
                     any more; name a model: {}",
                    names(offer, name)
                ))
            }
            (None, None) => return Err("argument 'model' is required".to_string()),
        },
    };
    let choices = offer.choices(name);
    if choices.iter().any(|c| c.name == m) {
        return Ok(m);
    }
    Err(if choices.is_empty() {
        format!("{name} has no model to run on right now")
    } else {
        format!(
            "model '{m}' is not one {name} can use; pick one of: {}",
            names(offer, name)
        )
    })
}

/// Tool `name`'s choices, as a list.
fn names(offer: &Offer, name: &str) -> String {
    let choices = offer.choices(name);
    if choices.is_empty() {
        return "none right now".to_string();
    }
    choices
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A language as a short code, or the refusal saying what it wants.
pub(super) fn language(args: &Map<String, Value>) -> Result<Option<String>, String> {
    let Some(l) = str_arg(args, "language")? else {
        return Ok(None);
    };
    let l = l.trim().to_ascii_lowercase();
    if l.len() < 2 || l.len() > 3 || !l.chars().all(|c| c.is_ascii_lowercase()) {
        return Err(format!(
            "language '{l}' is not a two-letter language code like en or de"
        ));
    }
    Ok(Some(l))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn numbers_take_the_spellings_a_small_model_writes() {
        let a = m(json!({"steps": "8", "cfg": "1.5", "seed": 7.0, "bad": "x"}));
        assert_eq!(int_arg(&a, "steps"), Ok(Some(8)));
        assert_eq!(num_arg(&a, "cfg"), Ok(Some(1.5)));
        assert_eq!(int_arg(&a, "seed"), Ok(Some(7)));
        assert!(int_arg(&a, "bad").unwrap_err().contains("whole number"));
        assert_eq!(int_arg(&a, "absent"), Ok(None));
    }

    #[test]
    fn a_model_outside_the_enum_is_refused_with_the_choices() {
        let offer = Offer {
            generate: vec![super::super::list::Choice {
                name: "image/a".into(),
                line: String::new(),
                row: true,
            }],
            ..Default::default()
        };
        let a = m(json!({"model": "image/b"}));
        let e = model(&a, &offer, super::super::IMAGE_GENERATE).unwrap_err();
        assert!(e.contains("image/a"), "{e}");
        let ok = m(json!({"model": "image/a"}));
        assert_eq!(
            model(&ok, &offer, super::super::IMAGE_GENERATE),
            Ok("image/a")
        );
    }

    #[test]
    fn a_language_is_a_short_code() {
        assert_eq!(
            language(&m(json!({"language": "DE"}))),
            Ok(Some("de".into()))
        );
        assert!(language(&m(json!({"language": "German"}))).is_err());
        assert_eq!(language(&m(json!({}))), Ok(None));
    }
}
