//! How long a download takes on this connection (find-a-model design D23).
//!
//! The rate is measured, never assumed: this machine's finished Hugging Face
//! downloads, weighted by bytes (their bytes summed over their run times
//! summed, from each job's `started_at` and `finished_at`), so a 50 MiB
//! projector doesn't weigh like a 17 GiB file. Every finished download the
//! job table keeps counts: the table is bounded by its own retention
//! setting, so there is no second count here. Before any download finished,
//! the rate is the header reads' the plan just made, from the same CDN
//! (§2.2 measured them slower than a bulk read, the safe side). With
//! neither, there is no time.
//!
//! The time includes the engine image when it must be fetched: its layers
//! still missing here, the sizes the registry's manifest lists for this
//! machine's platform less the layers Podman's image store has already
//! (machine-check design §5, §1.6, which reads them the same way). When the
//! store's layers can't be read, every layer counts, and the answer says the
//! time is at most that long.
//!
//! A download's bytes are what that transfer moved: a download that went on
//! from a kept `.part` moved its length less the byte it went on from
//! (`resumed_from` in the job's result, D46), and its time is the job's less
//! the read of the `.part` that checked its hash before the rest was asked
//! for (`check_secs`, D130). The job table times in whole seconds, so a
//! download that took less than one has no time to weigh and is left out.

use lmgw_api_types::model_find::{DownloadEstimate, RateSource};

use crate::state::SharedState;
use crate::store::JobRow;

/// Bytes that took `secs` to arrive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timed {
    pub bytes: u64,
    pub secs: f64,
}

/// The bytes-weighted rate of `samples`: their bytes over their seconds.
/// `None` without a sample that moved bytes over a measurable time.
pub fn weighted(samples: &[Timed]) -> Option<f64> {
    let (bytes, secs) = samples
        .iter()
        .filter(|t| t.bytes > 0 && t.secs > 0.0)
        .fold((0u64, 0f64), |(b, s), t| {
            (b.saturating_add(t.bytes), s + t.secs)
        });
    (bytes > 0 && secs > 0.0).then(|| bytes as f64 / secs)
}

/// What each finished `hf_download` job of `rows` moved, and in how long
/// (module doc). Rows of other kinds or statuses, and rows without both
/// times, are left out.
pub fn from_job_rows(rows: &[JobRow]) -> Vec<Timed> {
    let at = |t: &Option<String>| {
        chrono::NaiveDateTime::parse_from_str(t.as_deref()?, "%Y-%m-%d %H:%M:%S").ok()
    };
    rows.iter()
        .filter(|r| r.kind == crate::jobs::JobKind::HfDownload.as_str() && r.status == "done")
        .filter_map(|r| {
            let secs = (at(&r.finished_at)? - at(&r.started_at)?).num_seconds();
            let result: serde_json::Value = serde_json::from_str(r.result.as_deref()?).ok()?;
            let bytes = result["bytes"].as_u64()?;
            let from = result["resumed_from"].as_u64().unwrap_or(0);
            let checked = result["check_secs"].as_f64().unwrap_or(0.0);
            Some(Timed {
                bytes: bytes.saturating_sub(from),
                secs: (secs as f64 - checked).max(0.0),
            })
        })
        .collect()
}

/// The rate of this machine's finished Hugging Face downloads, bytes per
/// second; `None` before one finished.
pub async fn of_downloads(state: &SharedState) -> Option<f64> {
    let kind = crate::jobs::JobKind::HfDownload.as_str();
    let rows = crate::store::list_jobs(&state.db, Some(kind), false, 0)
        .await
        .map_err(|e| tracing::warn!("the download rate could not be read: {e}"))
        .ok()?;
    weighted(&from_job_rows(&rows))
}

