//! Structured output on llama.cpp's legacy `/v1/completions`
//! (structured-output design §11.22).
//!
//! The route reads no `response_format`. llama-server (official master of
//! 2026-09-26, `95887577`) hands the raw body of `POST /v1/completions` to
//! `handle_completions_impl` (`tools/server/server-context.cpp`,
//! `post_completions_oai`), whose parameter reader knows only the top-level
//! `json_schema` and `grammar` (`tools/server/server-schema.cpp`, the
//! `json_schema` field: `data.contains("json_schema") &&
//! !data.contains("grammar")`). ik_llama.cpp (`15e7ecac`, 2026-10-21) parses
//! that route's body with the one-argument `oaicompat_chat_params_parse`,
//! which copies it through unread, and the same reader follows
//! (`examples/server/server-common.cpp`, `server-context.cpp`). A
//! `response_format` there is dropped without a word — so lmgw writes it in
//! the spelling both servers read: the schema the chat route would apply, as
//! the top-level `json_schema`.

use serde_json::{json, Value};

use crate::error::GatewayError;

/// Rewrite a legacy completions body's `response_format` into llama.cpp's
/// top-level `json_schema`, in place — the schema llama-server's chat route
/// would take from the same object (`oaicompat_chat_params_parse`):
///
/// - `json_schema`: `json_schema.schema`, `{}` (any object) without one;
/// - `json_object`: its `schema` (llama.cpp's extension), else a top-level
///   `json_schema` the client sent beside it, else `{}`;
/// - `text`, `null` or none: no format; the field goes, a top-level
///   `json_schema` stays as sent.
///
/// Refused where the route would otherwise drop the format ([`check`]).
pub fn apply(body: &mut Value) -> Result<(), GatewayError> {
    let rewrite = plan(body)?;
    let Some(o) = body.as_object_mut() else {
        return Ok(());
    };
    match rewrite {
        Rewrite::Keep => {}
        Rewrite::Drop => {
            o.remove("response_format");
        }
        Rewrite::Schema(schema) => {
            o.remove("response_format");
            o.insert("json_schema".into(), schema);
        }
    }
    // The reader applies `json_schema` only where the body has no `grammar`
    // key at all, so an empty one (`""`, `null`) would silently void it.
    if o.get("json_schema").is_some_and(|s| !s.is_null())
        && o.get("grammar").is_some_and(|g| !grammar_set(g))
    {
        o.remove("grammar");
    }
    Ok(())
}

/// Whether [`apply`] would take the body — what the gate asks before
/// admission, so nothing is started for a body it refuses: any other type
/// (`response_format_unsupported`), a malformed object, and a JSON type next
/// to a `grammar` (`bad_request`) — the route's reader takes the grammar and
/// ignores the schema, where the chat route refuses the pair.
pub fn check(body: &Value) -> Result<(), GatewayError> {
    plan(body).map(|_| ())
}

/// What [`apply`] does to a body.
enum Rewrite {
    /// No `response_format`: the body goes as sent.
    Keep,
    /// `text` or `null`: the field goes, nothing else changes.
    Drop,
    /// A JSON type: the field goes, this is the top-level `json_schema`.
    Schema(Value),
}

/// A `grammar` that asks for something: neither `null` nor `""`.
fn grammar_set(g: &Value) -> bool {
    !g.is_null() && g.as_str() != Some("")
}

/// A top-level `json_schema` the outgoing body carries next to a set
/// `grammar` is refused whichever way it got there: the reader applies the
/// grammar and drops the schema, where the chat route refuses the pair.
fn refuse_pair(body: &Value, schema_out: bool) -> Result<(), GatewayError> {
    if schema_out && body.get("grammar").is_some_and(grammar_set) {
        return Err(GatewayError::BadRequest(
            "Cannot use both json_schema and grammar — llama.cpp's /v1/completions applies the \
             grammar and ignores a schema; send one of them"
                .into(),
        ));
    }
    Ok(())
}

