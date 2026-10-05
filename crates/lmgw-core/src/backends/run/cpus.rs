//! A build's `cpus` against what podman can actually enforce.
//!
//! `cpus` becomes `podman build --cpuset-cpus`, which needs the `cpuset`
//! cgroup controller. Rootless podman only has the controllers systemd
//! delegates to `user@.service`, and a stock Fedora delegates
//! `cpu io memory pids` — so the flag failed the build after the clone, with
//! podman's own words about a cgroup file.
//!
//! The alternatives were measured and do not hold: `podman build` has no
//! `--cpus`, a CPU quota leaves `nproc` (and so `-j$(nproc)`) at every host
//! core, and a CPU affinity set on `podman build` itself (`taskset`) is not
//! inherited by the RUN steps — the runtime resets it (`nproc` printed 32 under
//! `taskset -c 0-3`). So without the controller the setting is refused, at save
//! and before a run clones anything, naming what is missing and how to
//! delegate it — never dropped or translated behind the owner's back.

use super::podman::Podman;

/// The controllers podman runs containers with (`podman info`'s
/// `Host.CgroupControllers`). Rootless, that is what `user@.service`
/// delegates.
pub(crate) async fn controllers(podman: &Podman) -> Result<Vec<String>, String> {
    let out = podman
        .run_ok(&["info", "--format", "{{json .Host.CgroupControllers}}"])
        .await?;
    parse_controllers(&out.stdout)
}

/// `["cpu","io","memory","pids"]`; `null` (no controllers at all) is empty.
fn parse_controllers(stdout: &str) -> Result<Vec<String>, String> {
    serde_json::from_str::<Option<Vec<String>>>(stdout.trim())
        .map(Option::unwrap_or_default)
        .map_err(|e| {
            format!(
                "podman info printed {:?} as its cgroup controllers: {e}",
                stdout.trim()
            )
        })
}

/// The refusal for `cpus` on a podman whose `controllers` lack `cpuset`, or
/// `None` when it can be honoured (or there is nothing to honour). Starts
/// with `cpus` so the build editor puts it under that field.
pub(crate) fn refusal(cpus: Option<&str>, controllers: &[String]) -> Option<String> {
    let cpus = cpus.map(str::trim).filter(|c| !c.is_empty())?;
    if controllers.iter().any(|c| c == "cpuset") {
        return None;
    }
    let have = match controllers.is_empty() {
        true => "none".to_string(),
        false => controllers.join(", "),
    };
    Some(format!(
        "cpus '{cpus}' needs the cpuset cgroup controller (podman build --cpuset-cpus), and \
         podman here has only: {have} — a rootless podman gets what systemd delegates to \
         user@.service: add a drop-in (e.g. /etc/systemd/system/user@.service.d/delegate.conf \
         with [Service] Delegate=cpu cpuset io memory pids), run systemctl daemon-reload and log \
         in again; or leave cpus empty to build on all cores"
    ))
}

/// The host's online CPUs, as the kernel lists them (`0-31`).
const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";

/// The `(first, last)` ranges of a CPU list (`0-7,16-23`, `3`); `None` when
/// it does not parse.
pub(crate) fn ranges(list: &str) -> Option<Vec<(u32, u32)>> {
    list.trim()
        .split(',')
        .map(|item| match item.trim().split_once('-') {
            None => item.trim().parse().ok().map(|n| (n, n)),
            Some((lo, hi)) => Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?)),
        })
        .collect()
}

/// The refusal for a `cpus` naming CPUs the host does not have online
/// (`0-63` on a 32-thread box), or `None` when every one is. podman refuses
/// such a cpuset only once the build starts, after the clone, in its own
/// words about a cgroup file.
pub(crate) fn outside_refusal(cpus: &str, online: &str) -> Option<String> {
    let have = ranges(online)?;
    let outside: Vec<&str> = cpus
        .split(',')
        .map(str::trim)
        .filter(|item| {
            let Some(&[(lo, hi)]) = ranges(item).as_deref() else {
                return true;
            };
            // Walk the online ranges from `lo`: covered when they reach `hi`
            // without a gap.
            let mut need = lo;
            loop {
                match have.iter().find(|(a, b)| *a <= need && need <= *b) {
                    Some((_, b)) if *b >= hi => return false,
                    Some((_, b)) => need = b + 1,
                    None => return true,
                }
            }
        })
        .collect();
    (!outside.is_empty()).then(|| {
        format!(
            "cpus '{cpus}' names CPUs this host does not have online ({}; online: {}) — podman \
             would refuse the build after the clone; use CPUs from the online list",
            outside.join(","),
            online.trim()
        )
    })
}

/// What a save and a run start ask: `Err` with [`refusal`]'s sentence when
/// `cpus` is set and podman lacks `cpuset`, or with [`outside_refusal`]'s
/// when it names CPUs the host does not have; `Ok(Some(note))` when podman
/// (or the kernel's CPU list) could not be asked — the flag is then passed
/// as set, and fails the build in podman's words if it cannot be honoured;
/// `Ok(None)` otherwise. Empty `cpus` never asks.
pub(crate) async fn check(podman: &Podman, cpus: Option<&str>) -> Result<Option<String>, String> {
    check_with(podman, cpus, || std::fs::read_to_string(ONLINE_CPUS)).await
}

