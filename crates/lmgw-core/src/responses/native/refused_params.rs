//! A sampling parameter the model refuses, on the native passthrough
//! (api-compat design §9, the chat sends' mechanic in
//! `proxy/reasoning_fit/params.rs` on the forwarded body): current models
//! refuse what older ones took, OpenAI's "Unsupported parameter:
//! 'temperature' is not supported with this model." among them.
//!
//! A `400` (or `422`) whose words name exactly one modelled parameter the
//! body carries as a top-level key is sent once more without exactly that
//! key, the others as they were. The refused attempt is a request row of its
//! own whose note says which parameter was left out and why. When a send
//! answers, the parameter is remembered for the route (per upstream, base
//! URL and model, the one memory the chat sends share) and taken out of the
//! body of its later requests before their first send, each saying so in an
//! INFO line and in its answer's `x-lmgw-params-refused`. A stream's refusal
//! is an HTTP status, so it arrives before any byte is relayed: its client
//! sees one stream and the header.
//!
//! Anything else stands as the provider said it: wording the matcher does
//! not know, a refusal of a parameter the body did not carry, of two at
//! once, of `stop`, or of a client's own passthrough key. The retry comes
//! before the hop's hand-on decisions (`hand_on.rs`): a refused parameter is
//! no exhausted quota, and a retry that meets one is handed on like any
//! other send. A fallback that answers then names nothing this hop left out.
//! A refusal the matcher does not take goes on to the reasoning off's retry
//! (`reasoning_off.rs`), or stands.

use std::time::Instant;

use axum::http::StatusCode;

use super::Forward;
use crate::error::GatewayError;
use crate::proxy::reasoning_fit::params;

impl Forward {
    /// Take what the route's model refused before out of the body, before
    /// its first send, and say so.
    pub(super) fn leave_out_on_record(&mut self) {
        let on_record = params::on_record(&self.state, &self.route);
        let left_out = params::carried_in_body(&on_record, &self.body);
        if left_out.is_empty() {
            return;
        }
        for s in &left_out {
            s.remove_from(&mut self.body);
        }
        params::say_left_out(&self.route, &left_out);
        self.left_out = left_out;
    }

    /// The upstream refused a send made at `attempt` with `status`, read as
    /// `e`: when that refuses one modelled parameter the body carries, the
    /// attempt's own row is written, the parameter taken out, and `true`
    /// asks for one more send.
    pub(super) async fn retry_without_refused(
        &mut self,
        status: StatusCode,
        e: &GatewayError,
        attempt: Instant,
    ) -> bool {
        if !matches!(status.as_u16(), 400 | 422) || !params::any_in_body(&self.body) {
            return false;
        }
        let Some(gone) = params::refused_in_body(e, &self.body) else {
            return false;
        };
        let message = e.to_string();
        params::say_retrying(&self.route, gone, &message);
        let note = params::refused_note(&message, gone);
        self.record_refused(status, e, attempt, note).await;
        gone.remove_from(&mut self.body);
        self.left_out.insert(gone);
        self.params_retried = true;
        true
    }

    /// A send answered: what this request's retries left out is remembered
    /// for the route.
    pub(super) fn learned(&self) {
        if self.params_retried {
            params::remember(&self.state, &self.route, &self.left_out);
        }
    }
}
