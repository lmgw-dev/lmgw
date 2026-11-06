//! Finding a model sized to the card (find-a-model design §3, §9.2): the
//! fit of one file, the card it was measured against, the file a repo's
//! list recommends, the download's time, and where a plan's input came from.
//!
//! WP2 of the design builds these; the file list's marks (WP3), the finder's
//! card (WP4) and Get it (WP5) read them. Every field is `#[serde(default)]`,
//! as the DTOs are.

use serde::{Deserialize, Serialize};

mod share;
pub use share::{display_name, share_name, share_text, share_url};

/// Which of the two measures of "fits" a [`FileFit`] was taken against
/// (design D5, D120).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FitMeasure {
    /// The whole card as admission measures it, with nothing else on it:
    /// admission unloads lmgw's other models when this one is asked for.
    #[default]
    WholeCard,
    /// The card less what other programs hold on it now and less lmgw's own
    /// models on it now.
    Beside,
}

/// A file's mark (design §3.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FitMark {
    /// Fits with the model's whole trained context (`ctx` is it).
    Fits,
    /// Fits a shorter conversation: `ctx` is the largest that fits.
    FitsShorter,
    /// Doesn't fit, not even at one token.
    DoesNotFit,
    /// The header wasn't read (or couldn't be), and the file's bytes plus
    /// the headroom fit: "fits by size · not checked" (D8).
    FitsBySize,
    /// The header wasn't read (or couldn't be), and the file's bytes alone
    /// are more than the room (D8).
    DoesNotFitBySize,
    /// Neither the header's fit nor the file's size is known: the listing
    /// gives no size, or a split file's part is missing. Never a fit.
    Unknown,
    /// Nothing to measure against: no GPU telemetry and no declared budget
    /// (D9).
    #[default]
    NoCard,
}

impl FitMark {
    /// Whether the file fits at all, by its header or by its size.
    pub fn fits(self) -> bool {
        matches!(self, Self::Fits | Self::FitsShorter | Self::FitsBySize)
    }
}

/// What a footprint is made of, at the fit's context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FitTerms {
    /// The weights on the card, the projector and the drafter included.
    pub weights_bytes: u64,
    /// The conversation's memory: the KV cache and any recurrent state.
    pub kv_cache_bytes: u64,
    /// Working memory: the compute buffers.
    pub compute_bytes: u64,
}

/// One of lmgw's models on the card now, as the beside measure charges it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardResident {
    /// `chat | aux | audio | image`.
    pub class: String,
    pub model: String,
    /// Its charge plus what admission keeps free for it (an image
    /// pipeline's measured peak, an audio model's pending load).
    pub bytes: u64,
}

/// The card a fit was measured against (design §3.1, §9.2), from one ledger
/// read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardMeasure {
    /// What lmgw may use: the declared GPU memory budget, else the devices'
    /// total.
    pub capacity_bytes: u64,
    /// The headroom setting, added to every footprint.
    pub headroom_bytes: u64,
    /// The capacity is the declared budget, not the devices' total.
    pub budget: bool,
    /// Admission is on and refuses a model that doesn't fit.
    pub admission: bool,
    /// The GPU hold is on: local models are paused (D10). The marks don't
    /// change for it.
    pub hold: bool,
    /// What other programs hold on the card now: the devices' used memory
    /// less lmgw's charges. `None` without a device reading, or when one of
    /// lmgw's models on the card has no figure.
    pub outside_bytes: Option<u64>,
    /// lmgw's own models on the card now, with what admission keeps free for
    /// them.
    pub residents: Vec<CardResident>,
}

impl CardMeasure {
    /// What lmgw's own models on the card are charged, together.
    pub fn residents_bytes(&self) -> u64 {
        self.residents
            .iter()
            .map(|r| r.bytes)
            .fold(0, u64::saturating_add)
    }
}

