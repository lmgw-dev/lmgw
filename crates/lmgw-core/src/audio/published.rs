//! Which of the catalog's package files Hugging Face actually publishes.
//!
//! A spec can be ahead of the weights: `parakeet_tdt.json` lists Orukeet
//! packages whose `Orukeet-GGUF/…` files are not in `audio-cpp/audio.cpp-gguf`
//! yet, and the catalog offered Download for them until the queue refused it
//! with a toast. So a catalog refresh lists each package repo once and keeps,
//! with the snapshot, which of the spec-referenced files were there. Nothing
//! here runs on a plain read of the catalog — only the explicit refresh goes
//! to the network.
//!
//! A repo that cannot be listed (offline, rate-limited, gated) keeps the
//! listing it had, stamped with its date, and becomes a catalog warning until
//! the next refresh that lists it.
//!
//! A package a spec pins to a commit is listed at that commit too
//! ([`super::pins`]): under `audio.catalog_revision = pinned` that is where
//! its download comes from, and "not published" has to be true of it there.
//! Every package is also listed at `main`, which is what `latest` takes — so
//! the catalog stays truthful in both modes without a refresh in between.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{pins, ModelSpec};
use crate::hf::ListFailure;

/// Repos listed at once. The anonymous hub budget is a few hundred calls per
/// five minutes and a refresh makes one per repo (~60 today), so a small
/// fan-out is plenty and leaves the budget to downloads.
const CONCURRENT_LISTINGS: usize = 4;

/// What one package repo published, as of `listed_at`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoListing {
    /// RFC3339 time of the listing `present` comes from; empty when the repo
    /// has never been listed.
    pub listed_at: String,
    /// The spec-referenced files the repo had then (only those are kept);
    /// `None` when it has never been listed.
    pub present: Option<BTreeSet<String>>,
    /// The files `present` was checked for: what the specs referenced at
    /// `listed_at`. A failed listing keeps the previous one, and a file a
    /// newer spec added was never looked for in it — unknown, not missing.
    /// Empty in a listing saved before this existed, which therefore knows
    /// nothing either way.
    pub checked: BTreeSet<String>,
    /// Why the latest attempt to list it failed; empty when it did not.
    pub error: String,
}

/// Every `(repo, revision)` a package downloads from — `main` for each, and
/// the spec's pin where it pins one — with the files the specs reference
/// there.
pub fn referenced(specs: &[ModelSpec]) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for spec in specs {
        for pkg in &spec.packages {
            let Some(repo) = spec.package_repo(pkg) else {
                continue;
            };
            let pin = pins::spec_pin(spec, pkg);
            for revision in std::iter::once(pins::MAIN).chain(pin) {
                out.entry((repo.to_string(), revision.to_string()))
                    .or_default()
                    .extend(pkg.files.iter().cloned());
            }
        }
    }
    out
}

/// Where a snapshot keeps the listing of `repo` at `revision`: the repo for
/// `main` (as listings were kept before pins), `repo@commit` for a pin.
pub fn listing_key(repo: &str, revision: &str) -> String {
    match revision == pins::MAIN {
        true => repo.to_string(),
        false => format!("{repo}@{revision}"),
    }
}

/// `repo`, or `repo at abc1234` for a pin — how a sentence names a listing.
pub fn listing_label(repo: &str, revision: &str) -> String {
    match revision == pins::MAIN {
        true => repo.to_string(),
        false => format!("{repo} at {}", pins::short(revision)),
    }
}

