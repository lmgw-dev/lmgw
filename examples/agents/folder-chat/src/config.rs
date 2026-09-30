//! What the container is told, read from where lmgw puts it (docs/agents.md
//! §5): the gateway's address in `LMGW_API_BASE` (or `LMGW_BASE_URL`), the
//! agent token in the JSON file named by `LMGW_SECRETS`, and the owner's config
//! in the JSON file named by `LMGW_INPUT`.
//!
//! The environment carries addressing only — `podman inspect` prints it — so
//! the token is never read from an environment variable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The hidden directory inside the owner's folder that holds the index. It is
/// the **only** path this agent writes; its leading `.` is also what makes the
/// scan skip it (every hidden entry is skipped).
pub const INDEX_DIR_NAME: &str = ".lmgw-folder-chat";

/// The index file inside [`INDEX_DIR_NAME`]. SQLite's WAL sidecars
/// (`index.sqlite-wal`, `index.sqlite-shm`) live beside it, in the same
/// directory, which is why the index is a directory and not a single file.
pub const INDEX_FILE_NAME: &str = "index.sqlite";

/// `chunk_tokens` when the owner sets nothing. Mirrors the manifest's
/// `default` (a test pins the two together). Roughly two paragraphs of prose:
/// small enough that one excerpt answers one thing, large enough that a
/// heading's section usually stays whole.
pub const DEFAULT_CHUNK_TOKENS: usize = 400;

/// The smallest `chunk_tokens` the manifest accepts (its `minimum`, pinned by
/// the same test). Below it a chunk is a sentence fragment and retrieval
/// returns noise.
pub const MIN_CHUNK_TOKENS: usize = 50;

/// Where lmgw mounts the `folder` field inside the container. Informational:
/// the value actually used is the one `input.json` carries in `config.folder`,
/// which lmgw has already substituted with this path.
pub const CONTAINER_FOLDER: &str = "/lmgw/mounts/folder";

/// Where `input.json` is when `LMGW_INPUT` is unset (lmgw always sets it; the
/// default only matters for a hand-started container).
pub const DEFAULT_INPUT_PATH: &str = "/lmgw/input.json";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config field '{field}': {message}")]
    Field {
        field: &'static str,
        message: String,
    },
    #[error("cannot read {path}: {message}")]
    Read { path: String, message: String },
    #[error("environment: {0}")]
    Env(String),
}

fn field(field: &'static str, message: impl Into<String>) -> ConfigError {
    ConfigError::Field {
        field,
        message: message.into(),
    }
}

/// The owner's configuration, as the Run tab's form sets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// The folder to index — inside the container, `/lmgw/mounts/folder`.
    pub folder: PathBuf,
    /// The embedding alias. Changing it re-embeds the whole folder.
    pub embed_model: String,
    /// The chat alias that answers.
    pub chat_model: String,
    /// The rerank alias; `None` (the field left empty) skips reranking.
    pub rerank_model: Option<String>,
    /// The vision alias that reads PDF pages from their images
    /// ([`crate::vision`]); `None` (the field left empty) reads no page. It
    /// sees page images, and a table page's text, so for a private folder it
    /// should be a local alias.
    #[serde(default)]
    pub vision_model: Option<String>,
    /// Read every PDF page with the vision model, not only the pages the rule
    /// picks (`crate::vision::select`). Off by default: a reading takes
    /// seconds per page.
    #[serde(default)]
    pub vision_every_page: bool,
    /// Target chunk size, in estimated tokens (see [`crate::chunk::estimator`]).
    pub chunk_tokens: usize,
    /// Serve the app face to clients other than this machine's loopback
    /// (`X-Forwarded-For`, see [`crate::server`]). Off by default: there is no
    /// login, so a client that can reach the gateway port and send this
    /// agent's host name could read and chat with the whole folder.
    #[serde(default)]
    pub allow_remote: bool,
}

