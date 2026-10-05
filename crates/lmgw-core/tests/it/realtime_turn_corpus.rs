//! **Local only, never in CI**: the owner's labelled pauses replayed
//! through the Rust `semantic_vad` rule and the Rust Smart Turn (realtime
//! design §6.3), to check the per-eagerness defaults against the Python
//! numbers they were chosen from.
//!
//! The recordings are the owner's own voice: they never enter the
//! repository, and nothing derived from them does — this test reads them
//! from wherever `LMGW_TURN_CORPUS` points and prints numbers only (no
//! transcripts, no audio). Without the variable it reads nothing.
//!
//! ```text
//! LMGW_TURN_CORPUS=<out>/scripted-1:<out>/scripted-2 \
//!   cargo test -p lmgw-core --test it realtime_turn_corpus -- --ignored --nocapture
//! ```
//!
//! Each directory is one run of the spike's analysis
//! (`spontaneous/run.sh`): `pauses.csv` (one row per Silero pause ≥ 150 ms,
//! with its label, its 32 ms-frame position and the Python Smart Turn
//! score `pB`) and `wav/<clip>.wav` (the clip at 16 kHz). The rows are
//! selected as the Python report's combined table selects them: sections
//! B C D F K L M, INCOMPLETE pauses and true ends (block ends and final
//! silences of at least 200 ms).
//!
//! Three replays per eagerness:
//! - **Python formula on `pB`** — `report_scripted.py`'s `ctime` again: it
//!   must reproduce the published numbers, which proves the selection;
//! - **Rust rule on `pB`** — the detector driven frame by frame through the
//!   pause, each request answered with the Python score: what the rule
//!   itself changes (frame rounding: 224 / 512 / 2016 / 3008 / 4000 ms);
//! - **Rust rule on Rust scores** — the same with Smart Turn run here on
//!   the turn's audio up to 200 ms into the pause (variant B): the product.
//!
//! The scores are not Python's, pause for pause (2026-10-01: mean |Δp|
//! 0.016, max 0.27). On the six that differ most the Rust features are
//! bit-identical to the Python extractor's, and ONNX Runtime 1.30 fed them
//! gives exactly `pB`: the difference is ONNX Runtime 1.28 (the `ort` pin)
//! running the int8 graph. The intra-op thread count changes nothing
//! (`LMGW_TURN_CORPUS_THREADS`, default the product's 4); a lower graph
//! optimization level makes it far worse (max 0.90). Results: §6.3.

use std::collections::HashMap;
use std::path::Path;

use lmgw_core::realtime::audio::pcm::parse_wav;
use lmgw_core::realtime::turn::semantic::SemanticParams;
use lmgw_core::realtime::turn::server_vad::{ServerVad, ServerVadParams, TurnEvent};
use lmgw_core::realtime::turn::smart_turn::{SmartTurn, INTRA_THREADS, SMART_TURN_ONNX};

const RATE: u64 = 16_000;
/// A Silero frame at 16 kHz (32 ms): the CSV's pauses are whole frames.
const FRAME: u64 = 512;
const SECTIONS: &str = "BCDFKLM";

/// One selected pause.
struct Pause {
    incomplete: bool,
    /// Its length in frames.
    frames: u64,
    /// The scores; NaN for a pause shorter than the 200 ms probe, which is
    /// never scored and never commits.
    py: f32,
    rust: f32,
}

/// RFC 4180 rows, header first (the transcripts in the CSV hold commas and
/// quotes).
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A time in seconds from the CSV, as whole frames' samples.
fn frames_at(seconds: &str) -> u64 {
    let s: f64 = seconds.parse().unwrap();
    (s * RATE as f64 / FRAME as f64).round() as u64 * FRAME
}

