//! Where this lmgw runs, and what its start was told (headless server design
//! §1, §2, D2, D20): the deployment, the settings its container sets, and
//! the stop budget its unit gives it. Read on `GET /api/settings-full`.

use serde::{Deserialize, Serialize};

/// The command that restarts the server's service, which a pinned row, its
/// refusal and a setting that applies at the next start name (headless
/// server design §2.2, §6.2): the service the server docs install is the
/// user unit `lmgw`.
pub const RESTART_COMMAND: &str = "systemctl --user restart lmgw";

/// Where this lmgw runs (headless server design D2): the desktop (the tray
/// app, a dev instance) or the server image's container, which says so with
/// `LMGW_DEPLOYMENT=container`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Deployment {
    #[default]
    Desktop,
    Container,
}

impl Deployment {
    /// The server image's container.
    pub fn is_container(self) -> bool {
        self == Self::Container
    }
}

/// Where a pinned setting's value came from on this start (headless server
/// design §2.2): its variable, or — the variable unset — the value an earlier
/// start wrote, or the image's default for a data dir no container start has
/// seeded yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PinnedSource {
    /// The variable is set on this start, and its value is the stored one.
    Variable,
    /// The variable is not set on this start: the stored value stays, as an
    /// earlier start wrote it (or the default, or what a data dir brought
    /// along).
    #[default]
    Kept,
    /// The variable is not set, and this start wrote the image's default
    /// into a data dir no container start had seeded yet.
    ImageDefault,
}

/// A setting this server's container sets from its environment on every
/// start (headless server design §2.2): the dashboard shows it read-only, and
/// every settings save refuses a change of it with [`Self::why`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PinnedSetting {
    /// The setting's dotted save path (`bind_addr`, `realtime.webrtc_port`).
    pub setting: String,
    /// The environment variable that sets it (`LMGW_BIND_ADDR`).
    pub variable: String,
    /// Whether the variable was set on this start, and where the value came
    /// from when it was not.
    pub source: PinnedSource,
    /// What the row says, and what a save of it is refused with.
    pub why: String,
}

/// How long a quit can take against what the server's unit gives it
/// (headless server design D20, §3.3): `STOP_WITHIN` for the server's own
/// streams, a running benchmark's abort (the registry's stop grace plus
/// `vram.unload_timeout_seconds`), and the model containers' stop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StopBudget {
    /// `LMGW_STOP_TIMEOUT`: the seconds the unit gives lmgw to stop (its
    /// `StopTimeout`); `None` when the start was not told, and then nothing
    /// is compared.
    pub unit_seconds: Option<u64>,
    /// The seconds a quit can take with the stored unload timeout.
    pub needed_seconds: u64,
    /// The stored `vram.unload_timeout_seconds` the sum was made with.
    pub unload_timeout_seconds: u64,
    /// The quit can take longer than the unit gives it.
    pub over: bool,
    /// The comparison in words, as the start's log line says it.
    pub sentence: String,
}

/// A newer lmgw than the one running, as the server image's update notice
/// says it (headless server design §6.3, ruling 13): from the feed the
/// desktop's updater polls, with no download; Settings → About and the log
/// carry [`Self::sentence`]. Only in container mode: from the background
/// check while `update_check_enabled` is on, or from an explicit
/// `update_check`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdateNotice {
    /// The feed's version.
    pub latest: String,
    /// This server's version.
    pub running: String,
    /// When the feed said so: UTC, `YYYY-MM-DD HH:MM:SS`.
    pub checked_at: String,
    /// The line: what is out, what runs, and how an update comes.
    pub sentence: String,
}

/// A copy of a database a container start made before it migrated it
/// (headless server design ruling 14, §6.4): `VACUUM INTO` the data dir's
/// `backups/<stem>-<from>-<to>.<ext>` (`lmgw-0101-0102.sqlite`,
/// `knowledge-0002-0003.db`, `quickdoc-0002-0003.db`), ahead of the repairs
/// and the migrations. A manual restore source, never a rollback: restoring
/// it on the build whose newest migration of that database is `from` loses
/// everything written since it was made. lmgw never deletes or restores
/// one. Settings → About lists them, in container mode only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MigrationCopy {
    /// The file's name in the backups dir: `lmgw-0101-0102.sqlite`, with
    /// `-2`, `-3` … before the extension when an earlier start's copy held
    /// the name.
    pub file: String,
    /// Its path.
    pub path: String,
    /// The database it copies, by its file in the data dir: `lmgw.sqlite`,
    /// `knowledge.db` or `quickdoc.db`.
    pub database: String,
    /// The newest migration the database had recorded when it was copied
    /// (below the first one the start applied):
    /// the build whose newest migration this is opens the copy as it is.
    pub from: i64,
    /// The newest migration of the build that copied it and then migrated.
    pub to: i64,
    /// Its size on disk.
    pub bytes: u64,
    /// When it was made (the file's modification time): UTC,
    /// `YYYY-MM-DD HH:MM:SS`; empty when it cannot be read.
    pub made_at: String,
}