impl AgentConfig {
    /// Read `config` out of an `input.json` document.
    pub fn from_input(input: &Value) -> Result<Self, ConfigError> {
        let config = input
            .get("config")
            .ok_or_else(|| ConfigError::Env("input.json has no 'config' object".into()))?;
        Self::from_config(config)
    }

    /// Read the effective config object itself (stored values over schema
    /// defaults, as lmgw writes it). lmgw validated it against the manifest
    /// already; this re-checks what the code relies on, because a hand-written
    /// `input.json` in development gets no such check.
    pub fn from_config(config: &Value) -> Result<Self, ConfigError> {
        let text = |name: &'static str| -> Option<String> {
            config
                .get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let folder = text("folder").ok_or_else(|| {
            field(
                "folder",
                "no folder is bound — choose one on the agent's Run tab",
            )
        })?;
        let embed_model =
            text("embed_model").ok_or_else(|| field("embed_model", "no embedding model chosen"))?;
        let chat_model =
            text("chat_model").ok_or_else(|| field("chat_model", "no chat model chosen"))?;
        let rerank_model = text("rerank_model");
        let vision_model = text("vision_model");
        let chunk_tokens = match config.get("chunk_tokens") {
            None | Some(Value::Null) => DEFAULT_CHUNK_TOKENS,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| field("chunk_tokens", format!("{v} is not a whole number")))?;
                usize::try_from(n)
                    .map_err(|_| field("chunk_tokens", format!("{n} is too large")))?
            }
        };
        let flag = |name: &'static str| match config.get(name) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(v) => Err(field(name, format!("{v} is not true or false"))),
        };
        let allow_remote = flag("allow_remote")?;
        let vision_every_page = flag("vision_every_page")?;
        if chunk_tokens < MIN_CHUNK_TOKENS {
            return Err(field(
                "chunk_tokens",
                format!("{chunk_tokens} is below the minimum of {MIN_CHUNK_TOKENS}"),
            ));
        }
        Ok(Self {
            folder: PathBuf::from(folder),
            embed_model,
            chat_model,
            rerank_model,
            vision_model,
            vision_every_page,
            chunk_tokens,
            allow_remote,
        })
    }

    /// Read `input.json` from `path`.
    pub fn from_input_file(path: &Path) -> Result<Self, ConfigError> {
        Self::from_input(&read_json(path)?)
    }

    /// `LMGW_INPUT`, or [`DEFAULT_INPUT_PATH`].
    pub fn from_env() -> Result<Self, ConfigError> {
        let path = std::env::var("LMGW_INPUT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_INPUT_PATH.to_string());
        Self::from_input_file(Path::new(&path))
    }
}

/// How to reach lmgw.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayEnv {
    /// Base URL **including** `/v1`.
    pub api_base: String,
    /// The agent token, used as the bearer. `None` only outside a container
    /// (`LMGW_SECRETS` unset), against a gateway that does not require a key.
    pub token: Option<String>,
}

// Hand-written so a `{:?}` in a log line can never print the token.
impl std::fmt::Debug for GatewayEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayEnv")
            .field("api_base", &self.api_base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl GatewayEnv {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into().trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.is_empty()),
        }
    }

    /// `LMGW_API_BASE`, else `LMGW_BASE_URL` + `/v1`; the token from the
    /// `token` key of the JSON file at `LMGW_SECRETS`.
    ///
    /// A set `LMGW_SECRETS` whose file cannot be read or has no `token` is an
    /// error, not a silent anonymous client: inside a container every call
    /// would then fail with a 401 that says nothing about why.
    pub fn from_env() -> Result<Self, ConfigError> {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let api_base = match (var("LMGW_API_BASE"), var("LMGW_BASE_URL")) {
            (Some(api), _) => api,
            (None, Some(base)) => format!("{}/v1", base.trim_end_matches('/')),
            (None, None) => {
                return Err(ConfigError::Env(
                    "neither LMGW_API_BASE nor LMGW_BASE_URL is set; lmgw sets both when it \
                     starts the container"
                        .into(),
                ))
            }
        };
        let token = match var("LMGW_SECRETS") {
            None => None,
            Some(path) => Some(read_token(Path::new(&path))?),
        };
        Ok(Self::new(api_base, token))
    }
}

