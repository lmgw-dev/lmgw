//! Chat export (chat-complete design §6): a thread as a readable Markdown
//! transcript or as lossless JSON, one thread per file, and folders / the
//! whole Chat as a `.zip` with one file per thread.
//!
//! - `GET /chat/api/threads/{id}/export?format=md|json`
//! - `GET /chat/api/folders/{id}/export?format=…&archived=0|1|all`
//! - `GET /chat/api/export?format=…&archived=0|1|all`
//!
//! A thread is read through [`ChatRepo`], so a temporary thread (negative
//! id) exports from memory — that is one of the two ways to keep one. Zips
//! are written into an anonymous temp file under the data dir, one thread in
//! memory at a time, and streamed back with their length: an export of every
//! thread with its attachments is as large as the Chat is and is not capped.
//! The writing — rendering, compressing, file I/O — runs on the blocking
//! pool, not on a runtime worker, and a JSON file is serialized straight
//! into the zip with its attachments' base64 written as it goes, so a thread
//! is in memory about once rather than four times over (review R1 item a).

use std::io::{Seek, SeekFrom, Write};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use zip::write::SimpleFileOptions;

use super::api_knowledge::attachment as content_disposition;
use super::chat::err_json;
use super::chat_extract::{ChatPath, ChatQuery};
use super::chat_repo::ChatRepo;
use crate::state::{AppState, SharedState};
use crate::store::{
    self, ChatAttachmentFull, ChatAttachmentMeta, ChatFolder, ChatMessageRow, ChatThread, DbResult,
    KbMode, ThreadListMode,
};

/// The `format` marker of the JSON export; an import checks it.
pub const JSON_FORMAT: &str = "lmgw.chat.v1";

#[derive(Deserialize, Default)]
pub struct ExportQuery {
    /// `md` (default) or `json`.
    #[serde(default)]
    format: String,
    /// Zips only: `0` active threads, `1` archived only, `all` both
    /// (default).
    #[serde(default)]
    archived: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Md,
    Json,
}

impl Format {
    fn parse(s: &str) -> Result<Self, Response> {
        match s {
            "" | "md" | "markdown" => Ok(Self::Md),
            "json" => Ok(Self::Json),
            other => Err(err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("format must be md or json, not {other:?}"),
            )),
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Md => "md",
            Self::Json => "json",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Md => "text/markdown; charset=utf-8",
            Self::Json => "application/json",
        }
    }
}

fn archived_mode(s: &str) -> Result<ThreadListMode, Response> {
    match s {
        "" | "all" => Ok(ThreadListMode::All),
        "0" => Ok(ThreadListMode::Active),
        "1" => Ok(ThreadListMode::Archived),
        other => Err(err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("archived must be 0, 1 or all, not {other:?}"),
        )),
    }
}

fn internal(e: impl std::fmt::Display) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

fn not_found(what: &str) -> Response {
    err_json(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("{what} not found"),
    )
}

/// A file-name fragment from a title: lower-case letters and digits (any
/// script) joined by single dashes; `untitled` when nothing is left.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "untitled".into()
    } else {
        out
    }
}