/// One repo's listing from what the hub answered (`listed`: every file path
/// in the repo, or why it could not be listed), and the warning a failure
/// becomes. A failure keeps `previous`'s files and date.
pub fn settle(
    repo: &str,
    wanted: &BTreeSet<String>,
    listed: Result<Vec<String>, String>,
    previous: Option<&RepoListing>,
    now: &str,
) -> (RepoListing, Option<String>) {
    match listed {
        Ok(paths) => {
            let present = paths.into_iter().filter(|p| wanted.contains(p)).collect();
            let listing = RepoListing {
                listed_at: now.to_string(),
                present: Some(present),
                checked: wanted.clone(),
                error: String::new(),
            };
            (listing, None)
        }
        Err(error) => {
            let kept = previous.filter(|p| p.present.is_some());
            let warning = match kept {
                Some(p) => format!(
                    "could not list {repo} on Hugging Face ({error}) — which of its files are \
                     published is as of {}",
                    day(&p.listed_at)
                ),
                None => format!(
                    "could not list {repo} on Hugging Face ({error}) — whether its packages' \
                     files are published is unknown"
                ),
            };
            let listing = RepoListing {
                listed_at: kept.map(|p| p.listed_at.clone()).unwrap_or_default(),
                present: kept.and_then(|p| p.present.clone()),
                checked: kept.map(|p| p.checked.clone()).unwrap_or_default(),
                error,
            };
            (listing, Some(warning))
        }
    }
}

/// List every package repo of `specs` once, `CONCURRENT_LISTINGS` at a time.
/// Returns the listings by repo and a warning per repo that failed.
pub async fn list_repos(
    http: &reqwest::Client,
    token: &str,
    specs: &[ModelSpec],
    previous: &BTreeMap<String, RepoListing>,
) -> (BTreeMap<String, RepoListing>, Vec<String>) {
    let base = crate::hf::hf_base();
    list_repos_at(http, token, &base, specs, previous, CONCURRENT_LISTINGS).await
}

/// [`list_repos`] against the hub at `base`, `concurrent` at a time.
///
/// Once the hub says its rate limit is used up, the repos not asked yet are
/// not asked at all (until the window it named has passed): every further
/// call would be one more 429 against the same exhausted window. They keep
/// their previous listing like any other failure, and say why.
async fn list_repos_at(
    http: &reqwest::Client,
    token: &str,
    base: &str,
    specs: &[ModelSpec],
    previous: &BTreeMap<String, RepoListing>,
    concurrent: usize,
) -> (BTreeMap<String, RepoListing>, Vec<String>) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let limited: Arc<Mutex<Option<Limited>>> = Arc::default();
    let settled: Vec<(String, RepoListing, Option<String>)> = futures::stream::iter(
        referenced(specs)
            .into_iter()
            .map(|((repo, revision), wanted)| {
                let previous = previous.get(&listing_key(&repo, &revision)).cloned();
                list_one(
                    http,
                    token,
                    base,
                    (repo, revision),
                    wanted,
                    previous,
                    &now,
                    limited.clone(),
                )
            }),
    )
    .buffer_unordered(concurrent.max(1))
    .collect()
    .await;
    let mut listings = BTreeMap::new();
    let mut warnings = Vec::new();
    for (key, listing, warning) in settled {
        warnings.extend(warning);
        listings.insert(key, listing);
    }
    warnings.sort();
    (listings, warnings)
}

/// One `(repo, revision)` of [`list_repos_at`]: its listing under its
/// [`listing_key`], and the warning a failure becomes. Not asked at all while
/// `limited` says the window is used up.
#[allow(clippy::too_many_arguments)]
async fn list_one(
    http: &reqwest::Client,
    token: &str,
    base: &str,
    (repo, revision): (String, String),
    wanted: BTreeSet<String>,
    previous: Option<RepoListing>,
    now: &str,
    limited: Arc<Mutex<Option<Limited>>>,
) -> (String, RepoListing, Option<String>) {
    let skip = limited.lock().unwrap().as_ref().and_then(Limited::skip);
    let listed = match skip {
        Some(why) => Err(why),
        None => match crate::hf::list_tree(http, token, base, &repo, &revision).await {
            Ok(files) => Ok(files.into_iter().map(|f| f.path).collect()),
            Err(e) => {
                if let ListFailure::RateLimited { reset_s, .. } = &e {
                    limited.lock().unwrap().get_or_insert(Limited {
                        until: reset_s.map(|s| Instant::now() + Duration::from_secs(s)),
                    });
                }
                Err(String::from(e))
            }
        },
    };
    let label = listing_label(&repo, &revision);
    let (listing, warning) = settle(&label, &wanted, listed, previous.as_ref(), now);
    (listing_key(&repo, &revision), listing, warning)
}

