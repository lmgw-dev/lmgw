//! A local model's deferrals over the last 24 h (per-request background
//! design §10), where candidate aliases already show theirs: the Models
//! page's answer and `lmgw__models`.

use lmgw_api_types::self_admin::ModelEntry;
use serde_json::Value;

use super::*;
use crate::support::gpu_world::GIB;

/// Two deferred background requests for `m` count on its row — and nothing
/// on the row of the owner's model that was in the way.
#[tokio::test]
async fn a_local_models_deferrals_are_counted_on_its_row() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("x", 16 * GIB).await;
    g.model("m", 12 * GIB).await;
    owner_loads(&g, "x").await;
    deferred(&g, guest_chat(&gw, "m").await, "m", "model").await;
    let resp = guest_chat(&gw, "m").await;
    assert_eq!(resp.status(), 503);
    // The second row is written as its response goes out.
    until_logged(&g, "m", 2).await;

    let full: Value = gw
        .client()
        .get(format!("{gw}/api/models/full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let count = |name: &str| {
        full["local"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["public_name"] == name)
            .map(|v| v["deferrals_24h"].clone())
    };
    assert_eq!(count("m"), Some(Value::from(2)), "{full}");
    assert_eq!(count("x"), Some(Value::from(0)), "{full}");

    let listed = lmgw_core::ops::models(&g.state, Some("local"), None)
        .await
        .unwrap();
    let local = |name: &str| {
        listed.models.iter().find_map(|e| match e {
            ModelEntry::Local(l) if l.name == name => Some(l.deferrals_24h),
            _ => None,
        })
    };
    assert_eq!(local("m"), Some(2));
    assert_eq!(local("x"), Some(0));
}

/// Wait until `alias` has `n` request-log rows.
async fn until_logged(g: &Gpu, alias: &str, n: usize) {
    for _ in 0..200 {
        let rows = store::query_logs(
            &g.state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if rows.len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("'{alias}' never had {n} request-log rows");
}

/// A deferral count that cannot be read is zero, with a warning in the log,
/// in `lmgw__models` as on the Models page — never a failed list over one
/// column.
#[tokio::test]
async fn an_unreadable_deferral_count_is_zero_not_a_failed_list() {
    let (g, gw) = world(24 * GIB, 1).await;
    g.model("m", 8 * GIB).await;
    sqlx::query("ALTER TABLE request_logs RENAME TO request_logs_away")
        .execute(&g.state.db)
        .await
        .unwrap();

    let listed = lmgw_core::ops::models(&g.state, Some("local"), None)
        .await
        .expect("the list, without its deferral counts");
    let deferrals = listed.models.iter().find_map(|e| match e {
        ModelEntry::Local(l) if l.name == "m" => Some(l.deferrals_24h),
        _ => None,
    });
    assert_eq!(deferrals, Some(0));

    let resp = gw
        .client()
        .get(format!("{gw}/api/models/full"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let full: Value = resp.json().await.unwrap();
    assert_eq!(full["local"][0]["deferrals_24h"], 0, "{full}");
}
