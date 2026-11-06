//! `lmgw-machine`'s integration tests, one binary with one module per file
//! (machine-check design §6: the spec, system CDI and driver groups, and the
//! fact readers WP1 adds).
//!
//! Every test states its machine in a `FakeFacts`; none reads the computer it
//! runs on. New test files go in as a new `mod <name>;` line below
//! (alphabetical), with the file at `tests/it/<name>.rs`.

mod cdi_own;
mod cdi_spec;
mod cdi_system;
mod driver;
mod fake;
mod helper;
mod os;
mod pci;
mod plan;
mod repos;
mod rpm;
mod support;