fn thread_file_name(t: &ChatThread, f: Format) -> String {
    let id = if t.id < 0 {
        format!("temp{}", -t.id)
    } else {
        t.id.to_string()
    };
    format!("lmgw-chat-{id}-{}.{}", slug(&t.title), f.ext())
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// A thread with everything the export needs.
struct Bundle {
    thread: ChatThread,
    folder: Option<ChatFolder>,
    messages: Vec<ChatMessageRow>,
    /// Every attachment of the thread, bytes included; drafts (no message)
    /// too.
    attachments: Vec<Attachment>,
}

struct Attachment {
    meta: ChatAttachmentMeta,
    full: ChatAttachmentFull,
    /// Its position among its message's files.
    ord: i64,
    created_at: String,
}

async fn load(s: &AppState, thread: ChatThread) -> DbResult<Bundle> {
    let repo = ChatRepo::of(thread.id);
    let messages = repo.messages(s, thread.id).await?;
    let order = repo.attachment_order(s, thread.id).await?;
    let mut attachments = Vec::new();
    for meta in repo.attachments_meta(s, thread.id).await? {
        if let Some(full) = repo.attachment(s, meta.id).await? {
            let (ord, created_at) = order.get(&meta.id).cloned().unwrap_or_default();
            attachments.push(Attachment {
                meta,
                full,
                ord,
                created_at,
            });
        }
    }
    let folder = match thread.folder_id {
        Some(id) if !repo.is_temp() => store::get_chat_folder(&s.db, id).await?,
        _ => None,
    };
    Ok(Bundle {
        thread,
        folder,
        messages,
        attachments,
    })
}

// -- routes -----------------------------------------------------------------

/// `GET /chat/api/threads/{id}/export`.
pub async fn export_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatQuery(q): ChatQuery<ExportQuery>,
) -> Response {
    let format = match Format::parse(&q.format) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let repo = ChatRepo::of(id);
    let thread = match repo.thread(&state, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return not_found("thread"),
        Err(e) => return internal(e),
    };
    let bundle = match load(&state, thread).await {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let rendered = tokio::task::spawn_blocking(move || {
        let body = render(&bundle, format)?;
        Ok::<_, anyhow::Error>((bundle, body))
    })
    .await;
    let (bundle, body) = match rendered {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    (
        [
            (header::CONTENT_TYPE, format.mime().to_string()),
            (
                header::CONTENT_DISPOSITION,
                content_disposition(&thread_file_name(&bundle.thread, format)),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

/// `GET /chat/api/folders/{id}/export`.
pub async fn export_folder(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatQuery(q): ChatQuery<ExportQuery>,
) -> Response {
    let (format, mode) = match (Format::parse(&q.format), archived_mode(&q.archived)) {
        (Ok(f), Ok(m)) => (f, m),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let folder = match store::get_chat_folder(&state.db, id).await {
        Ok(Some(f)) => f,
        Ok(None) => return not_found("folder"),
        Err(e) => return internal(e),
    };
    let threads: Vec<ChatThread> = match ChatRepo::stored_threads(&state, mode).await {
        Ok(t) => t.into_iter().filter(|t| t.folder_id == Some(id)).collect(),
        Err(e) => return internal(e),
    };
    let readme = readme(
        &format!("the folder \"{}\"", folder.name),
        format,
        mode,
        threads.len(),
        false,
    );
    let name = format!("lmgw-folder-{}-{}.zip", slug(&folder.name), today());
    zip_response(&state, threads, format, readme, name, false).await
}

/// `GET /chat/api/export`.
pub async fn export_all(
    State(state): State<SharedState>,
    ChatQuery(q): ChatQuery<ExportQuery>,
) -> Response {
    let (format, mode) = match (Format::parse(&q.format), archived_mode(&q.archived)) {
        (Ok(f), Ok(m)) => (f, m),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let threads = match ChatRepo::stored_threads(&state, mode).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let readme = readme("every stored chat", format, mode, threads.len(), true);
    zip_response(
        &state,
        threads,
        format,
        readme,
        format!("lmgw-chats-{}.zip", today()),
        true,
    )
    .await
}

fn readme(what: &str, f: Format, mode: ThreadListMode, n: usize, all: bool) -> String {
    let which = match mode {
        ThreadListMode::Active => "active threads only",
        ThreadListMode::Archived => "archived threads only",
        ThreadListMode::All => "active and archived threads",
    };
    let mut s = format!(
        "lmgw chat export of {what}, {}.\n\n{n} thread(s): {which}, one {} file each.\n",
        today(),
        match f {
            Format::Md => "Markdown",
            Format::Json => "JSON (format lmgw.chat.v1)",
        }
    );
    if all {
        s.push_str("Threads in a folder are in a directory named after it.\n");
    }
    s.push_str(
        "Temporary chats live only in memory and are not part of a zip: export one from its own banner.\n",
    );
    if f == Format::Md {
        s.push_str(
            "Markdown lists attachments by name only; the JSON export carries their bytes and extracted text.\n",
        );
    }
    s
}

/// Write the zip into an anonymous temp file and stream it back.
async fn zip_response(
    state: &SharedState,
    threads: Vec<ChatThread>,
    format: Format,
    readme: String,
    file_name: String,
    by_folder: bool,
) -> Response {
    match build_zip(state, threads, format, &readme, by_folder).await {
        Ok((file, len)) => {
            let file = tokio::fs::File::from_std(file);
            let body = Body::from_stream(futures::stream::unfold(file, |mut f| async move {
                let mut buf = vec![0u8; 64 * 1024];
                match f.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), f))
                    }
                    Err(e) => Some((Err(e), f)),
                }
            }));
            (
                [
                    (header::CONTENT_TYPE, "application/zip".to_string()),
                    (header::CONTENT_DISPOSITION, content_disposition(&file_name)),
                    (header::CONTENT_LENGTH, len.to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                body,
            )
                .into_response()
        }
        Err(e) => internal(e),
    }
}

async fn build_zip(
    state: &AppState,
    threads: Vec<ChatThread>,
    format: Format,
    readme: &str,
    by_folder: bool,
) -> anyhow::Result<(std::fs::File, u64)> {
    let mut dirs: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    if by_folder {
        for f in store::list_chat_folders(&state.db).await? {
            dirs.insert(f.folder.id, slug(&f.folder.name));
        }
    }
    let data_dir = state.data_dir.clone();
    let readme = readme.to_string();
    let mut zip = blocking(move || {
        std::fs::create_dir_all(&data_dir)?;
        let mut zip = zip::ZipWriter::new(tempfile::tempfile_in(&data_dir)?);
        zip.start_file("README.txt", entry_options(false))?;
        zip.write_all(readme.as_bytes())?;
        Ok(zip)
    })
    .await?;
    for t in threads {
        let dir = t
            .folder_id
            .and_then(|id| dirs.get(&id))
            .map(|d| format!("{d}/"))
            .unwrap_or_default();
        let name = format!("{dir}{}", thread_file_name(&t, format));
        // The read is async (the DB); the rendering, compressing and writing
        // are not, and go to the blocking pool with the writer.
        let bundle = load(state, t).await?;
        zip = blocking(move || {
            zip.start_file(name, entry_options(mostly_compressed(&bundle, format)))?;
            write_rendered(&bundle, format, &mut zip)?;
            Ok(zip)
        })
        .await?;
    }
    blocking(move || {
        let mut file = zip.finish()?;
        let len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        Ok((file, len))
    })
    .await
}

/// Run `f` on the blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await?
}

/// A zip entry's options: deflated, or stored as is when what it holds is
/// already compressed — deflating it again spends the CPU for next to
/// nothing (review R1 item a).
fn entry_options(stored: bool) -> SimpleFileOptions {
    let method = if stored {
        zip::CompressionMethod::Stored
    } else {
        zip::CompressionMethod::Deflated
    };
    SimpleFileOptions::default()
        .compression_method(method)
        .large_file(true)
}

/// Whether an attachment's bytes are already compressed: images other than
/// bitmaps and SVG, PDFs, audio other than WAV, and the zip-based office
/// formats.
fn compressed(a: &ChatAttachmentFull) -> bool {
    let mime = a.mime.as_str();
    match a.kind.as_str() {
        "image" => !matches!(
            mime,
            "image/bmp" | "image/svg+xml" | "image/x-portable-pixmap"
        ),
        "pdf" => true,
        "audio" => !matches!(
            mime,
            "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave"
        ),
        "office" => {
            mime.starts_with("application/vnd.openxmlformats")
                || mime.starts_with("application/vnd.oasis.opendocument")
        }
        _ => false,
    }
}

/// A JSON entry that is mostly already-compressed attachment bytes (base64
/// of them) is stored rather than deflated; Markdown carries no bytes.
fn mostly_compressed(b: &Bundle, f: Format) -> bool {
    if f == Format::Md {
        return false;
    }
    let (mut packed, mut rest) = (0usize, 0usize);
    for a in &b.attachments {
        let n = a.full.data.len();
        if compressed(&a.full) {
            packed += n;
        } else {
            rest += n;
        }
        rest += a.full.extracted.as_ref().map_or(0, String::len);
    }
    for m in &b.messages {
        rest += m.content.len() + m.reasoning.len();
        rest += m.ir_messages.as_ref().map_or(0, String::len);
    }
    packed > rest
}

// -- formats ----------------------------------------------------------------

fn render(b: &Bundle, f: Format) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    write_rendered(b, f, &mut out)?;
    Ok(out)
}

/// Write the thread in format `f` into `w` — a zip entry, or the one
/// file's body.
fn write_rendered(b: &Bundle, f: Format, w: &mut impl Write) -> anyhow::Result<()> {
    match f {
        Format::Md => w.write_all(markdown(b).as_bytes())?,
        Format::Json => serde_json::to_writer_pretty(w, &JsonExport::of(b))?,
    }
    Ok(())
}

/// The lossless form: every thread column, every message column with
/// `ir_messages` parsed (a record that does not parse is kept as its raw
/// string, flagged `ir_messages_unparsed`), attachments with their metadata
/// (as the API serialises it), their position (`ord`) and upload time,
/// extracted text and base64 bytes. Drafts (unsent attachments) are in
/// `draft_attachments`.
///
/// Serialized straight into its writer: the bytes are base64-encoded as they
/// are written, never held as one more string (review R1 item a).
#[derive(Serialize)]
struct JsonExport<'a> {
    format: &'static str,
    exported_at: String,
    thread: ThreadOut<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<Value>,
    messages: Vec<MessageOut<'a>>,
    draft_attachments: Vec<AttachmentOut<'a>>,
}

#[derive(Serialize)]
struct ThreadOut<'a> {
    #[serde(flatten)]
    thread: &'a ChatThread,
    temporary: bool,
}

#[derive(Serialize)]
struct MessageOut<'a> {
    /// The row's columns, `ir_messages` parsed.
    #[serde(flatten)]
    row: Value,
    attachments: Vec<AttachmentOut<'a>>,
}

