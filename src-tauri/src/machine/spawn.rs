//! Starting the helper (machine-check design §3.5): `/usr/bin/pkexec`
//! by its absolute path, never through `PATH`, running
//! `/usr/libexec/lmgw/lmgw-machine-helper apply <hash>`.
//!
//! - **pkexec's own agent is off** (`--disable-internal-agent`, D120):
//!   without a password window on the desktop pkexec would otherwise ask on
//!   lmgw's controlling terminal, where nobody is looking (lmgw started from
//!   a terminal), or fail with "Error creating textual authentication
//!   agent" (lmgw started from the app menu), never with the "No
//!   authentication agent found" the outcome reads. pkexec's options come
//!   before the program, so the helper's own arguments, and with them the
//!   polkit action's `exec.argv1 = apply` match (D89), are unchanged.
//! - **Nothing of lmgw's process goes with it:** stdin is null, the
//!   environment empty (pkexec gives the helper its own, and the helper
//!   ignores that too), the working directory `/`, and no descriptor
//!   beyond stdin, stdout and stderr ([`lmgw_core::child::system_tool`]).
//! - **It is not killed when the command goes away.** pkexec execs the
//!   helper, which then runs as root; lmgw could not signal it, and the
//!   steps run in a system task of their own anyway (D22).
//!
//! The helper's stdout is held by the helper alone (pkexec execs it, and it
//! gives the system task and every step pipes of their own), so it ends
//! when the helper does.

use std::process::Stdio;

use lmgw_machine::plan::embedded::{HELPER, PKEXEC};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::Erased;

/// pkexec's option that keeps its textual agent off (D120).
pub(crate) const NO_INTERNAL_AGENT: &str = "--disable-internal-agent";

/// pkexec's arguments: its option, then the helper and its own two.
pub(crate) fn pkexec_args(hash: &str) -> Vec<String> {
    [NO_INTERNAL_AGENT, HELPER, "apply", hash]
        .map(String::from)
        .to_vec()
}

/// How the started program ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ended {
    /// Its exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// What it wrote on stderr (pkexec's own words; the helper's usage).
    pub stderr: String,
    /// Reading its output or waiting for it failed.
    pub error: Option<String>,
}

/// The started helper, as the command reads it.
pub(crate) trait Helper: Send {
    /// Its next stdout line, without the newline; `None` once stdout ended.
    fn line(&mut self) -> Erased<'_, Option<String>>;
    /// Wait for its end, once stdout ended.
    fn ended(self: Box<Self>) -> Erased<'static, Ended>;
}

/// The real one: pkexec, then the helper it execs.
struct Pkexec {
    child: tokio::process::Child,
    stdout: BufReader<tokio::process::ChildStdout>,
    stderr: tokio::task::JoinHandle<String>,
    read_error: Option<String>,
}

/// Start `/usr/bin/pkexec … apply <hash>`. Only the real world calls it,
/// and a debug build refuses here too, below the gates (D4).
pub(crate) fn pkexec(hash: &str) -> Result<Box<dyn Helper>, String> {
    if cfg!(debug_assertions) {
        return Err("a debug build never starts the root helper".into());
    }
    let mut cmd = tokio::process::Command::new(PKEXEC);
    cmd.args(pkexec_args(hash))
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lmgw_core::child::system_tool_tokio(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{PKEXEC} could not be started: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "pkexec's stdout was not piped".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "pkexec's stderr was not piped".to_string())?;
    // Read beside stdout, so a full stderr pipe never stalls the helper.
    let stderr = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });
    Ok(Box::new(Pkexec {
        child,
        stdout: BufReader::new(stdout),
        stderr,
        read_error: None,
    }))
}

impl Helper for Pkexec {
    fn line(&mut self) -> Erased<'_, Option<String>> {
        Box::pin(async move {
            if self.read_error.is_some() {
                return None;
            }
            let mut buf = Vec::new();
            match self.stdout.read_until(b'\n', &mut buf).await {
                Ok(0) => None,
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                    }
                    Some(String::from_utf8_lossy(&buf).into_owned())
                }
                Err(e) => {
                    self.read_error = Some(format!("reading the helper's output: {e}"));
                    None
                }
            }
        })
    }

    fn ended(self: Box<Self>) -> Erased<'static, Ended> {
        Box::pin(async move {
            let Pkexec {
                mut child,
                stderr,
                read_error,
                ..
            } = *self;
            let status = child.wait().await;
            let stderr = stderr.await.unwrap_or_default();
            match status {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    Ended {
                        code: status.code(),
                        signal: status.signal(),
                        stderr,
                        error: read_error,
                    }
                }
                Err(e) => Ended {
                    code: None,
                    signal: None,
                    stderr,
                    error: Some(format!("waiting for pkexec: {e}")),
                },
            }
        })
    }
}
