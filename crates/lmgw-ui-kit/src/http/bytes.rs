//! lmgw's bytes shown or saved by the page, through its transport: an
//! element's `src` ([`use_src`]) and a download ([`download`]).
//!
//! A transport with a link of its own (the dashboard's `fetch`) hands the
//! browser that URL, so the browser loads it itself, ranges and its cache
//! included, exactly as an `src` attribute did. A transport without one (a
//! host that relays frames) fetches the bytes and shows them by an object
//! URL (`URL.createObjectURL`), which is revoked when the element goes away
//! or shows something else, and after a download has started.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::transport::Http;

/// The object URLs one element showed, so each is revoked exactly once:
/// when the element moves on to another path, when it goes, and when a
/// fetch lands after either.
#[derive(Debug, Default)]
pub struct ObjectUrls {
    /// The newest load; an older one landing late is not shown.
    generation: u64,
    shown: Option<String>,
    gone: bool,
}

impl ObjectUrls {
    /// A new load starts: its ticket, and the URL shown so far to revoke.
    pub fn start(&mut self) -> (u64, Option<String>) {
        self.generation += 1;
        (self.generation, self.shown.take())
    }

    /// Load `ticket` made `url`: `Ok` to show it, `Err` to revoke it at once
    /// (a newer load started, or the element is gone).
    pub fn landed(&mut self, ticket: u64, url: String) -> Result<(), String> {
        if !self.current(ticket) {
            return Err(url);
        }
        self.shown = Some(url);
        Ok(())
    }

    /// Whether load `ticket` is still the one to show (its failure too).
    pub fn current(&self, ticket: u64) -> bool {
        !self.gone && ticket == self.generation
    }

    /// The element is gone: the URL to revoke.
    pub fn dispose(&mut self) -> Option<String> {
        self.gone = true;
        self.shown.take()
    }
}

fn revoke(url: &str) {
    let _ = web_sys::Url::revoke_object_url(url);
}

/// What an element showing lmgw's bytes shows ([`use_src`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Src {
    /// No path: nothing to show.
    #[default]
    None,
    /// The bytes are on their way through the transport.
    Loading,
    /// The URL to show: the transport's link, or an object URL of the bytes.
    Ready(String),
    /// The bytes could not be read, and why (a refusal's message: a missing
    /// file, a relaying host's size bound).
    Failed(String),
}

impl Src {
    /// The URL to show, once there is one.
    pub fn url(&self) -> Option<String> {
        match self {
            Src::Ready(url) => Some(url.clone()),
            _ => None,
        }
    }
}

/// What an element showing `path`'s bytes shows: the transport's link at
/// once, else [`Src::Loading`] until an object URL of the bytes is in, or
/// [`Src::Failed`] with the reason. Follows `path`; call where the element is
/// made, so the URL is revoked when it goes.
pub fn use_src(http: Http, path: Signal<Option<String>>) -> Signal<Src> {
    // A link is there at once, as an `src` attribute was.
    let src = RwSignal::new(
        path.get_untracked()
            .and_then(|p| http.link(&p))
            .map_or(Src::None, Src::Ready),
    );
    let urls = StoredValue::new_local(ObjectUrls::default());
    Effect::new(move |_| {
        let Some(p) = path.get() else {
            if let Some(old) = urls.try_update_value(|u| u.start().1).flatten() {
                revoke(&old);
            }
            src.set(Src::None);
            return;
        };
        if let Some(link) = http.link(&p) {
            src.set(Src::Ready(link));
            return;
        }
        let Some((ticket, old)) = urls.try_update_value(|u| u.start()) else {
            return;
        };
        if let Some(old) = old {
            revoke(&old);
        }
        src.set(Src::Loading);
        leptos::task::spawn_local(async move {
            let made = match http.blob(p).await {
                Ok((blob, _)) => web_sys::Url::create_object_url_with_blob(&blob)
                    .map_err(|_| "the browser would not hold the bytes".to_string()),
                Err(e) => Err(e.to_string()),
            };
            match made {
                Ok(url) => match urls.try_update_value(|u| u.landed(ticket, url.clone())) {
                    Some(Ok(())) => {
                        src.try_set(Src::Ready(url));
                    }
                    // Moved on or gone (or the store with it): never shown.
                    Some(Err(url)) => revoke(&url),
                    None => revoke(&url),
                },
                Err(why) => {
                    if urls.try_with_value(|u| u.current(ticket)) == Some(true) {
                        src.try_set(Src::Failed(why));
                    }
                }
            }
        });
    });
    on_cleanup(move || {
        if let Some(url) = urls.try_update_value(|u| u.dispose()).flatten() {
            revoke(&url);
        }
    });
    src.into()
}

