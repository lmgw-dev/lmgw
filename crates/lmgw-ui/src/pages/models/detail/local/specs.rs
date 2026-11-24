//! Every `LlamaParams` field the local model editor edits, with its label,
//! its hint and how it reads (a whole number, a number, text, a choice), and
//! the group of the detail it sits in.

use leptos::prelude::*;
use lmgw_api_types::LlamaParams;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Int,
    Float,
    Text,
    /// (value, label) pairs; "" = unset.
    Sel(&'static [(&'static str, &'static str)]),
}

pub(super) struct Spec {
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) hint: &'static str,
    /// The llama-server flag the field sets, shown after the title; empty when it names none.
    pub(super) flag: &'static str,
    pub(super) kind: Kind,
}

impl Spec {
    /// The flag as a row's wire name: none when the field names no flag.
    pub(super) fn wire(&self) -> Option<&'static str> {
        (!self.flag.is_empty()).then_some(self.flag)
    }
}

const UNSET: (&str, &str) = ("", "Server default");

const CACHE_TYPES: &[(&str, &str)] = &[
    UNSET,
    ("f16", "f16"),
    ("bf16", "bf16"),
    ("q8_0", "q8_0"),
    ("q5_1", "q5_1"),
    ("q5_0", "q5_0"),
    ("q4_1", "q4_1"),
    ("q4_0", "q4_0"),
];

