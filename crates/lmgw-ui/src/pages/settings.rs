//! Settings — the KDE System Settings shape (UX plan Phase 3, CFG S3–S10).
//!
//! A rail of categories with a search box over them; the pane shows one
//! category, or every field the search matches under its category. There is
//! one draft for the whole page and one Save: [`FormState`] tracks every
//! field against what the server last said, the foot counts what changed and
//! where, and Save posts a sparse patch of the changed keys only —
//! `SettingsFullPatch` and every nested patch are all-`Option`, so any mix of
//! keys is one valid patch (a refusal refuses all of it, and says so in the
//! foot until the next save).
//!
//! The page is table-driven: [`fields`] names each setting once — its patch
//! path, label, category, group and control — and the same row feeds the
//! renderer, the search, the dirty counts and the deep links
//! (`/settings/<cat>#f-<key-with-dashes>`, scrolled to and flashed). Every
//! category stays mounted and is only hidden, so a search or a category
//! switch never loses an edit.
//!
//! Controls that act the moment they are used — the GPU hold, Apply to
//! running containers, Check now, theme and scale — are never part of the
//! draft; they carry an "applies now" tag. Secrets: empty keeps what is
//! stored, the clear tick erases it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_location, use_navigate};
use lmgw_api_types::builds::MAX_BUILD_UPDATE_CHECK_HOURS;
use serde_json::{json, Map, Value};

use crate::catalog::CatalogEntry;
use crate::live::use_live;
use crate::ops_state::{class_key, use_ops};
use crate::scope::Scope;
use crate::widgets::{
    use_dirty_guard, use_slash_focus, use_toasts, Explain, Field, FormState, ImageClass,
    ImagePicker, Kind, ModelPicker, PageFrame, PageMode, SaveBar, Section, Select,
};

/// The Realtime category (realtime design §12): its rows, the controls only
/// it has, and its VRAM budget.
mod realtime;
use realtime::{BudgetPanel, SpeechStyleField, TagHintField, VoiceField, WordsField};
/// The Chat's Voice group (chat-voice design §2.1).
mod chat_profile;
mod chat_voice;

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// The categories: slug (the URL segment), rail label, one line on what is
/// in it.
const CATS: &[(&str, &str, &str)] = &[
    (
        "network",
        "Network & access",
        "where the gateway listens and who may call it",
    ),
    (
        "retention",
        "Retention",
        "how long logs, jobs, rollups and stored responses are kept",
    ),
    (
        "usage-cost",
        "Usage & cost",
        "the currency, the gateway-wide budget, the local-vs-cloud reference",
    ),
    ("gpu", "GPU", "admission control and the manual hold"),
    (
        "runtimes",
        "Runtimes",
        "the container defaults every model of a class inherits",
    ),
    (
        "backends",
        "Backends",
        "container builds: forge tokens, the builds directory, the update check",
    ),
    (
        "chat",
        "Chat",
        "the system prompt new conversations start with, how attachments are read, the knowledge budget, the voice, the change feed client apps follow",
    ),
    (
        "realtime",
        "Realtime",
        "spoken conversations on /v1/realtime: the voice cascade and its GPU memory, turns, barge-in, output",
    ),
    (
        "agents",
        "Agents & tools",
        "agent containers, the Responses tool loop, MCP sampling",
    ),
    ("docs", "Docs", "ingestion costs and the rerank model"),
    (
        "tokens",
        "Tokens & updates",
        "the credentials lmgw uses outward, and the update check",
    ),
    (
        "appearance",
        "Appearance",
        "this window only — not a gateway setting",
    ),
];
const CAT_SLUGS: &[&str] = &[
    "network",
    "retention",
    "usage-cost",
    "gpu",
    "runtimes",
    "backends",
    "chat",
    "realtime",
    "agents",
    "docs",
    "tokens",
    "appearance",
];

/// A card within a category. `fold` makes it a collapsible [`Section`]
/// (persisted as `lmgw.ui.open.<fold>`) — the runtime classes, which are long
/// and looked at one at a time.
struct Group {
    id: &'static str,
    cat: &'static str,
    title: &'static str,
    sub: &'static str,
    fold: &'static str,
}

const fn g(cat: &'static str, id: &'static str, title: &'static str) -> Group {
    Group {
        id,
        cat,
        title,
        sub: "",
        fold: "",
    }
}

const fn class_group(
    id: &'static str,
    title: &'static str,
    sub: &'static str,
    fold: &'static str,
) -> Group {
    Group {
        id,
        cat: "runtimes",
        title,
        sub,
        fold,
    }
}

const GROUPS: &[Group] = &[
    g("network", "listen", "Gateway"),
    g("network", "access", "Access"),
    g("retention", "logs", "Request log"),
    g("retention", "jobs", "Job history"),
    g("retention", "chat", "Chat threads"),
    g("retention", "rollups", "Usage rollups"),
    g("retention", "responses", "Responses storage"),
    g("usage-cost", "money", "Currency & budget"),
    g("usage-cost", "compare", "Local vs cloud"),
    g("gpu", "admission", "Admission"),
    g("gpu", "hold", "Hold"),
    g("runtimes", "containers", "Containers"),
    class_group("router", "Chat", "llama-server", "settings.runtimes.chat"),
    class_group(
        "aux_router",
        "Aux",
        "llama-server · embeddings + rerank",
        "settings.runtimes.aux",
    ),
    class_group("audio", "Audio", "audio.cpp", "settings.runtimes.audio"),
    class_group(
        "image",
        "Image",
        "stable-diffusion.cpp",
        "settings.runtimes.image",
    ),
    g("backends", "forge", "Forge tokens"),
    g("backends", "builds", "Builds"),
    g("chat", "prompt", "Default system prompt"),
    g("chat", "attachments", "Attachments"),
    g("chat", "knowledge", "Knowledge bases"),
    g("chat", "chat-voice", "Voice"),
    g("chat", "feed", "Change feed"),
    g("realtime", "rt-cascade", "Voice cascade"),
    g("realtime", "rt-budget", "GPU memory of the cascade"),
    g("realtime", "rt-prompt", "Voice instructions"),
    g("realtime", "rt-names", "Client names"),
    g("realtime", "rt-vad", "Turn detection"),
    g("realtime", "rt-smart", "Smart Turn (semantic_vad)"),
    g("realtime", "rt-barge", "Barge-in"),
    g("realtime", "rt-output", "Output"),
    g("realtime", "rt-limits", "Connection"),
    g("agents", "agent-containers", "Agent containers"),
    g("agents", "tool-loop", "Responses tool loop"),
    g("agents", "mcp", "MCP"),
    g("docs", "ingest", "Ingestion"),
    g("docs", "search", "Search"),
    g("tokens", "tokens", "Tokens"),
    g("tokens", "updates", "Updates"),
    g("appearance", "window", "This window"),
];

/// How wide a field's cell wants to be: numbers, words, paths.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
    S,
    M,
    L,
}

/// The control a setting is edited with.
#[derive(Clone, Copy)]
enum Ctl {
    Text,
    Mono,
    /// A whole number no smaller than this (`i64::MIN`: any).
    Int(i64),
    /// A number in `[min, max]` (both inclusive).
    Float(f64, f64),
    /// A whole number in `[min, max]` (both inclusive) — like [`Ctl::Int`],
    /// with an upper bound too, for the one setting
    /// (`build_update_check_hours`) the server refuses past a real maximum
    /// (`lmgw_api_types::builds::MAX_BUILD_UPDATE_CHECK_HOURS`).
    IntRange(i64, i64),
    /// Whole currency units in the box, micro-units in the patch; empty = 0.
    Money,
    Bool,
    /// One per line in the box, a list in the patch.
    Lines,
    /// Write-only: (the `SettingsFull` pointer that says one is stored, the
    /// patch key that erases it).
    Secret(&'static str, &'static str),
    Choice(&'static [(&'static str, &'static str)]),
    /// A class's container image, with the local images of its engine to
    /// pick from (container-builds §9.1).
    Image(ImageClass),
    /// The forge tokens by host (container-builds §7): write-only like a
    /// [`Ctl::Secret`], one row per host. One `Raw` draft key holding every
    /// row, since a host has dots in it and a dotted form key would nest.
    ForgeTokens,
    /// The builds directory, with the path in use now and the tmpfs warning.
    BuildsDir,
    /// A default prompt: prose in a tall box, and a Reset that puts the
    /// built-in text back — the server's `…_builtin` at this `SettingsFull`
    /// pointer.
    Prompt(&'static str),
    /// A name → name map, one `name = value` per line in the box, an object
    /// in the patch ([`realtime::map_text`]).
    Map,
    /// A list of short words as one comma-separated box that wraps and grows
    /// ([`realtime::WordsField`]); a list in the patch.
    Words,
    /// A voice of the TTS model the draft names — the first of these keys
    /// that is set — typed or picked from its list
    /// ([`realtime::VoiceField`]).
    Voice(&'static [&'static str]),
    /// The speech style, told what the drafted TTS (the first of these keys
    /// that is set) does with it ([`realtime::SpeechStyleField`]).
    SpeechStyle(&'static [&'static str]),
    /// The sound-tag hint, a checkbox with the text the prompt would get
    /// ([`realtime::TagHintField`]).
    TagHint,
    /// One of the Chat's two languages — the one the user speaks, the one
    /// replies are in — with where the saved speech model of its stage does
    /// not take it ([`chat_voice::LanguageField`]).
    VoiceLanguage,
    /// One cell of the Smart Turn table (`realtime.semantic_vad`): its
    /// column, `0..4` ([`realtime::VAD_COLS`]).
    Vad(usize),
    /// What the draft's voice cascade holds on the GPU — read, never part
    /// of the draft ([`realtime::BudgetPanel`]).
    Budget,
    /// A model alias: (the tasks it is for, why a local model is refused —
    /// `None` when one is fine, what empty means).
    Model(&'static [&'static str], Option<&'static str>, &'static str),
    // Acts at once; never in the draft.
    Hold,
    Apply(&'static str),
    CheckNow,
    Theme,
    Scale,
    /// A pointer to where the thing is really managed.
    Link(&'static str),
    /// The personality profile new Chat threads start with: one of the
    /// gateway's profiles, or none (`chat_profile`).
    Profile,
}

impl Ctl {
    /// The form's reading of it, or `None` for a control outside the draft.
    fn form_kind(self) -> Option<Kind> {
        Some(match self {
            Ctl::Text
            | Ctl::Mono
            | Ctl::Lines
            | Ctl::Secret(..)
            | Ctl::Choice(_)
            | Ctl::Image(_)
            | Ctl::BuildsDir
            | Ctl::Prompt(_)
            | Ctl::Map
            | Ctl::Words
            | Ctl::Voice(_)
            | Ctl::SpeechStyle(_)
            | Ctl::VoiceLanguage
            | Ctl::Model(..) => Kind::Text,
            Ctl::ForgeTokens => Kind::Raw,
            Ctl::Int(_) | Ctl::IntRange(..) => Kind::Int,
            Ctl::Float(..) => Kind::Float,
            Ctl::Vad(col) => realtime::vad_kind(col),
            Ctl::Money => Kind::OptFloat,
            Ctl::Profile => Kind::OptInt,
            Ctl::Bool | Ctl::TagHint => Kind::Flag,
            Ctl::Hold
            | Ctl::Apply(_)
            | Ctl::CheckNow
            | Ctl::Theme
            | Ctl::Scale
            | Ctl::Link(_)
            | Ctl::Budget => return None,
        })
    }
}

/// One setting. `key` is its dotted patch path (and, dashed, its anchor).
#[derive(Clone, Copy)]
struct Def {
    key: &'static str,
    label: &'static str,
    cat: &'static str,
    group: &'static str,
    ctl: Ctl,
    unit: &'static str,
    hint: &'static str,
    ph: &'static str,
    size: Size,
    /// More words the search finds it by.
    terms: &'static str,
    /// Empty is refused (the server would refuse it too).
    req: bool,
}

const fn f(
    cat: &'static str,
    group: &'static str,
    key: &'static str,
    label: &'static str,
    ctl: Ctl,
) -> Def {
    Def {
        key,
        label,
        cat,
        group,
        ctl,
        unit: "",
        hint: "",
        ph: "",
        size: Size::M,
        terms: "",
        req: false,
    }
}

impl Def {
    const fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }
    const fn hint(mut self, hint: &'static str) -> Self {
        self.hint = hint;
        self
    }
    const fn ph(mut self, ph: &'static str) -> Self {
        self.ph = ph;
        self
    }
    const fn s(mut self) -> Self {
        self.size = Size::S;
        self
    }
    const fn l(mut self) -> Self {
        self.size = Size::L;
        self
    }
    const fn terms(mut self, terms: &'static str) -> Self {
        self.terms = terms;
        self
    }
    const fn req(mut self) -> Self {
        self.req = true;
        self
    }
}

const PERIODS: &[(&str, &str)] = &[
    ("day", "per day"),
    ("month", "per month"),
    ("total", "total"),
];
const SELF_ADMIN: &[(&str, &str)] = &[("off", "off"), ("read_only", "read only"), ("full", "full")];
const PDF_MODES: &[(&str, &str)] = &[
    ("text", "text"),
    ("images", "page images"),
    ("ask", "ask each time"),
];
const CATALOG_REVISIONS: &[(&str, &str)] = &[
    ("pinned", "the commit the spec pins"),
    ("latest", "latest (main)"),
];
const BACKENDS: &[(&str, &str)] = &[
    ("cuda", "cuda"),
    ("cpu", "cpu"),
    ("vulkan", "vulkan"),
    ("metal", "metal"),
    ("hip", "hip"),
];
const NO_LOCAL_HOLD: Option<&str> = Some("the fallback must not need this GPU");

