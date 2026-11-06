//! The Chat's reads of the gateway beside its threads and folders
//! (chat-plane design §2): the settings it uses, the toolsets a thread may
//! attach, the knowledge bases a thread may search and a cited file's text,
//! on `/chat/api/*` and scoped to the caller. The dashboard's Chat and a paired device read the
//! same routes; what a device is shown follows the rules its writes are
//! checked by (client-apps design L3, L5).

use serde::{Deserialize, Serialize};

use crate::knowledge::{ChunkSpan, KnowledgeSource};

/// `GET /chat/api/settings`: the gateway settings the Chat reads, and only
/// those. None is a secret, and every caller reads the same values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatSettings {
    /// Tokens of knowledge-base excerpts a turn carries when its thread sets
    /// no budget of its own (Settings → Chat → Knowledge budget).
    pub kb_budget_tokens: u32,
    /// Days after its last activity a thread is archived; `0`: never. A
    /// folder's own retention overrides it.
    pub archive_days: i64,
    /// Days after it was archived a thread is deleted; `0`: never. A
    /// folder's own retention overrides it.
    pub purge_days: i64,
    /// The Chat's text-to-speech alias; empty: [`Self::realtime_tts_alias`]
    /// is the one a thread inherits.
    pub tts_alias: String,
    /// The realtime voice's text-to-speech alias.
    pub realtime_tts_alias: String,
    /// The largest request body the gateway reads, in MiB; `0`: no bound.
    /// A dictation's recording is one such body.
    pub max_body_mb: u32,
    /// Seconds between two reads of an MCP task's status (Settings → MCP);
    /// `0`: none is set.
    pub task_poll_interval_s: u32,
    /// The audio a realtime session keeps from before speech starts, in
    /// milliseconds: push-to-talk's pre-roll.
    pub realtime_prefix_padding_ms: u32,
    /// Seconds between the gateway's pings on a realtime session.
    pub realtime_ping_interval_s: u32,
}

/// `GET /chat/api/tools`: the toolsets the caller may attach to a thread,
/// for the Chat's tool picker. Built-in toolsets first, in a fixed order,
/// then the registered servers and service agents by label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatToolList {
    pub sources: Vec<ChatToolSource>,
}

/// One toolset a thread may attach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatToolSource {
    /// What a thread's `mcp_tools` entry names: a toolset's own label, a
    /// server's tool prefix, else its name.
    pub server_label: String,
    /// `builtin` (one of lmgw's own toolsets), `server` (a registered MCP
    /// server) or `agent` (a service agent's tools).
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["builtin", "server", "agent"]))
    )]
    pub kind: String,
    /// A server's configured name; a toolset's label.
    pub name: String,
    /// A toolset's purpose in one line; empty for a server.
    pub description: String,
    /// A server's connection now: `ready`, `connecting`, `error` or
    /// `stopped`. A built-in toolset is `ready` when it offers tools and
    /// `unavailable` when it offers none ([`Self::reason`] says why).
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["ready", "connecting", "error", "stopped", "unavailable"]))
    )]
    pub status: String,
    /// The tools of it the caller may tick now: a built-in toolset's
    /// [`Self::tools`]; a server's tools as last listed, the owner's
    /// switched-off ones and those outside the caller's tool scope left out
    /// (`0` before it was first listed).
    pub tool_count: u32,
    /// Why it offers nothing now, when it does not.
    pub reason: Option<String>,
    /// A built-in toolset's tools. Empty for a server:
    /// `GET /chat/api/tools/{label}` lists those, connecting it.
    pub tools: Vec<ChatTool>,
}

/// One tool a thread may tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatTool {
    /// The exposed name (`docs__query`): what a thread's `allowed_tools`
    /// and `require_approval` name.
    pub name: String,
    pub description: Option<String>,
}

/// `GET /chat/api/tools/{label}`: one toolset's tools the caller may tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatToolDetail {
    pub server_label: String,
    pub tools: Vec<ChatTool>,
}

