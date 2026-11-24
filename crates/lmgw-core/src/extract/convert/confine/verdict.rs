//! Whether poppler, tesseract, pandoc and ffmpeg run confined as they are
//! installed here (performance round review S5): each is run once under the
//! rules its conversions get, at its probe (at start, on Settings › Documents' "Check
//! again", after it changed), and a tool that fails only when confined is
//! found then, not by a conversion. Such a tool runs with its limits alone
//! from then on (as where Landlock is missing), and the page says so with
//! the reason ([`state`]); its next probe runs it confined again.
//!
//! The check run is started through lmgw's reader like every conversion
//! run (no fork of the gateway), with the cleared environment, its own
//! process group and [`super::super::probe::TOOL_PROBE_TIMEOUT`]. The tool
//! probes themselves (`probe.rs`) still start their tools directly: not
//! moved to the reader yet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use lmgw_api_types::documents::{Confinement, ExtraKind};

use super::super::caps::ToolId;
use super::super::probe::{self, TOOL_PROBE_TIMEOUT};
use super::super::run::start;
use super::super::tools::{Fact, Found, ToolFacts};
use super::super::watch::Watched;

/// The check runs' failures, by the tool and the directory of its binary.
static FAILED: Mutex<Option<HashMap<(ToolId, PathBuf), String>>> = Mutex::new(None);

fn failed() -> std::sync::MutexGuard<'static, Option<HashMap<(ToolId, PathBuf), String>>> {
    FAILED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a binary's verdict is kept: its tool and its directory. poppler's
/// five binaries share one; poppler, tesseract, pandoc and ffmpeg in one
/// `/usr/bin` never do, so one tool's check, run beside the others', cannot
/// decide for them (verification of the performance fix round, SF1).
fn key(id: ToolId, bin: &Path) -> Option<(ToolId, PathBuf)> {
    let abs = start::absolute(bin)?;
    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
    abs.parent().map(|d| (id, d.to_path_buf()))
}

/// Why tool `id` at `bin` failed its confined check run, when it did.
pub fn failure(id: ToolId, bin: &Path) -> Option<String> {
    let k = key(id, bin)?;
    failed().as_ref().and_then(|m| m.get(&k).cloned())
}

fn record(id: ToolId, bin: &Path, outcome: Result<(), String>) {
    let Some(k) = key(id, bin) else {
        return;
    };
    let mut g = failed();
    let m = g.get_or_insert_with(HashMap::new);
    match outcome {
        Ok(()) => {
            m.remove(&k);
        }
        Err(why) => {
            tracing::warn!(
                "document tool at {} runs without confinement: {why}",
                bin.display()
            );
            m.insert(k, why);
        }
    }
}

/// tesseract's language data directory, as its probe read it.
pub fn tessdata_of(f: &Found) -> Option<PathBuf> {
    f.watched.iter().find_map(|w| match w {
        Watched::File { path, .. } => Some(path.clone()),
        Watched::Binary { .. } => None,
    })
}