/// The engine image a download would also fetch (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// It is on this machine.
    Here,
    /// It isn't, and the registry lists this many bytes still to fetch,
    /// the layers the image store has already left out.
    Missing(u64),
    /// It isn't, and fetching it takes at most this many bytes: every layer
    /// the registry lists, because the image store's layers couldn't be read
    /// (the sentence says why); a layer shared with an image here isn't
    /// fetched again.
    AtMost(u64, String),
    /// Whether it is here, or its size, isn't known: the sentence says why.
    NotKnown(String),
}

/// The engine image `target`'s models run in, and what fetching it costs.
/// Asks Podman whether it is here and, when it isn't, the registry's
/// manifest for this platform's layer sizes.
pub async fn engine(state: &SharedState, target: &str) -> Engine {
    use crate::backends::oci::{Platform, PullRef};
    use crate::backends::pull::{self, Presence};

    let snap = state.snapshot();
    let s = &snap.settings;
    let image = match target {
        "chat" => &s.router.image,
        "aux" => &s.aux_router.image,
        "audio" => &s.audio.image,
        "image" => &s.image.image,
        _ => return Engine::NotKnown(format!("{target} is no class with an engine")),
    }
    .trim()
    .to_string();
    let bound = crate::web::hf::engine::PRESENCE_BOUND;
    match crate::web::hf::engine::within(bound, &image, pull::presence(state, &image)).await {
        Presence::Present => return Engine::Here,
        Presence::Unknown(why) => return Engine::NotKnown(why),
        Presence::Missing => {}
    }
    let Some(at) = PullRef::parse(&image) else {
        return Engine::NotKnown(format!("{image} names no registry manifest"));
    };
    let client = match state.builds.registry_client() {
        Ok(c) => c,
        Err(e) => return Engine::NotKnown(e),
    };
    let blobs = match client.platform_blobs(&at, Platform::host()).await {
        Ok(b) => b,
        Err(e) => return Engine::NotKnown(format!("the registry's manifest for {image}: {e}")),
    };
    match state.machine.layers_here(state).await {
        Ok(here) => Engine::Missing(still_to_fetch(&blobs.sizes, &here)),
        Err(why) => Engine::AtMost(blobs.total(), why),
    }
}

/// The bytes of `blobs` (short ID → size) the image store `here` doesn't
/// have: what a pull fetches ("Already exists" for the rest).
pub fn still_to_fetch(
    blobs: &std::collections::BTreeMap<String, u64>,
    here: &std::collections::BTreeSet<String>,
) -> u64 {
    blobs
        .iter()
        .filter(|(id, _)| !here.contains(*id))
        .map(|(_, b)| *b)
        .fold(0, u64::saturating_add)
}

/// The time to fetch `files_bytes` and the `engine`, at the downloads' rate
/// `downloads`, else the rate of `header_reads` (module doc).
pub fn estimate(
    files_bytes: u64,
    engine: &Engine,
    downloads: Option<f64>,
    header_reads: &[Timed],
) -> DownloadEstimate {
    let (rate, from) = match (downloads, weighted(header_reads)) {
        (Some(r), _) => (Some(r), RateSource::Downloads),
        (None, Some(r)) => (Some(r), RateSource::HeaderReads),
        (None, None) => (None, RateSource::None),
    };
    let mut notes = Vec::new();
    let engine_bytes = match engine {
        Engine::Here => None,
        Engine::Missing(b) => Some(*b),
        Engine::AtMost(b, why) => {
            notes.push(format!(
                "the engine's download counts every layer of its image, {}, because lmgw \
                 couldn't read which layers are here already ({why}): layers it shares with \
                 an image on this computer aren't fetched again, so the time may be shorter",
                crate::hf::fmt_bytes(*b)
            ));
            Some(*b)
        }
        Engine::NotKnown(why) => {
            notes.push(format!(
                "the engine's download isn't counted: {why}, so the time may be longer"
            ));
            None
        }
    };
    if from == RateSource::HeaderReads {
        notes.push(
            "no download has finished on this machine yet, so the time comes from the header \
             reads just made, which run slower than a download"
                .into(),
        );
    }
    let total = files_bytes.saturating_add(engine_bytes.unwrap_or(0));
    let secs = rate
        .filter(|r| *r > 0.0)
        .map(|r| (total as f64 / r).ceil() as u64);
    DownloadEstimate {
        files_bytes,
        engine_bytes,
        rate_bytes_per_sec: rate,
        rate_from: from,
        secs,
        sentence: secs.map(sentence),
        notes,
    }
}

