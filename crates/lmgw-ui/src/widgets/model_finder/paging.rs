//! The two lists' windows and pages (find-a-model design D97, revised in
//! WP6's fix round): each list keeps its own hub page (`cursor`), the next
//! one, and how much of its page it has shown, so "Show more" asks for the
//! next window of the page the list is on and "Look further on Hugging
//! Face" moves only the list that asked. A later page's card is one of the
//! other models with the name (the server leaves it out of that page's
//! `others`), so it leads that page's entries. Pure, so the arithmetic is
//! unit-tested without a browser.

use lmgw_api_types::model_find::{ModelCard, ModelChoice, ModelFindArgs, DEFAULT_LIST_COUNT};

/// One of the two lists ("Other models with this name", "Did you mean"),
/// as far as it has been shown.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ListState {
    pub items: Vec<ModelChoice>,
    /// How many of its page's entries come after the ones taken.
    pub more: u32,
    /// The entries of the pages seen, this one's included.
    pub total: u32,
    /// The hub's page the list is on (`None`: the first) and the next one.
    pub cursor: Option<String>,
    pub next: Option<String>,
    /// How many of its page's entries the windows took: the next window's
    /// offset. Not `items.len()`, which holds the earlier pages, a later
    /// page's card and none of the entries shown twice.
    pub taken: u32,
    /// The entries of the pages before this one.
    pub before: u32,
}

/// What the list's next request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ask {
    pub cursor: Option<String>,
    pub offset: u32,
    /// The hub's next page: its card leads the other models' entries.
    pub new_page: bool,
}

/// The list's button under its entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Further {
    /// "Show more (N not shown)": the next window of this page.
    ShowMore(u32),
    /// "Look further on Hugging Face": the hub's next page.
    NextPage,
    None,
}

impl ListState {
    /// The first page's window, as the search answered it.
    pub fn first(items: Vec<ModelChoice>, more: u32, total: u32, next: Option<String>) -> Self {
        let mut l = Self {
            more,
            total,
            next,
            taken: items.len() as u32,
            ..Self::default()
        };
        for c in items {
            l.push(c, None);
        }
        l
    }

    /// The next request, when there is one.
    pub fn ask(&self) -> Option<Ask> {
        if self.more > 0 {
            return Some(Ask {
                cursor: self.cursor.clone(),
                offset: self.taken,
                new_page: false,
            });
        }
        self.next.clone().map(|c| Ask {
            cursor: Some(c),
            offset: 0,
            new_page: true,
        })
    }

    pub fn further(&self) -> Further {
        match (self.more, &self.next) {
            (n, _) if n > 0 => Further::ShowMore(n),
            (_, Some(_)) if !self.items.is_empty() => Further::NextPage,
            _ => Further::None,
        }
    }

    /// `ask`'s answer for this list: its window (`items`, `more`, `total`
    /// as the server counts them on that page), the page after it, and a
    /// new page's card (`lead`). `exclude` is the card the finder shows,
    /// which no list repeats; an entry already listed isn't listed again.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &mut self,
        ask: &Ask,
        items: Vec<ModelChoice>,
        more: u32,
        total: u32,
        next: Option<String>,
        lead: Option<ModelChoice>,
        exclude: Option<&str>,
    ) {
        if ask.new_page {
            self.before = self.total;
            self.cursor = ask.cursor.clone();
            self.next = next;
            self.taken = 0;
        }
        let lead = lead.filter(|_| ask.new_page);
        let lead_n = u32::from(lead.as_ref().is_some_and(|c| self.fresh(c, exclude)));
        if let Some(c) = lead {
            self.push(c, exclude);
        }
        self.taken = self.taken.saturating_add(items.len() as u32);
        for c in items {
            self.push(c, exclude);
        }
        self.more = more;
        if ask.new_page {
            self.total = self.before.saturating_add(total).saturating_add(lead_n);
        }
    }

    fn fresh(&self, c: &ModelChoice, exclude: Option<&str>) -> bool {
        let same = |o: &str| o.eq_ignore_ascii_case(&c.origin);
        !exclude.is_some_and(same) && !self.items.iter().any(|i| same(&i.origin))
    }

    fn push(&mut self, c: ModelChoice, exclude: Option<&str>) {
        if self.fresh(&c, exclude) {
            self.items.push(c);
        }
    }
}

/// `model_find`'s arguments for `ask` on one list: that list's window, the
/// other's count 0, and no card's other uploads.
pub(super) fn args(query: &str, ask: &Ask, others: bool) -> ModelFindArgs {
    let (n, off) = (DEFAULT_LIST_COUNT, ask.offset);
    ModelFindArgs {
        query: query.to_string(),
        cursor: ask.cursor.clone(),
        others_offset: if others { off } else { 0 },
        others_count: if others { n } else { 0 },
        did_you_mean_offset: if others { 0 } else { off },
        did_you_mean_count: if others { 0 } else { n },
        other_uploads_offset: 0,
        other_uploads_count: 0,
    }
}