/// Start a download the way a click on `<a download>` does; the dashboard's
/// router leaves a `download` link alone. In the desktop app the shell asks
/// where to save it (a Save As dialog with the server's or `filename`'s
/// name), as it does for every download of the window; a browser decides for
/// itself.
pub fn click_download(href: &str, filename: &str) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Ok(el) = doc.create_element("a") else {
        return;
    };
    let _ = el.set_attribute("href", href);
    let _ = el.set_attribute("download", filename);
    let Ok(a) = el.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    if let Some(body) = doc.body() {
        let _ = body.append_child(&a);
        a.click();
        let _ = body.remove_child(&a);
    }
}

/// How long an object URL handed to a download stays valid. The click only
/// starts the download; the browser reads the URL after it, so revoking at
/// once can cut it off (Firefox). A minute is far past the start of any
/// download of bytes already in memory.
const DOWNLOAD_URL_LIFETIME: std::time::Duration = std::time::Duration::from_secs(60);

/// Download `path` as a file named `fallback` unless the answer names it
/// (`content-disposition`). With a link the browser fetches it itself (the
/// desktop shell's Save As sees the server's name), and the click happens
/// now, inside the caller's click handler, as an `<a download>` of its own
/// did. Without one the bytes come through the transport and are saved from
/// an object URL once they are in; `on_error` hears why when they could not
/// be read.
pub fn download(
    http: Http,
    path: &str,
    fallback: &str,
    on_error: impl FnOnce(super::Error) + 'static,
) {
    if let Some(link) = http.link(path) {
        click_download(&link, fallback);
        return;
    }
    let (path, fallback) = (path.to_string(), fallback.to_string());
    leptos::task::spawn_local(async move {
        if let Err(e) = download_bytes(http, &path, &fallback).await {
            on_error(e);
        }
    });
}

/// [`download`] without a link: the bytes through the transport, saved from
/// an object URL.
async fn download_bytes(http: Http, path: &str, fallback: &str) -> super::Result<()> {
    let (blob, headers) = http.blob(path.to_string()).await?;
    let name = headers
        .iter()
        .find(|(k, _)| k == "content-disposition")
        .and_then(|(_, v)| disposition_filename(v))
        .unwrap_or_else(|| fallback.to_string());
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|_| super::Error::Transport("the browser would not hold the file".into()))?;
    click_download(&url, &name);
    set_timeout(move || revoke(&url), DOWNLOAD_URL_LIFETIME);
    Ok(())
}

/// The file name a `content-disposition` value gives: `filename*`
/// (RFC 5987, UTF-8) before `filename`. Only its last path part is kept.
pub fn disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_params(value) {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "filename*" {
            // charset'lang'percent-encoded
            let mut it = v.splitn(3, '\'');
            let (charset, _lang, enc) = (it.next()?, it.next()?, it.next()?);
            if charset.eq_ignore_ascii_case("utf-8") {
                extended = percent_decode(enc);
            }
        } else if k == "filename" {
            plain = Some(unquote(v));
        }
    }
    let name = extended.or(plain)?;
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// `;`-separated parameters, a `;` inside quotes kept.
fn split_params(value: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let (mut quoted, mut escaped) = (false, false);
    for c in value.chars() {
        let cur = out.last_mut().expect("never empty");
        if escaped {
            cur.push(c);
            escaped = false;
        } else if c == '\\' && quoted {
            cur.push(c);
            escaped = true;
        } else if c == '"' {
            cur.push(c);
            quoted = !quoted;
        } else if c == ';' && !quoted {
            out.push(String::new());
        } else {
            cur.push(c);
        }
    }
    out
}

