//! The codepoints of `x-lmgw-speech`'s `chars=` part (review TC-9).
//!
//! Every distinct codepoint costs up to 8 bytes of header, and a long text
//! in a script the vocabulary lacks has hundreds: a header of several
//! kilobytes, which an 8 KB reverse-proxy buffer or Node's 16 KB limit
//! would refuse along with the whole answer. So each list names its first
//! [`HEADER_CODEPOINTS`] and then says how many it left out — `+<n> more`,
//! in the header itself — and the full list is in lmgw's log line for the
//! request (`proxy::audio::speech`, `proxy::synthesize`).

/// The most codepoints one list of the `chars=` part names: about 500
/// bytes, so both lists stay near a kilobyte.
pub const HEADER_CODEPOINTS: usize = 64;

/// `U+201E/U+2028`, and `/+<n> more` past [`HEADER_CODEPOINTS`].
pub(super) fn listed(cps: &[u32]) -> String {
    let mut parts: Vec<String> = cps
        .iter()
        .take(HEADER_CODEPOINTS)
        .map(|c| format!("U+{c:04X}"))
        .collect();
    if cps.len() > HEADER_CODEPOINTS {
        parts.push(format!("+{} more", cps.len() - HEADER_CODEPOINTS));
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_says_how_many_it_left_out() {
        assert_eq!(listed(&[0x201E, 0x2028]), "U+201E/U+2028");
        let all: Vec<u32> = (0x4E00..0x4E00 + 1000).collect();
        let l = listed(&all);
        assert!(l.starts_with("U+4E00/U+4E01/"), "{l}");
        assert!(l.ends_with("/U+4E3F/+936 more"), "{l}");
        assert_eq!(l.split('/').count(), HEADER_CODEPOINTS + 1);
        assert!(l.len() < 600, "{}", l.len());
        let exact: Vec<u32> = (0..HEADER_CODEPOINTS as u32).collect();
        assert!(!listed(&exact).contains("more"));
    }
}
