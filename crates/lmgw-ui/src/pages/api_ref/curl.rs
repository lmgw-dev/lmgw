//! "Copy as curl" (api-docs design §6.9): pure string building — quoting,
//! the JSON heredoc, multipart `-F`, raw file bodies, and secret redaction.
//!
//! [`Prepared`] is deliberately its own shape, not `send::Prepared`: the pasted
//! key never appears in a curl (§6.9), and keeping the two types apart makes
//! that a fact about what this module's input even *can* contain, not a
//! behaviour someone has to remember to preserve at every call site.

use serde_json::Value;

use super::identity::Identity;

/// A part of a multipart body (§6.6): a plain field, or a picked file — only
/// the file's *name* is ever known here, never its bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum MultipartPart {
    Field(String, String),
    File(String, String),
}

/// A request body shape the tester can hold.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    None,
    /// `secret_props` are the property names `x-lmgw-secret` marks anywhere
    /// in the body's schema (§4.4) — every object key of that name, at any
    /// depth, is redacted to `"<secret: name>"` before printing. The whole
    /// value is: an object of forge tokens or a list of header pairs becomes
    /// the one placeholder string, never a structure with some leaves left.
    Json {
        value: Value,
        secret_props: Vec<String>,
    },
    Multipart {
        parts: Vec<MultipartPart>,
        secret_props: Vec<String>,
    },
    /// A raw (`Req::Raw`) body: only the picked file's name is known, plus
    /// the content type the tester sends it as.
    Raw {
        file_name: String,
        content_type: String,
    },
}

/// Everything `curl_command` needs: the method, the path with its query
/// string, extra headers (not `Authorization`, derived from `identity`), and
/// the body.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub identity: Identity,
    pub stream: bool,
    pub body: Body,
}

const LMGW_KEY_COMMENT: &str = "# export LMGW_KEY=…; replace <secret: …> placeholders";

/// Single-quote a shell word, escaping embedded `'`s the POSIX way
/// (`'` → `'\''`, i.e. close the quote, an escaped quote, reopen it).
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double-quote the URL the way §6.9's worked example shows it.
fn dq(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

/// The `"<secret: name>"` placeholder §6.9 prints in place of a secret.
pub fn placeholder(name: &str) -> String {
    format!("<secret: {name}>")
}

/// Headers that carry a credential: never printed with their value, whoever
/// typed them into "Extra headers" (§6.9 "never included: the pasted key,
/// the cookie").
pub fn is_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "x-lmgw-admin-token" | "cookie"
    )
}

/// Every object key named in `secret_props`, at any depth — a secret nested
/// in a patch object is as secret as a top-level one.
fn redact(value: &Value, secret_props: &[String]) -> (Value, bool) {
    match value {
        Value::Object(map) => {
            let mut any = false;
            let out = map
                .iter()
                .map(|(k, v)| {
                    if secret_props.contains(k) && !v.is_null() {
                        any = true;
                        (k.clone(), Value::String(placeholder(k)))
                    } else {
                        let (v, a) = redact(v, secret_props);
                        any |= a;
                        (k.clone(), v)
                    }
                })
                .collect();
            (Value::Object(out), any)
        }
        Value::Array(items) => {
            let mut any = false;
            let out = items
                .iter()
                .map(|v| {
                    let (v, a) = redact(v, secret_props);
                    any |= a;
                    v
                })
                .collect();
            (Value::Array(out), any)
        }
        other => (other.clone(), false),
    }
}