/// What else is on the card, for the beside measure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BesideTerms {
    /// Other programs' use; `None` when it couldn't be told (the note says
    /// why), and then the measure is the card less lmgw's models alone.
    pub outside_bytes: Option<u64>,
    /// lmgw's own models on the card now.
    pub residents_bytes: u64,
}

/// One file's fit on the card, by one measure (design §3.1, §9.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileFit {
    pub mark: FitMark,
    pub measure: FitMeasure,
    /// The largest context that fits, at most the trained one (D6); `None`
    /// when nothing fits, without a card, or for a mark by size.
    pub ctx: Option<u64>,
    /// The model's trained context, from its header.
    pub trained_ctx: Option<u64>,
    /// The footprint at `ctx` (at one token when nothing fits; the files'
    /// bytes for a mark by size), without the headroom.
    pub uses_bytes: u64,
    /// The room this measure leaves: the card, less what is beside it for
    /// the beside measure. The headroom comes off it as well.
    pub capacity_bytes: u64,
    pub headroom_bytes: u64,
    pub terms: FitTerms,
    /// What else is on the card, for the beside measure.
    pub beside: Option<BesideTerms>,
    /// Bits per weight over the repeating layers (D7), every part of a split
    /// file's.
    pub bits_per_weight: Option<f64>,
    /// The model's parameters from the header's descriptors (D7, D131).
    pub parameters: Option<u64>,
    /// What the reader should also know, each a sentence: the hold, admission
    /// off, a term that couldn't be sized, why the header wasn't read.
    pub notes: Vec<String>,
}

/// Which of the pick's rules chose the file (design §3.2, D7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PickRule {
    /// The largest file that fits the whole trained context.
    WholeContext,
    /// No file fits the whole context: the largest that fits at least
    /// 32 768 tokens, or the trained context when that is smaller.
    ShorterContext,
    /// No file of 4 to 8.5 bits per weight fits at least that much.
    #[default]
    None,
}

/// The smaller file that keeps the whole context, named in the fold when the
/// pick fits only a shorter one (D7).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FullContextFile {
    pub file: String,
    pub quant: Option<String>,
    pub trained_ctx: u64,
    pub bits_per_weight: Option<f64>,
    /// "Q4_K_S remembers all 262 144 tokens, at 4.6 bits per weight."
    pub sentence: String,
}

/// The file a repo's list recommends, by one measure (design §3.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilePick {
    /// The recommended file; `None` when no file qualifies.
    pub file: Option<String>,
    pub rule: PickRule,
    pub measure: FitMeasure,
    /// The "Why this file" line.
    pub why: String,
    /// When the pick fits only a shorter conversation and a smaller file of
    /// at least 4 bits per weight keeps the whole one.
    pub full_context: Option<FullContextFile>,
}

/// Where a download time's rate came from (D23).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum RateSource {
    /// This machine's finished Hugging Face downloads, weighted by bytes.
    Downloads,
    /// The header reads made for this answer, from the same CDN.
    HeaderReads,
    /// Nothing measured yet: no time is given.
    #[default]
    None,
}

/// A download's size and the time it takes on this connection (D23).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DownloadEstimate {
    /// The files: the weights (every part) and their companions.
    pub files_bytes: u64,
    /// The engine image's layers still to fetch; `None` when it is on this
    /// machine already or its size isn't known (`notes` says which).
    pub engine_bytes: Option<u64>,
    /// Bytes per second; `None` with nothing measured.
    pub rate_bytes_per_sec: Option<f64>,
    pub rate_from: RateSource,
    /// The whole download's seconds at that rate, rounded up.
    pub secs: Option<u64>,
    /// "about 4 minutes on your connection".
    pub sentence: Option<String>,
    pub notes: Vec<String>,
}

/// Where a plan's input came from (design §2.3, §9.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlanFrom {
    /// The files on disk in the class's models directory.
    #[default]
    Disk,
    /// The file's header read from Hugging Face before any download.
    Hub,
}