/// [`check`] with the online CPU list read by `online`.
async fn check_with(
    podman: &Podman,
    cpus: Option<&str>,
    online: impl FnOnce() -> std::io::Result<String>,
) -> Result<Option<String>, String> {
    let Some(cpus) = cpus.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    match controllers(podman).await {
        Ok(have) => {
            if let Some(why) = refusal(Some(cpus), &have) {
                return Err(why);
            }
            match online() {
                Ok(list) => match outside_refusal(cpus, &list) {
                    Some(why) => Err(why),
                    None => Ok(None),
                },
                Err(e) => Ok(Some(format!(
                    "could not read the host's online CPUs ({ONLINE_CPUS}: {e}) — passing \
                     --cpuset-cpus as set"
                ))),
            }
        }
        Err(e) => Ok(Some(format!(
            "could not ask podman for its cgroup controllers ({e}) — passing --cpuset-cpus as set"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::registry::{CmdOutput, CommandRunner};
    use std::sync::Arc;

    #[test]
    fn the_probe_output_parses() {
        assert_eq!(
            parse_controllers("[\"cpu\",\"io\",\"memory\",\"pids\"]\n").unwrap(),
            ["cpu", "io", "memory", "pids"]
        );
        assert!(parse_controllers("null").unwrap().is_empty());
        assert!(parse_controllers("cpu io").is_err());
    }

    #[test]
    fn cpus_without_cpuset_is_refused_by_name() {
        let rootless: Vec<String> = ["cpu", "io", "memory", "pids"].map(String::from).to_vec();
        let why = refusal(Some("0-3"), &rootless).unwrap();
        assert!(
            why.starts_with("cpus '0-3' needs the cpuset cgroup controller"),
            "{why}"
        );
        assert!(why.contains("only: cpu, io, memory, pids"), "{why}");
        assert!(why.contains("Delegate=cpu cpuset io memory pids"), "{why}");
        assert!(refusal(Some("0-3"), &[]).unwrap().contains("only: none"));
        // Delegated, or nothing asked for: nothing to refuse.
        let full: Vec<String> = ["cpuset", "cpu", "io", "memory", "pids"]
            .map(String::from)
            .to_vec();
        assert_eq!(refusal(Some("0-3"), &full), None);
        assert_eq!(refusal(None, &rootless), None);
        assert_eq!(refusal(Some("  "), &rootless), None);
    }

    /// `podman info` answering with `stdout`, or failing to run.
    struct Info(Option<&'static str>);

    #[async_trait::async_trait]
    impl CommandRunner for Info {
        async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
            assert_eq!(program, "podman");
            assert_eq!(args[0], "info", "only the probe runs");
            match self.0 {
                Some(out) => Ok(CmdOutput {
                    status: 0,
                    stdout: out.into(),
                    stderr: String::new(),
                }),
                None => Err(std::io::Error::other("no podman")),
            }
        }
    }

    #[tokio::test]
    async fn check_asks_podman_only_when_cpus_is_set() {
        let podman = |out| Podman::with_runner(Arc::new(Info(out)));
        // A 32-thread host.
        async fn check(p: &Podman, cpus: Option<&str>) -> Result<Option<String>, String> {
            check_with(p, cpus, || Ok("0-31\n".to_string())).await
        }
        let rootless = Some(r#"["cpu","io","memory","pids"]"#);
        let err = check(&podman(rootless), Some("0-3")).await.unwrap_err();
        assert!(err.contains("cpuset"), "{err}");
        let full = Some(r#"["cpuset","cpu","io","memory","pids"]"#);
        assert_eq!(check(&podman(full), Some("0-3")).await, Ok(None));
        // Never asked (the fake would answer rootless): nothing set.
        assert_eq!(check(&podman(rootless), None).await, Ok(None));
        assert_eq!(check(&podman(rootless), Some("")).await, Ok(None));
        // podman cannot be asked: the flag goes as set, with a note.
        let note = check(&podman(None), Some("0-3")).await.unwrap().unwrap();
        assert!(note.contains("passing --cpuset-cpus as set"), "{note}");
        // CPUs the host does not have: refused before any clone.
        let err = check(&podman(full), Some("0-63")).await.unwrap_err();
        assert!(err.contains("(0-63; online: 0-31)"), "{err}");
        // The kernel's list cannot be read: a note, not a refusal.
        let unreadable = || Err(std::io::Error::other("no sysfs"));
        let note = check_with(&podman(full), Some("0-3"), unreadable)
            .await
            .unwrap()
            .unwrap();
        assert!(note.contains("online CPUs"), "{note}");
    }

    #[test]
    fn a_cpu_list_is_checked_against_the_online_ranges() {
        let online = "0-15,32-47";
        assert_eq!(outside_refusal("0-15", online), None);
        assert_eq!(outside_refusal("0-3,33,40-47", online), None);
        let why = outside_refusal("0-3,14-33,48", online).unwrap();
        assert!(why.contains("(14-33,48; online: 0-15,32-47)"), "{why}");
        assert_eq!(
            outside_refusal("0-3", "garbage"),
            None,
            "nothing to judge by"
        );
    }
}