/// The three classes shaped like llama-server's (chat, aux, image): the same
/// five fields under their own patch prefix, and the class's apply.
macro_rules! class_fields {
    ($p:literal, $target:literal, $class:expr) => {
        [
            f(
                "runtimes",
                $p,
                concat!($p, ".image"),
                "Image",
                Ctl::Image($class),
            )
            .l()
            .hint("per-model overrides win")
            .terms("container podman build backends"),
            f(
                "runtimes",
                $p,
                concat!($p, ".models_dir"),
                "Models dir",
                Ctl::Mono,
            )
            .l()
            .unit("host")
            .terms("path gguf"),
            f(
                "runtimes",
                $p,
                concat!($p, ".public_prefix"),
                "Public prefix",
                Ctl::Mono,
            )
            .s()
            .hint("empty = bare ids"),
            f(
                "runtimes",
                $p,
                concat!($p, ".request_timeout_seconds"),
                "Request timeout",
                Ctl::Int(0),
            )
            .s()
            .unit("s")
            .hint("per call, once the model is up; 0 = none"),
            f(
                "runtimes",
                $p,
                concat!($p, ".extra_run_args"),
                "Extra podman run args",
                Ctl::Lines,
            )
            .unit("one per line")
            .terms("device gpu security-opt"),
            f(
                "runtimes",
                $p,
                concat!("apply.", $p),
                "Apply to running containers",
                Ctl::Apply($target),
            )
            .terms("recreate restart"),
        ]
    };
}

const CHAT: [Def; 6] = class_fields!("router", "chat", ImageClass::Chat);
const AUX: [Def; 6] = class_fields!("aux_router", "aux", ImageClass::Aux);
const IMAGE: [Def; 6] = class_fields!("image", "image", ImageClass::Image);

