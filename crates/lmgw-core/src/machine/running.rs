//! §1.3 running models (Podman), §1.5 room for models and §1.6 the
//! download for the card.

use std::collections::BTreeMap;

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItem, MachineItemId};

use super::gather::{Images, PodmanFacts, Room};
use super::words::item;
use crate::backends::pull::Presence;
use crate::hf::fmt_bytes;

/// `path` with the home directory written as `~`.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

/// §1.3's item, and whether it is a problem.
pub(super) fn podman(p: &PodmanFacts) -> (MachineItem, bool) {
    let id = MachineItemId::RunningModels;
    let version = match &p.version {
        Err(None) => {
            return (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw needs one more program to run models.",
                    vec![
                        "Podman is not installed (`podman` is not in PATH). On Fedora: sudo dnf \
                         install podman"
                            .into(),
                    ],
                    Some(MachineCause::PodmanMissing),
                ),
                true,
            )
        }
        Err(Some(e)) => {
            return (broken(e, p), true);
        }
        Ok(v) => v.clone(),
    };
    let info = match &p.info {
        Ok(i) => i,
        Err(e) => return (broken(e, p), true),
    };
    let mut parts = vec![version.clone()];
    match info.rootless {
        Some(true) => parts.push("rootless".into()),
        Some(false) => parts.push("as root".into()),
        None => {}
    }
    if let Some(r) = &info.oci_runtime {
        parts.push(r.clone());
    }
    if let Some(c) = &info.cgroup_version {
        parts.push(format!("cgroup {c}"));
    }
    if let Some(g) = &info.graph_root {
        parts.push(format!("storage {}", tilde(g)));
    }
    let mut detail = vec![parts.join(", ")];
    if p.has_option == Some(false) {
        detail.push(format!(
            "`podman --help` lists no {}, so lmgw's models read the system's description of the \
             graphics card instead of lmgw's own",
            crate::runtime::cdi::SPEC_DIR_OPTION
        ));
        return (
            item(
                id,
                ItemState::Problem,
                "lmgw needs a newer version of one program to run models.",
                detail,
                Some(MachineCause::PodmanTooOld { version }),
            ),
            true,
        );
    }
    if p.has_option.is_none() {
        detail.push("`podman --help` did not answer".into());
    }
    (
        item(
            id,
            ItemState::Ok,
            "lmgw can run models on this computer.",
            detail,
            None,
        ),
        false,
    )
}

fn broken(error: &str, p: &PodmanFacts) -> MachineItem {
    let mut detail = vec![format!("Podman: {error}")];
    if let Some((uid, gid)) = p.subids {
        for (file, has) in [("/etc/subuid", uid), ("/etc/subgid", gid)] {
            if !has {
                detail.push(format!(
                    "this account has no range in {file}: rootless Podman needs one (accounts \
                     from a directory service often lack it; an administrator adds it with \
                     usermod --add-subuids / --add-subgids)"
                ));
            }
        }
    }
    item(
        MachineItemId::RunningModels,
        ItemState::Problem,
        "lmgw can't start models on this computer yet.",
        detail,
        Some(MachineCause::PodmanBroken {
            error: error.to_string(),
        }),
    )
}

/// What [`super::gather`] calls Podman's image store in a [`Room`].
pub(super) const SOFTWARE: &str = "the software that runs models";

