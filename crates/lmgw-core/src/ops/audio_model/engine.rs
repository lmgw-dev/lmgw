//! `audio_model_set`'s `backend` and `threads` (the per-row CPU switch):
//! what a patch may set them to, and what a save says about them.

use crate::host::HostCpu;
use crate::runtime::audio::ROW_BACKENDS;

/// The row's `backend` after a patch: `value` when supplied (blank inherits
/// the class), else `current`; `cleared` (named in `clear`) inherits too.
/// Only `cpu` is accepted: another GPU backend is the class's setting, and
/// it has to match the image the class runs.
pub(super) fn backend(
    value: Option<&str>,
    cleared: bool,
    current: Option<String>,
) -> Result<Option<String>, String> {
    if cleared {
        return Ok(None);
    }
    let Some(v) = value.map(str::trim) else {
        return Ok(current);
    };
    match v {
        "" => Ok(None),
        v if ROW_BACKENDS.contains(&v) => Ok(Some(v.to_string())),
        other => Err(format!(
            "backend: '{other}' is not a backend a row can set — only 'cpu' runs one row on the \
             CPU; another GPU backend is the class setting (audio.backend), and it must match the \
             image"
        )),
    }
}

/// The row's `threads` after a patch: `value` when supplied, else
/// `current`; `cleared` inherits (this machine's physical cores for a row
/// switched to the CPU itself, else the class's count). A count must be
/// positive; there is no upper
/// cap ([`note`] says when it exceeds the CPUs).
pub(super) fn threads(
    value: Option<i64>,
    cleared: bool,
    current: Option<i64>,
) -> Result<Option<i64>, String> {
    if cleared {
        return Ok(None);
    }
    match value {
        Some(n) if n <= 0 => Err(format!(
            "threads: {n} is not a thread count — give 1 or more, or clear it to inherit"
        )),
        Some(n) => Ok(Some(n)),
        None => Ok(current),
    }
}

/// What a save says about a count above this machine's online CPUs: it is
/// saved as given (audio.cpp runs that many threads, and they contend for
/// the cores), but the owner should know.
pub(super) fn note(threads: Option<i64>, host: HostCpu) -> Option<String> {
    let n = threads?;
    let logical = i64::try_from(host.logical_cpus).unwrap_or(i64::MAX);
    (n > logical).then(|| {
        format!(
            "threads {n} is more than this machine's {} online CPUs ({} physical cores) — saved \
             as given; the threads beyond them only contend for the same cores",
            host.logical_cpus, host.physical_cores
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::CoreSource;

    #[test]
    fn a_row_takes_cpu_or_inherits_and_nothing_else() {
        assert_eq!(backend(Some("cpu"), false, None), Ok(Some("cpu".into())));
        assert_eq!(backend(Some(" cpu "), false, None), Ok(Some("cpu".into())));
        assert_eq!(backend(Some(""), false, Some("cpu".into())), Ok(None));
        assert_eq!(
            backend(None, false, Some("cpu".into())),
            Ok(Some("cpu".into()))
        );
        assert_eq!(backend(Some("cpu"), true, None), Ok(None), "clear wins");
        let e = backend(Some("vulkan"), false, None).unwrap_err();
        assert!(e.contains("'vulkan'") && e.contains("class setting"), "{e}");
        assert!(backend(Some("cuda"), false, None).is_err());
    }

    #[test]
    fn a_thread_count_is_positive_with_no_upper_cap() {
        assert_eq!(threads(Some(8), false, None), Ok(Some(8)));
        assert_eq!(threads(None, false, Some(8)), Ok(Some(8)));
        assert_eq!(threads(Some(8), true, Some(4)), Ok(None));
        assert!(threads(Some(0), false, None).is_err());
        assert!(threads(Some(-2), false, None).is_err());
        assert_eq!(threads(Some(512), false, None), Ok(Some(512)));
        let host = HostCpu {
            physical_cores: 16,
            logical_cpus: 32,
            source: CoreSource::Topology,
        };
        assert_eq!(note(Some(32), host), None);
        assert_eq!(note(None, host), None);
        let n = note(Some(48), host).unwrap();
        assert!(n.contains("32 online CPUs") && n.contains("saved"), "{n}");
    }
}
