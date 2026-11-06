//! How each sample was made: `<data_dir>/voice-lab/samples.json`, a
//! [`VoiceProvenance`] per library clip file name.
//!
//! Kept in the lab's directory, not beside the clips: the voice library is
//! the directory the audio containers read as their `voice_dir`, and holds
//! only what audio.cpp reads there (the clips and `prompt_text`). Each
//! record carries its clip's SHA-256, so a clip replaced since — an upload
//! under the same name, an edit by hand — no longer shows a record that
//! describes another recording; a record whose clip is gone is simply never
//! shown.

use std::collections::BTreeMap;
use std::path::Path;

use lmgw_api_types::audio_lab::Clip;
use lmgw_api_types::voice_lab::{VoiceProvenance, VoiceSample};
use sha2::{Digest, Sha256};

use super::LabFail;

const FILE: &str = "samples.json";

/// The records, by clip file name: none when there is no file, `Err`
/// naming it when it is there and cannot be read — the owner's to fix, and
/// never written over ([`record`] refuses).
pub fn read(lab_dir: &Path) -> Result<BTreeMap<String, VoiceProvenance>, String> {
    super::read_own_json(&lab_dir.join(FILE)).map(Option::unwrap_or_default)
}

/// Record `p` for clip `file`, replacing the record it had; with `drop`,
/// the records of the files it replaced go. `Err` (nothing written) when
/// the records' file cannot be read. The caller holds
/// [`super::library_write`].
pub fn record(
    lab_dir: &Path,
    file: &str,
    p: &VoiceProvenance,
    drop: &[String],
) -> Result<(), LabFail> {
    let mut all = read(lab_dir).map_err(super::unreadable)?;
    for f in drop {
        all.remove(f);
    }
    all.insert(file.to_string(), p.clone());
    let body = serde_json::to_vec_pretty(&all).map_err(|e| LabFail::internal(e.to_string()))?;
    std::fs::create_dir_all(lab_dir)
        .and_then(|()| super::write_atomic(&lab_dir.join(FILE), &body))
        .map_err(|e| LabFail::internal(format!("writing {FILE}: {e}")))
}

/// The SHA-256 of `bytes`, hex.
pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The library's clips (in `voices_dir`) with their records, and why
/// there are none when the records' file cannot be read (logged). A clip's
/// bytes are hashed only when it has a record. Blocking: reads the files.
pub fn samples(
    lab_dir: &Path,
    voices_dir: &Path,
    clips: Vec<Clip>,
) -> (Vec<VoiceSample>, Option<String>) {
    let (records, error) = match read(lab_dir) {
        Ok(r) => (r, None),
        Err(why) => {
            tracing::warn!("voice lab: the samples' records are not shown: {why}");
            (BTreeMap::new(), Some(why))
        }
    };
    let samples = clips
        .into_iter()
        .map(|clip| {
            let provenance = records.get(&clip.name).cloned();
            let provenance_current = provenance.as_ref().is_some_and(|p| {
                std::fs::read(voices_dir.join(&clip.name)).is_ok_and(|b| sha256(&b) == p.sha256)
            });
            VoiceSample {
                clip,
                provenance,
                provenance_current,
            }
        })
        .collect();
    (samples, error)
}
