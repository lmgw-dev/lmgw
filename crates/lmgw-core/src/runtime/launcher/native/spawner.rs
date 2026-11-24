//! The one thread every native child is spawned from.
//!
//! Linux delivers `PR_SET_PDEATHSIG` when the *thread* that forked a child
//! ends, not when its process does: a child spawned on a tokio worker that
//! later retires (a `block_in_place`, a blocking-pool thread past its idle
//! time) would be killed with it, long before lmgw exits. So the native
//! launcher forks only here, on a thread that lives exactly as long as the
//! launcher: when the launcher is dropped, the thread's queue closes, the
//! thread ends, and the kernel kills what it started. On Windows the same
//! thread is used for the same order of events; the job object is what ties
//! the children's lifetime there.

use std::sync::mpsc;

use tokio::sync::oneshot;

type Job = Box<dyn FnOnce() + Send>;

pub(super) struct Spawner {
    queue: mpsc::Sender<Job>,
}

impl Spawner {
    pub(super) fn new() -> std::io::Result<Self> {
        let (queue, jobs) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("lmgw-engine-spawner".into())
            .spawn(move || {
                for job in jobs {
                    job();
                }
            })?;
        Ok(Self { queue })
    }

    /// `cmd.spawn()` on the spawner thread, inside the caller's tokio
    /// runtime (whose process driver then reaps the child).
    /// `spawn`, handing the command back with the answer so a refused spawn
    /// can be tried again.
    pub(super) async fn spawn_keeping(
        &self,
        mut cmd: tokio::process::Command,
    ) -> (
        tokio::process::Command,
        std::io::Result<tokio::process::Child>,
    ) {
        let runtime = tokio::runtime::Handle::current();
        let (reply, answer) = oneshot::channel();
        let job: Job = Box::new(move || {
            let _entered = runtime.enter();
            let spawned = cmd.spawn();
            let _ = reply.send((cmd, spawned));
        });
        let gone = || std::io::Error::other("the native launcher's spawner thread has ended");
        if self.queue.send(job).is_err() {
            return (tokio::process::Command::new(""), Err(gone()));
        }
        match answer.await {
            Ok(back) => back,
            Err(_) => (tokio::process::Command::new(""), Err(gone())),
        }
    }
}