#[derive(Serialize)]
struct AttachmentOut<'a> {
    /// The API's metadata; a field added there is in the export.
    #[serde(flatten)]
    meta: Value,
    ord: i64,
    created_at: &'a str,
    extracted: &'a Option<String>,
    data: Base64<'a>,
}

/// Bytes serialized as base64, written as they are encoded.
struct Base64<'a>(&'a [u8]);

impl Serialize for Base64<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&base64::display::Base64Display::new(
            self.0,
            &base64::engine::general_purpose::STANDARD,
        ))
    }
}

impl<'a> JsonExport<'a> {
    fn of(b: &'a Bundle) -> Self {
        let att = |a: &'a Attachment| AttachmentOut {
            meta: serde_json::to_value(&a.meta).expect("attachment meta serializes"),
            ord: a.ord,
            created_at: &a.created_at,
            extracted: &a.full.extracted,
            data: Base64(&a.full.data),
        };
        let messages = b
            .messages
            .iter()
            .map(|m| {
                let mut row = serde_json::to_value(m).expect("ChatMessageRow always serializes");
                if let Some(raw) = m.ir_messages.as_deref() {
                    match serde_json::from_str::<Value>(raw) {
                        Ok(v) => row["ir_messages"] = v,
                        Err(_) => row["ir_messages_unparsed"] = json!(true),
                    }
                }
                MessageOut {
                    row,
                    attachments: b
                        .attachments
                        .iter()
                        .filter(|a| a.full.message_id == Some(m.id))
                        .map(att)
                        .collect(),
                }
            })
            .collect();
        JsonExport {
            format: JSON_FORMAT,
            exported_at: chrono::Utc::now().to_rfc3339(),
            thread: ThreadOut {
                thread: &b.thread,
                temporary: b.thread.id < 0,
            },
            folder: b
                .folder
                .as_ref()
                .map(|f| json!({"id": f.id, "name": f.name})),
            messages,
            draft_attachments: b
                .attachments
                .iter()
                .filter(|a| a.full.message_id.is_none())
                .map(att)
                .collect(),
        }
    }
}

