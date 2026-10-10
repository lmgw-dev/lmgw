//! The one acknowledgement shape.

use serde::{Deserialize, Serialize};

/// The answer of a write that has nothing to report but that it
/// happened: `{"ok": true}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Ack {
    /// Always `true`; a failure is an error answer instead.
    pub ok: bool,
}

impl Ack {
    /// `{"ok": true}`.
    pub const fn ok() -> Self {
        Self { ok: true }
    }
}

/// A write that reports what it did in a sentence: `{"ok": true, "message": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessageAck {
    /// Always `true`; a failure is an error answer instead.
    pub ok: bool,
    /// What was done, for a person to read.
    pub message: String,
}

impl MessageAck {
    /// `{"ok": true, "message": message}`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }
}
