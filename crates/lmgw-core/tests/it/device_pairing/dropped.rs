//! A client that hangs up on its enrolment (device link design §3.4,
//! *2026-10-23*): the enrolment is not cancelled with it. A client
//! app's pairing client raced one request across the link's URLs and closed
//! the connection that answered last; hyper dropped that handler's future,
//! and the enrolment's tail after its commit never ran. The owner's device
//! ops are the same (*2026-10-23*): `device_confirm`, `key_delete` and
//! `key_rotate` are tested below, and so is an upstream update, whose tail
//! flushes the catalog cache (the other upstream and alias writes, and the
//! client `key_create`, run as such a task too; their tails have nothing
//! to observe).

use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use super::confirm::{threads, Since};
use super::world;
use crate::support::device_pair;
use crate::support::dropped_op::{op_unanswered, slow_mcp_server};

/// `body` written to the device listener over TLS pinned to `link`'s `fp`
/// as an HTTP/1.1 request, and the open connection handed back, the answer
/// unread: the caller hangs up by dropping it.
async fn post_unanswered(
    link: &str,
    body: String,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let parsed = lmgw_client::pairing::PairingLink::parse(link).unwrap();
    let cfg =
        lmgw_client::tls::enrol_config(lmgw_core::devices::tls::provider(), parsed.fp).unwrap();
    let url = reqwest::Url::parse(&parsed.urls.as_slice()[0]).unwrap();
    let host = url.host_str().unwrap().to_string();
    let tcp = tokio::net::TcpStream::connect((host.as_str(), url.port().unwrap()))
        .await
        .unwrap();
    let name = rustls::pki_types::ServerName::try_from(host.clone()).unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(name, tcp)
        .await
        .unwrap();
    let request = format!(
        "POST {} HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{body}",
        lmgw_api_types::device_link::ENROL_PATH,
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();
    tls
}

/// The token is used and its answer noted, though the client never read
/// the answer: the same key asks again and gets its certificate, and once
/// the owner confirmed it, the device's first request with that certificate
/// ends the token, which only an answer the enrolment noted lets it do.
///
/// Nothing here waits for a guessed time. The request is on its way once the
/// row shows its token used (this request's commit, not a later one's), the
/// reload that follows hangs on the MCP server until the test lets it go,
/// and the hang-up is known to have reached the server when the device
/// listener has no connection left. The tail's last step, the `enrolled`
/// frame, shows that it ran to its end.
#[tokio::test]
async fn an_enrolment_the_client_hung_up_on_still_completes() {
    let w = world().await;
    let (id, link) = w.waiting("phone").await;
    let events = std::sync::Mutex::new(w.state.telemetry.subscribe());
    let (release, reached) = slow_mcp_server(&w.state).await;
    let (spki, private) = device_pair::key();
    let tls = post_unanswered(&link, super::enrolment(&super::token_of(&link), &spki)).await;
    crate::common::patience::until_async("the request committed its enrolment", || async {
        w.row("phone").await["pairing"]["token"]["used_at"].is_string()
    })
    .await;
    crate::common::patience::until_async("the reload reached the MCP server", || async {
        reached.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    drop(tls);
    crate::common::patience::until_async("the server saw the hang-up", || async {
        lmgw_core::devices::status(&w.state).listener.connections == 0
    })
    .await;
    release.notify_one();

    // The tail ends with the `enrolled` frame, which comes after the reload
    // and so only from a tail that was not cancelled with the handler.
    let said = std::sync::Mutex::new(Vec::new());
    crate::common::patience::until_async("the enrolment's tail announced it", || async {
        let mut said = said.lock().unwrap();
        said.extend(device_pair::keys_said(&mut events.lock().unwrap(), id));
        said.contains(&"enrolled".to_string())
    })
    .await;

    // The token is used meanwhile: the same key gets its answer back.
    let answer = std::sync::Mutex::new(None);
    crate::common::patience::until_async("the hung-up enrolment used the token", || async {
        let (status, body) = device_pair::post_enrol(&link, &spki).await;
        let ok = status == 201;
        *answer.lock().unwrap() = ok.then_some(body);
        ok
    })
    .await;
    let answer = answer.into_inner().unwrap();
    let parsed = lmgw_client::pairing::PairingLink::parse(&link).unwrap();
    let credentials =
        lmgw_client::requests::read_enrolment(201, &answer.unwrap(), &parsed, &spki).unwrap();
    let enrolled = device_pair::Enrolled {
        link: parsed,
        identity: (
            rustls::pki_types::CertificateDer::from(credentials.certificate.clone()),
            private,
        ),
        key_hash: lmgw_core::devices::tls::key_hash(&spki),
        credentials,
        spki,
    };
    device_pair::confirm(&w.gw, &enrolled).await;
    let phone = device_pair::connected(enrolled, "device:phone".into()).await;

    // The tail may still run when the answer is out, and a request that
    // comes before it finds no noted answer, so the device asks until the
    // token ends.
    crate::common::patience::until_async("the enrolment noted its answer", || async {
        let resp = phone
            .client
            .get(format!("{}/chat/api/threads", w.gw))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        lmgw_core::devices::status(&w.state).pairing.tokens_alive == 0
    })
    .await;
}

/// The owner's `device_confirm`: [`op_unanswered`].
async fn confirm_unanswered(gw: &crate::common::Gw, id: i64, code: &str) -> tokio::net::TcpStream {
    op_unanswered(
        gw,
        "device_confirm",
        serde_json::json!({ "id": id, "confirm_code": code }),
    )
    .await
}

/// A confirmation the owner's client hung up on still replaces the kept
/// key (*2026-10-23*): the snapshot stops serving it, what it held open
/// closes, and the `confirmed` frame is said, all of which come after the
/// reload.
///
/// The commit is known by the row showing its new key confirmed; the reload
/// that follows hangs on the MCP server until the test lets it go. The
/// plain listener has no connection count to wait on, so the hang-up gets a
/// short settle before the release; the assertions then wait for the tail.
#[tokio::test]
async fn a_confirmation_the_owner_hung_up_on_still_completes() {
    let w = world().await;
    let old = device_pair::pair(&w.state, &w.gw, "phone", serde_json::json!({})).await;
    let link = device_pair::mint(
        &w.state,
        &w.gw,
        "device_token",
        serde_json::json!({ "id": old.id }),
    )
    .await;
    let new = device_pair::connected(device_pair::enrol(&link).await, "device:phone".into()).await;
    assert_eq!(threads(&w, &old.client).await, 200, "the kept key works");
    let mut since = Since::now(&w.state, &old);
    let events = std::sync::Mutex::new(w.state.telemetry.subscribe());
    let (release, reached) = slow_mcp_server(&w.state).await;

    let tcp = confirm_unanswered(&w.gw, old.id, &new.credentials.confirm_code).await;
    crate::common::patience::until_async("the request committed its confirmation", || async {
        let row = w.row("phone").await;
        row["pairing"]["state"] == serde_json::json!("confirmed")
            && row["pairing"]["confirm_code"] == serde_json::json!(new.credentials.confirm_code)
    })
    .await;
    crate::common::patience::until_async("the reload reached the MCP server", || async {
        reached.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    drop(tcp);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    release.notify_one();

    let said = std::sync::Mutex::new(Vec::new());
    crate::common::patience::until_async("the confirmation's tail announced it", || async {
        let mut said = said.lock().unwrap();
        said.extend(device_pair::keys_said(&mut events.lock().unwrap(), old.id));
        said.contains(&"confirmed".to_string())
    })
    .await;
    assert_eq!(
        since.ended(&w.state),
        Some(lmgw_core::devices::RevokeReason::Rotated),
        "the kept key's connections were closed"
    );
    assert_eq!(threads(&w, &old.client).await, 401, "the kept key is ended");
    assert_eq!(threads(&w, &new.client).await, 200, "the new key is served");
}

/// A deleted device the owner hung up on still ends (*2026-10-23*): what it
/// held open closes and the `deleted` frame is said, which come after the
/// reload (the snapshot itself is published before it).
///
/// The delete is known to be written when the reload reaches the MCP server
/// (the delete precedes it); the test lets the reload go after the hang-up.
#[tokio::test]
async fn a_device_delete_the_owner_hung_up_on_still_completes() {
    let w = world().await;
    let phone = device_pair::pair(&w.state, &w.gw, "phone", serde_json::json!({})).await;
    assert_eq!(threads(&w, &phone.client).await, 200, "the device works");
    let events = std::sync::Mutex::new(w.state.telemetry.subscribe());
    let (release, reached) = slow_mcp_server(&w.state).await;

    let tcp = op_unanswered(&w.gw, "key_delete", serde_json::json!({ "id": phone.id })).await;
    crate::common::patience::until_async("the reload reached the MCP server", || async {
        reached.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    drop(tcp);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    release.notify_one();

    let said = std::sync::Mutex::new(Vec::new());
    crate::common::patience::until_async("the delete's tail announced it", || async {
        let mut said = said.lock().unwrap();
        said.extend(device_pair::keys_said(
            &mut events.lock().unwrap(),
            phone.id,
        ));
        said.contains(&"deleted".to_string())
    })
    .await;
    assert_eq!(threads(&w, &phone.client).await, 401, "the device is gone");
}

/// `GET /api/usage/keys` with `key`: the status.
async fn keys_status(w: &super::World, key: &str) -> u16 {
    w.gw.client()
        .get(format!("{}/api/usage/keys", w.gw))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A rotation of an owner key the owner hung up on still ends the old
/// value (*2026-10-23*): what it held open closes and the `rotated` frame is
/// said, which come after the reload.
#[tokio::test]
async fn a_key_rotation_the_owner_hung_up_on_still_completes() {
    let w = world().await;
    let (s, v) = w
        .op(
            "key_create",
            serde_json::json!({ "name": "laptop", "kind": "owner" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (id, old) = (
        v["id"].as_i64().unwrap(),
        v["key"].as_str().unwrap().to_string(),
    );
    assert_eq!(keys_status(&w, &old).await, 200, "the old value works");
    let events = std::sync::Mutex::new(w.state.telemetry.subscribe());
    let (release, reached) = slow_mcp_server(&w.state).await;

    let tcp = op_unanswered(&w.gw, "key_rotate", serde_json::json!({ "id": id })).await;
    crate::common::patience::until_async("the reload reached the MCP server", || async {
        reached.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    drop(tcp);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    release.notify_one();

    // The snapshot is published before the reload's reconcile, so the old
    // value is refused already; the revocation comes after it.
    let said = std::sync::Mutex::new(Vec::new());
    crate::common::patience::until_async("the rotation's tail announced it", || async {
        let mut said = said.lock().unwrap();
        said.extend(device_pair::keys_said(&mut events.lock().unwrap(), id));
        said.contains(&"rotated".to_string())
    })
    .await;
    assert_eq!(keys_status(&w, &old).await, 401, "the old value is ended");
}

/// An `/v1/models` list with the one model `id`.
async fn catalog_of(id: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "object": "list", "data": [{ "id": id, "object": "model" }] }),
        ))
        .mount(&mock)
        .await;
    mock
}

/// An upstream update the owner hung up on still flushes the upstream's
/// cached catalog (*2026-10-23*): `/v1/models` lists the new base URL's models,
/// not the five minutes' old cache, though the flush comes after the reload.
/// (A created upstream's flush has nothing to observe: its id has no entry.)
#[tokio::test]
async fn an_upstream_update_the_owner_hung_up_on_still_flushes_its_catalog() {
    let w = world().await;
    let (old, new) = (catalog_of("alpha").await, catalog_of("beta").await);
    let id = lmgw_core::store::insert_upstream(
        &w.state.db,
        &lmgw_core::store::NewUpstream {
            name: "up".into(),
            protocol: lmgw_core::config::Protocol::OpenaiCompatible,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", old.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "up".into(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let listed = || async {
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .bearer_auth(&w.gw.key)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    assert!(
        listed().await.contains("alpha"),
        "the old catalog is cached"
    );
    let (release, reached) = slow_mcp_server(&w.state).await;

    let tcp = op_unanswered(
        &w.gw,
        "upstream_set_full",
        serde_json::json!({
            "action": "update", "id": id, "base_url": format!("{}/v1", new.uri())
        }),
    )
    .await;
    crate::common::patience::until_async("the reload reached the MCP server", || async {
        reached.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    drop(tcp);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    release.notify_one();

    crate::common::patience::until_async("the update's tail flushed the catalog", || async {
        listed().await.contains("beta")
    })
    .await;
}
