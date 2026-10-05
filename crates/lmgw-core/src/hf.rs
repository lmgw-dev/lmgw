//! Hugging Face model manager (§8): browse GGUF files in a repo, resolve
//! download URLs, and check for updates by comparing the resolve-URL ETag
//! against the one recorded at download time.
//!
//! Durable state (status, etag, size) lives in the `hf_models` table. The
//! transfer itself is the `hf_download` job kind
//! ([`crate::jobs::hf_download`]) — this module used to carry its own
//! spawn-and-track machinery, which the generalized Jobs subsystem replaced.

use serde::Deserialize;

use crate::state::SharedState;
use crate::store::{self, HfModelRow};

mod listing;
mod resolve;
pub use listing::{list_repo_files, list_tree, ListFailure};
pub use resolve::{get_with_commit, listing_refusal, resolve_url};

pub const HF_BASE: &str = "https://huggingface.co";

/// The four download targets, one per managed container class.
///
/// `image` joined the other three with WP3 of the image-generation design
/// (§7.1): migration 0034 had already widened `hf_models.target`'s CHECK for
/// it, and what was missing was the downloader's side — an image pipeline is
/// mostly `.safetensors` spread across several repos, so "GGUF-only" could
/// not stand for this class. Accepted file kinds are therefore per target
/// ([`accepted_extensions`]); everything else about a transfer is identical.
pub const TARGETS: [&str; 4] = ["chat", "aux", "audio", "image"];

/// Canonical target name, accepting the pre-rename `embed` spelling for the
/// aux container. Anything else is an error rather than a silent fall back to
/// `chat`: a typo used to write the file into the wrong models dir and only
/// show up as a missing GGUF at apply time.
pub fn normalize_target(target: &str) -> Result<&'static str, String> {
    match target.trim() {
        "embed" | "aux" => Ok("aux"),
        "chat" => Ok("chat"),
        "audio" => Ok("audio"),
        "image" => Ok("image"),
        other => Err(format!(
            "unknown download target '{other}' (expected {})",
            TARGETS.join(", ")
        )),
    }
}

/// The file extensions a target's models dir accepts.
///
/// GGUF-only for the three llama.cpp/audio.cpp classes — unchanged, byte for
/// byte, from before the image class existed. The image class takes the whole
/// stable-diffusion.cpp set (design §2.6): every VAE and CLIP encoder a
/// pipeline needs is a `.safetensors`, and an all-in-one SD/SDXL checkpoint is
/// a `.safetensors` or a `.ckpt`, so a GGUF-only gate would make the class
/// undownloadable.
pub fn accepted_extensions(target: &str) -> &'static [&'static str] {
    match target {
        "image" => &[".gguf", ".safetensors", ".ckpt", ".pt", ".pth"],
        _ => &[".gguf"],
    }
}

/// Whether a repo path is a weights file this target will store.
pub fn accepts_file(target: &str, path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    accepted_extensions(target)
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// The accepted kinds as a phrase for an error or a tool description
/// (`.gguf` / `.gguf, .safetensors, .ckpt, .pt, .pth`).
pub fn accepted_extensions_phrase(target: &str) -> String {
    accepted_extensions(target).join(", ")
}

/// Models dir for a download target: `aux` writes into the aux router's dir,
/// `audio` into the audio.cpp container's dir, `image` into the
/// stable-diffusion.cpp class's dir, anything else (the `chat` default) into
/// the chat router's dir.
pub fn models_dir_for_target(settings: &crate::config::Settings, target: &str) -> String {
    match target {
        "aux" | "embed" => settings.aux_router.models_dir.clone(),
        "audio" => settings.audio.models_dir.clone(),
        "image" => settings.image.models_dir.clone(),
        _ => settings.router.models_dir.clone(),
    }
}

/// The settings field a target's models dir is stored under — the name to put
/// in a refusal, so "not configured" says what to configure.
/// Where the dashboard sets that directory: its Settings → Runtimes section.
pub fn runtimes_section(target: &str) -> &'static str {
    match target {
        "aux" | "embed" => "Aux",
        "audio" => "Audio",
        "image" => "Image",
        _ => "Chat",
    }
}

