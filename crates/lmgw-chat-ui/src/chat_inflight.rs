//! The page's own turn while it streams, kept beyond the page.
//!
//! A turn's worker outlives the Chat page that started it: leaving the Chat
//! (another page of the dashboard) does not stop the stream, and the gateway
//! goes on generating and saves the reply at its end (`chat_turn`). What the
//! page knew of it did not outlive the page, though: which thread streams,
//! the reply's bubble, its live numbers and its Stop. A page opened again
//! before the reply was saved read the thread from the server, which has
//! the user's message and no reply yet, and showed it as idle until the
//! saved reply came in at the end.
//!
//! So these live here, made once under the app's root owner rather than the
//! page's, and every Chat page takes them ([`use_in_flight`]):
//! - which thread's reply streams, its bubble (`live`, put back under its
//!   thread when a page opens it, `chat::page`, in its stored row's place
//!   for a continue: [`put_live`]), its stats row and its Stop;
//! - when a page goes with a reply still streaming, the bubble is the
//!   page's and would go with it: [`keep_live`] makes a copy of it under an
//!   owner of its own here, and the turn writes into the copy from then on
//!   (`chat_turn` reads its bubble through `live`). The copy is let go once
//!   its turn has ended and no page shows it ([`sweep`]).

use std::cell::{OnceCell, RefCell};

use leptos::prelude::*;

use super::chat::{Msg, Stats, ToolCard};
use super::chat_approvals::Approval;

/// The page's turn in flight, as every Chat page reads it. `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct InFlight {
    /// The thread whose reply is streaming, not just "something is": the
    /// owner may open another thread meanwhile, and that one's Stop must
    /// not abort this stream, nor its stats row show this stream's numbers.
    pub streaming: RwSignal<Option<i64>>,
    /// The streaming reply's own message, to put back under its thread when
    /// a page opens it before the server has stored it, and the bubble the
    /// turn writes into.
    pub live: StoredValue<Option<(i64, Msg)>>,
    /// The last reply's numbers, tagged with its thread.
    pub stats: RwSignal<Option<(i64, Stats)>>,
    /// The streaming request's Stop.
    pub aborter: StoredValue<Option<web_sys::AbortController>>,
}

/// A bubble copied out of a page that went ([`keep_live`]): its owner, and
/// the key the turn's bubble had.
struct Kept {
    key: u64,
    owner: Owner,
}

struct Store {
    /// Under the app's root owner: lives as long as the app.
    owner: Owner,
    flight: InFlight,
    kept: RefCell<Vec<Kept>>,
}

thread_local! {
    static STORE: OnceCell<Store> = const { OnceCell::new() };
}

/// The app's root owner: the top of the current owner's ancestors.
fn app_root() -> Option<Owner> {
    let mut o = Owner::current()?;
    while let Some(p) = o.parent() {
        o = p;
    }
    Some(o)
}

fn with_store<T>(f: impl FnOnce(&Store) -> T) -> T {
    STORE.with(|cell| {
        let store = cell.get_or_init(|| {
            // Made under the root, not the page: the page goes, this stays.
            let owner = app_root().map_or_else(Owner::new, |root| root.child());
            let flight = owner.with(|| InFlight {
                streaming: RwSignal::new(None),
                live: StoredValue::new(None),
                stats: RwSignal::new(None),
                aborter: StoredValue::new(None),
            });
            Store {
                owner,
                flight,
                kept: RefCell::new(Vec::new()),
            }
        });
        f(store)
    })
}

