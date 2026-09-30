//! Chat search (chat-complete §4), the sidebar half: after the filter box has
//! been idle for [`IDLE_MS`] with at least [`MIN_CHARS`] characters, a
//! **Messages** section under the thread list shows the server's hits with
//! snippets. A hit opens `?t=<id>&m=<message_id>`: the page scrolls that
//! message into view and flashes it ([`install_reveal`]).

use std::time::Duration;

use leptos::prelude::*;
use serde::Deserialize;

use super::chat::Msg;
use super::chat_folders::FolderInfo;
use crate::scope::{Latest, Scope};

/// Idle time before the server is asked.
const IDLE_MS: u64 = 300;
/// The server refuses fewer characters (`SEARCH_MIN_CHARS`); asking would only
/// produce its 400.
const MIN_CHARS: usize = 2;
/// How long a revealed message stays flashed.
const FLASH_MS: u64 = 1500;
/// The server's match markers (private-use characters).
const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
struct Hit {
    kind: String,
    message_id: Option<i64>,
    role: Option<String>,
    snippet: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
struct SearchThread {
    thread_id: i64,
    title: String,
    folder_id: Option<i64>,
    archived: bool,
    match_count: i64,
    hits: Vec<Hit>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
struct SearchResp {
    total_threads: i64,
    threads: Vec<SearchThread>,
    next_offset: Option<i64>,
}

/// Is `q` long enough to ask the server?
fn searchable(q: &str) -> bool {
    q.trim().chars().count() >= MIN_CHARS
}

fn search_url(q: &str, archived: bool, offset: i64) -> String {
    let q = js_sys::encode_uri_component(q.trim());
    let arch = if archived { "all" } else { "0" };
    format!("/chat/api/search?q={q}&archived={arch}&offset={offset}")
}

/// A snippet as HTML: the text escaped first, then the server's markers
/// turned into `<mark>` — so nothing in a message can inject markup.
pub(super) fn snippet_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            OPEN => out.push_str("<mark>"),
            CLOSE => out.push_str("</mark>"),
            c => out.push(c),
        }
    }
    out
}

fn who(h: &Hit) -> &'static str {
    match (h.kind.as_str(), h.role.as_deref()) {
        ("t", _) => "title",
        ("a", _) => "file",
        (_, Some("user")) => "you",
        (_, Some("assistant")) => "reply",
        _ => "message",
    }
}