pub fn models_dir_setting(target: &str) -> &'static str {
    match target {
        "aux" | "embed" => "aux_router.models_dir",
        "audio" => "audio.models_dir",
        "image" => "image.models_dir",
        _ => "router.models_dir",
    }
}

/// [`models_dir_for_target`], refusing an unset one by the setting's name.
pub fn models_dir_or_refuse(
    settings: &crate::config::Settings,
    target: &str,
) -> Result<String, String> {
    let dir = models_dir_for_target(settings, target);
    if dir.trim().is_empty() {
        return Err(format!(
            "the {target} models directory is not configured, so there is nowhere to download \
             into — set `{}` first (Settings → Runtimes → {})",
            models_dir_setting(target),
            runtimes_section(target)
        ));
    }
    Ok(dir)
}

/// [`models_dir_or_refuse`] for a write into it: a dev instance also refuses a
/// models dir outside its own data dir ([`SharedState`]'s
/// `refuse_shared_models_dir`), so a dev copy's download, retry or delete
/// never lands in the installed app's tree.
pub fn models_dir_to_write(state: &SharedState, target: &str) -> Result<String, String> {
    let dir = models_dir_or_refuse(&state.snapshot().settings, target)?;
    state.refuse_shared_models_dir(std::path::Path::new(&dir))?;
    Ok(dir)
}

/// The sentence a gated repo gets instead of a bare HTTP status (design §2.7).
///
/// `black-forest-labs/FLUX.1-schnell` answers **401** to an unauthenticated
/// tree listing and to a file GET alike (measured, §12.1), and a token that
/// has not accepted the licence gets **403**. Reporting "HF API 401" left the
/// owner guessing; this names the two things that have to be true.
pub fn gated_message(repo: &str) -> String {
    format!(
        "gated repo — set the Hugging Face token under Settings → Tokens & updates and accept \
         the licence on the hub (huggingface.co/{repo})"
    )
}

/// The sentence a **rejected token** gets. A 401 *with* a token on the request
/// is not a gated repo — it is an expired, revoked or mistyped token, and
/// telling the owner to accept a licence they have already accepted sends them
/// to the wrong page (the hub's own reason is in the body, so it is quoted).
pub fn rejected_token_message(status: reqwest::StatusCode, body: &str) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    format!(
        "the configured Hugging Face token was rejected ({}){} — check the Hugging Face token \
         under Settings → Tokens & updates",
        status.as_u16(),
        if first.is_empty() {
            String::new()
        } else {
            format!(": {first}")
        }
    )
}

/// The sentence a failed hub response deserves, or `None` when the status is
/// just a status: the gated advice, or [`rejected_token_message`] when a token
/// was actually sent.
pub fn hub_refusal(
    status: reqwest::StatusCode,
    body: &str,
    repo: &str,
    token_sent: bool,
) -> Option<String> {
    if token_sent && status == reqwest::StatusCode::UNAUTHORIZED {
        return Some(rejected_token_message(status, body));
    }
    is_gated_response(status, body, token_sent).then(|| gated_message(repo))
}

/// Whether a failed hub response is the gated case: an **unauthenticated** 401
/// always, 403 only when the body says so.
///
/// A 403 is also what a rate limit and a plain permission error return, so it
/// is not turned into licence advice on the status alone — the hub names the
/// reason in the body (`"Access to model … is restricted"`, `gated`,
/// `awaiting a review`), and that is what decides. A 401 *with* a token is not
/// this case at all ([`rejected_token_message`]).
pub fn is_gated_response(status: reqwest::StatusCode, body: &str, token_sent: bool) -> bool {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return !token_sent;
    }
    if status != reqwest::StatusCode::FORBIDDEN {
        return false;
    }
    let body = body.to_ascii_lowercase();
    ["gated", "restricted", "accept", "licen", "awaiting"]
        .iter()
        .any(|m| body.contains(m))
}

/// Hub base URL; honors the ecosystem-standard `HF_ENDPOINT` override
/// (mirrors; also how tests point at a mock hub).
pub fn hf_base() -> String {
    std::env::var("HF_ENDPOINT")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HF_BASE.to_string())
}

