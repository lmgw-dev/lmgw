//! `semantic_vad`'s settings (realtime design §6.3, §12): which detector
//! serves it, and the Smart Turn rule's knobs per eagerness.
//!
//! The defaults are measured, not OpenAI's 2 / 4 / 8 s waits: on the
//! owner's own recordings (161 incomplete pauses, 109 true ends, German and
//! English) high commits ~40 % of the incomplete pauses with a mean
//! true-end latency of ~0.4 s, medium ~30 % at ~0.7 s, low ~12 % at ~1.8 s
//! (§6.3 has the table, and the Rust replay of it). A threshold finer than
//! ~0.01 means nothing: the model's output is int8, and moves in steps of
//! about 0.01 near 0.5.

use serde::{Deserialize, Deserializer, Serialize};

/// What serves `turn_detection: {type: "semantic_vad"}`:
/// `realtime.semantic_vad_engine`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticVadEngine {
    /// Smart Turn decides when a pause ends the turn (§6.3).
    #[default]
    SmartTurn,
    /// The escape hatch: plain `server_vad` on the eagerness's
    /// `silence_duration_ms` — `semantic_vad` as it was before Smart Turn,
    /// logged as a substitution and echoed as
    /// `lmgw.resolved.turn_detection: "server_vad"`.
    ServerVad,
}

/// One eagerness's row of `realtime.semantic_vad`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SemanticVadRow {
    /// A Smart Turn score at or above this commits the turn 200 ms into the
    /// pause, `0.0..=1.0`.
    pub threshold: f64,
    /// A score at or above this, and below the threshold, commits at
    /// `realtime.semantic_floor_window_ms`; below it the turn waits for
    /// `max_wait_ms`. Equal to the threshold: no middle band.
    pub floor: f64,
    /// The longest a pause waits before it commits the turn, whatever the
    /// score.
    pub max_wait_ms: u32,
    /// The plain `server_vad` window of this eagerness (`semantic_vad`
    /// before Smart Turn): a pause Smart Turn could not score commits here,
    /// and the `server_vad` engine runs on it.
    pub silence_duration_ms: u32,
}

/// `realtime.semantic_vad`: the rows by eagerness. OpenAI's `auto` is
/// `medium`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SemanticVadTable {
    pub high: SemanticVadRow,
    pub medium: SemanticVadRow,
    pub low: SemanticVadRow,
}

impl Default for SemanticVadTable {
    fn default() -> Self {
        Self {
            // 40 % of the incomplete pauses committed, true ends after
            // 0.38 s on average (Python, 161 / 109 pauses).
            high: SemanticVadRow {
                threshold: 0.5,
                floor: 0.1,
                max_wait_ms: 2000,
                silence_duration_ms: 300,
            },
            // 30 %, 0.71 s.
            medium: SemanticVadRow {
                threshold: 0.5,
                floor: 0.2,
                max_wait_ms: 4000,
                silence_duration_ms: 500,
            },
            // ~12 %, ~1.8 s: confident completions only.
            low: SemanticVadRow {
                threshold: 0.95,
                floor: 0.95,
                max_wait_ms: 3000,
                silence_duration_ms: 800,
            },
        }
    }
}

impl SemanticVadRow {
    /// Why this row cannot run with the floor window `floor_window_ms`
    /// (`realtime.semantic_floor_window_ms`), or `None` (fix package B6):
    /// a threshold or floor outside `0.0..=1.0`, the floor above the
    /// threshold, or the floor window past the maximum wait. The detector
    /// refuses the same (`SemanticParams::validate`).
    pub fn problem(&self, floor_window_ms: u32) -> Option<String> {
        for (what, v) in [("threshold", self.threshold), ("floor", self.floor)] {
            if !(0.0..=1.0).contains(&v) {
                return Some(format!("its {what} {v} is not in 0.0..=1.0"));
            }
        }
        if self.floor > self.threshold {
            return Some(format!(
                "its floor {} is above its threshold {} (equal turns the floor off)",
                self.floor, self.threshold
            ));
        }
        if floor_window_ms > self.max_wait_ms {
            return Some(format!(
                "realtime.semantic_floor_window_ms {floor_window_ms} is above its max_wait_ms {}",
                self.max_wait_ms
            ));
        }
        None
    }
}

