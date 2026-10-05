//! The audio catalog browser's pure rules: the order its families keep while
//! it is open, which family starts open, a package's live download state, and
//! when a download that stopped moving re-reads the catalog.
//!
//! Kept out of the component so they are testable without a DOM. The list
//! used to be rebuilt by position on every catalog read, so a finished
//! download moved its family up, the scroll jumped, and a family's
//! open/closed state stayed at the old index while the families moved under
//! it. These rules are what replaced that.

use lmgw_api_types::{AudioCatalog, AudioFamily, AudioPackage, DownloadsView};

/// The order on screen after a catalog read: the families already shown keep
/// their places, new ones are appended (in the catalog's own order), and the
/// ones gone from the catalog are dropped. With nothing shown yet, the
/// catalog's order as it comes — that is the seed, where what is installed
/// sits first.
pub(super) fn merge_order(prev: &[String], incoming: &[String]) -> Vec<String> {
    let mut out: Vec<String> = prev
        .iter()
        .filter(|id| incoming.contains(id))
        .cloned()
        .collect();
    for id in incoming {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// What a catalog read changes on screen ([`after_read`]).
#[derive(Debug, PartialEq)]
pub(super) struct AfterRead {
    pub order: Vec<String>,
    /// The list goes back to the top: the order was re-seeded on open.
    pub to_top: bool,
}

/// The order after a catalog read, `None` when it stays as shown. The first
/// read after the browser opens (`reseeding`, the one the open asks for)
/// re-seeds it from the catalog's own order, so what was installed while it
/// was closed floats up on open — and the list starts from the top. Every
/// other read only merges ([`merge_order`]), so nothing moves under the
/// pointer.
pub(super) fn after_read(
    shown: &[String],
    incoming: &[String],
    reseeding: bool,
) -> Option<AfterRead> {
    let order = match reseeding {
        true => incoming.to_vec(),
        false => merge_order(shown, incoming),
    };
    (order != shown).then_some(AfterRead {
        order,
        to_top: reseeding,
    })
}

/// Whether a family's row starts open: when something of it is here (a
/// downloaded package, a serving row), and — with nothing here at all — the
/// first one shown, so the browser never reads as a wall of closed rows.
/// Decided once, when the row is built: a family installed while the browser
/// is open is not popped open under the pointer.
pub(super) fn default_open(families: &[AudioFamily], order: &[String], id: &str) -> bool {
    let here = |f: &AudioFamily| f.any_installed || f.served;
    if families.iter().any(|f| f.family == id && here(f)) {
        return true;
    }
    !families.iter().any(here) && order.first().is_some_and(|first| first == id)
}

/// Whether the hub lacks a file a download of this package would fetch, so
/// the action slot says "not published yet" instead of offering it. A
/// download fetches only what the package lacks once any of it is here
/// (`missing_files`), everything when nothing is: a file the hub has since
/// dropped that is already on disk does not stand in the way of completing
/// the rest.
pub(super) fn download_blocked(p: &AudioPackage) -> bool {
    if p.unpublished_files.is_empty() {
        return false;
    }
    if p.download_ids.is_empty() {
        return true;
    }
    p.unpublished_files
        .iter()
        .any(|f| p.missing_files.contains(f))
}

/// Live state of one package's files, joined from `/api/hf/downloads`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Live {
    /// Nothing of this package is moving.
    Idle,
    /// Queued or transferring; `Some(pct)` once a total is known.
    Running(Option<u64>),
    Failed,
    /// Its files stopped moving while the browser was open, and the catalog
    /// on screen was read before they did: it still says "not installed"
    /// until the re-read the landing asked for is in ([`Landed`]).
    Finishing,
}

/// A download row of a package that stopped moving while the browser was
/// open — done, failed, or gone from the list — and the catalog read that
/// counts it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Landed {
    pub package: String,
    pub id: i64,
    /// The catalog read asked for when it landed (the browser's reload
    /// count): a read answering this ask, or a later one, was made after the
    /// row stopped and has the package as it is now.
    pub asked: u32,
}

