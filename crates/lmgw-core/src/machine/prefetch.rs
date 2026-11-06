//! The pre-fetch (machine-check design §5, D16's second half, D72, WP11):
//! once the machine check's verdict turns `ready` — the boot run's, or a
//! later check's — lmgw begins the download of the engine image of every
//! class that has a model on the graphics card, when it is not on this
//! machine yet, so a first message only loads its model instead of sitting
//! through a download that a client's own timeout may end first.
//!
//! - **Once per turn to `ready`**, not on every check: `ready` after
//!   anything else (or after nothing, at boot) begins it
//!   ([`super::MachineChecks`]' publish); `ready` again does not.
//! - **Which images** (D16, D72): the class's Image setting, for each class
//!   with an enabled row placed on the graphics card, each once. A row on
//!   the processor, a disabled row and a class without rows add nothing; a
//!   row's own image (an override named on the row) is not fetched yet: its
//!   first start downloads it.
//! - **Not on a metered connection** (D72): NetworkManager's say for the
//!   device the default route leaves through (`/proc/net/route`, then
//!   `nmcli -t -f GENERAL.METERED device show <device>` within the load
//!   timeout) — "yes", guessed or not, skips the whole pre-fetch with a log
//!   line saying so; anything it cannot tell counts as not metered, and the
//!   log says why. A switch to turn the pre-fetch off is not built yet.
//! - **How:** each image is asked of podman (`podman image exists`, bounded
//!   as a model download asks it, `web::hf::engine::within`); one that is
//!   there is left alone, one podman cannot say anything about is left
//!   alone and logged, and a missing one gets the `image_pull` job a start
//!   would use (`pull::begin`, keyed `image:<reference>`), so a first start
//!   or an Engines-page pull joins it instead of downloading again. A
//!   `localhost/` build is never pulled (`pull::begin` refuses it, and the
//!   log says why). Nothing waits for a pull and nothing is started:
//!   pulling an image is not loading a model (the 2026-10-13 ruling).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use lmgw_machine::Facts;

use crate::backends::pull::{self, Presence};
use crate::runtime::descriptor::model_runtimes;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::web::hf::engine::{within, PRESENCE_BOUND};

/// What the pre-fetch did about one engine image that was not plainly
/// here, or why it fetched nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prefetched {
    /// Its pull began, or joined the one under way: the job.
    Pulling { image: String, job: i64 },
    /// It was not fetched, and why: podman could not say whether it is here,
    /// or its pull could not begin (a `localhost/` build).
    Left { image: String, why: String },
    /// Nothing was fetched: the connection is metered (the device, and what
    /// NetworkManager says).
    Metered { how: String },
}

/// NetworkManager's command line, where Fedora installs it.
pub(super) const NMCLI: &str = "/usr/bin/nmcli";

/// The kernel's IPv4 routes, the default one among them.
const ROUTES: &str = "/proc/net/route";

/// The engine images to fetch (module doc): each class's Image setting, for
/// the classes with an enabled row on the graphics card.
fn wanted(state: &SharedState) -> BTreeSet<String> {
    let snap = state.snapshot();
    let s = &snap.settings;
    model_runtimes(&snap)
        .into_iter()
        .filter(|r| r.enabled && r.placement().is_gpu())
        .map(|r| match r.class {
            Class::Chat => s.router.image.clone(),
            Class::Aux => s.aux_router.image.clone(),
            Class::Audio => s.audio.image.clone(),
            Class::Image => s.image.image.clone(),
        })
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty())
        .collect()
}

/// Whether the connection is metered (module doc). The boot's resume of
/// the downloads a shutdown cut asks the same (find-a-model design D122,
/// [`connection_metered`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Metered {
    /// Metered: the device and NetworkManager's word for it.
    Yes(String),
    No,
    /// Not known, and why: it counts as not metered.
    Unknown(String),
}

/// The device the default IPv4 route leaves through (the lowest metric of
/// the routes to `0.0.0.0/0` that are up), from `/proc/net/route`.
fn default_device(f: &dyn Facts) -> Result<String, String> {
    let text = lmgw_machine::facts::read_text(f, Path::new(ROUTES))
        .map_err(|e| format!("{ROUTES} could not be read: {e}"))?;
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let c: Vec<&str> = l.split_whitespace().collect();
            let up = c
                .get(3)
                .and_then(|f| u32::from_str_radix(f, 16).ok())
                .is_some_and(|flags| flags & 1 == 1);
            (c.len() > 6 && c[1] == "00000000" && up)
                .then(|| (c[6].parse::<u64>().unwrap_or(u64::MAX), c[0].to_string()))
        })
        .min()
        .map(|(_, device)| device)
        .ok_or_else(|| format!("{ROUTES} has no default route"))
}