/// "about 4 minutes on your connection", rounded up to the minute.
pub fn sentence(secs: u64) -> String {
    if secs < 60 {
        return "less than a minute on your connection".into();
    }
    let minutes = secs.div_ceil(60);
    let plural = |n: u64, one: &str| match n {
        1 => format!("1 {one}"),
        n => format!("{n} {one}s"),
    };
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("about {} on your connection", plural(m, "minute")),
        (h, 0) => format!("about {} on your connection", plural(h, "hour")),
        (h, m) => format!(
            "about {} {} on your connection",
            plural(h, "hour"),
            plural(m, "minute")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, status: &str, started: &str, finished: &str, result: &str) -> JobRow {
        JobRow {
            id: 1,
            kind: kind.into(),
            key: None,
            label: String::new(),
            status: status.into(),
            input: "{}".into(),
            progress: "{}".into(),
            result: Some(result.into()),
            error: None,
            created_at: started.into(),
            started_at: Some(started.into()),
            finished_at: Some(finished.into()),
        }
    }

    /// D23: weighted by bytes, so a 50 MB projector in 10 s doesn't pull a
    /// 17 GB file at 100 MB/s down to the mean of two rates; a resumed
    /// download counts the bytes it moved; other jobs, failures and a
    /// download under a second are left out.
    #[test]
    fn the_rate_is_weighted_by_bytes() {
        const MB: u64 = 1_000_000;
        let rows = [
            row(
                "hf_download",
                "done",
                "2026-11-04 10:00:00",
                "2026-11-04 10:02:50",
                &format!(r#"{{"bytes": {}}}"#, 17_000 * MB),
            ),
            row(
                "hf_download",
                "done",
                "2026-11-04 10:03:00",
                "2026-11-04 10:03:10",
                &format!(r#"{{"bytes": {}}}"#, 50 * MB),
            ),
            row(
                "hf_download",
                "done",
                "2026-11-04 11:00:00",
                "2026-11-04 11:00:20",
                &format!(
                    r#"{{"bytes": {}, "resumed_from": {}}}"#,
                    3_000 * MB,
                    1_000 * MB
                ),
            ),
            row(
                "hf_download",
                "failed",
                "2026-11-04 12:00:00",
                "2026-11-04 12:00:09",
                "{}",
            ),
            row(
                "image_pull",
                "done",
                "2026-11-04 12:00:00",
                "2026-11-04 12:00:01",
                r#"{"bytes": 5}"#,
            ),
            row(
                "hf_download",
                "done",
                "2026-11-04 13:00:00",
                "2026-11-04 13:00:00",
                &format!(r#"{{"bytes": {}}}"#, 9 * MB),
            ),
        ];
        let samples = from_job_rows(&rows);
        assert_eq!(samples.len(), 4, "{samples:?}");
        let rate = weighted(&samples).unwrap();
        let want = (17_000 + 50 + 2_000) as f64 * MB as f64 / (170.0 + 10.0 + 20.0);
        assert!((rate - want).abs() < 1.0, "{rate} vs {want}");
        // The mean of the rates would be far lower: 100, 5 and 100 MB/s.
        assert!(rate > 95.0 * MB as f64, "{rate}");
        assert_eq!(weighted(&[]), None);
    }

    /// A resumed download's time leaves out the read of its `.part` that
    /// checked the hash (D130), which moved nothing; a job whose check took
    /// all its time has none left and is left out.
    #[test]
    fn a_resumes_hash_check_is_not_download_time() {
        const MB: u64 = 1_000_000;
        let rows = [
            row(
                "hf_download",
                "done",
                "2026-11-04 10:00:00",
                "2026-11-04 10:00:30",
                &format!(
                    r#"{{"bytes": {}, "resumed_from": {}, "check_secs": 10.0}}"#,
                    3_000 * MB,
                    1_000 * MB
                ),
            ),
            row(
                "hf_download",
                "done",
                "2026-11-04 11:00:00",
                "2026-11-04 11:00:05",
                &format!(
                    r#"{{"bytes": {}, "resumed_from": {}, "check_secs": 5.4}}"#,
                    3_000 * MB,
                    3_000 * MB
                ),
            ),
        ];
        let samples = from_job_rows(&rows);
        assert_eq!(
            samples[0],
            Timed {
                bytes: 2_000 * MB,
                secs: 20.0
            }
        );
        assert_eq!(weighted(&samples), Some(100.0 * MB as f64));
    }

    /// The engine's bytes are the layers the image store doesn't have.
    #[test]
    fn only_the_layers_not_here_are_fetched() {
        let blobs = [
            ("aaa".to_string(), 100u64),
            ("bbb".into(), 20),
            ("ccc".into(), 3),
        ]
        .into_iter()
        .collect();
        let here = ["bbb".to_string()].into_iter().collect();
        assert_eq!(still_to_fetch(&blobs, &here), 103);
        assert_eq!(still_to_fetch(&blobs, &Default::default()), 123);
    }

    /// The time covers the files and the engine's missing layers, rounded up
    /// to the minute; the header reads stand in before a download finished,
    /// and an engine of unknown size is said.
    #[test]
    fn the_estimate_counts_the_engine_and_says_its_source() {
        const MB: u64 = 1_000_000;
        let e = estimate(
            16_800 * MB,
            &Engine::Missing(3_400 * MB),
            Some(66.7 * MB as f64),
            &[],
        );
        assert_eq!(e.rate_from, RateSource::Downloads);
        assert_eq!(e.engine_bytes, Some(3_400 * MB));
        assert_eq!(e.secs, Some((20_200.0f64 / 66.7).ceil() as u64));
        assert_eq!(
            e.sentence.as_deref(),
            Some("about 6 minutes on your connection")
        );

        let reads = [Timed {
            bytes: 11 * MB,
            secs: 0.25,
        }];
        let e = estimate(16_800 * MB, &Engine::Here, None, &reads);
        assert_eq!(e.rate_from, RateSource::HeaderReads);
        assert_eq!(e.engine_bytes, None);
        assert_eq!(e.secs, Some((16_800.0f64 / 44.0).ceil() as u64));
        assert!(e.notes[0].contains("header reads"), "{:?}", e.notes);

        let e = estimate(
            MB,
            &Engine::NotKnown("podman did not answer".into()),
            None,
            &[],
        );
        assert_eq!((e.secs, e.rate_from), (None, RateSource::None));
        assert!(
            e.notes[0].contains("podman did not answer"),
            "{:?}",
            e.notes
        );

        // The store's layers unread: every layer, said to be an upper bound.
        let e = estimate(
            MB,
            &Engine::AtMost(3_400 * MB, "podman info exited 125".into()),
            Some(MB as f64),
            &[],
        );
        assert_eq!(e.engine_bytes, Some(3_400 * MB));
        assert!(
            e.notes[0].contains("every layer") && e.notes[0].contains("may be shorter"),
            "{:?}",
            e.notes
        );

        assert_eq!(sentence(59), "less than a minute on your connection");
        assert_eq!(sentence(61), "about 2 minutes on your connection");
        assert_eq!(sentence(3600), "about 1 hour on your connection");
        assert_eq!(sentence(3601), "about 1 hour 1 minute on your connection");
    }
}
