//! A tool's input file and where its output goes (chat-media-tools design
//! §3.6): a model cannot pass bytes, so a Chat thread's files get a ref,
//! **`att:N`**, `N` the attachment's id. A ref is resolved by the call,
//! never trusted: it must name an attachment of the turn's own thread, of
//! the kind the tool takes. `data:` URLs are taken everywhere; `http(s)`
//! URLs are refused — lmgw does not fetch for tools.
//!
//! [`Refs`] is the caller's side of it: a Chat thread's files
//! (`web::chat_media`), or [`NoThread`] for `/mcp`, `/v1/responses`,
//! agents and realtime, where `att:N` names nothing.

use base64::Engine as _;

use crate::erased::Erased;

use super::result::Produced;

/// An input file, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub mime: String,
    pub name: String,
    pub bytes: Vec<u8>,
    /// The attachment it came from, when a ref named one.
    pub id: Option<i64>,
    /// The model a media call made it with (its `meta.media.model`), when
    /// one did: the row an upscale prefers (design §9).
    pub made_by: Option<String>,
}

/// Where a call's refs resolve and its files go.
pub(crate) trait Refs: Send + Sync {
    /// Attachment `id` of the caller's thread, of `kind` (`image`, `audio`),
    /// with its bytes; `Err` is the refusal the model reads.
    fn attachment<'a>(&'a self, id: i64, kind: &'a str) -> Erased<'a, Result<Source, String>>;

    /// Store what call `call_id` made, as the thread's attachments; their
    /// ids, in order. `None`: no thread — the files go back in the result.
    fn store<'a>(
        &'a self,
        call_id: &'a str,
        files: &'a [Produced],
    ) -> Erased<'a, Option<Result<Vec<i64>, String>>>;
}

/// A caller with no thread.
pub(crate) struct NoThread;

/// Its refusal of `att:N`.
pub(crate) const NO_THREAD: &str =
    "refs like att:N name a Chat conversation's files, and this call has none: pass the file as \
     a data: URL here";

impl Refs for NoThread {
    fn attachment<'a>(&'a self, _id: i64, _kind: &'a str) -> Erased<'a, Result<Source, String>> {
        Box::pin(async { Err(NO_THREAD.to_string()) })
    }

    fn store<'a>(
        &'a self,
        _call_id: &'a str,
        _files: &'a [Produced],
    ) -> Erased<'a, Option<Result<Vec<i64>, String>>> {
        Box::pin(async { None })
    }
}

/// An `att:N` (or a bare number, read as one).
pub(crate) fn attachment_id(raw: &str) -> Option<i64> {
    let t = raw.trim();
    let n = t
        .strip_prefix("att:")
        .or_else(|| t.strip_prefix("att"))
        .unwrap_or(t);
    n.trim().parse().ok()
}

/// Resolve `raw` (argument `arg`) to a file of `kind` (`image`, `audio`).
pub(super) async fn resolve(
    refs: &dyn Refs,
    raw: &str,
    kind: &str,
    arg: &str,
) -> Result<Source, String> {
    let t = raw.trim();
    if let Some(id) = attachment_id(t) {
        return refs.attachment(id, kind).await;
    }
    if t.starts_with("data:") {
        return data_url(t, kind, arg);
    }
    if t.starts_with("http://") || t.starts_with("https://") {
        return Err(format!(
            "argument '{arg}' is a URL, and lmgw does not fetch URLs for tools: pass att:N from \
             this conversation or a data: URL"
        ));
    }
    Err(format!(
        "argument '{arg}' is neither att:N from this conversation nor a data: URL"
    ))
}

/// A `data:<mime>;base64,<bytes>` URL of `kind`.
fn data_url(t: &str, kind: &str, arg: &str) -> Result<Source, String> {
    let wrong = || format!("argument '{arg}' is not a base64 data: URL (data:<type>;base64,...)");
    let (head, b64) = t
        .strip_prefix("data:")
        .and_then(|r| r.split_once(','))
        .ok_or_else(wrong)?;
    let mime = head.strip_suffix(";base64").ok_or_else(wrong)?.trim();
    if !mime.starts_with(&format!("{kind}/")) {
        return Err(format!(
            "argument '{arg}' holds {}, and this tool takes {kind}",
            if mime.is_empty() { "no type" } else { mime }
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| wrong())?;
    let ext = mime.rsplit('/').next().unwrap_or(kind);
    Ok(Source {
        mime: mime.to_string(),
        name: format!("{kind}.{ext}"),
        bytes,
        id: None,
        made_by: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_parse_in_both_spellings() {
        assert_eq!(attachment_id("att:57"), Some(57));
        assert_eq!(attachment_id(" 57 "), Some(57));
        assert_eq!(attachment_id("att:x"), None);
        assert_eq!(attachment_id("data:image/png;base64,AA=="), None);
    }

    #[tokio::test]
    async fn a_thread_less_call_takes_data_urls_and_refuses_the_rest() {
        let pic = resolve(&NoThread, "data:image/png;base64,AQID", "image", "image")
            .await
            .unwrap();
        assert_eq!(pic.bytes, vec![1, 2, 3]);
        assert_eq!(pic.mime, "image/png");
        let e = resolve(&NoThread, "att:3", "image", "image")
            .await
            .unwrap_err();
        assert!(e.contains("data: URL"), "{e}");
        let e = resolve(&NoThread, "https://example.com/a.png", "image", "image")
            .await
            .unwrap_err();
        assert!(e.contains("does not fetch"), "{e}");
        let e = resolve(&NoThread, "data:audio/wav;base64,AQID", "image", "image")
            .await
            .unwrap_err();
        assert!(e.contains("takes image"), "{e}");
    }
}