const COMMON: &[Def] = &[
    // Network & access
    f("network", "listen", "bind_addr", "Bind address", Ctl::Mono)
        .req()
        .unit("restart to apply")
        .ph("127.0.0.1:8787")
        .terms("listen port host interface"),
    f(
        "network",
        "listen",
        "max_body_mb",
        "Max request body",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = unlimited")
    .terms("size upload payload limit"),
    f(
        "network",
        "access",
        "auth_enabled",
        "Require gateway API keys on /v1",
        Ctl::Bool,
    )
    .hint("off: any caller that reaches the bind address may use /v1")
    .terms("auth authentication bearer"),
    f(
        "network",
        "access",
        "self_admin",
        "Self-admin tools (lmgw__*)",
        Ctl::Choice(SELF_ADMIN),
    )
    .hint("The self-admin credential is managed on Usage → Keys.")
    .terms("mcp admin self-admin"),
    f(
        "network",
        "access",
        "api_keys",
        "API keys",
        Ctl::Link("/usage/keys"),
    )
    .terms(
        "api key keys token tokens credential credentials owner client bearer create revoke rotate",
    ),
    // Retention
    f(
        "retention",
        "logs",
        "retention_days",
        "Log retention",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .terms("request log traffic"),
    f(
        "retention",
        "logs",
        "retention_max_rows",
        "Log retention",
        Ctl::Int(0),
    )
    .s()
    .unit("max rows")
    .terms("request log traffic"),
    f(
        "retention",
        "jobs",
        "jobs_retention_days",
        "Job history",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint("0 = keep all"),
    f(
        "retention",
        "jobs",
        "jobs_max_rows",
        "Job history",
        Ctl::Int(0),
    )
    .s()
    .unit("max rows")
    .hint("0 = keep all"),
    f(
        "retention",
        "chat",
        "chat_archive_days",
        "Auto-archive idle chats",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint(
        "0 = never auto-archive; pinned threads and an ongoing conversation's current thread \
         are exempt; a folder's own retention overrides this",
    )
    .terms("chat thread conversation idle archive"),
    f(
        "retention",
        "chat",
        "chat_purge_days",
        "Delete archived chats",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint(
        "0 = keep archived threads forever; pinned threads are exempt; a folder's own \
         retention overrides this",
    )
    .terms("chat thread conversation purge delete"),
    f(
        "retention",
        "rollups",
        "usage_retention_months",
        "Rollup retention",
        Ctl::Int(0),
    )
    .s()
    .unit("months")
    .hint("0 = forever")
    .terms("usage hourly"),
    f(
        "retention",
        "responses",
        "responses_store",
        "Store responses",
        Ctl::Bool,
    )
    .hint("needed for previous_response_id and approvals")
    .terms("conversations"),
    f(
        "retention",
        "responses",
        "responses_retention_hours",
        "Evict after idle",
        Ctl::Int(0),
    )
    .s()
    .unit("hours")
    .hint("0 = never")
    .terms("conversations"),
    f(
        "retention",
        "responses",
        "responses_max_chains",
        "Keep at most",
        Ctl::Int(0),
    )
    .s()
    .unit("conversations")
    .hint("0 = unlimited"),
    // Usage & cost
    f("usage-cost", "money", "currency", "Currency", Ctl::Text)
        .s()
        .hint("a label, never a conversion")
        .ph("USD")
        .req(),
    f(
        "usage-cost",
        "money",
        "global_budget_micro",
        "Global budget",
        Ctl::Money,
    )
    .s()
    .unit("units")
    .hint("in that currency; empty = none")
    .ph("none")
    .terms("spend ceiling limit"),
    f(
        "usage-cost",
        "money",
        "global_budget_period",
        "Global budget resets",
        Ctl::Choice(PERIODS),
    )
    .s(),
    f(
        "usage-cost",
        "compare",
        "local_reference_alias",
        "Compare local against",
        Ctl::Model(
            &["chat"],
            Some("the comparison is against a cloud price"),
            "none — the panel says no reference is configured",
        ),
    )
    .terms("counterfactual cloud reference alias"),
    // GPU
    f(
        "gpu",
        "admission",
        "vram.enabled",
        "Arbitrate GPU memory",
        Ctl::Bool,
    )
    .hint("off = forward everything unchanged")
    .terms("vram admission"),
    f(
        "gpu",
        "admission",
        "vram.headroom_mb",
        "Headroom",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("kept free above the GGUF estimate")
    .terms("vram"),
    f("gpu", "admission", "vram.budget_mb", "Budget", Ctl::Int(0))
        .s()
        .unit("MiB")
        .hint("0 = the GPU's real total, as the driver reports it")
        .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.queue_timeout_seconds",
        "Queue timeout",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("how long a request may wait; 0 = forever")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.load_timeout_seconds",
        "Load timeout",
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("to wait for 'loaded'")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.unload_timeout_seconds",
        "Unload timeout",
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("to wait for 'unloaded'")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.fallback_on_external",
        "Fall back when VRAM outside lmgw's control is short",
        Ctl::Bool,
    )
    .hint(
        "When a model does not fit because games, browsers or other apps use \
         GPU memory lmgw cannot free, its fallback answers at once instead of \
         queueing. Contention between lmgw's own models still queues. Turn \
         this off on shared-memory systems (APUs), where GPU memory is host \
         RAM that grows and shrinks with everything else running.",
    )
    .terms("external vram fallback apu shared memory"),
    f("gpu", "hold", "hold.active", "GPU hold", Ctl::Hold).terms("pause gaming engage release"),
    f(
        "gpu",
        "hold",
        "hold.fallback_alias",
        "Global fallback (chat models)",
        Ctl::Model(&["chat"], NO_LOCAL_HOLD, "None — refuse with 503"),
    )
    .terms("gpu_hold"),
    // Runtimes
    f(
        "runtimes",
        "containers",
        "container_prefix",
        "Container name prefix",
        Ctl::Mono,
    )
    .req()
    .ph("lmgw")
    .hint("takes effect on the next start of each model")
    .terms("podman name"),
];

const AUDIO: &[Def] = &[
    f(
        "runtimes",
        "audio",
        "audio.image",
        "Image",
        Ctl::Image(ImageClass::Audio),
    )
    .l()
    .hint("per-model overrides win")
    .terms("container podman build backends"),
    f(
        "runtimes",
        "audio",
        "audio.models_dir",
        "Models dir",
        Ctl::Mono,
    )
    .l()
    .unit("host")
    .terms("path"),
    f(
        "runtimes",
        "audio",
        "audio.voice_dir",
        "Voice library",
        Ctl::Mono,
    )
    .l()
    .unit("container path")
    .terms("voices wav"),
    f(
        "runtimes",
        "audio",
        "audio.voice_transcribe_alias",
        "Clip transcripts",
        Ctl::Model(
            &["asr"],
            None,
            "none — a clip is transcribed only when you ask",
        ),
    )
    .l()
    .hint(
        "writes an uploaded clip's transcript, which cloning models need; its configured \
         fallback is used as for any request, so pick a local model without one to keep your \
         clips on this machine",
    )
    .terms("voice clip transcript reference text asr transcribe library"),
    f(
        "runtimes",
        "audio",
        "audio.catalog_revision",
        "Catalog downloads",
        Ctl::Choice(CATALOG_REVISIONS),
    )
    .l()
    .hint(
        "a spec's pin is the commit its engine was tested with; latest takes main even where a \
         spec pins one",
    )
    .terms("audio catalog download revision commit pin pinned latest main hugging face"),
    f(
        "runtimes",
        "audio",
        "audio.backend",
        "Backend",
        Ctl::Choice(BACKENDS),
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.device",
        "Device index",
        Ctl::Int(i64::MIN),
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.threads",
        "Threads",
        Ctl::Int(i64::MIN),
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.public_prefix",
        "Public prefix",
        Ctl::Mono,
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.request_timeout_seconds",
        "Request timeout",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("per call, once the model is up; 0 = none"),
    f(
        "runtimes",
        "audio",
        "audio.busy_timeout_ms",
        "Busy timeout",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("0 = wait forever"),
    f(
        "runtimes",
        "audio",
        "audio.idle_unload_ms",
        "Idle unload",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("0 = never"),
    f(
        "runtimes",
        "audio",
        "audio.min_free_memory_mb",
        "Min free memory",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = no guard"),
    f(
        "runtimes",
        "audio",
        "audio.max_request_body_mb",
        "Max request body",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = engine default"),
    f(
        "runtimes",
        "audio",
        "audio.lazy_load",
        "Lazy-load models",
        Ctl::Bool,
    ),
    f(
        "runtimes",
        "audio",
        "audio.extra_run_args",
        "Extra podman run args",
        Ctl::Lines,
    )
    .unit("one per line")
    .terms("device gpu security-opt"),
    f(
        "runtimes",
        "audio",
        "apply.audio",
        "Apply to running containers",
        Ctl::Apply("audio"),
    )
    .terms("recreate restart"),
];

/// Container builds (container-builds §5, §7, §8).
const BUILDS: &[Def] = &[
    f(
        "backends",
        "forge",
        "forge_tokens",
        "Forge tokens",
        Ctl::ForgeTokens,
    )
    .unit("write-only")
    .hint("per host; empty keeps what is stored")
    .terms("github gitlab gitea forgejo token credential pull request pr rate limit api"),
    f(
        "backends",
        "builds",
        "builds_dir",
        "Builds directory",
        Ctl::BuildsDir,
    )
    .l()
    .unit("host")
    .hint("git mirrors, per-run worktrees and logs; empty = the default")
    .terms("path mirror worktree cache tmpfs disk"),
    f(
        "backends",
        "builds",
        "build_update_check_hours",
        "Update check",
        Ctl::IntRange(0, MAX_BUILD_UPDATE_CHECK_HOURS as i64),
    )
    .s()
    .unit("hours")
    .hint("how often builds are checked for new commits; 0 to 8760 (a year), 0 = off")
    .terms("update badge poll interval commits moved range"),
    f(
        "backends",
        "builds",
        "backends_page",
        "Builds and images",
        Ctl::Link("/backends"),
    )
    .terms("backends builds images llama.cpp audio.cpp sd.cpp"),
];

const REST: &[Def] = &[
    // Chat
    f(
        "chat",
        "prompt",
        "chat_system_prompt",
        "System prompt",
        Ctl::Prompt("/chat_system_prompt_builtin"),
    )
    .hint(
        "what a new chat thread starts with, as its own copy — existing threads keep theirs; \
         empty starts them without one",
    )
    .terms("chat default system prompt instructions persona new thread conversation"),
    f(
        "chat",
        "prompt",
        "chat_profile",
        "Profile for new threads",
        Ctl::Profile,
    )
    .hint(
        "the personality profile a new chat thread starts with, unless its folder names one; \
         Default is none. Edit profiles on the Chat profiles page",
    )
    .terms("chat personality profile persona concise voice new thread default"),
    f(
        "chat",
        "attachments",
        "chat_pdf_mode",
        "PDF attachments start as",
        Ctl::Choice(PDF_MODES),
    )
    .hint(
        "text works with every model; page images need a vision model; ask leaves the chip \
         unset and blocks Send until you choose",
    )
    .terms("chat pdf attachment text images pages ask mode"),
    f(
        "chat",
        "knowledge",
        "chat_kb_budget_tokens",
        "Excerpt budget per turn",
        Ctl::Int(1),
    )
    .s()
    .unit("tokens")
    .hint("knowledge-base excerpts one turn may carry; a thread can override it; above 0")
    .terms("chat knowledge base kb rag retrieval budget"),
    // The change feed client apps follow (client-apps design §2).
    f(
        "chat",
        "feed",
        "chat_feed_retention_days",
        "Keep changes for",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint("a client away longer reloads what it shows (resync); 0 = keep all")
    .terms("chat feed change client app device retention resync cursor"),
    f(
        "chat",
        "feed",
        "chat_feed_keepalive_s",
        "Keep-alive every",
        Ctl::Int(1),
    )
    .s()
    .unit("seconds")
    .hint("told to each client, which takes a silent feed as dead after it; open feeds keep theirs")
    .terms("chat feed keepalive keep-alive ping dead link timeout"),
    f(
        "chat",
        "feed",
        "chat_feed_page_size",
        "Catch-up page",
        Ctl::IntRange(1, lmgw_api_types::chat_feed::MAX_PAGE_SIZE as i64),
    )
    .s()
    .unit("records")
    .hint("read per query while a client catches up; bounds memory, every record is still sent; at most 10 000")
    .terms("chat feed catch-up page size since"),
    f(
        "chat",
        "feed",
        "chat_feed_live_buffer",
        "Live buffer",
        Ctl::IntRange(1, lmgw_api_types::chat_feed::MAX_LIVE_BUFFER as i64),
    )
    .s()
    .unit("events")
    .hint("turns, voice and hold events held for a slow client; past it the client gets a fresh state; at most 65 536, as it is allocated whole")
    .terms("chat feed live buffer lag slow state turn voice hold"),
    // Agents & tools
    f(
        "agents",
        "agent-containers",
        "agent_script_image",
        "Agent script image",
        Ctl::Mono,
    )
    .l()
    .hint("empty restores the default")
    .ph("docker.io/library/node:24-alpine")
    .terms("script step node"),
    f(
        "agents",
        "agent-containers",
        "agent_origin_suffix",
        "Agent origin suffix",
        Ctl::Mono,
    )
    .req()
    .s()
    .ph("localhost")
    .terms("domain dns app ui cookie"),
    f(
        "agents",
        "tool-loop",
        "responses_max_tool_calls",
        "Max tool calls",
        Ctl::Int(0),
    )
    .s()
    .hint("per response")
    .terms("responses budget"),
    f(
        "agents",
        "tool-loop",
        "responses_timeout_seconds",
        "Time limit",
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("per response")
    .terms("responses budget timeout"),
    f(
        "agents",
        "mcp",
        "sampling_alias",
        "MCP sampling alias",
        Ctl::Model(&["chat"], None, "none configured"),
    )
    .hint("answers sampling/createMessage for a server with no alias of its own"),
    // The device MCP host link (client-apps design §5.1).
    f(
        "agents",
        "mcp",
        "mcp.host_max_message_mb",
        "Device link: largest message",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = no bound of its own; a full-desktop screenshot is the large one")
    .terms("device host link websocket size"),
    f(
        "agents",
        "mcp",
        "mcp.host_max_frame_mb",
        "Device link: largest frame",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = bounded by the message limit; not both 0")
    .terms("device host link websocket size"),
    f(
        "agents",
        "mcp",
        "mcp.host_ping_interval_s",
        "Device link: ping every",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("a missed pong closes the link; 0 = no pings")
    .terms("device host link liveness"),
    // MCP Tasks (MCP Tasks design §5.2): how often a job is asked about.
    f(
        "agents",
        "mcp",
        "mcp.task_poll_interval_s",
        "Task poll interval",
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("used when a server suggests none; a server's status notifications act at once")
    .terms("mcp tasks job poll tasks/get status interval"),
    // Docs
    f(
        "docs",
        "ingest",
        "docs_ingest_reply_tokens",
        "Extraction reply budget",
        Ctl::Int(1),
    )
    .unit("tokens")
    .terms("quickdoc corpus"),
    f(
        "docs",
        "ingest",
        "docs_embed_batch",
        "Embed batch",
        Ctl::Int(1),
    )
    .s()
    .unit("texts")
    .hint("per embedding call")
    .terms("quickdoc embedding"),
    f(
        "docs",
        "ingest",
        "docs_fetch_delay_ms",
        "Fetch delay",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("between fetches to one host; 0 = none")
    .terms("quickdoc crawl"),
    f(
        "docs",
        "search",
        "docs_rerank_model",
        "Rerank model",
        Ctl::Model(
            &["rerank"],
            None,
            "auto — the aux router's enabled rerank model",
        ),
    )
    .terms("quickdoc"),
    f(
        "docs",
        "search",
        "docs_search",
        "Search stage defaults",
        Ctl::Link("/docs/playground#params"),
    )
    .terms("quickdoc k_fts k_vec rrf playground stages"),
    // Tokens & updates
    f(
        "tokens",
        "tokens",
        "hf_token",
        "Hugging Face token",
        Ctl::Secret("/has_hf_token", "clear_hf_token"),
    )
    .l()
    .hint("empty keeps what is stored")
    .terms("huggingface hf download credential"),
    f(
        "tokens",
        "tokens",
        "update_token",
        "Update feed token",
        Ctl::Secret("/has_update_token", "clear_update_token"),
    )
    .l()
    .hint("only for a private feed; empty keeps what is stored")
    .terms("credential registry gitlab update feed"),
    f(
        "tokens",
        "updates",
        "update_check_enabled",
        "Check for new versions in the background",
        Ctl::Bool,
    )
    .terms("update version"),
    f(
        "tokens",
        "updates",
        "update_check_now",
        "Check for app update now",
        Ctl::CheckNow,
    )
    .terms("update version"),
    // Appearance
    f("appearance", "window", "theme", "Theme", Ctl::Theme)
        .s()
        .terms("dark light colour color"),
    f(
        "appearance",
        "window",
        "ui_scale",
        "Interface scale",
        Ctl::Scale,
    )
    .s()
    .terms("zoom size font"),
];

/// Every setting, in page order.
fn fields() -> impl Iterator<Item = &'static Def> {
    COMMON
        .iter()
        .chain(CHAT.iter())
        .chain(AUX.iter())
        .chain(AUDIO.iter())
        .chain(IMAGE.iter())
        .chain(BUILDS.iter())
        .chain(REST.iter())
        .chain(chat_voice::CHAT_VOICE.iter())
        .chain(realtime::REALTIME.iter())
}

/// `vram.headroom_mb` → `f-vram-headroom-mb`: what `/settings/gpu#…` names.
fn anchor(key: &str) -> String {
    format!("f-{}", key.replace(['.', '_'], "-"))
}

/// The deep link to a setting, for the "Settings → X" pointers elsewhere.
pub(super) fn href(key: &str) -> String {
    let cat = fields().find(|d| d.key == key).map_or("network", |d| d.cat);
    format!("/settings/{cat}#{}", anchor(key))
}

fn pointer(key: &str) -> String {
    format!("/{}", key.replace('.', "/"))
}

fn cat_label(slug: &str) -> &'static str {
    CATS.iter()
        .find(|(s, ..)| *s == slug)
        .map_or("", |(_, l, _)| l)
}

/// The category a changed form key belongs to (a secret's clear tick counts
/// with its secret).
fn cat_of_key(k: &str) -> Option<&'static str> {
    fields()
        .find(|d| d.key == k || matches!(d.ctl, Ctl::Secret(_, clear) if clear == k))
        .map(|d| d.cat)
}

/// Everything the search reads for one setting, lowercased.
fn haystack(d: &Def) -> String {
    let group = GROUPS
        .iter()
        .find(|g| g.id == d.group)
        .map_or("", |g| g.title);
    format!(
        "{} {} {} {} {} {} {} {}",
        d.label,
        d.unit,
        d.hint,
        d.key,
        d.key.replace(['.', '_'], " "),
        group,
        cat_label(d.cat),
        d.terms
    )
    .to_lowercase()
}

/// What the server holds, as the form's flat baseline: one entry per form
/// key, in the shape its box shows (money in units, lists as lines, a
/// missing alias as "").
fn baseline(s: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    for d in fields() {
        let read = || s.pointer(&pointer(d.key)).cloned().unwrap_or(Value::Null);
        match d.ctl {
            Ctl::Secret(_, clear) => {
                m.insert(d.key.into(), Value::String(String::new()));
                m.insert(clear.into(), Value::Bool(false));
            }
            Ctl::ForgeTokens => {
                m.insert(d.key.into(), json!(forge_baseline(&read())));
            }
            Ctl::Money => {
                let micro = read().as_i64().unwrap_or(0);
                m.insert(
                    d.key.into(),
                    if micro > 0 {
                        json!(micro as f64 / 1e6)
                    } else {
                        Value::Null
                    },
                );
            }
            Ctl::Map => {
                m.insert(d.key.into(), Value::String(realtime::map_text(&read())));
            }
            Ctl::Words => {
                m.insert(d.key.into(), Value::String(realtime::words_text(&read())));
            }
            Ctl::Lines => {
                let lines: Vec<String> = read()
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                m.insert(d.key.into(), Value::String(lines.join("\n")));
            }
            c if c.form_kind() == Some(Kind::Text) => {
                let v = read();
                m.insert(
                    d.key.into(),
                    if v.is_null() {
                        Value::String(String::new())
                    } else {
                        v
                    },
                );
            }
            c if c.form_kind().is_some() => {
                m.insert(d.key.into(), read());
            }
            _ => {}
        }
    }
    m
}

/// The form's patch in the shape the op takes: lines back into lists, money
/// back into micro-units (empty is 0, "no budget").
fn finish(mut patch: Value) -> Value {
    // The forge-token rows become the op's two keys: tokens to set by host,
    // hosts to clear.
    if let Some(obj) = patch.as_object_mut() {
        if let Some(rows) = obj.remove("forge_tokens") {
            let (set, clear) = forge_patch(&forge_rows(&rows));
            if !set.is_empty() {
                obj.insert("forge_tokens".into(), json!(set));
            }
            if !clear.is_empty() {
                obj.insert("clear_forge_tokens".into(), json!(clear));
            }
        }
    }
    for d in fields() {
        let Some(v) = patch.pointer_mut(&pointer(d.key)) else {
            continue;
        };
        match d.ctl {
            Ctl::Lines => {
                let list: Vec<String> = v
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                *v = json!(list);
            }
            Ctl::Money => {
                *v = json!(v.as_f64().map_or(0, |u| (u * 1e6).round() as i64));
            }
            Ctl::Map => {
                *v = realtime::map_object(v.as_str().unwrap_or_default());
            }
            Ctl::Words => {
                *v = realtime::words_list(v.as_str().unwrap_or_default());
            }
            _ => {}
        }
    }
    patch
}

/// A number that parses but is out of range, or a required field left
/// empty — a parse error is the form's own. Blocks Save like one.
fn range_error(d: &Def, text: &str) -> Option<String> {
    let t = text.trim();
    if d.req && t.is_empty() {
        return Some("cannot be empty".to_string());
    }
    match d.ctl {
        Ctl::Int(min) if min > i64::MIN => match t.parse::<i64>() {
            Ok(n) if n < min => Some(if min == 0 {
                "must be 0 or more".to_string()
            } else {
                format!("must be at least {min}")
            }),
            _ => None,
        },
        Ctl::IntRange(min, max) => match t.parse::<i64>() {
            Ok(n) if n < min || n > max => Some(format!("must be between {min} and {max}")),
            _ => None,
        },
        Ctl::Float(min, max) => match t.parse::<f64>() {
            Ok(n) if !(min..=max).contains(&n) => Some(format!("must be between {min} and {max}")),
            _ => None,
        },
        Ctl::Vad(col) => realtime::vad_range_error(col, t),
        Ctl::Map => realtime::map_error(t),
        Ctl::Money => match t.replace(',', ".").parse::<f64>() {
            Ok(v) if v < 0.0 => Some("must be 0 or more, or empty for none".to_string()),
            _ => None,
        },
        Ctl::BuildsDir if !t.is_empty() && !t.starts_with('/') => {
            Some("an absolute path, or empty for the default".to_string())
        }
        Ctl::BuildsDir if t.trim_end_matches('/').is_empty() && !t.is_empty() => {
            Some("cannot be / — builds create and remove directories under it".to_string())
        }
        _ => None,
    }
}

/// What is wrong with a field's draft, in words: [`range_error`], a
/// cross-field refusal ([`realtime::cross_error`]), or a forge row the
/// server would refuse.
fn problem(form: FormState, d: &Def) -> Option<String> {
    range_error(d, &form.text(d.key))
        .or_else(|| chat_voice::error(d.key, &form.text(d.key)))
        .or_else(|| realtime::cross_error(form, d.key))
        .or_else(|| match d.ctl {
            Ctl::ForgeTokens => forge_error(&forge_rows(&form.value(d.key))),
            _ => None,
        })
}

/// The keys a save of the changed keys `dirty` judges — the server's rule
/// (realtime design §12, "Judged when touched"): what it changes, and what
/// a change is judged together with ([`realtime::judged_with`]). A value a
/// hand edit broke in a field nobody touched is shown, as a warning, but
/// holds back no other change: the server would not refuse it either.
fn judged_keys(dirty: &[String]) -> BTreeSet<&'static str> {
    let groups: BTreeSet<&str> = dirty
        .iter()
        .filter_map(|k| realtime::judged_with(k))
        .collect();
    fields()
        .map(|d| d.key)
        .filter(|k| {
            dirty.iter().any(|d| d == k)
                || realtime::judged_with(k).is_some_and(|g| groups.contains(g))
        })
        .collect()
}

/// A field's message as the page shows it: `(error, warning)` — the
/// problem as an error Save waits for when the save judges the field, else
/// as a warning about the stored value.
fn messages(form: FormState, d: &Def, judged: bool) -> (Option<String>, Option<String>) {
    match problem(form, d) {
        Some(m) if judged => (Some(m), None),
        Some(m) => (
            None,
            Some(format!("as stored: {m} — other changes still save")),
        ),
        None => (None, None),
    }
}

/// What Save waits for, per key: the problems of the fields `judged`.
fn blocking(form: FormState, judged: &BTreeSet<&'static str>) -> BTreeMap<&'static str, String> {
    fields()
        .filter(|d| judged.contains(d.key))
        .filter_map(|d| problem(form, d).map(|e| (d.key, e)))
        .collect()
}

// ---------------------------------------------------------------------------
// Forge tokens (container-builds §7)
// ---------------------------------------------------------------------------

/// The host a forge token is always offered for: the official repositories
/// live there, and its anonymous API limit is the one that bites.
const GITHUB: &str = "github.com";

/// One host of the forge-token list, as the draft holds it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ForgeRow {
    /// The row's identity in the list: the host of a stored (or the GitHub)
    /// row, `new-<n>` for one added here.
    key: String,
    host: String,
    /// A token to set on Save; empty keeps what is stored.
    token: String,
    /// The server holds a token for this host.
    stored: bool,
    /// Erase the stored token on Save.
    clear: bool,
}

impl ForgeRow {
    /// Typed in here, not read from the server: its host is editable and it
    /// can be removed.
    fn added(&self) -> bool {
        self.key.starts_with("new-")
    }
}

/// The rows for what the server holds (`{"github.com": "<set>", …}`):
/// GitHub first whether or not it has a token, then every other host.
fn forge_baseline(stored: &Value) -> Vec<ForgeRow> {
    let mut hosts: Vec<String> = stored
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    hosts.sort();
    let row = |host: &str, stored: bool| ForgeRow {
        key: host.to_string(),
        host: host.to_string(),
        stored,
        ..ForgeRow::default()
    };
    let mut rows = vec![row(GITHUB, hosts.iter().any(|h| h == GITHUB))];
    rows.extend(hosts.iter().filter(|h| *h != GITHUB).map(|h| row(h, true)));
    rows
}

fn forge_rows(v: &Value) -> Vec<ForgeRow> {
    serde_json::from_value(v.clone()).unwrap_or_default()
}

/// The save's two halves: the tokens to set by host, and the hosts to clear.
fn forge_patch(rows: &[ForgeRow]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut set = BTreeMap::new();
    let mut clear = Vec::new();
    for r in rows {
        let host = r.host.trim().to_ascii_lowercase();
        if r.clear {
            if r.stored {
                clear.push(host);
            }
        } else if !r.token.trim().is_empty() && !host.is_empty() {
            set.insert(host, r.token.trim().to_string());
        }
    }
    (set, clear)
}

/// What the server holds after `rows` were saved, when it could not be read
/// back: a set token is stored, a cleared one gone, every box empty.
fn forge_after(rows: &[ForgeRow]) -> Vec<ForgeRow> {
    rows.iter()
        .filter_map(|r| {
            let host = r.host.trim().to_ascii_lowercase();
            let stored = if r.clear {
                false
            } else {
                r.stored || !r.token.trim().is_empty()
            };
            (stored || host == GITHUB).then(|| ForgeRow {
                key: host.clone(),
                host,
                stored,
                ..ForgeRow::default()
            })
        })
        .collect()
}

/// A forge host as `settings_set_full` accepts it: the host alone, lowercase
/// labels, an optional port (`git.example:8443`). The server checks again.
fn forge_host_ok(h: &str) -> bool {
    let h = h.trim().to_ascii_lowercase();
    let (name, port) = match h.split_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (h.as_str(), None),
    };
    let name_ok = !name.is_empty()
        && name.split('.').all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    name_ok && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0))
}

/// What blocks Save in the forge-token list, in words.
fn forge_error(rows: &[ForgeRow]) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    for r in rows {
        let host = r.host.trim().to_ascii_lowercase();
        let token = r.token.trim();
        if r.added() {
            if host.is_empty() {
                return Some(if token.is_empty() {
                    "an added host is empty: fill it in, or remove the row".to_string()
                } else {
                    "a token needs its host".to_string()
                });
            }
            if !forge_host_ok(&host) {
                return Some(format!(
                    "\u{201c}{host}\u{201d} is not a host name: the host alone, e.g. \
                     git.example.com or git.example:8443"
                ));
            }
            if token.is_empty() {
                return Some(format!("the token for {host} is empty"));
            }
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Some(format!("the token for {host} contains whitespace"));
        }
        if seen.contains(&host) {
            return Some(format!("{host} is listed twice"));
        }
        seen.push(host);
    }
    None
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// What every part of the page reads: the draft, the server's last answer,
/// the keys a save of the draft judges, the search, the category, the class
/// sections' open state.
#[derive(Clone, Copy)]
struct Page {
    form: FormState,
    data: RwSignal<Option<Value>>,
    /// [`judged_keys`] of the draft's changes.
    judged: Memo<BTreeSet<&'static str>>,
    words: Memo<Vec<String>>,
    active: Memo<&'static str>,
    folds: [(&'static str, RwSignal<bool>); 4],
}

impl Page {
    fn searching(&self) -> bool {
        self.words.with(|w| !w.is_empty())
    }

    fn matches(&self, d: &Def) -> bool {
        let hay = haystack(d);
        self.words
            .with(|w| w.iter().all(|w| hay.contains(w.as_str())))
    }

    fn shows(&self, d: &Def) -> bool {
        if self.searching() {
            self.matches(d)
        } else {
            self.active.get() == d.cat
        }
    }

    fn fold(&self, group: &str) -> Option<RwSignal<bool>> {
        self.folds
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, o)| *o)
    }

    /// A boolean the server reported (`/has_hf_token`).
    fn server_flag(&self, ptr: &str) -> bool {
        self.data.with(|d| {
            d.as_ref()
                .and_then(|v| v.pointer(ptr))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
    }
}

#[component]
pub fn Settings() -> impl IntoView {
    let toasts = use_toasts();
    let navigate = use_navigate();
    let form = FormState::new(Map::new());
    let data = RwSignal::new(None::<Value>);
    let load_err = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let save_err = RwSignal::new(None::<String>);

    // What the server holds becomes the baseline every field is compared to.
    let take = move |v: Value| {
        for d in fields() {
            if let Some(kind) = d.ctl.form_kind() {
                form.with_kind(d.key, kind);
            }
            if let Ctl::Secret(_, clear) = d.ctl {
                form.with_kind(clear, Kind::Flag);
            }
        }
        form.rebase(baseline(&v));
        data.set(Some(v));
        load_err.set(None);
    };
    let scope = Scope::new();
    let load = move || {
        scope.spawn(async move {
            match crate::api::get::<Value>("/api/settings-full").await {
                Ok(v) => take(v),
                Err(e) => load_err.set(Some(e.to_string())),
            }
        });
    };
    load();

    let query = crate::url_state::use_query_signal("q");
    let words = Memo::new(move |_| {
        query.with(|q| {
            q.split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
    });
    let active = crate::url_state::use_view("cat", CAT_SLUGS, "network");
    let fold = |group: &'static str, persist: &str, open: bool| {
        (
            group,
            crate::prefs::persisted_bool(&format!("open.{persist}"), open),
        )
    };
    let judged = Memo::new(move |_| judged_keys(&form.dirty_keys()));
    let page = Page {
        form,
        data,
        judged,
        words,
        active,
        folds: [
            fold("router", "settings.runtimes.chat", true),
            fold("aux_router", "settings.runtimes.aux", false),
            fold("audio", "settings.runtimes.audio", false),
            fold("image", "settings.runtimes.image", false),
        ],
    };

    // Numbers that parse but are out of range, and what the server would
    // refuse across fields — of the fields this save judges only, per key.
    let range_errs = Memo::new(move |_| judged.with(|j| blocking(form, j)));
    let invalid = Signal::derive(move || form.invalid_count() + range_errs.with(|r| r.len()));
    let dirty_count = Signal::derive(move || form.dirty_count());
    let dirty_by_cat = Memo::new(move |_| {
        let mut n: BTreeMap<&'static str, usize> = BTreeMap::new();
        for k in form.dirty_keys() {
            if let Some(c) = cat_of_key(&k) {
                *n.entry(c).or_default() += 1;
            }
        }
        n
    });
    let detail = Signal::derive(move || {
        dirty_by_cat.with(|by| {
            CATS.iter()
                .filter_map(|(slug, label, _)| by.get(slug).map(|n| format!("{label} ({n})")))
                .collect::<Vec<_>>()
                .join(" · ")
        })
    });
    let matches_by_cat = Memo::new(move |_| {
        let mut n: BTreeMap<&'static str, usize> = BTreeMap::new();
        if page.searching() {
            for d in fields() {
                if page.matches(d) {
                    *n.entry(d.cat).or_default() += 1;
                }
            }
        }
        n
    });
    use_dirty_guard().watch("Settings", Signal::derive(move || dirty_count.get() > 0));

    let save = Callback::new(move |()| {
        if saving.get_untracked() || invalid.get_untracked() > 0 {
            return;
        }
        let Ok(patch) = form.patch() else { return };
        let patch = finish(patch);
        let dirty = form.dirty_keys();
        // The fields stay editable while the save is out: what was sent is
        // kept apart from what is typed meanwhile, which stays a draft.
        let sent = form.snapshot();
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/settings_set_full", &patch).await;
            match res {
                Ok(v) => {
                    save_err.set(None);
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("settings saved")
                        .to_string());
                    // Read back what was stored: the server trims and
                    // normalises (a prefix loses its slashes), and the
                    // secrets' "stored" flags move. Not for a page that was
                    // left while the save was out (the toast above still
                    // told how it went).
                    if !scope.alive() {
                        return;
                    }
                    match crate::api::get::<Value>("/api/settings-full").await {
                        Ok(v) => {
                            form.saved(&sent, baseline(&v));
                            data.set(Some(v));
                        }
                        Err(e) => {
                            // Stored all the same: what was sent is the new
                            // baseline, or the bar would say "unsaved" beside
                            // a "saved" toast (review code:S4). A secret is
                            // write-only either way: its box empties.
                            let mut now = form.baseline_after(&sent);
                            for d in fields() {
                                if let Ctl::Secret(_, clear) = d.ctl {
                                    now.insert(d.key.into(), Value::String(String::new()));
                                    now.insert(clear.into(), Value::Bool(false));
                                }
                                if let Ctl::ForgeTokens = d.ctl {
                                    let rows = forge_rows(&now[d.key]);
                                    now.insert(d.key.into(), json!(forge_after(&rows)));
                                }
                            }
                            form.saved(&sent, now);
                            toasts.err(format!("saved, but not read back: {e}"));
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    // A refusal refuses the whole patch. When one field was
                    // changed, the message is about that field: say it there
                    // too.
                    if let [only] = dirty.as_slice() {
                        form.set_error(only, msg.clone());
                    }
                    save_err.set(Some(msg));
                }
            }
            saving.set(false);
        });
    });
    let discard = Callback::new(move |()| {
        form.discard();
        save_err.set(None);
    });

    // `/settings/<cat>#f-<key>`: scroll the field into view and flash it,
    // opening the class section that holds it first. Once per link, and once
    // the fields are there: a save refetches `data`, and that must not scroll
    // back, flash again or reopen a section folded since (review code:S2).
    let loc = use_location();
    let loaded = Memo::new(move |_| data.with(Option::is_some));
    Effect::new(move |_| {
        let hash = loc.hash.get();
        active.track();
        if !loaded.get() {
            return;
        }
        let id = hash.trim_start_matches('#').to_string();
        if !id.starts_with("f-") {
            return;
        }
        if let Some(open) = fields()
            .find(|d| anchor(d.key) == id)
            .and_then(|d| page.fold(d.group))
        {
            open.set(true);
        }
        set_timeout(move || flash(&id), Duration::from_millis(60));
    });

    // The narrow pane's picker for what the rail does.
    let cat_sel = RwSignal::new(active.get_untracked().to_string());
    Effect::new(move |_| {
        let a = active.get().to_string();
        if cat_sel.get_untracked() != a {
            cat_sel.set(a);
        }
    });
    Effect::new(move |_| {
        let v = cat_sel.get();
        if v != active.get_untracked() {
            navigate(&format!("/settings/{v}"), Default::default());
        }
    });
    let cat_opts = Signal::derive(|| {
        CATS.iter()
            .map(|(s, l, _)| (s.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });

    let sub = move || {
        data.with(|d| {
            d.as_ref()
                .map(|v| {
                    format!(
                        "v{} · data in {}",
                        v["version"].as_str().unwrap_or_default(),
                        v["data_dir"].as_str().unwrap_or_default()
                    )
                })
                .unwrap_or_default()
        })
    };
    let search_box = move || {
        let q_box: NodeRef<leptos::html::Input> = NodeRef::new();
        use_slash_focus(q_box);
        view! {
            <input
                class="input set-search"
                type="search"
                node_ref=q_box
                data-slash
                placeholder="Search settings"
                aria-label="Search settings"
                title="Search · / focuses, Esc clears"
                data-untracked=""
                prop:value=move || query.get()
                on:input=move |ev| query.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Escape" {
                        query.set(String::new());
                    }
                }
            />
        }
    };
    let search_line = move || {
        let q = query.get();
        if q.trim().is_empty() {
            return None;
        }
        let n: usize = matches_by_cat.with(|m| m.values().sum());
        let text = match n {
            0 => format!("No setting matches \u{201c}{}\u{201d}", q.trim()),
            1 => format!("1 setting matches \u{201c}{}\u{201d}", q.trim()),
            n => format!("{n} settings match \u{201c}{}\u{201d}", q.trim()),
        };
        Some(view! {
            <div class="set-search-line">
                {text} " · "
                <button class="link-btn" on:click=move |_| query.set(String::new())>
                    "Esc clears"
                </button>
            </div>
        })
    };

    view! {
        <PageFrame title="Settings" sub=sub mode=PageMode::Split class="settings">
            <nav class="split-rail set-rail" aria-label="Settings categories">
                {search_box}
                {CATS
                    .iter()
                    .map(|(slug, label, _)| {
                        let slug: &'static str = slug;
                        let current = move || {
                            (!page.searching() && active.get() == slug).then_some("page")
                        };
                        let dirty = move || dirty_by_cat.with(|m| m.get(slug).copied().unwrap_or(0));
                        let hits = move || matches_by_cat.with(|m| m.get(slug).copied().unwrap_or(0));
                        view! {
                            <a class="rail-item" href=format!("/settings/{slug}") aria-current=current>
                                <span class="rail-label">{*label}</span>
                                {move || {
                                    (dirty() > 0)
                                        .then(|| {
                                            view! {
                                                <span class="count attn" title="unsaved changes here">
                                                    {dirty()}
                                                </span>
                                            }
                                        })
                                }}
                                {move || {
                                    page.searching()
                                        .then(|| {
                                            view! {
                                                <span
                                                    class="count"
                                                    class:zero=move || hits() == 0
                                                    title="settings matching the search"
                                                >
                                                    {hits()}
                                                </span>
                                            }
                                        })
                                }}
                            </a>
                        }
                    })
                    .collect_view()}
            </nav>
            <div class="split-pane set-pane">
                <div class="rail-select">
                    {search_box} <Select value=cat_sel options=cat_opts/>
                </div>
                <div class="fill-pane set-scroll">
                    {search_line}
                    {move || {
                        load_err
                            .get()
                            .map(|e| {
                                view! {
                                    <div class="notice err row">
                                        "Loading the settings failed: " {e}
                                        <button class="btn ghost sm" on:click=move |_| load()>
                                            "Retry"
                                        </button>
                                    </div>
                                }
                            })
                    }}
                    <Show when=move || data.with(Option::is_some)>
                        {CATS.iter().map(|c| cat_view(c, page)).collect_view()}
                    </Show>
                    <Show when=move || data.with(Option::is_none) && load_err.with(Option::is_none)>
                        <div class="dim">"Loading…"</div>
                    </Show>
                </div>
                <div class="page-foot">
                    <SaveBar
                        dirty_count=dirty_count
                        detail=detail
                        invalid=invalid
                        saving=saving
                        error=save_err
                        on_save=save
                        on_discard=discard
                    />
                </div>
            </div>
        </PageFrame>
    }
}

/// Scroll a field into the middle of the pane and flash it once.
fn flash(id: &str) {
    let Some(el) = document().get_element_by_id(id) else {
        return;
    };
    let opts = web_sys::ScrollIntoViewOptions::new();
    opts.set_block(web_sys::ScrollLogicalPosition::Center);
    el.scroll_into_view_with_scroll_into_view_options(&opts);
    let _ = el.class_list().remove_1("flash");
    // Re-adding the class in the same frame would not restart the animation.
    let again = el.clone();
    request_animation_frame(move || {
        let _ = again.class_list().add_1("flash");
    });
    set_timeout(
        move || {
            let _ = el.class_list().remove_1("flash");
        },
        Duration::from_millis(1800),
    );
}

fn cat_view(c: &'static (&'static str, &'static str, &'static str), page: Page) -> impl IntoView {
    let (slug, label, blurb) = *c;
    let shown = move || {
        if page.searching() {
            fields().any(|d| d.cat == slug && page.matches(d))
        } else {
            page.active.get() == slug
        }
    };
    view! {
        <section class="set-cat" hidden=move || !shown()>
            <header class="set-cat-head">
                <h2>
                    <a href=format!("/settings/{slug}")>{label}</a>
                </h2>
                <span class="dim">{blurb}</span>
            </header>
            <div class="card-flow set-flow">
                {GROUPS
                    .iter()
                    .filter(|g| g.cat == slug)
                    .map(|g| group_view(g, page))
                    .collect_view()}
            </div>
        </section>
    }
}

/// How a run of fields is laid out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lay {
    Grid(Size),
    Checks,
    Block,
    Acts,
    /// The Smart Turn table: a run of [`Ctl::Vad`] cells, one table.
    Vad,
}

fn lay(d: &Def) -> Lay {
    match d.ctl {
        Ctl::Bool => Lay::Checks,
        Ctl::Lines
        | Ctl::Link(_)
        | Ctl::Hold
        | Ctl::ForgeTokens
        | Ctl::Prompt(_)
        | Ctl::Map
        | Ctl::Words
        | Ctl::SpeechStyle(_)
        | Ctl::TagHint
        | Ctl::Budget => Lay::Block,
        Ctl::Vad(_) => Lay::Vad,
        Ctl::Apply(_) | Ctl::CheckNow => Lay::Acts,
        Ctl::Theme | Ctl::Scale => Lay::Grid(Size::S),
        _ => Lay::Grid(d.size),
    }
}

fn group_view(grp: &'static Group, page: Page) -> AnyView {
    let defs: Vec<&'static Def> = fields().filter(|d| d.group == grp.id).collect();
    let any_shown = {
        let defs = defs.clone();
        move || defs.iter().any(|d| page.shows(d))
    };
    let body = {
        let defs = defs.clone();
        move || {
            // Runs of one layout share a container: a field grid of one cell
            // size, a row of ticks, a block, the applies-now actions.
            let mut runs: Vec<(Lay, Vec<&'static Def>)> = Vec::new();
            for d in &defs {
                match runs.last_mut() {
                    Some((l, v)) if *l == lay(d) => v.push(d),
                    _ => runs.push((lay(d), vec![d])),
                }
            }
            view! {
                {runs.into_iter().map(|(l, v)| run_view(l, v, page)).collect_view()}
                {explain(grp.id, page)}
            }
        }
    };
    if grp.fold.is_empty() {
        return view! {
            <div class="set-flow-item" hidden=move || !any_shown()>
                <section class="card edit-section set-group">
                    <h3>{grp.title}</h3>
                    {body()}
                </section>
            </div>
        }
        .into_any();
    }
    let form = page.form;
    let prefix = grp.id;
    // Folded, the section still says what it runs and where from — and that
    // something in it is unsaved.
    let summary = Signal::derive(move || {
        let n = form
            .dirty_keys()
            .iter()
            .filter(|k| k.starts_with(&format!("{prefix}.")))
            .count();
        let mut parts = Vec::new();
        if n > 0 {
            parts.push(format!("{n} unsaved"));
        }
        parts.push(form.text(&format!("{prefix}.image")));
        parts.push(form.text(&format!("{prefix}.models_dir")));
        parts.retain(|p| !p.is_empty());
        parts.join(" · ")
    });
    let open = page.fold(grp.id).unwrap_or_else(|| RwSignal::new(true));
    let title = format!("{} · {}", grp.title, grp.sub);
    view! {
        <div class="set-flow-item" hidden=move || !any_shown()>
            <div class="card set-group set-fold">
                <Section
                    title=title
                    summary=summary
                    open=open
                    force_open=Signal::derive(move || page.searching())
                >
                    {body()}
                </Section>
            </div>
        </div>
    }
    .into_any()
}

fn run_view(l: Lay, defs: Vec<&'static Def>, page: Page) -> AnyView {
    if l == Lay::Vad {
        return realtime::vad_table(defs, page);
    }
    let any_shown = {
        let defs = defs.clone();
        move || defs.iter().any(|d| page.shows(d))
    };
    let items = defs.into_iter().map(|d| def_view(d, page)).collect_view();
    let class = match l {
        Lay::Grid(Size::S) => "field-grid fg-s",
        Lay::Grid(Size::M) => "field-grid fg-m",
        Lay::Grid(Size::L) => "field-grid fg-l",
        Lay::Checks => "set-checks",
        Lay::Block => "set-blocks",
        Lay::Acts => "set-acts",
        Lay::Vad => unreachable!("drawn by realtime::vad_table above"),
    };
    view! { <div class=class hidden=move || !any_shown()>{items}</div> }.into_any()
}

/// A `Select` or `ModelPicker` holds its own signal; this keeps one in step
/// with a form key both ways (Discard or a rebase moves the control, a pick
/// moves the draft).
fn bridge(form: FormState, key: &'static str) -> RwSignal<String> {
    let now = move || {
        form.draft.with_untracked(|d| match d.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        })
    };
    let sig = RwSignal::new(now());
    Effect::new(move |_| {
        let t = form.text(key);
        if sig.get_untracked() != t {
            sig.set(t);
        }
    });
    Effect::new(move |_| {
        let v = sig.get();
        if now() != v {
            form.set_text(key, v);
        }
    });
    sig
}

fn def_view(d: &'static Def, page: Page) -> AnyView {
    let form = page.form;
    let k = d.key;
    let id = anchor(k);
    let hidden = Signal::derive(move || !page.shows(d));
    let dirty = form.dirty_signal(k);
    // A problem of a field this save does not judge is the stored value's:
    // a warning, and Save goes ahead (`judged_keys`).
    let shown = Memo::new(move |_| messages(form, d, page.judged.with(|j| j.contains(k))));
    let error = Signal::derive(move || form.error(k).or_else(|| shown.get().0));
    let warn = Signal::derive(move || shown.get().1);
    match d.ctl {
        Ctl::Text | Ctl::Mono | Ctl::Int(_) | Ctl::IntRange(..) | Ctl::Float(..) | Ctl::Money => {
            let money = matches!(d.ctl, Ctl::Money);
            let numeric = matches!(
                d.ctl,
                Ctl::Int(_) | Ctl::IntRange(..) | Ctl::Float(..) | Ctl::Money
            );
            let class = if matches!(d.ctl, Ctl::Text) {
                "input"
            } else {
                "input mono"
            };
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <input
                        class=class
                        placeholder=d.ph
                        inputmode=if numeric { "decimal" } else { "text" }
                        spellcheck="false"
                        prop:value=move || form.text(k)
                        on:input=move |ev| {
                            let v = event_target_value(&ev);
                            // Comma decimals are how half the world writes
                            // money; "10,50" means 10.50, not "no budget".
                            form.set_text(k, if money { v.replace(',', ".") } else { v });
                        }
                    />
                </Field>
            }
            .into_any()
        }
        Ctl::Choice(opts) => {
            let value = bridge(form, k);
            let options = Signal::derive(move || {
                opts.iter()
                    .map(|(v, l)| (v.to_string(), l.to_string()))
                    .collect::<Vec<_>>()
            });
            view! {
                <Field label=d.label unit=d.unit hint=d.hint dirty=dirty id=id hidden=hidden>
                    <Select value=value options=options/>
                </Field>
            }
            .into_any()
        }
        Ctl::Profile => {
            let value = bridge(form, k);
            let options = chat_profile::options();
            view! {
                <Field label=d.label unit=d.unit hint=d.hint dirty=dirty id=id hidden=hidden>
                    <Select value=value options=options placeholder="Default"/>
                </Field>
            }
            .into_any()
        }
        Ctl::Image(class) => {
            let value = bridge(form, k);
            // The audio class says which GPU backend it runs; an image built
            // for another one is worth a warning.
            let backend = Signal::derive(move || {
                if class == ImageClass::Audio {
                    form.text("audio.backend")
                } else {
                    String::new()
                }
            });
            // Every category stays mounted: the picker reads the local
            // images once it is on screen — its category shown (or a search
            // matching it) and its class section open — not on every visit.
            let visible = Signal::derive(move || {
                page.shows(d)
                    && (page.searching() || page.fold(d.group).is_none_or(|o| o.get()))
            });
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <ImagePicker value=value class=class backend=backend visible=visible/>
                </Field>
            }
            .into_any()
        }
        Ctl::ForgeTokens => view! {
            <ForgeTokensField page=page id=id hidden=hidden error=error warn=warn/>
        }
        .into_any(),
        Ctl::BuildsDir => {
            let server = move |key: &'static str| {
                page.data.with(|d| {
                    d.as_ref()
                        .and_then(|v| v[key].as_str())
                        .map(str::to_string)
                })
            };
            let effective = move || server("builds_dir_effective").unwrap_or_default();
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <input
                        class="input mono"
                        placeholder=effective
                        spellcheck="false"
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    />
                    <div class="field-hint">
                        "in use now: " <code>{effective}</code>
                    </div>
                    {move || {
                        server("builds_dir_warning")
                            .map(|w| view! { <div class="notice warn set-notice">{w}</div> })
                    }}
                </Field>
            }
            .into_any()
        }
        Ctl::Model(tasks, no_local, empty) => {
            let value = bridge(form, k);
            let picker = match no_local {
                Some(reason) => view! {
                    <ModelPicker
                        value=value
                        tasks=tasks
                        empty_label=empty
                        recent_key="settings"
                        disallow=(Callback::new(|e: CatalogEntry| e.local), reason)
                    />
                }
                .into_any(),
                None => view! {
                    <ModelPicker value=value tasks=tasks empty_label=empty recent_key="settings"/>
                }
                .into_any(),
            };
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=Signal::derive(move || form.error(k))
                    id=id
                    hidden=hidden
                >
                    {picker}
                </Field>
            }
            .into_any()
        }
        Ctl::Secret(has, clear) => {
            let stored = move || page.server_flag(has);
            let either = Signal::derive(move || form.is_dirty(k) || form.is_dirty(clear));
            view! {
                <Field
                    label=d.label
                    unit=Signal::derive(move || if stored() { "stored" } else { "not set" })
                    hint=d.hint
                    dirty=either
                    error=Signal::derive(move || form.error(k))
                    id=id
                    hidden=hidden
                >
                    <div class="row secret-row">
                        <input
                            class="input mono"
                            type="password"
                            autocomplete="off"
                            placeholder=move || if stored() { "type to replace" } else { "" }
                            prop:value=move || form.text(k)
                            on:input=move |ev| form.set_text(k, event_target_value(&ev))
                        />
                        <Show when=stored>
                            <label class="check" title="Erased on Save">
                                <input
                                    type="checkbox"
                                    prop:checked=move || form.flag(clear)
                                    on:change=move |ev| form.set_flag(clear, event_target_checked(&ev))
                                />
                                "clear"
                            </label>
                        </Show>
                    </div>
                </Field>
            }
            .into_any()
        }
        Ctl::Bool => view! {
            <label class="check check-line" class:dirty=move || dirty.get() id=id hidden=move || hidden.get()>
                <input
                    type="checkbox"
                    prop:checked=move || form.flag(k)
                    on:change=move |ev| form.set_flag(k, event_target_checked(&ev))
                />
                <span>{d.label}</span>
                {(!d.hint.is_empty()).then(|| view! { <span class="check-hint">{d.hint}</span> })}
            </label>
        }
        .into_any(),
        Ctl::Lines => {
            // As many rows as there are lines, and one to type into.
            let rows = move || (form.text(k).lines().count() + 1).max(2).to_string();
            view! {
                <Field label=d.label unit=d.unit hint=d.hint dirty=dirty id=id hidden=hidden>
                    <textarea
                        class="input mono ta set-ta"
                        rows=rows
                        spellcheck="false"
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    ></textarea>
                </Field>
            }
            .into_any()
        }
        Ctl::Prompt(builtin_at) => {
            let builtin = move || {
                page.data.with(|d| {
                    d.as_ref()
                        .and_then(|v| v.pointer(builtin_at))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_default()
                })
            };
            // What Save would store is the built-in default when the box holds
            // its text: that one keeps following it as it improves.
            let is_builtin = move || form.text(k).trim() == builtin().trim();
            // Only the Chat's prompt has placeholders; the voice prompt goes
            // to the model as it is.
            let placeholders = k == "chat_system_prompt";
            view! {
                <Field
                    label=d.label
                    unit=Signal::derive(move || {
                        if is_builtin() {
                            "built-in"
                        } else if form.text(k).trim().is_empty() {
                            "none"
                        } else {
                            "your own"
                        }
                    })
                    hint=d.hint
                    dirty=dirty
                    id=id
                    hidden=hidden
                >
                    <textarea
                        class="input ta set-ta set-prompt"
                        rows=if placeholders { "18" } else { "6" }
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    ></textarea>
                    <div class="set-prompt-foot">
                        {if placeholders {
                            view! {
                                <span class="field-hint">
                                    <code>"{{model}}"</code>
                                    " becomes the thread's model alias and "
                                    <code>"{{date}}"</code>
                                    " today's date, each time a message is sent."
                                </span>
                            }
                                .into_any()
                        } else {
                            view! {
                                <span class="field-hint">
                                    "Reset follows the built-in text as releases improve it; an \
                                     empty box sends none."
                                </span>
                            }
                                .into_any()
                        }}
                        <button
                            class="btn ghost sm"
                            disabled=is_builtin
                            title="Put the built-in prompt back in the box; Save to keep it"
                            on:click=move |_| form.set_text(k, builtin())
                        >
                            "Reset to built-in"
                        </button>
                    </div>
                </Field>
            }
            .into_any()
        }
        Ctl::Map => {
            let rows = move || (form.text(k).lines().count() + 1).max(2).to_string();
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <textarea
                        class="input mono ta set-ta"
                        rows=rows
                        spellcheck="false"
                        placeholder=d.ph
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    ></textarea>
                </Field>
            }
            .into_any()
        }
        Ctl::Words => view! {
            <WordsField d=d page=page dirty=dirty error=error id=id hidden=hidden/>
        }
        .into_any(),
        Ctl::Voice(keys) => view! {
            <VoiceField d=d page=page dirty=dirty error=error id=id hidden=hidden tts_keys=keys/>
        }
        .into_any(),
        Ctl::SpeechStyle(keys) => view! {
            <SpeechStyleField
                d=d
                page=page
                dirty=dirty
                error=error
                id=id
                hidden=hidden
                tts_keys=keys
            />
        }
        .into_any(),
        Ctl::TagHint => view! { <TagHintField d=d page=page dirty=dirty id=id hidden=hidden/> }
            .into_any(),
        Ctl::VoiceLanguage => view! {
            <chat_voice::LanguageField
                d=d
                page=page
                dirty=dirty
                error=error
                warn=warn
                id=id
                hidden=hidden
            />
        }
        .into_any(),
        Ctl::Budget => view! { <BudgetPanel d=d page=page id=id hidden=hidden/> }
            .into_any(),
        // Drawn as one table by `run_view`; never reached.
        Ctl::Vad(_) => ().into_any(),
        Ctl::Link(href) => {
            let what = move || match k {
                "api_keys" => {
                    let n = page.data.with(|d| {
                        d.as_ref()
                            .and_then(|v| v["api_keys"].as_array().map(Vec::len))
                            .unwrap_or(0)
                    });
                    format!("{n} keys · created, revoked, scoped and budgeted on")
                }
                "backends_page" => {
                    "the builds, their runs and every local image of the three engines are on"
                        .to_string()
                }
                _ => "set on the Docs playground with its Save as defaults, next to the search \
                      that measures them —"
                    .to_string(),
            };
            let target = match k {
                "api_keys" => "Usage → Keys",
                "backends_page" => "Backends",
                _ => "Docs → Playground",
            };
            view! {
                <div class="set-line" id=id hidden=move || hidden.get()>
                    <span class="set-line-label">{d.label}</span>
                    <span class="dim">{what}</span>
                    <a href=href>{target}</a>
                </div>
            }
            .into_any()
        }
        Ctl::Hold => view! {
            <div class="set-line" id=id hidden=move || hidden.get()>
                <HoldSwitch page=page/>
            </div>
        }
        .into_any(),
        Ctl::Apply(target) => {
            // `apply.router` → the chat class's own keys, `router.*`.
            let prefix = format!("{}.", k.trim_start_matches("apply."));
            let unsaved =
                Signal::derive(move || form.dirty_keys().iter().any(|d| d.starts_with(&prefix)));
            view! {
                <span class="set-act" id=id hidden=move || hidden.get()>
                    <ClassApplyButton target=target unsaved=unsaved/>
                    <span class="applies-now">"applies now"</span>
                </span>
            }
            .into_any()
        }
        Ctl::CheckNow => view! {
            <span class="set-act" id=id hidden=move || hidden.get()>
                <CheckNowButton/>
                <span class="applies-now">"applies now"</span>
            </span>
        }
        .into_any(),
        Ctl::Theme => view! { <ThemeField id=id hidden=hidden/> }.into_any(),
        Ctl::Scale => view! { <ScaleField id=id hidden=hidden/> }.into_any(),
    }
}

// ---------------------------------------------------------------------------
// The prose: every note the old cards carried, folded to its first sentence
// ---------------------------------------------------------------------------

fn explain(group: &'static str, page: Page) -> Option<AnyView> {
    let hidden = move || {
        page.searching()
            && !fields()
                .filter(|d| d.group == group)
                .any(|d| page.matches(d))
    };
    let (persist, summary, body): (&'static str, String, AnyView) = match group {
        "responses" => (
            "settings.explain.responses",
            "Backs previous_response_id and MCP tool-approval round trips.".into(),
            view! {
                "Eviction is chain-aware: the whole conversation goes at once, timed from its "
                "most recent response — see "
                <a href="/traffic/conversations">"Traffic → Conversations"</a>
                " for the stored list and a manual evict/clear."
            }
            .into_any(),
        ),
        "money" => (
            "settings.explain.money",
            "The global budget is the ceiling on this gateway rather than on a credential.".into(),
            view! {
                "So it applies even with " <b>"require gateway API keys"</b>
                " off. Crossing it answers " <code>"403 key_budget"</code>
                " — not a 429, because a monthly budget will not clear inside any retry window. "
                "Catalog prices arrive in USD. Changing the currency relabels every amount on the "
                "Usage page and converts nothing — type EUR only if the numbers in your price "
                "sheets are euros."
            }
            .into_any(),
        ),
        "compare" => (
            "settings.explain.compare",
            "The alias whose price answers what your locally-served tokens would have cost in the cloud.".into(),
            view! {
                "Empty leaves the Local vs cloud panel on " <a href="/usage">"Usage"</a>
                " saying no reference is configured, rather than quietly picking one."
            }
            .into_any(),
        ),
        "admission" => (
            "settings.explain.gpu",
            "lmgw is the only ingress for all three containers, so it decides whether a model will fit before the request goes out.".into(),
            view! {
                "It evicts the least recently used idle model when it has to, never one that is "
                "still generating. Both routers run with --models-max 0 so their own count-based "
                "eviction stays out of it. The live ledger and the queue are on "
                <a href="/traffic">"Traffic"</a> ". "
                "The estimate is weights + KV cache read from the GGUF — a lower bound, because "
                "compute buffers and the driver's own context have no metadata to derive them "
                "from. Headroom is that remainder. Telemetry comes from NVML on NVIDIA and from "
                "amdgpu's sysfs counters on AMD; where neither answers there is nothing to "
                "measure, so set a budget to plan against estimates alone, or leave it at 0 and "
                "admission stays inactive, which the ledger says out loud."
            }
            .into_any(),
        ),
        "hold" => (
            "settings.explain.hold",
            "A manual switch that takes lmgw off the GPU without quitting it — for gaming on the same card.".into(),
            view! {
                "Only chat-class local models inherit the global fallback; embedding, rerank and "
                "audio models fall back only through their own model's \"Hold fallback\" setting, "
                "and unattended batch jobs (quickdoc ingest, golden-query generation) never fall "
                "back — they are refused and simply re-run after release. A local model with no "
                "usable fallback answers with a 503, error code " <code>"gpu_hold"</code> "."
            }
            .into_any(),
        ),
        "containers" => (
            "settings.explain.containers",
            "Every model runs in its own container, named and ported automatically.".into(),
            view! {
                "A class's settings below are the defaults each of its models inherits; they "
                "reach a running model only when it restarts, or at once with Apply to running "
                "containers. A request timeout is how long one call may take once the model is "
                "up — not the start, which has its own limit under Runtime — and 0 means no "
                "ceiling at all. "
                "Every per-model container is named " <code>"<prefix>-<class>-<slug>-<hash6>"</code>
                " — a dev instance pointed at the same podman socket as prod needs its own "
                "prefix (e.g. " <code>"lmgw-dev"</code>
                "), or the two collide on container names and one instance's start/stop steps on "
                "the other's containers. It is also what scopes agent containers: boot "
                "reconciliation removes every container labelled " <code>"lmgw.kind=agent"</code>
                " with this prefix whose run is not a live job of " <i>"this"</i>
                " instance — so a second lmgw started with the same prefix will kill the first "
                "one's running agents, and their run directories under "
                <code>"$XDG_RUNTIME_DIR/lmgw/<prefix>/"</code> " with them."
            }
            .into_any(),
        ),
        "audio" => (
            "settings.explain.audio",
            "Busy timeout, idle unload, min free memory and max request body go into every model's server.json.".into(),
            view! {
                "Busy timeout bounds the wait for a model that is already running (a second "
                "request fails with 503 rather than parking forever behind a wedged GPU call) — "
                "raise it above the slowest generation this class does, or set a per-model value. "
                "Idle unload frees the model's VRAM without stopping its container; the next "
                "request reloads it. Min free memory refuses a load that would not leave that "
                "much free. The voice library is a directory of <name>.wav clips plus a "
                "prompt_text file, as the container sees it: /models/voices is the Audio lab's "
                "own library."
            }
            .into_any(),
        ),
        "image" => (
            "settings.explain.image",
            "Saving a models dir creates loras/ and upscalers/ inside it.".into(),
            view! {
                "sd-server's capabilities route throws when its LoRA and upscaler flags point at "
                "nothing, and lmgw renders both on every start. sd-server has no config file, so "
                "everything per-process is a flag in some model's args."
            }
            .into_any(),
        ),
        "forge" => (
            "settings.explain.forge",
            "A token goes only to its own host: in forge API calls, and as a git header passed through the environment, never argv or a log.".into(),
            view! {
                "The Backends page uses it to list pull requests for the extras picker and to "
                "check builds for new commits. Without one, GitHub answers 60 API requests an "
                "hour per address; with one, 5,000. A token that can read the repositories you "
                "build is enough. Other forges (GitLab, Gitea, Forgejo) are named by host alone, "
                "with a port when it is not 443: " <code>"git.example:8443"</code> ". "
                "The self-admin tools can neither read nor write these."
            }
            .into_any(),
        ),
        "agent-containers" => (
            "settings.explain.agents",
            "Agent UIs live at http://<id>.<suffix>:<port>/.".into(),
            view! {
                "Change the suffix only for a gateway reached from other machines, to a zone with "
                "a wildcard record that is not a sibling of the dashboard's own name — a page "
                "under the suffix could set a cookie on the shared parent domain and overwrite "
                "your session. Changing it stops every running app container. "
                "An agent step declared as a " <code>"script"</code>
                " runs in the agent script image — lmgw mounts its own shim and the manifest's "
                "module read-only and starts " <code>"node /lmgw/shim.mjs"</code>
                " under the same limits, cancel and run log every container agent gets. It is "
                "pulled on first use if it is not on the box; pin a digest here for an "
                "air-gapped or reproducible install."
            }
            .into_any(),
        ),
        "ingest" => (
            "settings.explain.docs",
            "Ingestion sizes each extraction window as the ingest model's real context minus the prompt minus the reply budget.".into(),
            view! {
                "The reply budget is the one number in that sum that is a choice, so it lives "
                "here. Corpora, the request queue and the search playground are on "
                <a href="/docs">"Docs"</a> "."
            }
            .into_any(),
        ),
        "window" if crate::ui_scale::supported() => (
            "settings.explain.appearance",
            "Zooms the whole window — text, controls, charts and spacing together.".into(),
            view! {
                "Ctrl + and Ctrl - step it, Ctrl 0 returns to 100%, Ctrl+wheel does the same. It "
                "is remembered per window, not a gateway setting — the theme too."
            }
            .into_any(),
        ),
        "window" => (
            "settings.explain.appearance",
            "This is the app window's own zoom, so it only applies inside the lmgw shell.".into(),
            view! {
                "In a browser tab use the browser's zoom (Ctrl +/-), which does exactly the same "
                "thing and is remembered per site. The theme is remembered per window too, not as "
                "a gateway setting."
            }
            .into_any(),
        ),
        other => realtime::explain(other)?,
    };
    // Why the stored suffix shadows the gateway, when it does: said where the
    // setting is read, not only where it was typed (origins §4.1).
    let warning = (group == "agent-containers").then_some(move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| v["agent_origin_suffix_warning"].as_str())
                .map(|w| view! { <div class="notice warn">{w.to_string()}</div> })
        })
    });
    Some(
        view! {
            {warning}
            <div class="set-explain" hidden=hidden>
                <Explain summary=summary persist=persist>
                    {body}
                </Explain>
            </div>
        }
        .into_any(),
    )
}

