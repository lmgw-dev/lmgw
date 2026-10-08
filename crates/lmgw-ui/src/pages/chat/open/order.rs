//! The page's opens in the order the owner meant them (reviews CL-20,
//! CF-4), on plain values. What the owner chose last decides what shows,
//! not which read went out last.
//!
//! - **The owner's choices** ([`Opens::choose`]): a row, a search hit, New
//!   chat (the toolbar's, the empty page's, the folder menu's) and Keep. A
//!   row or a search hit opens at once.
//! - **The open that follows a choice's request** (what New chat's or
//!   Keep's POST made) goes only when the owner chose nothing since
//!   ([`Opens::follows`]). Choosing drops no open: a New chat that fails
//!   leaves an open that was about to show to show.
//! - **An open the page makes on its own** (the fall-through after a
//!   delete, here or elsewhere, and the cold load's thread) goes only when
//!   no open started since and none is out ([`Opens::falls_through`]):
//!   that one is where the page is going.
//! - **The newest open shows** ([`Opens::lands`]): an older one that answers
//!   after it is dropped.
//! - **A New chat that fails gives back what it left** (the rescued draft's
//!   new chat, review CF-1) only when nothing else is on its way to show
//!   ([`Opens::failed`]).

/// Where the opens stood at a moment: what follows from it goes only when
/// nothing came after it. `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::pages) struct Since {
    chosen: u64,
    opened: u64,
}

impl Since {
    /// For a page that is gone: nothing follows from it.
    pub(in crate::pages) fn gone() -> Self {
        Since {
            chosen: u64::MAX,
            opened: u64::MAX,
        }
    }
}

/// The page's opens and the owner's choices.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::pages) struct Opens {
    /// The owner's choices so far.
    chosen: u64,
    /// The newest open's ticket.
    latest: u64,
    /// The newest open is still out: its answer has not landed.
    out: bool,
}

impl Opens {
    /// Now: a delete, made here or elsewhere, or the cold load starts.
    pub(in crate::pages) fn now(&self) -> Since {
        Since {
            chosen: self.chosen,
            opened: self.latest,
        }
    }

    /// The owner chooses what to show.
    pub(in crate::pages) fn choose(&mut self) -> Since {
        self.chosen += 1;
        self.now()
    }

    /// The owner clicks a row or a search hit; `open` says it is the thread
    /// already open. Clicking that one chooses nothing, so the open a
    /// request is about to make (Keep's, New chat's) still goes (review
    /// CF-10).
    pub(in crate::pages) fn choose_row(&mut self, open: bool) -> Since {
        if !open {
            self.chosen += 1;
        }
        self.now()
    }

    /// The open after the request of the choice `since` goes: the owner
    /// chose nothing since.
    pub(in crate::pages) fn follows(&self, since: Since) -> bool {
        since.chosen == self.chosen
    }

    /// An open the page makes on its own after `since` goes: no open
    /// started since, and none is out.
    pub(in crate::pages) fn falls_through(&self, since: Since) -> bool {
        since.opened == self.latest && !self.out
    }

    /// The request of the choice `since` failed: whether nothing else is on
    /// its way to show (the owner chose nothing since, and no open is out),
    /// so what the choice left comes back.
    pub(in crate::pages) fn failed(&self, since: Since) -> bool {
        self.follows(since) && !self.out
    }

    /// An open goes out: the newest from now, its ticket.
    pub(in crate::pages) fn start(&mut self) -> u64 {
        self.latest += 1;
        self.out = true;
        self.latest
    }

    /// The thread to open shows already: an open still out is dropped as it
    /// lands.
    pub(in crate::pages) fn stay(&mut self) {
        self.latest += 1;
        self.out = false;
    }

