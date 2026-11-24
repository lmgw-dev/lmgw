//! A whole-number [`Spin`] read and shown as an `i64`: an `f64` holds every
//! integer only up to 2^53, and a [`super::NumberRow`]'s value (a seed, a
//! byte count) may go past that.

use super::{group, Spin, SpinError};

impl Spin {
    /// `n` as [`Spin::show`] shows a whole number, every digit kept.
    pub fn show_whole(&self, n: i64) -> String {
        group(&n.to_string())
    }

    /// What was typed, as a whole number, the bounds aside: the marks read
    /// as [`Spin::read`] reads them, and no digit lost on the way. A point
    /// followed by zeros only ("5.0") is still whole; a number past the
    /// `i64` range is not a number here.
    pub fn read_whole(&self, text: &str) -> Result<i64, SpinError> {
        let plain = self.plain(text)?;
        let (int, frac) = plain.split_once('.').unwrap_or((&plain, ""));
        if frac.chars().any(|c| c != '0') {
            return Err(SpinError::NotWhole);
        }
        match int {
            "" | "-" => Ok(0),
            _ => int.parse::<i64>().map_err(|_| SpinError::NotANumber),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOLE: Spin = Spin {
        step: 1.0,
        min: None,
        max: None,
        digits: 0,
    };

    #[test]
    fn whole_numbers_past_two_to_the_53_keep_every_digit() {
        // 2^53 + 1, which an f64 rounds to 2^53.
        assert_eq!(
            WHOLE.read_whole("9007199254740993"),
            Ok(9_007_199_254_740_993)
        );
        assert_eq!(
            WHOLE.read_whole("9,007,199,254,740,993"),
            Ok(9_007_199_254_740_993)
        );
        assert_eq!(
            WHOLE.read_whole("9\u{2009}223\u{2009}372\u{2009}036\u{2009}854\u{2009}775\u{2009}807"),
            Ok(i64::MAX)
        );
        assert_eq!(WHOLE.read_whole("-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(
            WHOLE.show_whole(9_007_199_254_740_993),
            "9\u{2009}007\u{2009}199\u{2009}254\u{2009}740\u{2009}993"
        );
        assert_eq!(WHOLE.show_whole(-1_024), "-1\u{2009}024");
    }

    #[test]
    fn a_whole_read_refuses_what_read_refuses() {
        assert_eq!(WHOLE.read_whole("5.0"), Ok(5));
        assert_eq!(WHOLE.read_whole("0.5"), Err(SpinError::NotWhole));
        assert_eq!(WHOLE.read_whole("auto"), Err(SpinError::NotANumber));
        assert_eq!(
            WHOLE.read_whole("9223372036854775808"),
            Err(SpinError::NotANumber)
        );
    }
}