// ---------------------------------------------------------------------------
// Repo/file naming (pure; unit-tested)
// ---------------------------------------------------------------------------

fn valid_name_part(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && s != "."
        && s != ".."
}

/// `owner/name`, both parts restricted to HF's id charset.
pub fn validate_repo(repo: &str) -> Result<(), String> {
    match repo.split_once('/') {
        Some((owner, name)) if valid_name_part(owner) && valid_name_part(name) => Ok(()),
        _ => Err(format!("invalid repo id `{repo}` (expected owner/name)")),
    }
}

/// A revision as it goes into a hub URL path: a branch, tag or commit name
/// without `/` (no `refs/pr/…`), which every revision lmgw asks for is —
/// `main`, or the commit a spec pins.
pub fn validate_revision(revision: &str) -> Result<(), String> {
    match valid_name_part(revision) {
        true => Ok(()),
        false => Err(format!("invalid revision `{revision}`")),
    }
}

/// Path of a repo file inside the models dir: `<owner>/<name>/<file>`.
/// Rejects anything that could escape the models dir.
pub fn dest_rel_path(repo: &str, file: &str) -> Result<String, String> {
    validate_repo(repo)?;
    if file.is_empty() || file.starts_with('/') {
        return Err(format!("invalid file path `{file}`"));
    }
    for part in file.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("invalid file path `{file}`"));
        }
    }
    Ok(format!("{repo}/{file}"))
}

/// Split GGUFs are named `…-00001-of-00005.gguf`; selecting any part means
/// downloading all of them. Returns the parts of `file` that exist in
/// `available` (sorted), or just `file` when it is not split.
pub fn expand_parts(file: &str, available: &[HfFile]) -> Vec<String> {
    let Some((prefix, _, total)) = split_part_name(file) else {
        return vec![file.to_string()];
    };
    (1..=total)
        .map(|i| format!("{prefix}-{i:05}-of-{total:05}.gguf"))
        .filter(|p| available.iter().any(|f| &f.path == p))
        .collect()
}

/// One shard of a **sharded `.safetensors`** (`…-00001-of-00003.safetensors`)
/// as `(n, total)`, or `None` for a single-file one.
///
/// Not an `expand_parts` case: the split-GGUF rule cannot be extended to this
/// format, because sd-server loads one `.safetensors` file and has no
/// equivalent of llama.cpp's split loader — fetching every shard would fill
/// the models dir with files nothing can open. Naming the shape is all lmgw
/// can honestly do, so [`crate::ops::hf_add`] warns and queues what was asked
/// for.
pub fn safetensors_shard(file: &str) -> Option<(u32, u32)> {
    let stem = file.strip_suffix(".safetensors")?;
    let (rest, total) = stem.rsplit_once("-of-")?;
    let total: u32 = (total.len() == 5).then(|| total.parse().ok()).flatten()?;
    let (_, n) = rest.rsplit_once('-')?;
    let n: u32 = (n.len() == 5).then(|| n.parse().ok()).flatten()?;
    Some((n, total))
}

/// Parse `…-NNNNN-of-MMMMM.gguf` into (prefix, n, total).
pub fn split_part_name(file: &str) -> Option<(&str, u32, u32)> {
    let stem = file.strip_suffix(".gguf")?;
    let (rest, total) = stem.rsplit_once("-of-")?;
    let total: u32 = (total.len() == 5).then(|| total.parse().ok()).flatten()?;
    let (prefix, n) = rest.rsplit_once('-')?;
    let n: u32 = (n.len() == 5).then(|| n.parse().ok()).flatten()?;
    Some((prefix, n, total))
}

/// Whether `a` and `b` name the same file, or two shards of one split GGUF:
/// a spec may list one shard, and a download fetches every sibling
/// ([`expand_parts`]), so a tracked shard belongs to whatever lists its set.
pub fn same_split_set(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (split_part_name(a), split_part_name(b)) {
        (Some((pa, _, ta)), Some((pb, _, tb))) => pa == pb && ta == tb,
        _ => false,
    }
}

