//! A file's poppler runs run again under the wide limit (`pdf::wide`), as
//! the conversion says it: a `memory_limit_widened` warning naming the run
//! and both limits, under limits of the test's own (text 96 MiB, wide
//! 256 MiB; a path of 200 000 lines needs about 120 MiB, measured
//! 2026-11-20), and a layout that needs more than the wide one fails with
//! `memory_limit`, naming both.

use super::*;
use crate::extract::pdf::limit::with_limits;

const MIB: u64 = 1 << 20;
const LIMITS: [u64; 3] = [96 * MIB, 2 << 30, 256 * MIB];

#[tokio::test]
async fn a_layout_read_under_the_wide_limit_is_a_warning_and_past_it_a_named_failure() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let plan = plan_of(ConvertOptions::default());
    let pdf = test_files::pdf_one_path(200_000);
    let d = with_limits(LIMITS, convert(input(pdf), &plan, &facts))
        .await
        .doc;
    assert_ne!(d.status, DocStatus::Failed, "{d:?}");
    let w = d
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::MemoryLimitWidened)
        .unwrap_or_else(|| panic!("no memory_limit_widened: {:?}", d.warnings));
    assert_eq!(w.tool.as_deref(), Some("poppler"));
    assert!(
        w.message
            .contains("POPPLER_TEXT_MEMORY_LIMIT_BYTES (96.0 MiB)")
            && w.message
                .contains("POPPLER_WIDE_MEMORY_LIMIT_BYTES (256 MiB)")
            && w.message.contains("for the file"),
        "{}",
        w.message
    );
    let pdf = test_files::pdf_one_path(1_000_000);
    let d = with_limits(LIMITS, convert(input(pdf), &plan, &facts))
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Failed, "{d:?}");
    let e = d.error.unwrap();
    assert_eq!(e.code, ErrorCode::MemoryLimit, "{}", e.message);
    assert!(
        e.message
            .contains("POPPLER_WIDE_MEMORY_LIMIT_BYTES (256 MiB)")
            && e.message
                .contains("after POPPLER_TEXT_MEMORY_LIMIT_BYTES (96.0 MiB)"),
        "{}",
        e.message
    );
}
