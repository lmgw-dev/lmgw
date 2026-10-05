//! The sentence every run-args override field carries (aux, audio, image;
//! the chat editor says the same in its own field).
//!
//! An override replaces the class's run args whole, and an empty one is no
//! override — the gateway stores it as "inherit" (migration 0055). So the
//! one way to run a model without some of the class's flags is an override
//! that lists the flags it does want.

/// Appended to the "blank inherits" hint under the override field.
pub(super) const OVERRIDE_REPLACES: &str = "An override replaces the class args whole: to run \
     without some of them, list the ones to keep (an empty override inherits).";
