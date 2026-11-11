//! Sign-in events (§5.8): the session routes are `Public`, so they write
//! their own entries. An unknown name is never written.

use super::*;
use crate::accounts::{account_with_password, browser, sign_in_from, trust_loopback_proxy};
use lmgw_core::state::AppState;

/// Entries with `action`, once `want` of them have landed (a failure's
/// entry is written after its answer), or what there is after five
/// seconds.
async fn landed(gw: &Gw, action: &str, want: usize) -> Vec<Value> {
    let mut got = Vec::new();
    for _ in 0..100 {
        got = with_action(gw, action).await;
        if got.len() >= want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    got
}

#[tokio::test]
async fn sign_ins_failures_throttles_and_sign_outs_are_written() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;

    // The link's page: the password set, then the sign-in it starts.
    let set = landed(&gw, "password set", 1).await;
    assert_eq!(set.len(), 1);
    assert_eq!(set[0]["account_id"], sam);
    assert_eq!(set[0]["target"]["with"], "set-password link");
    assert_eq!(set[0]["principal"], "sam, set-password link");

    let (status, _, session) = sign_in(&gw, "sam", PASSWORD).await;
    assert_eq!(status, 204);
    let ins = landed(&gw, "sign-in", 2).await;
    assert_eq!(ins.len(), 2, "{ins:?}");
    assert_eq!(
        ins[0]["target"],
        json!({ "account": sam, "login": "sam", "with": "password" })
    );
    assert_eq!(ins[0]["account_id"], sam);
    assert_eq!(ins[1]["target"]["with"], "set-password link");

    // A wrong password for a name an account has, then for one it has not.
    trust_loopback_proxy(&state).await;
    let wrong = "wrong wrong wrong";
    assert_eq!(sign_in_from(&gw, "192.0.2.10", "sam", wrong).await.0, 401);
    assert_eq!(
        sign_in_from(&gw, "192.0.2.11", "my-password-typed-here", wrong)
            .await
            .0,
        401
    );
    let failed = landed(&gw, "sign-in failed", 2).await;
    assert_eq!(failed.len(), 2, "{failed:?}");
    let by_sam = failed
        .iter()
        .find(|e| e["target"]["login"] == "sam")
        .unwrap();
    assert_eq!(by_sam["account_id"], Value::Null, "nobody acted as sam");
    assert_eq!(by_sam["outcome"], 401);
    assert_eq!(by_sam["peer"], "192.0.2.10");
    assert!(failed
        .iter()
        .any(|e| e["target"] == json!({ "with": "password" })));
    let log = super::whole_log(&state).await;
    assert!(!log.contains("my-password-typed-here"), "{log}");
    assert!(!log.contains("wrong wrong wrong"), "{log}");

    // Inside the first failure's delay.
    assert_eq!(sign_in_from(&gw, "192.0.2.10", "sam", wrong).await.0, 429);
    let held = landed(&gw, "sign-in throttled", 1).await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["outcome"], 429);

    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={}", session.unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let out = landed(&gw, "sign-out", 1).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["account_id"], sam);
    assert!(out[0]["principal"]
        .as_str()
        .unwrap()
        .starts_with("sam, session #"));
}