/// A plan's input, as the plan's answer names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanSource {
    pub from: PlanFrom,
    /// The Hugging Face repo, for a plan from the hub.
    pub repo: Option<String>,
    /// The file's path in the repo, for a plan from the hub.
    pub file: Option<String>,
    /// The weights' content hash the hub named (`lfs.oid`).
    pub oid: Option<String>,
    /// The commit `main` resolved to when the header was read.
    pub commit: Option<String>,
    /// The header bytes this plan fetched from the hub, every file's (the
    /// weights' parts and the companions'); a header the process had read
    /// already (cached by its hash) adds nothing (D123).
    pub header_bytes_read: u64,
}

/// A plan's fit on the card by both measures (design §9.1): what
/// `local_model_plan` answers beside a chat or aux plan, from the disk or
/// the hub. An image plan has none yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanFit {
    /// The card both fits were measured against; `None` without one (D9).
    pub card: Option<CardMeasure>,
    pub whole_card: FileFit,
    pub beside: FileFit,
}

/// The note a row keeps when its context was fitted to the card rather
/// than planned at the trained one (D26): what Get it (WP5) writes on the
/// row it creates, and what the model editor's "Fit again" renews.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FitNote {
    /// The context the row got.
    pub ctx: u64,
    /// The model's trained context, which the plan proposed.
    pub trained_ctx: Option<u64>,
    /// The measure it was fitted to.
    pub measure: FitMeasure,
    /// That measure's room, before the headroom, and the headroom.
    pub capacity_bytes: u64,
    pub headroom_bytes: u64,
    /// When, as the server's UTC time (`YYYY-MM-DD HH:MM:SS`).
    pub at: String,
}

// ---------------------------------------------------------------------------
// Search by name (design §4, §9.2; WP4)
// ---------------------------------------------------------------------------

/// How many entries of `others`, of `did_you_mean` and of the card's
/// `other_uploads` an answer carries when the arguments don't say (the WP4
/// call D142): a page of the hub can hold hundreds of originals or uploads,
/// and a tool's answer is read whole into a model's context. The totals are
/// always there.
pub const DEFAULT_LIST_COUNT: u32 = 10;

/// The GPU hold's line on a fit (D10): where to resume, and that lmgw never
/// releases it for this. lmgw-core's fit puts it on a fit's notes, and the
/// finder's card shows it once from the live hold and leaves the note out.
pub const HOLD_LINE: &str = "lmgw is paused on the graphics card right now. The model \
     answers once you resume it: the HOLD button at the foot of the sidebar, or the tray menu's \
     GPU hold.";

/// `model_find`'s arguments (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelFindArgs {
    /// A model's name as people say it ("qwen 3.6"), a Hugging Face address
    /// (a repo, a file, a download link, also inside pasted text), or
    /// `owner/repo`. Names are searched as typed: numbers are written as
    /// digits.
    pub query: String,
    /// The previous answer's `next_cursor`: the search's next page.
    pub cursor: Option<String>,
    /// The first entry of `others` to answer with, from 0 (D142).
    pub others_offset: u32,
    /// How many entries of `others` to answer with; 10 when absent (D142).
    /// `others_total` and `others_more` say how many there are.
    pub others_count: u32,
    /// The first entry of `did_you_mean` to answer with, from 0 (D142).
    pub did_you_mean_offset: u32,
    /// How many entries of `did_you_mean` to answer with; 10 when absent
    /// (D142).
    pub did_you_mean_count: u32,
    /// The first entry of the card's `other_uploads` to answer with, from 0
    /// (D142).
    pub other_uploads_offset: u32,
    /// How many entries of the card's `other_uploads` to answer with; 10
    /// when absent (D142).
    pub other_uploads_count: u32,
}

impl Default for ModelFindArgs {
    fn default() -> Self {
        Self {
            query: String::new(),
            cursor: None,
            others_offset: 0,
            others_count: DEFAULT_LIST_COUNT,
            did_you_mean_offset: 0,
            did_you_mean_count: DEFAULT_LIST_COUNT,
            other_uploads_offset: 0,
            other_uploads_count: DEFAULT_LIST_COUNT,
        }
    }
}

