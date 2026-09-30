//! The measured phases (benchmark design §4.3, §5), one module each, in
//! suite order after the integration's load phase. Each takes the shared
//! [`PhaseCx`] and appends its points to the run's results as it measures
//! them, so a phase that fails half-way keeps the points it finished.

mod cx;
pub use cx::*;

pub mod concurrent;
pub mod decode;
pub mod mixed;
pub mod prefill;
pub mod probes;