/// The `token` of a `secrets.json`.
pub fn read_token(path: &Path) -> Result<String, ConfigError> {
    read_json(path)?
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or_else(|| ConfigError::Env(format!("{} has no 'token'", path.display())))
}

fn read_json(path: &Path) -> Result<Value, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    serde_json::from_str(&text).map_err(|e| ConfigError::Read {
        path: path.display().to_string(),
        message: format!("not JSON: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_service_input_shape() {
        let input = json!({
            "phase": "service",
            "agent": { "id": "folder-chat", "name": "Folder chat" },
            "config": {
                "folder": "/lmgw/mounts/folder",
                "embed_model": "embed/bge-m3",
                "chat_model": "qwen3.8",
                "rerank_model": "",
                "chunk_tokens": 300
            },
            "mounts": [{ "field": "folder", "path": "/lmgw/mounts/folder",
                         "kind": "directory", "access": "rw" }],
            "service": { "port": 8080, "origin": "http://folder-chat.localhost:8001",
                         "health_path": "/healthz" }
        });
        let c = AgentConfig::from_input(&input).unwrap();
        assert_eq!(c.folder, PathBuf::from(CONTAINER_FOLDER));
        assert_eq!(c.embed_model, "embed/bge-m3");
        assert_eq!(c.rerank_model, None, "an empty rerank field means off");
        assert_eq!(c.vision_model, None, "unset means no page is read");
        assert!(!c.vision_every_page);
        assert_eq!(c.chunk_tokens, 300);
        assert!(!c.allow_remote, "unset means this machine only");
    }

    #[test]
    fn defaults_and_refusals_name_the_field() {
        let base = json!({ "folder": "/f", "embed_model": "e", "chat_model": "c" });
        assert_eq!(
            AgentConfig::from_config(&base).unwrap().chunk_tokens,
            DEFAULT_CHUNK_TOKENS
        );
        let mut small = base.clone();
        small["chunk_tokens"] = json!(10);
        let e = AgentConfig::from_config(&small).unwrap_err().to_string();
        assert!(e.contains("chunk_tokens") && e.contains("minimum"), "{e}");
        let e = AgentConfig::from_config(&json!({ "embed_model": "e", "chat_model": "c" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("'folder'") && e.contains("Run tab"), "{e}");

        let mut remote = base.clone();
        remote["allow_remote"] = json!(true);
        assert!(AgentConfig::from_config(&remote).unwrap().allow_remote);
        remote["allow_remote"] = json!("yes");
        let e = AgentConfig::from_config(&remote).unwrap_err().to_string();
        assert!(e.contains("allow_remote"), "{e}");

        let mut vision = base.clone();
        vision["vision_model"] = json!(" gemma4-12b ");
        vision["vision_every_page"] = json!(true);
        let c = AgentConfig::from_config(&vision).unwrap();
        assert_eq!(c.vision_model.as_deref(), Some("gemma4-12b"));
        assert!(c.vision_every_page);
        vision["vision_model"] = json!("");
        assert_eq!(
            AgentConfig::from_config(&vision).unwrap().vision_model,
            None
        );
        vision["vision_every_page"] = json!(1);
        let e = AgentConfig::from_config(&vision).unwrap_err().to_string();
        assert!(e.contains("vision_every_page"), "{e}");
    }

    #[test]
    fn the_debug_form_never_prints_the_token() {
        let env = GatewayEnv::new("http://127.0.0.1:8001/v1/", Some("lmgw-agent-abc".into()));
        assert_eq!(env.api_base, "http://127.0.0.1:8001/v1");
        assert!(!format!("{env:?}").contains("lmgw-agent-abc"));
    }
}