/// `GET /chat/api/knowledge/bases`: the knowledge bases a thread may
/// search, for the Chat's Knowledge section, its `#` picker and its chips.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatKnowledgeBases {
    pub bases: Vec<ChatKnowledgeBase>,
}

/// One knowledge base, as the Chat shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatKnowledgeBase {
    pub id: i64,
    /// Unique across the bases.
    pub name: String,
    /// Its files, in any state.
    pub files: i64,
    /// The files ingested and searchable.
    pub ready: i64,
    /// Whether a model on the gateway still resolves to the base's pinned
    /// embedding model; without one a search is by keyword only.
    pub embed_resolvable: bool,
}

/// `GET /chat/api/knowledge/files/{id}/text`: a cited file's extracted text,
/// as the Chat's source viewer shows it. The Knowledge page's own answer
/// ([`KnowledgeSource`]) also carries the file's ingest record — its error,
/// its notes, its size and digest — which can name the models and upstreams
/// that ingested it; the Chat's carries what the viewer shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatKnowledgeSource {
    pub file: ChatKnowledgeFile,
    /// The base the file is in.
    pub kb_name: String,
    /// The extracted text; `null` until the file has been ingested once.
    pub text: Option<String>,
    /// Every chunk's span, in file order.
    pub chunks: Vec<ChunkSpan>,
    /// The cited chunk (or the one found by the citation's position).
    pub highlight: Option<ChunkSpan>,
    /// Said when the highlight is not the cited chunk itself: it was found by
    /// the citation's stored position because the chunk id no longer exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub notice: Option<String>,
    /// Whether this caller may download the file's original
    /// (`GET /chat/api/knowledge/files/{id}/original`): the owner may, a
    /// device key not yet.
    pub original: bool,
}

/// The file a [`ChatKnowledgeSource`] shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatKnowledgeFile {
    pub id: i64,
    /// The name it was uploaded under (a label, never a path).
    pub name: String,
    /// What the bytes were sniffed as.
    pub kind: String,
    /// `pending`, `ingesting`, `ready` or `failed`.
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["pending", "ingesting", "ready", "failed"]))
    )]
    pub status: String,
    /// Pages, slides or sheets, when the file has them.
    pub pages: Option<i64>,
}

impl ChatKnowledgeSource {
    /// The Knowledge page's answer `s` as the Chat shows it; `original`
    /// is [`Self::original`].
    pub fn of(s: KnowledgeSource, original: bool) -> Self {
        let KnowledgeSource {
            file,
            kb_name,
            text,
            chunks,
            highlight,
            notice,
        } = s;
        Self {
            file: ChatKnowledgeFile {
                id: file.id,
                name: file.name,
                kind: file.kind,
                status: file.status,
                pages: file.pages,
            },
            kb_name,
            text,
            chunks,
            highlight,
            notice,
            original,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::KnowledgeFile;

    #[test]
    fn the_chat_s_source_drops_the_ingest_record() {
        let s = KnowledgeSource {
            file: KnowledgeFile {
                id: 3,
                kb_id: 1,
                name: "Steuer.pdf".into(),
                kind: "pdf".into(),
                status: "failed".into(),
                error: Some("upstream 'cloud' refused model 'vision'".into()),
                notes: vec!["page 2 read by 'vision'".into()],
                pages: Some(4),
                sha256: "f00".into(),
                ..Default::default()
            },
            kb_name: "Taxes".into(),
            text: Some("text".into()),
            ..Default::default()
        };
        let c = ChatKnowledgeSource::of(s, false);
        assert_eq!(
            c.file,
            ChatKnowledgeFile {
                id: 3,
                name: "Steuer.pdf".into(),
                kind: "pdf".into(),
                status: "failed".into(),
                pages: Some(4),
            }
        );
        assert_eq!((c.kb_name.as_str(), c.original), ("Taxes", false));
        let wire = serde_json::to_string(&c).unwrap();
        for leak in ["cloud", "vision", "f00", "error", "notes"] {
            assert!(!wire.contains(leak), "{leak} in {wire}");
        }
    }
}
