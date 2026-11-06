//! A request's own claim, which the admission of that request's own
//! fallback may count as freeable (openai egress design §14.3).
//!
//! A request that leaves model A for its fallback B — a climb that cannot
//! happen, a candidate picked again — still holds its claim on A: a loop's
//! next turn finds A there if A is still loaded, and the claim keeps A from
//! the reaper meanwhile. When B fits only once A is gone, B's admission must
//! not queue behind that same request's claim. So the admission runs with
//! the claim marked as the request's own ([`crate::vram::leaving`]), and
//! eviction may take A for it when that claim is A's **only** user
//! ([`Freeable::frees`]): one claim in flight (this one), no ladder send
//! counted on the container, and no turn on the hold that carries the
//! claim: the leaving send's own turn ends as it leaves, so a turn still
//! counted is another model call sharing the claim. A claim shared that way,
//! or a second request on A, keeps A, as every claim in flight does.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::runtime::Class;

/// One request's claim on container `generation` of a model (module doc).
#[derive(Debug, Clone)]
pub struct Freeable {
    class: Class,
    model_id: String,
    generation: u64,
    /// The turns in progress on the hold that carries the claim
    /// ([`crate::vram::HoldTurn`]); the leaving send's own has ended.
    turns: Arc<AtomicUsize>,
}

impl Freeable {
    pub fn new(class: Class, model_id: String, generation: u64, turns: Arc<AtomicUsize>) -> Self {
        Self {
            class,
            model_id,
            generation,
            turns,
        }
    }

    /// Whether this claim is the only user of container `generation` of
    /// `class`/`model_id`, which counts `in_flight` claims and `sends` ladder
    /// sends now (module doc).
    pub fn frees(
        &self,
        class: Class,
        model_id: &str,
        generation: u64,
        in_flight: u32,
        sends: u32,
    ) -> bool {
        self.class == class
            && self.model_id == model_id
            && self.generation == generation
            && in_flight == 1
            && sends == 0
            && self.turns.load(Ordering::Acquire) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_sole_user_of_that_container_frees_it() {
        let turns = Arc::new(AtomicUsize::new(0));
        let f = Freeable::new(Class::Chat, "a".into(), 7, Arc::clone(&turns));
        assert!(f.frees(Class::Chat, "a", 7, 1, 0));
        // Another container of the model, another model, another class.
        assert!(!f.frees(Class::Chat, "a", 8, 1, 0));
        assert!(!f.frees(Class::Chat, "b", 7, 1, 0));
        assert!(!f.frees(Class::Aux, "a", 7, 1, 0));
        // A second claim, a ladder send in flight.
        assert!(!f.frees(Class::Chat, "a", 7, 2, 0));
        assert!(!f.frees(Class::Chat, "a", 7, 1, 1));
        // Another model call's turn on the same hold.
        turns.store(1, Ordering::Release);
        assert!(!f.frees(Class::Chat, "a", 7, 1, 0));
    }
}