/// Default local-model id for a downloaded file: stem without the split-part
/// suffix (`a/b/foo-00001-of-00002.gguf` → `foo`).
pub fn suggest_model_id(file: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    match split_part_name(base) {
        Some((prefix, _, _)) => prefix.to_string(),
        None => base.strip_suffix(".gguf").unwrap_or(base).to_string(),
    }
}

/// All GGUF files under the models dir, as sorted paths relative to it
/// (the form `gguf_path` is stored in). Skips hidden entries and `.part`
/// in-progress downloads; depth-limited against pathological trees.
pub fn scan_gguf_files(models_dir: &str) -> Vec<String> {
    scan_model_files(models_dir, "chat")
}

/// [`scan_gguf_files`] for one target's accepted kinds: identical for the
/// three GGUF-only classes, and every `.safetensors` / `.ckpt` / `.pt` /
/// `.pth` as well for `image` — a pipeline's VAE and text encoders are
/// exactly the files a GGUF-only scan cannot see, and the listing they feed
/// (`lmgw__gguf_files target=image`) is how a caller learns the relative path
/// an image row's `files` values take.
pub fn scan_model_files(models_dir: &str, target: &str) -> Vec<String> {
    fn walk(dir: &std::path::Path, rel: &str, depth: u8, exts: &[&str], out: &mut Vec<String>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let child = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            let path = entry.path();
            if path.is_dir() {
                walk(&path, &child, depth - 1, exts, out);
            } else {
                let lower = name.to_ascii_lowercase();
                if exts.iter().any(|e| lower.ends_with(e)) {
                    out.push(child);
                }
            }
        }
    }
    let mut out = Vec::new();
    if !models_dir.trim().is_empty() {
        walk(
            std::path::Path::new(models_dir),
            "",
            6,
            accepted_extensions(target),
            &mut out,
        );
    }
    out.sort();
    out
}

/// "1.5 GiB"-style sizes for the UI.
pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// HF API
// ---------------------------------------------------------------------------

/// One entry from the repo tree API.
#[derive(Debug, Clone, Deserialize)]
pub struct HfFile {
    pub path: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(rename = "type", default)]
    pub kind: String,
}

/// The `rel="next"` target of a `Link` header (RFC 8288), if it names one.
pub fn next_link(header: &str) -> Option<String> {
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let close = open + rest[open..].find('>')?;
        let target = &rest[open + 1..close];
        let after = &rest[close + 1..];
        let params = &after[..after.find('<').unwrap_or(after.len())];
        let is_next = params.split(';').any(|p| {
            let p = p.trim().trim_end_matches(',').trim();
            p.split_once('=').is_some_and(|(k, v)| {
                k.trim().eq_ignore_ascii_case("rel")
                    && v.trim()
                        .trim_matches('"')
                        .split_whitespace()
                        .any(|r| r.eq_ignore_ascii_case("next"))
            })
        });
        if is_next {
            return Some(target.to_string());
        }
        rest = &after[params.len()..];
    }
    None
}

/// Seconds until the hub's rate-limit window resets: the `t=` of its
/// `RateLimit` header (`"api";r=0;t=55`), else a `Retry-After` in seconds.
pub fn ratelimit_reset(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let from_ratelimit = headers
        .get("ratelimit")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .find_map(|p| p.trim().strip_prefix("t="))
                .and_then(|t| t.trim().parse().ok())
        });
    from_ratelimit.or_else(|| {
        headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    })
}

pub fn parse_tree_json(json: &str) -> Result<Vec<HfFile>, String> {
    let entries: Vec<HfFile> =
        serde_json::from_str(json).map_err(|e| format!("HF API response: {e}"))?;
    Ok(entries.into_iter().filter(|f| f.kind == "file").collect())
}

/// `W/"abc"` / `"abc"` → `abc`.
pub fn normalize_etag(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_string()
}

pub(crate) fn etag_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    ["x-linked-etag", "etag"]
        .iter()
        .find_map(|h| headers.get(*h))
        .and_then(|v| v.to_str().ok())
        .map(normalize_etag)
        .filter(|s| !s.is_empty())
}

