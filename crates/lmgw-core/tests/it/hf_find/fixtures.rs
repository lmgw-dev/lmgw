//! The recorded hub (find-a-model design D38): what Hugging Face answered
//! to every read the finder made for §4.4's queries, recorded by
//! `scripts/hf-find-record.sh` without a token. A recording of what the hub
//! said, not a list lmgw ships or consults.
//!
//! `index.json` names each read's path (path and query, as the finder asks
//! it) and the entry of `answers.zip` that holds the answer (its status, its
//! rate-limit headers, its `Link` and its body), so the reads can be looked
//! through without unpacking anything. A read the recording lacks fails the
//! test with the script to run: the rules then ask the hub something the
//! last recording didn't.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lmgw_core::hf::search::{Hub, HubAnswer};

pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hf_find")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Index {
    /// What the recording holds and how it was made.
    about: String,
    /// The date it was recorded, UTC.
    recorded: String,
    /// path → entry in `answers.zip`.
    answers: BTreeMap<String, Entry>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Entry {
    entry: String,
    status: u16,
}

/// The hub as recorded, with the reads made of it.
pub struct Recorded {
    answers: BTreeMap<String, HubAnswer>,
    asked: Mutex<Vec<String>>,
}

impl Recorded {
    pub fn load() -> Self {
        let index: Index =
            serde_json::from_slice(&std::fs::read(dir().join("index.json")).expect(
                "tests/fixtures/hf_find/index.json: record it with scripts/hf-find-record.sh",
            ))
            .unwrap();
        let file = std::fs::File::open(dir().join("answers.zip")).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut answers = BTreeMap::new();
        for (path, e) in index.answers {
            let mut text = String::new();
            zip.by_name(&e.entry)
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            let answer: HubAnswer = serde_json::from_str(&text).unwrap();
            every_field(&path, &answer);
            answers.insert(path, answer);
        }
        Self {
            answers,
            asked: Mutex::default(),
        }
    }

    /// The reads made since the last call, in order.
    pub fn take_asked(&self) -> Vec<String> {
        std::mem::take(&mut self.asked.lock().unwrap())
    }

    /// Every recorded answer, by its path: what a recording that adds
    /// reads starts from (`scripts/hf-find-record.sh --add`).
    pub fn into_answers(self) -> BTreeMap<String, HubAnswer> {
        self.answers
    }
}

/// The date the recording was made, as its index says.
pub fn recorded() -> String {
    std::fs::read(dir().join("index.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Index>(&b).ok())
        .map(|i| i.recorded)
        .unwrap_or_default()
}

impl Hub for Recorded {
    async fn get(&self, path: &str) -> Result<HubAnswer, String> {
        self.asked.lock().unwrap().push(path.to_string());
        match self.answers.get(path) {
            Some(a) => Ok(a.clone()),
            None => panic!(
                "the recording has no answer for {path}: the rules ask the hub something new, \
                 so record again with scripts/hf-find-record.sh"
            ),
        }
    }
}

/// Every repo a kept listing holds has every field `HubModel` has now (D78,
/// revised): the recording writes them all, so a field the finder starts
/// reading, which an old recording lacks, would default silently.
fn every_field(path: &str, answer: &HubAnswer) {
    use lmgw_core::hf::search::HubModel;
    let listing = path.starts_with("/api/models?");
    let one = path.starts_with("/api/models/") && !path.contains("/tree/");
    if answer.status != 200 || !(listing || one) {
        return;
    }
    let fields: Vec<String> = match serde_json::to_value(HubModel::default()).unwrap() {
        serde_json::Value::Object(m) => m.keys().cloned().collect(),
        _ => unreachable!("HubModel is a struct"),
    };
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    let repos = match body {
        serde_json::Value::Array(a) => a,
        one => vec![one],
    };
    for repo in repos {
        for f in &fields {
            assert!(
                repo.get(f).is_some(),
                "the recording's answer for {path} has no `{f}`, which the finder reads now: \
                 record again with scripts/hf-find-record.sh"
            );
        }
    }
}

/// A hub that keeps every answer `inner` gives.
pub struct Recording<H: Hub> {
    pub inner: H,
    pub kept: Mutex<BTreeMap<String, HubAnswer>>,
}

impl<H: Hub> Hub for Recording<H> {
    async fn get(&self, path: &str) -> Result<HubAnswer, String> {
        if let Some(a) = self.kept.lock().unwrap().get(path) {
            return Ok(a.clone());
        }
        let a = self.inner.get(path).await?;
        self.kept
            .lock()
            .unwrap()
            .insert(path.to_string(), a.clone());
        Ok(a)
    }
}

/// `answer` as the recording keeps it: a listing's repos as the finder's
/// reader reads them (`HubModel`'s fields, the tags `tag_read` names), so
/// the hub's other fields and tags don't fill the repository (D78). Trees,
/// overviews and refusals are kept whole.
fn trimmed(path: &str, answer: &HubAnswer) -> HubAnswer {
    use lmgw_core::hf::search::{tag_read, HubModel};
    let trim = |mut m: HubModel| {
        m.tags.retain(|t| tag_read(t));
        m
    };
    let listing = path.starts_with("/api/models?");
    let one = path.starts_with("/api/models/") && !path.contains("/tree/");
    let body = match (answer.status, listing, one) {
        (200, true, _) => serde_json::from_str::<Vec<HubModel>>(&answer.body)
            .map(|ms| serde_json::to_string(&ms.into_iter().map(trim).collect::<Vec<_>>())),
        (200, _, true) => {
            serde_json::from_str::<HubModel>(&answer.body).map(|m| serde_json::to_string(&trim(m)))
        }
        _ => return answer.clone(),
    };
    HubAnswer {
        body: body.unwrap().unwrap(),
        ..answer.clone()
    }
}

/// Writes `kept` as the recording, replacing the last one.
pub fn save(kept: &BTreeMap<String, HubAnswer>, recorded: &str) {
    let dir = dir();
    std::fs::create_dir_all(&dir).unwrap();
    let mut index = Index {
        about: "Hugging Face's answers to the reads lmgw's finder made for the 28 queries of \
                the find-a-model design's section 4.4 and the later ones the regression test \
                names, on a 24 GiB and an 8 GiB card, recorded \
                without a token by scripts/hf-find-record.sh. Each entry of answers.zip is one \
                answer: its status, rate-limit headers, Link and body, a listing's repos \
                trimmed to the fields and tags the finder reads."
            .into(),
        recorded: recorded.into(),
        answers: BTreeMap::new(),
    };
    let file = std::fs::File::create(dir.join("answers.zip")).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9))
        // A fixed time, so a recording of the same answers is the same file.
        .last_modified_time(zip::DateTime::default());
    for (i, (path, answer)) in kept.iter().enumerate() {
        let entry = format!("{i:04}.json");
        zip.start_file(entry.as_str(), opts).unwrap();
        zip.write_all(
            serde_json::to_string(&trimmed(path, answer))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        index.answers.insert(
            path.clone(),
            Entry {
                entry,
                status: answer.status,
            },
        );
    }
    zip.finish().unwrap();
    std::fs::write(
        dir.join("index.json"),
        serde_json::to_string_pretty(&index).unwrap() + "\n",
    )
    .unwrap();
}