// ---------------------------------------------------------------------------
// The forge-token list
// ---------------------------------------------------------------------------

/// One row per host (GitHub always, then every stored host, then any added
/// here): a masked box that sets a new token, a clear tick on a stored one,
/// Remove on an added one. A stored token is never shown, only that it is
/// there. The whole list is one `Raw` draft value ([`ForgeRow`]s), turned
/// into `forge_tokens` / `clear_forge_tokens` by [`finish`].
#[component]
fn ForgeTokensField(
    page: Page,
    id: String,
    hidden: Signal<bool>,
    error: Signal<Option<String>>,
    warn: Signal<Option<String>>,
) -> impl IntoView {
    const K: &str = "forge_tokens";
    let form = page.form;
    let rows = Memo::new(move |_| forge_rows(&form.value(K)));
    let edit = move |f: &dyn Fn(&mut Vec<ForgeRow>)| {
        let mut r = form
            .draft
            .with_untracked(|d| d.get(K).map(forge_rows))
            .unwrap_or_default();
        f(&mut r);
        form.set_value(K, json!(r));
    };
    let next = StoredValue::new(0u32);
    let add = move |_| {
        // Past any `new-<n>` a Discard left behind in the numbering.
        let n = next.get_value() + 1;
        next.set_value(n);
        edit(&|r| {
            r.push(ForgeRow {
                key: format!("new-{n}"),
                ..ForgeRow::default()
            })
        });
    };
    let row_view = move |key: String| {
        // `Copy` handles on the row, for the closures below.
        let key_s = StoredValue::new(key.clone());
        let get =
            move || key_s.with_value(|k| rows.with(|rs| rs.iter().find(|r| &r.key == k).cloned()));
        let set = move |f: &dyn Fn(&mut ForgeRow)| {
            key_s.with_value(|k| {
                edit(&|rs| {
                    if let Some(r) = rs.iter_mut().find(|r| &r.key == k) {
                        f(r);
                    }
                })
            })
        };
        let added = key.starts_with("new-");
        let host_cell = if added {
            view! {
                <input
                    class="input mono forge-host"
                    placeholder="git.example.com"
                    aria-label="Forge host"
                    spellcheck="false"
                    autocomplete="off"
                    prop:value=move || get().map(|r| r.host).unwrap_or_default()
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        set(&|r| r.host = v.clone());
                    }
                />
            }
            .into_any()
        } else {
            view! { <span class="mono forge-host forge-host-fixed">{key.clone()}</span> }.into_any()
        };
        let stored = move || get().is_some_and(|r| r.stored);
        let clearing = move || get().is_some_and(|r| r.clear);
        let token = move || get().map(|r| r.token).unwrap_or_default();
        let remove = move |_| key_s.with_value(|k| edit(&|rs| rs.retain(|r| &r.key != k)));
        let label = format!("Token for {key}");
        view! {
            <div class="forge-row">
                {host_cell}
                <input
                    class="input mono forge-token"
                    type="password"
                    autocomplete="off"
                    aria-label=label
                    placeholder=move || {
                        if stored() { "type to replace" } else { "token" }
                    }
                    disabled=clearing
                    prop:value=token
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        set(&|r| r.token = v.clone());
                    }
                />
                {move || {
                    if added {
                        view! {
                            <button type="button" class="btn ghost sm" on:click=remove>
                                "Remove"
                            </button>
                        }
                            .into_any()
                    } else if stored() {
                        view! {
                            <span class="chip ok forge-state">"stored"</span>
                            <label class="check" title="Erased on Save">
                                <input
                                    type="checkbox"
                                    prop:checked=clearing
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        set(&|r| {
                                            r.clear = on;
                                            if on {
                                                r.token.clear();
                                            }
                                        });
                                    }
                                />
                                "clear"
                            </label>
                        }
                            .into_any()
                    } else {
                        view! { <span class="chip off forge-state">"not set"</span> }.into_any()
                    }
                }}
            </div>
        }
    };
    view! {
        <Field
            label="Forge tokens"
            unit="write-only"
            hint="Per host; an empty box keeps what is stored. Saved with the rest of the page."
            dirty=form.dirty_signal(K)
            error=error
            warn=warn
            id=id
            hidden=hidden
        >
            <div class="forge-rows">
                <For each=move || rows.get().into_iter().map(|r| r.key) key=|k| k.clone() let:key>
                    {row_view(key)}
                </For>
            </div>
            <button type="button" class="btn sm forge-add" on:click=add>
                "Add host"
            </button>
        </Field>
    }
}