/// The **Messages** section. `on_pick(thread, message)` opens a hit.
#[component]
pub(super) fn MessageHits(
    query: RwSignal<String>,
    folders: RwSignal<Vec<FolderInfo>>,
    on_pick: Callback<(i64, Option<i64>)>,
) -> impl IntoView {
    let scope = Scope::new();
    let latest = Latest::new();
    let idle = RwSignal::new(String::new());
    let include_archived = RwSignal::new(false);
    let resp = RwSignal::new(None::<Result<SearchResp, String>>);
    let busy = RwSignal::new(false);

    // The idle debounce: `idle` follows `query` a beat after it stops.
    let gen = StoredValue::new(0u64);
    Effect::new(move |_| {
        let q = query.get();
        gen.update_value(|g| *g += 1);
        let mine = gen.get_value();
        set_timeout(
            move || {
                if gen.try_get_value() == Some(mine) {
                    idle.set(q);
                }
            },
            Duration::from_millis(IDLE_MS),
        );
    });

    // One read per (idle text, archived) — a later one wins over an earlier
    // answer that lands late.
    Effect::new(move |_| {
        let q = idle.get();
        let arch = include_archived.get();
        if !searchable(&q) {
            latest.next();
            busy.set(false);
            resp.set(None);
            return;
        }
        let Some(ticket) = latest.next() else { return };
        busy.set(true);
        scope.spawn(async move {
            let r = crate::api::get::<SearchResp>(search_url(&q, arch, 0))
                .await
                .map_err(|e| e.to_string());
            if latest.is(ticket) {
                resp.set(Some(r));
                busy.set(false);
            }
        });
    });

    let more = move || {
        let Some(Ok(cur)) = resp.get_untracked() else {
            return;
        };
        let Some(off) = cur.next_offset else { return };
        let q = idle.get_untracked();
        let arch = include_archived.get_untracked();
        let Some(ticket) = latest.next() else { return };
        busy.set(true);
        scope.spawn(async move {
            let r = crate::api::get::<SearchResp>(search_url(&q, arch, off)).await;
            if !latest.is(ticket) {
                return;
            }
            busy.set(false);
            match r {
                Ok(next) => resp.update(|cur| {
                    if let Some(Ok(cur)) = cur {
                        cur.threads.extend(next.threads);
                        cur.total_threads = next.total_threads;
                        cur.next_offset = next.next_offset;
                    }
                }),
                Err(e) => resp.set(Some(Err(e.to_string()))),
            }
        });
    };

    let folder_name =
        move |id: i64| folders.with(|f| f.iter().find(|f| f.id == id).map(|f| f.name.clone()));

    view! {
        <Show when=move || searchable(&query.get())>
            <section class="msg-hits">
                <div class="thread-group">
                    "Messages"
                    <span class="count">
                        {move || match resp.get() {
                            Some(Ok(r)) => format!("{} threads", r.total_threads),
                            _ if busy.get() => "searching…".to_string(),
                            _ => String::new(),
                        }}
                    </span>
                </div>
                <label class="msg-hits-arch dim">
                    <input
                        type="checkbox"
                        prop:checked=move || include_archived.get()
                        on:change=move |ev| include_archived.set(event_target_checked(&ev))
                    />
                    " include archived"
                </label>
                {move || match resp.get() {
                    Some(Err(e)) => {
                        view! { <div class="empty status-err">"Search failed: " {e}</div> }
                            .into_any()
                    }
                    Some(Ok(r)) if r.threads.is_empty() => {
                        view! { <div class="empty">"No messages match."</div> }.into_any()
                    }
                    Some(Ok(r)) => {
                        let next = r.next_offset.is_some();
                        let total = r.total_threads;
                        let shown = r.threads.len();
                        view! {
                            <div class="msg-hit-list">
                                {r
                                    .threads
                                    .into_iter()
                                    .map(|t| {
                                        let tid = t.thread_id;
                                        let extra = t.match_count - t.hits.len() as i64;
                                        let folder = t.folder_id.and_then(folder_name);
                                        view! {
                                            <div class="msg-hit-thread">
                                                <button
                                                    type="button"
                                                    class="msg-hit-title"
                                                    on:click=move |_| on_pick.run((tid, None))
                                                >
                                                    {t.title.clone()}
                                                </button>
                                                <div class="msg-hit-meta dim">
                                                    {folder}
                                                    {t
                                                        .archived
                                                        .then(|| {
                                                            view! { <span class="badge">"archived"</span> }
                                                        })}
                                                </div>
                                                {t
                                                    .hits
                                                    .into_iter()
                                                    .map(|h| {
                                                        let mid = h.message_id;
                                                        view! {
                                                            <button
                                                                type="button"
                                                                class="msg-hit"
                                                                on:click=move |_| on_pick.run((tid, mid))
                                                            >
                                                                <span class="msg-hit-who dim">{who(&h)}</span>
                                                                <span
                                                                    class="msg-hit-text"
                                                                    inner_html=snippet_html(&h.snippet)
                                                                ></span>
                                                            </button>
                                                        }
                                                    })
                                                    .collect_view()}
                                                {(extra > 0)
                                                    .then(|| {
                                                        view! {
                                                            <div class="msg-hit-more dim">
                                                                {format!("+{extra} more matches")}
                                                            </div>
                                                        }
                                                    })}
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                                {next
                                    .then(|| {
                                        view! {
                                            <button
                                                type="button"
                                                class="btn ghost sm"
                                                disabled=move || busy.get()
                                                on:click=move |_| more()
                                            >
                                                {format!("Show more ({shown} of {total} threads)")}
                                            </button>
                                        }
                                    })}
                            </div>
                        }
                            .into_any()
                    }
                    None => ().into_any(),
                }}
            </section>
        </Show>
    }
}

/// Scroll message `mid` to the middle of the transcript and flash it. `false`
/// when it is not in the DOM (yet).
fn reveal(mid: i64) -> bool {
    let Some(el) = document()
        .query_selector(&format!("[data-mid=\"{mid}\"]"))
        .ok()
        .flatten()
    else {
        return false;
    };
    let opts = web_sys::ScrollIntoViewOptions::new();
    opts.set_block(web_sys::ScrollLogicalPosition::Center);
    el.scroll_into_view_with_scroll_into_view_options(&opts);
    let _ = el.class_list().add_1("msg-flash");
    set_timeout(
        move || {
            let _ = el.class_list().remove_1("msg-flash");
        },
        Duration::from_millis(FLASH_MS),
    );
    true
}

/// Reveal the message named in `focus` once the transcript holds it, then
/// clear `focus`. A message the thread no longer has is dropped, not waited
/// for. `loaded` says whether the transcript is the target thread's.
pub(super) fn install_reveal(focus: RwSignal<Option<i64>>, msgs: RwSignal<Vec<Msg>>) {
    Effect::new(move |_| {
        let Some(mid) = focus.get() else { return };
        let has = msgs.with(|v| v.iter().any(|m| m.db_id.get_untracked() == Some(mid)));
        if !has {
            return;
        }
        request_animation_frame(move || {
            // Once the For has rendered the rows; a second frame covers the
            // first paint of a freshly opened thread.
            request_animation_frame(move || {
                reveal(mid);
                focus.set(None);
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_are_escaped_before_the_markers_become_marks() {
        let s = format!("a <b>{OPEN}x&y{CLOSE}</b> \"q\"…");
        assert_eq!(
            snippet_html(&s),
            "a &lt;b&gt;<mark>x&amp;y</mark>&lt;/b&gt; &quot;q&quot;…"
        );
        // A literal "<mark>" in a message stays text.
        assert_eq!(snippet_html("<mark>"), "&lt;mark&gt;");
    }

    #[test]
    fn a_search_needs_two_characters() {
        assert!(!searchable(""));
        assert!(!searchable(" a "));
        assert!(searchable("ab"));
        assert!(!searchable("ü "), "one char plus space is still one char");
    }
}
