//! §1.3 in the server image: Podman inside the lmgw container, as its ID
//! maps show it runs on the host. [`super::super::running::podman`] is the
//! one judge, desktop and image alike; these are the image's words in it.

use std::path::Path;

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItem, MachineItemId};
use lmgw_api_types::RESTART_COMMAND;
use lmgw_machine::Facts;

use super::super::gather::PodmanFacts;
use super::super::words::item;
use super::load_image;

/// One line of `/proc/self/uid_map` or `gid_map`: IDs `inside..inside+count`
/// of the container are `outside..` of the namespace around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRange {
    pub inside: u64,
    pub outside: u64,
    pub count: u64,
}

/// The container's user and group ID maps (§7), each as read or why it
/// could not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMaps {
    pub uid: Result<Vec<IdRange>, String>,
    pub gid: Result<Vec<IdRange>, String>,
}

/// How the lmgw container runs on the host, as its uid map shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnHost {
    /// Its root is the host's root: a rootful Podman started it.
    Rootful,
    /// Its root is host uid `uid`: a rootless Podman of that user, or a
    /// rootful one with a user namespace of its own (`--userns`), which the
    /// map alone does not tell apart.
    RootIsHostUid { uid: u64 },
}

const UID_MAP: &str = "/proc/self/uid_map";
const GID_MAP: &str = "/proc/self/gid_map";

/// A map file's lines.
pub fn parse_id_map(text: &str) -> Result<Vec<IdRange>, String> {
    let ranges: Vec<IdRange> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let n: Vec<u64> = l
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .map_err(|e| format!("'{}' is not three numbers: {e}", l.trim()))?;
            match n.as_slice() {
                [inside, outside, count] => Ok(IdRange {
                    inside: *inside,
                    outside: *outside,
                    count: *count,
                }),
                _ => Err(format!("'{}' is not three numbers", l.trim())),
            }
        })
        .collect::<Result<_, String>>()?;
    if ranges.is_empty() {
        return Err("it is empty".into());
    }
    Ok(ranges)
}

impl IdMaps {
    /// Both maps of the process the check runs in.
    pub fn read(f: &dyn Facts) -> Self {
        let one = |path: &str| {
            lmgw_machine::facts::read_text(f, Path::new(path))
                .map_err(|e| format!("{path} could not be read: {e}"))
                .and_then(|t| parse_id_map(&t).map_err(|e| format!("{path}: {e}")))
        };
        Self {
            uid: one(UID_MAP),
            gid: one(GID_MAP),
        }
    }

    /// How the container runs on the host: where its root (ID 0) maps to.
    pub fn on_host(&self) -> Option<OnHost> {
        let root = self
            .uid
            .as_ref()
            .ok()?
            .iter()
            .find(|r| r.inside == 0 && r.count > 0)?;
        Some(if root.outside == 0 {
            OnHost::Rootful
        } else {
            OnHost::RootIsHostUid { uid: root.outside }
        })
    }

    /// Whether both maps have more than the one ID of the container's root:
    /// the range Podman inside needs for an image's other users. `None`
    /// when a map could not be read.
    pub fn ranged(&self) -> Option<bool> {
        let total = |m: &Result<Vec<IdRange>, String>| -> Option<u64> {
            Some(m.as_ref().ok()?.iter().map(|r| r.count).sum())
        };
        Some(total(&self.uid)? > 1 && total(&self.gid)? > 1)
    }

    /// The detail line: the maps as read.
    pub(super) fn line(&self) -> String {
        let show = |m: &Result<Vec<IdRange>, String>| match m {
            Ok(r) => r
                .iter()
                .map(|r| format!("{} {} {}", r.inside, r.outside, r.count))
                .collect::<Vec<_>>()
                .join("; "),
            Err(e) => e.clone(),
        };
        format!(
            "{UID_MAP}: {} · {GID_MAP}: {}",
            show(&self.uid),
            show(&self.gid)
        )
    }
}

/// The sentence of a uid map without a range.
const NO_RANGE: &str = "lmgw can't start models on this server yet. On the host: the service \
                        account needs subuid and subgid ranges.";

/// The step for a uid map without a range.
fn no_range_step() -> String {
    format!(
        "On the host: give the service account a range in /etc/subuid and /etc/subgid (sudo \
         usermod --add-subuids <first>-<last> --add-subgids <first>-<last> <account>, a range no \
         other account uses), run podman system migrate as that account, then restart the \
         service ({RESTART_COMMAND})"
    )
}

