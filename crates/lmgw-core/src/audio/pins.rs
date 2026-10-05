//! Which Hugging Face revision an audio catalog download takes.
//!
//! An audio.cpp spec may pin a package's repo to a commit
//! (`download.revision`). The pin is the spec author's compatibility promise:
//! a third-party repo can later re-upload a file in a layout the engine cannot
//! load, and the error that produces names a tensor, not the cause. So under
//! `audio.catalog_revision = pinned` (the default) a pinned package downloads
//! every file at its pin, and the listing that says which files are
//! published is taken at that same revision; under `latest`, and for a
//! package pinned to nothing but `main`, everything is `main`.
//!
//! A pin that cannot be fetched is never quietly swapped for `main`: the
//! download fails with [`no_fallback`]'s sentence, which names the setting
//! that takes the latest instead.

use crate::config::CatalogRevision;
use crate::store::HfModelRow;

use super::{CatalogSnapshot, ModelSpec, SpecPackage};

/// The branch every unpinned download takes.
pub const MAIN: &str = "main";

/// A full git commit hash — what a spec's pin is, as opposed to `main` or
/// another branch or tag name, which move.
pub fn is_commit(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit the spec pins `pkg` to, if it pins one: the package's own
/// download, else the family default's.
pub fn spec_pin<'a>(spec: &'a ModelSpec, pkg: &'a SpecPackage) -> Option<&'a str> {
    spec.package_download(pkg)
        .and_then(|d| d.revision.as_deref())
        .map(str::trim)
        .filter(|r| is_commit(r))
}

/// The revision a download of `pkg` requests under `mode`.
pub fn download_revision<'a>(
    spec: &'a ModelSpec,
    pkg: &'a SpecPackage,
    mode: CatalogRevision,
) -> &'a str {
    match mode {
        CatalogRevision::Pinned => spec_pin(spec, pkg).unwrap_or(MAIN),
        CatalogRevision::Latest => MAIN,
    }
}

/// The first seven characters of a commit, as git and the hub show it.
pub fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Whether a failed fetch at a pin means the pin, or the file at it, is not
/// there: a 404 or 410, or the hub's own `RevisionNotFound` /
/// `EntryNotFound`. Only then does [`no_fallback`]'s advice apply — a rate
/// limit or a server error says nothing about the pin, and `main` would meet
/// it too.
pub fn pin_is_missing(status: u16, error_code: Option<&str>) -> bool {
    matches!(status, 404 | 410)
        || matches!(
            error_code.map(str::trim),
            Some("RevisionNotFound" | "EntryNotFound")
        )
}

/// What a failure at a pinned revision adds: no fallback happened, and how
/// to take the latest instead.
pub fn no_fallback(repo: &str, revision: &str) -> String {
    format!(
        "the audio.cpp spec pins {repo} to {} and lmgw does not fall back to main on its own — \
         set Settings → Runtimes → Audio → Catalog downloads to 'latest' \
         (audio.catalog_revision) to download the latest instead",
        short(revision)
    )
}

/// The revision the cached catalog downloads `file` of `repo` at under
/// `pinned`: the pin of the package that ships it (a split shard counts by
/// its whole set, which the download expands to), or `main` when that
/// package pins nothing. `None` when no package of the catalog ships it —
/// a download the catalog did not make keeps its own revision.
pub fn pinned_revision<'a>(
    snapshot: &'a CatalogSnapshot,
    repo: &str,
    file: &str,
) -> Option<&'a str> {
    snapshot.specs.iter().find_map(|spec| {
        spec.packages
            .iter()
            .find(|pkg| {
                spec.package_repo(pkg) == Some(repo)
                    && pkg.files.iter().any(|f| crate::hf::same_split_set(f, file))
            })
            .map(|pkg| spec_pin(spec, pkg).unwrap_or(MAIN))
    })
}

