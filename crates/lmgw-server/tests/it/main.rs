//! `lmgw-server`'s integration tests, one binary (the project's rule): one
//! module per file.
//!
//! `account` drives the account commands on a scratch data dir, offline
//! and through a test gateway, and the binary for their refusals;
//! `start_refusals` runs the built binary on a stated environment for the
//! refusals that end a start before the gateway exists; `start_serve` runs
//! the start in this process on fake host seams; `quit` drives the serving
//! loop on a test gateway; `migration_copy` drives a container start on a
//! database one migration behind and the offline refusal of a newer one;
//! `packaging` reads the server image's shipped files
//! (`packaging/server/`). None of them reaches the machine's podman or GPU.

#![cfg(target_os = "linux")]

mod account;
mod migration_copy;
mod packaging;
mod quit;
mod start_refusals;
mod start_serve;
mod support;
