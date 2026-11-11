//! The working budget of a partial start ([`super`]): the smallest
//! `--max-vram` a generation of the row has completed under,
//! `ImageModel::work_bytes` (migration 0104).
//!
//! **What is learned.** A generation that completes on an entry VRAM
//! admission held to a grant (`registry::admission_grant`) shows that the
//! budget rendered from it works for that row: what one segment of the
//! pipeline asks for fits in it. The row keeps the smallest such budget; a
//! larger one teaches nothing. Nothing is learned from an entry not held to
//! its grant — a `max_vram` in the row's own args, a build without the flag
//! — nor from a whole start, which sizes itself to the driver.
//!
//! **What a failure teaches** ([`failed`]). A generation that fails on an
//! entry held to a grant whose budget was at least the row's working one
//! shows that figure is not enough for every generation of the row — a
//! budget learned from a 512² picture is not known to be enough for a 1024²
//! one, and without this every larger job beside other models would go on
//! failing on it. Where sd.cpp names the budget it needed (its model
//! manager's "need … MB device / … MB budget", its graph-cut split's
//! "segment … needs … MB on …", read from the error and the container's log
//! tail, [`needed_budget`]), the figure is raised to that: a start then
//! needs it plus the context free, which [`super::decide`] sets aside first.
//! Where it names none, the figure is cleared, and the start rule for a row
//! with none learned applies until a generation completes again. A failure
//! under a budget below the working one teaches nothing new; one with no
//! working budget learned leaves it unlearned.
//!
//! **Not yet.** The figure is the row's, not per size: a raised figure holds
//! for the small pictures too.

use crate::hf::fmt_bytes;
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::broadcast;
use super::super::plan::image::learned_for;

/// A generation of image row `model_id` completed: where its entry is held
/// to a grant, the budget it ran with may be the row's smallest working one
/// (module doc). Never fails the request it follows.
pub async fn learned(state: &SharedState, model_id: &str) {
    let Some(entry) = state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.class == Class::Image && e.model_id == model_id)
    else {
        return;
    };
    let Some(grant) = entry.grant.as_ref() else {
        return;
    };
    if entry
        .warnings
        .iter()
        .any(|w| crate::runtime::registry::is_unheld_note(w))
    {
        return;
    }
    let snap = state.snapshot();
    let Some(row) = snap.image_models.iter().find(|m| m.model_id == model_id) else {
        return;
    };
    if super::own_budget(&row.args).is_some() {
        return;
    }
    // Learned under the engine the container runs (migration 0105).
    let Some(key) = entry.engine_key.as_deref() else {
        return;
    };
    let work = learned_for(row, key).work;
    let budget = grant.max_vram_bytes;
    if work.is_some_and(|w| w <= budget) {
        return;
    }
    let label = format!("image/{model_id}");
    if let Err(err) = crate::store::set_image_model_learned(
        &state.db,
        row.id,
        key,
        crate::store::LearnedFigure::Work,
        Some(budget),
    )
    .await
    {
        tracing::warn!("could not record the working budget of {label}: {err}");
        return;
    }
    if let Err(err) = state.reload_snapshot().await {
        tracing::warn!("working budget of {label} recorded but not reloaded: {err}");
        return;
    }
    tracing::info!(
        "{label}: a generation completed held to {} (--max-vram){} — VRAM admission now starts \
         it with that much budget beside lmgw's other models",
        fmt_bytes(budget),
        match work {
            Some(w) => format!(", less than the {} it had rendered with", fmt_bytes(w)),
            None => String::new(),
        }
    );
    broadcast(state);
}

/// How many lines of an image container's log a failed generation reads for
/// sd.cpp's budget line ([`failed`]): a reading window, not a cap on what
/// is learned — the line is printed right before the failure answers, and
/// where it is not in the window the figure is cleared instead.
pub const FAILURE_LOG_LINES: usize = 200;