impl SemanticVadTable {
    /// The rows by eagerness name, in the order the settings list them.
    pub fn rows(&self) -> [(&'static str, &SemanticVadRow); 3] {
        [
            ("high", &self.high),
            ("medium", &self.medium),
            ("low", &self.low),
        ]
    }

    /// Every row that cannot run with `floor_window_ms`, as
    /// `realtime.semantic_vad.<eagerness>: <why>` (fix package B6): what a
    /// settings save refuses, and what a session warns about at its start
    /// before it runs the built-in row in that row's place.
    pub fn problems(&self, floor_window_ms: u32) -> Vec<String> {
        self.rows()
            .into_iter()
            .filter_map(|(name, row)| {
                row.problem(floor_window_ms)
                    .map(|why| format!("realtime.semantic_vad.{name}: {why}"))
            })
            .collect()
    }
}

/// A stored table may lack rows or fields (a hand edit, or a blob from
/// before a field existed): each missing value is **its own row's**
/// default, never another eagerness's — and never a parse error, which
/// would cost every other setting.
impl<'de> Deserialize<'de> for SemanticVadTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct RowIn {
            threshold: Option<f64>,
            floor: Option<f64>,
            max_wait_ms: Option<u32>,
            silence_duration_ms: Option<u32>,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct TableIn {
            high: RowIn,
            medium: RowIn,
            low: RowIn,
        }
        let t = TableIn::deserialize(d)?;
        let def = Self::default();
        let row = |r: RowIn, d: SemanticVadRow| SemanticVadRow {
            threshold: r.threshold.unwrap_or(d.threshold),
            floor: r.floor.unwrap_or(d.floor),
            max_wait_ms: r.max_wait_ms.unwrap_or(d.max_wait_ms),
            silence_duration_ms: r.silence_duration_ms.unwrap_or(d.silence_duration_ms),
        };
        Ok(Self {
            high: row(t.high, def.high),
            medium: row(t.medium, def.medium),
            low: row(t.low, def.low),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_table_takes_each_row_s_own_defaults() {
        let t: SemanticVadTable =
            serde_json::from_str(r#"{"high": {"threshold": 0.6}, "low": {}}"#).unwrap();
        let d = SemanticVadTable::default();
        assert_eq!(
            t.high,
            SemanticVadRow {
                threshold: 0.6,
                ..d.high
            }
        );
        assert_eq!((t.medium, t.low), (d.medium, d.low));
        let round: SemanticVadTable =
            serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(round, d);
        assert_eq!(
            serde_json::to_value(SemanticVadEngine::ServerVad).unwrap(),
            "server_vad"
        );
    }

    #[test]
    fn a_row_that_cannot_run_says_why() {
        let d = SemanticVadTable::default();
        assert!(d.problems(500).is_empty(), "the defaults run");
        let mut t = d.clone();
        t.high.floor = 0.6;
        t.low.threshold = 1.5;
        assert_eq!(
            t.problems(500),
            [
                "realtime.semantic_vad.high: its floor 0.6 is above its threshold 0.5 (equal \
                 turns the floor off)",
                "realtime.semantic_vad.low: its threshold 1.5 is not in 0.0..=1.0",
            ]
        );
        assert_eq!(
            d.problems(2500),
            [
                "realtime.semantic_vad.high: realtime.semantic_floor_window_ms 2500 is above its \
              max_wait_ms 2000"
            ]
        );
        let nan = SemanticVadRow {
            floor: f64::NAN,
            ..d.medium
        };
        assert!(nan.problem(500).is_some());
    }
}
