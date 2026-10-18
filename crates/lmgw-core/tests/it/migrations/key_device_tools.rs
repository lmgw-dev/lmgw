//! Migration 0082: the device-tools grant (client-apps design L16 as changed
//! 2026-10-15). A client key or device whose `allow` patterns named a
//! hosted label (a pattern whose text before its first `*` starts with
//! `<label>__`) is granted that label, so it reaches what it reached, unless
//! another line could match the label's tools (`*`, `k*`, `*__search`): that
//! mixed list gets no grant rather than a wider reach. Every other key starts
//! with no grant, the hosting device included.

use super::db_at_version;

#[tokio::test]
async fn migration_0082_grants_the_labels_an_allow_list_named() {
    let pool = db_at_version(81).await;
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, kind, hosts_label, tool_scope_mode,
                               tool_scope_patterns)
         VALUES (50, 'device:desk', 'h50', 'device', 'desk', 'allow', 'desk__*'),
                (51, 'device:kai', 'h51', 'device', 'kai_b', 'all', ''),
                (60, 'device:phone', 'h60', 'device', NULL, 'allow',
                 'docs__*\nDESK__see\n  kai_b__x*  '),
                (61, 'ci', 'h61', 'key', NULL, 'allow', 'desk*\nd*\n*__x\nkai_bb__y'),
                (62, 'ci-deny', 'h62', 'key', NULL, 'deny', 'desk__*'),
                (63, 'ci-all', 'h63', 'key', NULL, 'all', 'desk__*'),
                (70, 'mixed-star', 'h70', 'key', NULL, 'allow', '*\nkai_b__notify'),
                (71, 'mixed-three', 'h71', 'key', NULL, 'allow',
                 'docs__*\nkai_b__echo\n*__search'),
                (72, 'mixed-k', 'h72', 'key', NULL, 'allow', 'k*\nkai_b__x'),
                (73, 'mixed-ka', 'h73', 'device', NULL, 'allow', 'KA*\nkai_b__x'),
                (74, 'naming-only', 'h74', 'key', NULL, 'allow',
                 'kai_b__notify\nd*\nkai_bb__*')",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let snap = lmgw_core::store::load_snapshot(&pool).await.unwrap();
    let grant = |id: i64| {
        snap.api_keys
            .iter()
            .find(|k| k.id == id)
            .unwrap()
            .policy
            .device_tools
            .clone()
    };
    assert_eq!(grant(60), ["desk", "kai_b"], "both named, in any case");
    assert!(
        grant(50).is_empty(),
        "the host reaches its own label anyway"
    );
    assert!(grant(51).is_empty());
    assert!(
        grant(61).is_empty(),
        "desk* and d* name no label, and kai_bb__ is not kai_b__"
    );
    assert!(
        grant(62).is_empty() && grant(63).is_empty(),
        "allow lists only"
    );
    for id in [70, 71, 72, 73] {
        assert!(
            grant(id).is_empty(),
            "a mixed list loses its reach rather than gaining the whole label (key {id})"
        );
    }
    assert_eq!(
        grant(74),
        ["kai_b"],
        "d* and kai_bb__* cannot match kai_b__… names"
    );
}
