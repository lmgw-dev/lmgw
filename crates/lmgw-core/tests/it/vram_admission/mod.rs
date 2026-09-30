//! VRAM admission control (per-model-containers design §4, quickdoc §9b): the
//! ledger, the eviction policy, the visible queue — and the handoff to the
//! runtime registry that actually starts the containers.
//!
//! Nothing here touches a GPU or podman. The driver is a [`FakeGpu`] whose free
//! memory is derived from the same [`World`] the fake `podman` mutates, so
//! "stop the chat model's container" really does make room in the numbers the
//! scheduler reads. Each model's container is a wiremock server: it answers
//! `/health` (readiness), `/slots` (the busy probe eviction runs against a
//! victim's own port) and the inference routes a request is finally forwarded
//! to.
//!
//! **That last part is the point of the `/v1` endpoint swap (§5), and it is
//! asserted structurally**: the class listen ports in these fixtures are dead
//! by construction, so a request that gets a 200 can only have reached the
//! container `acquire` started. A forward that still used the route's
//! configured base URL would fail with a transport error, not pass quietly.
//!
//! The containers are interchangeable on purpose — both answer everything, and
//! which model lands on which is decided by start order — so no test depends on
//! the port allocator handing out a particular port to a particular model.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::config::{AuxKind, HoldFallbackMode, Settings};
use lmgw_core::quickdoc::InProcessEmbedder;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry, HOST_PID_FORMAT};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAuxModel, NewLocalModel};
use lmgw_core::vram::{GpuMemory, GpuProbe, ProcessMemory};
use quickdoc_core::embed::{EmbedIdentity, Embedder};
use quickdoc_core::store::{self as qstore, NewCorpus};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

mod containers;
mod fixture;
mod podman;
mod world;

mod admission;
mod background_starts;
mod chat_tool_reroute;
mod dead_container;
mod degradation;
mod gate_follows_running_container;
mod gpu_hold;
mod image_pipeline_peak;
mod in_flight_claim;
mod ladder_admission;
mod ladder_http;
mod ladder_local_model_test;
mod ladder_review_fixes;
mod ladder_wp4;
mod live_frame;
mod outside_vram_fallback;
mod pool_mock;
mod release_after_llama_server;
mod request_gate;

use containers::*;
use fixture::*;
use podman::*;
use world::*;

use gpu_hold::{cloud_upstream, engage_hold, fallback_reason};
use image_pipeline_peak::add_image_model;
use ladder_admission::{
    climb_to_top, ladder_fixture, ladder_hold, ladder_runs, ladder_view, LADDER,
};
use ladder_http::{header, ladder_chat, served_by};
use ladder_review_fixes::{edit_ladder, spawn_climb, until_ladder};
use outside_vram_fallback::{
    cloud_chat, log_count, newest_log, set_global_fallback, until_vram, Dialect,
};
use request_gate::{
    chat_body, chat_port, edit_chat_model, guard_chat_model, pools, queued, set_queue_timeout,
    set_slot_busy, sized_chat, stream_and_hang_up, until_pool, until_pools_empty, warm_pool, words,
};
