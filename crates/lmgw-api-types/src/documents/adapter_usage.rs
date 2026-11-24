//! "Who calls the docling route" (document-conversion design §7.8, D17):
//! `POST /api/op/doc_adapter_usage` and `lmgw__doc_adapter_usage`, summed per
//! key from the `convert_calls` rows beside the request log. What a request
//! sent is kept as routes and option names, with values only for the options
//! whose values are a small fixed set (`to_formats=md`, `ocr_engine=easyocr`,
//! `pipeline=vlm`); prompts, addresses, headers and document content never.
//! The rows follow the request log's retention.

use serde::{Deserialize, Serialize};

/// Every key that called a docling-serve route in the kept log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AdapterUsage {
    /// Most recent caller first.
    pub keys: Vec<AdapterKeyRow>,
}

/// One caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AdapterKeyRow {
    /// `api_keys.id`; `None` for requests without a key (auth off).
    pub key_id: Option<i64>,
    /// The key's name as the request log has it; empty without a key.
    pub key: String,
    /// The latest request, UTC, as the request log writes it
    /// (`YYYY-MM-DD HH:MM:SS`).
    pub last_seen: String,
    pub requests: u64,
    /// Files sent, over all its requests.
    pub files: u64,
    /// The routes, most used first.
    pub routes: Vec<RouteSeen>,
    /// Every option it set with its value and outcome, the outcomes other
    /// than `honoured` first.
    pub options: Vec<OptionSeen>,
    /// The hosts of the `http` sources it sent, refused or fetched.
    pub hosts: Vec<HostSeen>,
}

/// A route and how often it was called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RouteSeen {
    /// `/v1/convert/file`, …
    pub route: String,
    pub count: u64,
}

/// An option a caller set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OptionSeen {
    /// docling's option name (`ocr_engine`), or `target` and `callbacks`.
    pub name: String,
    /// Its value for an option whose values are a small fixed set
    /// (`easyocr`, `md,text`); `set` for free text and addresses.
    pub value: String,
    /// `honoured`, `mapped`, `not_yet` or `refused`.
    pub outcome: OptionOutcome,
    /// What served it or why, as `x-lmgw-option-notes` says it
    /// (`easyocr->lmgw-ocr`, `model_not_chosen`).
    pub detail: Option<String>,
    pub count: u64,
}

/// What lmgw did with an option (§7.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum OptionOutcome {
    /// lmgw's equivalent served it (the note says what).
    Mapped,
    /// Left out for now; the output says so.
    NotYet,
    /// The request was refused over it (a named 400).
    Refused,
    /// Done as docling does it.
    Honoured,
}

impl OptionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Honoured => "honoured",
            Self::Mapped => "mapped",
            Self::NotYet => "not_yet",
            Self::Refused => "refused",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "honoured" => Self::Honoured,
            "not_yet" => Self::NotYet,
            "refused" => Self::Refused,
            _ => Self::Mapped,
        }
    }
}

/// The host of an `http` source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostSeen {
    pub host: String,
    /// `refused` (fetching web addresses is off) or `fetched`.
    pub outcome: String,
    pub count: u64,
}