/// The revision a tracked row is update-checked against and re-downloaded
/// at. Under `latest`, `main`. Under `pinned`, the revision the spec takes
/// the file at **now** ([`pinned_revision`] over the cached catalog, for an
/// audio row): a pin the spec moved is followed, and a row taken at `main`
/// (before pins were followed, or under `latest`) joins the pin rather than
/// tracking `main` on its own. A file the catalog does not ship keeps its own
/// pin, and every other row `main` — so no check offers an update the spec
/// does not endorse.
pub fn tracked_revision<'a>(
    row: &'a HfModelRow,
    catalog: Option<&'a CatalogSnapshot>,
    mode: CatalogRevision,
) -> &'a str {
    if mode == CatalogRevision::Latest {
        return MAIN;
    }
    let spec = catalog
        .filter(|_| row.target == "audio")
        .and_then(|c| pinned_revision(c, &row.repo, &row.file));
    match (spec, row.requested_revision.as_deref()) {
        (Some(rev), _) => rev,
        (None, Some(r)) if is_commit(r) => r,
        _ => MAIN,
    }
}

/// A revision for a sentence: a commit shortened, a branch as it is.
fn shown(revision: &str) -> &str {
    match is_commit(revision) {
        true => short(revision),
        false => revision,
    }
}

/// Which commit a package's downloaded files came from, for its row in the
/// catalog: the commit and what the download asked for, grouped when the
/// files differ. Rows from before lmgw recorded it are "unknown", never a
/// guess. Empty when nothing of the package is downloaded.
///
/// `takes` is the revision a download of the package takes now under
/// `pinned` (`None` under `latest`): files from another revision say that
/// the spec has moved on, and what the update check and Update do about it
/// ([`tracked_revision`]).
pub fn downloaded_from(rows: &[&HfModelRow], takes: Option<&str>) -> String {
    let mut groups: std::collections::BTreeMap<(Option<String>, Option<String>), usize> =
        Default::default();
    for r in rows
        .iter()
        .filter(|r| matches!(r.status.as_str(), "done" | "update_available"))
    {
        let commit = r.resolved_commit.as_deref().map(str::to_ascii_lowercase);
        let asked = r.requested_revision.clone();
        *groups.entry((commit, asked)).or_default() += 1;
    }
    let one = groups.len() == 1;
    groups
        .iter()
        .map(|((commit, asked), n)| {
            let (commit, asked) = (commit.as_deref(), asked.as_deref());
            let what = match (commit, asked) {
                (Some(c), Some(a)) if a.eq_ignore_ascii_case(c) => {
                    match takes.is_some_and(|t| !t.eq_ignore_ascii_case(c)) {
                        true => format!("at commit {}, a pin the spec has moved from", short(c)),
                        false => format!("at commit {}, the pinned one", short(c)),
                    }
                }
                (Some(c), Some(a)) => format!("at commit {} (asked for {})", short(c), shown(a)),
                (Some(c), None) => format!("at commit {}", short(c)),
                (None, Some(a)) => format!(
                    "from {} at a commit the hub did not name — unknown",
                    shown(a)
                ),
                (None, None) => "before lmgw recorded commits — which one is unknown".to_string(),
            };
            let moved = match takes {
                // The spec pins a commit these files are not known to be.
                Some(t) if is_commit(t) && !commit.is_some_and(|c| c.eq_ignore_ascii_case(t)) => {
                    format!(
                        " — the spec pins {}: the update check compares against it, and an \
                         update takes it",
                        short(t)
                    )
                }
                // The spec pins nothing any more, and these were taken at a pin.
                Some(t) if !is_commit(t) && asked.is_some_and(is_commit) => format!(
                    " — the spec no longer pins one: the update check compares against {t}, and an \
                     update takes {t}"
                ),
                _ => String::new(),
            };
            match one {
                true => format!("downloaded {what}{moved}"),
                false => format!("{n} file(s) downloaded {what}{moved}"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PIN: &str = "607a30d783dfa663caf39e06633721c8d4cfcd7e";

    fn spec() -> ModelSpec {
        crate::audio::parse_spec(&json!({
            "family": "f",
            "package_defaults": { "download": { "repo": "o/r", "revision": PIN } },
            "packages": [
                // Inherits the family's pin (its own download is null).
                { "id": "inherits", "files": ["a.gguf"], "download": null },
                { "id": "main", "files": ["b.gguf"],
                  "download": { "repo": "o/r", "revision": "main" } },
                { "id": "tag", "files": ["c.gguf"],
                  "download": { "repo": "o/r", "revision": "v1.0" } },
                { "id": "none", "files": ["d.gguf"], "download": { "repo": "o/r" } },
            ],
        }))
    }

    #[test]
    fn only_a_commit_is_a_pin() {
        assert!(is_commit(PIN));
        assert!(is_commit(&PIN.to_uppercase()));
        assert!(!is_commit("main"));
        assert!(
            !is_commit("607a30d"),
            "a short hash is not what the spec writes"
        );
        assert!(!is_commit(&format!("{}g", &PIN[..39])));
    }

    #[test]
    fn only_a_missing_pin_is_told_to_take_the_latest() {
        assert!(pin_is_missing(404, None));
        assert!(pin_is_missing(410, None));
        assert!(pin_is_missing(400, Some("RevisionNotFound")));
        assert!(pin_is_missing(403, Some("EntryNotFound")));
        for transient in [429, 500, 502, 503] {
            assert!(!pin_is_missing(transient, None), "{transient}");
        }
        assert!(!pin_is_missing(429, Some("RateLimited")));
    }

    #[test]
    fn a_download_follows_the_pin_only_when_pinned() {
        let s = spec();
        let rev = |id: &str, mode| {
            let p = s.packages.iter().find(|p| p.id == id).unwrap();
            download_revision(&s, p, mode).to_string()
        };
        assert_eq!(rev("inherits", CatalogRevision::Pinned), PIN);
        assert_eq!(rev("inherits", CatalogRevision::Latest), "main");
        for unpinned in ["main", "tag", "none"] {
            assert_eq!(rev(unpinned, CatalogRevision::Pinned), "main", "{unpinned}");
        }
        assert_eq!(short(PIN), "607a30d");
    }

    fn row(file: &str, status: &str, asked: Option<&str>, commit: Option<&str>) -> HfModelRow {
        HfModelRow {
            id: 1,
            repo: "o/r".into(),
            file: file.into(),
            dest_path: format!("o/r/{file}"),
            target: "audio".into(),
            etag: None,
            size_bytes: None,
            status: status.into(),
            error: None,
            downloaded_at: None,
            requested_revision: asked.map(String::from),
            resolved_commit: commit.map(String::from),
        }
    }

    const NEW: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn a_package_says_which_commit_its_files_came_from() {
        let row = |status: &str, asked, commit| row("f", status, asked, commit);
        let main_at = "1111111111111111111111111111111111111111";
        let pinned = row("done", Some(PIN), Some(PIN));
        assert_eq!(
            downloaded_from(&[&pinned, &pinned], None),
            "downloaded at commit 607a30d, the pinned one"
        );
        assert_eq!(
            downloaded_from(&[&pinned], Some(PIN)),
            "downloaded at commit 607a30d, the pinned one",
            "at the pin the spec names now: nothing to add"
        );
        let latest = row("done", Some("main"), Some(main_at));
        assert_eq!(
            downloaded_from(&[&latest], None),
            "downloaded at commit 1111111 (asked for main)"
        );
        let old = row("done", None, None);
        assert_eq!(
            downloaded_from(&[&old], None),
            "downloaded before lmgw recorded commits — which one is unknown"
        );
        let mixed = downloaded_from(&[&latest, &old], None);
        assert!(
            mixed.contains("1 file(s) downloaded at commit 1111111")
                && mixed.contains("1 file(s) downloaded before lmgw recorded commits"),
            "{mixed}"
        );
        // Queued or failed rows are not downloaded files.
        assert_eq!(
            downloaded_from(&[&row("queued", Some("main"), None)], None),
            ""
        );

        // The spec moved its pin: the old one is not called "the pinned one",
        // and the row says what the check and Update do now.
        assert_eq!(
            downloaded_from(&[&pinned], Some(NEW)),
            "downloaded at commit 607a30d, a pin the spec has moved from — the spec pins \
             2222222: the update check compares against it, and an update takes it"
        );
        // Taken at main before pins were followed, or unknown: the same note.
        assert!(downloaded_from(&[&latest], Some(PIN)).ends_with(
            "(asked for main) — the spec pins 607a30d: the update check compares against it, \
             and an update takes it"
        ));
        assert!(downloaded_from(&[&old], Some(PIN)).contains("— the spec pins 607a30d"));
        // The spec dropped its pin: files taken at it say main is next; files
        // at main have nothing to add.
        assert_eq!(
            downloaded_from(&[&pinned], Some("main")),
            "downloaded at commit 607a30d, a pin the spec has moved from — the spec no longer \
             pins one: the update check compares against main, and an update takes main"
        );
        assert_eq!(
            downloaded_from(&[&latest], Some("main")),
            "downloaded at commit 1111111 (asked for main)"
        );
    }

    fn snapshot(pin: Option<&str>) -> CatalogSnapshot {
        let mut download = json!({ "repo": "o/r" });
        if let Some(p) = pin {
            download["revision"] = json!(p);
        }
        CatalogSnapshot {
            fetched_at: String::new(),
            specs: vec![crate::audio::parse_spec(&json!({
                "family": "f",
                "package_defaults": { "download": download },
                "packages": [
                    { "id": "split", "files": ["q4/m-00001-of-00002.gguf"] },
                    { "id": "one", "files": ["one.gguf"] },
                ],
            }))],
            listings: Default::default(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn the_catalog_names_the_revision_a_file_is_taken_at_now() {
        let pinned = snapshot(Some(PIN));
        assert_eq!(pinned_revision(&pinned, "o/r", "one.gguf"), Some(PIN));
        assert_eq!(
            pinned_revision(&pinned, "o/r", "q4/m-00002-of-00002.gguf"),
            Some(PIN),
            "a shard the download expanded to counts by its set"
        );
        assert_eq!(pinned_revision(&pinned, "o/r", "other.gguf"), None);
        assert_eq!(pinned_revision(&pinned, "x/y", "one.gguf"), None);
        let unpinned = snapshot(None);
        assert_eq!(pinned_revision(&unpinned, "o/r", "one.gguf"), Some("main"));
    }

    #[test]
    fn an_update_check_follows_the_pin_the_spec_names_now() {
        use CatalogRevision::{Latest, Pinned};
        let moved = snapshot(Some(NEW));
        let at_old = row("one.gguf", "done", Some(PIN), Some(PIN));
        let at_main = row("one.gguf", "done", None, None);
        // Under pinned: the spec's current pin, for a row at an old pin and
        // for one at main alike.
        assert_eq!(tracked_revision(&at_old, Some(&moved), Pinned), NEW);
        assert_eq!(tracked_revision(&at_main, Some(&moved), Pinned), NEW);
        // A spec that dropped its pin takes main.
        let dropped = snapshot(None);
        assert_eq!(tracked_revision(&at_old, Some(&dropped), Pinned), "main");
        // No catalog, or a file it does not ship: the row's own pin.
        assert_eq!(tracked_revision(&at_old, None, Pinned), PIN);
        let stray = row("stray.gguf", "done", Some(PIN), Some(PIN));
        assert_eq!(tracked_revision(&stray, Some(&moved), Pinned), PIN);
        assert_eq!(tracked_revision(&at_main, None, Pinned), "main");
        // Not an audio row: the catalog is not asked.
        let chat = HfModelRow {
            target: "chat".into(),
            ..at_main.clone()
        };
        assert_eq!(tracked_revision(&chat, Some(&moved), Pinned), "main");
        // Under latest: main, whatever the spec pins.
        assert_eq!(tracked_revision(&at_old, Some(&moved), Latest), "main");
        assert_eq!(tracked_revision(&at_old, None, Latest), "main");
    }
}
