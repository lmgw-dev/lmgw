//! The benchmark engine (docs/design/2026-09-29-benchmark-design.md,
//! WP1): everything a run does *against a running llama-server*, and nothing
//! about how that server was started.
//!
//! The integration (WP2) starts the bench container, times its load, and then
//! hands the engine a [`BenchTarget`] — the server's base URL plus the row's
//! derived capabilities — together with a running [`Sampler`], the job's
//! [`crate::agent::Cancel`] and a [`BenchSink`] that persists and publishes.
//! [`run_suite`] drives the probes, prefill, decode, concurrent and mixed
//! phases (§4, §5) and fills a [`Record`]: the `results`, `probes`, `timeline`
//! and `gpu` JSON columns, in the `lmgw_api_types::bench` shapes.
//!
//! * [`corpus`] — the frozen text every prompt is sliced from (§4.1);
//! * [`points`] — the points a context and slot count give (§4.2);
//! * [`client`] and [`stream`] — llama-server's native API, streamed
//!   `/completion` with a client timestamp per token;
//! * [`facts`] — `/props` and `/slots`, both engines' shapes (§2.1);
//! * [`stats`] — median/min/max, the concurrent and mixed arithmetic;
//! * [`sampler`] — VRAM, power, temperature, clock events and energy (§4.3);
//! * [`probes`] and [`png`] — the behaviour probes' requests and verdicts (§5);
//! * [`phases`] — one module per phase;
//! * [`run`] — the order, progress, persistence hooks and cancellation.
//!
//! The integration (WP2) around it:
//!
//! * [`lease`] — the GPU lease and the one predicate every admission site
//!   asks ([`crate::config::Snapshot::gpu_block`], §3.2);
//! * [`state`] — the run in flight and the test seams;
//! * [`launcher`] — the bench container: render, `podman run`, `/health`,
//!   log tail, removal (§3.4);
//! * [`plan`] — a request resolved against the row: overrides, identity,
//!   provisional points, what a start stops, why it cannot start (§8.1);
//! * [`drain`] — emptying the card (§3.2 step 3);
//! * [`identity`] and [`compare`] — §6's identity, comparability and the
//!   regression rule;
//! * [`runner`] — the `benchmark` job: start, load, suite, end (§3.2, §3.3).

pub mod client;
pub mod compare;
pub mod corpus;
pub mod drain;
pub mod facts;
pub mod identity;
pub mod launcher;
pub mod lease;
pub mod phases;
pub mod plan;
pub mod png;
pub mod points;
pub mod probes;
pub mod run;
pub mod runner;
pub mod sampler;
pub mod state;
pub mod stats;
pub mod stream;

pub use client::{BenchError, HttpAnswer, LlamaClient};
pub use launcher::{BenchLauncher, PodmanLauncher, BENCH_LABEL};
pub use run::{run_suite, Bench, BenchSink, BenchTarget, Progress, Record, SuiteEnd};
pub use runner::{abort_for_hold, boot_sweep, BenchmarkExecutor};
pub use sampler::{Baseline, EnergyReading, EnergyWindow, Sample, Sampler};
pub use state::{BenchState, OnCard, Tuning};
