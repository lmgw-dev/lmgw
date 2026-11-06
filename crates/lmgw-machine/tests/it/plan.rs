//! The plan (machine-check design §6, "The toolkit by owner", "The plan",
//! "The hash", "The embedded key"): every machine here is stated in a
//! `FakeFacts` ([`machine`]); no test reads or runs anything of the computer
//! it runs on.

mod hash;
mod how;
mod key;
/// Also the helper's wire tests' machine (`super::helper`).
pub(crate) mod machine;
mod owners;
mod scripts;
mod sections;
mod shape;