// ---------------------------------------------------------------------------
// Controls that apply at once
// ---------------------------------------------------------------------------

/// The manual GPU hold (gpu-hold design §3.1, §6): a state line and one
/// button. `active` is never part of the draft — only `ops::hold_set` may
/// flip it, since engaging it stops containers (§5) — the same op the
/// titlebar pill and the tray call.
#[component]
fn HoldSwitch(page: Page) -> impl IntoView {
    let toasts = use_toasts();
    let live = use_live();
    // Seeded from the settings snapshot, then the live bus wins — a toggle
    // from the tray, MCP or another tab shows up here unprompted.
    let active = Memo::new(move |_| {
        live.vram
            .get()
            .map(|v| v.hold_active)
            .unwrap_or_else(|| page.server_flag("/hold/active"))
    });
    let busy = RwSignal::new(false);
    let toggle = move |_| crate::model_ops::hold_set(toasts, busy, !active.get_untracked());
    view! {
        <span class="set-line-label">"GPU hold"</span>
        <span class="chip" class:warn=move || active.get() class:off=move || !active.get()>
            <span class="dot"></span>
            {move || {
                if active.get() {
                    "engaged — local models are paused"
                } else {
                    "off — local models start on demand"
                }
            }}
        </span>
        <button
            class="btn sm"
            disabled=move || busy.get()
            title=move || {
                if active.get() {
                    "Let local models start again"
                } else {
                    "Stop every local model and refuse or reroute local requests until released"
                }
            }
            on:click=toggle
        >
            {move || match (busy.get(), active.get()) {
                (true, _) => "Working…",
                (false, true) => "Release hold",
                (false, false) => "Engage hold",
            }}
        </button>
        <span class="applies-now">"applies now"</span>
    }
}

