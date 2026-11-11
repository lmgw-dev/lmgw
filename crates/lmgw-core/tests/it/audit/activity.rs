//! The Activity list (ruling 18's follow-up, §5.8): everyone's entries, one
//! account's or those of no account, each action in words; and the ways in
//! the list names (the Chat, a trusted proxy's client, the desktop's tray, a
//! refused tool call).

use super::*;
use crate::accounts::{account_with_password, sign_in_from, trust_loopback_proxy};
use lmgw_core::agent::ToolExecutor;
use lmgw_core::config::{ApiKeyKind, SelfAdmin};
use lmgw_core::mcp::exec::SelfAdminExecutor;
use lmgw_core::principal::Principal;
use lmgw_core::proxy::RequestCtx;
use lmgw_core::state::AppState;

/// A page of `account` (`audit_list`'s filter), newest first.
async fn of(gw: &Gw, account: Value) -> Vec<Value> {
    let (status, page) = op(
        gw,
        "audit_list",
        json!({ "account": account, "count": 500 }),
    )
    .await;
    assert_eq!(status, 200, "{page}");
    page["entries"].as_array().unwrap().clone()
}

/// The entries `action` has once `want` landed (a sign-in route's entry is
/// written after its answer).
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
async fn the_list_filters_by_account_and_names_each_action() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let owner = turn_on(&gw).await;
    let (sam, sams) = account_with_password(&gw, "sam").await;
    assert_eq!(
        op_as(&gw, &sams, "key_create", json!({ "name": "sams-key" }))
            .await
            .0,
        200
    );
    assert_eq!(
        op_as(&gw, &owner, "key_create", json!({ "name": "owners-key" }))
            .await
            .0,
        200
    );
    trust_loopback_proxy(&state).await;
    assert_eq!(
        sign_in_from(&gw, "192.0.2.30", "nobody-here", "wrong wrong wrong")
            .await
            .0,
        401
    );
    landed(&gw, "sign-in failed", 1).await;
    landed(&gw, "sign-in", 1).await;
    landed(&gw, "password set", 1).await;

    let sams_entries = of(&gw, json!(sam)).await;
    assert!(!sams_entries.is_empty());
    assert!(
        sams_entries.iter().all(|e| e["account_id"] == sam),
        "{sams_entries:?}"
    );
    assert!(sams_entries
        .iter()
        .any(|e| e["target"]["name"] == "sams-key"));
    let ones = of(&gw, json!(1)).await;
    assert!(ones.iter().all(|e| e["account_id"] == 1));
    assert!(ones.iter().any(|e| e["target"]["name"] == "owners-key"));
    let nobodys = of(&gw, json!("none")).await;
    assert!(!nobodys.is_empty());
    assert!(
        nobodys.iter().all(|e| e["account_id"].is_null()),
        "{nobodys:?}"
    );
    assert_eq!(nobodys[0]["action"], "sign-in failed");
    let everyone = of(&gw, json!("everyone")).await;
    assert_eq!(
        everyone.len(),
        sams_entries.len() + ones.len() + nobodys.len(),
        "everyone is each account's and nobody's together"
    );
    assert_eq!(
        op(&gw, "audit_list", json!({ "account": "somebody" }))
            .await
            .0,
        400
    );

    // Each action in words, never its stored name.
    for e in &everyone {
        let (action, label) = (
            e["action"].as_str().unwrap(),
            e["action_label"].as_str().unwrap(),
        );
        assert!(
            !label.is_empty() && label != action,
            "{action} has no words: {e}"
        );
    }
    // A sign-in names the session it started, as its later entries do.
    let sign_in = landed(&gw, "sign-in", 1).await;
    let p = sign_in[0]["principal"].as_str().unwrap();
    assert!(p.starts_with("sam, session #"), "{p}");
    let later = sams_entries
        .iter()
        .find(|e| e["action"] == "key_create")
        .unwrap();
    assert_eq!(later["principal"], p, "the key was made in that session");
}