fn plan(body: &Value) -> Result<Rewrite, GatewayError> {
    let sent_schema = body.get("json_schema").is_some_and(|s| !s.is_null());
    let Some(format) = body.get("response_format") else {
        refuse_pair(body, sent_schema)?;
        return Ok(Rewrite::Keep);
    };
    if format.is_null() {
        refuse_pair(body, sent_schema)?;
        return Ok(Rewrite::Drop);
    }
    let f = format
        .as_object()
        .ok_or_else(|| GatewayError::BadRequest("'response_format' must be an object".into()))?;
    let t = f
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest("'response_format' has no string 'type'".into()))?;
    let schema = match t {
        "text" => {
            refuse_pair(body, sent_schema)?;
            return Ok(Rewrite::Drop);
        }
        "json_schema" => f
            .get("json_schema")
            .and_then(|w| w.get("schema"))
            .filter(|s| !s.is_null()),
        "json_object" => f
            .get("schema")
            .filter(|s| !s.is_null())
            .or_else(|| body.get("json_schema").filter(|s| !s.is_null())),
        other => {
            return Err(GatewayError::ResponseFormatUnsupported {
                message: format!(
                    "response_format type '{other}' has no spelling on llama.cpp's \
                     /v1/completions, which reads a top-level json_schema only — use \
                     json_schema, json_object or text"
                ),
            })
        }
    };
    refuse_pair(body, true)?;
    Ok(Rewrite::Schema(
        schema.cloned().unwrap_or_else(|| json!({})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(body: Value) -> Result<Value, &'static str> {
        let mut b = body;
        apply(&mut b).map(|()| b).map_err(|e| e.kind())
    }

    #[test]
    fn each_type_becomes_the_schema_the_chat_route_would_apply() {
        let s = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let p = json!("Reply with ok.");
        for (rf, top, want) in [
            (
                json!({"type": "json_schema", "json_schema": {"name": "a", "schema": s}}),
                None,
                Some(s.clone()),
            ),
            (json!({"type": "json_schema"}), None, Some(json!({}))),
            (json!({"type": "json_object"}), None, Some(json!({}))),
            (
                json!({"type": "json_object", "schema": s}),
                Some(json!({"type": "string"})),
                Some(s.clone()),
            ),
            (
                json!({"type": "json_object"}),
                Some(s.clone()),
                Some(s.clone()),
            ),
            (json!({"type": "text"}), None, None),
            (json!({"type": "text"}), Some(s.clone()), Some(s.clone())),
            (Value::Null, Some(s.clone()), Some(s.clone())),
        ] {
            let mut body = json!({"prompt": p, "response_format": rf});
            if let Some(t) = &top {
                body["json_schema"] = t.clone();
            }
            let out = applied(body).unwrap();
            assert!(out.get("response_format").is_none(), "{out}");
            assert_eq!(out.get("json_schema"), want.as_ref(), "{rf} / {top:?}");
            assert_eq!(out["prompt"], p);
        }
        let plain = json!({"prompt": p, "json_schema": s});
        assert_eq!(applied(plain.clone()).unwrap(), plain);
    }

    #[test]
    fn what_the_route_would_drop_is_refused() {
        assert_eq!(
            applied(json!({"prompt": "x", "response_format": {"type": "regex"}})),
            Err("response_format_unsupported")
        );
        assert_eq!(
            applied(json!({"prompt": "x", "grammar": "root ::= \"a\"",
                           "response_format": {"type": "json_object"}})),
            Err("bad_request")
        );
        assert_eq!(
            applied(json!({"prompt": "x", "response_format": "json"})),
            Err("bad_request")
        );
        // A top-level schema beside a grammar, with no JSON format, is the
        // same pair (the chat route refuses it too).
        for rf in [None, Some(Value::Null), Some(json!({"type": "text"}))] {
            let mut b = json!({"prompt": "x", "grammar": "root ::= \"a\"",
                               "json_schema": {"type": "object"}});
            if let Some(rf) = rf {
                b["response_format"] = rf;
            }
            assert_eq!(applied(b), Err("bad_request"));
        }
        // Text next to a grammar is no format: the grammar goes as sent.
        assert!(applied(json!({"prompt": "x", "grammar": "root ::= \"a\"",
                               "response_format": {"type": "text"}}))
        .is_ok());
    }

    #[test]
    fn an_empty_grammar_goes_so_the_schema_is_applied() {
        let s = json!({"type": "object"});
        for g in [json!(""), Value::Null] {
            for body in [
                json!({"prompt": "x", "grammar": g, "response_format": {"type": "json_object"}}),
                json!({"prompt": "x", "grammar": g, "json_schema": s}),
            ] {
                let out = applied(body).unwrap();
                assert!(out.get("grammar").is_none(), "{out}");
                assert!(out.get("json_schema").is_some(), "{out}");
            }
        }
        // No schema out: the field is the client's.
        let out = applied(json!({"prompt": "x", "grammar": ""})).unwrap();
        assert_eq!(out["grammar"], "");
    }
}
