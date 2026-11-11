//! Lines Podman prints on stderr that say nothing about the call they come
//! with (headless server design §8, item 6): dropped from the stderr the
//! runner hands back, so a failed start's text quotes what Podman said
//! about the failure, and kept in the debug log. Each entry names its
//! evidence and matches the whole line exactly as Podman prints it;
//! anything else stays as Podman said it, and `podman logs` (a container's
//! own stderr) is never filtered.

/// One known-harmless line.
struct Harmless {
    /// Whether `line` is it.
    matches: fn(&str) -> bool,
    /// Where it was seen and why it says nothing.
    evidence: &'static str,
}

/// The list.
const HARMLESS: &[Harmless] = &[Harmless {
    matches: could_not_locate_device,
    evidence: "spike/headless-image/REPORT.md (2026-11-08): every GPU container the nested \
               Podman in the server image starts prints it, and the container runs with the \
               card",
}];

/// `Could not locate device <major>:<minor> on host` (the podman binary's
/// format string is `Could not locate device %s on host`), as a whole line:
/// bare, behind logrus's text prefix (`WARN[0000] `), or as logrus's
/// key-value form (`time="…" level=warning msg="…"`). Case and spelling as
/// Podman prints it; anything around it keeps the line.
fn could_not_locate_device(line: &str) -> bool {
    let message = if let Some(rest) = line.strip_prefix("WARN[") {
        match rest.split_once("] ") {
            Some((secs, msg)) if !secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit()) => {
                msg
            }
            _ => return false,
        }
    } else if let Some(rest) = line.strip_prefix("time=\"") {
        let Some(msg) = rest
            .split_once('"')
            .and_then(|(_, after)| after.strip_prefix(" level=warning msg=\""))
            .and_then(|m| m.strip_suffix('"'))
        else {
            return false;
        };
        msg
    } else {
        line
    };
    let Some(device) = message
        .strip_prefix("Could not locate device ")
        .and_then(|m| m.strip_suffix(" on host"))
    else {
        return false;
    };
    device.split_once(':').is_some_and(|(major, minor)| {
        !major.is_empty()
            && !minor.is_empty()
            && major.bytes().all(|b| b.is_ascii_digit())
            && minor.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The verbs whose stderr is handed back raw: `podman logs` prints the
/// container's own stderr there, which the caller asked for as it is
/// (`Registry::logs_tail`).
const RAW: &[&str] = &["logs"];

/// `stderr` of `program args` without the known-harmless lines, each
/// dropped line logged at debug with its evidence. Unchanged when it has
/// none, or when the verb's stderr is the container's own ([`RAW`]).
pub fn without_harmless(program: &str, args: &[String], stderr: &str) -> String {
    if args.first().is_some_and(|a| RAW.contains(&a.as_str())) || !stderr.lines().any(is_harmless) {
        return stderr.to_string();
    }
    let mut kept = String::with_capacity(stderr.len());
    for line in stderr.split_inclusive('\n') {
        match HARMLESS
            .iter()
            .find(|h| (h.matches)(line.trim_end_matches(['\n', '\r'])))
        {
            Some(h) => tracing::debug!(
                "{program}: dropped a known-harmless stderr line ({}): {}",
                h.evidence,
                line.trim_end()
            ),
            None => kept.push_str(line),
        }
    }
    kept
}

fn is_harmless(line: &str) -> bool {
    HARMLESS.iter().any(|h| (h.matches)(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(stderr: &str) -> String {
        without_harmless("podman", &["run".into()], stderr)
    }

    /// The known line is dropped in each of Podman's spellings; every other
    /// line stays as Podman said it, in its order.
    #[test]
    fn the_known_line_is_dropped_and_every_other_kept() {
        for noise in [
            "Could not locate device 195:254 on host",
            "WARN[0000] Could not locate device 195:254 on host",
            "time=\"2026-11-08T10:00:00Z\" level=warning msg=\"Could not locate device 195:254 on host\"",
        ] {
            let stderr = format!("{noise}\nError: crun: executable file not found\n");
            assert_eq!(
                run(&stderr),
                "Error: crun: executable file not found\n",
                "{noise}"
            );
        }
        let two = "WARN[0000] Could not locate device 195:254 on host\nfirst\n\
                   WARN[0000] Could not locate device 195:255 on host\nsecond";
        assert_eq!(run(two), "first\nsecond");
        for kept in [
            "Error: Could not locate device 195:254 on host: and then it failed",
            "Error: Could not locate device 195:254 on host",
            "could not locate device 195:254 on host",
            "WARN[0000] could not locate device 195:254 on host",
            "ERRO[0000] Could not locate device 195:254 on host",
            "WARN[] Could not locate device 195:254 on host",
            "time=\"x\" level=error msg=\"Could not locate device 195:254 on host\"",
            "the container said: Could not locate device 195:254 on host",
            "Could not locate device nvidia0 on host",
            "Could not locate device 195: on host",
            "Could not locate device 195:254 in the container",
            "Error: image not known",
            "",
        ] {
            assert_eq!(run(kept), kept, "{kept}");
        }
    }

    /// `podman logs` hands the container's stderr back as it is: a model
    /// printing the line keeps it in its tail.
    #[test]
    fn a_logs_tail_is_never_filtered() {
        let tail = "WARN[0000] Could not locate device 195:254 on host\nloading model\n";
        assert_eq!(
            without_harmless(
                "podman",
                &["logs".into(), "--tail".into(), "50".into()],
                tail
            ),
            tail
        );
        assert_eq!(run(tail), "loading model\n");
    }
}
