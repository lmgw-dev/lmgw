//! An override with no top-level `model` (labs-store design §2.4): the route's
//! own rewrite (`SystemOneBody::with_model`) only replaces a member that is
//! there, so such an override would reach every compared model as the
//! route's 400 while its run named the model asked. The lab inserts the
//! cell's model as the object's first member instead; every other byte stays
//! as typed. The route is not changed: a client that sends no `model` gets
//! its 400.

use std::borrow::Cow;

use lmgw_api_types::systemone::Ordered;
use serde_json::value::RawValue;

/// `text` with `"model":<model>` as its first member when its top level has
/// no `model`; `text` itself when it has one. `Err` when it is no JSON
/// object.
pub(crate) fn ensure_model<'a>(text: &'a str, model: &str) -> Result<Cow<'a, str>, String> {
    let members: Ordered<&RawValue> =
        serde_json::from_str(text).map_err(|e| format!("not a JSON object: {e}"))?;
    if members.get("model").is_some() {
        return Ok(Cow::Borrowed(text));
    }
    // A JSON object's first byte past any whitespace is its brace.
    let open = text
        .find('{')
        .ok_or_else(|| "not a JSON object".to_string())?;
    let name = serde_json::to_string(model).map_err(|e| e.to_string())?;
    let sep = if members.is_empty() { "" } else { "," };
    Ok(Cow::Owned(format!(
        "{}{{\"model\":{name}{sep}{}",
        &text[..open],
        &text[open + 1..]
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_becomes_the_first_member_and_nothing_else_moves() {
        let typed = " {\n  \"state\": \"s\",  \"questions\": {\"q\": {\"type\": \"noul\"}}\n}";
        assert_eq!(
            ensure_model(typed, "decide/laya").unwrap(),
            " {\"model\":\"decide/laya\",\n  \"state\": \"s\",  \"questions\": {\"q\": {\"type\": \"noul\"}}\n}"
        );
        assert_eq!(ensure_model("{ }", "m").unwrap(), "{\"model\":\"m\" }");
        // One that names a model, even a nested `model` aside, is left as typed.
        let named = "{\"state\": {\"model\": 1}, \"model\": \"x\"}";
        assert!(matches!(ensure_model(named, "m").unwrap(), Cow::Borrowed(t) if t == named));
        let nested_only = "{\"state\": {\"model\": 1}}";
        assert_eq!(
            ensure_model(nested_only, "m").unwrap(),
            "{\"model\":\"m\",\"state\": {\"model\": 1}}"
        );
        assert!(ensure_model("[1]", "m").is_err());
    }
}
