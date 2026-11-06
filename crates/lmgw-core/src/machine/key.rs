//! An OpenPGP key's fingerprint, for the plan's commands fold (machine-check
//! design §4.5): the armored key a step writes can't be checked by eye, its
//! fingerprint can, against the one NVIDIA publishes. Computed from the text
//! the step writes (the embedded key), never taken from a second constant.
//!
//! Only what a v4 fingerprint needs (RFC 4880 §6.2, §4.2, §12.2): the
//! armor's base64 body, the first packet, and SHA-1 over `0x99`, the
//! packet body's two-byte length and the body. Anything else (another
//! armor, a v5 or v6 key, a body that is not base64) has no fingerprint
//! here, and the fold shows the key without one.

use aws_lc_rs::digest::{digest, SHA1_FOR_LEGACY_USE_ONLY};
use base64::Engine;

/// `armored`'s fingerprint in groups of four upper-case hex digits
/// (`C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0`), when it is an
/// armored v4 public key.
pub(crate) fn fingerprint(armored: &str) -> Option<String> {
    let packets = dearmor(armored)?;
    let (tag, body) = first_packet(&packets)?;
    if tag != 6 || body.first() != Some(&4) {
        return None;
    }
    let len = u16::try_from(body.len()).ok()?;
    let mut input = Vec::with_capacity(body.len() + 3);
    input.push(0x99);
    input.extend_from_slice(&len.to_be_bytes());
    input.extend_from_slice(body);
    let hex: Vec<String> = digest(&SHA1_FOR_LEGACY_USE_ONLY, &input)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    Some(
        hex.chunks(2)
            .map(|pair| pair.concat())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The packets of a public-key block: the base64 between the blank line
/// after the armor headers and the `=` checksum line.
fn dearmor(armored: &str) -> Option<Vec<u8>> {
    let mut lines = armored.lines();
    lines.find(|l| l.trim_end() == "-----BEGIN PGP PUBLIC KEY BLOCK-----")?;
    // The armor headers (`Version: …`, `Comment: …`) end at a blank line.
    lines.find(|l| l.trim().is_empty())?;
    let mut body = String::new();
    for line in lines {
        let line = line.trim();
        if line.starts_with('=') || line.starts_with("-----END ") {
            break;
        }
        body.push_str(line);
    }
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

/// The first packet's tag and body, in the old or the new format. A partial
/// or indeterminate length is no key packet.
fn first_packet(bytes: &[u8]) -> Option<(u8, &[u8])> {
    let ctb = *bytes.first()?;
    if ctb & 0x80 == 0 {
        return None;
    }
    let byte = |i: usize| bytes.get(i).copied().map(usize::from);
    let (tag, len, at): (u8, usize, usize) = if ctb & 0x40 == 0 {
        let tag = (ctb >> 2) & 0x0f;
        match ctb & 0x03 {
            0 => (tag, byte(1)?, 2),
            1 => (tag, byte(1)? << 8 | byte(2)?, 3),
            2 => (
                tag,
                byte(1)? << 24 | byte(2)? << 16 | byte(3)? << 8 | byte(4)?,
                5,
            ),
            _ => return None,
        }
    } else {
        let tag = ctb & 0x3f;
        match byte(1)? {
            l @ 0..=191 => (tag, l, 2),
            l @ 192..=223 => (tag, ((l - 192) << 8) + byte(2)? + 192, 3),
            255 => (
                tag,
                byte(2)? << 24 | byte(3)? << 16 | byte(4)? << 8 | byte(5)?,
                6,
            ),
            _ => return None,
        }
    };
    Some((tag, bytes.get(at..at.checked_add(len)?)?))
}

#[cfg(test)]
mod tests {
    use lmgw_machine::plan::embedded::{NVIDIA_KEY, NVIDIA_KEY_FINGERPRINT, REPO_TEXT};

    use super::fingerprint;

    /// The embedded key's, which the plan's rpm check compares in
    /// lower case without spaces.
    #[test]
    fn the_embedded_keys_fingerprint_in_groups_of_four() {
        let fp = fingerprint(NVIDIA_KEY).expect("an armored v4 key");
        assert_eq!(fp, "C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0");
        assert_eq!(fp.replace(' ', "").to_lowercase(), NVIDIA_KEY_FINGERPRINT);
    }

    #[test]
    fn anything_but_an_armored_key_has_none() {
        assert_eq!(fingerprint(REPO_TEXT), None);
        assert_eq!(fingerprint(""), None);
        // An armor whose body is not base64, or is cut short.
        let broken =
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n!!!!\n-----END PGP PUBLIC KEY BLOCK-----\n";
        assert_eq!(fingerprint(broken), None);
        let cut: String = NVIDIA_KEY.lines().take(5).collect::<Vec<_>>().join("\n");
        assert_eq!(fingerprint(&cut), None);
    }
}
