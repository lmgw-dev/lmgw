//! Podman's compat-mode pull stream, read (machine-check design §5).
//!
//! The libpod pull with `compatMode=true` answers with one JSON object per
//! line, Docker's progress format. Measured against Podman 5.8.9
//! (`docker.io/library/golang:1.24-bookworm`):
//!
//! - `{"status":"Pulling fs layer","progressDetail":{},"id":"89edcaae7ec4"}`
//!   when a blob's download begins — the config blob too, last;
//! - `{"status":"Downloading","progressDetail":{"current":48875203,"total":79128635},
//!   "progress":"[====>   ]  48.88MB/79.13MB","id":"f7bdfd728ac2"}` about once a
//!   second per blob in flight, so a blob that arrives within a second has
//!   none;
//! - `{"status":"Already exists","progressDetail":{},"id":"4f4fb700ef54"}` for
//!   a blob the store had;
//! - `{"status":"Download complete","progressDetail":{},"id":"…"}` once a blob
//!   is stored (the config blob's twice);
//! - no closing line: the answer ends when the image is stored.
//!
//! A failure before the stream is an HTTP error with `{"message": …}`
//! (`manifest unknown: manifest unknown`, 404); one during it is an
//! `{"error": …, "errorDetail": {"message": …}}` line. An older Podman that
//! does not know `compatMode` answers in libpod's own format, `{"stream":
//! …}` lines with no byte counts; those are read as words.
//!
//! Each blob is named by the first twelve hex digits of its digest, which is
//! how [`Tally`] matches it to the registry manifest's sizes.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::backends::oci::Blobs;
use crate::hf::fmt_bytes;

#[derive(Deserialize, Default)]
#[serde(default)]
struct Line {
    status: Option<String>,
    id: Option<String>,
    #[serde(rename = "progressDetail")]
    progress_detail: Option<Detail>,
    error: Option<String>,
    #[serde(rename = "errorDetail")]
    error_detail: Option<ErrorDetail>,
    stream: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Detail {
    current: Option<u64>,
    total: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ErrorDetail {
    message: Option<String>,
}

/// What happened to one blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Its download began.
    Begun,
    /// `current` of `total` bytes are in.
    Downloading { current: u64, total: Option<u64> },
    /// The store had it: nothing to download.
    Exists,
    /// It is stored.
    Done,
}

/// One line of the answer, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// News of one blob, by its short id; `words` is the line as a person
    /// reads it (`f7bdfd728ac2: Downloading 46.6 of 75.5 MiB`).
    Blob {
        id: String,
        step: Step,
        words: String,
    },
    /// The pull failed: Podman's message.
    Failed(String),
    /// Words about no one blob: a status without an id, libpod's own
    /// `stream` lines, a line that is not the format.
    Note(String),
}

/// Read one line; `None` for a blank one.
pub fn parse(line: &str) -> Option<Event> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Ok(l) = serde_json::from_str::<Line>(line) else {
        return Some(Event::Note(line.to_string()));
    };
    if let Some(msg) = l
        .error_detail
        .and_then(|d| d.message)
        .or(l.error)
        .filter(|m| !m.trim().is_empty())
    {
        return Some(Event::Failed(msg.trim().to_string()));
    }
    let status = l.status.unwrap_or_default();
    let id = l.id.unwrap_or_default();
    if id.is_empty() || status.is_empty() {
        let words = [Some(status), l.stream, l.message]
            .into_iter()
            .flatten()
            .map(|s| s.trim().to_string())
            .find(|s| !s.is_empty())?;
        return Some(Event::Note(words));
    }
    let step = match status.as_str() {
        "Pulling fs layer" | "Waiting" => Step::Begun,
        "Downloading" => {
            let d = l.progress_detail.unwrap_or_default();
            Step::Downloading {
                current: d.current.unwrap_or(0),
                total: d.total.filter(|t| *t > 0),
            }
        }
        "Already exists" => Step::Exists,
        "Download complete" | "Pull complete" => Step::Done,
        _ => {
            return Some(Event::Note(format!("{id}: {status}")));
        }
    };
    // The sizes from `progressDetail`'s bytes, in lmgw's binary units: the
    // `progress` bar beside them says them in decimal MB.
    let words = match step {
        Step::Downloading {
            current,
            total: Some(total),
        } => format!(
            "{id}: {status} {}",
            lmgw_api_types::units::fmt_bytes_of(current, total)
        ),
        Step::Downloading {
            current,
            total: None,
        } if current > 0 => format!(
            "{id}: {status} {} so far",
            lmgw_api_types::units::fmt_bytes_so_far(current)
        ),
        _ => format!("{id}: {status}"),
    };
    Some(Event::Blob { id, step, words })
}

/// One blob's state in a [`Tally`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Begun,
    Downloading,
    Exists,
    Done,
}

#[derive(Debug, Clone, Copy)]
struct Blob {
    state: State,
    current: u64,
    /// The size the stream told, from a `Downloading` line's total.
    total: Option<u64>,
}