/// Build the `curl` command text for `p`, run against `origin`
/// (`location.origin`, written literally — §6.9).
pub fn curl_command(p: &Prepared, origin: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut placeholders = p.identity != Identity::None;

    let mut first = String::from("curl -sS");
    if p.stream {
        first.push_str(" -N");
    }
    first.push_str(&format!(" -X {}", p.method));
    first.push(' ');
    first.push_str(&dq(&format!("{origin}{}", p.path)));
    lines.push(first);

    // Session and Key identities both use the shell variable, never the
    // pasted key itself — this module never even receives it (§6.9).
    if p.identity != Identity::None {
        lines.push("  -H \"Authorization: Bearer $LMGW_KEY\"".to_string());
    }
    for (name, value) in &p.headers {
        let value = if is_credential_header(name) {
            placeholders = true;
            placeholder(name)
        } else {
            value.clone()
        };
        lines.push(format!("  -H {}", sq(&format!("{name}: {value}"))));
    }

    let mut heredoc_body: Option<String> = None;
    match &p.body {
        Body::None => {}
        Body::Json {
            value,
            secret_props,
        } => {
            let (redacted, any) = redact(value, secret_props);
            placeholders = placeholders || any;
            lines.push(format!("  -H {}", sq("Content-Type: application/json")));
            lines.push("  --data-binary @- <<'LMGW_JSON'".to_string());
            heredoc_body = Some(serde_json::to_string_pretty(&redacted).unwrap_or_default());
        }
        Body::Multipart {
            parts,
            secret_props,
        } => {
            for part in parts {
                match part {
                    MultipartPart::Field(name, value) => {
                        let v = if secret_props.contains(name) {
                            placeholders = true;
                            placeholder(name)
                        } else {
                            value.clone()
                        };
                        lines.push(format!("  -F {}", sq(&format!("{name}={v}"))));
                    }
                    MultipartPart::File(field, file_name) => {
                        lines.push(format!("  -F {}", sq(&format!("{field}=@{file_name}"))));
                    }
                }
            }
        }
        Body::Raw {
            file_name,
            content_type,
        } => {
            if !content_type.is_empty() {
                lines.push(format!(
                    "  -H {}",
                    sq(&format!("Content-Type: {content_type}"))
                ));
            }
            // Quoted like every other word: a picked file's name is whatever
            // the file system allowed (spaces, `;`, `$`…).
            lines.push(format!("  --data-binary {}", sq(&format!("@{file_name}"))));
        }
    }

    let mut out = lines.join(" \\\n");
    if let Some(body) = heredoc_body {
        out.push('\n');
        out.push_str(&body);
        out.push_str("\nLMGW_JSON");
    }
    if placeholders {
        format!("{LMGW_KEY_COMMENT}\n{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(body: Body) -> Prepared {
        Prepared {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            headers: Vec::new(),
            identity: Identity::None,
            stream: false,
            body,
        }
    }

    #[test]
    fn a_simple_get_quotes_the_url_and_has_no_auth_header() {
        let p = Prepared {
            method: "GET".into(),
            path: "/api/status".into(),
            headers: Vec::new(),
            identity: Identity::None,
            stream: false,
            body: Body::None,
        };
        let cmd = curl_command(&p, "http://127.0.0.1:8899");
        assert_eq!(cmd, "curl -sS -X GET \"http://127.0.0.1:8899/api/status\"");
        assert!(!cmd.contains("Authorization"));
    }

    #[test]
    fn session_and_key_identities_both_use_the_shell_variable_never_a_real_key() {
        for identity in [Identity::Session, Identity::Key] {
            let mut p = base(Body::None);
            p.identity = identity;
            let cmd = curl_command(&p, "http://x");
            assert!(cmd.contains("\"Authorization: Bearer $LMGW_KEY\""));
            assert!(cmd.starts_with(LMGW_KEY_COMMENT));
        }
    }

    #[test]
    fn extra_headers_are_single_quoted_and_escaped() {
        let mut p = base(Body::None);
        p.headers
            .push(("x-lmgw-reasoning".into(), "it's off".into()));
        let cmd = curl_command(&p, "http://x");
        assert!(cmd.contains("-H 'x-lmgw-reasoning: it'\\''s off'"));
    }

    #[test]
    fn the_dash_n_flag_only_appears_when_streaming() {
        let mut p = base(Body::None);
        p.stream = true;
        assert!(curl_command(&p, "http://x").contains(" -N "));
        p.stream = false;
        assert!(!curl_command(&p, "http://x").contains(" -N "));
    }

    #[test]
    fn a_json_body_is_sent_as_a_quoted_heredoc_verbatim() {
        let p = base(Body::Json {
            value: json!({"model": "m", "stream": true}),
            secret_props: vec![],
        });
        let cmd = curl_command(&p, "http://x");
        assert!(cmd.contains("--data-binary @- <<'LMGW_JSON'"));
        assert!(cmd.trim_end().ends_with("LMGW_JSON"));
        assert!(cmd.contains("\"model\": \"m\""));
    }

    #[test]
    fn multipart_uses_dash_f_for_fields_and_files() {
        let p = base(Body::Multipart {
            parts: vec![
                MultipartPart::Field("model".into(), "mock-tts".into()),
                MultipartPart::File("file".into(), "clip.wav".into()),
            ],
            secret_props: vec![],
        });
        let cmd = curl_command(&p, "http://x");
        assert!(cmd.contains("-F 'model=mock-tts'"));
        assert!(cmd.contains("-F 'file=@clip.wav'"));
    }

    #[test]
    fn a_raw_body_uses_data_binary_with_the_quoted_file_name_and_its_type() {
        let p = base(Body::Raw {
            file_name: "my corpus; rm -rf ~.sqlite".into(),
            content_type: "application/vnd.sqlite3".into(),
        });
        let cmd = curl_command(&p, "http://x");
        assert!(cmd.contains("--data-binary '@my corpus; rm -rf ~.sqlite'"));
        assert!(cmd.contains("-H 'Content-Type: application/vnd.sqlite3'"));
    }

    #[test]
    fn nested_secrets_are_redacted_too() {
        let p = base(Body::Json {
            value: json!({"patch": {"api_key": "sk-nested"}, "list": [{"token": "t-1"}]}),
            secret_props: vec!["api_key".to_string(), "token".to_string()],
        });
        let cmd = curl_command(&p, "http://x");
        assert!(!cmd.contains("sk-nested"));
        assert!(!cmd.contains("t-1"));
        assert!(cmd.contains("<secret: api_key>"));
        assert!(cmd.starts_with(LMGW_KEY_COMMENT));
    }

    #[test]
    fn an_object_or_list_valued_secret_is_one_placeholder() {
        // Review R2 #3's fields: header pairs, forge tokens by host, an
        // agent's config values — each replaced whole.
        let p = base(Body::Json {
            value: json!({
                "action": "update",
                "extra_headers": [["Authorization", "Bearer sk-in-a-pair"]],
                "forge_tokens": {"git.example.dev": "glpat-by-host"},
                "values": {"imap_password": "hunter2", "folder": "INBOX"},
            }),
            secret_props: vec![
                "extra_headers".to_string(),
                "forge_tokens".to_string(),
                "values".to_string(),
            ],
        });
        let cmd = curl_command(&p, "http://x");
        for leaked in [
            "sk-in-a-pair",
            "glpat-by-host",
            "hunter2",
            "INBOX",
            "git.example.dev",
        ] {
            assert!(!cmd.contains(leaked), "{leaked} leaked into {cmd}");
        }
        assert!(cmd.contains("\"extra_headers\": \"<secret: extra_headers>\""));
        assert!(cmd.contains("\"forge_tokens\": \"<secret: forge_tokens>\""));
        assert!(cmd.contains("\"values\": \"<secret: values>\""));
        assert!(cmd.contains("\"action\": \"update\""));
        assert!(cmd.starts_with(LMGW_KEY_COMMENT));
    }

    #[test]
    fn a_typed_credential_header_never_prints_its_value() {
        let mut p = base(Body::None);
        p.headers
            .push(("Authorization".into(), "Bearer sk-typed-by-hand".into()));
        p.headers
            .push(("x-lmgw-admin-token".into(), "admin-secret".into()));
        let cmd = curl_command(&p, "http://x");
        assert!(!cmd.contains("sk-typed-by-hand"));
        assert!(!cmd.contains("admin-secret"));
        assert!(cmd.contains("-H 'x-lmgw-admin-token: <secret: x-lmgw-admin-token>'"));
        assert!(cmd.starts_with(LMGW_KEY_COMMENT));
    }

    #[test]
    fn a_secret_property_is_redacted_with_a_placeholder_and_flagged() {
        let p = base(Body::Json {
            value: json!({"name": "n", "token": "sk-real-secret-value"}),
            secret_props: vec!["token".to_string()],
        });
        let cmd = curl_command(&p, "http://x");
        assert!(cmd.contains("<secret: token>"));
        assert!(!cmd.contains("sk-real-secret-value"));
        assert!(cmd.starts_with(LMGW_KEY_COMMENT));
    }

    #[test]
    fn no_placeholder_comment_when_nothing_needs_replacing() {
        let p = base(Body::Json {
            value: json!({"a": 1}),
            secret_props: vec![],
        });
        let cmd = curl_command(&p, "http://x");
        assert!(!cmd.starts_with(LMGW_KEY_COMMENT));
    }
}