/// Recreate every *running* model of a class with its freshly rendered argv
/// (`ops::container` group `apply`) — the follow-up step after a class
/// settings save when a running model was inheriting the value that just
/// changed. A no-op (and says so) when nothing of the class is up.
///
/// It recreates from the *saved* definition, so while this class's section
/// has unsaved edits it waits for Save: applying then would restart every
/// model of the class on the old values (review code:S5).
#[component]
fn ClassApplyButton(target: &'static str, #[prop(into)] unsaved: Signal<bool>) -> impl IntoView {
    let ops = use_ops();
    let toasts = use_toasts();
    let busy = move || ops.busy(&class_key(target));
    let run = move |_| {
        let key = class_key(target);
        if !ops.start(&key) {
            return;
        }
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/container",
                &json!({ "target": target, "action": "apply" }),
            )
            .await;
            ops.finish(&key);
            match res {
                Ok(v) => toasts.ok(v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("applied")
                    .to_string()),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <button
            class="btn sm"
            disabled=move || busy() || unsaved.get()
            title=move || {
                if unsaved.get() {
                    "Save first: this recreates the running models from the saved settings, not the edits above"
                } else {
                    "Recreate every running model of this class with the saved settings above"
                }
            }
            on:click=run
        >
            {move || if busy() { "Applying…" } else { "Apply to running containers" }}
        </button>
    }
}

