//! Telling an item's owner that its audio ran dry (chat-voice §8.5, WP9
//! review m3).
//!
//! A realtime tool turn is one item: its spoken preamble plays, then nothing
//! comes while the tool runs, though the item has not ended. The worklet
//! reports that as an `underrun` listing the items still waiting; the owner
//! that asked ([`super::Player::watch_starved`]) is told `true` then, and
//! `false` at the next push of audio for it. One item is watched at a time:
//! the page has one playback.

use std::rc::Rc;

/// Called with `true` when the watched item's audio ran dry, `false` when
/// audio for it came again.
pub(crate) type Starved = Rc<dyn Fn(bool)>;

/// The watched item, whether it is dry now, and whom to tell.
#[derive(Default)]
pub(super) struct Watch {
    item: Option<(u32, bool, Starved)>,
}

impl Watch {
    pub(super) fn set(&mut self, item: u32, tell: Starved) {
        self.item = Some((item, false, tell));
    }

    /// Stop watching `item`, or whatever is watched (`None`).
    pub(super) fn forget(&mut self, item: Option<u32>) {
        if item.is_none() || self.item.as_ref().map(|w| w.0) == item {
            self.item = None;
        }
    }

    /// An underrun with `waiting` items: the call to make, when the watched
    /// one is among them and was not dry yet.
    pub(super) fn underrun(&mut self, waiting: &[u32]) -> Option<(Starved, bool)> {
        self.flip(|item| waiting.contains(&item), true)
    }

    /// Audio was pushed for `item`: the call to make, when it is the
    /// watched one and was dry.
    pub(super) fn pushed(&mut self, item: u32) -> Option<(Starved, bool)> {
        self.flip(|i| i == item, false)
    }

    fn flip(&mut self, hit: impl Fn(u32) -> bool, to: bool) -> Option<(Starved, bool)> {
        let (item, dry, tell) = self.item.as_mut()?;
        if !hit(*item) || *dry == to {
            return None;
        }
        *dry = to;
        Some((tell.clone(), to))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn the_owner_is_told_once_per_change() {
        let told = Rc::new(RefCell::new(Vec::new()));
        let t = told.clone();
        let mut w = Watch::default();
        assert!(w.underrun(&[3]).is_none(), "nothing watched");
        w.set(3, Rc::new(move |s| t.borrow_mut().push(s)));
        for (tell, s) in [
            w.pushed(3),
            w.underrun(&[1]),
            w.underrun(&[1, 3]),
            w.underrun(&[3]),
            w.pushed(1),
            w.pushed(3),
            w.pushed(3),
        ]
        .into_iter()
        .flatten()
        {
            tell(s);
        }
        assert_eq!(*told.borrow(), [true, false]);
        w.forget(Some(1));
        assert!(w.underrun(&[3]).is_some(), "another item forgotten");
        w.forget(Some(3));
        assert!(w.pushed(3).is_none());
    }
}
