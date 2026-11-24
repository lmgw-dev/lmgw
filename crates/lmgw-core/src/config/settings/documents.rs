//! Settings › Documents' stored choices (document-conversion design §4.4),
//! the `documents` section: the tool pool's size, how long async conversion
//! jobs keep their results, whether the docling adapter fetches web
//! addresses and from where, whether the layout model is used, and the
//! page-reading model (WP6) and the picture-description model (WP11). OCR
//! languages join it with the work package that uses them.

use serde::{Deserialize, Serialize};

/// `documents.job_retention_days`'s default: one day. A job's stored results
/// are the converted documents' text, so they are kept long enough for a
/// caller to fetch them and not much longer.
pub const JOB_RETENTION_DAYS_DEFAULT: i64 = 1;

/// The `documents` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DocumentsSettings {
    /// "Parallel tool runs": how many conversion steps (tool runs, native
    /// reads) run at once, gateway-wide (design §7.7). **0 = derived** from
    /// the machine: the smaller of the logical cores and the memory over the
    /// largest per-run memory limit.
    pub tool_slots: u32,
    /// Settings › Retention › "Conversion jobs": days an ended async
    /// conversion job and its stored results are kept (design §7.3, §9).
    /// **0 keeps them** (a cancelled job's results go at once). A
    /// docling-serve client's fetched result goes sooner, 300 s after its
    /// first fetch, as docling-serve's single-use results do.
    pub job_retention_days: i64,
    /// "Let callers send web addresses": whether the docling-serve adapter
    /// fetches `http`/`https` sources (design ruling 9, D28). **Off by
    /// default**: a web source is then a named 400 that asks for the file
    /// itself.
    pub fetch_sources: bool,
    /// "Allowed sites": the only hosts a web source may name, each a host
    /// name or address with an optional `:port` (without one: ports 80 and
    /// 443), stored in its normal spelling. A private, loopback or
    /// link-local address is fetched only when listed here itself
    /// (`convert::source_fetch::hosts` has the matching rule).
    pub source_hosts: Vec<String>,
    /// "Use the layout model" (design §3, §4.4): PDF pages are laid out
    /// from the regions docling's layout model finds, on the processor.
    /// Off until someone switches it on, and it can only be switched on
    /// while the model's file is here.
    pub onnx_layout: bool,
    /// "Read pages with a model": the lmgw name of the vision model that
    /// reads the pages tier 0 reads badly (design §3 tier 1, §8, D5, D15).
    /// **Empty = off**, the default: no conversion asks a model, and
    /// `model_help` other than `off` answers `model_not_chosen`.
    pub page_reader: String,
    /// The page-reading model answers in DocTags (docling's page format, a
    /// granite-docling model): it is asked with docling's own prompt and its
    /// answer is read by the DocTags reader. Off: lmgw's page prompts, the
    /// answer read as markdown.
    pub page_reader_doctags: bool,
    /// "Describe pictures with": the lmgw name of the vision model that
    /// describes a conversion's exported pictures when a request asks for
    /// it (`describe_pictures`, docling's `do_picture_description`; design
    /// §5.3, ruling 5, D5). **Empty = off**, the default: no picture is
    /// sent to a model, and `describe_pictures` answers `model_not_chosen`.
    /// It may be the same model as `page_reader`.
    pub picture_alias: String,
    /// "Transcribe audio with" (design §4.2, §4.4, WP7): the alias audio
    /// and video files are transcribed with in a conversion, as the calling
    /// key. **Empty: none chosen**, so a conversion never calls a model for
    /// them (D5) and they fail with `model_not_chosen`.
    pub asr_alias: String,
    /// "Storage for conversion jobs" (security round A): the most the async
    /// conversion jobs may store together (their uploads while they run,
    /// their results after), in MiB. **0 = derived**: half the space that
    /// was free on the data directory's file system when lmgw started. A
    /// job past it is refused with 507, a result past it is not stored.
    pub job_storage_mib: u64,
    /// "Jobs per key" (security round A): the most async conversion jobs
    /// one key (or every request without a key, together) has running or
    /// queued at once. **0 = derived**: the tool pool's size. One more is
    /// refused with 429.
    pub jobs_per_key: u32,
}

impl Default for DocumentsSettings {
    fn default() -> Self {
        Self {
            tool_slots: 0,
            job_retention_days: JOB_RETENTION_DAYS_DEFAULT,
            fetch_sources: false,
            source_hosts: Vec::new(),
            onnx_layout: false,
            page_reader: String::new(),
            page_reader_doctags: false,
            picture_alias: String::new(),
            asr_alias: String::new(),
            job_storage_mib: 0,
            jobs_per_key: 0,
        }
    }
}
