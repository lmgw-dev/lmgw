//! Entry stems a shell or Windows would read as something else (security
//! round B, hardening 2): a stem that starts with `-` reads as an option to
//! a command given the extracted name (`rm -rf.md`), and a Windows device
//! name (`CON`, `NUL`, `COM1`, `CONIN$`, … with any extension after it) cannot be
//! made as a file there. Such a stem gets [`PREFIX`] in front; the answer's
//! `x-lmgw-documents` counts it with the renamed names.

/// What such a stem gets in front.
pub const PREFIX: &str = "_";

/// Windows' reserved device names (compared without regard to case, on the
/// name's part before its first dot, trailing spaces left out).
const DEVICES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
    "LPT9", "LPT¹", "LPT²", "LPT³", "CONIN$", "CONOUT$", "CLOCK$",
];

/// Whether `stem` needs [`PREFIX`].
pub fn needs_prefix(stem: &str) -> bool {
    if stem.starts_with('-') {
        return true;
    }
    let base = stem.split('.').next().unwrap_or(stem).trim_end_matches(' ');
    DEVICES
        .iter()
        .any(|d| d.eq_ignore_ascii_case(base) || d.to_uppercase() == base.to_uppercase())
}

/// `stem`, with [`PREFIX`] when it needs one.
pub fn guarded(stem: String) -> String {
    if needs_prefix(&stem) {
        format!("{PREFIX}{stem}")
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_device_names_get_a_prefix() {
        for s in [
            "-rf",
            "--help",
            "CON",
            "con",
            "Nul",
            "com1",
            "LPT9",
            "aux.tar",
            "CON ",
            "COM¹",
            "CONIN$",
            "conout$.md",
            "Clock$",
        ] {
            assert!(needs_prefix(s), "{s}");
            assert_eq!(guarded(s.to_string()), format!("_{s}"));
        }
        for s in [
            "report",
            "a-b",
            "console",
            "COM10",
            "LPT",
            "connect.v2",
            "file",
        ] {
            assert!(!needs_prefix(s), "{s}");
            assert_eq!(guarded(s.to_string()), s);
        }
    }
}
