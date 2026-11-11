//! lmgw-private's one integration-test binary: the Windows checks and creates
//! against the real file system (crate doc). On Unix the crate is empty and
//! so is this binary.

#[cfg(windows)]
mod windows;
