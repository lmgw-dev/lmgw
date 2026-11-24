//! The options a request sends that docling does not have (or this table
//! does not know): each a `not_yet` note with its name. The names are the
//! caller's free text, so what is kept of them is bounded where the bounds
//! show: a name is cut to [`MAX_NAME_BYTES`] with its length beside it, and
//! only the first [`MAX_LISTED`] names are noted one by one, the rest as a
//! count.

use lmgw_api_types::documents::adapter_usage::OptionOutcome;

use crate::extract::convert::calls::{Noted, Value};

/// The most unknown option names noted one by one.
pub const MAX_LISTED: usize = 32;

/// The most bytes of an unknown option's name kept.
pub const MAX_NAME_BYTES: usize = 64;

/// The detail every unknown option's note carries.
pub const DETAIL: &str = "unknown option";

/// The unknown names of one request, as they are met.
#[derive(Default)]
pub struct Names {
    listed: usize,
    more: usize,
}

/// `name`, cut to [`MAX_NAME_BYTES`] at a character boundary, with its
/// length when it was cut.
fn bounded(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    let mut end = MAX_NAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[{} bytes]", &name[..end], name.len())
}

impl Names {
    /// The note for unknown option `name`; `None` once [`MAX_LISTED`] names
    /// have been noted (the rest are counted for [`Self::rest`]).
    pub fn note(&mut self, name: &str) -> Option<Noted> {
        if self.listed >= MAX_LISTED {
            self.more += 1;
            return None;
        }
        self.listed += 1;
        Some(Noted::new(&bounded(name), Value::Set, OptionOutcome::NotYet).detail(DETAIL))
    }

    /// How many names were met past [`MAX_LISTED`], when any were.
    pub fn more(&self) -> Option<usize> {
        (self.more > 0).then_some(self.more)
    }

    /// The note for the names past the limit, when there were any.
    pub fn rest(&self) -> Option<Noted> {
        (self.more > 0).then(|| {
            Noted::new("unknown_options", Value::Set, OptionOutcome::NotYet).detail(format!(
                "{} more unknown names and only the first {MAX_LISTED} are listed",
                self.more
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_and_many_names_are_bounded_visibly() {
        let mut n = Names::default();
        let long = "é".repeat(5_000);
        let first = n.note(&long).unwrap();
        assert!(
            first.name.len() <= MAX_NAME_BYTES + 20,
            "{}",
            first.name.len()
        );
        assert!(first.name.ends_with("[10000 bytes]"), "{}", first.name);
        assert_eq!(n.note("short").unwrap().name, "short");
        for i in 0..MAX_LISTED * 3 {
            let _ = n.note(&format!("example_{i}"));
        }
        let rest = n.rest().unwrap();
        assert_eq!(rest.name, "unknown_options");
        assert!(rest
            .detail
            .unwrap()
            .starts_with(&format!("{} more", 2 + MAX_LISTED * 3 - MAX_LISTED)));
    }
}