#[component]
fn CheckNowButton() -> impl IntoView {
    let toasts = use_toasts();
    let checking = RwSignal::new(false);
    let check = move |_| {
        if checking.get_untracked() {
            return;
        }
        checking.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/update_check", &json!({})).await;
            checking.set(false);
            match res {
                Ok(v) => toasts.ok(v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("checked")
                    .to_string()),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <button class="btn sm" disabled=move || checking.get() on:click=check>
            {move || if checking.get() { "Checking…" } else { "Check for app update now" }}
        </button>
    }
}

/// Theme: this window's own, stored where index.html reads it before paint.
#[component]
fn ThemeField(id: String, hidden: Signal<bool>) -> impl IntoView {
    let theme = RwSignal::new(
        window()
            .local_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item("lmgw-theme").ok().flatten())
            .unwrap_or_else(|| "dark".into()),
    );
    Effect::new(move |_| {
        let t = theme.get();
        let _ = document()
            .document_element()
            .map(|el| el.set_attribute("data-theme", &t));
        if let Ok(Some(storage)) = window().local_storage() {
            let _ = storage.set_item("lmgw-theme", &t);
        }
    });
    let opts = Signal::derive(|| {
        [("dark", "dark"), ("light", "light")]
            .into_iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });
    view! {
        <Field label="Theme" unit="applies now" id=id hidden=hidden>
            <Select value=theme options=opts/>
        </Field>
    }
}

