//! Devices paired for real (device link design §3): the owner's `key_create`
//! mints the link, the device enrols its own P-256 key over TLS pinned to
//! the link's `fp` (`lmgw-client`'s enrolment verifier and reader), and the
//! owner confirms its code. From then on the device speaks to the device
//! listener with its certificate (`lmgw-client`'s steady-state
//! configuration), as every device does since bearer device keys ended (D12).
//!
//! A suite drives a device the way it drives the owner, against the plain
//! listener's base URL: the device's reqwest client sends every request to
//! a proxy of its own, which forwards it, upgrades included, over a fresh
//! mutual-TLS connection to the device listener. The device listener's face,
//! its gate and the certificate's row decide, as for a real device; a
//! WebSocket the suite opens at [`Paired::addr`] reaches it the same way.

#![allow(dead_code)]

use std::net::Ipv4Addr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use lmgw_client::pairing::PairingLink;
use lmgw_client::requests::Credentials;
use lmgw_core::devices::tls;
use lmgw_core::state::SharedState;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::ClientConfig;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};

use super::device_link::Identity;
use crate::common::Gw;

/// A device paired and confirmed: its row, the client and the address that
/// reach the device listener as it, and what it keeps of its enrolment.
pub struct Paired {
    pub id: i64,
    /// `device:<name>`.
    pub name: String,
    /// Every `http://` request it sends goes to the device listener with
    /// the device's certificate, whatever its URL's address.
    pub client: reqwest::Client,
    /// `host:port` of the device's proxy: a WebSocket opened there reaches
    /// the device listener as the device.
    pub addr: String,
    /// The device's certificate and key.
    pub identity: Identity,
    /// The row's `key_hash`: `spki:` and the key's hash.
    pub key_hash: String,
    /// The key's DER SubjectPublicKeyInfo.
    pub spki: Vec<u8>,
    pub credentials: Credentials,
    _proxy: Proxy,
}

/// A device that enrolled and that the owner has not confirmed yet.
pub struct Enrolled {
    pub link: PairingLink,
    pub credentials: Credentials,
    pub identity: Identity,
    pub key_hash: String,
    pub spki: Vec<u8>,
}

/// The device listener's port, turning it on (loopback, a free port) when
/// it does not serve yet, as the pairing form's confirmation does.
pub async fn listening(state: &SharedState) -> u16 {
    let status = state.devices.listener.status(state);
    if status.state == lmgw_api_types::device_link::DeviceListenerState::Listening {
        return status.port.expect("a listening device listener has a port");
    }
    super::device_link::arm_free(state, |_| {}).await.0
}

/// An `/api/op/*` call as the owner: status and body.
pub async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A device named `name` paired on `state` (served as `gw`) as this
/// computer with `policy` (`key_set`'s fields), enrolled and confirmed.
pub async fn pair(state: &SharedState, gw: &Gw, name: &str, policy: Value) -> Paired {
    let mut body = json!({ "kind": "device", "name": name, "local_only": true });
    for (k, v) in policy.as_object().cloned().unwrap_or_default() {
        body[k] = v;
    }
    let link = mint(state, gw, "key_create", body).await;
    let enrolled = enrol(&link).await;
    confirm(gw, &enrolled).await;
    connected(enrolled, format!("device:{name}")).await
}