/// Current ETag of a repo file at `revision` (HEAD on the resolve URL,
/// redirects followed).
pub async fn remote_etag(
    http: &reqwest::Client,
    token: &str,
    repo: &str,
    revision: &str,
    file: &str,
) -> Result<Option<String>, String> {
    let url = resolve_url(repo, revision, file);
    let mut rb = http.head(&url);
    if !token.is_empty() {
        rb = rb.bearer_auth(token);
    }
    let resp = rb
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("HEAD {url}: {e}"))?;
    if !resp.status().is_success() {
        // No body to read on a HEAD, so only the unambiguous 401 becomes a
        // sentence — the licence one when nothing was sent to authenticate
        // with, the rejected-token one when something was.
        if let Some(sentence) = hub_refusal(resp.status(), "", repo, !token.is_empty()) {
            return Err(sentence);
        }
        return Err(format!("HEAD {url}: {}", resp.status()));
    }
    Ok(etag_from_headers(resp.headers()))
}

/// Compare the remote ETag against the recorded one; flags the row
/// `update_available` when they differ. Returns whether an update was found.
///
/// The revision compared against is [`crate::audio::pins::tracked_revision`]:
/// under `audio.catalog_revision = pinned` an audio catalog file is compared
/// with the commit its spec pins now, so a pin the spec moved is offered and
/// no update the spec does not endorse is; under `latest`, and for every
/// other row, against `main`.
pub async fn check_update(state: &SharedState, row: &HfModelRow) -> Result<bool, String> {
    let catalog = match row.target == "audio" {
        true => crate::web::audio::catalog_cached(state).await,
        false => None,
    };
    let mode = state.snapshot().settings.audio.catalog_revision;
    let revision = crate::audio::pins::tracked_revision(row, catalog.as_ref(), mode);
    check_update_at(state, row, revision).await
}

/// [`check_update`] against `revision`.
///
/// A row flagged earlier whose file now matches the remote one goes back to
/// `done`: the flag came from a check against another revision (one taken
/// under `latest`, before the setting went back to `pinned`), and Update
/// would fetch the same bytes again. A remote that names no ETag clears
/// nothing — that is "cannot tell", not "the same".
///
/// A file that matches at another revision than the row's (a pin the spec
/// moved, with this file unchanged between the two) is that revision's
/// file: the row records it, so nothing goes on saying an update would
/// take a pin that offers no update.
pub async fn check_update_at(
    state: &SharedState,
    row: &HfModelRow,
    revision: &str,
) -> Result<bool, String> {
    let token = state.snapshot().settings.hf_token.clone();
    let remote = remote_etag(&state.http, &token, &row.repo, revision, &row.file).await?;
    let changed = match (&row.etag, &remote) {
        (Some(local), Some(remote)) => local != remote,
        (None, Some(_)) => true,
        (_, None) => false, // no remote etag → can't tell
    };
    let same = matches!((&row.etag, &remote), (Some(l), Some(r)) if l == r);
    let status = match (changed, same) {
        (true, _) => Some("update_available"),
        (false, true) if row.status == "update_available" => Some("done"),
        _ => None,
    };
    if let Some(status) = status {
        store::set_hf_status(&state.db, row.id, status, None)
            .await
            .map_err(|e| e.to_string())?;
    }
    if same {
        record_matched(state, row, revision).await?;
    }
    Ok(changed)
}