/// Interface scale. The value lives in the shared signal (the hotkeys write
/// it too), the select only mirrors it — hence the two effects instead of a
/// plain binding: one carries a Ctrl+= into the dropdown, the other carries
/// a pick back out. Both are guarded on "actually different", so they do not
/// chase each other.
#[component]
fn ScaleField(id: String, hidden: Signal<bool>) -> impl IntoView {
    let scale = crate::ui_scale::use_ui_scale();
    let in_shell = crate::ui_scale::supported();
    let sel = RwSignal::new(scale.pct.get_untracked().to_string());
    Effect::new(move |_| {
        let pct = scale.pct.get().to_string();
        if sel.get_untracked() != pct {
            sel.set(pct);
        }
    });
    Effect::new(move |_| {
        if let Ok(pct) = sel.get().parse::<i32>() {
            if pct != scale.pct.get_untracked() {
                scale.set(pct);
            }
        }
    });
    let opts = Signal::derive(|| {
        crate::ui_scale::steps()
            .iter()
            .map(|p| (p.to_string(), format!("{p}%")))
            .collect::<Vec<_>>()
    });
    view! {
        <Field label="Interface scale" unit="applies now" id=id hidden=hidden>
            {if in_shell {
                view! { <Select value=sel options=opts/> }.into_any()
            } else {
                view! { <div class="dim set-static">"browser zoom · Ctrl +/-"</div> }.into_any()
            }}
        </Field>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "bind_addr": "127.0.0.1:8899",
            "max_body_mb": 64,
            "global_budget_micro": 12_500_000,
            "hold": {"active": false, "fallback_alias": null},
            "router": {"extra_run_args": ["--device", "nvidia.com/gpu=all"], "image": "img"},
            "has_hf_token": true,
        })
    }

    #[test]
    fn every_key_is_named_once_and_every_group_exists() {
        let mut seen = std::collections::BTreeSet::new();
        for d in fields() {
            assert!(seen.insert(d.key), "{} twice", d.key);
            assert!(
                GROUPS.iter().any(|g| g.id == d.group && g.cat == d.cat),
                "{}",
                d.key
            );
            assert!(CAT_SLUGS.contains(&d.cat), "{}", d.key);
        }
        assert_eq!(CATS.len(), CAT_SLUGS.len());
        for g in GROUPS {
            assert!(fields().any(|d| d.group == g.id), "empty group {}", g.id);
        }
    }

    #[test]
    fn the_baseline_is_what_the_boxes_show() {
        let b = baseline(&sample());
        assert_eq!(b["global_budget_micro"], json!(12.5));
        assert_eq!(b["hold.fallback_alias"], json!(""));
        assert_eq!(
            b["router.extra_run_args"],
            json!("--device\nnvidia.com/gpu=all")
        );
        assert_eq!(b["hf_token"], json!(""));
        assert_eq!(b["clear_hf_token"], json!(false));
        assert!(
            !b.contains_key("hold.active"),
            "the hold is never in the draft"
        );
        assert!(!b.contains_key("api_keys"));
    }

    #[test]
    fn a_finished_patch_has_lists_and_micro_units() {
        let p = finish(json!({
            "global_budget_micro": 10.5,
            "router": {"extra_run_args": " --a \n\n--b\n"},
            "retention_days": 30,
        }));
        assert_eq!(
            p,
            json!({
                "global_budget_micro": 10_500_000,
                "router": {"extra_run_args": ["--a", "--b"]},
                "retention_days": 30,
            })
        );
        // empty money clears the budget
        assert_eq!(
            finish(json!({"global_budget_micro": null})),
            json!({"global_budget_micro": 0})
        );
    }

    #[test]
    fn anchors_and_deep_links() {
        assert_eq!(anchor("vram.headroom_mb"), "f-vram-headroom-mb");
        assert_eq!(href("hold.active"), "/settings/gpu#f-hold-active");
        assert_eq!(
            href("audio.voice_dir"),
            "/settings/runtimes#f-audio-voice-dir"
        );
    }

    #[test]
    fn the_keys_link_is_found_as_a_credential() {
        let d = fields().find(|d| d.key == "api_keys").unwrap();
        let hay = haystack(d);
        for q in ["api key", "token", "credential"] {
            assert!(q.split(' ').all(|w| hay.contains(w)), "{q}");
        }
    }

    #[test]
    fn out_of_range_numbers_are_caught() {
        let load = fields()
            .find(|d| d.key == "vram.load_timeout_seconds")
            .unwrap();
        assert!(range_error(load, "0").is_some());
        assert!(range_error(load, "5").is_none());
        let money = fields().find(|d| d.key == "global_budget_micro").unwrap();
        assert!(range_error(money, "-1").is_some());
        assert!(range_error(money, "").is_none());
        // The update-check interval: 0 (off) to a year, same as the server
        // (`lmgw_core::backends::updates::validate_check_hours`).
        let hours = fields()
            .find(|d| d.key == "build_update_check_hours")
            .unwrap();
        assert!(matches!(hours.ctl, Ctl::IntRange(0, 8760)));
        assert!(range_error(hours, "0").is_none());
        assert!(range_error(hours, "8760").is_none());
        assert!(range_error(hours, "-1").is_some());
        let over = range_error(hours, "8761").unwrap();
        assert!(over.contains("0") && over.contains("8760"), "{over}");
    }

    #[test]
    fn the_four_class_images_are_pickers_for_their_class() {
        let class = |k: &str| match fields().find(|d| d.key == k).unwrap().ctl {
            Ctl::Image(c) => Some(c),
            _ => None,
        };
        assert_eq!(class("router.image"), Some(ImageClass::Chat));
        assert_eq!(class("aux_router.image"), Some(ImageClass::Aux));
        assert_eq!(class("audio.image"), Some(ImageClass::Audio));
        assert_eq!(class("image.image"), Some(ImageClass::Image));
        // still a plain string in the draft and the patch
        let b = baseline(&sample());
        assert_eq!(b["router.image"], json!("img"));
        assert_eq!(b["image.image"], json!(""));
    }

    #[test]
    fn forge_tokens_list_github_first_and_never_the_token() {
        let rows = forge_baseline(&json!({"git.example.com": "<set>", "codeberg.org": "<set>"}));
        let hosts: Vec<(&str, bool)> = rows.iter().map(|r| (r.host.as_str(), r.stored)).collect();
        assert_eq!(
            hosts,
            [
                ("github.com", false),
                ("codeberg.org", true),
                ("git.example.com", true)
            ]
        );
        assert!(rows.iter().all(|r| r.token.is_empty() && !r.added()));
        let b = baseline(&json!({"forge_tokens": {"github.com": "<set>"}}));
        assert!(forge_rows(&b["forge_tokens"])[0].stored);
        assert_eq!(forge_rows(&b["forge_tokens"]).len(), 1);
    }

    #[test]
    fn a_forge_token_save_sets_by_host_and_clears_stored_ones() {
        let mut rows = forge_baseline(&json!({"github.com": "<set>", "git.example.com": "<set>"}));
        rows[0].token = " ghp_new ".into();
        rows[1].clear = true;
        rows.push(ForgeRow {
            key: "new-1".into(),
            host: "Codeberg.org".into(),
            token: "cb".into(),
            ..ForgeRow::default()
        });
        let p = finish(json!({"forge_tokens": rows, "retention_days": 3}));
        assert_eq!(
            p,
            json!({
                "forge_tokens": {"github.com": "ghp_new", "codeberg.org": "cb"},
                "clear_forge_tokens": ["git.example.com"],
                "retention_days": 3,
            })
        );
        // Nothing typed and nothing cleared: nothing sent.
        let untouched = forge_baseline(&json!({"github.com": "<set>"}));
        assert_eq!(finish(json!({"forge_tokens": untouched})), json!({}));
        // Read back failed: what the server holds now.
        let after = forge_after(&rows);
        let hosts: Vec<(&str, bool)> = after.iter().map(|r| (r.host.as_str(), r.stored)).collect();
        assert_eq!(hosts, [("github.com", true), ("codeberg.org", true)]);
    }

    #[test]
    fn an_added_forge_host_has_to_be_a_host_with_a_token() {
        let with = |host: &str, token: &str| {
            let mut rows = forge_baseline(&json!({}));
            rows.push(ForgeRow {
                key: "new-1".into(),
                host: host.into(),
                token: token.into(),
                ..ForgeRow::default()
            });
            forge_error(&rows)
        };
        assert_eq!(with("git.example:8443", "t"), None);
        assert!(with("", "").unwrap().contains("remove the row"));
        assert!(with("https://git.example", "t")
            .unwrap()
            .contains("not a host name"));
        assert!(with("git.example", "").unwrap().contains("empty"));
        assert!(with("github.com", "t").unwrap().contains("twice"));
        assert!(with("git.example", "a b").unwrap().contains("whitespace"));
        assert!(forge_host_ok("GIT.Example.com"));
        assert!(!forge_host_ok("git.example/path"));
        assert!(!forge_host_ok("git.example:0"));
    }

    #[test]
    fn the_builds_dir_is_absolute_or_empty() {
        let d = fields().find(|d| d.key == "builds_dir").unwrap();
        assert!(range_error(d, "").is_none());
        assert!(range_error(d, "/srv/builds").is_none());
        assert!(range_error(d, "builds").is_some());
        assert!(range_error(d, "/").is_some());
        assert_eq!(href("forge_tokens"), "/settings/backends#f-forge-tokens");
    }

    #[test]
    fn what_the_server_always_refuses_is_refused_here_first() {
        let def = |k: &str| fields().find(|d| d.key == k).unwrap();
        // `settings_set_full` refuses a 0 for these two and an empty value
        // for the three names: with several keys dirty the whole save would
        // fail with a message that belongs to none of the fields in view.
        for k in ["docs_ingest_reply_tokens", "docs_embed_batch"] {
            assert!(range_error(def(k), "0").is_some(), "{k}");
            assert!(range_error(def(k), "1").is_none(), "{k}");
        }
        for k in ["bind_addr", "container_prefix", "agent_origin_suffix"] {
            assert!(range_error(def(k), "  ").is_some(), "{k}");
            assert!(range_error(def(k), "x").is_none(), "{k}");
        }
    }

    /// A form whose baseline is `flat` — what the server holds, as the
    /// page's boxes show it.
    fn stored(flat: Value) -> FormState {
        FormState::new(flat.as_object().unwrap().clone())
    }

    /// The keys Save waits for with the form's changes.
    fn waits_for(form: FormState) -> Vec<&'static str> {
        blocking(form, &judged_keys(&form.dirty_keys()))
            .into_keys()
            .collect()
    }

    /// Settings a hand edit broke: the low row's floor above its threshold,
    /// both WebSocket limits at 0, the bind address emptied.
    fn hand_edited() -> Value {
        json!({
            "bind_addr": "",
            "realtime.barge_in_min_ms": 200,
            "realtime.ping_interval_s": 20,
            "realtime.max_message_mb": 0,
            "realtime.max_frame_mb": 0,
            "realtime.semantic_floor_window_ms": 300,
            "realtime.semantic_vad.high.threshold": 0.5,
            "realtime.semantic_vad.high.floor": 0.25,
            "realtime.semantic_vad.high.max_wait_ms": 2000,
            "realtime.semantic_vad.low.threshold": 0.5,
            "realtime.semantic_vad.low.floor": 0.9,
            "realtime.semantic_vad.low.max_wait_ms": 4000,
        })
    }

    #[test]
    fn a_broken_stored_value_is_shown_but_does_not_block_an_unrelated_save() {
        // WP8 review: the page judged every field, the server only what a
        // save changes — so one broken row blocked every other change.
        let owner = Owner::new();
        owner.with(|| {
            let def = |k: &str| fields().find(|d| d.key == k).unwrap();
            let form = stored(hand_edited());
            assert!(
                waits_for(form).is_empty(),
                "nothing changed, nothing judged"
            );
            form.set_text("realtime.barge_in_min_ms", "250");
            form.set_text("realtime.ping_interval_s", "30");
            assert_eq!(form.dirty_count(), 2);
            assert!(waits_for(form).is_empty(), "{:?}", waits_for(form));
            assert_eq!(form.invalid_count(), 0);
            // Each broken value is said at its field, as a warning.
            for (k, says) in [
                ("realtime.semantic_vad.low.floor", "above the threshold 0.5"),
                ("realtime.max_frame_mb", "cannot both be 0"),
                ("bind_addr", "cannot be empty"),
            ] {
                let (error, warn) = messages(form, def(k), false);
                assert_eq!(error, None, "{k}");
                let warn = warn.unwrap_or_else(|| panic!("{k}: no warning"));
                assert!(
                    warn.starts_with("as stored: ") && warn.contains(says),
                    "{k}: {warn}"
                );
                assert!(warn.ends_with("other changes still save"), "{k}: {warn}");
            }
            // A sound value says nothing either way.
            let row = def("realtime.semantic_vad.high.floor");
            assert_eq!(messages(form, row, false), (None, None));
        });
    }

    #[test]
    fn editing_a_broken_value_or_what_it_is_judged_with_is_still_refused() {
        let owner = Owner::new();
        owner.with(|| {
            let low_floor = ["realtime.semantic_vad.low.floor"];
            // The broken row itself: its threshold moves, still below its
            // floor — the error is the floor's, where it can be fixed.
            let form = stored(hand_edited());
            form.set_text("realtime.semantic_vad.low.threshold", "0.6");
            assert_eq!(waits_for(form), low_floor);
            let floor = fields().find(|d| d.key == low_floor[0]).unwrap();
            let (error, warn) = messages(form, floor, true);
            assert!(error.unwrap().contains("above the threshold 0.6"));
            assert_eq!(warn, None);
            // Another cell of the table, or the floor window: the server
            // judges the whole table then (`SemanticVadTable::problems`).
            for (k, v) in [
                ("realtime.semantic_vad.high.max_wait_ms", "2500"),
                ("realtime.semantic_floor_window_ms", "250"),
            ] {
                let form = stored(hand_edited());
                form.set_text(k, v);
                assert_eq!(waits_for(form), low_floor, "{k}");
            }
            // Fixed, it saves.
            let form = stored(hand_edited());
            form.set_text("realtime.semantic_vad.low.floor", "0.4");
            assert!(waits_for(form).is_empty(), "{:?}", waits_for(form));
            // The two limits are judged together once either moves.
            let form = stored(hand_edited());
            form.set_text("realtime.max_message_mb", "8");
            assert!(waits_for(form).is_empty());
            let form = stored(json!({"realtime.max_message_mb": 16, "realtime.max_frame_mb": 0}));
            form.set_text("realtime.max_message_mb", "0");
            assert_eq!(waits_for(form), ["realtime.max_frame_mb"]);
            // A field edited to a value the server refuses is refused.
            let form = stored(json!({"bind_addr": "127.0.0.1:8001"}));
            form.set_text("bind_addr", " ");
            assert_eq!(waits_for(form), ["bind_addr"]);
        });
    }

    #[test]
    fn the_smart_turn_table_and_the_limits_are_judged_as_wholes() {
        let judged = judged_keys(&["realtime.semantic_vad.medium.threshold".to_string()]);
        assert!(judged.contains("realtime.semantic_vad.low.floor"));
        assert!(judged.contains("realtime.semantic_floor_window_ms"));
        assert!(!judged.contains("realtime.semantic_vad_engine"));
        assert!(!judged.contains("realtime.max_frame_mb"));
        let judged = judged_keys(&["realtime.max_frame_mb".to_string()]);
        assert_eq!(
            judged.into_iter().collect::<Vec<_>>(),
            ["realtime.max_frame_mb", "realtime.max_message_mb"]
        );
        let judged = judged_keys(&["bind_addr".to_string()]);
        assert_eq!(judged.into_iter().collect::<Vec<_>>(), ["bind_addr"]);
    }
}
