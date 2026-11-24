//! A usage row's `error_msg`, bounded where every row is written
//! (security round B, M4): an error message can repeat what a caller sent
//! (a refused option's value, an upstream's answer), and a row is kept for
//! the usage retention and read by every list of rows. Past
//! [`ERROR_MSG_MAX_BYTES`] the message is cut at a character boundary and
//! says so with the constant.

/// The most bytes of an error message a usage row keeps: 4 KiB.
pub const ERROR_MSG_MAX_BYTES: usize = 4096;

/// What of `msg` the row keeps.
pub fn bounded(msg: &str) -> std::borrow::Cow<'_, str> {
    if msg.len() <= ERROR_MSG_MAX_BYTES {
        return msg.into();
    }
    let mut end = ERROR_MSG_MAX_BYTES;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}… (cut at {ERROR_MSG_MAX_BYTES} bytes, ERROR_MSG_MAX_BYTES)",
        &msg[..end]
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_message_is_cut_and_says_so() {
        assert_eq!(bounded("short"), "short");
        let long = format!("{}é{}", "a".repeat(4095), "b".repeat(10 << 20));
        let b = bounded(&long);
        assert!(b.len() < ERROR_MSG_MAX_BYTES + 100, "{}", b.len());
        assert!(b.starts_with(&"a".repeat(4095)));
        assert!(
            b.ends_with("… (cut at 4096 bytes, ERROR_MSG_MAX_BYTES)"),
            "{b}"
        );
    }
}
