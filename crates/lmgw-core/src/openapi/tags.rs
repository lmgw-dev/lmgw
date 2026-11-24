//! The tag list (api-docs design §4.3), final as of WP1: every `DocRoute` and
//! `OpDoc` names one of these by `id`, and the rail groups by [`Group`] then
//! lists tags in this order (§6.4).
//!
//! `name` is the human label the rail head and the OpenAPI tag's own
//! `description` show; `description` is a longer sentence for the handful of
//! tags that need one — empty means "use `name`" (`build.rs`'s
//! `tag_description`). `id` is what a `DocRoute`/`OpDoc` references and what
//! the emitted tag's `name` field is (OpenAPI's `tags: […]` on an operation
//! matches by that field, not by this list's own `name`).

use super::registry::Group;

pub(crate) struct TagDef {
    pub id: &'static str,
    pub name: &'static str,
    pub group: Group,
    pub description: &'static str,
}

/// Group order, then declaration order — the rail's own order (§6.4).
pub(crate) const TAGS: &[TagDef] = &[
    // -- Inference -----------------------------------------------------
    TagDef {
        id: "openai",
        name: "OpenAI-compatible",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "anthropic",
        name: "Anthropic-compatible",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "llamacpp",
        name: "llama.cpp-compatible",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "lmgw-inference",
        name: "lmgw extensions",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "docling-serve",
        name: "docling-serve compatible",
        group: Group::Inference,
        description: "docling-serve's convert routes (release 1.38), answered by lmgw's own \
            conversion, for the tools that call docling today: point them at lmgw and change \
            the key (X-Api-Key works; the status WebSocket takes ?api_key=). Every docling \
            option a request sets gets an outcome, listed in x-lmgw-option-notes when it is \
            not honoured. honoured: from_formats (others are skipped), to_formats md and \
            text, do_ocr, do_table_structure, page_range, document_timeout (a per-file \
            deadline), abort_on_error, md_page_break_placeholder, md_compact_tables. Mapped \
            onto lmgw's equivalent: ocr_engine and ocr_preset (any name: lmgw's OCR), \
            ocr_lang (BCP-47 to tesseract codes), pdf_backend (poppler), table_mode \
            (accurate: table pages go to a chosen page-reading model; fast: lmgw's own \
            tables, and a chosen page-reading model reads only the pages nothing else \
            read), table_cell_matching and the table/layout presets (lmgw's tables and \
            layout), \
            pipeline (standard, legacy, native: lmgw's pipeline; vlm: the page-reading \
            model; asr: the speech-to-text model chosen under Settings > Documents, which \
            transcribes audio and videos on every pipeline), the vlm_pipeline_* options (the page-reading model; an API url is not \
            followed), do_pdf_heading_hierarchy (lmgw's heading levels), picture \
            descriptions (the picture-description model, the caller's prompt kept; an API \
            url is not followed), formula and code enrichment (do_formula_enrichment, \
            do_code_enrichment, code_formula_preset: the page-reading model writes formulas \
            as LaTeX and code as fenced blocks on the pages it reads; no option picks pages \
            for them). image_export_mode (placeholder, embedded, referenced: the \
            zip holds referenced pictures under artifacts/ as the markdown links them), \
            include_images (false keeps placeholders) and images_scale are the native \
            pictures and images_scale. do_picture_description is the native \
            describe_pictures, answered by the picture-description model the owner chose \
            (mapped; none chosen: mapped(model_not_chosen), pictures without descriptions), \
            picture_description_api/_local/_custom_config give their prompt and a note naming \
            each of their keys (url and headers never followed), and \
            picture_description_area_threshold is the native describe_min_area (smaller \
            pictures kept, not described); pictures are exported whatever their size, as \
            docling exports them. Not yet: to_formats json \
            (DoclingDocument), html, doctags and the rest (their fields stay null, status \
            partial_success, errors names them), include_page_images, ocr_custom_config, picture \
            classification, chart extraction, chunking, and \
            any option lmgw does not know (a note, never a 422; a wrong type for a known \
            option is docling's 422). lmgw calls no address a caller chose but one: an http \
            source is fetched only with the owner's setting documents.fetch_sources on, \
            for a request with an API key, and from the hosts on documents.source_hosts (a \
            private, loopback or link-local address, or one of this computer's own, only \
            when listed itself; redirects re-checked; max_body_mb, SOURCE_FETCH_TIMEOUT, \
            a gateway-wide fetch pool); off it is a 400 http_source_refused, without a key \
            a 401 http_source_needs_key, with max_body_mb 0 a 400 \
            http_source_needs_limit, an unlisted host 400 http_source_host_refused, and a \
            fetch that fails fails its file with \
            source_address_refused, source_redirect_refused, source_too_large, \
            source_timeout, source_http_status or source_unreachable. Callbacks are not \
            called, s3 and other storage sources are 400 source_kind_refused, \
            put/s3/presigned_url targets 400 target_refused.",
    },
    TagDef {
        id: "mcp",
        name: "MCP",
        group: Group::Inference,
        description: "",
    },
    // -- Gateway -----------------------------------------------------------
    TagDef {
        id: "meta",
        name: "API description",
        group: Group::Gateway,
        description: "",
    },
    // -- Dashboard API ---------------------------------------------------
    TagDef {
        id: "session",
        name: "Session",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "status",
        name: "Status & live feed",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "models",
        name: "Models",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "upstreams",
        name: "Upstreams",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "tools",
        name: "MCP servers & tools",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "downloads",
        name: "Downloads",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "settings",
        name: "Settings",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "responses",
        name: "Stored responses",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "usage",
        name: "Usage",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "docs",
        name: "Doc corpora",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "knowledge",
        name: "Knowledge bases",
        group: Group::DashboardApi,
        description: "Named collections of your own documents: create a base, upload \
                       files, follow the background job that reads and embeds them, view a \
                       file's text and search across bases. Uploading and changing a base's \
                       model or chunk size start jobs (kinds kb_ingest and kb_reembed, key \
                       kb:<id>) that answer at once; watch them on the jobs routes and the \
                       event stream.",
    },
    TagDef {
        id: "audio-lab",
        name: "Audio lab",
        group: Group::DashboardApi,
        description: "The dashboard's audio playground: the audio models and their voices, \
                       the library of reference clips a voice-cloning model reads (upload, \
                       preview, transcripts, delete), and the synthesis, transcription, \
                       alignment and generic-task routes that run a request through the same \
                       handlers as /v1/audio/* and /v1/tasks/*.",
    },
    TagDef {
        id: "image-lab",
        name: "Image lab",
        group: Group::DashboardApi,
        description: "The dashboard's image playground: the models that can draw, and the \
                       generate and edit routes that turn a form into a /v1/images/* request \
                       and run it through the same handlers.",
    },
    TagDef {
        id: "documents-lab",
        name: "Documents lab",
        group: Group::DashboardApi,
        description: "The dashboard's document-conversion playground: a form and its files \
                       kept as a group, and the run that sends them to \
                       /v1/documents/convert (sync or as a job) or the docling-compatible \
                       /v1/convert/file through the same handlers.",
    },
    TagDef {
        id: "decision-lab",
        name: "Decision lab",
        group: Group::DashboardApi,
        description: "The dashboard's System One playground: the models that answer \
                       /v1/systemone with their header facts and token limits, the token \
                       gauge, and Ask, which stores a group (models × variants) whose cells \
                       run one at a time through the same handler as /v1/systemone and are \
                       kept in the lab store.",
    },
    TagDef {
        id: "voice-lab",
        name: "Voices lab",
        group: Group::DashboardApi,
        description: "The dashboard's voice sample generator: fixed voices for the cloning \
                       text-to-speech models. A text-to-speech model renders rounds of \
                       candidates of a phonetically rich sentence; the chosen one is saved to \
                       the voice library with its exact transcript and how it was made, and a \
                       cloning model can speak a test line with it.",
    },
    TagDef {
        id: "labs",
        name: "Lab store",
        group: Group::DashboardApi,
        description: "What the labs keep, one store for all three: each lab's history of \
                       groups (one click: the form, the plan, the override) and runs (one \
                       dispatch: the request and answer as the texts that went and came), the \
                       artifact files they name, the size it all takes, deleting a group or a \
                       whole lab, and the JSONL, zip or CSV export. Nothing is deleted by age \
                       or count.",
    },
    TagDef {
        id: "agents",
        name: "Agents",
        group: Group::DashboardApi,
        description: "",
    },
    // -- Device API ----------------------------------------------------------
    TagDef {
        id: "chat",
        name: "Chat",
        group: Group::DeviceApi,
        description: "The Chat API a client app uses as a paired device (the `chat` \
                       capability), with its client certificate on the device listener: the \
                       threads and folders it follows, an ongoing conversation's current \
                       thread, and the change feed that keeps them current. Every route here \
                       refuses a device with 401 device_key_unknown once its key opens no \
                       device (it was replaced or deleted: pair the device again), 403 \
                       device_unconfirmed until the owner confirms its enrolment, 401 \
                       device_disabled while the device is disabled, 401 \
                       device_certificate_expired past its certificate's notAfter, and 401 \
                       key_expired past its expiry date. What is open when that happens ends: \
                       the feed with `revoked`, a turn's stream with an `error` event whose \
                       code is revoked. Both carry `kind`, what to do: device_disabled and \
                       key_expired wait for the gateway's side, key_unknown and \
                       certificate_expired mean pair the device again, and revoked is any \
                       other kind of key's revocation.",
    },
    TagDef {
        id: "transfers",
        name: "Transfers",
        group: Group::DeviceApi,
        description: "Files between paired devices of one app (device transfers design): a \
                       device offers a file to another device that declared the same app id \
                       at pairing, uploads it to lmgw in ranges it can resume, and the other \
                       device is offered it once every byte matched its sha256, downloads it \
                       with ranges and says it arrived; lmgw then deletes the bytes. The \
                       transfer capability: paired devices (on the device listener) send and \
                       receive; the owner lists every transfer and cancels any; a client key or \
                       an agent token is refused 403 transfer_device_only. A refusal is the \
                       flat {code, message}, with free_bytes, received or state where its code \
                       names one. JSON bodies are at most 16 KiB (16384 bytes), refused 413 \
                       body_too_large; the only bound on a file is the free space of lmgw's \
                       data directory. The Chat feed's transfer.updated says each offer, ready \
                       and end to the sender, to the receiver from ready on, and to the owner.",
    },
    TagDef {
        id: "devices",
        name: "Devices",
        group: Group::DeviceApi,
        description: "The device listener's own routes (device link design §4.6): served \
                       on the device listener only, over TLS 1.3, and the 404 an unknown \
                       path is on the plain listener. Their bodies are at most 16 KiB \
                       (16384 bytes), refused 413 body_too_large naming it, not \
                       max_body_mb. Every answer carries Connection: close.",
    },
    // -- Ops ---------------------------------------------------------------
    // No human names were carried over from the design table (§4.3 lists ids
    // only); these are the smallest reasonable reading of the self-admin
    // catalog's own grouping (`mcp/selfadmin/catalog/{routing,models,runtime,
    // sources,prices,docs,builds,agents,reads}.rs`).
    TagDef {
        id: "ops-routing",
        name: "Routing ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-models",
        name: "Model ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-runtime",
        name: "Runtime ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-downloads",
        name: "Download ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-settings",
        name: "Settings ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-tools",
        name: "Tool ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-keys",
        name: "Key ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-accounts",
        name: "Account ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-usage",
        name: "Usage & pricing ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-responses",
        name: "Stored-response ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-builds",
        name: "Backends & build ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-bench",
        name: "Benchmark ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-agents",
        name: "Agent ops",
        group: Group::Ops,
        description: "",
    },
    // -- Agent runtime -------------------------------------------------------
    TagDef {
        id: "agent-runtime",
        name: "Agent runtime",
        group: Group::AgentRuntime,
        description: "The routes an agent container calls with its own token: reading its own \
                       definition and runs, and opening and driving a run.",
    },
];