/// Whether the maps have no range for an image's other users (§7): a
/// problem even when `podman info` answers, since a start of such an image
/// fails.
fn no_range(p: &PodmanFacts) -> bool {
    p.id_maps.as_ref().and_then(IdMaps::ranged) == Some(false)
}

/// The detail lines of a missing Podman: the image is incomplete.
pub(in crate::machine) fn missing() -> Vec<String> {
    vec![
        "Podman is not in this server image (`podman` is not in PATH): the image is incomplete"
            .into(),
        load_image(),
    ]
}

/// Where Podman runs, for the detail's first line: inside the lmgw
/// container, and how that runs on the host.
pub(in crate::machine) fn whereabouts(p: &PodmanFacts) -> Vec<String> {
    let mut parts = vec!["inside the lmgw container".to_string()];
    match p.id_maps.as_ref().and_then(IdMaps::on_host) {
        Some(OnHost::Rootful) => parts.push("rootful on the host".into()),
        Some(OnHost::RootIsHostUid { uid }) => parts.push(format!("its root is host uid {uid}")),
        None => {}
    }
    parts
}

/// The maps' detail line, when they were read.
pub(in crate::machine) fn maps_line(p: &PodmanFacts) -> Option<String> {
    p.id_maps.as_ref().map(IdMaps::line)
}

/// The step for a Podman too old for lmgw: a current image.
pub(in crate::machine) fn too_old_step() -> String {
    load_image()
}

/// A Podman that answers, but whose maps have no range: the host's step,
/// with `detail` as read so far.
pub(in crate::machine) fn without_range(
    p: &PodmanFacts,
    mut detail: Vec<String>,
) -> Option<MachineItem> {
    if !no_range(p) {
        return None;
    }
    detail.push(no_range_step());
    Some(item(
        MachineItemId::RunningModels,
        ItemState::Problem,
        NO_RANGE,
        detail,
        Some(MachineCause::PodmanBroken {
            error: "the lmgw container's user namespace maps only its own root, so Podman \
                    inside it cannot run an image's other users"
                .into(),
        }),
    ))
}

/// Podman inside the image failed: its error, the maps, and the host's
/// step when the map has no range; never the desktop's subuid hints, which
/// are about an account inside the image.
pub(in crate::machine) fn broken(error: &str, p: &PodmanFacts) -> MachineItem {
    let no_range = no_range(p);
    let mut detail = vec![format!("Podman inside the lmgw container: {error}")];
    detail.extend(maps_line(p));
    if no_range {
        detail.push(no_range_step());
    }
    item(
        MachineItemId::RunningModels,
        ItemState::Problem,
        if no_range {
            NO_RANGE
        } else {
            "lmgw can't start models on this server yet."
        },
        detail,
        Some(MachineCause::PodmanBroken {
            error: error.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_maps_say_how_the_container_runs_on_the_host() {
        let rootless = IdMaps {
            uid: parse_id_map(
                "         0       1000          1\n         1     524288      65536\n",
            ),
            gid: parse_id_map("0 1000 1\n1 524288 65536\n"),
        };
        assert_eq!(
            rootless.on_host(),
            Some(OnHost::RootIsHostUid { uid: 1000 })
        );
        assert_eq!(rootless.ranged(), Some(true));

        let rootful = IdMaps {
            uid: parse_id_map("0 0 4294967295\n"),
            gid: parse_id_map("0 0 4294967295\n"),
        };
        assert_eq!(rootful.on_host(), Some(OnHost::Rootful));
        assert_eq!(rootful.ranged(), Some(true));

        let single = IdMaps {
            uid: parse_id_map("0 1000 1\n"),
            gid: parse_id_map("0 1000 1\n1 524288 65536\n"),
        };
        assert_eq!(
            single.ranged(),
            Some(false),
            "the uid map alone has no range"
        );

        assert!(parse_id_map("").is_err());
        assert!(parse_id_map("0 x 1").is_err());
        let unread = IdMaps {
            uid: Err("no".into()),
            gid: parse_id_map("0 0 1\n"),
        };
        assert_eq!((unread.on_host(), unread.ranged()), (None, None));
    }
}
