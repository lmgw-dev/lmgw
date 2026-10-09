//! `lmgw-update`'s integration tests, one binary with one module per file.
//!
//! New test files go in as a new `mod <name>;` line below (alphabetical), with
//! the file at `tests/it/<name>.rs`.

mod download;
mod feed_auth;
mod manifest;
mod version;
