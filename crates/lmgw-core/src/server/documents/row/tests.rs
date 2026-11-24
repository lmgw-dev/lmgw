use super::*;
use crate::server::documents::send::Ended;
use lmgw_api_types::units::fmt_bytes;

/// Connection cut review, nit 3: an answer whose client did not read it
/// within its write deadline is a 499 row of the kind `write_deadline`,
/// saying how much was handed over and what the answer's status was.
#[tokio::test]
async fn an_answer_that_missed_its_write_deadline_is_a_499_write_deadline_row() {
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let row = Row::open(&state, &RequestCtx::default());
    let (sent, len) = (128 << 10, 4 << 20);
    (row.when_sent(200, None))(Ended::Late { sent, len }).await;
    let rows = crate::store::query_logs(
        &state.db,
        &crate::store::LogFilter {
            alias: Some(ALIAS.into()),
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, 499);
    assert_eq!(rows[0].error_kind.as_deref(), Some(WRITE_DEADLINE_KIND));
    let m = rows[0].error_msg.as_deref().unwrap_or("");
    assert!(
        m.contains(&format!(
            "{} of {} handed over, then its connection was ended (the answer was 200)",
            fmt_bytes(sent),
            fmt_bytes(len)
        )),
        "{m}"
    );
    assert!(
        m.contains("ANSWER_WRITE_GRACE_S, ANSWER_MIN_RATE_BYTES_PER_S"),
        "{m}"
    );
}