/// `watching` is `(package id, download row id)` — a package queued a moment
/// ago has rows the catalog snapshot does not know about yet. `landed` are
/// the rows still waiting for the catalog read that counts them.
pub(super) fn live_of(
    pkg: &AudioPackage,
    dv: Option<&DownloadsView>,
    watching: &[(String, i64)],
    landed: &[Landed],
) -> Live {
    // What nothing of it moving reads as: finishing while a catalog read
    // asked after its files landed is on its way. A read that already
    // counts it installed leaves nothing to wait for.
    let at_rest = match !pkg.installed && landed.iter().any(|l| l.package == pkg.id) {
        true => Live::Finishing,
        false => Live::Idle,
    };
    let mine: Vec<i64> = watching
        .iter()
        .filter(|(p, _)| *p == pkg.id)
        .map(|(_, id)| *id)
        .collect();
    let Some(dv) = dv else {
        return match mine.is_empty() {
            true => at_rest,
            false => Live::Running(None),
        };
    };
    let rows: Vec<_> = dv
        .downloads
        .iter()
        .filter(|d| pkg.download_ids.contains(&d.id) || mine.contains(&d.id))
        .collect();
    if rows.is_empty() && !mine.is_empty() {
        return Live::Running(None);
    }
    if rows.iter().any(|d| d.status == "failed") {
        return Live::Failed;
    }
    let running: Vec<_> = rows
        .iter()
        .filter(|d| matches!(d.status.as_str(), "queued" | "downloading"))
        .collect();
    if running.is_empty() {
        return at_rest;
    }
    let pcts: Vec<u64> = running.iter().filter_map(|d| d.percent).collect();
    match pcts.len() == running.len() && !pcts.is_empty() {
        true => Live::Running(Some(pcts.iter().sum::<u64>() / pcts.len() as u64)),
        false => Live::Running(None),
    }
}

/// What the action slot of a package with a download source holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Slot {
    /// Its download's progress; `None` while queued.
    Progress(Option<u64>),
    /// Downloaded, waiting for the catalog to count it: no button. Offering
    /// "Download" here offered a package whose files are on disk.
    Finishing,
    /// "Create model".
    Create,
    /// "Download", or "Complete install" / "Resume download".
    Download,
}

/// The slot from the package's live state and the catalog's word on it: a
/// finished download waits for its read rather than reading as not here.
pub(super) fn slot(live: Live, installed: bool) -> Slot {
    match (live, installed) {
        (Live::Running(pct), _) => Slot::Progress(pct),
        (Live::Finishing, _) => Slot::Finishing,
        (_, true) => Slot::Create,
        (_, false) => Slot::Download,
    }
}

/// The rows of this catalog still moving, as `(package id, row id)`: queued
/// or transferring in the downloads snapshot, or queued from here
/// (`watching`) and not in that snapshot yet. The poll runs while this is
/// not empty, and a row that leaves it has landed ([`landings`]).
pub(super) fn moving(
    catalog: Option<&AudioCatalog>,
    dv: Option<&DownloadsView>,
    watching: &[(String, i64)],
) -> Vec<(String, i64)> {
    let status = |id: i64| {
        dv.and_then(|v| v.downloads.iter().find(|r| r.id == id))
            .map(|r| r.status.as_str())
    };
    let runs = |s: &str| matches!(s, "queued" | "downloading");
    let known = catalog
        .into_iter()
        .flat_map(|c| c.families.iter())
        .flat_map(|f| f.packages.iter())
        .flat_map(|p| p.download_ids.iter().map(|id| (p.id.clone(), *id)))
        .filter(|(_, id)| status(*id).is_some_and(runs));
    let queued = watching
        .iter()
        .filter(|(_, id)| status(*id).is_none_or(runs))
        .cloned();
    let mut out: Vec<(String, i64)> = Vec::new();
    for row in known.chain(queued) {
        if !out.contains(&row) {
            out.push(row);
        }
    }
    out
}

/// The rows that landed since the last look: moving then (`before`), or
/// queued from here, and not moving `now`. A watched row counts even if no
/// look ever saw it move — a small file can be done by the first read after
/// it was queued. One already waiting for its catalog read is not news.
pub(super) fn landings(
    before: &[(String, i64)],
    watching: &[(String, i64)],
    now: &[(String, i64)],
    landed: &[Landed],
) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    for row in before.iter().chain(watching) {
        let waiting = landed.iter().any(|l| l.package == row.0 && l.id == row.1);
        if !now.contains(row) && !waiting && !out.contains(row) {
            out.push(row.clone());
        }
    }
    out
}

