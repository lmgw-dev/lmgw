//! The agent run's meter (container-runtime / agents design §4.5): a
//! [`TurnRunner`] that tallies what every turn's row was billed. A child of
//! `batch` since the chat-voice WP11 review (n4), which grew it.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::agent::{DeltaSink, TurnRunner};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Completion, Usage};

/// A [`TurnRunner`] that keeps a running total of what the model spent.
///
/// The loop reports usage on its `Ok` return only, so a run that ended in a
/// transport error would price at zero however many turns it had already paid
/// for. Tallying at the call makes the number independent of how the loop
/// ended (§4.5).
pub(super) struct MeteredRunner<'a> {
    inner: &'a dyn TurnRunner,
    usage: Mutex<Usage>,
}

impl<'a> MeteredRunner<'a> {
    pub(super) fn new(inner: &'a dyn TurnRunner) -> Self {
        Self {
            inner,
            usage: Mutex::new(Usage::default()),
        }
    }

    pub(super) fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }
}

#[async_trait]
impl TurnRunner for MeteredRunner<'_> {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        let mut billed = Billed { sink, usage: None };
        let out = self.inner.run_turn(ir, deadline, &mut billed).await;
        // What the turn's row was billed, a stopped or failed turn's too
        // (WP11 server review n4: a cancelled turn writes its row with the
        // cost so far, and the job's meter must not be lower than its
        // rows); a runner that does not say leaves its completion's usage.
        let usage = billed.usage.or_else(|| out.as_ref().ok().map(|c| c.usage));
        if let Some(u) = usage {
            self.usage.lock().unwrap().add(&u);
        }
        out
    }
}

/// The caller's sink, noting what the turn's row was billed
/// (`DeltaSink::billed`).
struct Billed<'s> {
    sink: &'s mut dyn DeltaSink,
    usage: Option<Usage>,
}

impl DeltaSink for Billed<'_> {
    fn on_delta(&mut self, d: &crate::ir::StreamDelta) {
        self.sink.on_delta(d);
    }

    fn stop(&self) -> Option<crate::proxy::StopSignal> {
        self.sink.stop()
    }

    fn flush(&mut self) {
        self.sink.flush();
    }

    fn billed(&mut self, usage: &Usage) {
        self.usage = Some(*usage);
        self.sink.billed(usage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ChatRequest {
        ChatRequest {
            model_alias: "m".into(),
            messages: Vec::new(),
            params: Default::default(),
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            passthrough: Default::default(),
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
        }
    }

    fn usage(prompt: u64, completion: u64) -> Usage {
        Usage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            ..Default::default()
        }
    }

    /// Bills its row, then fails `canceled` — a turn stopped mid-stream.
    struct Stopped;

    #[async_trait]
    impl TurnRunner for Stopped {
        async fn run_turn(
            &self,
            _ir: &ChatRequest,
            _deadline: Duration,
            sink: &mut dyn DeltaSink,
        ) -> Result<Completion, GatewayError> {
            sink.billed(&usage(40, 7));
            Err(crate::proxy::canceled("stopped by the caller mid-stream"))
        }
    }

    /// Answers without saying what its row was billed.
    struct Silent;

    #[async_trait]
    impl TurnRunner for Silent {
        async fn run_turn(
            &self,
            ir: &ChatRequest,
            _deadline: Duration,
            _sink: &mut dyn DeltaSink,
        ) -> Result<Completion, GatewayError> {
            Ok(Completion {
                content: vec![crate::ir::ContentPart::text("Hallo.")],
                reasoning: String::new(),
                finish_reason: crate::ir::FinishReason::Stop,
                usage: usage(10, 3),
                model: ir.model_alias.clone(),
                timings: None,
            })
        }
    }

    /// WP11 server review n4: a turn stopped mid-stream writes its row with
    /// the cost so far and fails `canceled`; the run's meter tallies what
    /// the row was billed, not nothing. A runner that does not say keeps
    /// its completion's usage.
    #[tokio::test]
    async fn the_meter_tallies_what_a_stopped_turn_s_row_was_billed() {
        let mut sink = |_: &crate::ir::StreamDelta| {};
        let stopped = MeteredRunner::new(&Stopped);
        let out = stopped
            .run_turn(&request(), Duration::from_secs(1), &mut sink)
            .await;
        assert!(out.is_err());
        assert_eq!(stopped.usage(), usage(40, 7));

        let silent = MeteredRunner::new(&Silent);
        silent
            .run_turn(&request(), Duration::from_secs(1), &mut sink)
            .await
            .unwrap();
        assert_eq!(silent.usage(), usage(10, 3));
    }
}
