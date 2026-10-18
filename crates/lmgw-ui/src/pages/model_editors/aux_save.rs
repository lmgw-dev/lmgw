//! What the aux editor's Save sends to `aux_model_set`: the whole form as
//! one patch, built from the form's text so it can be checked without a
//! page. Blank fields are named in `clear`, so a saved value can be unset.

use serde_json::{json, Value};

use super::aux_fields::{lines, FieldsDraft};
use super::num_field;

/// The form as typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Draft {
    /// The saved row's id; `None` creates one.
    pub id: Option<i64>,
    pub model_id: String,
    pub gguf_path: String,
    pub kind: String,
    pub pooling: String,
    pub ctx_size: String,
    pub idle_seconds: String,
    /// The extra llama-server flags, one argument per line.
    pub args: String,
    pub image: String,
    /// The `podman run` args override, one argument per line; blank inherits.
    pub extra_run_args: String,
    pub warm_start: bool,
    pub hold_fallback_mode: String,
    pub hold_fallback: Option<String>,
    pub fields: FieldsDraft,
}

/// A reranker takes no pooling at all (`--reranking` selects rank pooling
/// itself and a second key breaks it), and neither does a decision model,
/// whose server reads the head the file declares (decision-models §3.5).
pub(super) fn takes_no_pooling(kind: &str) -> bool {
    matches!(kind, "rerank" | "decision")
}

/// The `aux_model_set` patch for `d`, or what in it cannot be saved.
pub(super) fn save_body(d: &Draft) -> Result<Value, String> {
    let ctx: Option<i64> = num_field("context size", &d.ctx_size)?;
    let idle: i64 = d
        .idle_seconds
        .trim()
        .parse()
        .map_err(|_| "idle seconds must be an integer".to_string())?;
    let (row_fields, mut clear) = d.fields.body()?;
    if ctx.is_none() {
        clear.push("ctx_size");
    }
    // Empty run args inherit the class settings, whose run args carry the
    // GPU and SELinux flags: no list is sent, and the field is named in
    // `clear` so an override saved before goes too.
    let run_args = lines(&d.extra_run_args);
    if run_args.is_empty() {
        clear.push("extra_run_args");
    }
    let mut body = json!({
        "action": if d.id.is_some() { "update" } else { "create" },
        "id": d.id,
        "model_id": d.model_id.trim(),
        "gguf_path": d.gguf_path.trim(),
        "kind": d.kind,
        "pooling": if takes_no_pooling(&d.kind) { "" } else { d.pooling.as_str() },
        "ctx_size": ctx,
        "hold_fallback_mode": d.hold_fallback_mode,
        "hold_fallback": d.hold_fallback,
        "args": lines(&d.args),
        "idle_seconds": idle,
        "image": d.image.trim(),
        "extra_run_args": if run_args.is_empty() { Value::Null } else { json!(run_args) },
        "warm_start": d.warm_start,
        "clear": clear.join(","),
    });
    if let Some(b) = body.as_object_mut() {
        b.extend(row_fields);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `aux_model_set`'s patch (`ops::aux_model::AuxModelPatch`, which denies
    /// unknown fields): `args` is its alias of `extra_args`.
    const ACCEPTED: &[&str] = &[
        "action",
        "id",
        "model_id",
        "gguf_path",
        "kind",
        "pooling",
        "ctx_size",
        "ubatch_size",
        "batch_size",
        "mmproj_path",
        "backend",
        "threads",
        "args",
        "idle_seconds",
        "enabled",
        "image",
        "extra_run_args",
        "warm_start",
        "hold_fallback_mode",
        "hold_fallback",
        "clear",
    ];

    fn draft(kind: &str) -> Draft {
        Draft {
            id: Some(3),
            model_id: " laya ".into(),
            gguf_path: "decision-models/Laya-Q8_0.gguf".into(),
            kind: kind.into(),
            pooling: "mean".into(),
            ctx_size: "8192".into(),
            idle_seconds: "300".into(),
            args: "--flash-attn\non\n".into(),
            image: String::new(),
            extra_run_args: String::new(),
            warm_start: false,
            hold_fallback_mode: "inherit".into(),
            hold_fallback: None,
            fields: FieldsDraft {
                ubatch_size: "8192".into(),
                batch_size: "8192".into(),
                mmproj_path: String::new(),
                backend: "cpu".into(),
                threads: String::new(),
            },
        }
    }

    fn keys(v: &Value) -> Vec<&str> {
        v.as_object().unwrap().keys().map(String::as_str).collect()
    }

    #[test]
    fn every_key_is_one_the_op_accepts() {
        for kind in ["embed", "rerank", "decision"] {
            let b = save_body(&draft(kind)).unwrap();
            for k in keys(&b) {
                assert!(ACCEPTED.contains(&k), "{kind}: '{k}' is refused by the op");
            }
        }
    }

    #[test]
    fn only_an_embedder_sends_its_pooling() {
        assert_eq!(save_body(&draft("embed")).unwrap()["pooling"], "mean");
        assert_eq!(save_body(&draft("rerank")).unwrap()["pooling"], "");
        assert_eq!(save_body(&draft("decision")).unwrap()["pooling"], "");
    }

    #[test]
    fn a_decision_row_sends_its_fields_and_clears_its_blanks() {
        let b = save_body(&draft("decision")).unwrap();
        assert_eq!(b["action"], "update");
        assert_eq!(b["id"], 3);
        assert_eq!(b["model_id"], "laya");
        assert_eq!(b["ctx_size"], 8192);
        assert_eq!(
            (b["ubatch_size"].clone(), b["batch_size"].clone()),
            (json!(8192), json!(8192))
        );
        assert_eq!(b["backend"], "cpu");
        assert_eq!(b["args"], json!(["--flash-attn", "on"]));
        assert!(b["extra_run_args"].is_null());
        assert_eq!(b["clear"], "threads,mmproj_path,extra_run_args");
    }

    #[test]
    fn blanks_are_named_in_clear_with_the_run_args_and_the_context() {
        let mut d = draft("embed");
        d.id = None;
        d.ctx_size = " ".into();
        d.extra_run_args = "--cpus\n4\n".into();
        d.fields = FieldsDraft::default();
        let b = save_body(&d).unwrap();
        assert_eq!(b["action"], "create");
        assert!(b["id"].is_null());
        assert_eq!(b["extra_run_args"], json!(["--cpus", "4"]));
        assert_eq!(
            b["clear"],
            "ubatch_size,batch_size,threads,mmproj_path,backend,ctx_size"
        );
        assert!(b.get("ubatch_size").is_none() && b.get("backend").is_none());
    }

    #[test]
    fn what_cannot_be_saved_is_said() {
        let mut d = draft("embed");
        d.idle_seconds = "soon".into();
        assert_eq!(
            save_body(&d).unwrap_err(),
            "idle seconds must be an integer"
        );
        let mut d = draft("embed");
        d.ctx_size = "big".into();
        assert!(save_body(&d).unwrap_err().starts_with("context size"));
        let mut d = draft("decision");
        d.fields.threads = "0".into();
        assert!(save_body(&d).unwrap_err().starts_with("threads"));
    }
}