/// The hub's rate limit, as the first 429 of a refresh reported it.
struct Limited {
    /// When the window resets; `None` when the hub did not say, which holds
    /// for the rest of the refresh.
    until: Option<Instant>,
}

impl Limited {
    /// Why a repo is not asked now, or `None` once the window has passed.
    fn skip(&self) -> Option<String> {
        let left = match self.until {
            Some(t) => {
                let left = t.checked_duration_since(Instant::now())?;
                format!(" — it resets in {} s", left.as_secs().max(1))
            }
            None => String::new(),
        };
        Some(format!(
            "not asked: Hugging Face's rate limit was reached earlier in this refresh{left}"
        ))
    }
}

/// A package's files against its repo's listing: the ones the hub does not
/// publish, and the sentence the catalog shows for it — the missing files
/// with the listing's date, the files the listing never looked for, or why
/// the repo could not be listed. Both empty when every file is there, or when
/// the repo was never checked (the catalog says that once, not per package).
pub fn availability(
    repo: &str,
    files: &[String],
    listing: Option<&RepoListing>,
) -> (Vec<String>, String) {
    let Some(listing) = listing else {
        return (Vec::new(), String::new());
    };
    let Some(present) = &listing.present else {
        let note = match listing.error.is_empty() {
            true => String::new(),
            false => format!(
                "could not check whether {repo} publishes these files: {}",
                listing.error
            ),
        };
        return (Vec::new(), note);
    };
    // Only a file the listing looked for can be missing from it: one a
    // newer spec added is unknown until a listing that wanted it.
    let (looked, unknown): (Vec<&String>, Vec<&String>) =
        files.iter().partition(|f| listing.checked.contains(*f));
    let missing: Vec<String> = looked
        .into_iter()
        .filter(|f| !present.contains(*f))
        .cloned()
        .collect();
    let mut notes = Vec::new();
    if !missing.is_empty() {
        notes.push(format!(
            "{repo} has no {} (listed {}) — the spec is ahead of the published weights; a \
             catalog refresh re-checks",
            missing.join(", "),
            day(&listing.listed_at)
        ));
    }
    if !unknown.is_empty() {
        let unknown: Vec<&str> = unknown.iter().map(|f| f.as_str()).collect();
        notes.push(format!(
            "whether {repo} publishes {} is unknown — the listing of {} did not look for it; \
             a catalog refresh checks it",
            unknown.join(", "),
            day(&listing.listed_at)
        ));
    }
    (missing, notes.join("; "))
}