/// §1.5's item: free space where models and their software are kept, one
/// figure per file system. Information only, and never a threshold lmgw
/// made up: a low figure is named only against a size lmgw knows — what the
/// download for the card still fetches ([`Download::remaining`]), against
/// the file system of the store it goes into.
pub(super) fn room(rooms: &[Room], download: Option<u64>) -> MachineItem {
    let id = MachineItemId::Room;
    let mut detail = Vec::new();
    let mut seen: Vec<(u64, u64, Vec<&'static str>)> = Vec::new();
    for r in rooms {
        match &r.space {
            Ok(s) => {
                detail.push(format!(
                    "{}: {} · {} free of {}",
                    r.what,
                    tilde(&r.path),
                    fmt_bytes(s.free_bytes),
                    fmt_bytes(s.total_bytes)
                ));
                match seen.iter_mut().find(|(fs, ..)| *fs == s.fs_id) {
                    Some((_, _, whats)) => whats.push(r.what),
                    None => seen.push((s.fs_id, s.free_bytes, vec![r.what])),
                }
            }
            Err(e) => detail.push(format!("{}: {} · {e}", r.what, tilde(&r.path))),
        }
    }
    if let (Some(needs), Some(free)) = (
        download,
        rooms
            .iter()
            .find(|r| r.what == SOFTWARE)
            .and_then(|r| r.space.as_ref().ok())
            .map(|s| s.free_bytes),
    ) {
        if free < needs {
            return item(
                id,
                ItemState::Info,
                &format!(
                    "Space is running low: {} free. The download for your graphics card needs {}.",
                    fmt_bytes(free),
                    fmt_bytes(needs)
                ),
                detail,
                None,
            );
        }
    }
    let sentence = match seen.as_slice() {
        [] if rooms.is_empty() => {
            return item(
                id,
                ItemState::NotChecked,
                "Not checked yet: no folder for models is set.",
                detail,
                None,
            )
        }
        [] => {
            return item(
                id,
                ItemState::Info,
                "lmgw couldn't read how much room is left. The details say why.",
                detail,
                None,
            )
        }
        [(_, free, _)] => format!("Room for models · {} free.", fmt_bytes(*free)),
        many => {
            // Where the models are, then where only their software is.
            let software = |whats: &[&str]| whats.iter().all(|w| *w == SOFTWARE);
            let models: Vec<String> = many
                .iter()
                .filter(|(_, _, w)| !software(w))
                .map(|(_, free, _)| fmt_bytes(*free))
                .collect();
            let rest: Vec<String> = many
                .iter()
                .filter(|(_, _, w)| software(w))
                .map(|(_, free, _)| fmt_bytes(*free))
                .collect();
            let mut s = format!("Room for models · {} free", models.join(" and "));
            if !rest.is_empty() {
                s.push_str(&format!(
                    ", and {} for the software that runs them",
                    rest.join(" and ")
                ));
            }
            s.push('.');
            s
        }
    };
    item(id, ItemState::Info, &sentence, detail, None)
}

/// What §1.6 found: its item, and the bytes the download still fetches when
/// they are known, for §1.5's low-space line.
pub(super) struct Download {
    pub item: MachineItem,
    pub remaining: Option<u64>,
}

/// §1.6's item: whether every engine image the settings name is here, and
/// what the download of the others fetches: the compressed blobs of each
/// one's registry manifest that this machine's image store does not have
/// (each blob once, however many images share it), as WP10's pull totals
/// it, and for a download that runs, its own bytes still to come. No size
/// rather than a guess: an image whose manifest could not be read says
/// why, and the sentence then gives none.
pub(super) fn download(images: &Images) -> Download {
    let id = MachineItemId::Download;
    if images.images.is_empty() {
        return Download {
            item: item(
                id,
                ItemState::NotChecked,
                "Not checked yet: lmgw can't run models on this computer yet.",
                Vec::new(),
                None,
            ),
            remaining: None,
        };
    }
    let here = images.layers_here.as_ref().ok();
    let mut detail = Vec::new();
    let mut missing = Vec::new();
    let mut downloading = false;
    // Every blob still to fetch, once: short ID → size.
    let mut blobs: BTreeMap<&str, u64> = BTreeMap::new();
    let mut in_flight: u64 = 0;
    let mut unknown = false;
    for i in &images.images {
        let r = &i.reference;
        match &i.here {
            Presence::Present => {
                detail.push(format!("{r}: on this computer"));
                continue;
            }
            Presence::Unknown(why) => {
                detail.push(format!("{r}: {why}"));
                continue;
            }
            Presence::Missing => missing.push(r.clone()),
        }
        if let Some(b) = &i.pulling {
            downloading = true;
            match b.total {
                Some(total) if total > 0 => {
                    in_flight += total.saturating_sub(b.done);
                    detail.push(format!(
                        "{r}: downloading now · {}",
                        lmgw_api_types::units::fmt_bytes_of(b.done, total)
                    ));
                }
                _ => {
                    unknown = true;
                    detail.push(format!(
                        "{r}: downloading now · {} so far, {}",
                        lmgw_api_types::units::fmt_bytes_so_far(b.done),
                        b.size_note.as_deref().unwrap_or("size not known yet")
                    ));
                }
            }
            continue;
        }
        match &i.blobs {
            Some(Ok(sizes)) => {
                let fetched: Vec<(&str, u64)> = sizes
                    .iter()
                    .filter(|(blob, _)| !here.is_some_and(|h| h.contains(*blob)))
                    .map(|(blob, n)| (blob.as_str(), *n))
                    .collect();
                let had = sizes.len() - fetched.len();
                let own: u64 = fetched.iter().map(|(_, n)| n).sum();
                blobs.extend(fetched);
                detail.push(format!(
                    "{r}: not downloaded yet · {} to download{}",
                    fmt_bytes(own),
                    if had > 0 {
                        format!(
                            " ({had} of its {} blobs are on this computer already)",
                            sizes.len()
                        )
                    } else {
                        String::new()
                    }
                ));
            }
            Some(Err(why)) => {
                unknown = true;
                detail.push(format!("{r}: not downloaded yet · size not known — {why}"));
            }
            None => {
                unknown = true;
                detail.push(format!("{r}: not downloaded yet · size not known"));
            }
        }
    }
    if missing.is_empty() {
        return Download {
            item: item(
                id,
                ItemState::Info,
                "The download for your graphics card is done.",
                detail,
                None,
            ),
            remaining: None,
        };
    }
    if let Err(why) = &images.layers_here {
        detail.push(format!(
            "which blobs are on this computer already could not be read ({why}), so the sizes \
             count every one"
        ));
    }
    let remaining = (!unknown).then(|| blobs.values().sum::<u64>() + in_flight);
    let sentence = match (downloading, remaining) {
        (true, Some(n)) => format!(
            "lmgw is downloading what your graphics card needs to run models · {} to go.",
            fmt_bytes(n)
        ),
        (true, None) => {
            "lmgw is downloading what your graphics card needs to run models.".to_string()
        }
        (false, Some(n)) => format!(
            "Before the first model runs, lmgw downloads what your graphics card needs to run \
             it ({}).",
            fmt_bytes(n)
        ),
        (false, None) => "Before the first model runs, lmgw downloads what your graphics card \
                          needs to run it."
            .to_string(),
    };
    Download {
        item: item(
            id,
            ItemState::Info,
            &sentence,
            detail,
            Some(MachineCause::ImagesMissing {
                references: missing,
            }),
        ),
        remaining,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::backends::pull::Bytes;
    use crate::machine::gather::Image;

    fn image(reference: &str, here: Presence) -> Image {
        Image {
            reference: reference.into(),
            here,
            pulling: None,
            blobs: None,
        }
    }

    /// A download that runs counts what it still has to fetch, by its own
    /// bytes, beside the manifest sizes of the others; one whose total is
    /// not known yet leaves the sentence without a size.
    #[test]
    fn a_download_in_progress_counts_its_own_bytes_still_to_come() {
        let mut pulling = image("ghcr.io/x/engine:cuda", Presence::Missing);
        pulling.pulling = Some(Bytes {
            done: 1_200_000_000,
            total: Some(3_400_000_000),
            size_note: None,
        });
        let mut other = image("ghcr.io/x/other:cuda", Presence::Missing);
        other.blobs = Some(Ok([("aaaaaaaaaaaa".to_string(), 500_000_000)].into()));
        let images = Images {
            images: vec![
                pulling,
                other,
                image("ghcr.io/x/here:cuda", Presence::Present),
            ],
            layers_here: Ok(BTreeSet::new()),
        };
        let d = download(&images);
        assert_eq!(d.remaining, Some(2_700_000_000));
        assert_eq!(
            d.item.sentence,
            "lmgw is downloading what your graphics card needs to run models · 2.5 GiB to go."
        );
        assert!(d
            .item
            .detail
            .contains(&"ghcr.io/x/engine:cuda: downloading now · 1.1 of 3.2 GiB".to_string()));

        let mut unknown = image("ghcr.io/x/engine:cuda", Presence::Missing);
        unknown.pulling = Some(Bytes {
            done: 10_000_000,
            total: None,
            size_note: Some(
                "size not known — the registry's manifest could not be read: offline".into(),
            ),
        });
        let d = download(&Images {
            images: vec![unknown],
            layers_here: Ok(BTreeSet::new()),
        });
        assert_eq!(d.remaining, None);
        assert_eq!(
            d.item.sentence,
            "lmgw is downloading what your graphics card needs to run models."
        );
        assert!(d.item.detail.contains(
            &"ghcr.io/x/engine:cuda: downloading now · 9.5 MiB so far, size not known — the \
              registry's manifest could not be read: offline"
                .to_string()
        ));
    }

    /// A store whose layer list could not be read counts every blob, and
    /// says so; an image podman could not tell about is named, not counted.
    #[test]
    fn an_unreadable_layer_list_counts_every_blob_and_says_so() {
        let mut missing = image("ghcr.io/x/engine:cuda", Presence::Missing);
        missing.blobs = Some(Ok([
            ("aaaaaaaaaaaa".to_string(), 1_000_000_000),
            ("bbbbbbbbbbbb".to_string(), 1_000_000_000),
        ]
        .into()));
        let d = download(&Images {
            images: vec![
                missing,
                image(
                    "ghcr.io/x/odd:cuda",
                    Presence::Unknown("podman image exists exited 125: boom".into()),
                ),
            ],
            layers_here: Err("layers.json: Permission denied".into()),
        });
        assert_eq!(d.remaining, Some(2_000_000_000));
        assert!(d
            .item
            .detail
            .iter()
            .any(|l| l.contains("could not be read (layers.json: Permission denied)")));
        assert!(d
            .item
            .detail
            .contains(&"ghcr.io/x/odd:cuda: podman image exists exited 125: boom".to_string()));
    }

    /// Every folder's room unreadable is not "no folder is set".
    #[test]
    fn room_that_could_not_be_read_is_not_an_unset_folder() {
        let rooms = vec![Room {
            what: "chat models",
            path: "/srv/models".into(),
            space: Err("Permission denied (os error 13)".into()),
        }];
        let r = room(&rooms, None);
        assert_eq!(
            r.sentence,
            "lmgw couldn't read how much room is left. The details say why."
        );
        assert!(r.detail[0].contains("Permission denied"));
        assert_eq!(
            room(&[], None).sentence,
            "Not checked yet: no folder for models is set."
        );
    }
}