/// The selected pauses of one run directory, scored by Rust Smart Turn.
fn load_run(dir: &Path, st: &mut SmartTurn) -> Vec<Pause> {
    let text = std::fs::read_to_string(dir.join("pauses.csv")).unwrap();
    let mut rows = csv_rows(&text).into_iter();
    let head = rows.next().unwrap();
    let col: HashMap<&str, usize> = head
        .iter()
        .enumerate()
        .map(|(i, h)| (h.as_str(), i))
        .collect();
    let mut audio: HashMap<String, Vec<f32>> = HashMap::new();
    let mut turn_start: HashMap<String, u64> = HashMap::new();
    let mut out = Vec::new();
    for r in rows.filter(|r| r.len() == head.len()) {
        let get = |k: &str| r[col[k]].as_str();
        let clip = get("clip").to_string();
        let gs = frames_at(get("start_s"));
        let dur = frames_at(get("dur_s"));
        let kind = get("kind");
        let label = get("label");
        let sec = get("sec");
        // The Python scored each pause from the turn's start: the clip's
        // start, or 150 ms before the end of the last block-end silence.
        let start = *turn_start.get(&clip).unwrap_or(&0);
        if kind == "blockend" {
            turn_start.insert(clip.clone(), (gs + dur).saturating_sub(RATE * 150 / 1000));
        }
        let picked = !sec.is_empty()
            && SECTIONS.contains(sec)
            && label != "DROPPED"
            && kind != "lead"
            && (label == "INCOMPLETE"
                || ((kind == "blockend" || kind == "final") && dur >= RATE / 5));
        if !picked {
            continue;
        }
        let x = audio.entry(clip.clone()).or_insert_with(|| {
            let wav =
                parse_wav(&std::fs::read(dir.join("wav").join(format!("{clip}.wav"))).unwrap())
                    .unwrap();
            assert_eq!(wav.rate as u64, RATE, "{clip}");
            wav.samples
        });
        let end = ((gs + RATE / 5) as usize).min(x.len());
        let scored = dur >= RATE / 5;
        out.push(Pause {
            incomplete: label == "INCOMPLETE",
            frames: dur / FRAME,
            py: if scored {
                get("pB").parse().unwrap()
            } else {
                f32::NAN
            },
            rust: if scored {
                st.score(&x[start as usize..end]).unwrap()
            } else {
                f32::NAN
            },
        });
    }
    out
}

/// The rule of one eagerness: Rust knobs, and the Python report's floor
/// (`None` where it had none).
struct Eager {
    name: &'static str,
    rule: SemanticParams,
    py_floor: Option<f32>,
}

/// The settings' defaults (`config::SemanticVadTable`).
fn eagernesses() -> Vec<Eager> {
    let d = lmgw_core::config::SemanticVadTable::default();
    [
        ("high", d.high, Some(d.high.floor as f32)),
        ("medium", d.medium, Some(d.medium.floor as f32)),
        ("low", d.low, None),
    ]
    .into_iter()
    .map(|(name, r, py_floor)| Eager {
        name,
        rule: SemanticParams {
            threshold: r.threshold as f32,
            floor: r.floor as f32,
            floor_window_ms: 500,
            max_wait_ms: r.max_wait_ms,
        },
        py_floor,
    })
    .collect()
}

/// `report_scripted.py`'s `ctime`: seconds of silence at which the pause
/// commits (it scored at 0.2 s).
fn py_commit(e: &Eager, p: f32) -> f64 {
    if p >= e.rule.threshold {
        0.2
    } else if e.py_floor.is_some_and(|f| p >= f) {
        0.5
    } else {
        f64::from(e.rule.max_wait_ms) / 1000.0
    }
}

/// The detector through one pause of `frames` unvoiced frames (`None`: a
/// true end — silence until it commits), its request answered at once with
/// `p`: the silence at the commit in seconds, `None` when the voice came
/// back first.
fn rust_commit(e: &Eager, p: f32, frames: Option<u64>) -> Option<f64> {
    let mut sv = ServerVad::new(&ServerVadParams {
        sample_rate: RATE as u32,
        semantic: Some(e.rule.clone()),
        ..ServerVadParams::default()
    })
    .unwrap();
    let mut k = 0u64;
    let mut frame = |sv: &mut ServerVad, prob: f32| {
        let ev = sv.push_frame(prob, k * FRAME, (k + 1) * FRAME);
        k += 1;
        ev
    };
    for _ in 0..10 {
        frame(&mut sv, 0.9);
    }
    assert!(sv.in_speech());
    for n in 1..=frames.unwrap_or(u64::MAX) {
        let mut ev = frame(&mut sv, 0.1);
        if let Some(r) = sv.take_score() {
            ev = ev.or(sv.scored(r.id, Some(p)).1);
        }
        if let Some(TurnEvent::SpeechStopped { .. }) = ev {
            return Some((n * FRAME) as f64 / RATE as f64);
        }
        assert!(n < 100_000, "a true end that never commits");
    }
    None
}

/// numpy's default percentile (linear between closest ranks).
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let at = q / 100.0 * (sorted.len() - 1) as f64;
    let (lo, hi) = (at.floor() as usize, at.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (at - lo as f64)
}

