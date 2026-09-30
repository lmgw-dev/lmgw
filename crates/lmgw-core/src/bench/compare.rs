//! Runs side by side (benchmark design §6): comparability, the previous
//! comparable run, the headline numbers and the regression rule.
//!
//! The rule lives in [`lmgw_api_types::bench_compare`], so the Benchmarks
//! page's Compare view judges any two runs with the same code the ops judge
//! a run against its previous comparable one; this module is its name on the
//! engine's side.

pub use lmgw_api_types::bench_compare::*;