/// Where the manifest's sizes stand.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Manifest {
    /// Still being read.
    Pending,
    /// Read: size by short id.
    Read(BTreeMap<String, u64>),
    /// Not to be had, and why.
    Off(String),
}

/// The bytes of one pull: what the stream reported, against the sizes the
/// registry's manifest lists (machine-check design §5). The total is the
/// manifest's blobs less those the store already had ("Already exists");
/// it is not known while the manifest is being read, when it could not be,
/// or once the stream names a blob the manifest does not list (the manifest
/// was then not the one Podman pulls) — [`Self::size_note`] says which.
#[derive(Debug, Clone)]
pub struct Tally {
    manifest: Manifest,
    blobs: BTreeMap<String, Blob>,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            manifest: Manifest::Pending,
            blobs: BTreeMap::new(),
        }
    }
}

impl Tally {
    /// The manifest's sizes are in.
    pub fn set_manifest(&mut self, blobs: Blobs) {
        self.manifest = Manifest::Read(blobs.sizes);
    }

    /// The manifest could not be read: `why`.
    pub fn manifest_failed(&mut self, why: String) {
        self.manifest = Manifest::Off(why);
    }

    /// Blob `id` took `step`.
    pub fn note(&mut self, id: &str, step: &Step) {
        let b = self.blobs.entry(id.to_string()).or_insert(Blob {
            state: State::Begun,
            current: 0,
            total: None,
        });
        match step {
            Step::Begun => {}
            Step::Downloading { current, total } => {
                b.state = State::Downloading;
                b.current = *current;
                if total.is_some() {
                    b.total = *total;
                }
            }
            Step::Exists => b.state = State::Exists,
            Step::Done => b.state = State::Done,
        }
    }

    /// A blob the stream named that the manifest does not list.
    fn unlisted(&self) -> Option<&str> {
        let Manifest::Read(sizes) = &self.manifest else {
            return None;
        };
        self.blobs
            .keys()
            .find(|id| !sizes.contains_key(*id))
            .map(String::as_str)
    }

    /// A blob's size: the manifest's, else the stream's.
    fn size_of(&self, id: &str, b: &Blob) -> Option<u64> {
        match &self.manifest {
            Manifest::Read(sizes) => sizes.get(id).copied().or(b.total),
            _ => b.total,
        }
    }

    /// Compressed bytes downloaded so far: each blob's progress, and the
    /// whole of each stored one whose size is known.
    pub fn done_bytes(&self) -> u64 {
        self.blobs
            .iter()
            .map(|(id, b)| match b.state {
                State::Begun | State::Exists => 0,
                State::Downloading => b.current,
                State::Done => self.size_of(id, b).unwrap_or(b.current),
            })
            .sum()
    }

    /// The whole download, when it is known.
    pub fn total_bytes(&self) -> Option<u64> {
        let Manifest::Read(sizes) = &self.manifest else {
            return None;
        };
        if self.unlisted().is_some() {
            return None;
        }
        let had: u64 = self
            .blobs
            .iter()
            .filter(|(_, b)| b.state == State::Exists)
            .filter_map(|(id, _)| sizes.get(id))
            .sum();
        Some(sizes.values().sum::<u64>().saturating_sub(had))
    }

    /// Why [`Self::total_bytes`] is `None`, when it is.
    pub fn size_note(&self) -> Option<String> {
        match &self.manifest {
            Manifest::Pending => {
                Some("size not known yet — reading the registry's manifest".into())
            }
            Manifest::Off(why) => Some(format!(
                "size not known — the registry's manifest could not be read: {why}"
            )),
            Manifest::Read(_) => self.unlisted().map(|id| {
                format!(
                    "size not known — Podman pulls blob {id}, which the registry's manifest for \
                     this machine does not list"
                )
            }),
        }
    }

