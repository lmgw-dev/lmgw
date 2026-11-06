//! Entries a test puts in the registry without starting anything.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::*;
use crate::runtime::{Class, Placement};

impl Registry {
    /// Test-only: a `ready` entry for `model_id` that answers `in_flight`
    /// requests, as if its container ran, with nothing started (the
    /// machine check's settings-switch tests stop it, D136). Its generation.
    /// `pub` for the integration tests.
    #[doc(hidden)]
    pub fn ready_for_tests(&self, class: Class, model_id: &str, in_flight: u32) -> u64 {
        let now = Instant::now();
        let generation = next_generation();
        let (tx, _rx) = watch::channel(Phase::Ready);
        self.map().insert(
            (class, model_id.to_string()),
            Entry {
                generation,
                container_name: format!("machine-test-{model_id}"),
                host_port: 1,
                state: RuntimeState::Ready,
                started_at: now,
                in_flight,
                last_used: now,
                stop_timeout: Duration::from_secs(5),
                phase: Arc::new(tx),
                warnings: Vec::new(),
                capabilities: None,
                llama: None,
                gate: None,
                charge: None,
                resident_key: None,
                placement: Placement::Gpu,
                sends: watch::channel(0).0,
                climb: None,
                owner: Origin::Owner,
            },
        );
        generation
    }
}