/// The fields by the cards the editor used to show them on; the detail
/// places each by its key ([`group_of`] says where).
pub(super) const SECTIONS: &[(&str, &[Spec])] = &[
    (
        "Context & batch",
        &[
            Spec {
                key: "ctx_size",
                label: "Context size",
                flag: "",
                hint: "Tokens",
                kind: Kind::Int,
            },
            Spec {
                key: "parallel",
                label: "Parallel slots",
                flag: "-np",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "kv_unified",
                label: "Unified KV",
                flag: "-kvu / -no-kvu",
                hint: "",
                kind: Kind::Sel(&[
                    ("", "Server default"),
                    ("true", "on — one shared pool"),
                    ("false", "off — split per slot"),
                ]),
            },
            Spec {
                key: "kv_unified_per_slot",
                label: "Unified per-slot cap",
                flag: "--kv-unified-per-slot",
                hint: "Tokens",
                kind: Kind::Int,
            },
            Spec {
                key: "batch_size",
                label: "Batch size",
                flag: "-b",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "ubatch_size",
                label: "Physical batch",
                flag: "-ub",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "threads",
                label: "Threads",
                flag: "-t",
                hint: "",
                kind: Kind::Int,
            },
        ],
    ),
    (
        "GPU & memory",
        &[
            Spec {
                key: "n_gpu_layers",
                label: "GPU layers",
                flag: "-ngl",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "n_cpu_moe",
                label: "Expert layers kept in system RAM",
                flag: "--n-cpu-moe",
                hint: "The first N layers' MoE experts stay in RAM, off the card",
                kind: Kind::Int,
            },
            Spec {
                key: "moe_cache_mib",
                label: "GPU cache for experts in RAM (MiB)",
                flag: "--moe-cache-mib",
                hint: "Keeps the most used of the experts in RAM on the card",
                kind: Kind::Int,
            },
            Spec {
                key: "flash_attn",
                label: "Flash attention",
                flag: "",
                hint: "",
                kind: Kind::Sel(&[UNSET, ("auto", "auto"), ("on", "on"), ("off", "off")]),
            },
            Spec {
                key: "cache_type_k",
                label: "KV cache K",
                flag: "",
                hint: "",
                kind: Kind::Sel(CACHE_TYPES),
            },
            Spec {
                key: "cache_type_v",
                label: "KV cache V",
                flag: "",
                hint: "",
                kind: Kind::Sel(CACHE_TYPES),
            },
            Spec {
                key: "cache_ram",
                label: "Cache RAM",
                flag: "",
                hint: "MiB; -1 unlimited, 0 off",
                kind: Kind::Int,
            },
            Spec {
                key: "fit",
                label: "Fit",
                flag: "--fit",
                hint: "Shrinks the arguments left unset to the device's memory",
                kind: Kind::Text,
            },
            Spec {
                key: "fit_ctx",
                label: "Fit context floor",
                flag: "--fit-ctx",
                hint: "",
                kind: Kind::Int,
            },
        ],
    ),
    (
        "Template & reasoning",
        &[
            Spec {
                key: "chat_template_file",
                label: "Template file",
                flag: "",
                hint: "Relative to models dir",
                kind: Kind::Text,
            },
            Spec {
                key: "reasoning_format",
                label: "Reasoning format",
                flag: "",
                hint: "",
                kind: Kind::Sel(&[
                    UNSET,
                    ("auto", "auto"),
                    ("none", "none"),
                    ("deepseek", "deepseek"),
                    ("deepseek-legacy", "deepseek-legacy"),
                ]),
            },
            Spec {
                key: "reasoning",
                label: "Reasoning",
                flag: "",
                hint: "",
                kind: Kind::Sel(&[UNSET, ("auto", "auto"), ("on", "on"), ("off", "off")]),
            },
            Spec {
                key: "reasoning_budget",
                label: "Thinking budget",
                flag: "",
                hint: "Tokens; -1 unrestricted, 0 off",
                kind: Kind::Int,
            },
            Spec {
                key: "n_predict",
                label: "Max output tokens",
                flag: "--n-predict",
                hint: "Tokens; -1 unbounded",
                kind: Kind::Int,
            },
            Spec {
                key: "reasoning_preserve",
                label: "Preserve reasoning",
                flag: "",
                hint: "Keep every turn's trace in history",
                kind: Kind::Sel(&[
                    ("", "Template default"),
                    ("true", "preserve all turns"),
                    ("false", "only the last turn"),
                ]),
            },
            Spec {
                key: "reasoning_effort",
                label: "Reasoning effort",
                flag: "",
                hint: "Template variable, e.g. low / medium / high",
                kind: Kind::Text,
            },
        ],
    ),
    (
        "Sampling defaults",
        &[
            Spec {
                key: "temp",
                label: "Temperature",
                flag: "",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "top_p",
                label: "Top-p",
                flag: "",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "top_k",
                label: "Top-k",
                flag: "",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "min_p",
                label: "Min-p",
                flag: "",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "repeat_penalty",
                label: "Repeat penalty",
                flag: "",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "presence_penalty",
                label: "Presence penalty",
                flag: "",
                hint: "",
                kind: Kind::Float,
            },
            Spec {
                key: "seed",
                label: "Seed",
                flag: "",
                hint: "",
                kind: Kind::Int,
            },
        ],
    ),
    (
        "Multimodal & speculative",
        &[
            Spec {
                key: "mmproj_path",
                label: "Projector (mmproj)",
                flag: "",
                hint: "Relative to models dir",
                kind: Kind::Text,
            },
            Spec {
                key: "draft_gguf_path",
                label: "Draft model",
                flag: "",
                hint: "Speculative decoding weights",
                kind: Kind::Text,
            },
            Spec {
                key: "spec_type",
                label: "Speculation type",
                flag: "",
                hint: "For example draft-mtp, draft-simple, ngram-mod",
                kind: Kind::Text,
            },
            Spec {
                key: "spec_draft_n_max",
                label: "Draft max",
                flag: "",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "spec_draft_n_min",
                label: "Draft min",
                flag: "",
                hint: "",
                kind: Kind::Int,
            },
            Spec {
                key: "spec_draft_ngl",
                label: "Draft GPU layers",
                flag: "",
                hint: "Number, auto or all",
                kind: Kind::Text,
            },
        ],
    ),
];

