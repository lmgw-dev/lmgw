//! The embedded key and repository text (§6, "The embedded key"): the key's
//! fingerprint, computed here from its own packets, is NVIDIA's; the
//! repository text is §3.2's, byte for byte.

use base64::Engine;
use lmgw_machine::plan::embedded::{
    KEY_URL, NVIDIA_KEY, NVIDIA_KEY_FINGERPRINT, NVIDIA_KEY_ID, REPO_ID, REPO_TEXT, TOOLKIT_BASE,
};
use lmgw_machine::repos::{parse_bool, parse_ini};
use sha1::{Digest, Sha1};

/// The packets of an ASCII-armored block: the base64 between the blank line
/// after the armor headers and the `=` checksum line.
fn dearmor(armored: &str) -> Vec<u8> {
    let mut body = String::new();
    let mut in_body = false;
    for line in armored.lines() {
        if line.starts_with("-----BEGIN ") {
            continue;
        }
        if !in_body {
            in_body = line.trim().is_empty();
            continue;
        }
        if line.starts_with('=') || line.starts_with("-----END ") {
            break;
        }
        body.push_str(line.trim());
    }
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .expect("the armor is base64")
}

/// The first packet's tag and body (RFC 4880 §4.2: old and new formats).
fn first_packet(bytes: &[u8]) -> (u8, &[u8]) {
    let ctb = bytes[0];
    assert!(ctb & 0x80 != 0, "not a packet");
    if ctb & 0x40 == 0 {
        let tag = (ctb >> 2) & 0x0f;
        let (len, at) = match ctb & 0x03 {
            0 => (usize::from(bytes[1]), 2),
            1 => (usize::from(u16::from_be_bytes([bytes[1], bytes[2]])), 3),
            2 => (
                u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize,
                5,
            ),
            _ => panic!("an indeterminate length"),
        };
        (tag, &bytes[at..at + len])
    } else {
        let tag = ctb & 0x3f;
        let (len, at) = match bytes[1] {
            l @ 0..=191 => (usize::from(l), 2),
            l @ 192..=223 => (
                (usize::from(l) - 192) * 256 + usize::from(bytes[2]) + 192,
                3,
            ),
            255 => (
                u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as usize,
                6,
            ),
            _ => panic!("a partial length"),
        };
        (tag, &bytes[at..at + len])
    }
}

/// A v4 key's fingerprint: SHA-1 over 0x99, the body's two-byte length and
/// the public-key packet's body (RFC 4880 §12.2).
fn v4_fingerprint(body: &[u8]) -> String {
    assert_eq!(body[0], 4, "a version 4 key");
    let mut h = Sha1::new();
    h.update([0x99]);
    h.update((body.len() as u16).to_be_bytes());
    h.update(body);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_embedded_key_is_nvidias() {
    let packets = dearmor(NVIDIA_KEY);
    let (tag, body) = first_packet(&packets);
    assert_eq!(tag, 6, "a public-key packet first");
    let fp = v4_fingerprint(body);
    assert_eq!(fp, "c95b321b61e88c1809c4f759ddcae044f796ecb0");
    assert_eq!(fp, NVIDIA_KEY_FINGERPRINT);
    assert!(fp.ends_with(NVIDIA_KEY_ID));
    // The user ID it carries, for the eye.
    assert!(packets
        .windows(b"cudatools@nvidia.com".len())
        .any(|w| w == b"cudatools@nvidia.com"));
}

/// §3.2's repository file, to the letter.
#[test]
fn the_repository_text_is_the_designs() {
    assert_eq!(
        REPO_TEXT,
        "# Written by lmgw. lmgw replaces this file when its text or NVIDIA's key changes.\n\
         [lmgw-nvidia-container-toolkit]\n\
         name=NVIDIA Container Toolkit (added by lmgw)\n\
         baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch\n\
         includepkgs=nvidia-container-toolkit-base\n\
         repo_gpgcheck=1\n\
         gpgcheck=1\n\
         enabled=1\n\
         gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit\n"
    );
    let sections = parse_ini(REPO_TEXT);
    assert_eq!(sections.len(), 1, "no experimental section");
    assert_eq!(sections[0].header, REPO_ID);
    let get = |k: &str| {
        sections[0]
            .entries
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("includepkgs"), Some(TOOLKIT_BASE));
    assert_eq!(get("gpgcheck").and_then(parse_bool), Some(true));
    assert_eq!(get("repo_gpgcheck").and_then(parse_bool), Some(true));
    assert_eq!(get("gpgkey"), Some(KEY_URL));
    assert_eq!(
        get("skip_if_unavailable"),
        None,
        "system updates keep working"
    );
}
