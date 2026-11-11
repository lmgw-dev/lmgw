//! A native child's exit, as the readiness poll is told it — whether it
//! ended on a taken port, and whether its build has no kernel for the card.
//!
//! A container's port is bound by Podman at `podman run`, which fails at
//! once when it is taken. A native engine binds it itself and, when it is
//! taken, exits. So an exit says "port in use" when the engine's log says it
//! could not bind, or when the port is held by something else right after
//! the exit — the engine's own listener is gone with it, so a bind that
//! fails then is somebody else's.
//!
//! **The Vulkan fallback** (Windows design §4.2). A pinned CUDA build's
//! process that ended on its own (lmgw did not ask it to stop: not the idle
//! reaper, not a user's stop, not a replacing start), not with code 0, with
//! ggml's CUDA error line "no kernel image is available" anywhere in its
//! log ring ([`fallback::no_kernel_image_line`]) marks that build
//! ([`fallback::mark`], with the exit and the line). While loading, the
//! exit asks the registry for the engine's next build
//! ([`Exited::fallback`]): the registry prepares the start again — the
//! choice passes over the marked release's CUDA builds, so the Vulkan build
//! is installed there, outside the load timeout — and launches it with a
//! fresh budget. A CUDA build that ended that way after it was ready (a
//! first compute at request time) is marked at the next start of its
//! engine ([`Native::mark_failed_cuda_builds`]). Each child is judged once
//! and its verdict kept with it ([`Run::verdict`]): a mark the owner
//! cleared is not written again from the same log, and when another
//! start's `prepare` judges a loading child first, that child's own
//! readiness poll still reads the verdict and falls back.

use std::sync::Arc;

use super::process::Proc;
use super::{Native, Resolved};
use crate::runtime::engine_assets::fallback;
use crate::runtime::launcher::types::Exited;

/// What a name's last child runs, for the fallback's marks.
pub(super) struct Run {
    resolved: Arc<Resolved>,
    /// The child's process id: a record belongs to that one child.
    pid: u32,
    /// lmgw asked it to stop (or killed it) while it ran: its exit is
    /// lmgw's, never a reason to mark its build.
    stop_asked: bool,
    /// What its exit was judged to say, once it has been
    /// ([`Native::judge`]): `Some(None)` no mark, `Some(Some(Ok(sentence)))`
    /// its build is marked, `Some(Some(Err(e)))` the mark failed. Whoever
    /// judges first, every later look reads the same verdict.
    verdict: Option<Verdict>,
}

/// One child's verdict ([`Run::verdict`]).
type Verdict = Option<Result<String, String>>;

/// What an engine prints when its listen address is taken, lower-cased:
/// llama-server's own sentence (`couldn't bind HTTP server socket`, b12236),
/// the POSIX error text, and Windows' (`WSAEADDRINUSE`, "error 10048"). The
/// bare number is no pattern: a log also holds sizes (`model buffer size =
/// 10048.55 MiB`) and process ids.
const BIND_FAILURES: &[&str] = &[
    "couldn't bind",
    "address already in use",
    "address in use",
    "only one usage of each socket address",
    "wsaeaddrinuse",
    "error 10048",
];

/// How the supervisor says a process ended with code 0 (`process.rs`'s
/// `describe`).
const CLEAN_EXIT: &str = "exited with code 0";

impl Native {
    pub(super) fn remember_launch(&self, name: &str, pid: u32, resolved: Arc<Resolved>) {
        let mut launched = self.launched.lock().unwrap_or_else(|e| e.into_inner());
        launched.insert(
            name.to_string(),
            Run {
                resolved,
                pid,
                stop_asked: false,
                verdict: None,
            },
        );
    }

    /// lmgw is about to stop or kill `p` (`name`'s child): its exit is not
    /// the engine's own. A child that has ended already is left as it is.
    pub(super) fn note_stop(&self, name: &str, p: &Proc) {
        if !p.running() {
            return;
        }
        let mut launched = self.launched.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = launched.get_mut(name).filter(|r| r.pid == p.pid) {
            r.stop_asked = true;
        }
    }

    /// Mark every pinned CUDA build whose child under any name ended on its
    /// own with ggml's "no kernel image is available" in its log (module
    /// doc): the after-ready path, at the next start.
    pub(super) fn mark_failed_cuda_builds(&self) {
        let names: Vec<String> = self
            .launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for name in names {
            if let Some(p) = self.child(&name) {
                let _ = self.judge(&name, &p);
            }
        }
    }