pub(super) fn param_string(p: &LlamaParams, key: &str) -> String {
    fn i(v: Option<i64>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    fn f(v: Option<f64>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    fn s(v: &Option<String>) -> String {
        v.clone().unwrap_or_default()
    }
    match key {
        "ctx_size" => i(p.ctx_size),
        "n_gpu_layers" => i(p.n_gpu_layers),
        "n_cpu_moe" => i(p.n_cpu_moe),
        "moe_cache_mib" => i(p.moe_cache_mib),
        "threads" => i(p.threads),
        "batch_size" => i(p.batch_size),
        "ubatch_size" => i(p.ubatch_size),
        "parallel" => i(p.parallel),
        "kv_unified" => match p.kv_unified {
            Some(true) => "true".into(),
            Some(false) => "false".into(),
            None => String::new(),
        },
        "kv_unified_per_slot" => i(p.kv_unified_per_slot),
        "flash_attn" => s(&p.flash_attn),
        "cache_type_k" => s(&p.cache_type_k),
        "cache_type_v" => s(&p.cache_type_v),
        "cache_ram" => i(p.cache_ram),
        "fit" => s(&p.fit),
        "fit_ctx" => i(p.fit_ctx),
        "chat_template_file" => s(&p.chat_template_file),
        "reasoning_format" => s(&p.reasoning_format),
        "reasoning" => s(&p.reasoning),
        "reasoning_budget" => i(p.reasoning_budget),
        "n_predict" => i(p.n_predict),
        "reasoning_preserve" => match p.reasoning_preserve {
            Some(true) => "true".into(),
            Some(false) => "false".into(),
            None => String::new(),
        },
        "reasoning_effort" => s(&p.reasoning_effort),
        "temp" => f(p.temp),
        "top_p" => f(p.top_p),
        "top_k" => i(p.top_k),
        "min_p" => f(p.min_p),
        "repeat_penalty" => f(p.repeat_penalty),
        "presence_penalty" => f(p.presence_penalty),
        "seed" => i(p.seed),
        "mmproj_path" => s(&p.mmproj_path),
        "draft_gguf_path" => s(&p.draft_gguf_path),
        "spec_type" => s(&p.spec_type),
        "spec_draft_n_max" => i(p.spec_draft_n_max),
        "spec_draft_n_min" => i(p.spec_draft_n_min),
        "spec_draft_ngl" => s(&p.spec_draft_ngl),
        _ => String::new(),
    }
}

/// The spec of `key`.
pub(super) fn spec(key: &str) -> &'static Spec {
    SECTIONS
        .iter()
        .flat_map(|(_, specs)| specs.iter())
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("no spec {key}"))
}

/// The group of the detail a field sits in: the name a dirty count and the
/// "unsaved" question say it by.
pub(super) fn group_of(key: &str) -> &'static str {
    match key {
        "model_id" | "enabled" | "public" | "warm_start" | "idle_seconds" => "General",
        "ctx_size"
        | "parallel"
        | "n_predict"
        | "kv_unified"
        | "kv_unified_per_slot"
        | "batch_size"
        | "ubatch_size" => "Context",
        "n_gpu_layers" | "n_cpu_moe" | "moe_cache_mib" | "flash_attn" | "cache_type_k"
        | "cache_type_v" | "cache_ram" | "fit" | "fit_ctx" | "threads" | "gpu_pin" => "GPU",
        "temp" | "top_p" | "top_k" | "min_p" | "repeat_penalty" | "presence_penalty" | "seed" => {
            "Sampling"
        }
        "reasoning"
        | "reasoning_format"
        | "reasoning_budget"
        | "reasoning_preserve"
        | "reasoning_effort"
        | "jinja"
        | "chat_template_kwargs" => "Reasoning",
        "spec_type" | "spec_draft_n_max" | "spec_draft_n_min" | "spec_draft_ngl" => "Speculative",
        "gguf_path" | "mmproj_path" | "no_mmproj" | "draft_gguf_path" | "chat_template_file" => {
            "Files"
        }
        "fallback" => "Fallback",
        _ => "Advanced",
    }
}

/// A choice's options as the kit's select takes them.
pub(super) fn options(kind: Kind) -> Signal<Vec<(String, String)>> {
    let opts: Vec<(String, String)> = match kind {
        Kind::Sel(o) => o
            .iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    Signal::stored(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field has its place in the detail: none falls through to
    /// Advanced by accident.
    #[test]
    fn every_param_has_a_group() {
        for (_, specs) in SECTIONS {
            for s in *specs {
                assert_ne!(group_of(s.key), "Advanced", "{}", s.key);
                assert_eq!(spec(s.key).key, s.key);
            }
        }
    }
}