/// What a `model_find` answer is (§4.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FindAnswer {
    /// A card: a model with a file that fits by its size.
    #[default]
    Card,
    /// Nothing on the hub matches.
    NoMatch,
    /// A match exists, but no GGUF of it does.
    NoRunnableFile,
    /// Only repos that match some of the words: `did_you_mean`.
    LooseOnly,
    /// Every upload that could be used is gated and couldn't be read.
    AllGated,
    /// No file of the top author's models fits: the card says so, for the
    /// one closest to fitting.
    NothingFits,
    /// The model has GGUF files, but none of 4 to 8 bits per weight, the
    /// files lmgw picks by itself (D71): the sentence says what they are.
    NoFileInBand,
    /// The top match is a kind of model Find a model doesn't set up yet
    /// (pictures, speech): the sentence points to where it is added.
    OtherKind,
    /// Hugging Face couldn't be reached.
    Unreachable,
    /// Hugging Face's rate limit is used up: `hub_budget.reset_s` says when
    /// it resets.
    RateLimited,
    /// An address on another host: answered locally, nothing sent (D124).
    NotHuggingFace,
    /// A Hugging Face page that isn't a model's (an account's, a dataset's,
    /// a Space's): answered locally, nothing sent.
    NotAModel,
    /// A later page of the search (`cursor`): only the lists.
    More,
}

/// The kind of row a card's model becomes (D126, D29).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum CardKind {
    #[default]
    Chat,
    Embed,
    Rerank,
    /// A decision model: known only from its header (`decision.type`), so
    /// no search answer says it yet.
    Decision,
}

/// The hub's rate limit after this answer (§4.5), from its `RateLimit` and
/// `RateLimit-Policy` headers. lmgw keeps no counter of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HubBudget {
    /// Requests left in the window.
    pub remaining: Option<u64>,
    /// Seconds until the window resets.
    pub reset_s: Option<u64>,
    /// Requests the window allows.
    pub quota: Option<u64>,
    /// The window's length in seconds.
    pub window_s: Option<u64>,
    /// The requests this answer made.
    pub calls: u32,
    /// Fewer requests are left than this answer made: the next search may
    /// meet the limit.
    pub near: bool,
    /// What the person reads when `near` or limited.
    pub sentence: Option<String>,
    /// What the hub allows, from the last policy it sent, with or without
    /// the token ("Without an account it allows 500 requests every 5
    /// minutes; a Hugging Face token in Settings raises that."): the end of
    /// `sentence`, alone, for a page that says the wait itself. `None`
    /// without a policy.
    pub allowance: Option<String>,
}

/// One model in a list: "Other models with this name", "Did you mean".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelChoice {
    /// The repo, `owner/name`.
    pub origin: String,
    /// The repo's name with hyphens as spaces.
    pub name: String,
    /// The author's id on the hub (a card's "by" is the display name).
    pub author: String,
    pub likes: u64,
    pub downloads: u64,
    /// The hub's pipeline name.
    pub pipeline: Option<String>,
    /// The listing's parameter total.
    pub parameters: Option<u64>,
    /// How many of the query's words its name matched (D13).
    pub matched_words: u32,
    pub gated: bool,
    pub private: bool,
    /// "not available as a file lmgw can run yet", when the walk found so.
    pub note: Option<String>,
}

/// Another upload of the card's model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadChoice {
    pub repo: String,
    pub likes: u64,
    pub downloads: u64,
    pub gated: bool,
    pub private: bool,
    /// A plain upload (the original's name, D16); a renamed one (an MTP
    /// variant, an abliterated one) when false.
    pub plain: bool,
}

/// "Send this model to a friend" (§7, D31): the file's public Hugging Face
/// address and the sentence around it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ShareLink {
    pub url: String,
    pub text: String,
}

