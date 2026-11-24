//! Page orientation (document-conversion design §5.1, WP4): tesseract's
//! orientation and script detection, `--psm 0`, which needs
//! `osd.traineddata` (Fedora: `tesseract-osd`). WP0a measured tesseract
//! without it on pages turned 90° and 180°: 3-gram Jaccard 0.023 and 0.029,
//! noise. With it, a page whose detection says `Rotate: 90` is turned 90°
//! clockwise before it is read (checked 2026-11-18 on the corpus' turned
//! scans: each reads as the upright one after the turn).
//!
//! Detection is a run of its own on the page's slot (0.5 s on an A4 page at
//! 300 DPI, measured). A page it cannot judge (too few characters: "Too few
//! characters. Skipping this page", exit 1), a run that fails, or a weak
//! answer leaves the page as it is: its read then says through its
//! confidence whether it looks turned ([`super::OCR_LOW_CONFIDENCE`]).

use bytes::Bytes;

use super::super::pool::Slot;
use super::super::run::{self, Bounds};
use super::{TESSERACT_ENV, TESSERACT_MEMORY_LIMIT_BYTES, TESSERACT_TIMEOUT};

/// The language pack name tesseract's orientation data is listed under in
/// `--list-langs`.
pub const OSD_PACK: &str = "osd";

/// The Fedora package that installs it.
pub const OSD_PACKAGE: &str = "tesseract-osd";

/// The least orientation confidence a turn is made on: 1.0. tesseract
/// prints it beside its answer; the corpus' turned scans gave 2.4 to 2.8,
/// the upright one 2.7 (for "no turn"); below 1 its documentation calls the
/// answer unreliable.
pub const OSD_MIN_CONFIDENCE: f32 = 1.0;

/// How many degrees clockwise the page has to be turned, when tesseract
/// says so with enough confidence.
pub async fn detect(slot: &Slot, setup: &super::Setup, page: Bytes) -> Option<u32> {
    let bounds = Bounds {
        timeout: TESSERACT_TIMEOUT,
        output: 64 << 10,
        memory: TESSERACT_MEMORY_LIMIT_BYTES,
    };
    let out = run::tesseract_held(
        slot,
        &setup.tesseract,
        &["stdin", "stdout", "--psm", "0"],
        TESSERACT_ENV,
        setup.rules.as_deref(),
        page,
        bounds,
    )
    .await
    .ok()?;
    turn(&String::from_utf8_lossy(&out))
}

/// The turn tesseract's `--psm 0` answer asks for.
fn turn(answer: &str) -> Option<u32> {
    let field = |name: &str| {
        answer
            .lines()
            .find_map(|l| l.trim().strip_prefix(name))
            .map(str::trim)
    };
    let rotate: u32 = field("Rotate:")?.parse().ok()?;
    let confidence: f32 = field("Orientation confidence:")?.parse().ok()?;
    let sure = confidence.is_finite() && confidence >= OSD_MIN_CONFIDENCE;
    (sure && matches!(rotate, 90 | 180 | 270)).then_some(rotate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_read_and_weak_or_upright_ones_turn_nothing() {
        let a = "Page number: 0\nOrientation in degrees: 270\nRotate: 90\n\
                 Orientation confidence: 2.81\nScript: Latin\nScript confidence: 90.00\n";
        assert_eq!(turn(a), Some(90));
        assert_eq!(turn(&a.replace("2.81", "0.40")), None);
        assert_eq!(turn(&a.replace("Rotate: 90", "Rotate: 0")), None);
        assert_eq!(turn(&a.replace("Rotate: 90", "Rotate: 45")), None);
        assert_eq!(turn(&a.replace("2.81", "NaN")), None);
        assert_eq!(turn("Too few characters. Skipping this page"), None);
    }
}
