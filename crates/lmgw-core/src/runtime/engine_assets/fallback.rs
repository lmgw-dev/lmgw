//! The Vulkan fallback (Windows design §4.2): a CUDA build whose process
//! ended on its own, not with code 0, with CUDA's "no kernel image is
//! available" in its log has no kernels for this card, whatever its
//! recorded coverage said. Its build directory gets a mark ([`MARK`], a
//! [`Mark`]: the engine's line, how the process ended, the time), and from
//! then on the choice passes over the CUDA builds of that release
//! ([`super::choice`]): the engine runs its Vulkan build. The mark belongs
//! to that build directory, so a new pin (a new release, new coverage) is
//! tried on its own merits, and the owner can clear it
//! (`engine_fallback_clear`, the Engines page's Windows tab), after which
//! the next start tries the CUDA build again.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The mark's file name in a build directory.
pub const MARK: &str = ".lmgw-no-kernel-image";

/// What CUDA prints when a build has no kernel for the card (the CUDA
/// runtime's `cudaErrorNoKernelImageForDevice`), lower-case.
pub const NO_KERNEL_IMAGE: &str = "no kernel image is available";

/// ggml's error line for it: `ggml_cuda_error` logs `CUDA error: <the
/// runtime's text>` through ggml's log (`ggml-cuda.cu`, llama.cpp b12244),
/// which every engine prints as a line of its own ([`no_kernel_image_line`]
/// for the prefixes their loggers add).
pub const CUDA_ERROR_LINE: &str = "CUDA error: no kernel image is available";

/// What a mark records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mark {
    /// When it was written, RFC 3339.
    pub marked_at: String,
    /// How the process ended (`exited with code 1`).
    pub exit: String,
    /// The engine's line.
    pub line: String,
}

/// The log line that says so, if `log` holds one: ggml's CUDA error line
/// ([`CUDA_ERROR_LINE`]) at the start of a line, after nothing but the
/// prefixes the engines' loggers put there, read from their sources:
///
/// - ggml's own logger (audio.cpp, a llama-server without log prefixes):
///   none, the line as it is;
/// - llama.cpp's `common_log` with `--log-prefix` / `--log-timestamps`
///   (`common/log.cpp`, b12244): a `M.ss.mmm.uuu ` timestamp, then the
///   level, `E `;
/// - stable-diffusion.cpp (`src/core/util.cpp` and `examples/common/log.cpp`,
///   master-1079-a3c49b1): the level, `[E] `, then the origin, `ggml: `;
///
/// and the terminal colour codes either may put around them. A line that
/// only mentions the words somewhere else (a model's prompt echoed, a help
/// text, another function's message) is not one.
pub fn no_kernel_image_line(log: &str) -> Option<&str> {
    log.lines().map(str::trim).find(|l| is_cuda_error_line(l))
}

fn is_cuda_error_line(line: &str) -> bool {
    let line = strip_colour(line);
    let mut rest = line.as_str();
    // llama.cpp: `[timestamp ]E `.
    if let Some((stamp, after)) = rest.split_once(' ') {
        if is_timestamp(stamp) {
            rest = after;
        }
    }
    for level in ["E ", "[E] "] {
        if let Some(after) = rest.strip_prefix(level) {
            rest = after;
            break;
        }
    }
    // stable-diffusion.cpp: `ggml: `, the origin of a line ggml logged.
    let rest = rest.strip_prefix("ggml: ").unwrap_or(rest);
    rest.starts_with(CUDA_ERROR_LINE)
}

/// llama.cpp's log timestamp, `M.ss.mmm.uuu` (minutes have as many digits
/// as they need).
fn is_timestamp(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().enumerate().all(|(i, p)| {
            let width_ok = match i {
                0 => !p.is_empty(),
                1 => p.len() == 2,
                _ => p.len() == 3,
            };
            width_ok && p.bytes().all(|b| b.is_ascii_digit())
        })
}

/// `line` without its ANSI SGR sequences (`ESC[31m`, `ESC[31;1m`), wherever
/// they are.
fn strip_colour(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("\u{1b}[") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        match after.find(|c: char| !(c.is_ascii_digit() || c == ';')) {
            Some(end) if after[end..].starts_with('m') => rest = &after[end + 1..],
            _ => {
                rest = after;
                out.push_str("\u{1b}[");
            }
        }
    }
    out.push_str(rest);
    out
}