/// The owner confirms `e`'s key with the code the device shows.
pub async fn confirm(gw: &Gw, e: &Enrolled) {
    let (status, v) = op(
        gw,
        "device_confirm",
        json!({ "id": e.credentials.device_id, "confirm_code": e.credentials.confirm_code }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
}

/// The link `op` (`key_create` or `device_token`) mints with `body`, the
/// device listener turned on first.
pub async fn mint(state: &SharedState, gw: &Gw, name: &str, body: Value) -> String {
    listening(state).await;
    let (status, v) = op(gw, name, body).await;
    assert_eq!(status, 200, "{name}: {v}");
    v["link"].as_str().unwrap().to_string()
}

/// What `/api/events`' `keys` frames said of key `key_id` since `rx`
/// subscribed (or last read), in order: `token`, `enrolled`, `confirmed`,
/// `contested`, `link`, ….
pub fn keys_said(
    rx: &mut tokio::sync::broadcast::Receiver<lmgw_core::telemetry::Event>,
    key_id: i64,
) -> Vec<String> {
    let mut said = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let lmgw_core::telemetry::Event::Keys(k) = e {
            if k.key_id == key_id {
                said.push(k.what);
            }
        }
    }
    said
}

/// A fresh EC P-256 key: its DER SubjectPublicKeyInfo and its PKCS#8.
pub fn key() -> (Vec<u8>, PrivateKeyDer<'static>) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
    (
        spki,
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
    )
}

/// `POST /devices/enrol` for `link` with `spki`, as `lmgw-client` sends it,
/// over TLS pinned to the link's `fp` and without a client certificate: the
/// status and the body.
pub async fn post_enrol(link: &str, spki: &[u8]) -> (u16, String) {
    let parsed = PairingLink::parse(link).unwrap();
    let request = lmgw_client::requests::enrol(&parsed.token, spki, Some("lmgw tests"));
    let cfg = lmgw_client::tls::enrol_config(tls::provider(), parsed.fp).unwrap();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(cfg)
        .build()
        .unwrap();
    let url = format!("{}{}", parsed.urls.as_slice()[0], request.path);
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(request.body.unwrap_or_default())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// Enrol a fresh key with `link`, read as `lmgw-client` reads it.
pub async fn enrol(link: &str) -> Enrolled {
    let (spki, private) = key();
    enrol_key(link, spki, private).await
}

/// Enrol the key `spki` (its private half `private`) with `link`, read as
/// `lmgw-client` reads it: a device that keeps its key across a re-pair
/// renews its certificate.
pub async fn enrol_key(link: &str, spki: Vec<u8>, private: PrivateKeyDer<'static>) -> Enrolled {
    let (status, body) = post_enrol(link, &spki).await;
    let parsed = PairingLink::parse(link).unwrap();
    let credentials = lmgw_client::requests::read_enrolment(status, &body, &parsed, &spki)
        .unwrap_or_else(|e| panic!("the enrolment ({status}) did not read: {e:?}: {body}"));
    Enrolled {
        link: parsed,
        identity: (
            CertificateDer::from(credentials.certificate.clone()),
            private,
        ),
        key_hash: tls::key_hash(&spki),
        credentials,
        spki,
    }
}

/// The device that enrolled as `e`, named `name`, with its client and proxy.
pub async fn connected(e: Enrolled, name: String) -> Paired {
    device(e.credentials, e.identity, e.key_hash, e.spki, name).await
}

impl Paired {
    /// The same device once the device listener moved to `port` on
    /// loopback: its own client and proxy, which reach the listener there.
    pub async fn at_port(&self, port: u16) -> Paired {
        device_on(
            self.credentials.clone(),
            (self.identity.0.clone(), self.identity.1.clone_key()),
            self.key_hash.clone(),
            self.spki.clone(),
            self.name.clone(),
            port,
        )
        .await
    }
}

/// A device holding `credentials` and `identity`, with its client and proxy.
async fn device(
    credentials: Credentials,
    identity: Identity,
    key_hash: String,
    spki: Vec<u8>,
    name: String,
) -> Paired {
    let port = url_port(&credentials);
    device_on(credentials, identity, key_hash, spki, name, port).await
}

/// [`device`], its proxy reaching the device listener on `port`.
async fn device_on(
    credentials: Credentials,
    identity: Identity,
    key_hash: String,
    spki: Vec<u8>,
    name: String,
    port: u16,
) -> Paired {
    let cfg = Arc::new(device_config(&credentials, &identity));
    let proxy = Proxy::start(port, cfg).await;
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", proxy.addr)).unwrap())
        .build()
        .unwrap();
    Paired {
        id: credentials.device_id,
        name,
        client,
        addr: proxy.addr.clone(),
        identity,
        key_hash,
        spki,
        credentials,
        _proxy: proxy,
    }
}

/// Device `p` with a certificate from lmgw's CA for its own key that lapses
/// `validity` from now (whole seconds, as a certificate's dates are): what
/// it opens ends at that `notAfter` (§5.1). Its own client and proxy, and
/// the `notAfter`.
pub async fn lapsing(
    state: &SharedState,
    p: &Paired,
    validity: chrono::Duration,
) -> (Paired, chrono::DateTime<chrono::Utc>) {
    let ca = state.devices.trust.ca().expect("the device CA is loaded");
    let short = p.name.strip_prefix("device:").unwrap_or(&p.name);
    let issued = tls::issue_device_for(&ca, &p.spki, short, chrono::Utc::now(), validity).unwrap();
    let mut credentials = p.credentials.clone();
    credentials.certificate = issued.der.clone();
    credentials.not_after_ms = issued.not_after.timestamp_millis();
    credentials.notice_after_ms = issued.notice_after.timestamp_millis();
    let identity = (CertificateDer::from(issued.der), p.identity.1.clone_key());
    let lapsing = device(
        credentials,
        identity,
        p.key_hash.clone(),
        p.spki.clone(),
        p.name.clone(),
    )
    .await;
    (lapsing, issued.not_after)
}

/// `lmgw-client`'s configuration for every connection after enrolment.
pub fn device_config(credentials: &Credentials, identity: &Identity) -> ClientConfig {
    let provider = tls::provider();
    let signing = provider
        .key_provider
        .load_private_key(identity.1.clone_key())
        .unwrap();
    let mut cfg = lmgw_client::tls::client_config(provider, credentials, signing).unwrap();
    cfg.alpn_protocols = vec![tls::ALPN_HTTP1.to_vec()];
    cfg
}

/// The device listener's port, from the first of the device's URLs.
fn url_port(credentials: &Credentials) -> u16 {
    reqwest::Url::parse(&credentials.urls.as_slice()[0])
        .unwrap()
        .port()
        .expect("a device URL with a port")
}

/// A local HTTP proxy that forwards every request, upgrades included, over a
/// fresh TLS connection made with `cfg` to the device listener on
/// `127.0.0.1:<port>`. Stops when dropped.
struct Proxy {
    addr: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    async fn start(port: u16, cfg: Arc<ClientConfig>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let svc =
                        hyper::service::service_fn(move |req| forward(port, cfg.clone(), req));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), svc)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self { addr, task }
    }
}

