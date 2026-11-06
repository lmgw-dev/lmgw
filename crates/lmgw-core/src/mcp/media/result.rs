//! What a media call says and hands back (chat-media-tools design §3.5).
//!
//! | Tool | In a Chat thread | On `/mcp` and thread-less callers |
//! |---|---|---|
//! | `image__*` | text + `Resource{lmgw:attachment/N}` per picture | text + the picture |
//! | `audio__speak` | text only; the WAV is stored for the player | text + the audio |
//! | `audio__transcribe` | the transcript | the transcript |
//!
//! The text says what happened in words a model can relay; a refusal is an
//! `isError` result with the gateway's own message.

use base64::Engine as _;
use lmgw_api_types::chat_media::MediaFacts;
use serde_json::{json, Value};

use crate::ir::ToolResultBlock;

/// The URI scheme of a stored file in a turn record (design §6.4).
pub(crate) const ATTACHMENT_URI: &str = "lmgw:attachment/";

/// A file a call made.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Produced {
    /// The attachment kind it is stored as: `image` or `audio`.
    pub kind: &'static str,
    pub mime: String,
    /// Its file name (`<model-stem>-<seed>.png`, `<model-stem>-<voice>.wav`).
    pub name: String,
    pub bytes: Vec<u8>,
    pub facts: MediaFacts,
}

impl Produced {
    /// The chat model is shown it: a picture is, a recording is not (§3.5:
    /// a chat model that hears a tool result's audio is rare, and the bytes
    /// would cost its context for nothing).
    pub fn shown(&self) -> bool {
        self.kind == "image"
    }
}

/// What the result says.
#[derive(Debug, Clone, PartialEq)]
enum Said {
    /// The same in every context: a transcript, a refusal.
    Plain(String),
    /// A sentence around the files' refs in a thread (`before`, the refs,
    /// `after`), and its form where there is no thread (`bare`).
    Made {
        before: String,
        after: String,
        bare: String,
    },
}

/// One call's outcome.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub tool: String,
    pub is_error: bool,
    pub files: Vec<Produced>,
    said: Said,
}

impl Outcome {
    pub fn error(tool: &str, message: impl Into<String>) -> Self {
        Self {
            tool: tool.to_string(),
            is_error: true,
            files: Vec::new(),
            said: Said::Plain(message.into()),
        }
    }

    pub fn text(tool: &str, text: impl Into<String>) -> Self {
        Self {
            tool: tool.to_string(),
            is_error: false,
            files: Vec::new(),
            said: Said::Plain(text.into()),
        }
    }

    /// Files and a sentence about them: `before` + the refs + `after` in a
    /// thread, `bare` elsewhere.
    pub fn made(
        tool: &str,
        files: Vec<Produced>,
        (before, after): (String, String),
        bare: String,
    ) -> Self {
        Self {
            tool: tool.to_string(),
            is_error: false,
            files,
            said: Said::Made {
                before,
                after,
                bare,
            },
        }
    }

    /// The text, with the stored files' `ids` as `att:N` when a thread
    /// stored them.
    pub fn text_for(&self, ids: Option<&[i64]>) -> String {
        match (&self.said, ids) {
            (Said::Plain(t), _) => t.clone(),
            (Said::Made { before, after, .. }, Some(ids)) => {
                let refs: Vec<String> = ids.iter().map(|i| format!("att:{i}")).collect();
                format!("{before}{}{after}", refs.join(", "))
            }
            (Said::Made { bare, .. }, None) => bare.clone(),
        }
    }

    /// The result in a thread whose files are `ids`: the text, then a
    /// resource per picture the model is shown, which every send turns back
    /// into the picture (design §6.4).
    pub fn thread_blocks(&self, ids: &[i64]) -> Vec<ToolResultBlock> {
        let mut out = vec![ToolResultBlock::text(self.text_for(Some(ids)))];
        for (f, id) in self.files.iter().zip(ids) {
            if f.shown() {
                out.push(ToolResultBlock::Resource {
                    uri: format!("{ATTACHMENT_URI}{id}"),
                    mime: Some(f.mime.clone()),
                    text: None,
                });
            }
        }
        out
    }

    /// The result with no thread: the text, then each file inline.
    pub fn inline_blocks(&self) -> Vec<ToolResultBlock> {
        let mut out = vec![ToolResultBlock::text(self.text_for(None))];
        for f in &self.files {
            let data = base64::engine::general_purpose::STANDARD.encode(&f.bytes);
            out.push(match f.kind {
                "audio" => ToolResultBlock::Audio {
                    mime: f.mime.clone(),
                    data,
                },
                _ => ToolResultBlock::Image {
                    mime: f.mime.clone(),
                    data,
                },
            });
        }
        out
    }

    /// The result as MCP's `CallToolResult` (`/mcp`).
    pub fn mcp_result(&self) -> Value {
        let content: Vec<Value> = self
            .inline_blocks()
            .into_iter()
            .map(|b| match b {
                ToolResultBlock::Text { text } => json!({"type": "text", "text": text}),
                ToolResultBlock::Image { mime, data } => {
                    json!({"type": "image", "data": data, "mimeType": mime})
                }
                ToolResultBlock::Audio { mime, data } => {
                    json!({"type": "audio", "data": data, "mimeType": mime})
                }
                other => json!({"type": "text", "text": format!("{other:?}")}),
            })
            .collect();
        json!({"content": content, "isError": self.is_error})
    }
}

/// `ids`' attachment id from a record's resource URI.
pub(crate) fn attachment_of_uri(uri: &str) -> Option<i64> {
    uri.strip_prefix(ATTACHMENT_URI)?.parse().ok()
}

/// Seconds as a person reads them: `4.7 s`.
pub(super) fn secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture() -> Produced {
        Produced {
            kind: "image",
            mime: "image/png".into(),
            name: "image-x-1.png".into(),
            bytes: vec![1, 2, 3],
            facts: MediaFacts::default(),
        }
    }

    #[test]
    fn a_thread_names_its_files_and_keeps_only_refs_in_the_record() {
        let o = Outcome::made(
            "image__generate",
            vec![picture()],
            ("Made image ".into(), " (64x64).".into()),
            "Made an image (64x64).".into(),
        );
        assert_eq!(o.text_for(Some(&[57])), "Made image att:57 (64x64).");
        let blocks = o.thread_blocks(&[57]);
        assert_eq!(
            blocks[1],
            ToolResultBlock::Resource {
                uri: "lmgw:attachment/57".into(),
                mime: Some("image/png".into()),
                text: None,
            }
        );
        let mcp = o.mcp_result();
        assert_eq!(mcp["content"][0]["text"], json!("Made an image (64x64)."));
        assert_eq!(mcp["content"][1]["type"], json!("image"));
        assert_eq!(mcp["content"][1]["data"], json!("AQID"));
        assert_eq!(mcp["isError"], json!(false));
    }

    #[test]
    fn a_recording_is_never_shown_to_the_model_in_a_thread() {
        let mut f = picture();
        f.kind = "audio";
        f.mime = "audio/wav".into();
        let o = Outcome::made("audio__speak", vec![f], ("".into(), "".into()), "".into());
        assert_eq!(o.thread_blocks(&[3]).len(), 1);
        assert_eq!(o.mcp_result()["content"][1]["type"], json!("audio"));
    }

    #[test]
    fn uris_and_stems() {
        assert_eq!(attachment_of_uri("lmgw:attachment/42"), Some(42));
        assert_eq!(attachment_of_uri("lmgw:attachment/x"), None);
        assert_eq!(secs(4700), "4.7 s");
    }
}
