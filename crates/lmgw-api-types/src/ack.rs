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

/// A write to a routing row that names the row it wrote: `{"ok": true, "id": …,
/// "message": …}`. `POST /api/op/upstream_set` and `model_set` answer it, for
/// every action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RowSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id: the new row's for `create`.
    pub id: i64,
    /// What was done, for a person to read.
    pub message: String,
}

impl RowSaved {
    /// `{"ok": true, "id": id, "message": message}`.
    pub fn new(id: i64, message: impl Into<String>) -> Self {
        Self {
            ok: true,
            id,
            message: message.into(),
        }
    }
}

/// A write that names the row it acted on and has nothing else to report:
/// `{"ok": true, "id": …}`. `POST /api/op/price_delete` answers it, with the id
/// it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdAck {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id, as the call named it.
    pub id: i64,
}

impl IdAck {
    /// `{"ok": true, "id": id}`.
    pub const fn new(id: i64) -> Self {
        Self { ok: true, id }
    }
}

/// An answer and the tool plane's hint at what to call next: the answer's own
/// keys, and `next_step` beside them. Only the `lmgw__*` tools add it; the
/// HTTP answer of the same op is the plain `T`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WithNextStep<T: Default> {
    #[serde(flatten)]
    pub answer: T,
    /// What to call next, and why.
    pub next_step: String,
}

impl<T: Default> WithNextStep<T> {
    /// `answer`, and `next_step` beside it.
    pub fn new(answer: T, next_step: impl Into<String>) -> Self {
        Self {
            answer,
            next_step: next_step.into(),
        }
    }
}