/// The first message key of a page's own range: each page mounted takes the
/// next 2^32 keys, so a bubble kept from a page that went never shares its
/// key with one of the page that shows it.
pub(crate) fn key_base() -> u64 {
    thread_local! {
        static PAGES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    PAGES.with(|p| {
        let n = p.get() + 1;
        p.set(n);
        n << 32
    })
}

/// The turn in flight, for a Chat page. Call in the page's body.
pub(crate) fn use_in_flight() -> InFlight {
    with_store(|s| s.flight)
}

/// Put the streaming reply's bubble `live` among a thread's messages as
/// they were just read: a continue or a resumed turn streams into a reply
/// that is stored already, which the read holds (its `db_id`), so the bubble
/// takes that row's place; a new reply is not stored yet and comes last.
pub(crate) fn put_live(msgs: &mut Vec<Msg>, live: Msg) {
    let stored = live.db_id.try_get_untracked().flatten();
    match stored.and_then(|id| {
        msgs.iter()
            .position(|m| m.db_id.try_get_untracked().flatten() == Some(id))
    }) {
        Some(i) => msgs[i] = live,
        None => msgs.push(live),
    }
}

/// The page goes (its `on_cleanup`, while its own signals are still there):
/// a reply still streaming keeps its bubble, copied under an owner of its
/// own, and the turn writes into the copy from now on. A bubble that is a
/// copy already stays as it is. Then whatever is let go goes ([`sweep`]),
/// none of it shown any more.
pub(crate) fn keep_live() {
    with_store(|s| {
        let live = s.flight.live.try_get_value().flatten();
        if let Some((tid, m)) = live {
            let copied = s.kept.borrow().iter().any(|k| k.key == m.key);
            if !copied {
                let owner = s.owner.child();
                let copy = owner.with(|| fork(&m));
                s.kept.borrow_mut().push(Kept { key: m.key, owner });
                s.flight.live.try_set_value(Some((tid, copy)));
            }
        }
    });
    sweep(|_| false);
}

/// Let go of the copies ([`keep_live`]) whose turn has ended and that no
/// page shows (`shown` is the open page's say, by key): after the current
/// task, so a view that still reads one is gone first.
pub(crate) fn sweep(shown: impl Fn(u64) -> bool) {
    let gone = let_go(shown);
    if gone.is_empty() {
        return;
    }
    set_timeout(move || drop(gone), std::time::Duration::ZERO);
}

/// The owners of the copies [`sweep`] lets go, taken out of the store.
fn let_go(shown: impl Fn(u64) -> bool) -> Vec<Owner> {
    with_store(|s| {
        let streaming = s
            .flight
            .live
            .try_with_value(|l| l.as_ref().map(|(_, m)| m.key))
            .flatten();
        let mut kept = s.kept.borrow_mut();
        let (go, stay): (Vec<Kept>, Vec<Kept>) = kept
            .drain(..)
            .partition(|k| Some(k.key) != streaming && !shown(k.key));
        *kept = stay;
        go.into_iter().map(|k| k.owner).collect()
    })
}

/// A copy of `m` under the current owner: every signal new, with what
/// `m`'s holds now (one already gone reads as empty), its tool cards too.
fn fork(m: &Msg) -> Msg {
    fn now<T: Clone + Default + Send + Sync + 'static>(s: RwSignal<T>) -> RwSignal<T> {
        RwSignal::new(s.try_get_untracked().unwrap_or_default())
    }
    let tools = m
        .tools
        .try_get_untracked()
        .unwrap_or_default()
        .iter()
        .map(fork_card)
        .collect();
    Msg {
        key: m.key,
        db_id: now(m.db_id),
        role: m.role.clone(),
        content: now(m.content),
        reasoning: now(m.reasoning),
        tools: RwSignal::new(tools),
        streaming: now(m.streaming),
        tokens: now(m.tokens),
        attachments: now(m.attachments),
        kb_refs: now(m.kb_refs),
        context: now(m.context),
        model: now(m.model),
        answered_by: now(m.answered_by),
        images_note: now(m.images_note),
        notices: now(m.notices),
        unsaved: now(m.unsaved),
        voice: now(m.voice),
        origin: now(m.origin),
        task: now(m.task),
        created_at: now(m.created_at),
        gpu_setup: now(m.gpu_setup),
    }
}