    /// Open `ticket` answered: whether it is still the newest, to be shown,
    /// or its failure said.
    pub(in crate::pages) fn lands(&mut self, ticket: u64) -> bool {
        let newest = ticket == self.latest;
        if newest {
            self.out = false;
        }
        newest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page showing thread A, its open landed.
    fn showing() -> Opens {
        let mut o = Opens::default();
        let a = o.start();
        assert!(o.lands(a));
        o
    }

    /// The owner clicks a row: a choice, and its open.
    fn click(o: &mut Opens) -> u64 {
        o.choose();
        o.start()
    }

    /// Review CF-4: the owner clicks B, and before B lands the open
    /// thread is deleted elsewhere, with no draft. The fall-through opens
    /// nothing: B shows.
    #[test]
    fn a_delete_elsewhere_falls_through_only_while_no_open_is_out() {
        let mut o = showing();
        let b = click(&mut o);
        assert!(!o.falls_through(o.now()), "B is out: the owner goes there");
        assert!(o.lands(b), "B shows");
        assert!(o.falls_through(o.now()), "nothing out: the next one opens");
    }

    /// Review CF-4: the page's own row delete falls through after its POST
    /// and the list read. An open the owner started meanwhile, or before
    /// and still out, is where the page goes.
    #[test]
    fn the_page_s_own_delete_falls_through_only_when_no_open_started_since() {
        let mut o = showing();
        let delete = o.now();
        let b = click(&mut o);
        assert!(!o.falls_through(delete));
        assert!(o.lands(b));
        assert!(!o.falls_through(delete), "B landed: nothing opens over it");

        let o = showing();
        let delete = o.now();
        assert!(o.falls_through(delete), "nothing since: the next one opens");

        let mut o = showing();
        let b = click(&mut o);
        let delete = o.now();
        assert!(!o.falls_through(delete), "B is still out");
        assert!(o.lands(b));

        // A New chat pressed meanwhile that failed opened nothing.
        let mut o = showing();
        let delete = o.now();
        let _new_chat = o.choose();
        assert!(o.falls_through(delete));
    }

    /// Review CF-4: the open of what New chat's, the folder menu's or Keep's
    /// POST made never overrides a row the owner clicked during the POST;
    /// pressed after one, New chat is the newer choice.
    #[test]
    fn an_open_after_a_request_yields_to_what_the_owner_chose_meanwhile() {
        let mut o = showing();
        let new_chat = o.choose();
        let b = click(&mut o);
        assert!(!o.follows(new_chat), "the made chat is not opened");
        assert!(o.lands(b), "B shows");

        let mut o = showing();
        let b = click(&mut o);
        let new_chat = o.choose();
        assert!(o.follows(new_chat));
        let made = o.start();
        assert!(!o.lands(b), "B, older, is dropped");
        assert!(o.lands(made));

        // New chat pressed twice: the last one opens.
        let mut o = showing();
        let first = o.choose();
        let second = o.choose();
        assert!(!o.follows(first) && o.follows(second));

        // A delete of another row meanwhile is no choice of what to show.
        let mut o = showing();
        let new_chat = o.choose();
        let _delete = o.now();
        assert!(o.follows(new_chat));
    }

    /// Review CF-4: a New chat whose POST fails cancels no open. On a cold
    /// `/chat?t=5`, the empty page's New chat pressed while thread 5's read
    /// is out: thread 5 shows.
    #[test]
    fn a_failed_new_chat_leaves_the_open_out_to_show() {
        let mut o = Opens::default();
        let deep_link = o.start();
        let _new_chat = o.choose();
        assert!(o.lands(deep_link));
    }

    /// The cold load opens its thread after the list read, as the page's
    /// own open: a New chat whose chat opened meanwhile shows instead, one
    /// that failed leaves the cold load to open.
    #[test]
    fn the_cold_load_yields_to_an_open_started_while_the_list_was_read() {
        let mut o = Opens::default();
        let cold = o.now();
        let new_chat = o.choose();
        assert!(o.follows(new_chat));
        let _made = o.start();
        assert!(!o.falls_through(cold), "the list answers: nothing opens");

        let mut o = Opens::default();
        let cold = o.now();
        let _failed_new_chat = o.choose();
        assert!(o.falls_through(cold), "the deep link opens");
    }

    /// Clicking the open row drops an open still out: the owner chose what
    /// shows already.
    #[test]
    fn the_open_row_clicked_drops_the_open_still_out() {
        let mut o = showing();
        let b = click(&mut o);
        o.choose_row(true);
        o.stay();
        assert!(!o.lands(b));
        assert!(o.falls_through(o.now()), "nothing is out");
    }

    /// Review CF-10: clicking the open row during Keep's (or New chat's)
    /// request still opens the kept chat.
    #[test]
    fn clicking_the_open_row_during_keeps_request_still_opens_the_kept_chat() {
        let mut o = showing();
        let keep = o.choose();
        o.choose_row(true);
        o.stay();
        assert!(o.follows(keep), "the kept chat opens");

        // A click on another row is a choice and does win.
        let mut o = showing();
        let keep = o.choose();
        o.choose_row(false);
        assert!(!o.follows(keep));
    }

    /// Review CF-1: a New chat that fails gives back what it left (the
    /// rescued draft's new chat) only when nothing else is on its way to
    /// show; an open that is decides, by showing or by failing.
    #[test]
    fn a_failed_new_chat_gives_back_only_when_nothing_else_will_show() {
        let mut o = showing();
        let new_chat = o.choose();
        assert!(o.failed(new_chat));

        let mut o = showing();
        let new_chat = o.choose();
        let b = click(&mut o);
        assert!(!o.failed(new_chat), "B is out: the owner went there");
        assert!(o.lands(b));
        assert!(!o.failed(new_chat), "B landed");

        let mut o = showing();
        let b = click(&mut o);
        let new_chat = o.choose();
        assert!(!o.failed(new_chat), "B, older, is still out");
        assert!(o.lands(b), "B lands, shown or failed: it decides");
    }

    #[test]
    fn nothing_follows_from_a_page_that_is_gone() {
        let o = Opens::default();
        assert!(!o.follows(Since::gone()) && !o.falls_through(Since::gone()));
        assert!(!o.failed(Since::gone()));
    }
}