    /// One line for the log once the pull ended: what came down.
    pub fn summary(&self) -> String {
        let had = self
            .blobs
            .values()
            .filter(|b| b.state == State::Exists)
            .count();
        let fetched = self.blobs.len() - had;
        let mut s = format!(
            "{} downloaded in {fetched} blob(s)",
            fmt_bytes(self.done_bytes())
        );
        if had > 0 {
            s.push_str(&format!(", {had} already on this machine"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(id: &str, step: Step) -> Event {
        let words = match &step {
            Step::Downloading { .. } => format!("{id}: Downloading"),
            _ => String::new(),
        };
        Event::Blob {
            id: id.into(),
            step,
            words,
        }
    }

    #[test]
    fn each_status_kind_reads() {
        assert_eq!(
            parse(r#"{"status":"Pulling fs layer","progressDetail":{},"id":"89edcaae7ec4"}"#),
            Some(Event::Blob {
                id: "89edcaae7ec4".into(),
                step: Step::Begun,
                words: "89edcaae7ec4: Pulling fs layer".into(),
            })
        );
        assert_eq!(
            parse(
                r#"{"status":"Downloading","progressDetail":{"current":48875203,"total":79128635},"progress":"[==============================>                    ]  48.88MB/79.13MB","id":"f7bdfd728ac2"}"#
            ),
            Some(Event::Blob {
                id: "f7bdfd728ac2".into(),
                step: Step::Downloading {
                    current: 48_875_203,
                    total: Some(79_128_635),
                },
                words: "f7bdfd728ac2: Downloading 46.6 of 75.5 MiB".into(),
            })
        );
        assert!(matches!(
            parse(r#"{"status":"Already exists","progressDetail":{},"id":"4f4fb700ef54"}"#),
            Some(Event::Blob {
                step: Step::Exists,
                ..
            })
        ));
        assert!(matches!(
            parse(r#"{"status":"Download complete","progressDetail":{},"id":"e0cffc405270"}"#),
            Some(Event::Blob {
                step: Step::Done,
                ..
            })
        ));
        assert_eq!(
            parse(r#"{"error":"x","errorDetail":{"message":"reading blob: connection reset"}}"#),
            Some(Event::Failed("reading blob: connection reset".into()))
        );
        assert_eq!(
            parse(r#"{"stream":"Trying to pull docker.io/library/x:y...\n"}"#),
            Some(Event::Note(
                "Trying to pull docker.io/library/x:y...".into()
            ))
        );
        assert_eq!(parse("  "), None);
        assert_eq!(parse("garbage"), Some(Event::Note("garbage".into())));
    }

    fn manifest() -> Blobs {
        Blobs {
            sizes: [
                ("aaaaaaaaaaaa".to_string(), 1000),
                ("bbbbbbbbbbbb".to_string(), 500),
                ("cccccccccccc".to_string(), 32),
                ("c0ff1ec0ff1e".to_string(), 10),
            ]
            .into(),
        }
    }

    fn feed(t: &mut Tally, events: &[Event]) {
        for e in events {
            if let Event::Blob { id, step, .. } = e {
                t.note(id, step);
            }
        }
    }

    /// The recorded run's shape: blobs begin, one the store had, progress,
    /// completions in another order, the config last.
    #[test]
    fn the_tally_follows_a_pull_against_the_manifest() {
        let mut t = Tally::default();
        assert_eq!(t.total_bytes(), None);
        assert!(t
            .size_note()
            .unwrap()
            .contains("reading the registry's manifest"));
        t.set_manifest(manifest());
        assert_eq!(t.total_bytes(), Some(1542));
        assert_eq!(t.size_note(), None);
        feed(
            &mut t,
            &[
                blob("cccccccccccc", Step::Exists),
                blob("aaaaaaaaaaaa", Step::Begun),
                blob("bbbbbbbbbbbb", Step::Begun),
                blob(
                    "aaaaaaaaaaaa",
                    Step::Downloading {
                        current: 400,
                        total: Some(1000),
                    },
                ),
            ],
        );
        assert_eq!(
            t.total_bytes(),
            Some(1510),
            "what the store had leaves the total"
        );
        assert_eq!(t.done_bytes(), 400);
        // A blob that arrived within a second: no progress line, its
        // manifest size once stored.
        feed(&mut t, &[blob("bbbbbbbbbbbb", Step::Done)]);
        assert_eq!(t.done_bytes(), 900);
        feed(
            &mut t,
            &[
                blob("aaaaaaaaaaaa", Step::Done),
                blob("c0ff1ec0ff1e", Step::Begun),
                blob("c0ff1ec0ff1e", Step::Done),
                blob("c0ff1ec0ff1e", Step::Done),
            ],
        );
        assert_eq!(t.done_bytes(), 1510);
        assert_eq!(t.total_bytes(), Some(1510));
        assert!(
            t.summary()
                .contains("in 3 blob(s), 1 already on this machine"),
            "{}",
            t.summary()
        );
    }

    #[test]
    fn an_unlisted_blob_or_no_manifest_leaves_the_size_unknown() {
        let mut t = Tally::default();
        t.set_manifest(manifest());
        feed(
            &mut t,
            &[blob(
                "dddddddddddd",
                Step::Downloading {
                    current: 5,
                    total: Some(9),
                },
            )],
        );
        assert_eq!(t.total_bytes(), None);
        assert!(t.size_note().unwrap().contains("blob dddddddddddd"));
        assert_eq!(t.done_bytes(), 5, "bytes still count");

        let mut t = Tally::default();
        t.manifest_failed("ghcr.io/o/r: the registry answered 500".into());
        feed(
            &mut t,
            &[
                blob(
                    "aaaaaaaaaaaa",
                    Step::Downloading {
                        current: 7,
                        total: Some(10),
                    },
                ),
                blob("aaaaaaaaaaaa", Step::Done),
            ],
        );
        assert_eq!(t.total_bytes(), None);
        assert_eq!(
            t.done_bytes(),
            10,
            "a stored blob counts at the stream's size"
        );
        assert!(t.size_note().unwrap().contains("answered 500"));
    }
}