fn fork_card(c: &ToolCard) -> ToolCard {
    fn now<T: Clone + Default + Send + Sync + 'static>(s: RwSignal<T>) -> RwSignal<T> {
        RwSignal::new(s.try_get_untracked().unwrap_or_default())
    }
    let a = &c.approval;
    ToolCard {
        index: c.index,
        name: now(c.name),
        args: now(c.args),
        output: now(c.output),
        is_error: now(c.is_error),
        ms: now(c.ms),
        done: now(c.done),
        approval: Approval {
            hold: now(a.hold),
            pick: now(a.pick),
            sending: now(a.sending),
            said: now(a.said),
        },
        call_id: now(c.call_id),
        media: now(c.media),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::new_msg;

    fn kept_len() -> usize {
        with_store(|s| s.kept.borrow().len())
    }

    /// Leaving the Chat mid-turn: the page's bubble goes with the page, the
    /// copy kept of it carries what had streamed and takes the rest, and
    /// is let go only once its turn has ended and no page shows it.
    #[test]
    fn a_reply_streaming_when_its_page_goes_is_kept_and_written_on() {
        let app = Owner::new();
        app.set();
        let flight = use_in_flight();
        let page = app.child();
        let m = page.with(|| new_msg(7, "assistant", "Hel".into()));
        m.streaming.set(true);
        flight.live.set_value(Some((3, m.clone())));
        flight.streaming.set(Some(3));
        // The page's `on_cleanup` runs while its signals are still there.
        page.with(keep_live);
        page.cleanup();
        drop(page);
        assert!(
            m.content.try_get_untracked().is_none(),
            "the page's bubble went"
        );
        let (tid, kept) = flight.live.get_value().expect("still streaming");
        assert_eq!((tid, kept.key), (3, 7));
        assert_eq!(kept.content.get_untracked(), "Hel");
        assert!(kept.streaming.get_untracked());
        kept.content.update(|c| c.push_str("lo"));
        assert_eq!(kept.content.get_untracked(), "Hello");
        assert_eq!(flight.streaming.get_untracked(), Some(3));
        // A page opened and left again keeps the same copy.
        keep_live();
        assert_eq!(kept_len(), 1);
        let (_, again) = flight.live.get_value().unwrap();
        assert_eq!(again.content.get_untracked(), "Hello");
        // Streaming, it stays whatever a page shows.
        assert!(let_go(|_| false).is_empty());
        // Ended: a page that shows it keeps it, then it goes.
        flight.live.set_value(None);
        flight.streaming.set(None);
        assert!(let_go(|k| k == 7).is_empty());
        let gone = let_go(|_| false);
        assert_eq!((gone.len(), kept_len()), (1, 0));
        drop(gone);
        assert!(kept.content.try_get_untracked().is_none(), "let go");
    }

    /// Back on a thread mid-turn: a new reply's bubble comes after the
    /// stored messages, a continued reply's stands in its stored row's place.
    #[test]
    fn the_live_bubble_takes_its_stored_rows_place_or_comes_last() {
        let owner = Owner::new();
        owner.set();
        let row = |key, role: &str, id: i64, text: &str| {
            let m = new_msg(key, role, text.into());
            m.db_id.set(Some(id));
            m
        };
        let read = || vec![row(1, "user", 10, "q"), row(2, "assistant", 11, "Once")];
        let keys = |v: &[Msg]| v.iter().map(|m| m.key).collect::<Vec<_>>();

        let mut v = read();
        put_live(&mut v, new_msg(9, "assistant", String::new()));
        assert_eq!(keys(&v), [1, 2, 9], "a new reply comes last");

        let mut v = read();
        put_live(&mut v, row(9, "assistant", 11, "Once upon"));
        assert_eq!(keys(&v), [1, 9], "a continued reply once, in its place");
        assert_eq!(v[1].content.get_untracked(), "Once upon");
    }

    #[test]
    fn every_page_takes_keys_of_its_own() {
        let (a, b) = (key_base(), key_base());
        assert!(b >= a + (1 << 32));
    }
}
