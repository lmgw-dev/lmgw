//! `lmgw-core`'s integration tests, folded into one binary.
//!
//! Cargo used to build each `tests/<name>.rs` file as its own binary, and
//! every one of them links the whole of `lmgw-core` (~450 MB in debug) — so a
//! one-line change to core relinked all 75 of them. Cargo auto-discovers
//! `tests/it/main.rs` as a single test target named `it`, so a core change
//! now links once instead of 75 times.
//!
//! Run one former file's tests with its module path, e.g.:
//!
//! ```sh
//! cargo test -p lmgw-core --test it vram_admission::
//! ```
//!
//! New test files go in as a new `mod <name>;` line below (alphabetical), with
//! the file at `tests/it/<name>.rs`.
//!
//! The one exception is `tests/trace_span.rs`, which installs a process-global
//! `tracing` subscriber and so needs a process of its own; see its header.

mod common;
mod support;

mod agents_catalog;
mod agents_container;
mod agents_ledger;
mod agents_package;
mod agents_runs;
mod agents_script;
mod agents_self_view;
mod agents_service;
mod anthropic_beta;
mod audio_backend;
mod audio_catalog;
mod audio_catalog_live;
mod aux_models;
mod aux_router;
mod backends;
mod backends_forge;
mod backends_git;
mod backends_ops;
mod backends_registry_live;
mod backends_run;
mod backends_updates;
mod background_admission;
mod bench;
mod bench_engine;
mod bench_lease;
mod body_limit;
mod candidate_alias_deferrals;
mod candidate_aliases;
mod candidate_routing;
mod capabilities_cloud;
mod capabilities_local;
mod capabilities_notes;
mod capabilities_override;
mod catalog_fields;
mod chat_actions;
mod chat_archive;
mod chat_attach_hardening;
mod chat_attach_kinds;
mod chat_attachments;
mod chat_export;
mod chat_folders;
mod chat_knowledge;
mod chat_live;
mod chat_sampling;
mod chat_search;
mod chat_stt_settings;
mod chat_tab;
mod chat_temporary;
mod chat_thread_defaults;
mod config_hoisting;
mod count_compat;
mod e2e_proxy;
mod egress_adapters;
mod embeddings_params;
mod gpu_contention;
mod hf_download;
mod image_backend;
mod image_lab;
mod image_live;
mod image_recipes;
mod ingress_roundtrip;
mod jobs;
mod key_policy;
mod key_set;
mod knowledge;
mod knowledge_hardening;
mod knowledge_limits;
mod knowledge_r3;
mod kv_unified_disable;
mod ladder_models;
mod mcp_admin_plane;
mod mcp_ingress;
mod mcp_live;
mod mcp_selfadmin;
mod migrations;
mod model_runtime_overrides;
mod modelinfo_real_files;
mod models_endpoint;
mod openapi_coverage;
mod openapi_dashboard;
mod openapi_headers;
mod openapi_live;
mod openapi_ops;
mod openapi_structure;
mod openapi_v1;
mod ops_container;
mod owner_keys;
mod principal_gate;
mod quickdoc_docs_plane;
mod quickdoc_ingest;
mod reasoning_live;
mod responses_api;
mod route_walk;
mod router;
mod runtime_argv;
mod runtime_descriptor;
mod runtime_lifecycle;
mod runtime_ownership;
mod runtime_registry;
mod session_login;
mod tool_inventory;
mod ui_cache;
mod vram_admission;
mod web_pages;
