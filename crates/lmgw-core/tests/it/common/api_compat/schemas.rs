//! The pinned schemas (`tests/fixtures/api_schemas/`, written by
//! `scripts/api-schemas-update.sh`) as compiled validators, one per entry
//! point, each compiled once per process on first use.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// One provider's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Openai,
    Anthropic,
    Gemini,
}

impl Provider {
    fn source(self) -> &'static str {
        match self {
            Self::Openai => include_str!("../../../fixtures/api_schemas/openai.json"),
            Self::Anthropic => include_str!("../../../fixtures/api_schemas/anthropic.json"),
            Self::Gemini => include_str!("../../../fixtures/api_schemas/gemini.json"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Openai => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
        }
    }
}

/// Which half of a file: the strict request side or the answer side
/// (`scripts/api_schemas_extract.py`'s module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Request,
    Answer,
}

impl Side {
    fn key(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Answer => "answer",
        }
    }
}

/// One entry point: what a body is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entry {
    pub provider: Provider,
    pub side: Side,
    pub name: &'static str,
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.provider.name(), self.name)
    }
}

fn file(provider: Provider) -> &'static Value {
    static FILES: OnceLock<Mutex<HashMap<Provider, &'static Value>>> = OnceLock::new();
    let mut files = FILES.get_or_init(Default::default).lock().unwrap();
    files.entry(provider).or_insert_with(|| {
        Box::leak(Box::new(
            serde_json::from_str(provider.source()).expect("a pinned schema file is JSON"),
        ))
    })
}

/// `entry`'s schema with its side's `$defs`, as one 2020-12 document: the
/// root a validator is compiled from, and what [`super::diagnose`] resolves
/// a branch's `$ref` in.
pub fn root(entry: Entry) -> Value {
    let side = &file(entry.provider)[entry.side.key()];
    let schema = side["entries"]
        .get(entry.name)
        .unwrap_or_else(|| panic!("{entry}: no such entry point in the pinned schemas"));
    let mut root = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": side["$defs"],
    });
    for (k, v) in schema
        .as_object()
        .expect("an entry point is a schema object")
    {
        root[k] = v.clone();
    }
    root
}

/// The compiled validator of `entry`.
pub fn validator(entry: Entry) -> &'static jsonschema::Validator {
    static COMPILED: OnceLock<Mutex<HashMap<Entry, &'static jsonschema::Validator>>> =
        OnceLock::new();
    let mut compiled = COMPILED.get_or_init(Default::default).lock().unwrap();
    compiled.entry(entry).or_insert_with(|| {
        let v = jsonschema::draft202012::new(&root(entry))
            .unwrap_or_else(|e| panic!("{entry}: the pinned schema does not compile: {e}"));
        Box::leak(Box::new(v))
    })
}

/// The `$defs` of `entry`'s side, for resolving a branch's `$ref`.
pub fn defs(entry: Entry) -> &'static Value {
    &file(entry.provider)[entry.side.key()]["$defs"]
}