/// Mark `build_dir` with `mark`.
pub fn mark(build_dir: &Path, mark: &Mark) -> Result<(), String> {
    let body = serde_json::to_string_pretty(mark).unwrap_or_default();
    std::fs::write(build_dir.join(MARK), body)
        .map_err(|e| format!("writing {} into {}: {e}", MARK, build_dir.display()))
}

/// A mark made now for a process that ended `exit` printing `line`.
pub fn mark_now(exit: &str, line: &str) -> Mark {
    Mark {
        marked_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        exit: exit.to_string(),
        line: line.to_string(),
    }
}

/// The mark, when `build_dir` has one. A mark file that is not this
/// module's JSON (written by hand, or by an earlier lmgw) still marks the
/// build; its text is the line.
pub fn marked(build_dir: &Path) -> Option<Mark> {
    let raw = std::fs::read_to_string(build_dir.join(MARK)).ok()?;
    Some(serde_json::from_str(&raw).unwrap_or_else(|_| Mark {
        line: raw.trim().to_string(),
        ..Mark::default()
    }))
}

/// Remove `build_dir`'s mark: `Ok(false)` when it had none.
pub fn clear(build_dir: &Path) -> Result<bool, String> {
    match std::fs::remove_file(build_dir.join(MARK)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "removing {} from {}: {e}",
            MARK,
            build_dir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cuda_error_line_is_found_and_marks_the_build() {
        let log = "ggml_cuda_init: found 1 CUDA devices\n\
                   CUDA error: no kernel image is available for execution on the device\n\
                   \x20 current device: 0, in function ggml_cuda_compute_forward\n";
        let line = no_kernel_image_line(log).unwrap();
        assert!(line.starts_with("CUDA error"));
        assert_eq!(no_kernel_image_line("all fine"), None);
        let d = tempfile::tempdir().unwrap();
        assert_eq!(marked(d.path()), None);
        mark(d.path(), &mark_now("exited with code 1", line)).unwrap();
        let m = marked(d.path()).unwrap();
        assert!(m.line.contains("no kernel image"), "{m:?}");
        assert_eq!(m.exit, "exited with code 1");
        assert!(!m.marked_at.is_empty());
        assert_eq!(clear(d.path()), Ok(true));
        assert_eq!(marked(d.path()), None);
        assert_eq!(clear(d.path()), Ok(false));
    }

    #[test]
    fn only_ggmls_line_counts_not_the_words_anywhere() {
        for yes in [
            // ggml's logger.
            "CUDA error: no kernel image is available for execution on the device",
            "   CUDA error: no kernel image is available",
            // llama.cpp's common_log, prefixes and colours on.
            "0.04.123.456 E CUDA error: no kernel image is available for execution on the device",
            "\u{1b}[34m12.03.004.005\u{1b}[0m \u{1b}[31mE CUDA error: no kernel image is available\u{1b}[0m",
            "E CUDA error: no kernel image is available for execution on the device",
            "\u{1b}[31mCUDA error: no kernel image is available for execution on the device",
            // stable-diffusion.cpp, with and without colour.
            "[E] ggml: CUDA error: no kernel image is available for execution on the device",
            "\u{1b}[31;1m[E]\u{1b}[0m ggml: CUDA error: no kernel image is available",
        ] {
            assert!(no_kernel_image_line(yes).is_some(), "{yes:?}");
        }
        for no in [
            "prompt: why does CUDA say \"no kernel image is available\"?",
            "prompt: CUDA error: no kernel image is available for execution on the device",
            "ggml_cuda_compute_forward: CUDA error: no kernel image is available",
            "warning: cuda error: no kernel image is available",
            "srv  log: user said CUDA error: no kernel image is available",
            "Error: no kernel image is available",
            "I CUDA error: no kernel image is available",
            "[I] ggml: CUDA error: no kernel image is available",
            "[E] model: CUDA error: no kernel image is available",
            "1.2.3 E CUDA error: no kernel image is available",
        ] {
            assert_eq!(no_kernel_image_line(no), None, "{no}");
        }
    }

    #[test]
    fn a_mark_in_another_format_still_marks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(MARK),
            "2026-11-10T00:00:00Z\nCUDA error: no kernel image is available\n",
        )
        .unwrap();
        let m = marked(d.path()).unwrap();
        assert!(m.line.contains("CUDA error"), "{m:?}");
    }
}
