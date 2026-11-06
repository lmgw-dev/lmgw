//! Settings — the table: categories, groups, the control and row types, and the class-shaped rows.

use super::*;

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// The categories: slug (the URL segment), rail label, one line on what is
/// in it.
pub(super) const CATS: &[(&str, &str, &str)] = &[
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
    (
        "gpu",
        labels::GPU,
        "this computer's graphics card, admission control and the manual hold",
    ),
    (
        "runtimes",
        labels::RUNTIMES,
        "the container defaults every model of a class inherits",
    ),
    (
        "backends",
        labels::ENGINES,
        "container builds: forge tokens, the builds directory, the update check",
    ),
    (
        "chat",
        labels::CHAT,
        "the system prompt new conversations start with, how attachments are read, the knowledge budget, the voice, the change feed client apps follow",
    ),
    (
        "realtime",
        labels::REALTIME,
        "spoken conversations on /v1/realtime and WebRTC calls: the voice cascade and its GPU memory, turns, barge-in, output, the UDP listener",
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
pub(super) const CAT_SLUGS: &[&str] = &[
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
pub(super) struct Group {
    pub(super) id: &'static str,
    pub(super) cat: &'static str,
    pub(super) title: &'static str,
    pub(super) sub: &'static str,
    pub(super) fold: &'static str,
    /// Across every column of its category rather than in one of them: a
    /// card whose sentences want the width (This computer).
    pub(super) wide: bool,
}

pub(super) const fn g(cat: &'static str, id: &'static str, title: &'static str) -> Group {
    Group {
        id,
        cat,
        title,
        sub: "",
        fold: "",
        wide: false,
    }
}

impl Group {
    pub(super) const fn wide(mut self) -> Self {
        self.wide = true;
        self
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
        wide: false,
    }
}

pub(super) const GROUPS: &[Group] = &[
    g("network", "listen", "Gateway"),
    g("network", "access", "Access"),
    g("network", "devices", "Devices"),
    g("retention", "logs", "Request log"),
    g("retention", "jobs", "Job history"),
    g("retention", "chat", "Chat threads"),
    g("retention", "rollups", "Usage rollups"),
    g("retention", "responses", "Responses storage"),
    g("usage-cost", "money", "Currency & budget"),
    g("usage-cost", "compare", "Local vs cloud"),
    g("gpu", "machine", "This computer").wide(),
    g("gpu", "admission", labels::ADMISSION),
    g("gpu", "hold", "Hold"),
    g("runtimes", "containers", "Containers"),
    class_group(
        "router",
        labels::CHAT_CLASS,
        "llama-server",
        "settings.runtimes.chat",
    ),
    class_group(
        "aux_router",
        labels::AUX_CLASS,
        "llama-server · embeddings, rerank + decisions",
        "settings.runtimes.aux",
    ),
    class_group(
        "audio",
        labels::AUDIO_CLASS,
        "audio.cpp",
        "settings.runtimes.audio",
    ),
    class_group(
        "image",
        labels::IMAGE_CLASS,
        "stable-diffusion.cpp",
        "settings.runtimes.image",
    ),
    g("backends", "forge", labels::FORGE_TOKENS),
    g("backends", "builds", labels::BUILDS),
    g("chat", "prompt", "Default system prompt"),
    g("chat", "attachments", "Attachments"),
    g("chat", "knowledge", "Knowledge bases"),
    g("chat", "chat-voice", labels::CHAT_VOICE),
    g("chat", "chat-media", "Picture and sound tools"),
    g("chat", "feed", "Change feed"),
    g("realtime", "rt-cascade", labels::VOICE_CASCADE),
    g("realtime", "rt-budget", "GPU memory of the cascade"),
    g("realtime", "rt-prompt", "Voice instructions"),
    g("realtime", "rt-names", "Client names"),
    g("realtime", "rt-vad", "Turn detection"),
    g("realtime", "rt-smart", "Smart Turn (semantic_vad)"),
    g("realtime", "rt-barge", "Barge-in"),
    g("realtime", "rt-output", "Output"),
    g("realtime", "rt-limits", "Connection"),
    g("realtime", "rt-webrtc", "WebRTC calls"),
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
pub(super) enum Size {
    S,
    M,
    L,
}

/// The control a setting is edited with.
#[derive(Clone, Copy)]
pub(super) enum Ctl {
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
    /// [`Ctl::IntRange`] where empty is `null`, which the row's placeholder
    /// says the meaning of (`realtime.webrtc_port`: the HTTP port's number).
    OptIntRange(i64, i64),
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
    /// The machine check's card (machine-check design §2.3), anchored at
    /// `#machine`: read, and its fixes act at once. Never in a search: the
    /// [`Ctl::MachineLink`] rows stand for it there.
    Machine,
    /// A search row that stands for the machine check's card, a link to
    /// `#machine`: the card is no field, so the words it is found by are
    /// rows of their own. Never shown outside a search.
    MachineLink,
    /// The device listener's switch, scope and port, under a confirmation
    /// ([`devices::ListenerPanel`]).
    DeviceListener,
    /// The WebRTC listener line, the firewall hint and the live calls —
    /// read, never part of the draft ([`webrtc::WebRtcPanel`]).
    WebRtc,
    Apply(&'static str),
    CheckNow,
    Theme,
    Scale,
    /// A pointer to where the thing is really managed.
    Link(&'static str),
    /// The personality profile new Chat threads start with: one of the
    /// gateway's profiles, or none (`chat_profile`).
    Profile,
    /// A media tool's default model: the tool's choices, a saved one that
    /// no longer resolves marked ([`chat_media::MediaDefaultField`]).
    MediaModel(lmgw_api_types::chat_media::MediaField),
    /// The default voice, from the voices of the drafted speech model.
    MediaVoice,
}

impl Ctl {
    /// The form's reading of it, or `None` for a control outside the draft.
    pub(super) fn form_kind(self) -> Option<Kind> {
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
            | Ctl::MediaModel(_)
            | Ctl::MediaVoice
            | Ctl::Model(..) => Kind::Text,
            Ctl::ForgeTokens => Kind::Raw,
            Ctl::Int(_) | Ctl::IntRange(..) => Kind::Int,
            Ctl::Float(..) => Kind::Float,
            Ctl::Vad(col) => realtime::vad_kind(col),
            Ctl::Money => Kind::OptFloat,
            Ctl::Profile | Ctl::OptIntRange(..) => Kind::OptInt,
            Ctl::Bool | Ctl::TagHint => Kind::Flag,
            Ctl::Hold
            | Ctl::Machine
            | Ctl::MachineLink
            | Ctl::Apply(_)
            | Ctl::CheckNow
            | Ctl::Theme
            | Ctl::Scale
            | Ctl::Link(_)
            | Ctl::Budget
            | Ctl::DeviceListener
            | Ctl::WebRtc => return None,
        })
    }
}

/// One setting. `key` is its dotted patch path (and, dashed, its anchor).
#[derive(Clone, Copy)]
pub(super) struct Def {
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) cat: &'static str,
    pub(super) group: &'static str,
    pub(super) ctl: Ctl,
    pub(super) unit: &'static str,
    pub(super) hint: &'static str,
    pub(super) ph: &'static str,
    pub(super) size: Size,
    /// More words the search finds it by.
    pub(super) terms: &'static str,
    /// Empty is refused (the server would refuse it too).
    pub(super) req: bool,
}

pub(super) const fn f(
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
    pub(super) const fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }
    pub(super) const fn hint(mut self, hint: &'static str) -> Self {
        self.hint = hint;
        self
    }
    pub(super) const fn ph(mut self, ph: &'static str) -> Self {
        self.ph = ph;
        self
    }
    pub(super) const fn s(mut self) -> Self {
        self.size = Size::S;
        self
    }
    pub(super) const fn l(mut self) -> Self {
        self.size = Size::L;
        self
    }
    pub(super) const fn terms(mut self, terms: &'static str) -> Self {
        self.terms = terms;
        self
    }
    pub(super) const fn req(mut self) -> Self {
        self.req = true;
        self
    }
}

pub(super) const PERIODS: &[(&str, &str)] = &[
    ("day", "per day"),
    ("month", "per month"),
    ("total", "total"),
];
pub(super) const SELF_ADMIN: &[(&str, &str)] =
    &[("off", "off"), ("read_only", "read only"), ("full", "full")];
pub(super) const PDF_MODES: &[(&str, &str)] = &[
    ("text", "text"),
    ("images", "page images"),
    ("ask", "ask each time"),
];
pub(super) const CATALOG_REVISIONS: &[(&str, &str)] = &[
    ("pinned", "the commit the spec pins"),
    ("latest", "latest (main)"),
];
pub(super) const BACKENDS: &[(&str, &str)] = &[
    ("cuda", "cuda"),
    ("cpu", "cpu"),
    ("vulkan", "vulkan"),
    ("metal", "metal"),
    ("hip", "hip"),
];

/// The three classes shaped like llama-server's (chat, aux, image): the same
/// five fields under their own patch prefix, and the class's apply.
macro_rules! class_fields {
    ($p:literal, $target:literal, $class:expr) => {
        [
            f(
                "runtimes",
                $p,
                concat!($p, ".image"),
                labels::IMAGE,
                Ctl::Image($class),
            )
            .l()
            .hint("per-model overrides win")
            .terms("container podman build engines backends"),
            f(
                "runtimes",
                $p,
                concat!($p, ".models_dir"),
                labels::MODELS_DIR,
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
            .hint("per call once the model is up, and each silence in a stream; 0 = none"),
            f(
                "runtimes",
                $p,
                concat!($p, ".extra_run_args"),
                labels::EXTRA_RUN_ARGS,
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

pub(super) const CHAT: [Def; 6] = class_fields!("router", "chat", ImageClass::Chat);
pub(super) const AUX: [Def; 6] = class_fields!("aux_router", "aux", ImageClass::Aux);
/// The aux class's second prefix, shown after its `public_prefix`
/// (decision-models design §3.10).
pub(super) const AUX_DECISION: [Def; 1] = [f(
    "runtimes",
    "aux_router",
    "aux_router.decision_prefix",
    "Decision prefix",
    Ctl::Mono,
)
.s()
.hint("decision models; empty = bare ids")
.terms("systemone decide")];
pub(super) const IMAGE: [Def; 6] = class_fields!("image", "image", ImageClass::Image);
