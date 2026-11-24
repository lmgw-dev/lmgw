//! The poppler and tesseract runs on a slot the caller holds, each under its
//! tool's confinement rules where they work here.

use std::path::Path;

use bytes::Bytes;

use super::{run_held, Bounds, Input, RunError, Tool};
use crate::extract::convert::pool;

/// A poppler run of `bin args…` with `input` on stdin on a slot the caller
/// already holds (a file whose runs and whose work on their output share
/// one slot), in `dir` when given: under poppler's rules where the
/// confinement works (`confine::poppler`, which lets it write `dir`).
pub async fn poppler_held(
    slot: &pool::Slot,
    dir: Option<&Path>,
    bin: &Path,
    args: &[&str],
    input: Input,
    bounds: Bounds,
) -> Result<Vec<u8>, RunError> {
    let rules = crate::extract::convert::confine::poppler(bin, dir);
    let tool = Tool {
        dir,
        rules: rules.as_ref(),
        ..Tool::new(bin, args)
    };
    let bounds = crate::extract::pdf::reading(bounds, &input);
    run_held(slot, tool, input, bounds).await
}

/// A tesseract run on a slot the caller holds, `input` on stdin, with
/// `env` set (its `OMP_THREAD_LIMIT`), under `rules` where the confinement
/// works (`confine::tesseract`).
pub async fn tesseract_held(
    slot: &pool::Slot,
    bin: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    rules: Option<&lmgw_doc_reader::sandbox::Confine>,
    input: Bytes,
    bounds: Bounds,
) -> Result<Vec<u8>, RunError> {
    let tool = Tool {
        env,
        rules,
        ..Tool::new(bin, args)
    };
    run_held(slot, tool, Input::Bytes(input), bounds).await
}