/// The date of an RFC3339 time, which is all a listing's age needs.
fn day(rfc3339: &str) -> &str {
    rfc3339.get(..10).unwrap_or(rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REPO: &str = "audio-cpp/audio.cpp-gguf";

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn files(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn referenced_groups_package_files_by_repo() {
        let spec = crate::audio::parse_spec(&json!({
            "family": "parakeet_tdt",
            "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": REPO } },
            "packages": [
                { "id": "q8", "files": ["Parakeet/q8.gguf"] },
                { "id": "orukeet", "files": ["Orukeet-GGUF/orukeet-q8_0.gguf"] },
                { "id": "other", "files": ["x.gguf"],
                  "download": { "kind": "huggingface_snapshot", "repo": "a/b" } },
                { "id": "none", "files": ["y.gguf"],
                  "download": { "kind": "unsupported", "repo": "", "reason": "licence" } },
            ],
        }));
        let r = referenced(&[spec]);
        let at = |repo: &str, rev: &str| r[&(repo.to_string(), rev.to_string())].clone();
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(
            at(REPO, "main"),
            set(&["Orukeet-GGUF/orukeet-q8_0.gguf", "Parakeet/q8.gguf"])
        );
        assert_eq!(at("a/b", "main"), set(&["x.gguf"]));
    }

    /// A pinned package is listed at its pin (what `pinned` downloads) and at
    /// `main` (what `latest` downloads); a pin that is not a commit is no pin.
    #[test]
    fn a_pinned_package_is_listed_at_its_pin_and_at_main() {
        let pin = "607a30d783dfa663caf39e06633721c8d4cfcd7e";
        let spec = crate::audio::parse_spec(&json!({
            "family": "fun_asr",
            "packages": [
                { "id": "pinned", "files": ["model.gguf"],
                  "download": { "repo": "x/fun", "revision": pin } },
                { "id": "tagged", "files": ["other.gguf"],
                  "download": { "repo": "x/fun", "revision": "v1" } },
            ],
        }));
        let r = referenced(&[spec]);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(
            r[&("x/fun".to_string(), "main".to_string())],
            set(&["model.gguf", "other.gguf"])
        );
        assert_eq!(
            r[&("x/fun".to_string(), pin.to_string())],
            set(&["model.gguf"])
        );
        assert_eq!(listing_key("x/fun", "main"), "x/fun");
        assert_eq!(listing_key("x/fun", pin), format!("x/fun@{pin}"));
        assert_eq!(listing_label("x/fun", pin), "x/fun at 607a30d");
    }

    #[test]
    fn a_listing_keeps_only_the_referenced_files() {
        let wanted = set(&["Parakeet/q8.gguf", "Orukeet-GGUF/orukeet-q8_0.gguf"]);
        let listed = Ok(files(&["Parakeet/q8.gguf", "README.md", "Other/z.gguf"]));
        let (l, warning) = settle(REPO, &wanted, listed, None, "2026-10-02T12:00:00Z");
        assert_eq!(warning, None);
        assert_eq!(l.present, Some(set(&["Parakeet/q8.gguf"])));
        assert_eq!(l.listed_at, "2026-10-02T12:00:00Z");

        // The Orukeet case: in the spec, not in the repo.
        let (missing, note) =
            availability(REPO, &files(&["Orukeet-GGUF/orukeet-q8_0.gguf"]), Some(&l));
        assert_eq!(missing, ["Orukeet-GGUF/orukeet-q8_0.gguf"]);
        assert!(
            note.starts_with(
                "audio-cpp/audio.cpp-gguf has no Orukeet-GGUF/orukeet-q8_0.gguf (listed 2026-10-02)"
            ),
            "{note}"
        );
        let (missing, note) = availability(REPO, &files(&["Parakeet/q8.gguf"]), Some(&l));
        assert!(missing.is_empty() && note.is_empty());
        // Never listed: nothing per package (the catalog says it once).
        assert_eq!(
            availability(REPO, &files(&["a"]), None),
            (Vec::new(), String::new())
        );
    }

    #[test]
    fn a_failed_listing_keeps_the_old_one_and_warns() {
        let wanted = set(&["Parakeet/q8.gguf"]);
        let old = RepoListing {
            listed_at: "2026-09-30T08:00:00Z".into(),
            present: Some(set(&["Parakeet/q8.gguf"])),
            checked: set(&["Parakeet/q8.gguf"]),
            error: String::new(),
        };
        let reason = "Hugging Face rate limit reached listing audio-cpp/audio.cpp-gguf — it \
                      resets in 55 s";
        let (l, warning) = settle(
            REPO,
            &wanted,
            Err(reason.into()),
            Some(&old),
            "2026-10-02T12:00:00Z",
        );
        assert_eq!(l.present, old.present, "the last good listing stays");
        assert_eq!(l.checked, old.checked, "with what it looked for");
        assert_eq!(l.listed_at, old.listed_at, "with its own date");
        assert_eq!(l.error, reason);
        let warning = warning.unwrap();
        assert!(
            warning.contains(REPO) && warning.contains("resets in 55 s"),
            "{warning}"
        );
        assert!(warning.contains("as of 2026-09-30"), "{warning}");

        // No earlier listing: unknown, said on the package too.
        let (l, warning) = settle(REPO, &wanted, Err("offline".into()), None, "now");
        assert_eq!(l.present, None);
        assert!(warning.unwrap().contains("unknown"));
        let (missing, note) = availability(REPO, &files(&["Parakeet/q8.gguf"]), Some(&l));
        assert!(missing.is_empty());
        assert_eq!(
            note,
            "could not check whether audio-cpp/audio.cpp-gguf publishes these files: offline"
        );
    }

    /// A kept listing knows only the files it looked for. A package the spec
    /// added (or a file it renamed) since then was never checked: unknown —
    /// Download stays — not "not published".
    #[test]
    fn a_kept_listing_does_not_judge_files_it_never_checked() {
        let then = set(&["Parakeet/q8.gguf"]);
        let (old, _) = settle(
            REPO,
            &then,
            Ok(files(&["Parakeet/q8.gguf"])),
            None,
            "2026-09-30T08:00:00Z",
        );
        // The spec grew a package; the next refresh is rate-limited.
        let now = set(&["Parakeet/q8.gguf", "New-GGUF/new-q8_0.gguf"]);
        let (kept, _) = settle(
            REPO,
            &now,
            Err("rate limited".into()),
            Some(&old),
            "2026-10-02T12:00:00Z",
        );
        let (missing, note) = availability(REPO, &files(&["New-GGUF/new-q8_0.gguf"]), Some(&kept));
        assert!(missing.is_empty(), "{missing:?}");
        assert!(
            note.starts_with(
                "whether audio-cpp/audio.cpp-gguf publishes New-GGUF/new-q8_0.gguf is unknown"
            ),
            "{note}"
        );
        // A file it did look for is still judged by it.
        let (missing, note) = availability(REPO, &files(&["Parakeet/q8.gguf"]), Some(&kept));
        assert!(missing.is_empty() && note.is_empty(), "{note}");

        // A listing saved before `checked` existed knows nothing either way.
        let legacy = RepoListing {
            listed_at: "2026-09-30T08:00:00Z".into(),
            present: Some(BTreeSet::new()),
            ..Default::default()
        };
        let (missing, note) = availability(REPO, &files(&["Parakeet/q8.gguf"]), Some(&legacy));
        assert!(missing.is_empty() && note.contains("unknown"), "{note}");
    }

    fn spec_of(repos: &[&str]) -> ModelSpec {
        let packages: Vec<serde_json::Value> = repos
            .iter()
            .map(|r| {
                json!({ "id": r.replace('/', "_"), "files": ["m.gguf"],
                        "download": { "kind": "huggingface_snapshot", "repo": r } })
            })
            .collect();
        crate::audio::parse_spec(&json!({ "family": "f", "packages": packages }))
    }

    /// The first 429 ends the asking: the repos after it are not listed
    /// against the exhausted window, keep what they had, and say why.
    #[tokio::test]
    async fn the_refresh_stops_asking_after_the_first_rate_limit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let hub = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models/a/one/tree/main"))
            .respond_with(ResponseTemplate::new(429).insert_header("ratelimit", "\"api\";r=0;t=55"))
            .expect(1)
            .mount(&hub)
            .await;
        for later in ["b/two", "c/three"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/models/{later}/tree/main")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .expect(0)
                .mount(&hub)
                .await;
        }
        let previous: BTreeMap<String, RepoListing> = [(
            "b/two".to_string(),
            RepoListing {
                listed_at: "2026-09-30T08:00:00Z".into(),
                present: Some(set(&["m.gguf"])),
                checked: set(&["m.gguf"]),
                error: String::new(),
            },
        )]
        .into();
        let specs = [spec_of(&["a/one", "b/two", "c/three"])];
        // One at a time, in repo order, so "after" is deterministic.
        let http = reqwest::Client::new();
        let (listings, warnings) = list_repos_at(&http, "", &hub.uri(), &specs, &previous, 1).await;
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("resets in 55 s"), "{warnings:?}");
        for w in &warnings[1..] {
            assert!(
                w.contains("not asked: Hugging Face's rate limit was reached"),
                "{w}"
            );
        }
        assert_eq!(
            listings["b/two"].present, previous["b/two"].present,
            "the skipped repo keeps its listing"
        );
        assert_eq!(listings["c/three"].present, None);
    }

    /// Snapshots persisted before listings existed still load.
    #[test]
    fn an_old_snapshot_reads_as_never_checked() {
        let old = json!({"fetched_at": "2026-09-01T00:00:00Z", "specs": []});
        let s: crate::audio::CatalogSnapshot = serde_json::from_value(old).unwrap();
        assert!(s.listings.is_empty() && s.warnings.is_empty());
    }
}
