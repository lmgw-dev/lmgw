//! The `pages` option (document-conversion design §7.1): `"3-7,10"`,
//! 1-based, applied to every file that has pages.

/// The pages a request asked for: inclusive ranges, sorted and merged into
/// disjoint ascending ones, so a spec that repeats itself costs what it
/// names once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSet {
    ranges: Vec<(u32, u32)>,
}

/// Which of a file's pages a [`PageSet`] picks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// The asked-for pages the file has, ascending, each once.
    pub pages: Vec<u32>,
    /// The asked-for ranges past the file's last page, as `"6-9"` or `"12"`.
    pub beyond: Vec<String>,
}

impl PageSet {
    /// Parse `spec`; the error says what is wrong and how it is written.
    pub fn parse(spec: &str) -> Result<Self, String> {
        // The caller's text, bounded (security round B, M4).
        let shown = super::quoted::quoted(spec);
        let bad = |why: &str| {
            format!("pages '{shown}' {why}; write 1-based pages and ranges such as \"3-7,10\"")
        };
        let mut ranges = Vec::new();
        for item in spec.split(',') {
            let item = item.trim();
            if item.is_empty() {
                return Err(bad("has an empty item"));
            }
            let (a, b) = match item.split_once('-') {
                Some((a, b)) => (a.trim(), b.trim()),
                None => (item, item),
            };
            let num = |s: &str| {
                s.parse::<u32>().map_err(|_| {
                    bad(&format!(
                        "has '{}', which is not a page or a range",
                        super::quoted::quoted(item)
                    ))
                })
            };
            let (a, b) = (num(a)?, num(b)?);
            if a == 0 {
                return Err(bad("counts from 0; pages count from 1"));
            }
            if b < a {
                return Err(bad(&format!(
                    "has the range '{}', which runs backwards",
                    super::quoted::quoted(item)
                )));
            }
            ranges.push((a, b));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len().min(1024));
        for (a, b) in ranges {
            match merged.last_mut() {
                // Overlapping or touching the one before.
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        Ok(Self { ranges: merged })
    }

    /// The first and the last page the set names: the span a tool run
    /// needs to read.
    pub fn span(&self) -> Option<(u32, u32)> {
        Some((self.ranges.first()?.0, self.ranges.last()?.1))
    }

    /// Whether the set names page `n` (the ranges are sorted and merged).
    pub fn contains(&self, n: u32) -> bool {
        let i = self.ranges.partition_point(|r| r.1 < n);
        self.ranges.get(i).is_some_and(|r| r.0 <= n)
    }

    /// The pages of a file with `total` pages this set picks.
    pub fn pick(&self, total: u32) -> Picked {
        let mut pages: Vec<u32> = Vec::new();
        let mut beyond = Vec::new();
        for &(a, b) in &self.ranges {
            if a <= total {
                pages.extend(a..=b.min(total));
            }
            if b > total {
                let from = a.max(total + 1);
                beyond.push(if from == b {
                    b.to_string()
                } else {
                    format!("{from}-{b}")
                });
            }
        }
        Picked { pages, beyond }
    }
}

/// `pages` (ascending, each once) as ranges: `3-900, 902`. Messages name
/// pages through this, so a page-by-page list never reaches one.
pub fn page_ranges(pages: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < pages.len() {
        let from = pages[i];
        let mut to = from;
        while i + 1 < pages.len() && pages[i + 1] == to.saturating_add(1) {
            i += 1;
            to = pages[i];
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if from == to {
            out.push_str(&from.to_string());
        } else {
            out.push_str(&format!("{from}-{to}"));
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_and_ranges_pick_each_page_once_in_order() {
        let p = PageSet::parse("10, 3-7,4").unwrap();
        let got = p.pick(12);
        assert_eq!(got.pages, [3, 4, 5, 6, 7, 10]);
        assert!(got.beyond.is_empty());
    }

    #[test]
    fn what_lies_past_the_last_page_is_named() {
        let p = PageSet::parse("2-4,9,6-100").unwrap();
        let got = p.pick(5);
        assert_eq!(got.pages, [2, 3, 4]);
        // 9 lies inside 6-100: the merged range is named once.
        assert_eq!(got.beyond, ["6-100"]);
        let got = PageSet::parse("2-4,9").unwrap().pick(5);
        assert_eq!(got.beyond, ["9"]);
        let got = PageSet::parse("4-8").unwrap().pick(5);
        assert_eq!(
            (got.pages, got.beyond),
            (vec![4, 5], vec!["6-8".to_string()])
        );
    }

    #[test]
    fn a_bad_spec_says_what_is_wrong() {
        for (spec, says) in [
            ("", "empty item"),
            ("1,,2", "empty item"),
            ("0-3", "counts from 0"),
            ("7-3", "runs backwards"),
            ("a", "not a page"),
            ("1-2-3", "not a page"),
        ] {
            let e = PageSet::parse(spec).unwrap_err();
            assert!(e.contains(says) && e.contains("3-7,10"), "{spec}: {e}");
        }
    }

    #[test]
    fn a_spec_that_repeats_itself_is_stored_and_picked_once() {
        let once = PageSet::parse("1-100000").unwrap();
        let spec = "1-100000,".repeat(50_000) + "1-100000";
        let many = PageSet::parse(&spec).unwrap();
        assert_eq!(many.ranges, [(1, 100_000)]);
        assert_eq!(many, once);
        let got = many.pick(100_000);
        assert_eq!(got.pages.len(), 100_000);
        assert_eq!(got.pages, once.pick(100_000).pages);
        // Touching and overlapping ranges merge; a gap keeps two.
        assert_eq!(
            PageSet::parse("5-6,1-2,3,2-4,9").unwrap().ranges,
            [(1, 6), (9, 9)]
        );
    }

    #[test]
    fn pages_are_named_as_ranges() {
        assert_eq!(page_ranges(&[]), "");
        assert_eq!(page_ranges(&[7]), "7");
        assert_eq!(page_ranges(&[3, 4, 5, 902]), "3-5, 902");
        let long: Vec<u32> = (3..=900).chain([902]).collect();
        assert_eq!(page_ranges(&long), "3-900, 902");
    }
}
