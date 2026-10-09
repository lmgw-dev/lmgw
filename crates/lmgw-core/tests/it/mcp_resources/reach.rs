//! A server's resources reach whoever reaches the server: a key's tool
//! scope, and a device-hosted label's L16 rule. Who is asked or woken for
//! it follows the same reach.

use serde_json::json;

use super::{connect_all, listed, rpc, server, weather};
use crate::device_chat::{bearer, op, pair};
use crate::mcp_host::{host_world, linked};
use crate::realtime_chat_thread::{world, World};
use crate::support::mcp_apps_stub::Apps;

/// A client key with tool scope `mode`/`patterns`: a client presenting it.
async fn key(w: &World, name: &str, mode: &str, patterns: &str) -> reqwest::Client {
    let mut body = json!({"name": name});
    if !mode.is_empty() {
        body["tool_scope_mode"] = json!(mode);
        body["tool_scope_patterns"] = json!(patterns);
    }
    let (s, v) = op(w, "key_create", body).await;
    assert_eq!(s, 200, "{v}");
    bearer(v["plaintext"].as_str().unwrap())
}

/// A key that reaches `wx` sees and reads its resources and not those of a
/// server it does not reach; a read of one says why.
#[tokio::test]
async fn resources_follow_the_key_s_tool_scope() {
    let w = world(|_| {}).await;
    let _wx = server(&w, "wx", "wx", weather()).await;
    let plain = Apps {
        tools: json!([{"name": "ping", "inputSchema": {"type": "object"}}]),
        resources: json!([{"uri": "ui://plain/a", "name": "a"}]),
        templates: json!([]),
        ..Apps::default()
    };
    let _p = server(&w, "plain", "", plain).await;
    connect_all(&w).await;

    let cases: Vec<(&str, reqwest::Client, Vec<&str>)> = vec![
        (
            "all",
            key(&w, "k-all", "", "").await,
            vec!["ui://plain/a", "ui://wx__weather/card"],
        ),
        (
            "allow wx__*",
            key(&w, "k-wx", "allow", "wx__*").await,
            vec!["ui://wx__weather/card"],
        ),
        (
            "allow wx__show",
            key(&w, "k-show", "allow", "wx__show").await,
            vec!["ui://wx__weather/card"],
        ),
        (
            "deny wx__*",
            key(&w, "k-deny", "deny", "wx__*").await,
            vec!["ui://plain/a"],
        ),
        (
            "allow ping",
            key(&w, "k-ping", "allow", "ping").await,
            vec!["ui://plain/a"],
        ),
    ];
    for (who, client, want) in &cases {
        let mut got = listed(&w, client).await;
        got.sort();
        assert_eq!(&got, want, "{who}");
    }
    let wx_only = &cases[1].1;
    let v = rpc(
        &w,
        wx_only,
        "resources/read",
        json!({"uri": "ui://plain/a"}),
    )
    .await;
    assert_eq!(v["error"]["code"], -32002, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside the tool scope"),
        "{v}"
    );
    let v = rpc(
        &w,
        wx_only,
        "resources/read",
        json!({"uri": "ui://wx__weather/card"}),
    )
    .await;
    assert!(v["result"]["contents"].is_array(), "{v}");
}

/// A device's resources reach as its tools do (L16): the owner, the device
/// itself and a key that names the label; never an `all` key or anonymous.
/// A read goes over the link in the device's own spelling, with `_meta`.
#[tokio::test]
async fn a_device_s_resources_reach_as_its_tools_do() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["see"]).await;
    dev.set_resources(vec![json!({"uri": "ui://panel/main", "name": "panel"})]);
    const URI: &str = "ui://desktop__panel/main";

    let all = key(&w, "k-all", "", "").await;
    let named = key(&w, "k-named", "allow", "desktop__*").await;
    let phone = pair(&w, "phone", json!({})).await;
    let anonymous = reqwest::Client::new();
    let owner = w.gw.client();
    let cases: Vec<(&str, &reqwest::Client, bool)> = vec![
        ("the owner", &owner, true),
        ("the hosting device", &d.client, true),
        ("a key naming the label", &named, true),
        ("an all key", &all, false),
        ("another device", &phone.client, false),
        ("anonymous", &anonymous, false),
    ];
    for (who, client, reaches) in cases {
        assert_eq!(
            listed(&w, client).await.contains(&URI.to_string()),
            reaches,
            "{who}"
        );
        let v = rpc(&w, client, "resources/read", json!({"uri": URI})).await;
        if reaches {
            assert_eq!(v["result"]["contents"][0]["uri"], URI, "{who}: {v}");
            assert_eq!(
                v["result"]["contents"][0]["text"], "page of ui://panel/main",
                "{who}: {v}"
            );
            let read = crate::common::patience::within("the device's read", dev.reads.recv())
                .await
                .unwrap();
            assert_eq!(read["params"]["uri"], "ui://panel/main", "{read}");
            assert!(read["params"]["_meta"]["lmgw/caller"].is_object(), "{read}");
        } else {
            assert_eq!(v["error"]["code"], -32002, "{who}: {v}");
            assert!(
                v["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("paired device's hosted label"),
                "{who}: {v}"
            );
        }
    }
    assert!(
        dev.reads.try_recv().is_err(),
        "a refused read reached the device"
    );
}