/// A generation of image row `model_id` failed with `message` from the image
/// server: where its entry is held to a grant whose budget is at least the
/// row's working one, that figure is raised to the budget sd.cpp names, else
/// cleared (module doc). Never fails the request it follows.
pub async fn failed(state: &SharedState, model_id: &str, message: &str) {
    let Some(entry) = state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.class == Class::Image && e.model_id == model_id)
    else {
        return;
    };
    let Some(grant) = entry.grant.as_ref() else {
        return;
    };
    if entry
        .warnings
        .iter()
        .any(|w| crate::runtime::registry::is_unheld_note(w))
    {
        return;
    }
    let snap = state.snapshot();
    let Some(row) = snap.image_models.iter().find(|m| m.model_id == model_id) else {
        return;
    };
    if super::own_budget(&row.args).is_some() {
        return;
    }
    let Some(key) = entry.engine_key.as_deref() else {
        return;
    };
    let Some(work) = learned_for(row, key).work else {
        return;
    };
    let budget = grant.max_vram_bytes;
    let log = state
        .runtime()
        .logs_tail(&entry.container_name, FAILURE_LOG_LINES)
        .await
        .unwrap_or_default();
    let needed = needed_budget(message).or_else(|| needed_budget(&log));
    let label = format!("image/{model_id}");
    let next = match needed {
        // More than it was given: this failure's figure, and more than the
        // working one where it was given at least that.
        Some(n) if n > budget && n > work => Some(n),
        Some(n) if n > budget => return,
        // A budget at least the working one failed, and sd.cpp named
        // nothing more than it had: the working figure is not known.
        _ if budget >= work => None,
        _ => return,
    };
    if let Err(err) = crate::store::set_image_model_learned(
        &state.db,
        row.id,
        key,
        crate::store::LearnedFigure::Work,
        next,
    )
    .await
    {
        tracing::warn!("could not record the working budget of {label}: {err}");
        return;
    }
    if let Err(err) = state.reload_snapshot().await {
        tracing::warn!("working budget of {label} recorded but not reloaded: {err}");
        return;
    }
    match next {
        Some(n) => tracing::info!(
            "{label}: a generation failed held to {} (--max-vram), and sd.cpp named a budget of \
             {} — VRAM admission now starts it beside lmgw's other models only with that much \
             budget (it was {})",
            fmt_bytes(budget),
            fmt_bytes(n),
            fmt_bytes(work)
        ),
        None => tracing::info!(
            "{label}: a generation failed held to {} (--max-vram), at least the {} it had \
             rendered with, and sd.cpp named no budget — its working budget is not known again, \
             so VRAM admission starts it on less than its on-card part only with nothing else \
             of lmgw's on the card to wait for",
            fmt_bytes(budget),
            fmt_bytes(work)
        ),
    }
    broadcast(state);
}

/// The largest budget sd.cpp says one step needed, in bytes, from its log
/// lines in `text` (sd.cpp prints MB of 1024², `/ (1024.0 * 1024.0)`):
/// - its model manager: "model manager cannot make enough memory available
///   on Vulkan0: need 1453.98 MB device / 941.98 MB budget, available …" —
///   the budget figure, which is what `--max-vram` bounds;
/// - its graph-cut layer split: "… graph-cut layer split: segment 0 needs
///   942.0 MB on Vulkan0, but only 512.0 MB is available under current VRAM
///   limits".
///
/// The last such line wins: the log tail can hold an earlier failure's.
pub fn needed_budget(text: &str) -> Option<u64> {
    static PATTERNS: std::sync::OnceLock<[regex::Regex; 2]> = std::sync::OnceLock::new();
    let [manager, split] = PATTERNS.get_or_init(|| {
        [
            regex::Regex::new(
                r"need\s+[0-9.]+\s*MB device\s*/\s*([0-9]+(?:\.[0-9]+)?)\s*MB budget",
            )
            .expect("a valid pattern"),
            regex::Regex::new(r"segment\s+[0-9]+\s+needs\s+([0-9]+(?:\.[0-9]+)?)\s*MB on\b")
                .expect("a valid pattern"),
        ]
    });
    let mut last: Option<(usize, f64)> = None;
    for re in [manager, split] {
        for c in re.captures_iter(text) {
            let (Some(all), Some(mb)) = (c.get(0), c.get(1)) else {
                continue;
            };
            let Ok(mb) = mb.as_str().parse::<f64>() else {
                continue;
            };
            if last.is_none_or(|(at, _)| all.start() >= at) {
                last = Some((all.start(), mb));
            }
        }
    }
    let (_, mb) = last?;
    (mb.is_finite() && mb > 0.0).then(|| (mb * 1024.0 * 1024.0).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sd.cpp's own lines, worded as its format strings print them
    /// (`model_manager.cpp` and `layer_split_partition.cpp`,
    /// stable-diffusion.cpp 2f88688). The need figures were measured 2026-11-11 (Z-Image-Turbo at `--max-vram 0.5` on a GTX 970 test card,
    /// "need 1453.98 MB device / 941.98 MB budget", per-model containers
    /// §4); the "available" figures are the test's.
    const MANAGER: &str = "[WARN ] model_manager.cpp:1905 - model manager cannot make enough \
        memory available on Vulkan0: need 1453.98 MB device / 941.98 MB budget, available \
        3110.00 MB device / 512.00 MB budget";
    const SPLIT: &str = "[ERROR] layer_split_partition.cpp:166 - diffusion graph-cut layer \
        split: segment 0 needs 942.0 MB on Vulkan0, but only 512.0 MB is available under \
        current VRAM limits";

    #[test]
    fn sdcpp_names_the_budget_it_needed() {
        let mib: f64 = 1024.0 * 1024.0;
        assert_eq!(needed_budget(MANAGER), Some((941.98 * mib).ceil() as u64));
        assert_eq!(needed_budget(SPLIT), Some(942 * 1024 * 1024));
        assert_eq!(needed_budget("generation failed"), None);
        // The last line of a tail wins over an earlier failure's.
        let tail = format!("{SPLIT}\n[INFO ] sampling\n{MANAGER}\n");
        assert_eq!(needed_budget(&tail), Some((941.98 * mib).ceil() as u64));
        let tail = format!(
            "{}\n{SPLIT}",
            MANAGER.replace("941.98 MB budget", "2000.00 MB budget")
        );
        assert_eq!(needed_budget(&tail), Some(942 * 1024 * 1024));
    }
}
