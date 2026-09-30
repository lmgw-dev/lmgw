//! Errors

use crate::runtime::Class;

fn excerpt(logs: &[String]) -> String {
    if logs.is_empty() {
        String::new()
    } else {
        format!("\n{}", logs.join("\n"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The container never came up. Carries the `podman logs` tail because
    /// the podman-level message ("exit 1") almost never says why — the reason
    /// is in the container's own output (§3.6).
    #[error("starting {class} model '{model_id}' failed: {message}{}", excerpt(.logs))]
    Start {
        class: Class,
        model_id: String,
        container_name: String,
        message: String,
        logs: Vec<String>,
    },
    /// A stop was refused because the model is serving requests (§3.6). The
    /// caller either waits or passes `force`; nothing kills work silently.
    #[error(
        "{class} model '{model_id}' has {in_flight} request(s) in flight — not stopping it \
         (force to override)"
    )]
    Busy {
        class: Class,
        model_id: String,
        in_flight: u32,
    },
    /// A `stop` landed on the model while it was starting. The start is not
    /// retried: somebody asked for this container to be down, and quietly
    /// bringing it back up is the opposite of what they asked for.
    #[error("starting {class} model '{model_id}' was superseded by a stop")]
    Aborted { class: Class, model_id: String },
    /// `podman stop`/`wait` itself failed. The entry is dropped regardless —
    /// once a stop has failed, lmgw's belief about that container is no
    /// longer worth anything, and the next start replaces it (`--replace`).
    #[error("stopping {class} model '{model_id}' failed: {message}")]
    Stop {
        class: Class,
        model_id: String,
        message: String,
    },
    /// A stop judged on one container found it replaced, or being replaced
    /// by a climb ([`Registry::stop_generation`](crate::runtime::registry::Registry::stop_generation), ladder design §12 entry 10).
    /// Nothing was stopped: the container there now is not the one the
    /// caller judged idle, or dead.
    #[error("{class} model '{model_id}' {why} — not stopping it")]
    Moved {
        class: Class,
        model_id: String,
        why: &'static str,
    },
    /// The climb this acquire waited on could not start its rung, and the
    /// entry is gone ([`Phase::ClimbFailed`](crate::runtime::registry::Phase::ClimbFailed)). The request path goes back to
    /// admission and starts the base; only a caller that acquires outside it
    /// (an operator start) sees this.
    #[error("{class} model '{model_id}': {message}")]
    ClimbFailed {
        class: Class,
        model_id: String,
        message: String,
    },
    /// A benchmark run holds the GPU (benchmark design §3.2), so no container
    /// is started ([`Registry::set_gpu_lease`](crate::runtime::registry::Registry::set_gpu_lease)).
    /// Admission maps it to `gpu_benchmark`.
    #[error(
        "{class} model '{model_id}' is not started: benchmark run {run_id} ('{holder}') has the \
         GPU to itself — wait for the run to finish, or cancel it (Benchmarks page, or \
         lmgw__bench_cancel)"
    )]
    GpuBenchmark {
        class: Class,
        model_id: String,
        run_id: i64,
        /// The model the run measures.
        holder: String,
    },
}

impl RuntimeError {
    /// The container log excerpt a failed start collected, if any. Exposed
    /// separately from [`Display`](std::fmt::Display) so the ops/status
    /// surfaces (§8) can render it as its own block instead of parsing it
    /// back out of a message.
    pub fn logs(&self) -> &[String] {
        match self {
            Self::Start { logs, .. } => logs,
            _ => &[],
        }
    }
}