/// The id of server `name`'s row.
fn row_id(w: &World, name: &str) -> i64 {
    w.state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.name == name)
        .map(|s| s.id)
        .unwrap_or_else(|| panic!("no server {name}"))
}

/// A server reaped for idleness offers no tools to judge a narrowing scope
/// by: a read connects it first, then judges, as a call would.
#[tokio::test]
async fn a_narrowed_key_reads_a_reaped_server_s_resource() {
    let w = world(|_| {}).await;
    let wx = server(&w, "wx", "wx", weather()).await;
    connect_all(&w).await;
    let id = row_id(&w, "wx");
    w.state.mcp.stop_server(id).await;
    assert!(!w.state.mcp.is_ready(id).await);
    let show_only = key(&w, "k-show", "allow", "wx__show").await;
    let v = rpc(
        &w,
        &show_only,
        "resources/read",
        json!({"uri": "ui://wx__weather/card"}),
    )
    .await;
    assert_eq!(
        v["result"]["contents"][0]["uri"], "ui://wx__weather/card",
        "{v}"
    );
    assert_eq!(wx.reads(), [crate::mcp_resources::CARD]);
    assert!(w.state.mcp.is_ready(id).await);
}

/// A read by a caller that cannot reach a server is refused before the
/// server is woken: nothing connects it.
#[tokio::test]
async fn an_unreachable_caller_s_read_never_starts_a_lazy_server() {
    let w = world(|_| {}).await;
    let wx = server(&w, "wx", "wx", weather()).await;
    let denied = key(&w, "k-deny", "deny", "wx__*").await;
    let elsewhere = key(&w, "k-else", "allow", "other__*").await;
    for client in [&denied, &elsewhere] {
        let v = rpc(
            &w,
            client,
            "resources/read",
            json!({"uri": "ui://wx__weather/card"}),
        )
        .await;
        assert_eq!(v["error"]["code"], -32002, "{v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("outside the tool scope"),
            "{v}"
        );
    }
    assert!(
        wx.inits().is_empty(),
        "the server was started: {:?}",
        wx.inits()
    );
    assert!(!w.state.mcp.is_ready(row_id(&w, "wx")).await);
}

/// A listing asks the servers its caller reaches, and no device link the
/// caller does not reach: an `all` key's lists send nothing to a device.
#[tokio::test]
async fn a_listing_asks_no_device_the_caller_does_not_reach() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["see"]).await;
    dev.set_resources(vec![json!({"uri": "ui://panel/main", "name": "panel"})]);
    let _wx = server(&w, "wx", "wx", weather()).await;
    let plain = Apps {
        tools: json!([{"name": "ping", "inputSchema": {"type": "object"}}]),
        resources: json!([{"uri": "ui://plain/a", "name": "a"}]),
        templates: json!([]),
        ..Apps::default()
    };
    let _p = server(&w, "plain", "", plain).await;
    connect_all(&w).await;

    let all = key(&w, "k-all", "", "").await;
    let mut got = listed(&w, &all).await;
    got.sort();
    assert_eq!(got, ["ui://plain/a", "ui://wx__weather/card"]);
    let v = rpc(&w, &all, "resources/templates/list", json!({})).await;
    assert!(v["result"]["resourceTemplates"].is_array(), "{v}");
    let v = rpc(&w, &all, "resources/read", json!({"uri": "ui://plain/a"})).await;
    assert!(v["result"]["contents"].is_array(), "{v}");
    assert!(
        dev.lists.try_recv().is_err(),
        "a caller that does not reach the device had it asked"
    );

    // The owner reaches it: its listing does ask.
    assert!(listed(&w, &w.gw.client())
        .await
        .contains(&"ui://desktop__panel/main".to_string()));
    let asked = dev
        .lists
        .try_recv()
        .expect("the owner's listing asked the device");
    assert_eq!(asked["method"], "resources/list", "{asked}");
}
