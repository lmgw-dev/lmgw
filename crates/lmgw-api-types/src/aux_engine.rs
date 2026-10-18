//! An aux row's thread count (decision-models design §3.3): the rule the
//! gateway renders `--threads` by, shared so the aux editor shows a draft
//! the count the gateway would start it with.
//!
//! The row's own `threads` wins. Without one, a `--threads`/`-t` in its
//! freeform args renders as written: the renderer claims the key only for a
//! count it renders itself, so the owner's count is never dropped. A row on
//! the CPU that names neither runs this machine's physical cores (the
//! logical CPU count where the core topology cannot be read), as an audio
//! row switched to the CPU does. A GPU row that names neither renders no
//! `--threads`, and llama-server picks its own.

use serde::{Deserialize, Serialize};

use crate::HostCpu;

/// Where an aux row's thread count comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum AuxThreadsSource {
    /// The row's own `threads`.
    Row,
    /// A `--threads`/`-t` in the row's extra args, rendered as written.
    Args,
    /// This machine's physical cores: a row on the CPU that names no count.
    Cores,
    /// [`Self::Cores`] where the core topology could not be read: the
    /// logical CPU count stands in.
    LogicalCpus,
    /// No count: a GPU row that names none renders no `--threads`, and
    /// llama-server picks.
    LlamaServer,
}

impl AuxThreadsSource {
    /// The count is rendered from a field (the row's, or the host's for a
    /// CPU row), so the renderer claims `--threads`; an args count renders
    /// as written, and llama-server's own needs no flag.
    pub fn rendered(self) -> bool {
        matches!(self, Self::Row | Self::Cores | Self::LogicalCpus)
    }

    /// How a surface names the source ("16 · physical cores").
    pub fn label(self) -> &'static str {
        match self {
            Self::Row => "set on the row",
            Self::Args => "from the extra args",
            Self::Cores => "physical cores",
            Self::LogicalCpus => "logical CPUs, topology unreadable",
            Self::LlamaServer => "llama-server's default",
        }
    }
}

/// The thread count a row with `threads`, placement `on_cpu` and freeform
/// `args` runs with on `host`, and where it comes from (module doc). `None`
/// when nothing names a count, or the args name one that is not a number.
pub fn threads_in_effect(
    threads: Option<i64>,
    on_cpu: bool,
    args: &[String],
    host: &HostCpu,
) -> (Option<i64>, AuxThreadsSource) {
    if let Some(n) = threads {
        return (Some(n), AuxThreadsSource::Row);
    }
    if let Some(n) = args_threads(args) {
        return (n, AuxThreadsSource::Args);
    }
    if !on_cpu {
        return (None, AuxThreadsSource::LlamaServer);
    }
    let source = match host.source.as_str() {
        "logical" => AuxThreadsSource::LogicalCpus,
        _ => AuxThreadsSource::Cores,
    };
    (Some(i64::from(host.physical_cores)), source)
}

/// The thread count freeform llama-server args set (`--threads N`, `-t N`,
/// `--threads=N`): `None` when they set none, `Some(None)` when the value is
/// missing or not a number. The last one wins, as llama-server reads them.
pub fn args_threads(args: &[String]) -> Option<Option<i64>> {
    let mut found = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if !a.starts_with('-') {
            continue;
        }
        let flag = a.trim_start_matches('-');
        let (key, inline) = match flag.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (flag, None),
        };
        if !matches!(key, "threads" | "t") {
            continue;
        }
        let value = match inline {
            Some(v) => Some(v),
            None => it.next().map(String::as_str),
        };
        found = Some(value.and_then(|v| v.trim().parse::<i64>().ok()));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(cores: u32, source: &str) -> HostCpu {
        HostCpu {
            physical_cores: cores,
            logical_cpus: cores * 2,
            source: source.into(),
        }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_row_s_own_count_wins_over_the_args_and_the_host() {
        let h = host(16, "topology");
        let a = args(&["--threads", "4"]);
        assert_eq!(
            threads_in_effect(Some(8), true, &a, &h),
            (Some(8), AuxThreadsSource::Row)
        );
        assert_eq!(
            threads_in_effect(Some(8), false, &[], &h),
            (Some(8), AuxThreadsSource::Row)
        );
    }

    #[test]
    fn an_args_count_renders_as_written_on_either_placement() {
        let h = host(16, "topology");
        for a in [
            args(&["--threads", "4"]),
            args(&["-t", "4"]),
            args(&["--threads=4"]),
            args(&["--threads", "2", "--ctx-size", "9", "-t", "4"]),
        ] {
            for on_cpu in [true, false] {
                let got = threads_in_effect(None, on_cpu, &a, &h);
                assert_eq!(
                    got,
                    (Some(4), AuxThreadsSource::Args),
                    "{a:?} on_cpu {on_cpu}"
                );
                assert!(!got.1.rendered());
            }
        }
        assert_eq!(
            threads_in_effect(None, true, &args(&["--threads"]), &h),
            (None, AuxThreadsSource::Args),
            "a flag without its value still names the count, which llama-server then refuses"
        );
        assert_eq!(
            args_threads(&args(&["-tb", "4", "--threads-batch", "4"])),
            None
        );
        assert_eq!(args_threads(&args(&["4", "t"])), None);
    }

    #[test]
    fn a_cpu_row_runs_the_cores_and_a_gpu_row_leaves_it_to_llama_server() {
        assert_eq!(
            threads_in_effect(None, true, &[], &host(16, "topology")),
            (Some(16), AuxThreadsSource::Cores)
        );
        assert_eq!(
            threads_in_effect(None, true, &[], &host(32, "logical")),
            (Some(32), AuxThreadsSource::LogicalCpus)
        );
        let gpu = threads_in_effect(None, false, &[], &host(16, "topology"));
        assert_eq!(gpu, (None, AuxThreadsSource::LlamaServer));
        assert!(!gpu.1.rendered());
        assert!(AuxThreadsSource::Cores.rendered() && AuxThreadsSource::Row.rendered());
    }

    #[test]
    fn the_source_is_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_value(AuxThreadsSource::LogicalCpus).unwrap(),
            "logical_cpus"
        );
        assert_eq!(
            serde_json::to_value(AuxThreadsSource::LlamaServer).unwrap(),
            "llama_server"
        );
    }
}