/// Run `found` (poppler's or tesseract's) once under its rules through the
/// reader at `helper`, and keep the verdict (module doc). Nothing is run
/// where its rules cannot be made (the reason is the verdict then, from
/// `super::poppler_rules`/`super::tesseract_rules`).
pub async fn check(id: ToolId, found: &Found, helper: &Path) {
    let bin = &found.located.path;
    // A verdict of an earlier check does not stand in the way of this one.
    record(id, bin, Ok(()));
    // A version run, within the smallest limit its conversions run under.
    let (rules, args, memory): (_, &[&str], u64) = match id {
        ToolId::Poppler => (
            super::poppler_rules(bin, None),
            &["-v"],
            crate::extract::pdf::POPPLER_TEXT_MEMORY_LIMIT_BYTES,
        ),
        ToolId::Tesseract => (
            super::tesseract_rules(bin, tessdata_of(found).as_deref()),
            &["--list-langs"],
            crate::extract::convert::ocr::TESSERACT_MEMORY_LIMIT_BYTES,
        ),
        // Their conversions' rules (`filter`), stdin and stdout only.
        ToolId::Pandoc => (
            Ok(super::filter(None, 1 << 20)),
            &["--version"],
            crate::extract::convert::pandoc::PANDOC_MEMORY_LIMIT_BYTES,
        ),
        ToolId::Ffmpeg => (
            Ok(super::filter(None, 1 << 20)),
            &["-version"],
            crate::extract::convert::media::ffmpeg::FFMPEG_MEMORY_LIMIT_BYTES,
        ),
        _ => return,
    };
    let rules = match rules {
        Ok(r) => r,
        // Said by `state`, logged once by the run that meets it.
        Err(_) => return,
    };
    let out = match run(helper, bin, args, memory, &rules).await {
        Ok(out) => out,
        Err(Failed::Other(why)) => return record(id, bin, Err(why)),
        // Its limit, not its confinement, stopped it (S3): no word on the
        // confinement, which stays on; a conversion that meets the limit
        // says it by name.
        Err(Failed::Memory) => {
            tracing::warn!(
                "document tool at {} did not start within its memory limit of {} in its \
                 confinement check; its confinement stays on",
                bin.display(),
                crate::hf::fmt_bytes(memory)
            );
            return record(id, bin, Ok(()));
        }
    };
    let outcome = if id == ToolId::Tesseract {
        // Confined, it must see the language packs its probe listed.
        let listed = probe::languages(&out);
        let probed = found
            .extras
            .iter()
            .find(|e| e.kind == ExtraKind::Languages)
            .map_or(0, |e| e.present.len());
        if listed.len() < probed {
            Err(format!(
                "run confined, it listed {} of its {probed} language packs",
                listed.len()
            ))
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    record(id, bin, outcome);
}

/// Why a check run failed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failed {
    /// It ran out of its memory limit (`run::ran_out_of_memory`): no word
    /// on its confinement.
    Memory,
    /// Why, in words: the confinement's verdict.
    Other(String),
}

/// `bin args…` under `rules` through the reader: its stdout and stderr, or
/// why it failed.
pub(super) async fn run(
    helper: &Path,
    bin: &Path,
    args: &[&str],
    memory: u64,
    rules: &lmgw_doc_reader::sandbox::Confine,
) -> Result<String, Failed> {
    let Some(abs) = start::absolute(bin) else {
        return Err(Failed::Other("it was not found on PATH".to_string()));
    };
    let token = start::token();
    let mut cmd = tokio::process::Command::new(helper);
    cmd.env_clear()
        .env("PATH", super::super::run::confined::PATH);
    for (k, v) in start::passed_on() {
        cmd.env(k, v);
    }
    let cpu = super::super::run::cpu::CpuBound::of(TOOL_PROBE_TIMEOUT).seconds;
    cmd.args(start::argv(
        Some(rules),
        &abs,
        args,
        (memory, cpu),
        false,
        &token,
    ));
    crate::child::group::own_group(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| Failed::Other(format!("lmgw's document reader could not be run: {e}")))?;
    let group = crate::child::group::KillGroup::of(&child);
    let out = match tokio::time::timeout(TOOL_PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(Failed::Other(format!(
                "its confined run could not be waited for: {e}"
            )))
        }
        Err(_) => {
            return Err(Failed::Other(format!(
                "its confined run did not finish within {} s (TOOL_PROBE_TIMEOUT)",
                TOOL_PROBE_TIMEOUT.as_secs()
            )))
        }
    };
    group.disarm();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if super::super::run::ran_out_of_memory(&out.status, &stderr) {
            return Err(Failed::Memory);
        }
        let line = if start::by_reader(&stderr, &token) {
            start::reader_line(&stderr).to_string()
        } else {
            stderr.lines().next().unwrap_or("").trim().to_string()
        };
        return Err(Failed::Other(format!(
            "its confined run failed ({}): {line}",
            out.status
        )));
    }
    Ok(format!(
        "{}\n{stderr}",
        String::from_utf8_lossy(&out.stdout)
    ))
}

/// Whether the reader can confine here, as its probe said ([`Confinement`]
/// on Settings › Documents).
pub fn confinement(facts: &ToolFacts) -> Confinement {
    match super::super::reader::path(facts) {
        None => Confinement {
            why_not: Some(super::super::run::start::no_reader()),
            ..Confinement::default()
        },
        Some(helper) => match super::answer(&helper) {
            Some(Ok(abi)) => Confinement {
                abi: Some(abi),
                why_not: None,
                lacks: lmgw_doc_reader::sandbox::abi::lacking_said(abi),
            },
            Some(Err(why)) => Confinement {
                why_not: Some(why),
                ..Confinement::default()
            },
            None => Confinement {
                why_not: Some("lmgw's document reader was not asked yet".to_string()),
                ..Confinement::default()
            },
        },
    }
}

/// Why the found tool `id` runs without confinement here, or `None` when
/// it runs confined (or lmgw does not start it: a missing tool, LibreOffice
/// whose probe already says it is refused).
pub fn state(id: ToolId, facts: &ToolFacts) -> Option<String> {
    let Some(Fact::Found(found)) = facts.fact(id) else {
        return None;
    };
    let landlock = confinement(facts);
    match id {
        ToolId::Poppler | ToolId::Tesseract | ToolId::Pandoc | ToolId::Ffmpeg | ToolId::Reader => {
            if let Some(why) = landlock.why_not {
                return Some(why);
            }
        }
        _ => return None,
    }
    let bin = &found.located.path;
    let rules = match id {
        ToolId::Poppler => super::poppler_rules(bin, None).map(drop),
        ToolId::Tesseract => super::tesseract_rules(bin, tessdata_of(found).as_deref()).map(drop),
        _ => failure(id, bin).map_or(Ok(()), Err),
    };
    rules.err()
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
