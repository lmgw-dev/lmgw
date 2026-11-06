//! The plan's hash (machine-check design §3.2, D2): SHA-256 over a dedicated
//! struct, [`HashedPlan`] — each step's kind, its exact argv and the full
//! bytes of the file it writes — in a canonical encoding. Never over the
//! API's plan DTO: its sentences change with wording, its download sizes
//! with the user's metadata cache, and neither changes what runs as root.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::steps::Step;

/// The layout of [`HashedPlan`] and of its encoding. A helper that answers
/// with another schema than the app's own was updated while the app ran
/// (§3.5: "lmgw was updated").
pub const PLAN_SCHEMA: u32 = 1;

/// What the hash covers, and what the helper prints when its plan differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedPlan {
    pub schema: u32,
    pub steps: Vec<HashedStep>,
}

/// One step as root runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedStep {
    /// [`super::steps::StepKind::as_str`].
    pub kind: String,
    /// Every argv it runs, in order.
    pub commands: Vec<Vec<String>>,
    /// The file it writes.
    pub file: Option<HashedFile>,
}

/// A file a step writes, in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedFile {
    pub path: String,
    pub mode: u32,
    /// lmgw's own file replaced, not a new one created.
    pub replace: bool,
    pub content: String,
}

impl HashedPlan {
    pub fn of(steps: &[Step]) -> Self {
        Self {
            schema: PLAN_SCHEMA,
            steps: steps
                .iter()
                .map(|s| HashedStep {
                    kind: s.kind().as_str().to_string(),
                    commands: s.commands(),
                    file: s.file().map(|w| HashedFile {
                        path: w.path.to_string(),
                        mode: w.mode,
                        replace: w.replace,
                        content: w.content.to_string(),
                    }),
                })
                .collect(),
        }
    }

    /// The canonical encoding: a fixed tag, then every field in order, each
    /// string and byte run as its length (8 bytes, big-endian) then its
    /// bytes, each number big-endian. Nothing in it depends on a serializer's
    /// choices (key order, escaping, whitespace).
    pub fn encode(&self) -> Vec<u8> {
        fn bytes(out: &mut Vec<u8>, b: &[u8]) {
            out.extend_from_slice(&(b.len() as u64).to_be_bytes());
            out.extend_from_slice(b);
        }
        fn count(out: &mut Vec<u8>, n: usize) {
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mut out = Vec::new();
        bytes(&mut out, b"lmgw-machine-plan");
        out.extend_from_slice(&self.schema.to_be_bytes());
        count(&mut out, self.steps.len());
        for step in &self.steps {
            bytes(&mut out, step.kind.as_bytes());
            count(&mut out, step.commands.len());
            for argv in &step.commands {
                count(&mut out, argv.len());
                for arg in argv {
                    bytes(&mut out, arg.as_bytes());
                }
            }
            match &step.file {
                None => out.push(0),
                Some(f) => {
                    out.push(1);
                    bytes(&mut out, f.path.as_bytes());
                    out.extend_from_slice(&f.mode.to_be_bytes());
                    out.push(u8::from(f.replace));
                    bytes(&mut out, f.content.as_bytes());
                }
            }
        }
        out
    }

    /// SHA-256 of [`Self::encode`], 64 lowercase hex digits: the one argument
    /// the helper takes besides its verb.
    pub fn sha256(&self) -> String {
        Sha256::digest(self.encode())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
