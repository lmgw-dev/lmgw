//! Totals over logged requests (usage-analytics §2.3): what one row adds to
//! a total, and a total kept from rows as they come in. The rollup states the
//! same rule over `usage_hourly`; a run's meters (`agents::batch`'s and the
//! HTTP `agents::RunMeters`) keep theirs with [`CostTotal`].

use super::sum_micro;

/// What one logged request adds to a total over requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCost {
    /// Its price, a real 0 for a free one.
    Priced(i64),
    /// Work nobody could price — answered, or it spent tokens or a measured
    /// quantity: the total is unknown from here on, never the others'
    /// partial sum.
    Unpriced,
    /// No price and no work — a refusal, an error that spent nothing: no
    /// hole in any total, so it adds nothing.
    Nothing,
}

impl RowCost {
    /// A row priced at `cost_micro`; `worked` is whether it did work a
    /// missing price would leave out of a total — the rollup's remainder
    /// rule, which [`crate::store::NewRequestLog::row_cost`] states for a row.
    pub fn of(cost_micro: Option<i64>, worked: bool) -> Self {
        match cost_micro {
            Some(c) => Self::Priced(c),
            None if worked => Self::Unpriced,
            None => Self::Nothing,
        }
    }
}

/// A total over rows' [`RowCost`]s: nothing added yet, their sum, or unknown
/// for good once one was [`RowCost::Unpriced`]. It starts at the first row
/// that adds to it, never at a `0` or a `None` of its own
/// ([`super::sum_micro`]).
///
/// Not `Serialize`: as JSON, "unknown" and "nothing added" would both read
/// `null`. A report writes [`Self::micro`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CostTotal(Option<Option<i64>>);

impl CostTotal {
    pub fn add(&mut self, row: RowCost) {
        match row {
            RowCost::Priced(c) => self.add_micro(Some(c)),
            RowCost::Unpriced => self.add_micro(None),
            RowCost::Nothing => {}
        }
    }

    /// Another total folded in: one that nothing was added to adds nothing.
    pub fn absorb(&mut self, other: CostTotal) {
        if let Some(c) = other.0 {
            self.add_micro(c);
        }
    }

    fn add_micro(&mut self, c: Option<i64>) {
        self.0 = Some(match self.0 {
            None => c,
            Some(sum) => sum_micro(sum, c),
        });
    }

    /// The total in micro-units: `None` when it is unknown, and when no row
    /// added to it — nothing priced is not "free".
    pub fn micro(self) -> Option<i64> {
        self.0.flatten()
    }

    /// No row has added to it.
    pub fn is_empty(self) -> bool {
        self.0.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(rows: &[RowCost]) -> CostTotal {
        let mut t = CostTotal::default();
        for r in rows {
            t.add(*r);
        }
        t
    }

    #[test]
    fn a_row_without_a_price_is_a_gap_only_when_it_did_work() {
        assert_eq!(RowCost::of(Some(5), false), RowCost::Priced(5));
        assert_eq!(RowCost::of(Some(0), true), RowCost::Priced(0));
        assert_eq!(RowCost::of(None, true), RowCost::Unpriced);
        assert_eq!(RowCost::of(None, false), RowCost::Nothing);
    }

    #[test]
    fn a_total_sums_from_its_first_row_and_stays_unknown_once_unknown() {
        use RowCost::*;
        assert_eq!(total(&[]).micro(), None, "nothing priced is not free");
        assert!(total(&[Nothing, Nothing]).is_empty());
        assert_eq!(total(&[Priced(0), Priced(0)]).micro(), Some(0));
        assert_eq!(total(&[Nothing, Priced(2), Priced(3)]).micro(), Some(5));
        assert_eq!(total(&[Priced(2), Unpriced, Priced(3)]).micro(), None);
        assert_eq!(total(&[Unpriced, Priced(3)]).micro(), None);
        assert!(!total(&[Unpriced]).is_empty());
    }

    #[test]
    fn folding_an_empty_total_in_changes_nothing() {
        use RowCost::*;
        let mut t = total(&[Priced(7)]);
        t.absorb(CostTotal::default());
        assert_eq!(t.micro(), Some(7));
        t.absorb(total(&[Nothing]));
        assert_eq!(t.micro(), Some(7));
        t.absorb(total(&[Priced(3)]));
        assert_eq!(t.micro(), Some(10));
        t.absorb(total(&[Unpriced]));
        assert_eq!(t.micro(), None);

        let mut empty = CostTotal::default();
        empty.absorb(total(&[Priced(4)]));
        assert_eq!(empty.micro(), Some(4), "it starts at the folded total");
    }
}