/// A code fence longer than any backtick run in `text`.
fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn fenced(lang: &str, text: &str) -> String {
    let f = fence(text);
    format!("{f}{lang}\n{}\n{f}\n", text.trim_end_matches('\n'))
}

fn human_size(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n:.0} B")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else {
        format!("{:.1} MB", n / 1024.0 / 1024.0)
    }
}

/// Text that goes into a heading or a list line: no line breaks.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn markdown(b: &Bundle) -> String {
    let t = &b.thread;
    let mut o = String::new();
    let title = if t.title.trim().is_empty() {
        "Untitled"
    } else {
        t.title.trim()
    };
    o.push_str(&format!("# {}\n\n", one_line(title)));
    o.push_str(
        "> Exported from lmgw. Attachment bytes are not included here; the JSON export has them \
         (and the extracted text).\n\n",
    );
    let mut meta = vec![
        format!("- **Model:** {}", t.model_alias),
        format!("- **Created:** {}", t.created_at),
        format!("- **Updated:** {}", t.updated_at),
        format!("- **Kind:** {}", t.kind),
    ];
    if let Some(f) = &b.folder {
        meta.push(format!("- **Folder:** {}", one_line(&f.name)));
    }
    if t.id < 0 {
        meta.push("- **Temporary chat** (was never stored)".into());
    }
    let mut params = Vec::new();
    let mut p = |name: &str, v: Option<String>| {
        if let Some(v) = v {
            params.push(format!("{name} {v}"));
        }
    };
    p("temperature", t.temperature.map(|v| v.to_string()));
    p("max_tokens", t.max_tokens.map(|v| v.to_string()));
    p("top_p", t.top_p.map(|v| v.to_string()));
    p("top_k", t.top_k.map(|v| v.to_string()));
    p("min_p", t.min_p.map(|v| v.to_string()));
    p("repeat_penalty", t.repeat_penalty.map(|v| v.to_string()));
    p(
        "presence_penalty",
        t.presence_penalty.map(|v| v.to_string()),
    );
    p(
        "frequency_penalty",
        t.frequency_penalty.map(|v| v.to_string()),
    );
    p("seed", t.seed.map(|v| v.to_string()));
    p(
        "stop",
        (!t.stop.is_empty()).then(|| format!("{:?}", t.stop)),
    );
    p("reasoning", t.reasoning_enabled.map(|v| v.to_string()));
    p("reasoning_effort", t.reasoning_effort.clone());
    p(
        "reasoning_budget",
        t.reasoning_budget.map(|v| v.to_string()),
    );
    if !params.is_empty() {
        meta.push(format!("- **Parameters:** {}", params.join(", ")));
    }
    if !t.mcp_tools.is_empty() {
        let servers: Vec<String> = t
            .mcp_tools
            .iter()
            .map(|m| match &m.allowed_tools {
                Some(list) => format!("{} ({})", m.server_label, list.join(", ")),
                None => m.server_label.clone(),
            })
            .collect();
        meta.push(format!("- **Tools:** {}", servers.join(", ")));
    }
    if !t.kb_ids.is_empty() {
        let mode = match t.kb_mode {
            KbMode::Auto => "auto: searched before every turn",
            KbMode::Tool => "tool: the model searches when it decides to",
        };
        let ids: Vec<String> = t.kb_ids.iter().map(|i| format!("#{i}")).collect();
        let budget = t
            .kb_budget_tokens
            .map(|b| format!(", budget {b} tokens"))
            .unwrap_or_default();
        meta.push(format!(
            "- **Knowledge bases:** {} ({mode}{budget})",
            ids.join(", ")
        ));
    }
    o.push_str(&meta.join("\n"));
    o.push_str("\n\n");
    if !t.system_prompt.trim().is_empty() {
        o.push_str("**System prompt**\n\n");
        o.push_str(&fenced("", &t.system_prompt));
        o.push('\n');
    }
    for m in &b.messages {
        o.push_str("---\n\n");
        // The model that answered this reply, as it was recorded then — not
        // the thread's model now, which a switch or a regenerate on another
        // model has changed since (review R1 item c). A reply saved before
        // that was recorded says no model rather than guess.
        let who = match (m.role.as_str(), m.model.as_deref()) {
            ("user", _) if m.voice.is_some() => "You · spoken".to_string(),
            ("user", _) => "You".to_string(),
            ("assistant", Some(model)) => match m.answered_by.as_deref() {
                Some(by) => format!(
                    "Assistant · {} (answered by {})",
                    one_line(model),
                    one_line(by)
                ),
                None => format!("Assistant · {}", one_line(model)),
            },
            ("assistant", None) => "Assistant".to_string(),
            (other, _) => other.to_string(),
        };
        // A spoken reply says so too (chat-voice design §3).
        let who = if m.role == "assistant" && m.voice.is_some() {
            format!("{who} · spoken")
        } else {
            who
        };
        o.push_str(&format!("## {who}\n\n*{}*\n\n", m.created_at));
        let atts: Vec<&ChatAttachmentFull> = b
            .attachments
            .iter()
            .map(|a| &a.full)
            .filter(|a| a.message_id == Some(m.id))
            .collect();
        if !atts.is_empty() {
            for a in &atts {
                let extracted = if a.extracted.is_some() {
                    " — extracted text not included; use the JSON export"
                } else {
                    ""
                };
                o.push_str(&format!(
                    "- Attachment: {} ({}, {}){extracted}\n",
                    one_line(&a.name),
                    a.kind,
                    human_size(a.data.len() as i64)
                ));
            }
            o.push('\n');
        }
        if !m.reasoning.trim().is_empty() {
            o.push_str(&format!(
                "<details><summary>Thinking</summary>\n\n{}\n\n</details>\n\n",
                m.reasoning.trim()
            ));
        }
        if let Some(ir) = m.ir_messages.as_deref().filter(|s| !s.trim().is_empty()) {
            let pretty = serde_json::from_str::<Value>(ir)
                .and_then(|v| serde_json::to_string_pretty(&v))
                .unwrap_or_else(|_| ir.to_string());
            o.push_str("<details><summary>Tool calls</summary>\n\n");
            o.push_str(&fenced("json", &pretty));
            o.push_str("\n</details>\n\n");
        }
        o.push_str(m.content.trim_end());
        o.push_str("\n\n");
        // What an interrupted reply never said aloud: after the heard text,
        // quoted and marked, as the bubble greys it.
        if let Some(rest) = m
            .voice
            .as_ref()
            .and_then(|v| v.unheard.as_deref())
            .filter(|r| !r.trim().is_empty())
        {
            let quoted: Vec<String> = rest
                .trim()
                .lines()
                .map(|l| format!("> {l}").trim_end().to_string())
                .collect();
            o.push_str(&format!("> *(not heard)*\n>\n{}\n\n", quoted.join("\n")));
        }
        if let Some(c) = m.context.as_ref().filter(|c| !c.excerpts.is_empty()) {
            o.push_str(&format!(
                "<details><summary>Sources ({})</summary>\n\n",
                c.excerpts.len()
            ));
            for (i, e) in c.excerpts.iter().enumerate() {
                let mut line = format!("- [{}] {}", i + 1, one_line(&e.file));
                if let Some(page) = e.page {
                    line.push_str(&format!(" · page {page}"));
                }
                if !e.heading_path.trim().is_empty() {
                    line.push_str(&format!(" · {}", one_line(&e.heading_path)));
                }
                line.push_str(&format!(" · {}\n", one_line(&e.kb)));
                o.push_str(&line);
            }
            o.push_str("\n</details>\n\n");
        }
    }
    let drafts: Vec<&ChatAttachmentFull> = b
        .attachments
        .iter()
        .map(|a| &a.full)
        .filter(|a| a.message_id.is_none())
        .collect();
    if !drafts.is_empty() {
        o.push_str("---\n\n**Unsent attachments**\n\n");
        for a in drafts {
            o.push_str(&format!(
                "- {} ({}, {})\n",
                one_line(&a.name),
                a.kind,
                human_size(a.data.len() as i64)
            ));
        }
    }
    o
}