/// What is left once a catalog read answering ask `answered` is in: the
/// landed rows it was asked after are settled — it has their packages as
/// they are now — and so is the watch on them, which only stood in for the
/// read that did not know the rows yet. Returns `(landed, watching)`.
pub(super) fn settle(
    landed: &[Landed],
    watching: &[(String, i64)],
    answered: u32,
) -> (Vec<Landed>, Vec<(String, i64)>) {
    let (done, waiting): (Vec<Landed>, Vec<Landed>) =
        landed.iter().cloned().partition(|l| l.asked <= answered);
    let watching = watching
        .iter()
        .filter(|(p, id)| !done.iter().any(|l| l.package == *p && l.id == *id))
        .cloned()
        .collect();
    (waiting, watching)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::DownloadRow;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merge_order_keeps_the_shown_order_appends_and_drops() {
        // A download landed: the catalog now sorts `c` first. The screen
        // does not follow while the browser is open.
        assert_eq!(
            merge_order(&ids(&["a", "b", "c"]), &ids(&["c", "a", "b"])),
            ids(&["a", "b", "c"])
        );
        // A refresh brought `d` and took `b` away.
        assert_eq!(
            merge_order(&ids(&["a", "b", "c"]), &ids(&["d", "c", "a"])),
            ids(&["a", "c", "d"])
        );
        // Nothing shown yet: the catalog's own order is the seed.
        assert_eq!(merge_order(&[], &ids(&["c", "a"])), ids(&["c", "a"]));
        assert!(merge_order(&ids(&["a"]), &[]).is_empty());
    }

    #[test]
    fn a_reopen_reseeds_from_its_own_read_and_later_reads_only_merge() {
        let shown = ids(&["a", "b", "c"]);
        // `c` was installed while the browser was closed: the read the open
        // asked for sorts it first, and the list starts from the top.
        assert_eq!(
            after_read(&shown, &ids(&["c", "a", "b"]), true),
            Some(AfterRead {
                order: ids(&["c", "a", "b"]),
                to_top: true,
            })
        );
        // The same order on open: nothing to redraw, nowhere to scroll.
        assert_eq!(after_read(&shown, &shown, true), None);
        // A poll while open: the catalog's new sort is not followed.
        assert_eq!(after_read(&shown, &ids(&["c", "a", "b"]), false), None);
        // A family that arrived while open is appended, without a scroll.
        assert_eq!(
            after_read(&shown, &ids(&["d", "a", "b", "c"]), false),
            Some(AfterRead {
                order: ids(&["a", "b", "c", "d"]),
                to_top: false,
            })
        );
    }

    fn fam(id: &str, installed: bool, served: bool) -> AudioFamily {
        AudioFamily {
            family: id.into(),
            any_installed: installed,
            served,
            ..Default::default()
        }
    }

    #[test]
    fn default_open_follows_what_is_here() {
        let order = ids(&["a", "b", "c"]);
        let some = [
            fam("a", false, false),
            fam("b", true, false),
            fam("c", false, true),
        ];
        assert!(!default_open(&some, &order, "a"));
        assert!(default_open(&some, &order, "b"), "installed");
        assert!(default_open(&some, &order, "c"), "served");
        // Nothing here anywhere: only the first one shown opens.
        let none = [fam("a", false, false), fam("b", false, false)];
        assert!(default_open(&none, &order, "a"));
        assert!(!default_open(&none, &order, "b"));
        assert!(
            !default_open(&none, &[], "a"),
            "no order yet, nothing first"
        );
    }

    #[test]
    fn only_a_file_still_to_fetch_blocks_the_download() {
        let files = |v: &[&str]| v.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        // Never downloaded, the hub lacks a file: every file would be fetched.
        let fresh = AudioPackage {
            unpublished_files: files(&["a.gguf"]),
            ..Default::default()
        };
        assert!(download_blocked(&fresh));
        // Downloaded once; the hub dropped a file that is here, and the spec
        // grew one it does publish: "Complete install" fetches only that.
        let grown = AudioPackage {
            unpublished_files: files(&["a.gguf"]),
            missing_files: files(&["voice.bin"]),
            download_ids: vec![1],
            ..Default::default()
        };
        assert!(!download_blocked(&grown));
        // The file it lacks is the one the hub does not publish.
        let short = AudioPackage {
            missing_files: files(&["a.gguf"]),
            ..grown.clone()
        };
        assert!(download_blocked(&short));
        assert!(!download_blocked(&AudioPackage::default()));
    }

    fn row(id: i64, status: &str, percent: Option<u64>) -> DownloadRow {
        DownloadRow {
            id,
            status: status.into(),
            percent,
            ..Default::default()
        }
    }

    fn pkg(download_ids: &[i64]) -> AudioPackage {
        AudioPackage {
            id: "p".into(),
            download_ids: download_ids.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn live_of_reads_the_package_rows() {
        let dv = |rows: Vec<DownloadRow>| DownloadsView {
            downloads: rows,
            active: 0,
        };
        // Downloaded, or never touched: nothing moves.
        let done = dv(vec![row(1, "done", None), row(2, "done", None)]);
        assert_eq!(live_of(&pkg(&[1, 2]), Some(&done), &[], &[]), Live::Idle);
        assert_eq!(live_of(&pkg(&[]), Some(&done), &[], &[]), Live::Idle);
        // Two files on their way: the mean once both report a percentage.
        let running = dv(vec![
            row(1, "downloading", Some(40)),
            row(2, "queued", Some(0)),
        ]);
        assert_eq!(
            live_of(&pkg(&[1, 2]), Some(&running), &[], &[]),
            Live::Running(Some(20))
        );
        let unknown = dv(vec![
            row(1, "downloading", Some(40)),
            row(2, "queued", None),
        ]);
        assert_eq!(
            live_of(&pkg(&[1, 2]), Some(&unknown), &[], &[]),
            Live::Running(None)
        );
        // One failed file fails the package.
        let failed = dv(vec![row(1, "failed", None), row(2, "downloading", Some(5))]);
        assert_eq!(
            live_of(&pkg(&[1, 2]), Some(&failed), &[], &[]),
            Live::Failed
        );
        // Queued from here a moment ago: running before any poll knows it,
        // and before the downloads list has been read at all.
        let watching = [("p".to_string(), 9)];
        assert_eq!(
            live_of(&pkg(&[]), Some(&done), &watching, &[]),
            Live::Running(None)
        );
        assert_eq!(
            live_of(&pkg(&[]), None, &watching, &[]),
            Live::Running(None)
        );
        assert_eq!(live_of(&pkg(&[]), None, &[], &[]), Live::Idle);
        // Another package's watch is not this one's.
        let other = [("q".to_string(), 9)];
        assert_eq!(live_of(&pkg(&[]), Some(&done), &other, &[]), Live::Idle);
    }

    fn watch(v: &[(&str, i64)]) -> Vec<(String, i64)> {
        v.iter().map(|(p, id)| (p.to_string(), *id)).collect()
    }

    fn named(id: &str, download_ids: &[i64], installed: bool) -> AudioPackage {
        AudioPackage {
            id: id.into(),
            download_ids: download_ids.to_vec(),
            installed,
            ..Default::default()
        }
    }

    fn catalog(packages: Vec<AudioPackage>) -> AudioCatalog {
        AudioCatalog {
            families: vec![AudioFamily {
                family: "f".into(),
                packages,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The owner's report: two packages downloading at once, and the one
    /// that finished first showed "Download" again until the other one was
    /// done too — the catalog on screen was read before its files landed.
    #[test]
    fn a_package_whose_files_landed_never_offers_download_again() {
        // Read before either was queued: neither knows its rows yet.
        let before_queue = catalog(vec![named("p", &[], false), named("q", &[], false)]);
        let watching = watch(&[("p", 1), ("p", 2), ("q", 3), ("q", 4)]);
        let dv = DownloadsView {
            downloads: vec![
                row(1, "done", None),
                row(2, "done", None),
                row(3, "downloading", Some(30)),
                row(4, "queued", None),
            ],
            active: 0,
        };
        let p = &before_queue.families[0].packages[0];
        let q = &before_queue.families[0].packages[1];

        // Without the landing, `p`'s rows are all done and the catalog says
        // "not installed": that fell through to the Download button.
        assert_eq!(
            slot(live_of(p, Some(&dv), &watching, &[]), p.installed),
            Slot::Download
        );

        // `p` stopped moving, `q` did not: only `p` landed, and it asks for a
        // catalog read (ask 2; the open asked 1).
        let now = moving(Some(&before_queue), Some(&dv), &watching);
        assert_eq!(now, watch(&[("q", 3), ("q", 4)]));
        let fresh = landings(&watching, &watching, &now, &[]);
        assert_eq!(fresh, watch(&[("p", 1), ("p", 2)]));
        let landed: Vec<Landed> = fresh
            .into_iter()
            .map(|(package, id)| Landed {
                package,
                id,
                asked: 2,
            })
            .collect();

        // Until that read is in, `p` is finishing — no button — and `q` is
        // still on its way.
        assert_eq!(live_of(p, Some(&dv), &watching, &landed), Live::Finishing);
        assert_eq!(slot(Live::Finishing, false), Slot::Finishing);
        assert_eq!(
            slot(live_of(q, Some(&dv), &watching, &landed), q.installed),
            Slot::Progress(None)
        );
        // A read that was asked before the landing settles nothing.
        assert_eq!(
            settle(&landed, &watching, 1),
            (landed.clone(), watching.clone())
        );

        // The read asked for it: `p` counted installed, `q` known and in
        // flight. `p` creates now, while `q` still downloads.
        let fresh_read = catalog(vec![named("p", &[1, 2], true), named("q", &[3, 4], false)]);
        let (landed, watching) = settle(&landed, &watching, 2);
        assert!(landed.is_empty());
        assert_eq!(watching, watch(&[("q", 3), ("q", 4)]));
        let p = &fresh_read.families[0].packages[0];
        let q = &fresh_read.families[0].packages[1];
        assert_eq!(
            slot(live_of(p, Some(&dv), &watching, &landed), p.installed),
            Slot::Create
        );
        assert_eq!(
            slot(live_of(q, Some(&dv), &watching, &landed), q.installed),
            Slot::Progress(None)
        );
        // A read that already counts it installed wins over a landing still
        // waiting for its own read.
        let waiting = [Landed {
            package: "p".into(),
            id: 1,
            asked: 3,
        }];
        assert_eq!(
            slot(live_of(p, Some(&dv), &watching, &waiting), p.installed),
            Slot::Create
        );
    }

    #[test]
    fn a_package_short_of_files_it_did_not_just_download_offers_to_complete() {
        // Downloaded long ago, a file gone from disk since: its rows are all
        // done and nothing landed while the browser watched. That is
        // "Complete install", not a package finishing forever.
        let dv = DownloadsView {
            downloads: vec![row(1, "done", None), row(2, "done", None)],
            active: 0,
        };
        let short = named("p", &[1, 2], false);
        assert_eq!(live_of(&short, Some(&dv), &[], &[]), Live::Idle);
        assert_eq!(slot(Live::Idle, false), Slot::Download);
        // A failed file of a landed package is a failure, not finishing.
        let failed = DownloadsView {
            downloads: vec![row(1, "done", None), row(2, "failed", None)],
            active: 0,
        };
        let landed = [Landed {
            package: "p".into(),
            id: 2,
            asked: 1,
        }];
        assert_eq!(live_of(&short, Some(&failed), &[], &landed), Live::Failed);
    }

    #[test]
    fn moving_and_landings_follow_the_rows() {
        let cat = catalog(vec![named("p", &[1, 2], false), named("q", &[3], true)]);
        let dv = DownloadsView {
            downloads: vec![
                row(1, "downloading", Some(10)),
                row(2, "done", None),
                row(3, "done", None),
                row(5, "queued", None),
            ],
            active: 0,
        };
        // Known to the catalog and transferring; queued from here (5), and
        // queued from here a moment ago and not in the list yet (6). A row
        // both known and watched is one row.
        let watching = watch(&[("p", 1), ("r", 5), ("r", 6)]);
        assert_eq!(
            moving(Some(&cat), Some(&dv), &watching),
            watch(&[("p", 1), ("r", 5), ("r", 6)])
        );
        // No list read yet: only what was queued from here is known to move.
        assert_eq!(moving(Some(&cat), None, &watching), watching);
        assert!(moving(None, None, &[]).is_empty());

        // Done, failed, or gone from the list: each landed. A row watched
        // from here lands even if no look saw it move — it was done by the
        // first read after it was queued.
        let before = watch(&[("p", 1), ("q", 3)]);
        let now = watch(&[("p", 1)]);
        assert_eq!(
            landings(&before, &watch(&[("r", 7)]), &now, &[]),
            watch(&[("q", 3), ("r", 7)])
        );
        // One already waiting for its catalog read is not news again.
        let waiting = [Landed {
            package: "q".into(),
            id: 3,
            asked: 4,
        }];
        assert!(landings(&before, &[], &now, &waiting).is_empty());
        // Nothing stopped: nothing landed.
        assert!(landings(&now, &now, &now, &[]).is_empty());
    }
}