/// The other measure's file, when the two picks differ (D120).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardFile {
    pub file: String,
    pub quant: Option<String>,
    pub bytes: u64,
    pub fit: FileFit,
}

/// The card (§5, §9.2): one model and one file of it, marked by size until
/// the page reads the header (D123).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelCard {
    /// The original's repo name with hyphens as spaces ("Qwen3.6 27B").
    pub name: String,
    /// The author's display name on the hub (D19), else its id.
    pub by: String,
    /// The account's id when the author is a person, whose display name is
    /// self-chosen: shown beside `by`. `None` for an organisation.
    pub by_user: Option<String>,
    /// The original, `owner/name`.
    pub origin: String,
    /// The original's likes and downloads on the hub, as its listing says
    /// them (a later page's card is listed among the other models).
    pub likes: u64,
    pub downloads: u64,
    pub kind: CardKind,
    /// The upload the file is from; empty when the walk passed the model
    /// over without reading its uploads (`NothingFits`).
    pub upload: String,
    /// The file (a split file's first part); empty when none was read.
    pub file: String,
    pub quant: Option<String>,
    pub parts: u32,
    /// The projector D121 picks, and the first drafter.
    pub companions: Vec<crate::HfCompanion>,
    /// "reads pictures", … (D22): what the listings tell.
    pub can: Vec<String>,
    /// The hub listing's parameter total; the header's count replaces it
    /// once read (`hf_repo` with `fit`).
    pub parameters: Option<u64>,
    /// The licence the hub's tags name, linked to the original's page
    /// (D22). Shown, never a filter.
    pub licence: Option<crate::ModelLicence>,
    pub gated: bool,
    /// The upload or the original is private to the account (D124).
    pub private: bool,
    /// The original is private to the account: a share text from a public
    /// upload of it names the model by the upload (WP7, D109).
    pub origin_private: bool,
    /// The file's bytes and its companions', with the time (D23).
    pub download: DownloadEstimate,
    /// Free bytes where the files land (D125).
    pub disk_free_bytes: Option<u64>,
    /// The file's fit by size (D123): beside what runs now when that pick
    /// exists, else on the whole card (D120); `fit.measure` says which.
    pub fit: FileFit,
    /// The whole card's pick, when it differs from `file` (D120).
    pub whole_card: Option<CardFile>,
    /// "Why this file" (§3.2): the upload, the rule, the width by size.
    pub why: String,
    /// None for a private repo (D124) and for a card without a file.
    pub share: Option<ShareLink>,
    /// The file a pasted address named, when `file` is that file (WP7,
    /// D100): the header's marks keep it as the card's file while it fits,
    /// and only then pass to their own pick.
    pub named_file: Option<String>,
    /// The original's other uploads, plain ones first: the window the
    /// arguments asked for (D142).
    pub other_uploads: Vec<UploadChoice>,
    /// How many other uploads the hub's first page listed, and how many come
    /// after `other_uploads`.
    pub other_uploads_total: u32,
    pub other_uploads_more: u32,
    /// The hub has more uploads than its first page listed.
    pub more_uploads: bool,
}

/// `model_find`'s answer (§9.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelFind {
    pub query: String,
    /// None: see `answer`.
    pub card: Option<ModelCard>,
    /// The other originals found, by rank (D15): the window the arguments
    /// asked for (D142).
    pub others: Vec<ModelChoice>,
    /// How many other originals the search found, and how many come after
    /// `others`.
    pub others_total: u32,
    pub others_more: u32,
    /// Partial matches (D13), by matched words: the window asked for.
    pub did_you_mean: Vec<ModelChoice>,
    pub did_you_mean_total: u32,
    pub did_you_mean_more: u32,
    pub answer: FindAnswer,
    /// What the person reads for `answer`.
    pub sentence: String,
    /// The search's next page, for `cursor`.
    pub next_cursor: Option<String>,
    pub hub_budget: Option<HubBudget>,
    /// What the walk passed over and why, and what couldn't be read (the
    /// fold).
    pub notes: Vec<String>,
}
