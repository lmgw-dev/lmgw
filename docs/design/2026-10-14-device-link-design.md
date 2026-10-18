# The device link: mutual TLS from pairing, and the Chat feed as the devices' push channel

**Approved, ready for implementation, 2026-10-14.** Draft v2, against main at 6153978b: v1 plus a
review's fixes (appendix "Review dispositions"), then the owner's answers to Q1 to Q5, each
approved as recommended (§15, "Answers"). Nothing of it is built. Everything under "Decided" is
the owner's: the rulings of 2026-10-14, the answers and the standing rules that bind every client
of lmgw. "Taken in this draft" (D1 to D24) is this record's own and stands until the owner changes
it. No question is open.

Paths are relative to the lmgw repository; `kai/` and `kai-mobile/` name the client repositories
beside it. Companion records are cited by short name:
- "client-apps §n": [lmgw for client apps](2026-10-06-client-apps-design.md) (device keys, the
  pairing link, the Chat feed, device-hosted MCP);
- "principals §n": [principals, origins and mounts](2026-09-22-principals-origins-mounts-design.md);
- "calls": [WebRTC realtime calls](2026-10-14-realtime-calls-design.md), revised in parallel with
  this one, so it is cited by content and by its decision numbers (C5, C6, C18), never by section;
- "kai §n": the Plasma client's design, `kai/docs/design/2026-10-06-kai-design.md`;
- "kai-mobile Mn, §n, Pn": the Android client's design,
  `kai-mobile/docs/design/2026-10-14-kai-mobile-design.md`.

**What this record covers.** How lmgw's remote devices connect, in two parts:
- **(A) the link itself**: encrypted, mutually authenticated connections between lmgw and each
  paired device, with the keys exchanged at pairing (§2 to §6);
- **(B) the Chat feed as the devices' push channel**: what a phone that holds the feed open in the
  background needs from lmgw (§7).

The first consumer is Kai mobile, the owner's Android client; the second is Kai desktop. Both are
Kai clients, which may use lmgw's own APIs freely; nothing here changes what a stock OpenAI client
sees on the existing listener.

| § | Content | WP |
|---|---|---|
| 1 | Overview: two listeners, one credential per listener | — |
| 2 | Trust: lmgw's device CA, the server certificates, the device certificates | 1 |
| 3 | Pairing: the link v2, pairing tokens, enrolment and its confirmation, what a row stores | 3 |
| 4 | The device listener: scope, port, TLS, the accept loop, its stop, what it serves, the principal | 2 |
| 5 | Revocation, re-pairing, expiry, rotation | 3, 8 |
| 6 | The device side: Android, the desktop, `lmgw-client`; what changes in kai-mobile | 6, Kai |
| 7 | The feed as push channel | 5 |
| 8 | Calls: the candidate, the SDP, the UDP socket, and what the calls design changes | calls |
| 9 | Settings and surfaces | 2–5 |
| 10–12 | Security, dependencies, migration on the alpha wire | 1, 2, 7 |
| 13–16 | Tests, work packages, questions for the owner, not verified | — |

## Decided by the owner (2026-10-14)

Rulings:
1. **lmgw and its devices talk over encrypted, mutually authenticated connections, with keys
   exchanged at pairing.** Standard TLS, no custom crypto layer. TLS covers HTTP, the SSE feed, the
   MCP-host WebSocket and the WebRTC SDP exchange, whose DTLS fingerprints it thereby
   authenticates.
2. **A separate TLS listener serves devices.** The loopback listener stays plain HTTP, because
   local OpenAI clients and the dashboard webview expect `http://127.0.0.1`.
3. **Pairing pins lmgw's identity through the QR's `fp`.** The device generates its own key pair
   (on Android a non-extractable P-256 key in the Keystore) and enrols the public key with a
   **one-time pairing token**, so the QR no longer carries a long-lived device key. From then on
   the device authenticates with mutual TLS.
4. **WireGuard becomes routing, not security.** The pairing link may carry several URLs (LAN,
   WireGuard), and the device uses whichever answers.
5. **The Chat feed is the push channel.** Kai mobile keeps the feed connection alive in its own
   foreground service. There is no ntfy, no UnifiedPush and no Web Push. lmgw gains what a held
   mobile connection needs, such as a per-device keep-alive (today `chat_feed_keepalive_s` is one
   global setting).

Standing rules, binding on this design:

6. **Alpha: no backward-compatibility constraint on lmgw's own wire.** Kai desktop and
   `lmgw-client` change along with it.
7. **Kai clients may use lmgw-only APIs freely; stock OpenAI clients see unchanged behaviour.**
8. **No hidden limits.** Token lifetimes, certificate validity, keep-alives and timeouts are visible
   settings or derived from real values, and an overrun is a visible error.
9. **Destructive actions warn and confirm, then really happen** (a single-user desktop app).

The owner's answers to §15 (2026-10-14), each as recommended:

10. **The device listener's port is the HTTP port's number plus one**, written into the settings
    the first time the listener is enabled (Q1; D10, §4.1).
11. **The calls' UDP socket follows the device listener live** (Q2; D21, §8).
12. **No bearer device keys on loopback**: every device authenticates by its certificate (Q3; D12,
    §4.8).
13. **Each enrolment is confirmed on the card** (Q4; D23, §3.5).
14. **Loopback only until the first remote device**, then widened with a confirmation (Q5; D24,
    §4.1).

## Taken in this draft

**D1. lmgw runs a small private CA; the pairing link pins it.** One P-256 key signs lmgw's server
certificates and every device's client certificate. The link's `fp` is the SHA-256 of that CA's
SubjectPublicKeyInfo. The server verifies clients with rustls's standard `WebPkiClientVerifier`
against the CA, and a device verifies lmgw with the standard `WebPkiServerVerifier` against the
same CA as its only root. §2.
- *Why:* both directions use rustls's stock verifiers, so the only code of ours is issuance and
  the row lookup. A server certificate can be re-issued at any time (new names, new key, new
  validity) without touching a device, because the device trusts the CA, not a leaf. Standard
  tools work against it: `curl --cacert ca.crt --cert … --key …` and `openssl s_client` (§13.2).
- *Rejected, a pinned self-signed leaf with RFC 7250 raw public keys.* rustls 0.23.40 supports raw
  public keys for TLS 1.3 only (its features manual), in both directions
  (`AlwaysResolvesServerRawPublicKeys`, `AlwaysResolvesClientRawPublicKeys`,
  `requires_raw_public_keys` on both verifier traits). That would spare the device a certificate,
  but each side then needs a verifier of its own written around the signature check, curl and
  OpenSSL's command-line tools cannot speak it, and the server's key becomes the pin, so replacing
  that key means re-pairing every device.
- *Rejected, pinning the leaf's certificate hash* (the meaning `fp` had while reserved, kai §3.1):
  every renewal and every new address in the certificate would re-pair every device.

**D2. The CA's key is the identity; it never expires on a date and is wrapped at rest.** The key is
`tls/ca.key` in the data directory (a `0700` directory; the file `0600`, created with its mode before
a byte is written, replaced atomically), the certificate `tls/ca.crt` (PEM, `0644`). Whenever a
session bus with a Secret Service answers, which is the normal case since lmgw starts inside the
desktop session, the key file holds the PKCS#8 key encrypted (AES-256-GCM, aws-lc-rs) under a
random 256-bit wrapping key kept as a Secret Service item. Only a headless start without one keeps
the key in plain PKCS#8 at `0600`, and the next start with a session bus wraps it. The status says
which mode is active. The CA certificate's `notAfter` is 9999-12-31, RFC 5280's value for "no
well-defined expiration": a rustls trust anchor is the subject, the SubjectPublicKeyInfo and the
name constraints alone (`rustls-pki-types` 1.14.1's `TrustAnchor`), so no rustls client reads a
root's dates, and the pin is the key. The key changes only by the owner's rotation (D19). §2.1.
- *Why wrapped:* the key lets whoever copies it impersonate lmgw to every paired device, its
  hosted tools included (§10). The database beside it is sensitive too, but that does not make one
  more copyable secret acceptable when the session offers a store.

**D3. Server certificates are issued in memory and never stored: one loopback leaf, one leaf per
interface address.** A connection's local address (known when the TCP connection is accepted,
before the handshake) picks its leaf: one leaf serves every loopback address (127/8 and `::1`);
every other address gets its own leaf only if `if_addrs` lists it, so no process can grow the cache
by connecting to arbitrary local addresses. Each leaf has a fresh P-256 key and is re-issued once
half of `devices.server_cert_days` (default 30) has passed. Its SANs are its address (or the
loopback addresses and `localhost`) and **every host of every device row's URL list, IP addresses
included**, so a device that dials an address the network rewrites (a WireGuard peer that
masquerades onto the LAN) still finds its URL's host in the leaf. An interface that comes up after
the start needs nothing. §2.2.

**D4. Device certificates are issued by the CA for the device's own key; renewal comes later.** The
device sends its public key (an EC P-256 SubjectPublicKeyInfo) with the pairing token, and lmgw
answers with a certificate for it: `clientAuth` only, a random serial, validity
`devices.client_cert_days` (default 365). From two thirds of the validity on (`notice_after`), the
card and the client show a notice with the date, and re-pairing (a new token for the same row)
issues a fresh certificate. Silent renewal over mTLS (`POST /devices/renew`, same key, no user) is
designed here (§5.4) but built in a later work package (WP8), after the first usable version. §2.3.
- *Why a finite lifetime when the row decides anyway (D5):* it is a second bound for what a row
  cannot cover: a database restored from a copy older than a device's deletion, or a device lmgw
  lost track of.
- *Why public key, not a CSR:* a CSR proves possession at enrolment, which proves nothing the token
  does not already settle, and possession is proven at every handshake after that. Android hands
  the SubjectPublicKeyInfo out as it is (`PublicKey.getEncoded()`), so the phone needs no PKCS#10
  encoder.

**D5. The device's key is its credential; the row and the certificate's date are checked on every
request.** A device row's `key_hash` holds `spki:<hex SHA-256 of its SubjectPublicKeyInfo>`, unique
among device rows. The principal is resolved per request from the connection's certificate against
the snapshot now (§4.7), so Disable, Delete and the policy's `expires_at` act on the next request
even on a kept-alive connection, with today's codes. The certificate's `notAfter` (read with
x509-parser at the handshake; rustls-webpki 0.103.13 keeps its validity `pub(crate)`) is checked
per request too, and a long-lived link (feed, WebSocket, call, host link) ends when it passes, with
the kind `certificate_expired`. There is no CRL and no OCSP: the row is the list. **TLS session
resumption is off** on the device listener: rustls's server default resumes from an in-memory cache
of 256 sessions and restores the client's certificate chain from it without running the client
verifier (`server/tls13.rs`).

**D6. The pairing link v2.** One string, given once, as text and as a QR code:

```
lmgw-pair:?v=2&name=<name>&url=<https URL>[&url=<https URL>…]&fp=sha256:<64 hex>&token=lmgw-enrol-<32 hex>
```

Each value is percent-encoded with RFC 3986's unreserved set kept, as v1's are
(`devices::pairing_link`). `url` repeats, in the owner's order of preference, and is `https` only.
`fp` and `token` are required, and there is no `key`. §3.1.

**D7. Enrolment is an lmgw route on the device listener.** `POST /devices/enrol` takes `{token,
public_key, client}` without a client certificate and answers the certificate, the CA certificate,
the dates, the device's URL list and the confirmation code (D23). §3.4.