/// What NetworkManager says of the default route's device, within `limit`.
fn metered(f: &dyn Facts, limit: Duration) -> Metered {
    let device = match default_device(f) {
        Ok(d) => d,
        Err(why) => return Metered::Unknown(why),
    };
    let args = [
        "-t",
        "-f",
        "GENERAL.METERED",
        "device",
        "show",
        device.as_str(),
    ];
    let out = match f.run_limited(Path::new(NMCLI), &args, limit) {
        Ok(out) if out.ok() => out,
        Ok(out) => {
            return Metered::Unknown(format!(
                "nmcli {} exited {}: {}",
                args.join(" "),
                out.status_text(),
                out.stderr.trim()
            ))
        }
        Err(e) => {
            return Metered::Unknown(super::gather::limited(
                &format!("nmcli {}", args.join(" ")),
                &e,
                limit,
            ))
        }
    };
    let said = out
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("GENERAL.METERED:"))
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    match said.split_whitespace().next() {
        Some("yes") => Metered::Yes(format!("{device}: {said}")),
        Some("no") => Metered::No,
        _ => Metered::Unknown(format!(
            "NetworkManager says '{said}' for {device}, which is neither yes nor no"
        )),
    }
}

/// What NetworkManager says of the connection the default route leaves
/// through (module doc), asked of the state's machine within the load
/// timeout on the blocking pool.
pub async fn connection_metered(state: &SharedState) -> Metered {
    let (f, limit) = (
        state.machine.facts(),
        crate::runtime::cdi::load_timeout(state),
    );
    tokio::task::spawn_blocking(move || metered(&*f, limit))
        .await
        .unwrap_or_else(|e| Metered::Unknown(format!("asking NetworkManager stopped: {e}")))
}

/// Begin the pull of every missing image the classes with a GPU row need
/// (module doc), and say what was done about each image that was not here
/// — or that nothing was fetched on a metered connection.
pub(super) async fn run(state: &SharedState) -> Vec<Prefetched> {
    match connection_metered(state).await {
        Metered::Yes(how) => {
            tracing::info!(
                "the machine check is ready; the engine pre-fetch is skipped: metered connection \
                 ({how}) — each model's first start downloads its engine"
            );
            return vec![Prefetched::Metered { how }];
        }
        Metered::No => {}
        Metered::Unknown(why) => tracing::info!(
            "the machine check is ready; whether the connection is metered is not known ({why}), \
             so it counts as not metered and the engine pre-fetch goes ahead"
        ),
    }
    let mut out = Vec::new();
    for image in wanted(state) {
        match within(PRESENCE_BOUND, &image, pull::presence(state, &image)).await {
            Presence::Present => continue,
            Presence::Unknown(why) => {
                tracing::warn!(
                    "the machine check is ready: whether {image} is on this machine is not known \
                     ({why}), so it is not fetched now; its model's first start looks again"
                );
                out.push(Prefetched::Left { image, why });
            }
            Presence::Missing => match pull::begin(state, &image).await {
                Ok(job) => {
                    tracing::info!(
                        "the machine check is ready: {image}, which a configured model runs in, \
                         is not on this machine — downloading it now (job {job}); nothing is \
                         started"
                    );
                    out.push(Prefetched::Pulling { image, job });
                }
                Err(failed) => {
                    let why = format!("{image} is not on this machine, and {failed}");
                    tracing::warn!("the machine check is ready, and {why}");
                    out.push(Prefetched::Left { image, why });
                }
            },
        }
    }
    if out.is_empty() {
        tracing::debug!(
            "the machine check is ready, and the engine of every class with a model on the \
             graphics card is on this machine"
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_machine::fake::FakeFacts;
    use lmgw_machine::CmdOut;

    const TABLE: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                         wlp1s0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                         eth0\t00000000\t010200C0\t0003\t0\t0\t425\t00000000\t0\t0\t0\n\
                         eth0\t000200C0\t00000000\t0001\t0\t0\t425\t00FFFFFF\t0\t0\t0\n\
                         down0\t00000000\t010200C0\t0002\t0\t0\t1\t00000000\t0\t0\t0\n";

    fn said(stdout: &'static str) -> FakeFacts {
        let f = FakeFacts::default();
        f.file(ROUTES, TABLE);
        f.program(NMCLI, move |args| {
            assert_eq!(
                args,
                ["-t", "-f", "GENERAL.METERED", "device", "show", "eth0"],
                "the default route's device: the lowest metric that is up"
            );
            Ok(CmdOut::success(stdout))
        });
        f
    }

    /// NetworkManager's yes, guessed or not, is metered; its no is not;
    /// anything else, no default route or no nmcli is not known.
    #[test]
    fn what_networkmanager_says_of_the_default_routes_device() {
        let limit = Duration::from_secs(5);
        assert_eq!(
            metered(&said("GENERAL.METERED:yes (guessed)\n"), limit),
            Metered::Yes("eth0: yes (guessed)".into())
        );
        assert_eq!(
            metered(&said("GENERAL.METERED:yes\n"), limit),
            Metered::Yes("eth0: yes".into())
        );
        assert_eq!(
            metered(&said("GENERAL.METERED:no (guessed)\n"), limit),
            Metered::No
        );
        assert!(matches!(
            metered(&said("GENERAL.METERED:unknown\n"), limit),
            Metered::Unknown(why) if why.contains("'unknown'")
        ));
        let none = FakeFacts::default();
        none.file(
            ROUTES,
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\n",
        );
        assert!(matches!(
            metered(&none, limit),
            Metered::Unknown(why) if why.contains("no default route")
        ));
        let no_nmcli = FakeFacts::default();
        no_nmcli.file(ROUTES, TABLE);
        assert!(matches!(metered(&no_nmcli, limit), Metered::Unknown(_)));
    }
}