async fn forward(
    port: u16,
    cfg: Arc<ClientConfig>,
    mut req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Incoming>, std::io::Error> {
    let io = |e: hyper::Error| std::io::Error::other(e.to_string());
    let upgrade = req
        .headers()
        .contains_key(hyper::header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));
    // A proxy request names the whole URL; the device listener takes the
    // path.
    let origin = req
        .uri()
        .path_and_query()
        .map_or("/".to_string(), |p| p.as_str().to_string());
    *req.uri_mut() = origin.parse().unwrap();
    req.headers_mut().remove("proxy-connection");
    let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
    let tls = tokio_rustls::TlsConnector::from(cfg)
        .connect(ServerName::IpAddress(Ipv4Addr::LOCALHOST.into()), tcp)
        .await?;
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(io)?;
    tokio::spawn(conn.with_upgrades());
    let mut resp = send.send_request(req).await.map_err(io)?;
    // A refused upgrade passes as the device listener framed it: a WebSocket
    // client reads a refusal's body from what came with its head, undecoded,
    // so the listener's own answer must carry its length (the counted body
    // keeps its size hint), which this end-to-end path holds it to.
    let Some(client) = upgrade.filter(|_| resp.status() == hyper::StatusCode::SWITCHING_PROTOCOLS)
    else {
        return Ok(resp);
    };
    let upstream = hyper::upgrade::on(&mut resp);
    tokio::spawn(async move {
        let (Ok(a), Ok(b)) = (client.await, upstream.await) else {
            return;
        };
        // Each way on its own: a device listener that refuses what the
        // client sends (a frame over its limit) still has its close frame
        // carried back after the client's side failed.
        use tokio::io::AsyncWriteExt;
        let (mut from_client, mut to_client) = tokio::io::split(TokioIo::new(a));
        let (mut from_server, mut to_server) = tokio::io::split(TokioIo::new(b));
        let up = async {
            let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
            let _ = to_server.shutdown().await;
        };
        let down = async {
            let _ = tokio::io::copy(&mut from_server, &mut to_client).await;
            let _ = to_client.shutdown().await;
        };
        tokio::join!(up, down);
    });
    Ok(resp)
}
