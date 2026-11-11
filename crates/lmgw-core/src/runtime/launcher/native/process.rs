//! One native child: what the launcher keeps of it, and the task that owns
//! its process (the supervisor), which is the only place it is signalled,
//! killed and reaped.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::ring::Ring;

/// How long the exit waits for the stdout and stderr readers to reach their
/// end after the process has been reaped, so a check right after the exit
/// sees the last error lines. A reader still blocked after that (a
/// grandchild holding the pipe open) does not hold the exit up.
pub(super) const READER_DRAIN: Duration = Duration::from_secs(2);

/// What a stop asks of the supervisor.
#[derive(Debug, Clone, Copy)]
pub(super) enum Signal {
    /// Stop now if the platform allows a graceful stop, and kill after the
    /// grace if it is still running.
    Term,
    /// Kill now.
    Kill,
}

/// One child the launcher started, running or ended.
pub(super) struct Proc {
    pub(super) pid: u32,
    /// The port it was told to listen on.
    pub(super) port: u16,
    /// Its executable's file name, for sentences.
    pub(super) exe: String,
    pub(super) ring: Arc<Ring>,
    /// `Some(how it ended)` once it has exited.
    ended: watch::Receiver<Option<String>>,
    control: mpsc::UnboundedSender<Signal>,
}

impl Proc {
    /// Take ownership of `child` (started as `exe`, told to listen on
    /// `port`): its output into `ring`, its process into a supervisor task.
    /// `grace` is how long a [`Signal::Term`] waits before it kills.
    pub(super) fn supervise(
        name: &str,
        exe: String,
        port: u16,
        mut child: tokio::process::Child,
        ring: Arc<Ring>,
        grace: Duration,
    ) -> Option<Arc<Self>> {
        let pid = child.id()?;
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(tokio::spawn(ring.clone().pump(out)));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(ring.clone().pump(err)));
        }
        let (ended_tx, ended) = watch::channel(None);
        let (control, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(
            name.to_string(),
            pid,
            child,
            rx,
            ended_tx,
            grace,
            readers,
        ));
        Some(Arc::new(Self {
            pid,
            port,
            exe,
            ring,
            ended,
            control,
        }))
    }

    pub(super) fn running(&self) -> bool {
        self.ended.borrow().is_none()
    }

    /// How it ended, once it has.
    pub(super) fn ended(&self) -> Option<String> {
        self.ended.borrow().clone()
    }

    pub(super) fn ask_to_stop(&self) {
        let _ = self.control.send(Signal::Term);
    }

    pub(super) fn kill(&self) {
        let _ = self.control.send(Signal::Kill);
    }

    /// Wait until it has exited, for at most `bound`; true when it has.
    pub(super) async fn wait_ended(&self, bound: Duration) -> bool {
        let mut ended = self.ended.clone();
        tokio::time::timeout(bound, ended.wait_for(Option::is_some))
            .await
            .is_ok_and(|r| r.is_ok())
    }
}

/// The supervisor: owns `child` until it has exited, acts on the stops sent
/// to it, and kills it when everything that could send one is gone (its
/// entry was dropped with the launcher).
async fn run(
    name: String,
    pid: u32,
    mut child: tokio::process::Child,
    mut rx: mpsc::UnboundedReceiver<Signal>,
    ended: watch::Sender<Option<String>>,
    grace: Duration,
    readers: Vec<tokio::task::JoinHandle<()>>,
) {
    let mut kill_at: Option<tokio::time::Instant> = None;
    let mut stopping = false;
    let mut orphaned = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            sig = rx.recv(), if !orphaned => match sig {
                Some(Signal::Term) => {
                    stopping = true;
                    if ask_to_stop(&mut child, pid) && kill_at.is_none() {
                        kill_at = Some(tokio::time::Instant::now() + grace);
                    }
                }
                Some(Signal::Kill) => {
                    stopping = true;
                    kill(&mut child, pid);
                }
                None => {
                    orphaned = true;
                    stopping = true;
                    kill(&mut child, pid);
                }
            },
            _ = sleep_until(kill_at), if kill_at.is_some() => {
                kill_at = None;
                tracing::warn!(
                    container = %name,
                    "the engine process {name} (pid {pid}) did not exit within the stop grace \
                     ({grace:?}): killed"
                );
                kill(&mut child, pid);
            }
        }
    };
    // Anything the engine started in its group goes with it.
    #[cfg(unix)]
    super::unix::kill(pid);
    let how = match status {
        Ok(status) => describe(status),
        Err(e) => format!("could not be waited for ({e})"),
    };
    if stopping {
        tracing::info!(container = %name, "the engine process {name} (pid {pid}) {how}");
    } else {
        tracing::warn!(container = %name, "the engine process {name} (pid {pid}) {how} on its own");
    }
    // The last lines the engine wrote belong to the log before anyone is
    // told it ended (READER_DRAIN).
    let drained = tokio::time::timeout(READER_DRAIN, async {
        for r in readers {
            let _ = r.await;
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            container = %name,
            "the output of the engine process {name} (pid {pid}) was still open {READER_DRAIN:?}              after it exited (a process it started holds the pipe): its last lines may be missing"
        );
    }
    ended.send_replace(Some(how));
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The platform's graceful stop; true when it is graceful and so needs a
/// kill after the grace.
fn ask_to_stop(child: &mut tokio::process::Child, pid: u32) -> bool {
    #[cfg(unix)]
    {
        let _ = child;
        super::unix::ask_to_stop(pid)
    }
    // No graceful stop for a console child of a GUI process (module doc of
    // `native`): TerminateProcess at once.
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = child.start_kill();
        false
    }
}

fn kill(child: &mut tokio::process::Child, pid: u32) {
    #[cfg(unix)]
    {
        let _ = child;
        super::unix::kill(pid);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = child.start_kill();
    }
}

/// "exited with code 1", "was killed by signal 9".
fn describe(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exited with code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("was killed by signal {sig}");
        }
    }
    format!("ended ({status})")
}