/// The row's file has the same ETag at `revision` as the one it recorded:
/// it is that revision's file, and the row tracks it (and, when `revision`
/// is a commit, names it as where the bytes came from). Nothing to do when
/// the row tracks `revision` already.
pub(crate) async fn record_matched(
    state: &SharedState,
    row: &HfModelRow,
    revision: &str,
) -> Result<(), String> {
    if revision == row.revision() {
        return Ok(());
    }
    let commit = crate::audio::pins::is_commit(revision).then(|| revision.to_ascii_lowercase());
    store::set_hf_revision_matched(&state.db, row.id, revision, commit.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<HfFile> {
        paths
            .iter()
            .map(|p| HfFile {
                path: p.to_string(),
                size: None,
                kind: "file".into(),
            })
            .collect()
    }

    #[test]
    fn dest_path_is_repo_scoped_and_safe() {
        assert_eq!(
            dest_rel_path(
                "unsloth/gemma-4-31B-it-GGUF",
                "MTP/gemma-4-31B-it-Q8_0-MTP.gguf"
            )
            .unwrap(),
            "unsloth/gemma-4-31B-it-GGUF/MTP/gemma-4-31B-it-Q8_0-MTP.gguf"
        );
        assert!(dest_rel_path("noslash", "a.gguf").is_err());
        assert!(dest_rel_path("a/b", "../escape.gguf").is_err());
        assert!(dest_rel_path("a/b", "x/../../escape.gguf").is_err());
        assert!(dest_rel_path("a/b", "/abs.gguf").is_err());
        assert!(dest_rel_path("a/../b", "x.gguf").is_err());
        assert!(dest_rel_path("a/b c", "x.gguf").is_err());
    }

    #[test]
    fn split_parts_expand_to_available_siblings() {
        let avail = files(&[
            "q4/m-00001-of-00003.gguf",
            "q4/m-00002-of-00003.gguf",
            "q4/m-00003-of-00003.gguf",
            "other.gguf",
        ]);
        assert_eq!(
            expand_parts("q4/m-00002-of-00003.gguf", &avail),
            vec![
                "q4/m-00001-of-00003.gguf",
                "q4/m-00002-of-00003.gguf",
                "q4/m-00003-of-00003.gguf",
            ]
        );
        assert_eq!(expand_parts("other.gguf", &avail), vec!["other.gguf"]);
        // Missing siblings are skipped rather than invented.
        let partial = files(&["m-00001-of-00002.gguf"]);
        assert_eq!(
            expand_parts("m-00001-of-00002.gguf", &partial),
            vec!["m-00001-of-00002.gguf"]
        );
    }

    #[test]
    fn model_id_suggestions_strip_dirs_and_part_suffixes() {
        assert_eq!(suggest_model_id("a/b/foo-00001-of-00002.gguf"), "foo");
        assert_eq!(
            suggest_model_id("gemma-4-31B-it-qat-UD-Q4_K_XL.gguf"),
            "gemma-4-31B-it-qat-UD-Q4_K_XL"
        );
    }

    #[test]
    fn tree_json_parses_files_only() {
        let json = r#"[
            {"type":"file","path":"a.gguf","size":123,"oid":"x"},
            {"type":"directory","path":"MTP"},
            {"type":"file","path":"MTP/b.gguf","size":456}
        ]"#;
        let f = parse_tree_json(json).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].path, "a.gguf");
        assert_eq!(f[0].size, Some(123));
        assert_eq!(f[1].path, "MTP/b.gguf");
    }

    #[test]
    fn link_headers_name_the_next_page() {
        let h = r#"<https://huggingface.co/api/models/a/b/tree/main?recursive=true&cursor=eyJm>; rel="next""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://huggingface.co/api/models/a/b/tree/main?recursive=true&cursor=eyJm")
        );
        // Several links, the next one not first; params in any case/spacing.
        let h = r#"<https://h/p1>; rel="prev", <https://h/p3>;REL = "next last""#;
        assert_eq!(next_link(h).as_deref(), Some("https://h/p3"));
        assert_eq!(
            next_link("<https://h/p3>; rel=next").as_deref(),
            Some("https://h/p3")
        );
        // The last page: no next.
        assert_eq!(next_link(r#"<https://h/p1>; rel="prev""#), None);
        assert_eq!(next_link(""), None);
        assert_eq!(next_link("<broken; rel=next"), None);
    }

    #[test]
    fn the_rate_limit_reset_comes_from_the_hub_headers() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut h = HeaderMap::new();
        assert_eq!(ratelimit_reset(&h), None);
        h.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(ratelimit_reset(&h), Some(30));
        h.insert("ratelimit", HeaderValue::from_static("\"api\";r=0;t=55"));
        assert_eq!(ratelimit_reset(&h), Some(55), "the hub's own header wins");
    }

    #[test]
    fn etags_normalize() {
        assert_eq!(normalize_etag("W/\"abc\""), "abc");
        assert_eq!(normalize_etag("\"abc\""), "abc");
        assert_eq!(normalize_etag("abc"), "abc");
    }

    #[test]
    fn bytes_format_human_readable() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(17_000_000_000), "15.8 GiB");
    }
}