**D8. Pairing tokens are one-time, short-lived and hash-only; their use is visible.** 128 random
bits, `devices.token_minutes` (default 10) shown on the QR card with its expiry, one pending token
per device row, used in one write transaction. The same key presenting a used token again gets the
same answer (a client that lost the first one). A different key presenting a used token is refused
and marks the row **contested**, with a dashboard notice. Closing the QR card offers to end the
token. Failed attempts are counted per source address (IPv6 by /64) and shown; a 429 comes only
far above any honest rate (`devices.enrol_refuse_per_min`, default 120), so a WireGuard peer that
masquerades many phones behind one address is never locked out; 128-bit tokens need no lockout.
§3.3.
- *Changed 2026-10-15 (WP3 review, the owner's decision):* a used token ends when the confirmed
  device makes its first request with the certificate the token's enrolment answered: that proves
  it got its answer, so the handshake without a certificate does not stay open for the token's
  whole life. Confirm alone does not end it (§3.3's notes).

**D9. The row exists from the pairing form on; scope is set there, as today.** "Pair a device"
creates the device row with its policy (scopes, budget, tool scope, hosting label, admin level,
URLs) and a token. The enrolment adds only the key. Re-pairing is a new token on the same row,
which keeps its id, policy and history; the old key works until the new token is used, unless the
owner ends it at once. `key_rotate` no longer applies to devices. §3.2, §5.2.
- *Changed 2026-10-15 (WP3 review, the owner's decision, following §3.5 and §5.3):* "until the new
  token is used" read as "until it is used **and confirmed**". An enrolment never ends a working
  key: the same key enrolling again is a renewal (a fresh certificate, its confirmation kept, no
  `Rotated`, no link closed); a new key beside a confirmed one waits for its own Confirm, while the
  kept key works on, and the Confirm makes it the row's key and ends the kept one with `Rotated`.
  "End the current key now" is as before. So whoever photographs a re-pair's QR code gets a key
  the owner never confirms, never the working device's end. §5.2's notes.

**D10. The device listener is a socket of its own, with a stop of its own.** It is off until the
first pairing turns it on, and its scope and port are visible settings (D24, §4.1): loopback only
until the first device that is not "this computer", `[::]` from then on, on the HTTP port's number
plus one (Q1), written into the settings the first time it starts, so a later change of
`bind_addr`'s port never moves it silently. TLS 1.3 only (the client's certificate travels
encrypted), ALPN `http/1.1`, tokio-rustls. It is served by a loop of lmgw's own over hyper-util,
because `axum::serve` sets no timer (axum 0.8.9, `serve/mod.rs:391`) and so cannot bound a
request's headers (§4.3). **Its streams run under a stop generation of its own** (§4.4): turning it
off or moving it ends the device listener's streams, sessions and calls, never the plain
listener's. Its accept task is owned by the listener and dies with it, so a move rebinds at once.
- *Rejected, one port with first-byte sniffing.* Plain HTTP would then be reachable wherever
  devices reach, against ruling 2's split. Every connection would pay a peek, with a timeout of its
  own, before either protocol starts. The gain is one firewall number, which Q2 gets for the phone
  anyway (TCP and UDP of the device port).

**D11. The pre-authentication surface is as small as the token's life.** The handshake's
configuration is chosen per connection: **a client certificate is mandatory while no pairing token
is pending**, and optional only during a token's life. A connection without a certificate reaches
`POST /devices/enrol` alone: any other route answers 401 `client_certificate_required`, no
connection without a certificate writes a `request_logs` row, every response to one carries
`Connection: close`, and the whole connection is bounded by `devices.unauthenticated_timeout_s`
(default 10). The `/devices/*` bodies are limited to 16 KiB, stated in their `DocRoute`, not
inherited from `max_body_mb`. The listener serves every `Inference` and `Chat` route and
`/devices/*`, nothing else: no SPA, `/api`, labs, agent proxies or CORS. A bearer, a session cookie
or a key subprotocol on any route but enrolment is refused, 400 `credential_on_device_listener`.
§4.6, §4.7.

**D12. Bearer device keys end** (Q3). Every device, Kai desktop on the same machine included,
authenticates by its certificate over the device listener; loopback is one of its addresses. The
plain listener authenticates no device. A `lmgw-device-` bearer is answered 401
`device_key_unknown`, and for a row paired with a v1 link the message says so (§12). The gain is
not the handshake's cost but **a squatter on the port gets nothing**: Kai desktop autostarts and
retries while lmgw is not running, so any local process that binds `127.0.0.1:<port>`
first would receive a bearer on Kai's next retry. A client that pins lmgw's CA refuses the squatter
before it sends anything, and it holds no bearer to send. §4.8.

**D13. Several URLs, refreshed by lmgw.** The pairing form proposes the device listener's
reachable addresses and the owner keeps, orders, removes or adds. The row stores the list, the link
carries it, and enrolment and the feed's `hello` return it, so a device follows a change without
re-pairing; a client keeps only the `https` URLs of a refreshed list. A client tries the URLs by the
rule `lmgw-client` holds (§6.3). TLS 1.3 has the client verify lmgw before it sends its own
certificate, so a URL that lands on a stranger's host (a foreign Wi-Fi with the same subnet) learns
nothing and gets nothing. §3.1.

**D14. Device-side keys stay on the device.** On Android the key is a non-extractable P-256 key in
the Keystore (TEE, not StrongBox), and TLS signs through a callback from the Rust core into Kotlin.
On the desktop it is a software P-256 key in Kai's existing Secret Service item. §6.1, §6.2.

**D15. `lmgw-client` holds the shared rules, and each client builds every lmgw-facing connection
from one pinned configuration.** The v2 parser (moved from Kai, kai-mobile P4), the enrolment
builder and reader, the URL order, and a non-default `tls` feature with the pinned enrolment
verifier and the steady-state client configuration, the crypto provider left to the app (aws-lc-rs
on the desktop, ring on Kai mobile, which spares building aws-lc-sys with the NDK). The reader keeps
`ca_certificate` only if its SubjectPublicKeyInfo hashes to `fp`. In each client, reqwest,
tokio-tungstenite and the SSE reader come from one constructor that takes that configuration, and a
test holds it to that. §6.3.

**D16. The feed's keep-alive is the stream's own, asked by the device.** `GET /chat/api/feed`
takes `keepalive_s`, from 1 to `chat_feed_keepalive_max_s` (default 900); without it a stream keeps
today's global value. The table backstop that rode on the keep-alive tick stays on the global
interval. A device's feed connection gets `TCP_USER_TIMEOUT` set from its keep-alive, so a phone
that vanished is noticed by lmgw within about two keep-alives. §7.1.

**D17. A device that was away learns the open approvals at once and knows when it has caught up.**
`hello.open_approvals` lists the pending approval requests within the principal's reach, with
`thread_title`, read in the transaction that reads the head. A live `caught_up {cursor}` follows
the last stored record up to that head. A phone takes `open_approvals` as the open set and
applies `approval.*` only after `caught_up`, so a catch-up neither raises nor withdraws anything
twice. §7.3.

**D18. Notification-ready events.** `approval.requested` gains `thread_title`. `message.added` and
`message.updated` (client-apps WP10, not built) carry the messages whole and `thread_title`. A
backgrounded phone renders a notification from the event alone. A push never starts voice. §7.4.

**D19. CA rotation is the owner's act and re-pairs every device.** A destructive action with a
confirmation that names every paired device. The server certificates are replaced at no cost (D3).
§5.5.

**D20. The Devices card shows the link.** The listener's scope and state, the CA fingerprint and
key mode, each device's pairing (where from, with which client, its confirmation code, contested or
not), its certificate's dates and notice, its open feeds with their keep-alive and peer address,
the pending and used tokens and the failed attempts. §9.3.

**D21. Calls: the candidate rule carries over, the SDP travels inside mTLS, and the UDP socket
follows the device listener live** (Q2). §8 lists what the calls design changes. *Changed
2026-10-14:* the calls design does it per socket group: the device listener's calls have UDP
sockets of their own, which follow it live, and the plain listener's calls keep theirs (§8).

**D22. Migration: one re-pair.** Existing v1 rows are marked, the card says "paired with a v1
link: pair again", and Kai desktop pairs once with a v2 build. §12.

**D23. An enrolment authenticates only after the owner confirms it on the card** (Q4). Enrolment
derives a short code from the enrolling key (the first 32 bits of its SubjectPublicKeyInfo's
SHA-256, shown as `7F3A-91C2`). The device shows it, and the still-open QR card shows it live, with
the peer address, through the `keys` frame. Until the owner clicks **Confirm**, the row answers
403 `device_unconfirmed`, naming the code. A photographed token then buys a stranger a row the
owner sees and deletes, never a working device. §3.5.

**D24. Loopback only until the first remote device** (Q5). While every paired device is "this
computer", the device listener binds `127.0.0.1` and `[::1]`. The first pairing of another device
widens it to `[::]`, with a confirmation that names the exposure: Fedora's Workstation zone, the
default on Workstation and KDE (`/usr/lib/firewalld/zones/FedoraWorkstation.xml`), admits TCP and
UDP 1025–65535, so the port is reachable from the LAN at once. §4.1.

## What exists today (relied on, not re-derived)

- **The listener.** `server::run` binds one `TcpListener` on `bind_addr` (`server.rs:465-500`;
  default `127.0.0.1:8787`, `config/settings.rs:79-81`). `server::stop::serve_stopping` serves any
  `axum::serve::Listener<Addr = SocketAddr>` with `into_make_service_with_connect_info::<SocketAddr>()`;
  the agent proxy reads that peer address, and the calls design adds the local one. CORS is
  permissive (`server.rs:146`).
- **The stop.** `Stops` is one generation for the whole process (`server/stop.rs`: `now`, `stop`,
  `stopped_after`). Every stream, session and turn ends when it moves, and `serve_stopping` takes
  its generation at the start of a serve.
- **Refusals are recorded.** A refusal at the gate goes through `policy_refusal`, which writes a
  `request_logs` row (`proxy::recording::record_middleware_refusal`).
- **The principal.** `principal_mw` (`server.rs:1021`) reads a bearer, then the subprotocol key on
  `/v1/realtime`, then the cookie (`principal.rs:445-492`); a device-prefixed bearer with no row is
  `device_key_unknown`. `Principal::Key.fingerprint` is the head of `key_hash`
  (`config/keys.rs:202`), and `presents` compares it, so a rotated credential fails every per-call
  check. The gate's `Chat` arm re-checks the row and its expiry. `key_hash` is the hex SHA-256 of
  the presented string (`config/keys.rs:377`), not unique in the schema.
- **Devices** (`devices.rs`, `devices/`). `mint` and `pairing_link` make the key and the v1 link
  with `fp` reserved (`devices/pairing.rs:15-35`); `MintedLinks` hands lmgw's own window only the
  exact link it minted, once. `Revocations` is one signal per key (`RevokeReason` Disabled,
  Rotated, Deleted, Expired; the tokens `device_disabled`, `key_expired`, `key_unknown`, `revoked`
  on 4003 closes, `devices.rs:339`, and on the feed's `revoked`). `Links` counts each device's
  feed, voice and tools links (`LinkKind`); `LinkGuard` stamps `last_seen_at`.
- **Addresses.** `net::reachable_urls` expands a wildcard bind into loopback plus each up,
  non-link-local interface address (`if_addrs`), as `http://` URLs (`net.rs:32-74`).
- **The feed.** `chat_feed_keepalive_s` is global (default 15, `config/settings.rs:279`), stated in
  `hello` and kept by a stream for its life. Its tick is also the table backstop for a write that
  forgot to wake the feed, and the measure of the snapshot-behind wait
  (`web/chat_feed/stream.rs:249-264`, `:512`). A stream knows the head it read at open
  (`caught_up_at`, `web/chat_feed.rs:353`). `hello` carries no approvals
  (`lmgw-api-types/src/chat_feed.rs:198`), and no route lists the open ones. `approval.requested`
  carries `thread_id`, `message_id`, `approval_request_id`, `server_label`, `name`, `arguments`
  and `call_id`, no title (`:419-434`). `message.*` records are not built (client-apps WP10, with
  the QR code).
- **Crates in the tree.** rustls 0.23.40 with aws-lc-rs (through reqwest 0.13), tokio-rustls 0.26.4,
  rustls-webpki 0.103.13, hyper 1.10.1, hyper-util 0.1.20, socket2 0.6.4, aws-lc-rs 1.17.0, axum
  0.8.9 (no `http2` feature), url 2.5.8, zbus 5.16.0. No rcgen, no Secret Service client.

## 1. Overview

```
              plain listener: bind_addr, http       device listener: devices.listen, https
local OpenAI  client or owner key, cookie           ✗ a bearer or cookie is a 400
dashboard     cookie (same origin)                  ✗ no SPA, no /api: 404
Kai desktop   ✗ device bearer: device_key_unknown   client certificate (loopback URL)
Kai mobile    ✗                                     client certificate (LAN or WireGuard URL)
new device    ✗                                     POST /devices/enrol: token, no certificate
WebRTC media  plain group: bind_addr, webrtc_port   device group: its addresses, its port
              (calls' UDP sockets; candidate = the local address the POST arrived on)
```

*Changed 2026-10-14:* the calls design groups its UDP sockets per listener (§8).

One credential per listener: keys and the cookie on the plain listener, certificates on the device
listener. Everything a device does today it does over the device listener with its certificate in
place of its bearer: the feed, the Chat API, `/v1` (calls included), the realtime WebSocket and
`/mcp/host`. Nothing secret crosses the network: the pairing token is single-use, travels in the QR
or the link and then inside TLS pinned to `fp`, and its use is confirmed by the owner; the device's
private key never leaves it.

## 2. Trust

### 2.1 The CA

- **Created** when the device listener is first turned on (§4.1), never before: a gateway whose
  owner never pairs a device has no CA and no listener.
- **Key and certificate**, by rcgen (§11): an ECDSA P-256 key; self-signed,
  `CN=lmgw devices <8 hex of fp>, O=lmgw`; `BasicConstraints: CA, pathlen 0` (it signs leaves
  only), `KeyUsage: keyCertSign, cRLSign`; a random 127-bit serial; `notBefore` its creation day,
  `notAfter` 9999-12-31T23:59:59Z (D2).
- **The key at rest** (D2):
  - **wrapped**, whenever the session bus's Secret Service answers: the wrapping key is an item in
    the default collection (attributes `application: lmgw`, `kind: device-ca-wrap`, and the data
    directory, so a dev instance has its own), made by the secret-service crate (§11). A locked
    collection brings the desktop's unlock prompt; declining it leaves the device listener down,
    said with its reason;
  - **plain**, `0600`, only when no Secret Service answers at start (a headless start). The next
    start that reaches one wraps the key and removes the plain copy;
  - a wrapped key that a start cannot unwrap (headless, or the item gone) leaves the device
    listener down with that reason, and the plain listener serves on.
- **Files**: lmgw refuses to start the device listener on a key file readable by group or others,
  or not owned by the process's user; the status and the card name the file and the mode found.
- **`fp`** is `sha256:` and the lowercase hex SHA-256 of the CA certificate's DER
  SubjectPublicKeyInfo: RFC 7469's pin input, in hex. The owner can recompute it from `ca.crt`
  (`openssl x509 -in ca.crt -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum`).
- **A dev instance** has its own data directory, so its own CA. Tests make one in their temporary
  directory, plain.
  *Changed 2026-10-15 (calls WP6 review):* a dev instance's is plain too. A dev instance (the
  headless runner, a debug build, `LMGW_DEV`) never asks the Secret Service
  (`state::wrap_key_store`), whatever its environment: zbus takes an unset
  `DBUS_SESSION_BUS_ADDRESS` to mean `$XDG_RUNTIME_DIR/bus`, the user's desktop bus, and a private
  bus activates a Secret Service of its own on the user's keyring files. Before this, a dev
  instance that turned its device listener on in the desktop session made an item in the user's
  default collection, labelled `lmgw: device CA key wrapping (<its data dir>)`.
- *Changed 2026-10-14 (WP1, as built):* the wrapped key file is PEM `LMGW WRAPPED PRIVATE KEY`
  (a version byte, the 96-bit nonce, the ciphertext and tag, sealed with a fixed AAD), the plain
  one PEM `PRIVATE KEY`; a new wrapping key is made for every wrapped write, so a random nonce
  never repeats under a key. The item holds the key as 64 hex digits (`text/plain`) with the
  attribute `data_dir`; `get` searches every collection and takes the items that carry the three
  attributes (others a provider adds are let be), unlocking a locked one (the prompt). A new
  wrapping key is added beside the old items, never in their place; the key file is written; then
  the items that do not hold the new key are deleted. A crash in between leaves the old file
  openable by its old item, and a provider that ignores `replace` piles nothing up. A rotation
  that fails before the key file is replaced leaves the CA, its state and the leaves as they were.
  The key is read from one descriptor opened with `O_NOFOLLOW` and checked with `fstat` (a regular
  file, lmgw's user's, no group or other bit, `0o077`); a `tls/` directory that is a symbolic
  link, another user's or group/other-writable is refused too. Messages and the status name the
  files relative to the data directory (`tls/ca.key`). A declined prompt leaves the listener down
  for a plain key as for a wrapped one, since a Secret Service answered. The store is a seam: only
  the app's own start (`AppState::init_with`) installs the Secret Service; every other gateway,
  tests included, answers as a headless start.

### 2.2 Server certificates

- **Chosen per connection** (D3) by the accepted socket's local address, made canonical (an
  IPv4-mapped address unmapped):
  - **a loopback address** (127/8, `::1`) → the loopback leaf: SANs `127.0.0.1`, `::1`,
    `localhost` and the URL-list hosts;
  - **an address `if_addrs` lists** (the list is re-read on a miss) → that address's leaf: SANs that
    address and the URL-list hosts;
  - **any other address** → no new leaf: the connection gets a shared leaf whose SANs are the
    URL-list hosts alone, and the log names the address. The cache holds at most one leaf per
    interface address, plus the loopback one and the shared one.
- **The URL-list hosts** are every host, IP or DNS name, of every device row's URL list (§3.1).
  A host a device dials that is in no list fails the device's own verification with a name error,
  which the client shows; the owner adds the URL on the card, which clears the cache.
- **Issue**: a P-256 key generated in memory, a certificate from the CA (`serverAuth`, `KeyUsage:
  digitalSignature`, the CA's key identifier), the rustls configurations around it (§4.2). About a
  millisecond.
- **Validity**: `devices.server_cert_days` (default 30). `notBefore` is one day before the issue,
  so a device whose clock runs behind still accepts a fresh leaf; the value is in the certificate.
- **Re-issue**: a leaf past half its validity is replaced at the next connection it would serve.
  The cache is cleared when a URL list changes and when the CA rotates; a restart starts empty.
  No device notices any of it (D1).
- **The chain** sent is the leaf and the CA certificate, so a device at enrolment finds the CA by
  `fp` (§6.3).
- *Changed 2026-10-14 (WP1, as built):* a leaf's subject is `CN=lmgw device listener, O=lmgw`,
  its serial random (127 bits); "an address `if_addrs` lists" is any address it lists, link-local
  included, and a re-read drops the leaves of addresses no interface lists any more. A URL-list
  host that is neither an IP address nor letter/digit/hyphen labels (a `*`, an `_`, non-ASCII) is
  left out with a warn line, so no URL makes a wildcard name. The cache is keyed by the CA's `fp`,
  so a leaf of an earlier CA is never served.

### 2.3 Device certificates

- **Issued** at enrolment (§3.4) for the public key the device sent: `CN=<device name>, O=lmgw
  device`; `clientAuth` only, `KeyUsage: digitalSignature`; a random 127-bit serial; `notBefore`
  one day back (as §2.2), `notAfter` `devices.client_cert_days` (default 365) from the issue.
- **Key types**: EC P-256 only. Android's Keystore makes it on every device, and the desktop makes
  the same, so there is one path to test. Anything else is refused, 400 `public_key_unsupported`,
  naming the accepted kind. Ed25519 is Later.
- **`notice_after`** is the issue time plus two thirds of the validity: from then on the card and
  the client say "this device's certificate lapses on <date>; pair it again before then" (§5.3).
- **The CA does not decide who a device is; the row does** (D5). A certificate the CA signed for a
  key that no row holds opens nothing (401 `device_key_unknown`). The CA signature gets a client
  through the handshake; the key, matched to a confirmed row, is the authentication.
- *Changed 2026-10-14 (WP1, as built):* `CN` is the device's name without `device:`. A device
  certificate carries `clientAuth` alone and no subjectAltName, so it never passes a client's
  check of lmgw's server leaf (`lmgw-client`'s pin relies on it; a test holds it). The key must
  be a canonical DER SubjectPublicKeyInfo of an uncompressed point on P-256, so the `spki:` hash
  kept at enrolment is the one computed from every certificate the device presents; a compressed
  point, a point off the curve or trailing bytes are `public_key_unsupported` too.

## 3. Pairing

### 3.1 The link v2

```
lmgw-pair:?v=2&name=phone&url=https%3A%2F%2F192.0.2.1%3A8788&url=https%3A%2F%2F198.51.100.20%3A8788&fp=sha256%3A9f2c…(64 hex)&token=lmgw-enrol-4b1d…(32 hex)
```

| Parameter | Count | Value |
|---|---|---|
| `v` | 1 | `2` |
| `name` | 1 | the device's name as lmgw gave it, without `device:` |
| `url` | 1 or more | an `https` base URL (host, optional port, optional path prefix; no user info, query or fragment), in the owner's order |
| `fp` | 1 | `sha256:` and 64 lowercase hex digits (§2.1) |
| `token` | 1 | `lmgw-enrol-` and 32 lowercase hex digits (128 bits) |

- **Strict**, as the v1 parser is. A missing, empty, repeated (except `url`) or unknown parameter
  is refused, and so are the same `url` twice and any other `v`. A v1 link gets "this link is from
  a pairing before TLS; make a new one on lmgw's Devices card". No error echoes a value from the
  link.
- **The URL list** is proposed by the pairing form and is the owner's to edit:
  - **for another device:** the device listener's addresses as `net::reachable_urls` expands a
    wildcard, minus loopback, as `https://<address>:<device port>`, each labelled with its
    interface; the owner orders them;
  - **for this computer:** `https://127.0.0.1:<device port>` alone, with client-apps' note
    "reachable from this computer only";
  - the owner may add a URL by name or by an address the network rewrites, which goes into the
    server certificates (§2.2).