fn unquote(v: &str) -> String {
    let Some(inner) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return v.to_string();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `path`'s bytes as a `File` named `name`, through `http` (the Chat's
/// Attach on a produced picture: the page's transport; the Audio lab's kept
/// uploads: [`Http::fetch`]): on the browser's fetch the response's own
/// blob, never in the wasm module's memory; through a relaying host the
/// bytes it relayed. A refusal reads as every call's (a `401
/// session_required` runs the host's hook).
pub async fn file_of(
    http: Http,
    path: &str,
    name: &str,
    mime: &str,
) -> Result<web_sys::File, String> {
    let (blob, _) = http.blob(path).await.map_err(|e| e.to_string())?;
    let opts = web_sys::FilePropertyBag::new();
    opts.set_type(mime);
    web_sys::File::new_with_blob_sequence_and_options(&js_sys::Array::of1(&blob), name, &opts)
        .map_err(|_| "the browser would not make a file of it".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_object_url_is_revoked_once_whatever_lands_when() {
        let mut u = ObjectUrls::default();
        // Shown, then replaced: the first is handed back for revoking.
        let (a, none) = u.start();
        assert_eq!(none, None);
        assert_eq!(u.landed(a, "blob:1".into()), Ok(()));
        let (b, old) = u.start();
        assert_eq!(old.as_deref(), Some("blob:1"));
        // A third load starts before the second landed: the second is
        // revoked at once, never shown.
        let (c, old) = u.start();
        assert_eq!(old, None);
        assert_eq!(u.landed(b, "blob:2".into()), Err("blob:2".into()));
        assert_eq!(u.landed(c, "blob:3".into()), Ok(()));
        // The element goes: the shown one is revoked, and a load landing
        // after that is revoked at once.
        let (d, old) = u.start();
        assert_eq!(old.as_deref(), Some("blob:3"));
        assert_eq!(u.dispose(), None);
        assert!(
            !u.current(d),
            "a failure after the element went is not shown"
        );
        assert_eq!(u.landed(d, "blob:4".into()), Err("blob:4".into()));
        assert_eq!(u.dispose(), None);
    }

    #[test]
    fn only_the_newest_load_may_fail_visibly() {
        let mut u = ObjectUrls::default();
        let (a, _) = u.start();
        assert!(u.current(a));
        let (b, _) = u.start();
        assert!(
            !u.current(a),
            "an older load's failure is not the element's"
        );
        assert!(u.current(b));
        assert_eq!(Src::Failed("413".into()).url(), None);
        assert_eq!(Src::Ready("blob:1".into()).url().as_deref(), Some("blob:1"));
        assert_eq!(Src::default(), Src::None);
    }

    #[test]
    fn a_shown_url_is_revoked_when_the_element_goes() {
        let mut u = ObjectUrls::default();
        let (t, _) = u.start();
        u.landed(t, "blob:x".into()).unwrap();
        assert_eq!(u.dispose().as_deref(), Some("blob:x"));
        assert_eq!(u.dispose(), None, "only once");
    }

    #[test]
    fn the_answer_names_the_download() {
        assert_eq!(
            disposition_filename(r#"attachment; filename="lmgw chat.md""#).as_deref(),
            Some("lmgw chat.md")
        );
        assert_eq!(
            disposition_filename(
                r#"attachment; filename="Gr_n.md"; filename*=UTF-8''Gr%C3%BCn%20%3B.md"#
            )
            .as_deref(),
            Some("Grün ;.md")
        );
        assert_eq!(
            disposition_filename(r#"attachment; filename="a\"b;c.zip""#).as_deref(),
            Some("a\"b;c.zip")
        );
        assert_eq!(
            disposition_filename("attachment; filename=../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(disposition_filename("inline"), None);
        assert_eq!(disposition_filename(r#"attachment; filename="""#), None);
    }
}