/// A later page's card as an entry of "Other models with this name".
pub(super) fn card_choice(c: &ModelCard) -> ModelChoice {
    let author = c
        .origin
        .split_once('/')
        .map_or(c.by.as_str(), |(a, _)| a)
        .to_string();
    ModelChoice {
        origin: c.origin.clone(),
        name: c.name.clone(),
        author,
        likes: c.likes,
        downloads: c.downloads,
        pipeline: None,
        parameters: c.parameters,
        matched_words: 0,
        gated: c.gated,
        private: c.private,
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(origin: &str) -> ModelChoice {
        ModelChoice {
            origin: origin.into(),
            ..ModelChoice::default()
        }
    }

    fn many(prefix: &str, from: usize, n: usize) -> Vec<ModelChoice> {
        (from..from + n)
            .map(|i| c(&format!("{prefix}/{i}")))
            .collect()
    }

    #[test]
    fn show_more_asks_for_the_next_window_of_its_page() {
        let mut l = ListState::first(many("p1", 0, 10), 15, 25, Some("p2".into()));
        assert_eq!(l.further(), Further::ShowMore(15));
        let ask = l.ask().unwrap();
        assert_eq!(
            ask,
            Ask {
                cursor: None,
                offset: 10,
                new_page: false
            }
        );
        l.apply(&ask, many("p1", 10, 10), 5, 25, None, None, None);
        assert_eq!(l.items.len(), 20);
        assert_eq!(l.total, 25);
        // The next page's cursor is the first answer's: a window doesn't move it.
        assert_eq!(l.next.as_deref(), Some("p2"));
    }

    #[test]
    fn a_later_pages_windows_start_at_that_page() {
        let mut l = ListState::first(many("p1", 0, 10), 0, 10, Some("p2".into()));
        assert_eq!(l.further(), Further::NextPage);
        let ask = l.ask().unwrap();
        assert_eq!(ask.cursor.as_deref(), Some("p2"));
        assert_eq!(ask.offset, 0);
        assert!(ask.new_page);
        // Page 2: its card leads, one entry is page 1's again and one is
        // the card the finder shows.
        let mut items = many("p2", 0, 8);
        items.push(c("p1/3"));
        items.push(c("Shown/Card"));
        l.apply(
            &ask,
            items,
            4,
            14,
            Some("p3".into()),
            Some(c("p2/top")),
            Some("shown/card"),
        );
        assert_eq!(l.items.len(), 10 + 1 + 8, "the card, less the two repeats");
        assert_eq!(l.items[10].origin, "p2/top");
        assert_eq!(l.total, 10 + 14 + 1);
        assert_eq!(l.cursor.as_deref(), Some("p2"));
        assert_eq!(l.next.as_deref(), Some("p3"));
        // Show more on page 2: its own offset, not the items shown so far.
        let more = l.ask().unwrap();
        assert_eq!(more.cursor.as_deref(), Some("p2"));
        assert_eq!(more.offset, 10);
        assert!(!more.new_page);
    }

    #[test]
    fn each_list_asks_only_for_its_own_window() {
        let ask = Ask {
            cursor: Some("p2".into()),
            offset: 10,
            new_page: false,
        };
        let o = args("demo", &ask, true);
        assert_eq!((o.others_offset, o.others_count), (10, DEFAULT_LIST_COUNT));
        assert_eq!((o.did_you_mean_offset, o.did_you_mean_count), (0, 0));
        assert_eq!(o.cursor.as_deref(), Some("p2"));
        let d = args("demo", &ask, false);
        assert_eq!((d.others_offset, d.others_count), (0, 0));
        assert_eq!(
            (d.did_you_mean_offset, d.did_you_mean_count),
            (10, DEFAULT_LIST_COUNT)
        );
        assert_eq!((d.other_uploads_offset, d.other_uploads_count), (0, 0));
    }

    #[test]
    fn a_list_without_a_next_page_ends() {
        let l = ListState::first(many("p1", 0, 3), 0, 3, None);
        assert_eq!(l.further(), Further::None);
        assert!(l.ask().is_none());
        assert_eq!(ListState::default().further(), Further::None);
    }

    #[test]
    fn a_later_pages_card_is_an_entry_by_its_owner() {
        let card = ModelCard {
            name: "Demo 9B".into(),
            by: "Demo Org".into(),
            origin: "demo-org/Demo-9B".into(),
            likes: 12,
            parameters: Some(9_000_000_000),
            ..ModelCard::default()
        };
        let e = card_choice(&card);
        assert_eq!(e.author, "demo-org");
        assert_eq!(e.likes, 12);
        assert_eq!(e.name, "Demo 9B");
    }
}
