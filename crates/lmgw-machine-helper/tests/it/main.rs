//! `lmgw-machine-helper`'s integration tests, one binary with one module per
//! file (machine-check design §6, "The helper").
//!
//! Every machine is a `FakeFacts` and every effect goes through a fake host
//! ([`support::FakeHost`]): no test runs dnf, rpmkeys, systemctl,
//! systemd-run or pkexec, or writes outside a temporary directory. Only
//! `real` starts real programs, harmless ones (`env`, `sh`, `sleep`), to show
//! what the real host hands them; `stdout` writes into pipes of its own. New test files go in as a new `mod <name>;` line
//! below (alphabetical), with the file at `tests/it/<name>.rs`.

#![cfg(target_os = "linux")]

mod apply;
mod grammar;
mod policy;
mod real;
mod run;
mod stdout;
mod support;