    /// `p`'s exit's verdict (module doc): `Some(Ok(sentence))` when its
    /// build is marked (now, or already) and the engine's next build should
    /// run, `Some(Err(e))` when the mark could not be written. Judged once,
    /// under the launch table's lock, so two starts that look at one child
    /// at the same time get the one verdict; every later look reads it.
    fn judge(&self, name: &str, p: &Proc) -> Verdict {
        let how = p.ended()?;
        let mut launched = self.launched.lock().unwrap_or_else(|e| e.into_inner());
        let r = launched.get_mut(name).filter(|r| r.pid == p.pid)?;
        if let Some(v) = &r.verdict {
            return v.clone();
        }
        let v = if r.stop_asked {
            None
        } else {
            Self::verdict_of(name, p, &how, &r.resolved)
        };
        r.verdict = Some(v.clone());
        v
    }

    /// What `p`'s exit (`how`) says about the build it ran ([`Self::judge`]),
    /// the mark written when it says "no kernel image".
    fn verdict_of(name: &str, p: &Proc, how: &str, resolved: &Resolved) -> Verdict {
        let install = &resolved.install;
        if !install.variant.is_some_and(|v| v.is_cuda()) || how == CLEAN_EXIT {
            return None;
        }
        let log = p.ring.tail(super::LOG_RING_LINES);
        let line = fallback::no_kernel_image_line(&log)?;
        let sentence = format!(
            "{} build {} has no kernel image for this machine's card ({line}; the process {how}), \
             so it is marked and its engine runs its next build (the Vulkan one) from now on",
            super::engines::executable_stem(resolved.class),
            install.build
        );
        if fallback::marked(&install.dir).is_some() {
            return Some(Ok(sentence));
        }
        Some(
            match fallback::mark(&install.dir, &fallback::mark_now(how, line)) {
                Ok(()) => {
                    tracing::warn!(container = %name, "{sentence}");
                    Ok(sentence)
                }
                Err(e) => {
                    tracing::warn!(container = %name, "{e}");
                    Err(e)
                }
            },
        )
    }

    /// The readiness poll's exit check (module doc): `None` while the
    /// engine is still loading.
    pub(super) async fn exit_or_fall_back(&self, name: &str) -> Option<Exited> {
        let mut exit = self.exit_of(name)?;
        // Removed while loading: nothing is left to judge.
        let Some(p) = self.child(name) else {
            return Some(exit);
        };
        match self.judge(name, &p) {
            Some(Ok(sentence)) => exit.fallback = Some(sentence),
            Some(Err(e)) => {
                exit.message = format!(
                    "{}; its build has no kernel image for this card, and marking it failed ({e}), \
                     so the next start would choose it again",
                    exit.message
                )
            }
            None => {}
        }
        Some(exit)
    }

    pub(super) fn exit_of(&self, name: &str) -> Option<Exited> {
        let Some(p) = self.child(name) else {
            return Some(Exited::plain(
                "the engine process was removed while the model was loading",
            ));
        };
        let how = p.ended()?;
        let log = p
            .ring
            .tail(crate::runtime::launcher::LOG_TAIL)
            .to_ascii_lowercase();
        let port_in_use = bind_failed(&log) || held_elsewhere(p.port);
        Some(Exited {
            message: format!(
                "the engine process {} (pid {}) {how} while the model was loading{}",
                p.exe,
                p.pid,
                if port_in_use {
                    format!(" (port {} is taken)", p.port)
                } else {
                    String::new()
                }
            ),
            port_in_use,
            fallback: None,
        })
    }
}

/// `127.0.0.1:port` cannot be bound because something holds it.
fn held_elsewhere(port: u16) -> bool {
    matches!(
        std::net::TcpListener::bind(("127.0.0.1", port)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
    )
}

/// The lower-cased log says the engine could not bind its address.
fn bind_failed(log: &str) -> bool {
    BIND_FAILURES.iter().any(|n| log.contains(n))
}

#[cfg(test)]
mod tests {
    use super::bind_failed;

    #[test]
    fn bind_texts_are_found_and_a_size_of_10048_is_not_one() {
        assert!(bind_failed("couldn't bind http server socket"));
        assert!(bind_failed("bind failed: error 10048 (wsaeaddrinuse)"));
        assert!(!bind_failed(
            "load_tensors:        cuda0 model buffer size = 10048.55 mib"
        ));
        assert!(!bind_failed("cudaMalloc failed: out of memory (pid 10048)"));
    }
}