#[test]
fn every_written_route_and_op_has_words() {
    use lmgw_core::audit::at_gate::{audits, op_audited, OP_ROUTE};
    use lmgw_core::audit::labels::action_label;
    for (method, path, cap) in lmgw_core::server::CAPABILITY_TABLE {
        let Ok(m) = axum::http::Method::from_bytes(method.as_bytes()) else {
            continue;
        };
        if *cap != lmgw_core::principal::Cap::Admin || *path == OP_ROUTE || !audits(&m, path, path)
        {
            continue;
        }
        let action = format!("{method} {path}");
        assert_ne!(action_label(&action), action, "{action} has no summary");
    }
    for name in lmgw_core::web::op_names::all().filter(|n| op_audited(n)) {
        assert_ne!(action_label(name), name, "op {name} has no summary");
    }
}

#[tokio::test]
async fn a_chat_tool_call_a_proxied_client_and_the_tray_are_named_by_their_way_in() {
    let state = AppState::init_for_tests().await.unwrap();
    with_settings(&state, |s| s.self_admin = SelfAdmin::Full).await;
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;

    // The Chat's executor, as the dashboard's Chat runs a model's call.
    let key = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "owner:dashboard")
        .cloned()
        .unwrap();
    let ctx = RequestCtx {
        principal: Principal::Key {
            id: key.id,
            name: key.name.clone(),
            kind: ApiKeyKind::Owner,
            agent_id: None,
            fingerprint: key.fingerprint(),
            account: Some(1),
            session: None,
        },
        ..Default::default()
    };
    let out = SelfAdminExecutor::new(state.clone(), ctx)
        .call("lmgw__hold_set", &json!({ "active": false }))
        .await;
    assert!(!out.is_error);
    let e = entries(&gw, 1).await;
    assert_eq!(e[0]["action"], "lmgw__hold_set");
    assert_eq!(e[0]["via"], "chat");
    assert_eq!(e[0]["account_id"], 1);
    assert_eq!(e[0]["principal"], "owner, key owner:dashboard");

    // Behind a trusted proxy, the client it forwards.
    trust_loopback_proxy(&state).await;
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/key_create"))
        .header("x-forwarded-for", "192.0.2.77")
        .header("x-forwarded-proto", "https")
        .json(&json!({ "name": "spike-wp7-proxied" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let e = entries(&gw, 1).await;
    assert_eq!(e[0]["action"], "key_create");
    assert_eq!(e[0]["peer"], "192.0.2.77");

    // The desktop's tray: account 1 at the machine.
    let target = lmgw_core::audit::target("active", true);
    lmgw_core::audit::desktop(&state, lmgw_core::audit::Via::Tray, "hold_set", target, 200).await;
    let e = entries(&gw, 1).await;
    assert_eq!(e[0]["via"], "tray");
    assert_eq!(e[0]["principal"], "owner, tray");
    assert_eq!(e[0]["peer"], "local");
    assert_eq!(e[0]["account_id"], 1);
}

#[tokio::test]
async fn a_writing_tool_call_the_mode_refuses_is_an_entry() {
    let state = AppState::init_for_tests().await.unwrap();
    with_settings(&state, |s| s.self_admin = SelfAdmin::ReadOnly).await;
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    let before = count(&state).await;
    assert!(mcp_call(&gw, "lmgw__hold_set", json!({ "active": true })).await);
    assert_eq!(count(&state).await, before + 1);
    let e = entries(&gw, 1).await;
    assert_eq!(e[0]["action"], "lmgw__hold_set");
    assert_eq!(e[0]["outcome"], 403);
    assert_eq!(e[0]["via"], "mcp");
}

#[tokio::test]
async fn a_prompt_reads_as_its_length() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;
    let prompt = "You are spike-wp7, a pirate.";
    let (status, body) = op_as(
        &gw,
        &session,
        "settings_set_full",
        json!({ "chat_system_prompt": prompt }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let e = entries(&gw, 1).await;
    assert_eq!(
        e[0]["target"]["fields"],
        json!({ "chat_system_prompt": format!("changed ({} characters)", prompt.chars().count()) })
    );
    assert!(!super::whole_log(&state).await.contains("pirate"));
}