/// "false / n (pct) | mean / p90 / max | true ends below the floor".
fn row(
    pauses: &[Pause],
    commit: impl Fn(&Pause, Option<u64>) -> Option<f64>,
    floor: f32,
    score: impl Fn(&Pause) -> f32,
) -> String {
    let inc: Vec<&Pause> = pauses.iter().filter(|p| p.incomplete).collect();
    let ends: Vec<&Pause> = pauses.iter().filter(|p| !p.incomplete).collect();
    let false_commits = inc
        .iter()
        .filter(|p| commit(p, Some(p.frames)).is_some())
        .count();
    let mut lat: Vec<f64> = ends.iter().map(|p| commit(p, None).unwrap()).collect();
    lat.sort_by(f64::total_cmp);
    let mean = lat.iter().sum::<f64>() / lat.len() as f64;
    let below = ends.iter().filter(|p| score(p) < floor).count();
    format!(
        "{false_commits}/{} ({:.0} %) | {mean:.2} / {:.2} / {:.1} | {below}/{}",
        inc.len(),
        100.0 * false_commits as f64 / inc.len() as f64,
        percentile(&lat, 90.0),
        lat.last().unwrap(),
        ends.len()
    )
}

#[test]
#[ignore = "local only: replays the owner's own recordings, read from LMGW_TURN_CORPUS"]
fn the_owner_s_pauses_through_the_rust_rule() {
    let Ok(dirs) = std::env::var("LMGW_TURN_CORPUS") else {
        eprintln!("LMGW_TURN_CORPUS is not set: nothing read, nothing replayed");
        return;
    };
    // The product's thread count unless `LMGW_TURN_CORPUS_THREADS` says
    // otherwise: ONNX Runtime's int8 kernels can land a step apart with
    // another count.
    let threads = std::env::var("LMGW_TURN_CORPUS_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(INTRA_THREADS);
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, threads).unwrap();
    let pauses: Vec<Pause> = dirs
        .split(':')
        .filter(|d| !d.is_empty())
        .flat_map(|d| load_run(Path::new(d), &mut st))
        .collect();
    let (inc, ends) = (
        pauses.iter().filter(|p| p.incomplete).count(),
        pauses.len() - pauses.iter().filter(|p| p.incomplete).count(),
    );
    eprintln!("{inc} INCOMPLETE pauses, {ends} true ends");
    let diffs: Vec<f32> = pauses
        .iter()
        .map(|p| (p.rust - p.py).abs())
        .filter(|d| !d.is_nan())
        .collect();
    eprintln!(
        "Rust vs Python Smart Turn: max |dp| {:.4}, mean |dp| {:.4}, {} of {} within 0.01",
        diffs.iter().copied().fold(0.0, f32::max),
        diffs.iter().sum::<f32>() / diffs.len() as f32,
        diffs.iter().filter(|d| **d <= 0.01).count(),
        diffs.len()
    );
    let mut worst: Vec<&Pause> = pauses.iter().filter(|p| !p.py.is_nan()).collect();
    worst.sort_by(|a, b| (b.rust - b.py).abs().total_cmp(&(a.rust - a.py).abs()));
    let worst: Vec<String> = worst
        .iter()
        .take(6)
        .map(|p| format!("{:.3}→{:.3}", p.py, p.rust))
        .collect();
    eprintln!(
        "largest differences (Python → Rust, {threads} threads): {}",
        worst.join(", ")
    );
    eprintln!();
    eprintln!("| eagerness | replay | false commits on INCOMPLETE | true-end latency mean / p90 / max (s) | true ends below the floor |");
    eprintln!("|---|---|---|---|---|");
    for e in eagernesses() {
        let floor = e.rule.floor;
        let py = |p: &Pause, frames: Option<u64>| {
            let t = py_commit(&e, p.py);
            frames.map_or(Some(t), |f| {
                (t <= (f * FRAME) as f64 / RATE as f64).then_some(t)
            })
        };
        let on_py = |p: &Pause, frames: Option<u64>| rust_commit(&e, p.py, frames);
        let on_rust = |p: &Pause, frames: Option<u64>| rust_commit(&e, p.rust, frames);
        let changed = pauses
            .iter()
            .filter(|p| !p.py.is_nan() && py_commit(&e, p.py) != py_commit(&e, p.rust))
            .count();
        eprintln!(
            "| {} | Python formula, Python scores | {} |",
            e.name,
            row(&pauses, py, floor, |p| p.py)
        );
        eprintln!(
            "| {} | Rust rule, Python scores | {} |",
            e.name,
            row(&pauses, on_py, floor, |p| p.py)
        );
        eprintln!(
            "| {} | Rust rule, Rust scores ({changed} of {} pauses change band) | {} |",
            e.name,
            diffs.len(),
            row(&pauses, on_rust, floor, |p| p.rust)
        );
    }
}

#[test]
fn the_csv_reader_keeps_quoted_commas() {
    let rows = csv_rows("a,b,c\n1,\"x, \"\"y\"\"\",3\n");
    assert_eq!(rows, [vec!["a", "b", "c"], vec!["1", "x, \"y\"", "3"]]);
}