- **The QR code** is the same string. When it is too long for a QR code (the encoder's own error),
  the card says so and shows the link alone. This is client-apps WP10's QR code, built here.
- **The link is a credential while its token lives.** Client-apps §1.4's guidance stands: no client
  logs it, and it is dropped once used. `MintedLinks` keeps its rule (lmgw's own window hands off
  only the exact link it minted, once) and also forgets a link when its token ends.
- *Changed 2026-10-15 (WP3, as built):* `key_create` and `device_token` answer `DevicePaired {id,
  name, link, urls, local_only, token_expires_at, fp, message}`, the link carrying `name`, each `url`,
  `fp` and `token` in that order. `MintedLinks` keeps each link with its token's expiry: lmgw's
  window hands a link off once and only before it lapses, and forgets it when its token is used
  (a desktop client's own key would only contest it) or ends (a new token, `device_token_end`, a
  revocation, the CA's rotation).

### 3.2 The pairing form and the row

- **"Pair a device…"** keeps client-apps §1.4's form and its review step: name, scopes, budget,
  tool scope, hosting label, admin level. It adds "Where is the device?" (another device or this
  computer) and the URL list.
- **"Pair the device"** writes the row (kind `device`, `key_hash = pending:<32 random hex>`, the
  policy, `urls`) and its token in one write transaction, and shows the QR card. When the device
  listener is off, or loopback-only and the device is another one, the form first says what turns
  on or widens (§4.1), and the button does it after that confirmation.
- **The row on the card** reads "waiting for its device, token valid until 14:32" until it enrols,
  then "enrolled from 192.0.2.2, code 7F3A-91C2: Confirm or Delete" until confirmed (§3.5), then
  shows the pairing (§9.3). An ended token offers **New token…** and **Delete…**.
- *Changed 2026-10-15 (WP3, as built):* `key_create {kind: "device"}` takes `local_only` (default
  `false`) and `urls`. Left out, `urls` is the form's proposal (`devices::proposed_urls`):
  `https://127.0.0.1:<device port>` for this computer; for another device the listener's concrete
  address, or every up, non-loopback, non-link-local interface address (IPv4 first), refused by
  name when there is none. A URL is `https` without user info, query or fragment, its host an IP
  address or letters, digits and hyphens (what a server certificate may carry), none twice: the
  rule of `lmgw-client`'s `normalize_base_url`, which a unit test holds it to. The device listener
  must serve, else 409 `device_listener_off` naming the setting: `fp` exists only once it has run,
  and a token would reach nothing; the form turns it on with its confirmation first. Widening stays
  the form's: a remote device paired while the listener is loopback-only is written, and the
  answer's message says it will not reach lmgw until `devices.listen` is widened. The name and the
  policy are checked before the listener, so a bad field is said whatever the listener does.
  `key_set` takes `urls` too.
- *Changed 2026-10-15 (WP3 review):* the proposal for another device leaves out what a device
  cannot count on: down interfaces, loopback and link-local addresses, IPv6 temporary (privacy)
  and deprecated addresses (the kernel's flags in `/proc/net/if_inet6`), and container and VM
  bridges and links (podman, CNI, docker with its `br-<12 hex>` networks, libvirt's `virbr` and
  `vnet`, LXC, LXD, Incus, `veth`); a bridge the owner named (`br0`) is a LAN's. The stable LAN
  and WireGuard addresses come first (a point-to-point link, an IPv4 private or `100.64/10`
  address, an IPv6 unique local one), then the rest, IPv4 before IPv6 (`devices::propose`, a unit
  test over an injected list).

### 3.3 Pairing tokens

- **Shape**: `lmgw-enrol-` and 32 hex digits from the OS's random source; stored as its SHA-256
  only; shown once, in the QR card.
- **Lifetime**: `devices.token_minutes` (default 10, 1 to 1440). The QR card shows the expiry and
  the minutes left. **Closing the card** asks "End the token now?" (the default) or "Keep it valid
  until 14:32"; it ends at once on the first.
- **One per row**: `device_pairing_tokens` holds one row per device: the token's hash, created,
  expires, used, used from (the peer address), used by (the enrolling key's hash), contested. A new
  token replaces the row.
- **Single use**, in one write transaction (`store::begin_write`, `BEGIN IMMEDIATE`):
  `UPDATE … SET used_at = now, used_from = ?, used_by = ? WHERE token_hash = ? AND used_at IS NULL
  AND expires_at > now RETURNING key_id`. Two devices racing with one token serialize on the write
  lock. When the update matches nothing, one read tells the cases apart:
  - **used by the same key**, within the token's life: the enrolment's answer again (the device
    lost the first one); nothing changes;
  - **used by another key**: 410 `pairing_token_used`; the device row is marked **contested**
    (`contested_at`, `contested_from`), the card shows a notice ("the token for 'phone' was used,
    then presented again from 203.0.113.5 by another key: if your device did not pair, delete it"),
    and `/api/events` carries it. `lmgw-client` reads this 410 as "someone else used this code:
    delete the device on lmgw";
  - **expired**: 410 `pairing_token_expired`, naming the setting; **unknown**: 401
    `pairing_token_unknown`.
- **While no token is pending** the handshake requires a certificate (§4.2), so no request without
  one arrives at all.
  *Changed 2026-10-14 (WP6):* so a device enrolling with an ended token (the QR card closed, the
  token expired or used) meets the handshake's `certificate_required` alert, not a 410, and
  `lmgw-client` reads that alert at enrolment as `TlsRefusal::LinkEnded` ("this pairing link has
  ended: make a new one"). The 410s above, and the same key's repeat, reach a device only while
  some other token is pending; a device that lost its enrolment's answer can ask again only then.
  *Changed 2026-10-15 (owner):* a TLS alert is unauthenticated, and this one can arrive before the
  server proved anything, so `lmgw-client` no longer concludes "the link ended" from it:
  `TlsRefusal::LinkEnded` is gone, and the alert reads at enrolment as `TlsRefusal::Unproven`, a
  failed handshake the caller may retry that says nothing about the link (an ended token meets it on
  every retry; a caller that keeps meeting it may suggest a new link, not state one).
- **Failed attempts**: an unknown, used or expired token counts as one failure for its source
  address, an IPv6 address by its /64, in a sliding minute; an address's entry is dropped once its
  window holds nothing. The card shows the failures of the last hour (count, last address, last
  time), and each is logged with the address, never the token. Past `devices.enrol_refuse_per_min`
  (default 120) an address is answered 429 `pairing_rate_limited` with `Retry-After` until its
  window frees. The threshold sits far above any device's retries, so the many phones behind one
  masquerading WireGuard peer are never refused; 128 bits cannot be guessed, and the count is there
  so the owner sees someone trying. The counts live in memory and start at zero after a restart,
  which the card says.
- *Changed 2026-10-15 (WP3, decided in the build, D8's lost answer):* **a used token lives on until
  it lapses or the owner ends it**, and while it lives the key that used it gets the same answer
  again: the same certificate (kept with the token), its dates, the row's URLs now, the code, and
  `confirmed` as it stands, so a repeat after the Confirm says `true`. This replaces "ends when it
  is used" (§4.2's WP2 note) and, for the repeat, the WP6 note's "only while some other token is
  pending": a phone whose one answer got lost asks again on a new connection, the handshake still
  lets it in without a certificate, and it is not stranded whether or not the owner confirmed
  meanwhile. Nobody without the key obtains anything: the repeat compares the presenting key's
  `spki:` hash with the one that used the token; another key gets 410 `pairing_token_used` and
  marks the row contested (now reachable without a second token); and the answer holds nothing
  secret, the certificate being useless without the device's private key. The cost is the
  certificate-less handshake for the rest of the token's life (`devices.token_minutes`, 10 by
  default), which the QR card's close ends by default. Confirm does not end it: a device whose
  answer got lost has shown no code yet, and the owner may have confirmed by the card's code alone.
- *Changed 2026-10-15 (WP3, as built):* the token table has two columns beyond §3.6's:
  `certificate` (the answer's, standard base64, for the repeat) and `ended_at` (the owner ended
  it: `device_token_end`, or the CA's rotation; a new token replaces the row). An ended token is
  410 `pairing_token_expired` saying it was ended on the card; a lapsed one names
  `devices.token_minutes`. Counted as failures: an unknown token (a malformed one included), a used
  one another key presents, a lapsed one, an ended one; a disabled device, a key in use, a bad body
  or key are not the token's. The 429's window slides over 60 s per address (`server::peer_key`:
  IPv4, IPv6 by its /64), and `Retry-After` is the seconds until enough of it has passed to fall
  below the setting. "A token is pending" (§4.2) is the store's tokens alive
  (`store::alive_tokens`), loaded at the start and handed to `PendingTokens` by every write that
  mints, ends or resets one.
- *Changed 2026-10-15 (WP3 review, the owner's decision on D8):* **a used token also ends at its
  device's first request with the certificate it answered.** The gate lets in a confirmed device
  whose certificate's serial is the one the token's enrolment issued (`PendingTokens` keeps it,
  read back from the token's `certificate` at a start) and ends the token there (`ended_at`): the
  device holds its answer, so the certificate-less handshake closes instead of staying open for
  the rest of `devices.token_minutes`. Confirm alone still does not end it, and neither does a
  request with another certificate of the same key (a renewal's old one). The handshake set lets
  go at once; the store's `ended_at` is written on a task of its own, with the `keys: token`
  frame.
- *Changed 2026-10-15 (WP3 review):* `PendingTokens` changes with each write as it commits (a
  mint sets the row's token, an end or a delete removes it, an enrolment notes the serial it
  answered); only a start and the CA's rotation, which ends every token at once, load the store's
  whole set. A whole read racing a write could otherwise bring back a token just ended or drop one
  just minted.
- *Changed 2026-10-15 (WP3 review):* the contested mark is the row's latest token's: the first
  presentation by another key marks it (and reloads, and says `contested`); a later one is refused
  and counted the same, and writes nothing. A new token clears `contested_at` and `contested_from`.
- *Changed 2026-10-15 (WP3 review):* the counts hold the same facts in constant space, whatever
  the rate: the per-address windows of the last minute, swept of addresses whose newest failure
  left the window once the map has doubled since its last sweep, and the last hour as 60
  per-minute counters with the last failure's address and time. Nothing is capped; the hour is
  counted by whole minutes.

### 3.4 Enrolment

```
POST /devices/enrol                      device listener only; no client certificate needed
Content-Type: application/json           body at most 16 KiB

{"token": "lmgw-enrol-<32 hex>",
 "public_key": "<standard base64 (RFC 4648 §4, padded) of the DER SubjectPublicKeyInfo>",
 "client": "Kai mobile 0.1.0 (Android 16)"}

201 Created
{"device": {"id": 12, "name": "phone"},
 "certificate": "<base64 DER: the device's certificate>",
 "ca_certificate": "<base64 DER: lmgw's device CA>",
 "not_after": "2027-10-14T14:03:11Z",
 "notice_after": "2027-06-14T14:03:11Z",
 "urls": ["https://192.0.2.1:8788", "https://198.51.100.20:8788"],
 "confirm_code": "7F3A-91C2",
 "confirmed": false}
```

- **Called over TLS pinned to `fp`** (§6.3), without a client certificate; one presented anyway is
  ignored, and so is any credential header: the token decides.
- **`client`** is free text within the body's limit, shown on the card ("Kai mobile 0.1.0
  (Android 16)"). It is optional and never trusted for anything.
- **The steps, in one write transaction:** take the token (§3.3); read its row (a disabled row is
  401 `device_disabled`, and the token stays unused); parse the key (rcgen's
  `SubjectPublicKeyInfo::from_der`) and require EC P-256; refuse a key another device row holds;
  issue the certificate; write `key_hash = spki:<hex>`, the certificate columns, `enrolled_at`,
  `enrolled_from`, `client_info` and `confirmed_at = NULL`; commit. Then the snapshot is published;
  if the row held an earlier key (a re-pair), `Rotated` is raised for it (§5.2); then the `keys`
  frame with the code and the peer address.
- **The answer's `Connection: close`** ends the connection: the device's next request comes on a
  new connection with its certificate.
- **Refusals:**

| HTTP | `code` | When |
|---|---|---|
| 400 | `bad_request` | the body is not the shape above |
| 400 | `public_key_unsupported` | not a DER SubjectPublicKeyInfo, or not EC P-256 |
| 401 | `pairing_token_unknown` | no such token (never issued, or its row deleted) |
| 401 | `device_disabled` | the token's device is disabled |
| 409 | `public_key_in_use` | another device enrolled this key |
| 410 | `pairing_token_used` | used by another key; the row is now contested |
| 410 | `pairing_token_expired` | expired at the time named; the message names `devices.token_minutes` |
| 413 | `body_too_large` | over 16 KiB |
| 429 | `pairing_rate_limited` | §3.3, with `Retry-After` |
| 503 | `devices_ca_unavailable` | the CA cannot be read or unwrapped; the message names the reason |

*Changed 2026-10-15 (WP3, as built):* the checks run in this order: a declared length past 16 KiB
(the route's own layer, inside its gate, §4.6); the address's 429; the body, read whole within the
same 16 KiB, and its shape; the key (standard base64, a
canonical EC P-256 SubjectPublicKeyInfo, `tls::check_public_key`); the token's shape; the CA loaded
(else 503 naming its state); then the one write transaction (`store::enrol_device`), which issues
the certificate inside it, so a refusal after the token was taken rolls the take back. A key
another device row holds is 409 and leaves the token unused, as a disabled row does. Re-enrolling a
row that held an enrolled key raises `Rotated` for it, the same key or a new one, so what the
earlier enrolment held open ends and the new one waits for its own Confirm. `client` is kept
trimmed, an empty one as none.
*Changed 2026-10-15 (WP3 review, the owner's decision):* the re-enrolment above is replaced by
D9's note: the key the row holds is renewed (its certificate columns, the enrolment's, its
confirmation kept; `confirmed: true` in the answer when it was confirmed), a new key beside a
confirmed one is written as the row's waiting key (§3.6) with its certificate, and only a row with
no working key (`pending:`, `v1:`, or an enrolled key never confirmed) takes the new key as its
own, unconfirmed; `Rotated` is raised only for such an unconfirmed key, which held nothing open.
"Another device row holds" includes a row's waiting key. When the reload after an enrolment that
replaced a key fails, the snapshot is laid over as the row's key ended (`KeyWritten::Pending`),
failing closed as `device_token` and `device_ca_rotate` do. `client` also loses control, format
(direction overrides and isolates, zero-width) and line separator characters, which a device's
name refuses. Every answer carries `Connection: close`, also to a connection that
presented a certificate anyway. The `keys` frame says `enrolled`; the code and the peer address are
the row's `pairing` in the Keys list (§9.3).

### 3.5 Confirmation (D23, Q4)

- **The code** is the first 32 bits of the SHA-256 of the enrolling key's SubjectPublicKeyInfo,
  as two groups of four uppercase hex digits. Both ends compute it from the same key, and a stranger
  enrolling with a stolen token holds another key, so another code.
- **The device** shows "Confirm 7F3A-91C2 on lmgw's Devices card". Its requests are answered 403
  `device_unconfirmed` (message: the code and where to confirm) until the owner confirms, and it
  retries on its own backoff; `lmgw-client`'s `key_refused` gains `Unconfirmed`.
  *Changed 2026-10-14 (WP6):* `Unconfirmed` carries no code (it read `Unconfirmed {code}`). The
  device shows the code of its own key, `Credentials.confirm_code`, which `read_enrolment` already
  held to lmgw's answer; the refusal's message is lmgw's wording for people
  (`device_link::unconfirmed_message`), not a contract, and no client parses it.
- **The QR card**, if still open, turns into the confirmation: "Enrolled from 192.0.2.2 with code
  7F3A-91C2. Does the device show this code?" with **Confirm** and **Delete the device**. The row's
  line on the card offers the same once the card is closed.
- **Confirm** sets `confirmed_at` (a write that publishes the snapshot), and the device's next
  request is served. A re-pair needs its own confirmation, and until then the old key works if the
  owner kept it (§5.2).
- *Changed after the owner's answers, 2026-10-14:* Q4 is answered yes, so every enrolment waits
  for Confirm. The alternative this bullet held (enrolment setting `confirmed_at` itself) is gone.
- *Changed 2026-10-15 (WP3, as built):* `device_confirm {id}` answers `DeviceConfirmed {id, name,
  confirm_code, message}`; a row with no enrolled key is refused 400 ("has not enrolled a key
  yet"), a confirmed one says it was, and when. The gateway's lookup (`devices::StoredKeys`) reads
  `confirmed_at`, so the listener's "authenticated" (§4.3) and the gate's `device_unconfirmed` are
  the row's real state; `/api/events` says `confirmed`.
- *Changed 2026-10-15 (WP3 review):* `device_confirm {id, confirm_code}`: the code the owner
  compared, which the key waiting for the confirmation must have, checked in the confirmation's
  transaction (eight hex digits, either case, hyphen and spaces optional,
  `device_link::parse_confirm_code`). Another code is 409 `confirm_code_mismatch` and changes
  nothing; its message never says the right one. The card sends the code it displayed, so a card
  still showing an earlier enrolment confirms nothing; for a contested row it asks the owner to
  type the code the device shows. The key waiting is the row's waiting key when there is one (a
  re-pair that kept the current key): the Confirm makes it the row's key, with its certificate's
  columns, ends the kept one with `Rotated` (its links close as a replaced key's, `key_unknown`)
  and, should the reload fail, lays that over the snapshot (`KeyWritten::Confirmed`). The answer
  gains `replaced`. With no key waiting and the code the confirmed key's, it says it was confirmed
  already.

### 3.6 What a device row stores

The `api_keys` row of kind `device`, as client-apps §1.1 made it, plus:

| Column | Meaning |
|---|---|
| `key_hash` | `spki:<hex>` once enrolled; `pending:<random>` before; `v1:<old hash>` for a v1 pairing (§12) |
| `urls` | the device's URL list, JSON, in order (§3.1) |
| `cert_serial`, `cert_not_after`, `cert_notice_after` | the latest certificate issued (RFC 3339 UTC) |
| `enrolled_at`, `enrolled_from`, `client_info` | the last enrolment's time, peer address and `client` text |
| `confirmed_at` | the owner's confirmation (§3.5); `NULL` refuses the device |
| `contested_at`, `contested_from` | a used token presented again by another key (§3.3) |
| `local_only` | paired as "this computer" (§4.1) |

- **Migration** (the next free number at the build): the columns, added with `ALTER TABLE`;
  `key_hash = 'v1:' || key_hash` for every device row; a unique index on `key_hash` for device rows
  whose hash starts `spki:`; and `device_pairing_tokens (key_id INTEGER PRIMARY KEY, token_hash
  TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL, expires_at TEXT NOT NULL, used_at TEXT,
  used_from TEXT, used_by TEXT)`. Every write goes through `store::begin_write`.
- **`ApiKey::fingerprint`** stays the head of `key_hash`, so `presents` keeps its meaning: a
  re-pair changes the hash, and every check that re-reads the row refuses the old key.
- **No bearer can match a device row**: a bearer's hash is 64 hex digits without a prefix.
- *Changed 2026-10-15 (WP3, as built):* migration 0081: the columns as above, `urls` `NOT NULL
  DEFAULT '[]'` and `local_only` with a CHECK; the unique index `api_keys_device_spki`; the token
  table with `certificate` and `ended_at` (§3.3's note), a soft link as every `key_id` is, deleted
  in `key_delete`'s transaction; and `request_logs.via` (§4.5). In the snapshot the columns are
  `ApiKey::link` (`config::DeviceLink`), and `DeviceLink::credential` reads the hash's prefix
  (`spki:`, `pending:`, `v1:`). Every publish hands the device rows' URL hosts to the server
  certificates (`LeafCache::set_url_hosts`), the start's too.
- *Changed 2026-10-15 (WP3 review):* four more columns, in 0081 itself (it has not shipped):
  `waiting_key_hash`, `waiting_cert_serial`, `waiting_cert_not_after`, `waiting_cert_notice_after`,
  a new key enrolled beside the row's confirmed one with its certificate (D9's note), unique among
  device rows (`api_keys_device_waiting_spki`). `DeviceLink::waiting` carries it in the snapshot,
  and `StoredKeys` finds a waiting key's row unconfirmed, so its requests get 403
  `device_unconfirmed` with its code. The `cert_*` columns are the row's key's; `enrolled_*` and
  `client_info` the last enrolment's, a waiting key's while one waits. A new token clears the
  waiting key with the token that enrolled it, and an ended key clears it too.

## 4. The device listener

### 4.1 Scope, port, on and off

- **`devices.listener`** (default off) is turned on by the first pairing (§3.2) or in Settings →
  Network & access → Devices. It takes effect at once: lmgw creates the CA if there is none (§2.1),
  binds and serves.
- **`devices.listen`** (D24): `loopback` (the default: `127.0.0.1` and `[::1]`), `all` (`[::]`,
  every address, dual-stack on Linux) or one concrete address.
  - While every device row is `local_only` ("this computer"), `loopback` stands.
  - The first pairing of another device asks to widen: "lmgw will listen for devices on every
    network address, port 8788. On Fedora's default desktop zone, TCP and UDP ports 1025–65535 are
    open, so devices on your LAN can reach it at once; they get nothing without a pairing token and
    your confirmation, or a certificate." Confirming writes `all`. Declining keeps `loopback`, and
    the form says the device will not reach lmgw.
  - **Why `all` once widened:** ruling 4 puts security in TLS, and the URL list, not the bind, says
    which addresses devices use. A WireGuard interface that comes up after lmgw starts is served
    without a rebind (D3 covers its certificate). The firewall decides what reaches the port.
- **`devices.port`** (default `null`: the HTTP port's number plus one, Q1). The first start writes
  the effective number into the setting, so a later change of `bind_addr`'s port leaves the device
  port, and every pairing link that names it, where it was.
- **Off, or a change of scope or port**, acts at once. The confirmation names the device-listener
  connections that end (feeds end; realtime sessions and calls close as at a stop) and the UDP
  socket that moves with it (§8). The plain listener's connections are untouched (§4.4).
  *Changed 2026-10-14:* what moves is the calls design's device socket group, and only device calls
  end; the plain listener's calls keep their own sockets (§8).
- **A failed bind** does not stop lmgw. The plain listener serves; the status and the card show the
  error and the setting, and the log names both.
- **The log line**: `lmgw devices listening on https://[::]:8788 (CA sha256:9f2c…41ab, key
  wrapped)` beside the plain listener's line.
- **`bind_addr` is not this design's.** It stays what the owner set. Once devices have their own
  listener, a `bind_addr` widened only so that devices could reach lmgw can return to loopback;
  only other machines' stock OpenAI clients still need it wide.
- *Changed 2026-10-15 (WP2, as built):* both saves take `listener`, `listen` and `port`; the save
  that turns the listener on with `port` unset writes the effective number in the same write (a
  setting written by hand gets it at the next start). After the save, `DeviceListener::reconcile`
  starts, moves or stops the listener and the answer says what it did ("the device listener
  listens on https://…; 1 device connection ended"). The listener runs while the gateway serves:
  `server::serve` arms it beside the plain listener (without holding the plain one for the
  wallet's prompt) and its stop disarms it. The confirmations are the dashboard's (Settings →
  Network & access → Devices → Device listener): the backend takes the patch as sent, and the
  tool plane refuses `devices` by name as before. `loopback` binds `[::1]` beside `127.0.0.1`
  when the machine has IPv6, else the IPv4 half alone with a warn line. Every start, move and
  stop is published on `DeviceListener::watch` (`Bound {addrs, served_at}`), which the calls'
  device socket group follows.
- *Changed 2026-10-15 (WP2 review, second round):* the CA is opened before the listener's lock is
  taken, since a wrapped key's unlock prompt waits for as long as nobody answers it.
  `server::serve` claims an epoch (writing the first start's port there), races only the CA's
  opening against the gateway's stop, and then starts without a race, since a cut start could
  leave a generation serving; a stop during the prompt returns within `STOP_WITHIN`, nothing
  binds after it, and the disarm takes the epoch back. `disarm` answers whether everything ended
  within the bound.
  There is no backend confirmation flag for the listener's settings: the owner-only save, the
  tool plane's refusal and the dashboard's dialog decide it, as for `bind_addr` and
  `auth_enabled`.

### 4.2 TLS

- **Two `rustls::ServerConfig`s per issued leaf** (§2.2), with the aws-lc-rs provider, TLS 1.3
  only (the client's certificate travels encrypted), ALPN `http/1.1`:
  - **mandatory**: `WebPkiClientVerifier::builder_with_provider(roots = {the CA},
    provider).build()`; a client without a certificate fails the handshake;
  - **optional**: the same with `.allow_unauthenticated()`, for enrolment.
  - The accept loop picks one per handshake: **optional only while a pairing token is pending**
    (the count kept with the tokens, §3.3), mandatory otherwise.
- **No resumption** (D5): `session_storage = NoServerSessionStorage`, `send_tls13_tickets = 0`.
- **HTTP/2** is Later: axum's `http2` feature is off today, and a phone's core pools HTTP/1.1
  connections.
- *Changed 2026-10-15 (WP2, as built):* "a pairing token is pending" is
  `devices::PendingTokens` (a token's device row and expiry), which pairing (WP3) sets when it
  issues a token and ends when it is used or ended; every handshake asks it.
- *Changed 2026-10-15 (WP3):* the set holds the tokens alive, a used one included, and is the
  store's (§3.3's notes).

### 4.3 The accept loop and the serving loop

- **`server::device_listener::TlsListener`** owns its `TcpListener`s (two for `loopback`) and an
  accept task per socket, held by an abort-on-drop handle: dropping the listener stops the accept
  tasks and closes the sockets at once, so a move rebinds the same port without waiting.
- Each accept sets `TCP_NODELAY`, reads the local address, picks the leaf and the configuration
  (§2.2, §4.2), and spawns the handshake under `devices.handshake_timeout_s` (default 10). A
  handshake that fails or runs out of time is logged at debug with the peer and the reason. One
  slow peer never holds another's accept.
- **Serving**: each finished TLS stream is served by hyper-util's `auto::Builder` with a
  `TokioTimer`, HTTP/1's `header_read_timeout` set to `devices.header_timeout_s` (default 30), and
  `serve_connection_with_upgrades` (WebSockets) over the device router (§4.6) with the connection's
  `Conn` (§4.5) as its connect info. hyper 1.10 starts that timer whenever it waits for a request's
  head (`proto/h1/conn.rs`, `poll_read_head`), so it bounds both a slow request and a kept-alive
  connection idle between requests. A connection without a certificate is further bounded as a
  whole by `devices.unauthenticated_timeout_s` (default 10), after which it is closed.
- **Why not `axum::serve`:** it sets no timer and exposes no builder (axum 0.8.9,
  `serve/mod.rs:391`), so it cannot bound headers. The loop is small: an accept channel, hyper-util's
  connection builder, and the stop of §4.4 with hyper-util's graceful shutdown for the drain.
  `axum-server` and `tls-listener` were not chosen: each brings a serve loop and stop of its own
  beside lmgw's.
- *Changed 2026-10-15 (WP2, as built):* each connection is served by hyper's own HTTP/1
  connection (`hyper::server::conn::http1::Builder` with hyper-util's `TokioTimer` and
  `TokioIo`, `.with_upgrades()`), not hyper-util's `auto::Builder`: the auto builder reads the
  first bytes to tell HTTP/2's preface before hyper's header timer starts, so an authenticated
  connection that sends nothing would not be bounded, and it would serve HTTP/2 by prior
  knowledge, which is Later; hyper-util's `GracefulShutdown` has no impl for hyper's upgradeable
  HTTP/1 connection, so the drain is the loop's own (each connection is told, finishes its
  request and closes; past the bound it is aborted and the requests still open are named). The
  unauthenticated bound counts from the accept. The router is called per request with
  `tower::ServiceExt::oneshot`, as `axum::serve` does.
- *Changed 2026-10-15 (WP2 review, second round):* connections that
  have not authenticated are bounded, since each holds a descriptor while it waits and lmgw's
  soft descriptor limit is 1024: at most `devices.unauthenticated_max` (default 64) from every
  peer together and `devices.unauthenticated_per_peer` (default 8) from one peer, an IPv4
  address or an IPv6 /64. "Not authenticated" is a handshake in progress, a connection without a
  certificate, and one whose certificate opens no working device: no row holds its key, or the
  row is disabled, unconfirmed or past its `expires_at` (the expiry is this record's addition
  to the review's list). Such a connection keeps its place, lapses at
  `devices.unauthenticated_timeout_s` and gets `Connection: close` on every answer, while the
  gate still names the reason. **At a bound the oldest goes, not the newcomer:** a new
  connection from a peer at its bound closes that peer's oldest unauthenticated connection, and
  one past the total closes the oldest of all; each such eviction is counted
  (`devices.listener.evicted`) and said in a warn line naming the setting, at most once a minute.
  A stranger then holds places only while it out-paces real handshakes (milliseconds on a LAN):
  a paired device that retries gets through, from the stranger's own address too, though a
  stranger opening connections faster than the device's handshake completes can still delay it.
  A connection gives its place back the moment it authenticates, and only then takes its socket
  handle (§4.5), so an unauthenticated one holds one descriptor. "The whole connection" is
  exact: when the handshake may come without a certificate (a token pending), it runs inside
  `devices.unauthenticated_timeout_s`, counted from the accept. A handshake is raced against the
  listener's stop, so a stop ends it at once instead of waiting it out. A connection that has
  not authenticated is never upgraded: the serving loop strips the upgrade (a WebSocket would
  outlive the place and the lapse), so its route answers as to a request that cannot upgrade;
  no new code is needed, since the gate refuses such a connection on every upgradable route
  first.

### 4.4 A stop per listener

- **Today one generation stops everything** (`Stops`, §What exists). Turning the device listener
  off by that generation would end every stream of the plain listener too, and every later one
  until a restart.
- **So each listener gets its own `Stops`**: the plain listener's, and the device listener's,
  created anew each time the device listener starts. A request carries its listener's generation
  (`ServedAt` gains the listener), and every stream, session, turn and call watches that one.
- **A gateway stop** stops both, with `STOP_WITHIN` shared as today. **The device listener's stop**
  ends its streams (feeds end; realtime sessions and calls close as at a stop), drains its requests
  within the same bound, drops `TlsListener` (§4.3), and leaves the plain listener running.
- *Changed 2026-10-15 (WP2, as built):* one `Stops` holds a generation per listener rather than a
  new `Stops` per start: the device listener's moves at each of its stops, which is the same
  thing (a start's requests carry its generation, and the stop after it ends them). `ServedAt` is
  `{via, at}`; `Stops::now_of`, `stop_of` and `running_on` take the listener, and `now`, `stop`
  and `running` are the plain listener's (what in-process work takes). A request row being
  written is waited for by every stop. A move or an off closes the sockets and moves the
  generation at once and drains in the background; the gateway's stop waits for its drain.

### 4.5 Connection info

- **One connect-info type for both listeners**: `server::Conn { via: Via::Plain | Via::Devices,
  peer, local, device: Option<DevicePeer> }`. For the device listener `device` is read once per
  connection from the TLS session's peer certificate: the SubjectPublicKeyInfo hash and the serial
  (rustls-webpki's `EndEntityCert`), and `notAfter` (x509-parser, since webpki keeps it
  `pub(crate)`). It also carries a handle to the connection's socket options, so the feed can set
  `TCP_USER_TIMEOUT` (§7.1).
- **This extends the calls design's change**, which adds `local` for its candidate. Whichever work
  package lands first introduces `Conn`; `serve_stopping`'s bound becomes `Conn: Connected<…>`, and
  the agent proxy reads `Conn::peer`.
- **`RequestCtx`** gains `via`, so request rows and log lines say which listener a request came in
  on.
- *Changed 2026-10-15 (WP2, as built):* `Conn {via, peer, local, device, socket}`, `peer` and
  `local` canonical (an IPv4 client of `[::]` reads as IPv4). `device` is `DevicePeer
  {spki_sha256, serial, not_after}`, all three read with x509-parser (rustls-webpki stays
  indirect); `DevicePeer::key_hash` is the `spki:` hash a row holds. `socket` is a
  `SocketHandle` on the device listener's authenticated connections only, those whose
  certificate opens a working device (`None` on the plain listener, where no device connects,
  and on every other: a handle costs a descriptor, §4.3's review note): a duplicate of the socket's
  descriptor, closed by the stream's own drop under the lock its setters take, so it never
  reaches a reused descriptor and never holds a closed connection open
  (`set_tcp_user_timeout`, `tcp_user_timeout`, `is_open`). The plain listener serves through
  `server::conn::PlainListener` (which also fires `serve`'s `released`), since axum's `TapIo`
  takes the connect-info impl for its address type. `RequestCtx.via` is in the request span (the
  `via` field of every request's log lines); a `request_logs` column for it needs a migration,
  which is not this work package's.
- *Changed 2026-10-15 (WP3, as built):* the `request_logs` column is `via` (`plain` or `devices`,
  migration 0081), written from `RequestCtx.via` on every row: a `/v1` call's, a free-form one's,
  an MCP tool call's, and an in-process call's through `KeyRef.via`, so the model calls of a
  device's Chat turn say `devices`.

### 4.6 What it serves

- **The device router** is `build_router`'s with a face. On `Via::Devices` a route is served when its
  `CAPABILITY_TABLE` row is `Inference` or `Chat` for the request's method, or is in `DEVICE_ROUTES`
  (`POST /devices/enrol`; `POST /devices/renew` from WP8). Everything else is a 404, as an unknown
  path is. On `Via::Plain`, `DEVICE_ROUTES` are a 404 and everything else is as today. The rule is
  derived, so a new `/v1` or Chat route reaches devices without a second table; the route walk
  checks both faces.
- **No CORS layer** on the device listener: no browser holds a device certificate.
- **Body limit**: `/devices/*` take at most 16 KiB (an enrolment is under 1 KiB), stated in their
  `DocRoute` and refused 413 naming it, instead of `max_body_mb`.
- **Capability rows**: `("POST", "/devices/enrol", Cap::Public)`, because the handler checks the
  token; `/devices/renew` with WP8, as `Cap::Chat` with a device principal required.
- *Changed 2026-10-15 (WP2, as built):* the face is applied by the gate, from the capability the
  route's own `require` declares (`server::serves_on`), before any credential is judged; the
  device router (`build_device_router`) has no CORS layer and no agent-origin dispatch, and a 405
  there is the 404 an unknown path gets. `DEVICE_ROUTES` holds `POST /devices/enrol`;
  `devices::routes` is the `/devices/*` router, merged into both listeners' routers with the
  16 KiB limit (`devices::with_body_limit`, `devices::read_body`, 413 `body_too_large` naming
  it). It holds no route yet: enrolment's handler, its `CAPABILITY_TABLE` row and its `DocRoute`
  come with WP3, which registers it there.
- *Changed 2026-10-15 (WP3, as built):* `devices::routes` holds `POST /devices/enrol`. The body
  limit is the route's own layer (`with_body_limit` takes its `MethodRouter`) inside
  `route_layer(require(Public))`, so the plain listener's 404 comes before a byte of the body counts
  (tested with a body past 16 KiB). The path is a literal in the router, for the route walk that
  reads the source. Its `DocRoute` is under a new `devices` tag of the Device API group, answered
  201 (`Resp::Created`).

### 4.7 The principal

`principal_mw` reads `ConnectInfo<Conn>` first.
- **`Via::Devices` with a certificate:**
  - a presented bearer, `x-api-key`, session cookie or key subprotocol → the refusal
    `credential_on_device_listener` (400, "the device listener authenticates by client
    certificate; send no Authorization header, API key, cookie or key subprotocol"), rendered by the
    route's gate as every pending refusal is;
  - past the certificate's `notAfter` → 401 `device_certificate_expired`, "pair the device again";
  - the row whose `key_hash` is `spki:<hash>`: enabled and confirmed → `Principal::Key` of kind
    `device`, its `fingerprint` the head of that hash; not confirmed → 403 `device_unconfirmed` with
    the code; disabled → `device_disabled`; no row → `device_key_unknown`.
- **`Via::Devices` without a certificate:** `Anonymous`. Only enrolment is served: any other route
  answers 401 `client_certificate_required` ("this route needs the device's certificate; pair the
  device on lmgw's Devices card") whatever *Require API key* says, writes **no `request_logs` row**
  (the gate skips `policy_refusal`'s recording for it), and every response carries `Connection:
  close`. Enrolment's `Public` gate ignores a credential header there.
- **`Via::Plain`:** as today, without device keys (§4.8).
- **After it, nothing changes:** the gate's `Chat` arm (row and expiry), `admit_session`, policy
  per call, the revocation watch by key id (now also waking at the certificate's `notAfter`, §5.1),
  `LinkGuard` and `last_seen_at`.
- *Changed 2026-10-15 (WP2, as built):* `principal_mw` and the gate moved to `server/gate.rs`,
  the device listener's half to `server/device_gate.rs`. The checks run in this order: a
  credential (any `Authorization` scheme, `x-api-key`, `x-lmgw-admin-token`, the session cookie,
  a key subprotocol); `notAfter`; the row the key opens, through the seam `devices::KeyLookup`:
  none is `device_key_unknown`, a disabled one `device_disabled` (before the confirmation: a
  disabled device is not told to wait for one), an unconfirmed one 403 `device_unconfirmed`. The
  gateway's lookup (`devices::StoredKeys`) finds the row by `key_hash` and **fails closed on the
  confirmation**: no column holds one before WP3's migration, so every row it finds reads as
  unconfirmed; WP3 reads `confirmed_at` there. The suites install `devices::MemoryKeys`. Without
  a certificate, `client_certificate_required` is answered in the route's dialect (`/v1`'s
  envelope on `Inference`, the flat shape elsewhere) without a request row.
- *Changed 2026-10-15 (WP3, as built):* `StoredKeys` reads the row's `confirmed_at`. A request
  resolved from a certificate carries its `notAfter` (`RequestCtx.cert_not_after`), which every
  revocation watch opened for it takes (§5.1).

### 4.8 The plain listener and device credentials

- With bearer device keys gone (D12, Q3), no device is authenticated on the plain listener. A
  `lmgw-device-` bearer matches no row (every device row's hash is prefixed) and is answered 401
  `device_key_unknown`, from loopback and elsewhere alike. When it matches a `v1:` row, the message
  is "device '<name>' was paired with a link from before TLS, which this lmgw no longer accepts —
  pair it again on Usage → Keys → Devices".
- *Changed after the owner's answers, 2026-10-14:* Q3 is answered no, so the plain listener keeps
  no loopback path for device bearers. The alternative this bullet held (a device bearer accepted
  from a loopback peer only, `device_needs_tls` from any other) is gone, and with it the squatting
  exposure of D12.
- *Changed 2026-10-15 (WP3, as built):* no bearer is compared with a device row
  (`principal::matching_row` skips the kind, beside `internal`). A `lmgw-device-` bearer is 401
  `device_key_unknown`, with the v1 message naming the device when a `v1:` row holds its hash; a
  `Public` route drops it as it drops every pending refusal.

## 5. Revocation, re-pairing, expiry, rotation

### 5.1 Revocation: unchanged signals, new door

- **Disable, Delete and the policy's `expires_at`** act as client-apps §1.6 built them: the
  revocation signal by key id ends the feed with `revoked {kind}`, the realtime WebSocket and
  `/mcp/host` with 4003 and the kind's token, a call with `lmgw.session.closed {kind: revoked}`
  (calls), and Chat streams with their `revoked` frame; and the next request on any connection is
  refused by the row (§4.7).
- **The certificate's `notAfter`** joins the watch: a connection's watch also wakes at its
  certificate's date and ends the link with the new kind `certificate_expired` (`RevokeKind`, its
  token on a 4003, the feed's `revoked.kind`), which a client reads as "pair again".
- **Delete** also deletes the device's token row and forgets its minted link.
- **The TLS connection itself is not cut.** A kept-alive connection of a revoked device gets each
  request refused with the reason, which tells the client more than a reset would, until its idle
  bound (§4.3) closes it.
- *Changed 2026-10-15 (WP3, as built):* `RevokeKind::CertificateExpired` (`certificate_expired`)
  and `RevokeReason::CertificateExpired`, never raised as a signal: each watch wakes at its own
  certificate's `notAfter` (`RequestCtx.cert_not_after`), by the wall clock a key's expiry uses,
  and ends its link with "the certificate of device 'phone' expired — pair it again". A device's
  `Rotated` reads "the key of device 'phone' was replaced — pair it again" (a new token that ends
  the key, an enrolment that replaced it, the CA's rotation), its kind still `key_unknown`.
  `lmgw-client`'s close test reads the new kind.

### 5.2 Re-pairing replaces the key

- **New token…** on an enrolled row asks: "Keep the current key until the new token is used and
  confirmed" (a move to a new phone) or "End the current key now". The second raises `Rotated` at
  once and sets `key_hash = pending:…`, as a stolen key needs.
- **The enrolment that uses the token** replaces `key_hash` and raises `Rotated` for the old key;
  the new key waits for its confirmation (§3.5). The old key's connections end with the kind
  `key_unknown` (pair again). The device that just re-paired has dropped its old connections before
  enrolling (kai §3.1's order), so it sees none of that.
  *Changed 2026-10-15 (WP3 review, the owner's decision, following §3.5 and §5.3):* the enrolment
  ends no working key. The same key is a renewal: a fresh certificate, its confirmation kept, no
  `Rotated`, nothing closed. A new key waits beside the kept, confirmed one (§3.6's waiting key),
  which works on until the owner confirms the new key's code; that Confirm makes the new key the
  row's and raises `Rotated` for the kept one, whose connections then end with `key_unknown`. "End
  the current key now" behaves as before, and the key the device enrols next takes the row
  unconfirmed. A new token drops a key still waiting from an earlier one.
- **`key_rotate`** answers a device row 400 naming **New token…** (`device_token`): a device has no
  secret of lmgw's making to rotate.
- *Changed 2026-10-15 (WP3, as built):* `device_token {id, end_current?, local_only?, urls?}`:
  `end_current` defaults to `false` (keep the current key); `true` makes the row's key `pending:…`,
  clears its certificate and confirmation columns and raises `Rotated` at once. `local_only` and
  `urls` replace the row's; a row with no URLs (one paired before TLS) gets the proposal for its
  `local_only`. The new link replaces the device's minted one. `key_rotate` on a device row is 400
  naming `device_token`, and `KeyRotateAnswer` is gone: `key_rotate` answers `KeySecret`.
- *Changed 2026-10-15 (WP3 review):* `end_current` on a row paired before TLS turns it to
  `pending:` and says there was no key to end (its bearer opens nothing), not that connections
  closed; on an enrolled key never confirmed, the answer says the next enrolment replaces it.

### 5.3 Expiry in the first usable version

- The card shows per device "certificate until 2027-10-14", and from `notice_after` on, in amber:
  "lapses on 2027-10-14: pair it again before then". The client shows the same notice from its
  stored `notice_after` (`lmgw-client`'s `Credentials::state`), and the feed's `hello` carries the
  device's own `cert: {not_after, notice_after}`.
- **Re-pairing renews**: New token… with "keep the current key until the new token is used" moves
  the device to a fresh certificate without a gap.
  *Changed 2026-10-15 (WP3 review):* as built now (§5.2's note): the same key enrolling keeps its
  confirmation and every link, and a new key works from its Confirm, the kept one until then.
- A lapsed certificate fails at the next handshake (TLS's `certificate_expired` alert) and ends open
  links (§5.1); the device knows its own date and says "this device's certificate lapsed on <date>;
  pair it again".

### 5.4 Silent renewal (WP8, after the first usable version)

```
POST /devices/renew        device listener, with the device's certificate; body at most 16 KiB
{"client": "Kai mobile 0.1.3 (Android 16)"}
200 OK — the enrolment's answer (without the confirmation fields), a new certificate for the same key
```

- For the connection's own key: a new serial and dates, `client_info` updated; the old certificate
  is not withdrawn (the same key, so only the device can use either). No confirmation: the key did
  not change.
- The device renews at its first request after `notice_after`; a failed renewal is retried at its
  next connection and said in its state. The card's notice then reads "renews by itself".
- Refusals: the device listener's (§4.7) and 503 `devices_ca_unavailable`.
- *Note 2026-10-14 (WP6):* the answer has no confirmation fields, so `lmgw-client` reads it with a
  type and reader of its own, not as `Enrolment`.

### 5.5 Rotating the CA

- **Settings → Network & access → Devices → Rotate the device CA…**, with a confirmation that
  names every paired device: "Every paired device (phone, desktop) stops connecting at once and
  must be paired again with a new token. Rotate only if lmgw's data directory, or its wallet, may
  have been copied."
- **Then, in order:** a new key and certificate replace `tls/ca.key` (wrapped as D2 says, with a
  new wrapping key) and `tls/ca.crt` atomically; the old key is not kept, since one that may have
  leaked serves nobody afterwards. In one write transaction every enrolled device row gets
  `key_hash = pending:…`, its certificate and confirmation columns cleared, its policy kept.
  `Rotated` is raised for each, the leaf cache is cleared, and the card shows each device with
  **New token…**.
- **A device after the rotation** fails to verify lmgw (an unknown issuer) before it sends its
  certificate; `lmgw-client` reads that as "lmgw's device authority changed, or this is not your
  lmgw: pair again".
- **Server certificates rotate by themselves** (§2.2) and never need this.
- *Changed 2026-10-15 (WP3, as built):* `device_ca_rotate {}` is refused 400 while no CA exists.
  `Trust::replace` writes the new CA (a failure before the key file is replaced changes nothing and
  is 503 `devices_ca_unavailable`); one transaction then resets every row holding an `spki:` key and
  ends every token alive, whose link pins the old CA; `Rotated` is raised and `keys: rotated` said
  for each, and every minted link is forgotten. Rows paired before TLS stay as they are (they need a
  new token anyway). The answer is `DeviceCaRotated {fp, devices_reset, message}`, `devices_reset`
  the names without `device:`.

## 6. The device side

### 6.1 Android (Kai mobile)

- **The key**: `KeyPairGenerator` for `EC` in `AndroidKeyStore`, its `KeyGenParameterSpec`
  `PURPOSE_SIGN`, curve `secp256r1`, digest `SHA-256`; no user-authentication requirement and no
  `setUnlockedDeviceRequired`, since the feed's service connects while the phone is locked; in the
  TEE, not StrongBox, because a handshake signs on the path of every new connection, a call's
  included, and StrongBox is slow. The key is non-extractable; its public half,
  `PublicKey.getEncoded()`, is the DER SubjectPublicKeyInfo enrolment sends.
- **Signing in TLS**: the Rust core implements rustls's `sign::SigningKey` (scheme
  `ECDSA_NISTP256_SHA256`, `public_key()` returning the SubjectPublicKeyInfo) with a `Signer` whose
  `sign(message)` calls a UniFFI callback into Kotlin. Kotlin runs `Signature("SHA256withECDSA")`
  over the raw handshake message with the Keystore key and returns its DER signature, which is
  what TLS 1.3 wants for that scheme. `Signer::sign` is synchronous, so the callback blocks one
  tokio worker for the length of one Keystore signature per handshake, which is acceptable.
- **The provider**: Kai mobile's core uses rustls with ring, which spares building aws-lc-sys with
  the NDK; `lmgw-client`'s `tls` feature takes the provider from the app (§6.3).
- **What the core stores**: the certificate, the CA certificate (only if its SubjectPublicKeyInfo
  hashes to `fp`), `not_after`, `notice_after`, the confirmation code and the URL list, in its
  settings file. None of it is secret. Forget gateway deletes the Keystore entry.

### 6.2 Kai desktop

- **The key**: a software P-256 key generated with aws-lc-rs. Its PKCS#8 bytes go into Kai's
  existing Secret Service item (kai §3.1: default collection, attributes `application`, `lmgw`,
  `kind`), whose kind becomes `device-tls-key`. A locked wallet keeps Kai from connecting, as it
  keeps it from reading the bearer today.
- **The certificate, CA certificate, dates and URL list** go into Kai's config directory (public
  data).
- **Local and remote alike**: Kai talks to the device listener with its certificate. When lmgw runs
  on the same machine, the link's URL is `https://127.0.0.1:<device port>` ("this computer"). The
  feed and every request (reqwest 0.13's `tls_backend_preconfigured`; `use_preconfigured_tls` is its
  deprecated alias), the realtime WebSocket and `/mcp/host` (tokio-tungstenite 0.30's
  `Connector::Rustls(Arc<ClientConfig>)`) all come from one constructor that takes the one
  `rustls::ClientConfig` `lmgw-client`'s `tls` feature builds, and a test checks that each refuses
  a server under another CA.
- **What goes**: the bearer header on every request, `DeviceKey` and `plain_http_remote` (links are
  `https` only), and `verify_fingerprint`'s fail-closed stub, which the pinned enrolment verifier
  replaces.

### 6.3 `lmgw-client`

- **`pairing`** (kai-mobile P4, now for v2): `PairingLink::parse` with §3.1's rules,
  `Fingerprint`, `PairingToken` (redacted `Debug`, no `Display`), `LinkError`, `normalize_base_url`
  (https only), `PairFacts` and `display_name`, moved from `kai/src/pairing.rs` with its tests. `url`
  2 joins the crate's dependencies; it builds for wasm and is in the lock.
- **Requests** (sans-IO, as the rest): `requests::enrol(token, spki_der, client)`;
  `read_enrolment`, which refuses an answer whose `ca_certificate` does not hash to the link's `fp`
  and drops non-`https` URLs, giving `Credentials { device_id, certificate, ca_certificate,
  not_after_ms, notice_after_ms, urls, confirm_code }` and `Credentials::state(now_ms)` (valid,
  notice, lapsed); `refresh_urls` with the same `https` rule for `hello.urls`; the refusal codes of
  §3.4 and §4.7 through `read_refusal`, with `key_refused` gaining `CertificateRequired`,
  `CredentialOnDeviceListener`, `Unconfirmed`, `CertificateExpired` and `TokenUsedElsewhere`
  ("someone else used this code: delete the device on lmgw").
- **The URL order**: `Urls::order(last_good)` gives the URL that last connected first, then the
  link's order. Clients race them, starting the next one a stagger they own after the previous one
  if no TCP answer has come; the first handshake that verifies wins.
- **Feature `tls`** (non-default; rustls 0.23 with no provider feature of its own, so the app
  brings one; wasm builds leave it off):
  - `EnrolVerifier { fp }`, a `ServerCertVerifier` that finds, among the certificates lmgw sent,
    the one whose SubjectPublicKeyInfo hashes to `fp`, then delegates to `WebPkiServerVerifier`
    with that certificate as the only root: chain, dates, `serverAuth`, and the URL's host against
    the SANs. No such certificate means "not the lmgw this link pairs with";
  - `client_config(provider, credentials, signing_key)`: `WebPkiServerVerifier` over the stored CA,
    and a client certificate resolver for the stored certificate with the app's
    `Arc<dyn SigningKey>`. TLS 1.3 only;
  - tests run both verifiers against certificates rcgen makes in the test.
- **The feed**: `requests::feed` takes an optional `keepalive_s`; `FeedEvent` gains `CaughtUp`, and
  `Hello` gains `urls`, `cert` and `open_approvals` (§7).
- *Changed 2026-10-14 (WP6), as built:* `Credentials` holds the device's id and DER bytes, its dates
  as unix milliseconds (it read `{ certificate, ca_certificate, not_after, notice_after, urls,
  confirm_code }`), and `read_enrolment(status, body, link, public_key)` also refuses a
  certificate that is not for the sent key and a `confirm_code` that is not that key's.
  `Unconfirmed` carries no code (§3.5). The `tls` feature adds `enrol_config(provider, fp)`
  (`EnrolVerifier::new` takes the provider too) and `TlsRefusal`: read with `of_enrolment` at
  enrolment, where lmgw's `certificate_required` alert is `LinkEnded` (an ended token, §3.3) and a
  chain that does not reach the pinned key, the real CA beside a foreign leaf included, is
  `NotPairedLmgw`; read with `of` on every later connection.
- *Changed 2026-10-14 (WP5's client part), as built:* `requests::feed(cursor, keepalive_s)` puts
  the value in the query, and `FeedReader::asking(keepalive_s)` has `resume()` ask for it again.
  `FeedReader` keeps the open set (`open_approvals()`, `caught_up()`) and yields each change as
  `FeedItem::Approvals(ApprovalsChange)` right after the event that made it: `Open(set)` after
  `caught_up`, then `Added` and `Withdrawn(ids)`, so a client that handles a pushed chunk's items in
  order knows which `approval.*` counted. Besides `approval.decided`, the deletion of a call's thread
  (`thread.deleted`, a gone `thread.*`) or its reply (`message.deleted`) withdraws it, since neither
  records a decision. `caught_up`'s cursor also moves the reader's resume cursor. `hello.urls`,
  `hello.cert`, `refresh_urls` on them and the RevokeKind `certificate_expired` wait for WP3.
- *Changed 2026-10-15 (WP5's rest, as built):* `Hello` gains `urls`, `cert` and `dashboard_url` (§7.2)
  and the reader exposes them as `FeedReader::urls()`, `certificate()` (`CertDates`, with the
  `state(now_ms)` of `Credentials::state`) and `dashboard_url()`; each `hello` replaces the last.

### 6.4 What changes in kai-mobile

This record does not edit kai-mobile's design. Where that design (draft v3) conflicts with this
one:
*Changed 2026-10-14 (consistency pass):* kai-mobile design v4 resolves all 13 items, in its appendix.

1. **M16, §5.4 and §12** run the feed only while the app is visible or a call runs ("No background
   work in v1 … no push"); ruling 5 has Kai mobile hold it in its own foreground service.
2. **Q11** recommends UnifiedPush later, as do its Later list, §16's "Later on lmgw's side" and
   "What exists today"'s "Not built: … UnifiedPush"; ruling 5 rules UnifiedPush out.
3. **M10 and §5.4** start the feed at "now" on every start of the core; a feed held in the
   background needs a persisted cursor (resuming with `Last-Event-ID` across process restarts), and
   `hello.open_approvals` (§7.3) for what was open while it was away.
4. **M5 and §5.2**: the VPN-only `networkIgnoreMask` breaks a call whose POST went to a LAN URL,
   because lmgw's one candidate is then the LAN address (§8); the tunnel gate ("refuses to connect
   without it"), the tunnel's identification and the process binding lose their reason with
   ruling 4 and D13, and `dev.allow_untunnelled` (§3.4) with them.
5. **M7 and §4.4** refuse every link with `fp` and define `fp` as the leaf certificate's SHA-256;
   here `fp` is required and is the CA's SubjectPublicKeyInfo hash (§2.1), and TLS is v1.
6. **P4** moves `DeviceKey`, `verify_fingerprint` and `plain_http_remote` into `lmgw-client` with a
   strict v=1 parser (§4.1 step 3); here they are replaced by `PairingToken`, the enrolment verifier
   and https-only URLs, and the parser reads v=2 (§6.3).
7. **M6, §3.3 and §4.3**: the bearer key encrypted with a Keystore AES-GCM key into
   `credentials.bin`, crossing the FFI twice; here the credential is a non-extractable Keystore
   signing key, and what the core stores is public (§6.1).
8. **M8 and §5.3**: the "WireGuard off" and "another VPN" states are no longer gates; new states
   are "waiting for confirmation (code)", "someone else used this code", "certificate lapses on
   <date>" and "certificate lapsed".
9. **Q2 and §5.1** (the tunnel at home, NAT hairpinning, no release switch that sends the key
   outside the tunnel, WP0 (k)) relax: the LAN URL serves at home, and nothing secret leaves the
   phone on any network.
10. **§4.2**: pairing's probe ("checks that a tunnel is up, probes the feed with the key", records
    the tunnel's facts) becomes enrolment, the confirmation and the first mTLS feed.
11. **§5.5**: the refusal kinds gain `client_certificate_required`, `credential_on_device_listener`,
    `device_unconfirmed`, `device_certificate_expired` and the RevokeKind `certificate_expired`
    (carried under kind `revoked`).
12. **§13**: the host link authenticates by the certificate, not "the device key"; "offline is
    normal … until push or a background service" changes with the held feed.
13. **§16**: P3's QR code is this design's WP4; P4 changes as item 6 says; "TLS on the listener and
    the link's `fp`" and push leave lmgw's Later list.

## 7. The feed as push channel

### 7.1 The keep-alive is the stream's own

- **`GET /chat/api/feed?keepalive_s=<n>`**: n from 1 to `chat_feed_keepalive_max_s` (Settings →
  Chat → Change feed, default 900). Without it a stream uses `chat_feed_keepalive_s`, as today. Out
  of range is 400 `bad_request`, naming the setting. `hello.keepalive_s` states the stream's value.
- **The tick splits in two.** The keep-alive comment (with the cursor's `id:` for a device,
  client-apps §2.4's W4-15) goes out at the stream's interval. The table backstop and the
  snapshot-behind wait keep the global `chat_feed_keepalive_s`, so a stream at 900 s still reads
  the table every 15 s and delays no event, only its comments.
- **`TCP_USER_TIMEOUT`**: when a device's feed opens, lmgw sets it on the connection's socket to the
  stream's keep-alive (`Conn`'s socket handle, socket2's `set_tcp_user_timeout`). A keep-alive that
  a vanished phone never acknowledges then fails within one more interval, the stream ends, and the
  card stops showing the device online.
- **Why set by the device and not by the owner per device:** the client knows its situation (a
  visible app, a background service, a desktop on loopback); a per-row setting would be a value the
  owner has to know and set for each device. The card shows what each stream asked (§9.3).
- **Why a maximum:** between events a stream sends only its comment, so the maximum bounds how long
  a dead stream can last on lmgw before `TCP_USER_TIMEOUT` ends it.
- **Why long values are fine inside WireGuard or TLS:** inside WireGuard the TCP connection runs
  between tunnel addresses and crosses no NAT, so nothing on its path forgets it, and the home LAN
  has none either. WireGuard's own `PersistentKeepalive`, on the phone's peer, keeps the outer UDP
  mapping through a carrier's NAT; lmgw's pushes need that mapping anyway, and no inner keep-alive
  could keep it. TLS has no idle timeout of its own, and a feed is one request in progress, which
  `devices.header_timeout_s` does not touch. So the inner keep-alive only measures how soon the two
  ends notice a silent drop, and the phone reconnects at once on its own network callbacks.
- **For a phone in the background, this design suggests 120 s.** With WireGuard's keep-alive at
  the usual 25 s the radio wakes anyway, and one more wake every two minutes is a small share of
  it; a silent drop is noticed within about four minutes. While the app is visible the phone may
  open its feed at 15 s; reopening costs a handshake and resumes by cursor (§7.2). The value is Kai
  mobile's setting, measured in its WP0.
- *Changed 2026-10-14 (WP5), as built:* `keepalive_s` outside 1 to the maximum, 0 and negatives
  included, is the 400 `bad_request` "keepalive_s N is out of range: a feed may ask for a keep-alive
  from 1 to M seconds (chat_feed_keepalive_max_s, Settings → Chat → Change feed → Longest
  keep-alive)"; one that is no number is the query's own 400. The setting's own range is 1 to
  `lmgw_api_types::chat_feed::MAX_KEEPALIVE_S` (86 400), refused past it by name and clamped on load
  like its neighbours, in the dashboard, the tool plane's patch and its schema. A device feed that
  waits for its levels to be published before `hello` (client-apps §2.1's 2026-10-08 note) waits
  up to one gateway keep-alive, as the snapshot-behind wait does. **Not built:** `TCP_USER_TIMEOUT`,
  which needs WP2's `Conn` and its socket handle; `web/chat_feed.rs`'s `open` marks the place, beside
  `devices::connect`, where WP3's `Links` details (keep-alive, peer, start) also go.
- *Changed 2026-10-15 (WP5, as built):* `TCP_USER_TIMEOUT` is built. A device feed's `open` sets it
  to the stream's `keepalive_s` through `ctx.conn`'s `SocketHandle` (the device gate puts the
  connection on the request context for a device the certificate opened; the plain listener's
  requests carry none, so an owner's feed sets nothing). A failure is logged, never a refusal. The
  option exists on Linux and Android; elsewhere the call is a no-op behind `cfg`. A test finds the
  sockets of the process whose option is the stream's value.

### 7.2 Reconnect and catch-up: what lmgw guarantees

As built (client-apps §2.1 to §2.4), restated for a device that drops often:
- **A cursor resumes exactly.** `Last-Event-ID` (or `?since=`) resumes after the last stored event
  the device received, and every stored record since is sent in commit order, page by page. Records
  the device may not see are skipped, and its keep-alive `id:` carries the cursor past them.
- **`resync {reason}`** when the cursor cannot be honoured (another database, older than
  `chat_feed_retention_days`, newer than the newest, a restored copy). The client reloads what it
  shows; nothing is skipped silently.
- **Live events are not replayed.** `turn.*`, `voice.*`, `hold` and `state` are live only; a
  reconnect's `hello` carries the live state now.
- **Retention**: `hello.retention_days` says how long a device may stay away and still resume.
- *Changed 2026-10-15 (WP5, as built):* a device's `hello` also says what lmgw holds about the
  device itself, read from its row in the snapshot the feed opens with:
  - **`urls`**: the row's list, `https` base URLs in the owner's order; empty on an owner's feed.
    `lmgw-client` reads it with `refresh_urls` (`FeedReader::urls()`: the `https` ones, each once;
    `None` when none is usable, and the client keeps the list it has).
  - **`cert`**: `{not_after, notice_after}`, RFC 3339 UTC, the dates of the certificate the row
    holds; `null` on an owner's feed. `FeedReader::certificate()` gives `CertDates` in unix
    milliseconds, whose `state(now_ms)` is `Credentials::state`'s answer (`Credentials::dates()`).
  - **`dashboard_url`** (the owner's decision, 2026-10-15): `null`, or the plain listener's
    dashboard as a URL derived from `bind_addr`, for a client that offers "Open in lmgw". A device
    paired as "this computer" (the row's `local_only`, which WP3 stores from `key_create`; the URL
    list being loopback only is not tested, since an owner may list 127.0.0.1 for a remote row)
    always gets it: the configured address, a wildcard bind read as `127.0.0.1`. Any other device
    gets it only when `bind_addr` is not loopback: a concrete address is itself; a wildcard bind is
    the address the device's own connection arrived on (`Conn::local`), because the dashboard
    answers on the same host as the device listener; none when that is loopback too. A host name is
    passed through, `localhost` excepted. `FeedReader::dashboard_url()` reads it. It is the
    dashboard's address and not a login: the device still has none, and the owner signs in there as
    on any other machine.

### 7.3 `open_approvals` and `caught_up`

- **`hello.open_approvals`**: every approval request still pending in a thread within the
  principal's reach, as `approval.requested` renders it with `thread_title`. It is read in the
  transaction that reads the feed's head (`store::feed::bounds_and_switch`, which already reads the
  head and the reach together), so it is the open set at that head. Without it a device away longer
  than the retention, or one that starts from "now", had no way to learn what waits: no route lists
  open approvals.
- **`event: caught_up`**, data `{cursor}`, live (no `id`), once per stream: sent when the stream has
  delivered or skipped every stored record up to that head (`caught_up_at`); right after `hello`
  (and the `resync` or profile list a cursor may owe) when there is nothing to catch up.
- **How a client combines them** (`lmgw-client`'s `FeedReader` keeps the set): the open set starts
  as `hello.open_approvals`, keyed by `approval_request_id`. `approval.*` records in the catch-up
  happened at or before the head, so they are already reflected in that set and change nothing.
  After `caught_up`, `approval.requested` adds and `approval.decided` removes. A phone notifies for
  the open set at `caught_up` and for each addition after it, and withdraws on each removal; a
  catch-up never raises a notification only to withdraw it two records later.
- Typed in `lmgw-api-types::chat_feed` and in `lmgw-client`, and listed in the feed's `DocRoute`.
  Kai desktop's reader logs and skips `caught_up` until it uses it.
- *Changed 2026-10-14 (WP5), as built:*
  - **"Still pending" is a thread's last message's open calls.** Only that reply can be decided
    (`claim_approvals` refuses an older one as moved on, and a new message declines what its
    predecessor held), and `GET /chat/api/threads/{id}` lists the same calls on it. The read finds
    each thread's last message by the `(thread_id, id)` index, so it costs one message row per
    thread (`store::feed::opening`, in the transaction that reads the head and the levels). The
    entries are ordered by reply, oldest first, each reply's calls in the model's order.
  - **`caught_up`'s cursor** is where the stream stands when it is sent: the head, or past it when
    a prune's `resync` moved the stream on during the catch-up. It is sent once per stream, owner
    streams included, and before any record written after the head.
  - *Changed 2026-10-15 (review of WP5, S1):* a `FeedReader` lives for one connection, so `Open`
    lists every open call again at every reconnect, and a call decided while the link was down is
    only absent from it (its `approval.decided` came before `caught_up`). So `Open` is the whole set
    to reconcile against: raise what is not shown, withdraw what is shown but absent, and keep the
    ids the user dismissed so they are not raised again. `FeedReader::knowing(known_ids)` does that
    comparison itself and yields `Withdrawn` and `Added` instead of `Open`. An `approval.requested`
    whose call is decided by delivery is not sent (its decision follows), so no client raises and
    withdraws it in one breath.
  - *Changed 2026-10-15 (owner):* the open set is re-sent after a reach move or a resync instead of
    being documented as a limit. A thread that comes into a device's reach after `caught_up` (its
    admin-tools level raised, the toolset taken off the thread or its folder) brings no
    `approval.requested` for calls already waiting in it, and a `resync` that passed records may
    have passed `approval.*` ones. So every `state` now carries `open_approvals`, read when it is
    sent at the reach then (`null` when the read failed: keep the set), and the feed sends a `state`
    after every reach move (as before), after a record that brought a thread or folder into a
    device's reach after `caught_up`, after a `resync` that passed a record after the head, and
    after a prune's `resync`; that one goes out before `caught_up`, so the set `caught_up` hands over
    is the true one. `hello`'s set covers everything up to the head, so the catch-up needs none.
    The reader replaces its set with a `state`'s: silently before `caught_up`, after it as
    `Withdrawn` and `Added` by id, so a set read a little past the stream's cursor is safe; a reach
    lost withdraws by the `thread.deleted` it brings. `FeedReader::knowing` reconciles against the
    replaced set at `caught_up`. *Changed 2026-10-15 (re-review):* a `state` sent because the set
    may have moved never goes out with `null`: when the read fails the stream ends, and the
    reconnect's `hello` carries the set (only a lag's, a resize's or a refresh's `state` says `null`,
    and the reader keeps its set). The prune's `state` starts the live side over as a lag does,
    keeping a dropped lag `state`'s reason. `Added` can bring back a call the user dismissed (it
    left the reach and came back), so clients filter it by their dismissed ids as they do `Open`.

### 7.4 Events a notification can be built from

- **`approval.requested`** gains `thread_title`, the thread's title at delivery. With
  `server_label`, `name` and `arguments` it renders "Kai wants to run desktop__open_file
  (path: …) in 'Kai'" without a fetch. `approval.decided` withdraws it.
- **`message.added`** (client-apps WP10, not built) renders `{thread_id, messages}` as
  `GET /chat/api/threads/{id}` returns them, so it carries the reply's text whole, and gains
  `thread_title`. A phone notifies from an assistant message in a thread it follows while the app
  is not visible; `message.updated` replaces the text of a notification still shown.
- **Both are stored events**, so a phone that was offline gets them in its catch-up.
- **`turn.done`** carries no text and stays as it is: a notification waits for the message.
- **A push never starts voice.** Android's microphone service is a while-in-use foreground
  service: Kai mobile starts it from a visible activity or an assistant session, never from its feed
  service in the background, and a voice session starts only by the owner's act (kai-mobile M11).
  So the feed raises a notification, and its tap brings the app forward and starts the call.

### 7.5 What stays the client's

Holding the feed in a foreground service, its type and its notification, the persisted cursor,
reconnecting on network callbacks, and which events notify are Kai mobile's design; so is whether
the socket survives Doze on a typical Android phone (its WP0). lmgw's side holds either way: a stream
that drops resumes by cursor, and `hello` says what is open.

## 8. Calls

- **The candidate rule carries over unchanged** (calls C6). A phone now POSTs to the device
  listener, so its candidate is the device listener's local address, LAN or WireGuard, whichever
  URL answered; `Conn::local` (§4.5) is where the calls design reads it.
- **The SDP now travels inside mTLS.** The calls design's security reasoning (C18) relied on
  WireGuard to protect the DTLS fingerprints in the offer and answer, and named TLS on the listener
  as what closes that exposure on a bare LAN. This design is that TLS: lmgw's DTLS fingerprint
  reaches a phone only inside a connection the phone verified against `fp`, and the phone's reaches
  lmgw only inside a connection lmgw verified against the phone's key.
- **The UDP socket follows the device listener live** (Q2). The device listener turns on at the
  first pairing, without a restart, and a socket that binds only at start would fail the first call
  after it; and with `bind_addr` on loopback, a socket on `bind_addr`'s IP has nothing behind a
  phone's candidate. So while the device listener runs, the UDP socket binds where it binds
  (`127.0.0.1` and `[::1]` for `loopback`, `[::]` for `all`, the address for a concrete one) on the
  device listener's port number, and every change of the device listener (on, off, scope, port)
  rebinds the UDP socket at once. The live calls end under the same confirmation, closing as at a
  stop. A wildcard UDP socket adds no exposure the calls design has not weighed: it answers only
  checks that carry a live call's credentials and arrive at its candidate.
  *Changed 2026-10-14:* the calls design does this per socket group (its C4, §3.1). Calls POSTed
  to the device listener use the **device group**: one UDP socket per address the device listener
  binds (two for the `loopback` pair, one for `[::]`) on its port number. The group follows the
  device listener live, and a change closes only its sockets and ends only device calls, with
  `lmgw.session.closed {kind: shutting_down}` under this listener's confirmation. Calls POSTed to
  the plain listener use the **plain group**: one socket per address of `bind_addr` on
  `webrtc_port` (by default the HTTP port's number), applied on restart. A device listener change
  never ends them, as §4.4 has it. A call uses the socket of its group whose address matches its
  candidate. One driver task reads both groups.

**What the calls design changes** (its writer carries this into that record; *carried into the
calls design on 2026-10-14*, after the owner's answers):
1. **C5, §3.1 and §9's `webrtc_port` row:** while `devices.listener` runs, `webrtc_port: null`
   means the device listener's port number, and the socket binds the device listener's addresses
   (D24's `loopback` pair, `[::]` for `all`, or its concrete address); with the device listener off,
   C5's rule (the HTTP port's number on `bind_addr`'s IP) stands. An explicit `webrtc_port` keeps
   its number either way. *Changed 2026-10-14 (consistency pass):* the last sentence is struck:
   `webrtc_port` is the plain group's port only (calls §3.1, §9).
2. **§3.1's "the port and the switch apply on restart":** the address and port follow the device
   listener **live**: each of its changes closes the UDP socket (its calls end with
   `lmgw.session.closed {kind: shutting_down}`, and the confirmation names them) and binds the new
   one. `webrtc_enabled` may stay a restart setting. The restart path's release wait covers both
   sockets.
3. **§3.2:** `Conn { via, peer, local, device }` is the shared connect-info type (§4.5); the
   sentence "TLS on lmgw itself is on the Later list" points here instead.
4. **§2.3 (credentials):** on the device listener the principal is the certificate's device;
   devices present no key anywhere; an anonymous call, the cookie and the `Origin` rule do not
   arise there (no certificate, no route but enrolment; no CORS). The plain listener keeps §2.3 as
   written for stock clients.
5. **C18 and §10's first bullet:** device calls exchange their SDP inside mTLS, so the exposure is
   closed for them; a stock client on a non-loopback plain listener keeps it.
6. **§12 ("Reaching lmgw from the phone needs `bind_addr` on the WireGuard address"):** the phone
   reaches the device listener; the firewall opens TCP and UDP of the device port.
7. **§9's status and settings help:** the `webrtc` block says which rule bound the socket (the
   device listener's or `bind_addr`'s) and its addresses; the firewall hint names the device port.
8. **WP2 and §13.1:** the live rebind and its tests (a call POSTed on the device listener gets the
   device listener's address as candidate; moving the device listener ends a live call and the next
   call connects on the new socket; turning it off falls back to C5's rule). *Changed 2026-10-14 (consistency pass):* "turning it
   off falls back to C5's rule" is struck: device calls have no fallback. Turning the device
   listener off closes the device group, and the plain group is unaffected.

*Changed 2026-10-14:* the calls design applies the list per socket group (above). Item 1: the
device group takes the device listener's addresses and port, and `webrtc_port` is the plain
group's alone. Item 2: a change of the device listener moves the device group and ends device calls
only, and the release wait covers the TCP listeners and both groups. Item 7: the status block and
the firewall hint show both groups; the hint names the device port (TCP and UDP) for phones and the
plain group's port only for a non-loopback `bind_addr`. Item 8: the device group and its live
rebind are the calls design's WP7 (its WP2 builds the driver and the plain group). Its tests check
that a device listener change leaves a stock call on the plain group running, and that turning the
listener off closes the device group while the plain group serves on.

*Changed 2026-10-15 (calls WP7, as built):* the device group follows `DeviceListener::watch` from
the listener's first arm on, closing its old sockets (the device calls told, their last events out,
the rest closed by force past `STOP_WITHIN`) before it binds the new ones; its `[::]` socket is
dual-stack as the listener's own is. The calls design's §3.9 has the details.

## 9. Settings and surfaces

### 9.1 Settings

New, in Settings → Network & access → Devices unless noted:

| Setting | Default | Range | Meaning | Applies |
|---|---|---|---|---|
| `devices.listener` | off | — | the device listener (§4.1) | at once |
| `devices.listen` | `loopback` | `loopback`, `all`, an address | where it binds; widened to `all` by the first remote pairing (D24) | at once |
| `devices.port` | `null` | 1–65535 | `null`: the HTTP port + 1 (Q1), written as a number at the first start | at once |
| `devices.token_minutes` | 10 | 1–1440 | a pairing token's lifetime (§3.3) | next token |
| `devices.client_cert_days` | 365 | 1–3650 | a device certificate's validity; notice from two thirds (§2.3) | next issue |
| `devices.server_cert_days` | 30 | 1–3650 | a server leaf's validity; re-issue from half (§2.2) | next issue |
| `devices.handshake_timeout_s` | 10 | 1–300 | a TLS handshake's bound (§4.3) | next connection |
| `devices.header_timeout_s` | 30 | 1–3600 | a request's headers, and an idle kept-alive connection's wait for the next (§4.3) | next connection |
| `devices.unauthenticated_timeout_s` | 10 | 1–300 | a whole connection without a certificate (§4.3) | next connection |
| `devices.enrol_refuse_per_min` | 120 | 1–100000 | failed enrolments per address (IPv6 /64) per minute before a 429 (§3.3) | at once |
| `chat_feed_keepalive_max_s` (Settings → Chat → Change feed) | 900 | 1–86400 | the largest keep-alive a stream may ask (§7.1) | next feed |

The save (one patch for the dashboard and the tool plane) refuses a value out of range by name, and
its answer says when the change applies. The `/devices/*` body limit (16 KiB) is a stated constant
in their `DocRoute` and in the 413. The server certificates' DNS names come from the URL lists.

*Changed 2026-10-15 (WP2 review):* two more, in the same group: `devices.unauthenticated_max`
(default 64, 1–4096: connections not authenticated yet, handshakes included, from every peer; a
new one past it closes the oldest) and `devices.unauthenticated_per_peer` (default 8, 1–4096: the
same from one IPv4 address or IPv6 /64, whose oldest goes), each applying to the next connection
(§4.3).

*Changed 2026-10-14 (WP1, as built):* one patch (`DevicesSettingsPatch`) serves the `settings_set`
op and the dashboard's `settings_set_full`, but `devices` is an access setting
(`ACCESS_SETTINGS`, beside `bind_addr`): the `lmgw__settings_set` tool refuses it by name, since
it decides who reaches lmgw and how long their credentials live. The read answer carries
`port_effective` (the number a `null` port stands for, `null` at an HTTP port of 65535). WP1 takes
the seven bounds; `listener`, `listen` and `port` are stored and read but refused by the saves
until WP2 adds them with their side effects (bind, rebind, confirmation). A stored value out of
range is clamped on load with a warning; an unreadable `listen` reads as `loopback`.

### 9.2 Status

`GatewayStatus` gains a `devices` block: the listener (off, scope, bound addresses, or the bind
error); the CA's `fp`, creation date and **key mode** (`wrapped` in the Secret Service, or `plain`
for a headless start) or its error (missing, locked, unreadable, bad mode); the leaf cache (per
leaf: issued, valid until, names); pending tokens; contested and unconfirmed devices; failed
enrolments in the last hour.

*Changed 2026-10-14 (WP1, as built):* the trust half is `devices.ca` (`state`: `missing`, `stored`
(on disk, read without opening the key or asking the Secret Service), `ready` or `error`; `fp`,
`created`, `key_mode`, `error {kind, message}`, `key_file`, `cert_file`) and `devices.leaves`
(`slot`, `address`, `issued`, `valid_until`, `renew_after`, `names`). The error kinds add
`no_secret_service`, `wrap_key_missing` and `secret_service` to the four above. It is part of
`GatewayStatus`, so `lmgw__status` shows it too (the pin is public; no key is in it), and so
`key_file`, `cert_file` and every message name the files relative to the data directory
(`tls/ca.key`), never the absolute path with the home directory.

*Changed 2026-10-15 (WP2, as built):* the listener's half is `devices.listener`: `state` (`off`,
`listening`, `error`), `listen` and `port` it serves or tried, `urls` (`https://` per bound
address), `error` (a bind error naming the address and both settings, the CA's reason, or a port
it cannot derive), and what it holds now: `connections` (handshakes included), `requests` in
flight (an open feed is one) and `sessions` (realtime sessions and turns). The dashboard's
confirmations read the last three.
*Changed 2026-10-15 (WP2 review):* it also says `unauthenticated` (the connections not
authenticated, now) and `evicted` (those closed to make room since the listener started, §4.3);
the dashboard's state line shows both when they are not zero, in amber once anything was evicted.

*Changed 2026-10-15 (WP3, as built):* the pairing half is `devices.pairing`: `tokens_alive`,
`failures_last_hour`, `last_failure_from` (the counted key: an IPv4 address, or the /64),
`last_failure_at` and `counted_since` (the counts start at zero with each start). Contested and
unconfirmed devices are each row's `pairing` in the Keys list (§9.3), not the status.

### 9.3 The Devices card

- **A link strip** above the device rows: the listener's scope and state; the CA fingerprint,
  shortened, and its key mode, with Copy whole and Copy CA certificate (PEM, for curl); Rotate the
  device CA… (§5.5); "N devices need a new pairing" while v1 rows remain.
- **Per device**, beside client-apps' policy and online state:
  - the pairing: "waiting for its device, token valid until 14:32"; "enrolled from 192.0.2.2, code
    7F3A-91C2: Confirm / Delete"; "token ended unused"; or "paired 2026-10-14 14:03 from 192.0.2.2:
    Kai mobile 0.1.0 (Android 16), confirmed";
  - a **contested** notice when a used token came back with another key (§3.3);
  - the certificate's dates and notice (§5.3), and the URL list, editable in its dialog;
  - each open link: "feed (keep-alive 120 s, since 09:00, from 192.0.2.2)", "voice (call, from
    192.0.2.2)", "tools". `Links` keeps the peer address, the start and the keep-alive beside the
    count.
  *Changed 2026-10-15 (WP5, as built):* `Links` keeps each open connection as an entry (kind, start,
  the TCP peer as the connect info saw it, and the keep-alive a feed asked for), and the Keys list's
  `open_links` carries them: `OpenLinks { kind, count, links: [{since, peer, keepalive_s}] }`, the
  oldest of a kind first. The card's rendering is not part of WP5.
- **Actions**: Confirm, New token…, Disable…, Delete… (each says what it ends, as today), and Edit
  (policy, URLs). A v1 row carries the chip "v1: pair again" and New token… (§12).
- **The QR card**: the QR code, the link with Copy and Open on this computer (`MintedLinks`), the
  expiry with the minutes left, the URL list and `fp` shortened; it turns into the confirmation when
  the device enrols (§3.5), and closing it asks whether to end the token (§3.3).
- **`/api/events`**: the `keys` frame gains `token` (issued, used, ended), `enrolled` (with the code
  and the peer address), `confirmed` and `contested`, so the card moves without polling and shows
  the contested notice at once.

*Changed 2026-10-15 (WP3, as built):* the Keys list's device rows carry `pairing`
(`DevicePairingView`): `state` (`waiting`, `enrolled`, `confirmed`, `v1`), the latest `token`
(created, expires, used at and from, ended, alive), `enrolled_at`, `enrolled_from`, `client`,
`confirm_code`, `confirmed_at`, `contested_at`, `contested_from`, the certificate's dates, `urls` and
`local_only`. The `keys` frame's `what` gains `token`, `enrolled`, `confirmed` and `contested`; the
card refetches the list on it, so the frame carries no code or address of its own. The card itself
is WP4's. Until it lands, the Devices card's form pairs "this computer" for a loopback address and
another device otherwise (it sends no `url`), its **New token…** (the old Rotate) ends the current
key, and the link's dialog shows the URLs, the expiry and the answer's message.
*Changed 2026-10-15 (WP3 review):* so the dashboard can finish a pairing before WP4, each device
row of the interim card shows a key waiting for its Confirm ("enrolled from 192.0.2.2, code
7F3A-91C2", "a new key enrolled …" beside a kept one) with **Confirm**, which sends that code; a
contested row says where the code came back from and asks for the code the device shows instead;
and while the row's pairing code lives, **End pairing code** (`device_token_end`). `pairing` gains
`kept_code`: with a key waiting beside a kept one, `state` is `enrolled`, `confirm_code` is the
waiting key's, `kept_code` the kept key's, and `confirmed_at` and the certificate's dates the
kept key's.

*Changed 2026-10-15 (WP4, as built):* the card is `pages/usage/devices.rs` with children in
`devices/` (the pure decisions in `logic.rs`, held by unit tests). It replaces WP3's interim card:
the form's address field, the link dialog and the old **New token…**/Rotate confirmation are gone;
what stays on a row is Confirm and **End pairing code**, now in the row's pairing line.
- **The link strip** sits in the card above the rows (it shows with no device paired): the
  listener's scope, port and open connections (amber when it does not serve); the CA's pin
  shortened (`sha256:9f2c1a7b…41ab`, the whole one in its tooltip) with the key mode; **Copy
  whole**, **Copy CA certificate** and **Rotate the device CA…** with §5.5's confirmation naming
  every device that holds a key; "N devices need a new pairing" while v1 rows remain; the failed
  pairings of the last hour with the last address (the tooltip says the counts start at zero with
  each start); and the pairing codes alive. It follows `/api/status`, refetched with the keys list
  on every `keys` frame.
- **Reading the CA certificate:** `devices.ca` in the status gains `certificate_pem`, the
  certificate as `tls/ca.crt` holds it (public, `0644`, like the pin; `null` while there is none),
  for **Copy CA certificate**. The status never carried a key and still does not.
- **The pairing form** asks "where": **this computer**, **the LAN** or **WireGuard**. The URL list
  is a textarea, one URL a line in the owner's order (no cap on the number), seeded from
  `GET /api/connect`, which gains `device_port` (`https://127.0.0.1:<port>` for this computer) and
  `device_urls` (`{url, interface, wireguard}`: `devices::proposed_for_device`, the §3.2 review's
  proposal with the interface and whether it is a point-to-point link). The LAN list is the
  proposal's addresses on ordinary interfaces and WireGuard's those on a point-to-point link
  (`wg0`; Tailscale too); a "where" with none says so and leaves the list empty for the owner. The
  form sends `local_only` and `urls` explicitly. **This computer** refuses a non-loopback URL in
  the list. The proposal is a starting point only: an edited list is sent as typed, and the
  gateway checks the rest (§3.2's note) and names what it refuses.
- **The widening confirmation (D24)** is the review step's notice, from the saved
  `devices` settings (`listener_need`): an off listener is turned on by the button ("Turn on and
  pair" for this computer, on loopback; "Turn on, widen and pair" for another device, which sets
  `listen` to `all`), with §4.1's exposure text for any widening; a listener on loopback with a
  remote device offers **Widen and pair the device** and **Pair without widening** (the device
  then cannot reach lmgw, which the button's tooltip and the answer's message say). An off
  listener cannot be declined, since `key_create` is 409 `device_listener_off` without it. The
  patch is `settings_set_full` with `listener`, `listen` and the port as a number, sent before
  `key_create`; a failure of either stops there.
- **The QR card** shows the link as a QR code (`qrcode` 0.14.1 with `svg` alone, no other crate
  joins the lock; dark on a white quiet zone whatever the palette, error correction M, L when M
  does not fit) beside its expiry ("valid until 14:32 · 9 min left", moved on every 20 s), the URL
  list, the pin shortened, the answer's message, the link as text with **Copy link** and **Open on
  this computer**. A link too long for any QR code gets a sentence and the link alone. The link
  lives in the card only, and is gone with it. The card follows its row through
  `logic::card_phase`: the QR code while the row's latest token is the card's own and unused; the
  confirmation (the device's code in large type, "Does the device show this code?", **Confirm**
  sending `confirm_code`, **Delete the device…** with an inline second step; for a contested row a
  notice and an input for the code the device itself shows); then "paired"; or "ended" when the
  token lapsed or was ended unused. A list that has not caught up (an earlier token still on the
  row) leaves the card waiting. A row deleted meanwhile closes the card without a question.
- **Closing the card** (✕, Esc, the backdrop, Done, however) asks "End the pairing code?" while the
  row's token is alive, which includes a used one that the device has not yet answered with its
  certificate: **End the pairing code now** is the primary button and the default, and dismissing
  the question without answering ends it too; **Keep it valid until 14:32** is the other
  choice.
- **New token…** opens a dialog. On a row with a working key (confirmed, or a new key waiting
  beside a kept one) it offers §5.2's two choices as radio buttons, **keep the current key until
  the new one is confirmed** first and preselected, and **end the current key now**, whose button
  turns danger-red ("End its key and make the token") and which says what it closes; other rows
  get the plain question with the state's own sentence (before TLS, never confirmed, waiting). The
  answer opens the QR card.
- **Per row**, under the name: the pairing in one line (§9.3's wording: "waiting for its device,
  token valid until 14:32", "enrolled from 192.0.2.2, code 7F3A-91C2" with **Confirm**, "token ended
  unused", "paired 2026-10-14 from 192.0.2.2: Kai mobile 0.1.0 (Android 16), confirmed"), the
  contested notice, the certificate line ("certificate until 2027-10-14", in amber from
  `notice_after`: "lapses on 2027-10-14: pair it again before then", and "certificate lapsed" past
  `notAfter`) with the URL list, and one line per open connection from `open_links` ("feed
  (keep-alive 120 s, since 09:00, from 192.0.2.2:51514)"). A v1 row carries the chip "v1: pair
  again". The policy dialog's **Edit** gains the URL list (`key_set {urls}`).
- **Live:** the keys list refetches on every `keys` frame (`token`, `enrolled`, `confirmed`,
  `contested`, `link`…), and a 20 s clock moves the words that count time, so the card, the strip
  and an open QR card follow without polling.
- **Not built here:** "renews by itself" (WP8), the failed-enrolment counts per address beyond the
  last one, and reopening an old QR card (the link is shown once; **New token…** makes another).

*(changed 2026-10-15: the owner's decision on device-hosted tools, client-apps design L16 and
§5.6. The pairing form's policy gains **Device tools**, the device-tools grant: a checkbox per
label a paired device hosts now (another device row with a hosting label), a free entry for a
label none hosts yet, none ticked by default, and a note naming a granted label no device hosts
("the grant waits until one does"); `key_create` sends it as `device_tools`. The review step
restates it in its table ("Device tools: kai", with the unhosted ones named). The policy dialog's
**Edit** carries the same field for a device and a client key (`key_set {device_tools}`), and its
tool preview counts another device's tools only when the grant names them. Each device row shows
its grant beside the name, "uses kai__" (amber "kai__: no device hosts it" for a label none hosts),
and in its reach line; a hosting row's "reached by" list names the keys and devices whose grant
names its label. Without the grant no other device reaches a device's tools, whatever its tool
scope says; with it, the tool scope picks which of them. So Kai mobile's phone, scope `all`, uses
Kai desktop's tools once `kai` is ticked.)*

### 9.4 Ops, routes, documents

- **Ops** (`/api` only, absent from the `lmgw__*` plane as the key ops are, principals §3.12):
  `key_create {kind: "device", name, local_only, urls, policy…}` answers `{id, name, link, urls,
  token_expires_at, fp}`; `device_token {id, end_current}` issues a new token (§5.2) and answers the
  same shape; `device_token_end {id}` ends a pending token; `device_confirm {id}`; `device_ca_rotate
  {}` answers `{fp, devices_reset}`; `key_set` takes `urls`, and `key_rotate` refuses device rows.
  Each op gets its `op_names` entry and an `OpDoc`; `key_create` and `device_token` are
  `reveals_secret` (the token), and `device_ca_rotate` has a `confirm_note`.
- **Routes**: `POST /devices/enrol` (and `/devices/renew` with WP8) get `CAPABILITY_TABLE` rows
  (§4.6) and `DocRoute`s under a "Devices" tag, with the body limit and every refusal of §3.4.
  `GET /chat/api/feed`'s `DocRoute` gains `keepalive_s`, `caught_up` and `hello`'s `urls`, `cert`
  and `open_approvals`; `approval.requested`'s schema gains `thread_title`; `RevokeKind` gains
  `certificate_expired`.
- **No new `x-lmgw` header**, so `LMGW_HEADERS` is unchanged.
- **Documents this updates when it lands**: client-apps §1.4 (the link v2, `fp` no longer reserved)
  and its Later list (TLS, push); principals §3.2's table (`/devices/*`, the device listener's
  face); the dashboard's help for Settings → Network & access. The calls record's changes are its
  own writer's (§8).

*Changed 2026-10-15 (WP3, as built):* the four device ops join `KEY_OPS` (86 ops in all).
`device_token` is `reveals_secret` beside `key_create`, with a `confirm_note` for `end_current`;
`device_token_end` answers `DeviceTokenEnded {id, name, ended, message}`. The ops add the code
`device_listener_off` (409) to `op_failed`.
*Changed 2026-10-15 (WP3 review):* `device_confirm {id, confirm_code}`, refused 409
`confirm_code_mismatch` for a code that is not the waiting key's (§3.5's note), answers
`DeviceConfirmed {id, name, confirm_code, replaced, message}`. `EnrolRequest.token` and `DevicePaired.link` are
flagged secret. The `chat` tag and the bearer scheme name the certificate and the device listener.
*(changed 2026-10-15: `key_create` and `key_set` take `device_tools`, a list of hosted labels, for
a device and a client key; the Keys list's `KeyRow` carries it. Any label a device could host is
taken, hosted now or not (client-apps design L16, §5.6).)*

## 10. Security

| Threat | What covers it |
|---|---|
| An on-path attacker on a LAN or a foreign Wi-Fi | TLS 1.3; the device verifies lmgw against `fp`'s CA before it sends its own certificate (§6.3) |
| A URL that reaches a stranger's host | the same: the handshake fails before the device shows anything, and no secret exists to show |
| A process squatting on lmgw's port while lmgw is down | a client that pins the CA refuses it before sending anything, and holds no bearer to send (D12) |
| A photographed QR code or a copied link | one use, 10 minutes by default, ended on the card's close if the owner says so; a stranger's enrolment shows another code and waits for a Confirm that never comes (D23), and the legitimate device's later attempt marks the row contested (§3.3) |
| Guessing tokens | 128 bits; while no token is pending the handshake needs a certificate; failures are counted per address and shown |
| Unauthenticated connections | they exist only during a token's life, reach enrolment alone, are bounded by `devices.unauthenticated_timeout_s` and `devices.header_timeout_s`, carry `Connection: close`, write no request rows, and send at most 16 KiB |
| A stolen phone | Disable or Delete: the next request is refused, and the feed, sessions, calls and host link close at once; the Keystore key cannot be copied off the phone |
| `ca.key` copied (with its wallet, or a headless start's plain file) | the copier can impersonate lmgw to every paired device: read what devices send, answer their turns, and **drive a device's hosted tools** over `/mcp/host` (Kai desktop's desktop tools now, the phone's tools later); not impersonate a device to lmgw (rows are keyed by device keys, §2.3). D2 wraps the key in the Secret Service; Rotate the device CA re-pairs everything (§5.5) |
| A forged client certificate | without a confirmed row holding its key it opens nothing (§2.3) |
| TLS downgrade, resumption skipping checks | TLS 1.3 only; resumption off (D5) |
| A certificate that lapses while a link is open | checked per request, and long-lived links end at `notAfter` (D5) |
| Browsers | no CORS on the device listener; a page cannot present a device certificate |
| The WebRTC media | DTLS fingerprints exchanged inside mTLS (§8) |

A peer that completes a handshake without a certificate, which is possible only while a token is
pending, sees the leaf: its local address or the loopback addresses, `localhost`, and the hosts of
the URL lists (§2.2), and the CA's subject.

## 11. Dependencies

- **rcgen 0.14.10** (2026-08-28), MIT or Apache-2.0, new, in lmgw-core, with `default-features =
  false, features = ["aws_lc_rs", "x509-parser", "pem"]` (no ring). Used for `KeyPair` generation,
  `CertificateParams::self_signed` and `signed_by(&impl PublicKeyData, &Issuer)`, `Issuer::new`,
  and `SubjectPublicKeyInfo::from_der`. It brings x509-parser 0.18 (with `verify-aws`), which also
  reads a peer certificate's `notAfter` (D5), and requires aws-lc-rs ≥ 1.18, so the lock moves from
  1.17.0; WP1 checks with `cargo tree -d` that rustls's, quinn's and the calls design's str0m's
  aws-lc-rs unify on it. **Its MSRV is 1.88.** The repository pins no toolchain (no
  `rust-toolchain` file, no `rust-version`); CI installs Fedora's `rust` package
  (`ci/install-build-deps.sh`), 1.98 at the time of writing, so nothing moves.
- **secret-service 5** (5.2.0 in Kai's lock), MIT or Apache-2.0, new, in lmgw-core, with
  `rt-tokio-crypto-rust` as Kai uses it: the CA key's wrapping key (D2). It runs on zbus 5, which the
  tree has (5.16.0).
- **tokio-rustls 0.26** (0.26.4 in the lock), direct now: features `aws_lc_rs`, without `tls12`.
- **rustls 0.23** (0.23.40) and **rustls-webpki 0.103** (0.103.13), direct now; in the build
  already: `WebPkiClientVerifier`, `NoServerSessionStorage`, `EndEntityCert`.
- **hyper-util 0.1.20**, direct already: its `server-auto`, `server-graceful` and `http1` features
  for the device listener's loop (§4.3). **socket2 0.6** (0.6.4 in the lock), direct now:
  `SockRef::set_tcp_user_timeout` (§7.1).
  *Changed 2026-10-15 (WP2, as built):* hyper 1.10 with `server` and `http1` and hyper-util with
  `tokio` alone (`TokioIo`, `TokioTimer`), §4.3's note says why; tower 0.5 with `util` (axum's)
  for the per-request `oneshot`; socket2 with `all` (the tree's) also binds the listener's sockets
  (dual-stack `[::]`, close-on-exec); tokio-rustls without its default `tls12`.
- **qrcode 0.14.1**, MIT or Apache-2.0, in lmgw-ui: `default-features = false, features =
  ["svg"]` (pure Rust, wasm; the `image` feature stays off).
- **`lmgw-client`**: `url` 2 (2.5.8 in the lock) for the parser, and rustls 0.23 behind the
  non-default `tls` feature, provider left to the app.
- **Clients**: reqwest 0.13.5's `tls_backend_preconfigured` (`use_preconfigured_tls` is its
  deprecated alias) and tokio-tungstenite 0.30's `Connector::Rustls(Arc<ClientConfig>)`, both read in
  their sources.
- **Not chosen**: `axum-server` and `tls-listener` (§4.3); a CSR path (D4);
  `rustls-platform-verifier` (the system's trust store has no say: the pin is lmgw's own CA).

## 12. Migration on the alpha wire

- **The migration marks every existing device row `v1:`** (§3.6), keeping its id, name, policy,
  hosting label, admin level and history. Its old bearer stops working at the release that brings
  this design.
- **Kai desktop** with a v1 pairing meets 401 `device_key_unknown` on its next request, with §4.8's
  v1 message, and reads it as "pair again" (`key_refused`'s `PairAgain`), as it reads a rotation
  today.
- **The dashboard tells the owner**: each v1 row carries the chip "v1: pair again" and its tooltip
  ("paired with a link from before TLS; this lmgw accepts only certificates now"); the card's strip
  says "N devices need a new pairing" until none does.
- **The owner's steps, once:** install the lmgw and Kai releases that carry the device link (Kai's
  pin moves to it); on the v1 row, **New token…** with "this computer" (which turns the device
  listener on, loopback only, with its confirmation); open the link in Kai (lmgw's own window hands
  it over); compare the code and Confirm.
- **For a device on another machine** the first such pairing widens the listener (§4.1), and the
  firewall needs TCP and, by Q2, UDP on the device port in the zone the device comes from; on
  Fedora's Workstation zone both are open already.

## 13. Tests

### 13.1 Integration tests (`tests/it`)

New modules of the one binary, after the repository's rules. Every case runs a real server on port
0 with both listeners, rustls clients built in the test, and certificates rcgen makes:
- **`device_enrol.rs`**: a fresh token's enrolment returns a certificate for the sent key that
  verifies under `ca_certificate`, with the stated dates, URLs and the code derived from the key.
  Two concurrent enrolments with one token give one 201 and one 410; the same key again gets the
  same answer; another key gets 410 and marks the row contested, with the `keys` frame. An expired
  token is 410, a disabled row's token is 401 and stays unused, an unknown one is 401. Failures from
  two addresses of one IPv6 /64 count together, and the 121st in a minute is 429 with
  `Retry-After`. A non-P-256 key is 400, another row's key 409, a 17 KiB body 413, and enrolment
  over the plain listener a 404.
- **`device_confirm.rs`**: before Confirm every route answers 403 `device_unconfirmed` with the
  code; after it the feed opens; a re-pair waits for its own confirmation while a kept old key
  still works.
- **`device_tls.rs`**: with no token pending, a client without a certificate fails the handshake;
  with one pending it reaches enrolment, gets 401 `client_certificate_required` with `Connection:
  close` on any other route, and leaves no `request_logs` row. A connection that sends no request
  is closed after `devices.unauthenticated_timeout_s`, and an authenticated one idle past
  `devices.header_timeout_s` too. A confirmed device's certificate gets the feed, a Chat route and
  `/v1/models`. A certificate from another CA, and a lapsed one, fail the handshake; a valid one
  whose key no row holds gets 401 `device_key_unknown`. A bearer, a cookie or `x-api-key` there
  gets 400; the SPA, `/api/status` and an agent proxy path are 404; TLS 1.2 is refused; a second
  connection does not resume.
- **`device_listener.rs`**: a plain listener's feed survives the device listener going off; moving
  the device listener to another port rebinds at once and the old port refuses; `null` scope binds
  loopback until a remote pairing's confirmed widening; the first start writes `devices.port`, and a
  later `bind_addr` port change leaves it.
- **`device_revoke.rs`**: Disable, Delete and expiry close a live feed (`revoked` with its kind), a
  realtime WebSocket and `/mcp/host` (4003 with the token) over the device listener, and refuse the
  next request on a kept-alive connection. A certificate whose `notAfter` passes under a test clock
  ends an open feed with `certificate_expired` and refuses the next request. A re-pair ends the old
  key's links with `key_unknown`; "end the current key now" ends them at once.
- **`device_rotation.rs`**: clearing the leaf cache, or passing half the validity under a test
  clock, issues a leaf with a new key, and a paired client connects with no re-pair. A connection on
  127.0.0.2 gets the loopback leaf and adds no cache entry; a URL list's IP host appears in every
  leaf's SANs. Rotate the device CA resets every row, and an old client fails to verify lmgw. The
  cache rule for interface addresses is a unit test over an injected address list.
- **`device_plain.rs`**: a `lmgw-device-` bearer on the plain listener is 401
  `device_key_unknown`, from a loopback peer too, and a `v1:` row's gets the v1 message; owner
  keys, client keys and the cookie are unchanged there. *Changed after the owner's answers,
  2026-10-14:* Q3 is answered no, so the loopback peer rule and its unit test are gone.
- **`chat_feed` additions**: `keepalive_s` within, at and beyond the maximum; a stream at a long
  interval still delivers a record whose writer did not wake the feed, within the global backstop;
  `hello.open_approvals` matches the pending set at the head, and a catch-up holding a request and
  its decision leaves the reader's set as `hello` gave it; `caught_up` after a paged catch-up and
  right after `hello`; `approval.requested.thread_title`; a device feed's socket carries
  `TCP_USER_TIMEOUT`.
- **The route walk** covers both faces (§4.6); the op tables cover the new ops. The test peers'
  addresses (127.0.0.1, 127.0.0.2) stand in for LAN and WireGuard.
- **The CA key's modes** are unit tests: wrap, unwrap, a plain key wrapped at the next start, and a
  wrapped key without its item refusing to start the listener. The Secret Service itself is checked
  live on a dev instance.

*Changed 2026-10-15 (WP3, as built):* the pairing cases are one module, `device_pairing` (`enrol`,
`repeat`, `confirm`, `plain`, `ca`, `logs`), and the migration's `migrations::device_link_pairing`.
Every suite that drove a device with its bearer now pairs it for real (`support::device_pair`: the
owner's `key_create`, `lmgw-client`'s pinned enrolment and reader, `device_confirm`) and speaks to
the device listener with `lmgw-client`'s configuration through a local proxy of the suite's own,
which forwards each request, upgrades included, over a fresh mutual-TLS connection; a WebSocket
opens at its address. A device on an admin route now meets the face's 404 where it met 403. The
lapse of a live link at the certificate's `notAfter` is a unit test (`devices::revoke`), the
per-request refusal WP2's.
*Changed 2026-10-15 (WP3 review):* and end to end: `device_keys`' lapsing-certificate case pairs a
device, issues a certificate for its own key that lapses in five seconds
(`tls::issue_device_for`, `support::device_pair::lapsing`), opens the feed and a realtime session
with it, and waits for `revoked {kind: certificate_expired}` and the 4003 with the design's own
bound, the `notAfter` plus the watch's re-check interval (`devices::RECHECK`), never before the
date. `device_pairing` adds the three re-pair choices (`confirm`), the token's end at the first
request with its certificate (`repeat`), and lmgw refusing an old CA's client certificate after a
rotation (`ca`); the suites' proxy passes a refused upgrade as the device listener framed it, so
the end-to-end path holds the counted body's size hint (§16's note).

### 13.2 With real TLS clients

`scripts/device-link-check.sh` (a new recipe, run against a dev instance from
`scripts/dev-instance.sh`, never a real data directory):
1. reads `tls/ca.crt` from the dev data directory, mints a token through the dev instance's `/api`
   with its owner bearer, and makes a P-256 key with `openssl genpkey`;
2. enrols with `curl --cacert ca.crt` and a JSON body built from `openssl pkey -pubout -outform DER
   | base64`, prints the code and confirms through `/api`;
3. calls `curl --cacert ca.crt --cert dev.crt --key dev.key https://127.0.0.1:<port>/v1/models`
   and opens the feed with `curl -N`;
4. checks that the same call without `--cert` fails the handshake once no token is pending, and
   that `openssl s_client -tls1_2` fails;
5. prints `fp` recomputed from `ca.crt` beside the one in the link.

Kai desktop and Kai mobile run their own pairing checks against such a dev instance (kai §14,
kai-mobile §15.3), including the one-constructor test (D15).

### 13.3 Repository rules this follows

- New logic in new child modules: `devices/tls/` (`ca.rs`, `wrap.rs`, `leaf.rs`, `issue.rs`),
  `devices/tokens.rs`, `devices/enrol.rs`, `server/device_listener.rs` (`TlsListener`, the serving
  loop, `Conn`), `web/devices.rs`, `store/device_tokens.rs`. Tests as modules of the one `tests/it`
  binary.
- SQLite writes through `store::begin_write`; migrations numbered as the repository does.
- Each new route gets a `DocRoute` (no exclusion), each new op an `op_names` entry and an `OpDoc`;
  no new `x-lmgw` header.

## 14. Work packages

| WP | Scope | Needs | Parallel with |
|---|---|---|---|
| 1 | **Trust:** rcgen, the CA (creation, files and modes, the Secret Service wrapping and its modes, `fp`), the leaf cache (loopback leaf, interface leaves, URL-list SANs, re-issue), device certificate issuance, the `devices` settings class, the status block's TLS part | — | 5, 6 |
| 2 | **The device listener:** `TlsListener` (owned accept tasks, handshakes off the loop, the per-handshake configuration), the serving loop with its timers, a `Stops` per listener, `Conn` for both listeners (coordinated with the calls' local address), the face rule and the route walk, `principal_mw` and the gate (certificate principal, per-request `notAfter`, no request rows and `Connection: close` without a certificate), the body limit, scope and port with their confirmations and the port's persistence, the log line; *(added 2026-10-14)* the calls design's device socket group (its WP7) lands with, or right after, this WP | 1 | 5, 6 |
| 3 | **Pairing:** the migration, tokens (issue, single use, same-key repeat, contested, end on close, per-/64 counting), `/devices/enrol`, the confirmation, `key_create`'s v2 answer, `device_token`, `device_token_end`, `device_confirm`, `device_ca_rotate`, re-pair with its two choices, `key_rotate`'s refusal, D12's end of device bearers with the v1 message, `MintedLinks` for v2 | 2 | 5, 6 |
| 4 | **The card:** the link strip, the pairing form's "where", URL list and widening confirmation, the QR card with its confirmation and close question, per-device pairing, notices and links, the v1 chip, `/api/events`' new `keys` facts | 3 | 5 |
| 5 | **The feed for push:** `keepalive_s` with its maximum, the split tick, `TCP_USER_TIMEOUT` from the keep-alive, `hello.open_approvals`, `caught_up`, `hello.urls` and `cert`, `approval.requested.thread_title`, `certificate_expired`, `Links`' per-link details | 2 for `Conn`'s socket handle | 1–4, 6 |
| 6 | **`lmgw-client`:** `pairing` v2 (moved from Kai), the enrolment builder and reader with the `fp` check, `Credentials`, the URL order and the https rule, the new refusal kinds, the `tls` feature's verifiers and client configuration, the feed's new parameter, events and open set | this record's wire | 1–5 |
| 7 | **Landing:** `scripts/device-link-check.sh`, the documents of §9.4, §12's steps where the release says what changed | 3, 4, 6 | — |
| 8 | **Silent renewal**, after the first usable version: `/devices/renew` (§5.4), the client's renewal at `notice_after`, the card's "renews by itself" | 7, and the owner's acceptance of the first version | — |

- *Changed 2026-10-15:* WP4 is built (§9.3's note "WP4, as built"): the card, the form's "where" and
  widening confirmation, the QR card, re-pair with its choices, notices and links, and the live
  `keys` facts. The QR code is `qrcode` 0.14.1 (`svg` only), as §11 chose.
- **The order**: WP1 → WP2 → WP3 → WP4; WP5 and WP6 beside them from the start (WP5's
  `TCP_USER_TIMEOUT` waits for WP2's `Conn`); WP7 closes the first usable version; WP8 comes after.
- **The calls design**: its WP2 and this WP2 both touch the connect info; whichever lands second
  rebases onto `Conn`. §8's changes go into the device socket group in the calls design's WP7, which lands with, or right
  after, this WP2. *Changed 2026-10-14 (consistency pass):* this replaces "the calls design's UDP
  socket before its WP2 binds it".
  *Changed 2026-10-14:* the calls design's WP2 builds the driver and the plain socket group. Its
  WP7, the device socket group and its live rebind (§8), lands with, or right after, this WP2, and
  whichever of the two lands second rebases onto `Conn`.
- **Kai desktop** switches (§6.2) after WP3 and WP6, at its next pin. Until then a Kai desktop on
  this lmgw is told to pair again and cannot (the v1 parser refuses a v2 link), so the lmgw and Kai
  releases go out together (§12).
- **Kai mobile**: its pairing WP needs WP3 and WP6, and the QR code needs WP4. Its background feed
  needs WP5, and its notifications need client-apps WP10's `message.*` beside WP5. Its calls need
  WP2's `Conn` and §8's UDP rule, which is the calls design's device socket group (its WP7,
  *changed 2026-10-14*).

## 15. Questions for the owner, in priority order

All five are answered (below).

1. **The device listener's port by default.** A socket of its own (D10). A fixed default would be a
   second number to remember on every instance, and a dev instance's would collide with
   production's. "The HTTP port plus one" keeps each instance's pair together (8787 and 8788 by
   default, a dev instance on another port and that port plus one), and it is written into the settings the first time
   the listener starts, so it never moves after that unless the owner moves it. A collision is a
   visible bind error naming the setting. *Recommendation:* the HTTP port's number plus one,
   persisted at the first start. *Answered 2026-10-14:* as recommended.
2. **The calls' UDP socket follows the device listener live.** Phones POST to the device listener,
   whose local address becomes the candidate, and the device listener turns on at the first pairing
   without a restart. *Recommendation:* while the device listener runs, the UDP socket binds where
   it binds, on its port number, and follows each of its changes at once, its live calls ending
   under the same confirmation (§8). One number, TCP and UDP, opens the way for phones. With the
   device listener off, the calls design's rule stands. *Answered 2026-10-14:* as recommended.
3. **Bearer device keys on loopback.** Kai desktop on the same machine could keep a bearer over the
   plain loopback listener, with device bearers refused from every other peer (§4.8). Kai desktop
   autostarts and retries while lmgw is not running, so any local process that binds
   lmgw's port first would collect that bearer on the next retry; a client that pins lmgw's CA
   refuses such a squatter before it sends anything, and holds no bearer to send. Keeping bearers
   also keeps a second credential path in lmgw, Kai and `lmgw-client`. *Recommendation:* no. Every
   device authenticates by its certificate, Kai desktop on loopback included (D12). *Answered
   2026-10-14:* as recommended.
4. **Confirm each enrolment on the card.** A token is good for ten minutes in a QR code that a
   camera or a screenshot can copy. Without a confirmation, whoever enrols first gets a device with
   the full scope the form set, silently. With it, the device and the card show a code derived from
   the enrolling key, and the device works only after the owner clicks Confirm (§3.5). The cost is
   one click per pairing. *Recommendation:* yes. *Answered 2026-10-14:* as recommended.
5. **Loopback only until the first remote device.** A purely local setup (Kai desktop on the same
   machine) needs no network exposure, yet Fedora's default desktop zone opens 1025–65535 to the
   LAN, so a `[::]` bind would be reachable there at once. *Recommendation:* yes: loopback while
   every paired device is "this computer"; the first pairing of another device widens to every
   address, with a confirmation that names the exposure (§4.1). *Answered 2026-10-14:* as
   recommended.

### Answers (the owner, 2026-10-14)

All five are approved as recommended:
- **Q1**: the device listener's port is the HTTP port's number plus one, written into the settings
  the first time the listener is enabled, so a later change of `bind_addr`'s port leaves it (D10,
  §4.1).
- **Q2**: the calls' UDP socket follows the device listener live (D21, §8). The calls design
  carries §8's eight changes, per socket group: the device listener's calls have sockets that
  follow it, and the plain listener's calls keep theirs (§8).
- **Q3**: no bearer device keys on loopback. Every device authenticates by its certificate over the
  device listener, Kai desktop on the same machine included (D12, §4.8).
- **Q4**: each enrolment is confirmed on the card before the device is served (D23, §3.5).
- **Q5**: the device listener binds loopback only until the first remote device, and widens to
  every address with a confirmation that names the exposure (D24, §4.1).

The answers are rulings 10 to 14 under "Decided". The text that held the declined alternatives
(§3.5's last bullet, §4.8's second, §13.1's `device_plain.rs`) carries a dated note.

## 16. Not verified

| Claim | Settled in |
|---|---|
| rcgen 0.14.10 writes a `notAfter` of 9999-12-31 as GeneralizedTime, and OpenSSL's curl accepts such a root | WP1's unit test; §13.2 |
| aws-lc-rs 1.18 (rcgen's floor) unifies with rustls's, quinn's and str0m's in one build | WP1, `cargo tree -d` |
| The secret-service crate's unlock prompt for a locked collection behaves under lmgw's tray process as under Kai's | WP1, live on a dev instance |
| A socket-options handle carried in `Conn` stays valid for the connection's life through hyper-util's serving | WP5 |
| An Android Keystore P-256 signature through the UniFFI callback is fast enough on a new connection's path | kai-mobile WP0 (e) |
| A foreground service keeps the feed's socket through Doze on a typical Android phone, and which service type fits | kai-mobile WP0 |
| The assistant role's while-in-use exemption covers starting the microphone from an assistant session only, not from a background service | kai-mobile WP0 (b); §7.4 holds either way, by kai-mobile M11 |

*Changed 2026-10-14 (WP1):* the first row is settled as far as a file goes: a test reads the
`notAfter` as GeneralizedTime `99991231235959Z`, and OpenSSL 3.5.9's `openssl verify -CAfile
ca.crt` accepts the root, a leaf (`-purpose sslserver`) and a device certificate (`-purpose
sslclient`, refused as `sslserver`); the doc's `fp` pipeline gives the CA's `fp`. curl against a
live listener stays §13.2's. The second is settled: the lock has one aws-lc-rs, 1.18.2 (with
aws-lc-sys 0.46.0), shared by rustls, rustls-webpki, rcgen, x509-parser and lmgw-core; quinn's
lock entry depends on the same one, and str0m is not in the tree yet. The third (the live
unlock prompt) is untested: no test or check of WP1 talks to the desktop's Secret Service.

*Changed 2026-10-15 (WP2):* the fourth row is settled for the handle's own life: it is tied to the
stream's (a unit test sets and reads `TCP_USER_TIMEOUT` and sees it refused once the stream is
dropped); the feed setting it from a device's keep-alive is WP5's.

*Changed 2026-10-15 (WP3):* found while moving the suites: every counted request's body (the
in-flight count of `server::stop`, on both listeners) lost its size hint to `map_frame`, so every
answer went out chunked, and a WebSocket client read a refused upgrade's `{code, message}` as chunk
framing (tokio-tungstenite reads that body as the bytes that came with the head). The counted body
forwards the size hint now (`Held`), which a unit test holds.

## Later

- **Android key attestation** at enrolment: the Keystore's attestation chain proves the key is
  hardware-backed and non-extractable, and lmgw shows it on the card.
- **The CA key in a TPM**, beyond the Secret Service's wrapping.
- **HTTP/2 on the device listener**, so a phone's feed and requests share one connection, measured
  against kai-mobile's handshake cost.
- **A TLS face for client keys**: stock OpenAI clients on other machines over TLS, with lmgw's CA
  installed there.
- **The owner bearer in MCP client configs** (a coding assistant's MCP entry pointing at
  `http://127.0.0.1:<port>/mcp` with the owner key) has D12's squatting exposure on the plain
  listener: a process that holds the port while lmgw is down collects it. A pinned-TLS or
  socket-credential path for local MCP clients is its own design.
- **The dashboard away from the desk**, which needs an owner certificate in a browser.
- **A new device key by renewal**, without a token, authorised by the current key.
- **Ed25519 device keys**, if a client needs them.
- **iOS**: the Secure Enclave makes P-256 keys too; the same enrolment, through the same core.
- **A reverse proxy in front of lmgw**: still the calls design's Later item; the device listener
  assumes devices reach lmgw directly.

## Appendix: Review dispositions (2026-10-14, against fc2e4a72)

| # | Finding | Disposition |
|---|---|---|
| 1 | `Stops` is one generation, so the device listener's stop would end the plain listener's streams | a `Stops` per listener; the accept task owned by `TlsListener`; a test that a plain feed survives (D10, §4.3, §4.4, §13.1) |
| 2 | A stolen token gives a silent full-scope device | the confirmation code and Confirm (D23, §3.5, Q4); the contested mark and notice; `TokenUsedElsewhere`; ending the token on the card's close (D8, §3.3) |
| 3 | The pre-authentication surface | a certificate mandatory while no token is pending; enrolment alone without one, no request rows, `Connection: close`; header and unauthenticated timeouts on a loop of lmgw's own; a 16 KiB body limit; counting per /64 with a 429 far above (D11, §4.2, §4.3, §4.7) |
| 4 | Exposure of a purely local setup | loopback until the first remote device, then a confirmed widening (D24, §4.1, Q5); Q3's reason corrected (D12); the MCP owner bearer in Later |
| 5 | Conflicts with kai-mobile | listed (§6.4) |
| 6 | Open approvals after a gap | `hello.open_approvals` and its combination with `caught_up` (D17, §7.3) |
| 7 | The UDP socket binds only at restart | it follows the device listener live; the calls design's changes listed (D21, §8, Q2) |
| 8 | The CA key's reach and storage | the threat table names hosted tools; the key wrapped in the Secret Service, plain only headless, the mode in the status (D2, §2.1, §9.2, §10) |
| 9 | SANs and the leaf cache | every URL-list host in the SANs; one loopback leaf; leaves only for `if_addrs` addresses (D3, §2.2) |
| 10 | Expiry | `notAfter` read with x509-parser, checked per request, links ended at it (D5, §4.7, §5.1) |
| 11 | Port stability | the effective port written at the first start (D10, §4.1, Q1) |
| 12 | Clients | the `fp` check on `ca_certificate`; https-only refreshes; one constructor with a test; reqwest and tokio-tungstenite settled; MSRV checked; the provider left to the app; the Keystore callback confirmed (D15, §6, §11) |
| 13 | Scope | `TCP_USER_TIMEOUT` into WP5; renewal to WP8 with notices and re-pairing until then (D4, D16, §5.3, §5.4, §14) |
| 14 | Questions | Q1 to Q5 as above; the `bind_addr` advice is a note (§4.1) |
