# WebRTC realtime calls (`POST /v1/realtime/calls`) — Design

**Approved, ready for implementation, 2026-10-14.** Draft v3, against main at fe727494: v2 (v1
plus a review checked against str0m's source and lmgw's router, appendix "Review dispositions")
plus the owner's answer to Q1 and Q4 (ruling 3), which moves the contract shared with the Android
design from "minimal lmgw-only additions" to lmgw's own events on bound calls (appendix "v2 → v3");
then the owner's approval of Q2 and Q3 as recommended, with Q2 refined by the device link's
approved Q2: the device listener's calls get UDP sockets where it binds, on its port, which follow
it live, while the plain listener's calls keep sockets of their own (rulings 8 and 9, C4,
appendix "v3 → approved"). Every question is answered. Paths are relative
to the lmgw repository. Companion records in `docs/design/` are
cited by short name: "realtime §n" (2026-10-01), "chat-voice §n" (2026-10-03), "client-apps §n"
(2026-10-06), "principals §n" (2026-09-22) and "device-link §n"
([the device link](2026-10-14-device-link-design.md), 2026-10-14). The Android client's own design
is written in parallel in its own repository; it is cited as "the Android design".

This record pulls into scope what two earlier records parked: client-apps' Later list ("WebRTC
realtime (`/v1/realtime/calls`) for mobile jitter") and realtime §2.1 and §19 ("WebRTC … separate
surfaces"). lmgw serves OpenAI's WebRTC transport for the Realtime API, `POST /v1/realtime/calls`
and `POST /v1/realtime/calls/{call_id}/hangup`, next to the WebSocket `GET /v1/realtime`. Behind
it runs the same session: the same cascade (realtime §4), the same thread binding (chat-voice §8),
the same policy. What changes is how audio and events travel.

**Why.** Kai mobile is meant for use on the move, for example over a WireGuard tunnel to the machine
that runs lmgw. A WebSocket carries PCM16 over TCP: one lost segment on a cellular link holds back
every byte behind it, and the client has no jitter buffer, so speech arrives in bursts and gaps.
WebRTC gives the phone libwebrtc's echo cancellation, noise suppression and NetEQ jitter buffer,
and Opus with in-band FEC over UDP, where a lost packet costs 20 ms of concealed audio and nothing
else. It also encrypts the media (DTLS-SRTP), which a plain-HTTP WebSocket does not.

| § | Content | WP |
|---|---|---|
| 1 | OpenAI's WebRTC surface: facts | — |
| 2 | The route: request shapes, the handshake, `chat_folder` and the turn mode, the answer, refusals, hangup, binding at open, `takeover=ask` | 2 |
| 3 | The UDP listener, the call's candidate, the driver, str0m's configuration, WireGuard | 2, 7 |
| 4 | Audio in: Opus decode, reorder, FEC, concealment, the wall clock, the commit window, the first frame | 3 |
| 5 | Audio out: the frame clock, encoding, the playing window and playout reports, heard | 3 |
| 6 | Events on the data channel, `output_audio_buffer.*`, the stop, size bounds and parts, the bound call's events | 4 |
| 7 | The transport seam | 1 |
| 8 | Liveness, the End, `lmgw.session.closed`, `lmgw-client` | 5 |
| 9 | Settings and surfaces | 2, 3, 6 |
| 10–12 | Security, dependencies, clients | — |
| 13 | Tests and the libwebrtc interop rig | all, 6 |

## Decided by the owner (2026-10-14)

1. **Mobile Kai is a native Android app** (Kotlin, Compose) over the Rust `lmgw-client` crate
   through UniFFI. iOS comes later.
2. **Mobile use goes over WireGuard.** WebRTC realtime (`/v1/realtime/calls`, OpenAI's shape) is
   in scope for it.
3. **lmgw stays OpenAI-compatible; Kai clients may use lmgw's own API where it is cleaner** (the
   owner's answer to Q1 and Q4, §15): lmgw stays compatible with OpenAI on its OpenAI-shaped routes, and Kai clients, which
   only work with lmgw, may use custom lmgw APIs where the cleaner approach needs one. This
   design reads it as three rules:
   - a stock OpenAI client sees OpenAI's behaviour, unchanged and OpenAI-shaped, on every call it
     can make;
   - lmgw's extensions appear only on calls bound to a Chat thread or folder (`chat_thread`,
     `chat_folder`: lmgw's own parameters, which no stock client sends), as `lmgw.*` events and
     `session.lmgw` fields, or on lmgw's own routes;
   - "minimal lmgw-only additions" is no longer a design goal for Kai-facing behaviour. Where an
     lmgw event makes the Android client simpler or exact, the design takes it (C10–C14, C16, C17,
     C21, C23, C24).

Standing rulings that bind this design, and where they apply:

4. **OpenAI-shaped routes copy OpenAI's protocol** for what OpenAI defines, now bounded by ruling 3.
   §1 is the protocol as OpenAI has it; §2 and §6 follow it, and an unbound call is exactly that
   protocol: it closes without a reason (§8.2) and keeps v2's emptied fields for an oversized event
   (§6.3). A bound call adds lmgw's events beside OpenAI's and changes the shape of none (§6.4).
5. **Alpha: no backward-compatibility constraint on lmgw's own wire**; the cleaner implementation
   wins. It applies to the seam (§7), the handshake's split into check and bind (§2.8), the
   `call_id` refusal on the WebSocket route (§2.7), the bound call's events (§6.4) and
   `lmgw-client`'s additions (§8.3), `CloseKind` among them.
6. **No hidden limits.** Every buffer depth, window or cap is a visible setting or derived from a
   real value, and an overrun is a visible error: the reorder and commit windows (§4.2, §4.4), the
   concealment bound (§4.2), the audio kept before the session (§4.6), the data channel's size
   bounds and parts (§6.3: the peer's real `max-message-size` out, `realtime.max_message_mb` for
   joined parts in), the playout reports' fallback (§5.3), the liveness and pending bounds (§8.1,
   §2.8), and the library constants listed in §3.5.
7. **A configured alias fallback always applies.** A call resolves its models exactly as a
   WebSocket session does (§2.2); nothing here routes models.

The owner's answers to Q2 and Q3 (§15, 2026-10-14), each as recommended:

8. **The UDP port's default is the HTTP port's number** (Q2, C5), refined by the device link's
   approved Q2 (device-link §8, D21): calls POSTed to the device listener use UDP sockets on the
   device listener's addresses and port, which follow that listener live (the device group, C4,
   §3.1).
9. **WebRTC is on by default** (Q3), with the switch in the WebRTC group (§3.1, §9).

**Shared with the Android design.** The contract both records follow: the bound call's events in
one table (§6.4, C24); `chat_folder` and `session.lmgw.turn_mode` (C23); the takeover on
`takeover=ask` (C21); the stop and the barge-in verdict (C12); heard (C11) and the playout reports
(C10); `lmgw.session.closed` (C17, Q1); parts (C14, Q4); the End (C16); the starved event (C22);
the RTP kept from the first frame (C13); the commit window (C8); the `session` part (C1); and the
Later list (the pre-roll event, the exact push-to-talk cut, the in-place rebind). The appendices
map each item.

## Taken in this draft

**C1. OpenAI's route, both request shapes, one handshake.** `POST /v1/realtime/calls` takes
`multipart/form-data` (`sdp`, plus an optional `session`, the GA session object) or a raw
`application/sdp` body. Both authenticate with an ordinary bearer: lmgw has no ephemeral keys, and
OpenAI's own Agents SDK posts raw SDP with whatever key it holds. The WebSocket upgrade's query
parameters (`model`, `chat_thread`, `chat_folder` with `new`, `takeover`) work on `/calls` exactly
as on `/v1/realtime`; `takeover=ask` is the calls' own (C21). The `session` part is applied inside
the session as its first update, so `Core::session_update` stays the one path for session changes.
The answer is `201 Created` with the SDP answer and a relative
`Location: /v1/realtime/calls/{call_id}`. `hangup` is served. (§2)

**C2. One refusal is added; the rest is the router's.** A `call_id` on the WebSocket route is
silently dropped today and an unrelated session opens; once calls exist that is a trap, so it
becomes 400 `sideband_unsupported`, documented in `/v1/realtime`'s `DocRoute`. Ephemeral client
secrets, SIP and the beta WebRTC endpoint get no route of their own: the router already answers
405 for each (§2.7).

**C3. The concurrency slot covers a call from its POST; hangup takes none.** A call takes the
key's slot in the gate, as an upgrade does, and holds it until the call ends. `hangup` ends work
and starts none, so it is admitted without a slot or an rpm count: a key at its concurrency limit
can still hang up. It answers only once the call's binding is let go, so a call posted right after
it is neither refused (`takeover=never`) nor left pending (`takeover=ask`) by the call it ended.
(§2.3, §2.6)

**C4. str0m in ICE-lite, one UDP socket per listener address in two groups read by one driver task,
each datagram's destination read from the kernel.** The server side is str0m (sans-IO, Rust, MIT): ICE-lite, DTLS-SRTP, SCTP data channels,
RTCP, RED. One task owns the socket and every call's `Rtc`. str0m drops a STUN request whose
destination is not one of the call's host candidates, and a wildcard-bound socket cannot tell the
destination by itself, so the socket is read through `quinn-udp` (in the tree through quinn),
which hands each datagram's destination address over. No STUN, no TURN: over WireGuard the phone
reaches the server's address directly. *Why not the others:* webrtc-rs 0.21 is a fresh rewrite
whose ICE-lite is unverified (the fallback); LiveKit's `libwebrtc` crate downloads Google's prebuilt
browser engine, which a server does not need. (§3, §11)
*Changed 2026-10-14 (the socket groups):* "one UDP socket for every call" becomes **one socket per
listener address, in two groups, read by one driver task**. The plain group, for calls POSTed to
the plain listener (stock clients), has one socket per address of `bind_addr` on `webrtc_port`
and applies on restart. The device group, for calls POSTed to the device link's listener, has one
socket per address that listener binds (two for its loopback pair, one for `[::]`) on its port,
and follows it live. A call uses the socket of its group whose address matches its candidate. A
wildcard socket still reads each datagram's destination through `quinn-udp`. (§3.1, §3.3)

**C5. The UDP port is a setting whose default is the HTTP port's number.** TCP and UDP ports are
separate namespaces, so the owner opens one number for both, and a dev instance on another HTTP
port gets another UDP port by the same rule. The socket binds the HTTP listener's IP. (§3.1, Q2;
on by default, Q3)
*Changed after the owner's answers, 2026-10-14, and with the socket groups (C4):* Q2 is approved.
The rule above is the plain group's: `webrtc_port`, by default the HTTP port's number, on
`bind_addr`'s IP. The device link's approved Q2 refines it for the calls POSTed to the device
listener (device-link §8): their group binds the device listener's addresses (device-link D24's
`loopback` pair `127.0.0.1` and `[::1]`, `[::]` for `all`, or its concrete address) on the device
listener's port number, and follows that listener live. `webrtc_port` does not apply to the device
group. (§3.1)

**C6. A call's one candidate is the local address its POST arrived on.** The phone reached lmgw at
that address, over the tunnel. The answer names nothing else: no setting, no interface
enumeration, and no LAN address that would let the media bypass the tunnel. (§3.2)

**C7. Opus audio and the `oai-events` channel, nothing else.** The answer keeps Opus, with RED for
Opus when the offer carries it, and drops every other codec; realtime already refuses G.711. An
offer without Opus or without a data channel is refused before any media flows. (§2.5)

**C8. Inbound Opus decodes straight onto the session's timeline.** libopus decodes to 24 kHz mono
itself (no resampler); frames are placed on the wall clock by RTP time, not by packet arrival. A
reorder window (`realtime.webrtc_reorder_ms`, default 60) waits for a missing packet; then in-band
FEC recovers it from the next packet, else Opus concealment fills it. Concealment never exceeds the
wall-clock time that passed; a larger RTP jump re-anchors the timeline. A push-to-talk commit takes
every frame stamped up to its own stamp plus a commit window (`realtime.webrtc_commit_window_ms`,
default 120), which is not tied to the reorder window. The window stays until the Android design's
WP0 and the interop rig decide on the exact cut, `lmgw.input_audio_buffer.commit` and `.clear` with
times from the phone's RTCP sender reports (§4.4, Later). (§4)

**C9. Outbound, the RTP clock paces.** One 20 ms Opus frame per 20 ms for the call's whole life:
TTS audio while a response speaks, silence otherwise. The writer's lead collapses to 0: the phone's
NetEQ is the client buffer the lead stood in for. In-band FEC is on, tuned to the loss the phone
reports. DTX only when the phone's offer asks for it (`usedtx=1`, RFC 7587). (§5.1, §5.2)

**C10. The window ends where the phone has played the audio, as the phone reports it.** While
audio plays, a bound client sends `lmgw.output_audio.playout {playout_ntp_ms, jitter_buffer_ms}` at
its own cadence (the Android client's visible setting, about 1 s). From the latest report lmgw
measures the phone's playout delay, from a frame's send to its playout, and takes the window's end
and drained from it. The formula stands in before the first report and on an unbound call: the
last frame's send end plus a playout allowance from the jitter the phone's receiver reports carry.
When reports stop, the call falls back to the formula with an INFO line. `response.done` waits for
drained, the phone's playback end, as it waits for the modelled playback end on a WebSocket
(realtime's Q1). The barge-in margin is the round trip plus `echo_tail_ms`, as on a WebSocket, with
the round trip from RTCP. Nothing is counted twice. (§5.3)

**C11. Heard is what the phone played when it says so, else what was sent.** A bound client's stop
carries `playout_ntp_ms`, libwebrtc's playout position at the tap (`getStats`'
`estimatedPlayoutTimestamp`), which is in lmgw's own RTCP sender-report clock. lmgw maps it to the
frame and the item's offset in it and cuts the heard table there. Without it, heard is what
`WriterHandle::purge` reports as sent: what NetEQ holds still plays on a client that does not mute,
and libwebrtc offers no flush of a remote track. A barge-in on a bound call keeps what was sent
less the playout allowance's worth, since the phone mutes on `cleared`.
`conversation.item.truncate` keeps working. (§5.4)

**C12. The stop is one lmgw event on a bound call, OpenAI's form stays for stock clients, and the
barge-in verdict is `output_audio_buffer.cleared`.** A bound client stops with
`lmgw.response.stop {playout_ntp_ms?}`. It is idempotent: it cancels whatever is open (a response
owed or held, thinking, speaking or waiting on a tool) and purges the unsent audio. It brings the
cancelled `response.done` and the one `cleared` when there was something to cut, and never
`response_cancel_not_active`. The Android client mutes the remote track at the tap. A stock client
keeps OpenAI's form, `response.cancel` then `output_audio_buffer.clear`, with OpenAI's events. A
call emits `output_audio_buffer.started`, `.stopped` and `.cleared`. `cleared` is the confirmed
cut: it is sent only once lmgw has stopped sending a response's audio, after the barge-in gate and
never on a `speech_started` alone. So the phone mutes on it and unmutes at the next `started`. A
WebSocket session keeps refusing `clear` with `not_implemented_yet`, reworded: it is WebRTC-only in
OpenAI's API. (§6.2)

**C13. The data channel carries no audio; RTP counts from its first frame.** No
`response.output_audio.delta` on a call (the audio is on RTP), and `input_audio_buffer.append` is
answered with an `error`. RTP that arrives before `oai-events` opens and before the bind is kept and
placed on the session's timeline when the session starts (discarded under `bind_pending`), so the
phone shows its listening cue at DTLS-connected. Audio captured before the peer connection exists
(`lmgw.input_audio.preroll`) is decided after the Android design's WP0 measurement, on its merits.
(§4.6, §6.1, Later)

**C14. The data channel's size bounds are the stack's, shown, and an oversized event arrives whole
on a bound call.** Inbound, str0m advertises 256 KiB and a conforming sender refuses more itself.
Outbound, the phone's advertised `max-message-size` holds. On a bound call an event larger than
that travels as `lmgw.event.part {id, index, count, data}` messages, each within the limit, which
`lmgw-client` joins before parsing. A bound client may send parts too; lmgw joins them up to
`realtime.max_message_mb`. On an unbound call the event is sent with its largest fields emptied
until it fits, plus one `event_too_large` error per emptied field. str0m's send buffer is sized to
the phone's limit, not left at its 128 KiB default. (§6.3, Q4)

**C15. A transport seam, with no behaviour change on the WebSocket.** The session loop, the inbox,
the writer and the close take a transport-neutral link instead of axum's `Message`; WebSocket and
WebRTC are its two implementations. `SessionInit.limits` becomes per transport. A close is a kind,
which the WebSocket turns into its code and a bound call into `lmgw.session.closed`. WP1 lands the
seam alone, with the full suite green. (§7)

**C16. Liveness from what the phone sends; the End applies everything before it.** A call is dead
after one `realtime.ping_interval_s` in which nothing arrived from the phone (no consent check, no
RTP, no RTCP, no SCTP); the round trip comes from RTCP receiver reports. A call that never
connects, and one that waits in `bind_pending`, is bound by the same interval. A network handover
keeps the call: the phone gathers candidates continually, and the server accepts checks from the
new address. The phone ends a call by sending `lmgw.response.stop` and then closing `oai-events`;
lmgw applies every event that arrived before the close, then ends the call as a hangup does. The
HTTP `hangup` stays, for a stale call id (Reconnect) and for stock clients. (§8.1, §3.6)

**C17. A bound call's last event says why it ends; an unbound call closes as OpenAI's does.**
Before lmgw ends a bound call it sends `lmgw.session.closed {kind, by, message}`, `kind` one of
`taken_over`, `revoked`, `out_of_reach`, `closed`, `shutting_down` and `thread_bound`, then closes
the peer connection once the event is delivered. `lmgw-client` maps the kinds onto `CloseKind`, the
enum the WebSocket's close codes already map to. An unbound call closes with nothing in band. The
Chat feed's `voice.ended` stays, for the other devices; the calling phone no longer needs its feed
for its own end. The phone does not rebind by itself: it shows the reason and offers Reconnect.
(§8.2, Q1)

**C18. Security: the exposure the bearer key already has, and better media.** The DTLS
fingerprints travel in the SDP over lmgw's plain-HTTP listener: WireGuard protects that exchange,
a bare LAN does not, and TLS on the listener closes both; for devices that TLS is
[the device link](2026-10-14-device-link-design.md). The media itself is encrypted, which a
WebSocket's PCM is not. (§10)
*Changed after the owner's answers, 2026-10-14:* device calls exchange their SDP inside mTLS
(device-link §8): a phone POSTs to the device listener, which it verified against the pairing
link's `fp` and which verified the phone's key, so the exposure is closed for them. A stock client
on a non-loopback plain listener keeps it.

**C19. Only the Android client uses it.** Kai desktop and the dashboard's voice panel stay on the
WebSocket: loopback or LAN, no jitter problem, and their client-side truncate is already built.
(§12)

**C20. Dependencies: str0m at a revision with the receiver-report fix, `opusic-c` on the system
libopus, `quinn-udp` from the tree.** str0m's released 0.24.1 lacks #1065 (the loss-fraction
wraparound in receiver reports, which C9's FEC tuning reads), so it is pinned to a main revision
until a release ships it. The `opus` crate cannot link the system library (§11), so the RPM build
gains `opus-devel` and the RPM a dependency on `opus`, rather than cmake and a vendored copy.
(§11)

**C21. A call takes its thread when `oai-events` opens, and under `takeover=ask` it asks first.**
Until then the handshake only checks, mutating nothing. A call that fails at ICE, DTLS or SCTP (a
closed firewall port, say) therefore never ends the desktop's voice session. The bind at open is
authoritative and runs the checks again. Under `takeover=ask`, which the Android client always
uses, a thread held at that moment gets `lmgw.session.bind_pending {held_by}` instead of a session.
The client's `lmgw.session.take_over` takes the thread (the holder closes with `taken_over`) and
the session starts with `session.created`. A holder that ends meanwhile lets the call bind by
itself. Declining is the client's hangup. Audio meanwhile is discarded, and the wait is bounded by
the never-opened interval. The default takes over at once. `takeover=never` refuses: at the POST
with the 409, whose `error.held_by` names the holder, and at the open with
`lmgw.session.closed {kind: "thread_bound", by}`. (§2.8, §8.4)

**C22. lmgw says when a bound call's output runs dry.** The phone cannot see an underrun in
libwebrtc's player as a WebSocket client sees its own. Where the frame clock starts padding a
response with silence, a bound call sends `lmgw.output_audio.starved {response_id, starved: true}`,
and `starved: false` when the response's audio comes again; the phone feeds
`voice::Machine::starved` from it, so a tool turn after its spoken preamble reads as thinking.
(§5.1, §6.4, §8.3)

**C23. `chat_folder` binds a folder's current thread, and lmgw resolves the turn mode.**
`?chat_folder=<id>[&new=true]` is in the shared `RealtimeQuery`, for both transports, exclusive
with `chat_thread` and `model`. The bind takes the folder's current thread by `current`'s rules (a
new one when there is none, it is out of reach, it idled past the folder's idle minutes, or on
`new=true` unless it is empty), atomically under the folder's lock, with `current`'s refusals
(`not_ongoing`, `folder_no_model`, the folder's 404) and the handshake's. The thread bound is
reported in `session.lmgw.resolved.chat_thread`. A bound session carries
`session.lmgw.turn_mode: "automatic" | "push_to_talk"`, which lmgw resolves from the thread's voice
settings. In automatic mode lmgw applies the thread's turn detection itself, or `semantic_vad`
where the thread names none. The client neither re-derives the VAD rule nor reads `voice_resolved`
for it. (§2.2)

**C24. The bound call's own events, in one table.** What lmgw adds for its own clients on a call
travels as `lmgw.*` events beside OpenAI's, and only on a bound call, next to the bound session's
existing `lmgw.chat.*` events. From lmgw: `lmgw.session.bind_pending`, `lmgw.session.closed`,
`lmgw.output_audio.starved` and `lmgw.event.part`. From the client: `lmgw.session.take_over`,
`lmgw.response.stop`, `lmgw.output_audio.playout` and `lmgw.event.part`. No OpenAI event changes
shape. An unbound call sends none of them and answers a client's with `error` `invalid_event`, as
it answers any unknown type. (§6.4)

## What exists today (relied on, not re-derived)

**The route and the handshake**
- `GET /v1/realtime` is `realtime::upgrade`, in the `/v1` group under `Cap::Inference` (`server.rs`:
  the `realtime_api` router, the `CAPABILITY_TABLE` row). Before the 101 it answers 426 for a
  request that is no upgrade, 400 `beta_protocol_unsupported` for `OpenAI-Beta: realtime=v1`, 403
  `cross_origin_refused` for an anonymous upgrade from a foreign `Origin`, 400 for a query that does
  not parse, and the resolution and policy refusals.
- `RealtimeQuery` carries `model`, `chat_thread` (negative: a temporary thread) and `takeover`
  (`always`, the default, or `never`; without `chat_thread` it is 400 `invalid_value`). It has no
  `call_id` field: serde drops it, and the doc comment says it "belongs to the sideband surface and
  is ignored".
- `thread::handshake` (`realtime/thread/bind.rs`) checks and then binds in one function: the chat
  capability, the thread's reach for the caller (404 `chat_thread_not_found`), Admin Chat (409), the
  thread's voice configuration, a device's scope and budget over the thread's aliases, and last the
  bind itself, `chat_live.bind_voice_as` (taking the thread over; the old session closes with 4000)
  or `bind_voice_unless_bound` (409 `chat_thread_bound` under `takeover=never`). An unbound upgrade
  runs `resolve::resolve_chat`, `asr::resolve_asr`, `session::speech::initial` and
  `policy::check_session`. Both build a `session::SessionInit` and call `open`, which upgrades and
  runs `session::run`.
- A session that ends marks its binding as closing first (`closing()`, review F-5), so a
  `takeover=never` rebind right after it is not refused by it.
- Credentials (`server::principal_mw`): a bearer or `x-api-key`; the dashboard cookie (owner rows,
  same origin); on `/v1/realtime` only, the `openai-insecure-api-key.<key>` subprotocol
  (`handshake::subprotocol_key` checks the path). A device key that is unknown, disabled or expired
  is refused at the door.
- The concurrency slot: the gate's policy step takes `admit_session` (a slot, no rpm count) when
  the path is `realtime::PATH` and `admit` otherwise, and offers the guard as a `SlotHandover`;
  `upgrade` takes it into `SessionInit.slot`. Refusal rows on `realtime::PATH` are labelled
  `realtime` (`refusal_label`).
- The router is served with `into_make_service_with_connect_info::<SocketAddr>` (`server/stop.rs`):
  a handler sees the peer's address, not the connection's local one.
- A request with a method a route does not answer gets axum's 405 with `Allow`; a path no plane
  claims falls into the dashboard's GET-only catch-all `/{*path}` (`web/ui.rs`), which answers a
  bare 404 for an API prefix such as `v1/`, and so a POST there gets 405.

**The session**
- `SessionInit` is transport-neutral except `limits` (`handshake::Limits`: the WebSocket message and
  frame bounds from `realtime.max_message_mb` and `max_frame_mb`).
- `session::run(socket: WebSocket, init)` is welded to axum's WebSocket in three places:
  `socket.split()` feeds `writer::spawn(sink)` (a `Sink<Message>`) and `Inbox::spawn(stream)` (a
  `Stream<Item = Result<Message, axum::Error>>`), and the main `select!` matches
  `Message::{Text, Binary, Close, Ping, Pong}`. `liveness::Frame` is
  `Option<Result<Message, axum::Error>>`.
- Closes are WebSocket close frames: 4000 taken over, 4003 revoked (its reason led by the
  `RevokeKind` token), 4004 out of reach, 1001 going away, 1009 a message or frame over the size
  settings, 1011 no pong. The codes live in `lmgw-api-types` (`realtime.rs`);
  `lmgw_client::realtime::close_kind(code, reason)` reads them into `CloseKind`.
- The core (`Core`; `session/events.rs`, `lifecycle`, `output`, `responder`) is channel-based. Input
  enters as `core.on_text(&str, Instant)`, and a `session.update` is `Core::session_update`; output
  leaves through the `Outbox` (`send`, `send_purgeable`, `send_audio`, `pace`, `drained`) to a
  `WriterHandle`. A `response.cancel` with nothing to cancel is answered `error`
  `response_cancel_not_active`, as with OpenAI (`lifecycle/cancel.rs`). Nor does a cancel reach the
  Pending phase (`lifecycle/pending.rs`, realtime §4.3): the automatic response owed to committed
  turns before its `response.created`, and a client's `response.create` held while the user speaks.
- A bound session's thread owns the model, tools, speech models and voice
  (`realtime/thread/owned.rs`); its client owns `audio.input.turn_detection` and
  `session.lmgw.half_duplex` (chat-voice §8.1).
  `session.lmgw` is strict: an unknown key is an `error`. A bound session already sends lmgw's
  events: `lmgw.chat.frame`, `.user`, `.input`, `.reply` and `.thread`, `lmgw.model.state`,
  `lmgw.response.timing`, `lmgw.approval.decided` and `lmgw.task.done` (`protocol/server.rs`).
- A device's session is a `devices::connect(.., LinkKind::Voice)` link (online, last seen), and
  `devices::revoked` ends it with 4003.

**The Chat feed** (client-apps §2; `lmgw-api-types::chat_feed`)
- `voice.bound {thread_id, by}` when a session binds a thread; `voice.ended {thread_id, by, reason,
  taken_over_by}` when it lets go, `reason` one of `closed`, `taken_over`, `thread_gone`, `revoked`
  (the binder's key was revoked, or the thread left the binding device's reach).
- `revoked {reason, message, kind}` when the feed's own key is revoked; the thread's removal
  (`thread.deleted`) when a thread leaves a device's reach (client-apps L3).
- Live events carry their thread's level, and a reader receives only the levels it sees
  (`web/chat_feed/live.rs`, `stream.rs`): 0 every reader, 1 a thread with the self-admin toolset
  (a device only with its admin-tools switch), 2 the owner alone (Admin Chat, a thread being
  deleted). A revoked key's stream sends `revoked` and ends, and nothing queued goes out after it.

**Folders** (client-apps §3.2, §3.3; `web/chat_folders/current.rs`)
- `POST /chat/api/folders/{id}/current {new?}` answers an ongoing folder's current thread,
  `{thread, rolled_over, reason, note}`. Under the folder's lock (`FolderLocks`), from the read to
  the write, it keeps the current thread or makes a new one from the folder's defaults: when there
  is none (`first`); when it is out of the caller's reach (`gone`, which a device is told as
  `first`); when it had no message for the folder's idle minutes (`idle`; a thread a turn answers
  now is never idle); or on `new: true`, unless it has no message (`requested`). A rollover
  records `folder.current` on the feed.
- Refusals: 404 `not_found` (a folder the caller cannot see), 409 `not_ongoing`, 409
  `folder_no_model`.

**Voice settings and `lmgw-client`**
- A thread's turn detection resolves from the thread, else Settings → Chat
  (`web/chat_voice/resolve.rs`), so `voice_resolved.turn_detection` always has a value:
  `semantic_vad`, `server_vad` or `push_to_talk`.
- `lmgw-client` derives a bound session's turn detection on the client:
  `TurnDetection::requested(push_to_talk, resolved)` reads the thread's `voice_resolved` and, in
  automatic mode, takes `push_to_talk` there to mean `semantic_vad`.
- `lmgw_client::realtime::CloseKind` is `Revoked { kind }`, `OutOfReach`, `ShuttingDown`,
  `TakenOver`, `Other { code }` (a close with no meaning of lmgw's) and `Unknown { code }` (an
  application code this build does not know), read by `close_kind(code, reason)`.
  `RevokeKind::of_close_reason` reads a 4003 reason's leading token.

**Audio**
- In: only `audio/pcm` at 24 kHz (`protocol/session.rs`; `audio/pcmu` and `audio/pcma` parse and
  are refused). `Core::input_append` decodes the base64 and calls
  `AudioIn::append(&pcm, arrived, &listen)`: one PCM16 24 kHz timeline, Silero VAD at 16 kHz behind
  rubato, 200 ms slices, the boost-only level normaliser. `arrived` places the samples on the wall
  clock for the barge-in gate (realtime §6.4).
- Out: TTS PCM16 24 kHz enters `Outbox::send_audio(generation, PartRef, Bytes)` in `DELTA_MS`
  (100 ms) chunks. The writer's paced queue releases it as `response.output_audio.delta`,
  base64-encoded as it leaves: the first `output_lead_ms` (default 500) at once, then as fast as it
  plays. `output_audio.done` and `response.done` wait for the drained acknowledgement, the modelled
  playback end (`writer/playback.rs`). The barge-in margin is the ping round trip (least of the
  last five, `liveness::round_trip`) plus `echo_tail_ms`.
- Truncate: `lifecycle/truncate.rs` → `conversation::audio::truncate` →
  `HeardTable::truncate(audio_end_ms)`. A server-side cancel keeps what `WriterHandle::purge`
  reports as sent.
- `output_audio_buffer.clear` parses (`protocol/client.rs`) and is answered with `error`
  `not_implemented_yet` (`session/events.rs`, with stale "work in progress" wording). No server
  `output_audio_buffer.*` event exists. The inference plane's text for `/v1/realtime`
  (`openapi/planes/inference.rs`) says WebRTC is not served.
- `lmgw-client`'s `voice::Machine` already has the fact a call needs from lmgw:
  `starved(id, starved)`, "response `id`'s audio ran dry in the player".

**The listener**
- axum 0.8 (`macros`, `multipart`, `ws`), hyper 1, tokio 1.52. `server::bind` binds `bind_addr`
  (code default `127.0.0.1:8787`) with a `TcpListener`; a `bind_addr` change takes effect on
  restart (`ops/settings_patch.rs`). The listener speaks plain `http://`; TLS for devices is
  designed in [the device link](2026-10-14-device-link-design.md) (a listener of its own with
  mutual TLS), not built. The JSON group's bodies are bounded by `max_body_mb` (`body_limit_mw`).
- No UDP socket exists. `quinn-udp` 0.5.14 is compiled in already, through quinn.
- `/v1/models` lists `/v1/realtime` among every chat model's `capabilities.endpoints` once a
  session could speak (`realtime::advertise`).

**Project rules that bind the build.** A new route needs its `CAPABILITY_TABLE` row, a `DocRoute`,
and the route walk; a new op needs `op_names` and an `OpDoc`; a new `x-lmgw` header needs
`LMGW_HEADERS` (this design adds no op and no header). New logic goes into new child modules.
Tests are modules of the one `tests/it` binary. SQLite writes go through `begin_write` (this design
adds no table).

## 1. OpenAI's WebRTC surface — facts

Read on 2026-10-14 from the openai-openapi master spec (`/realtime/calls` and its subpaths, the
`output_audio_buffer.*` events), OpenAI's WebRTC and server-controls guides, and the source of
`@openai/agents`' WebRTC transport (`openaiRealtimeWebRtc.ts`, main).

### 1.1 Creating a call

- `POST /v1/realtime/calls`, with one of two bodies:
  - `multipart/form-data`: `sdp` (`application/sdp`, required) and `session` (`application/json`,
    optional: `RealtimeSessionCreateRequestGA`, for instance `{"type": "realtime", "model":
    "gpt-realtime"}`), with an API key. This is what an app server does on its client's behalf.
  - `application/sdp` alone, documented for an ephemeral key whose session config travels with
    the token.
- `201 Created`; the body is the SDP answer (`application/sdp`); `Location` is relative,
  `/v1/realtime/calls/{call_id}`.
- OpenAI's sample answer: `a=setup:active`, `a=rtcp-mux`, `a=group:BUNDLE 0 1`; audio payload
  types `111 opus/48000/2`, `0` (PCMU) and `8` (PCMA); host candidates only, one UDP and one TCP
  passive on 443; `m=application … UDP/DTLS/SCTP webrtc-datachannel` with `a=sctp-port:5000`.
- **The Agents SDK** (verified in its source): creates the data channel `oai-events` before
  `createOffer`, adds the microphone track, and posts the raw offer as `application/sdp` with
  `Authorization: Bearer <key>` to `…/v1/realtime/calls`, with no query string. It takes the call
  id from the last path segment of `Location`. When the channel opens it sends `session.update`
  naming the model and waits up to 5 s for `session.updated`. It plays the remote track in an
  `<audio>` element. In a browser it refuses a key that is not `ek_…` unless `useInsecureApiKey`
  is set.

### 1.2 Events

- Events travel as JSON text on `oai-events`, both ways. The spec says nothing of ordering or
  reliability; a browser's channel is ordered and reliable by default, and so is the SDK's.
- WebRTC and SIP only:
  - client `output_audio_buffer.clear`: "cut off the current audio response"; it "should be
    preceded by a `response.cancel`". The SDK's `interrupt()` sends `response.cancel` (when a
    response is open), then `clear`, and **no** `conversation.item.truncate`: the server decides
    what was heard;
  - server `output_audio_buffer.started {response_id}`: the server begins streaming a response's
    audio, after its `response.content_part.added`;
  - `.stopped {response_id}`: the buffer has drained on the server and no more audio is coming;
    emitted after `response.done`;
  - `.cleared {response_id}`: on a VAD interruption (`input_audio_buffer.speech_started`) or a
    client `clear`.
- The audio rides RTP: no `input_audio_buffer.append` and no `response.output_audio.delta` on the
  channel. The SDP negotiates the formats.

### 1.3 The rest of the surface

- `POST /v1/realtime/calls/{call_id}/hangup`: bearer, no body; `200` once termination begins, `404`
  for a call that is not found.
- Sideband: `wss://…/v1/realtime?call_id=<id>` attaches a server-side WebSocket to a call's session.
- SIP only: `accept`, `reject`, `refer` under `/v1/realtime/calls/{call_id}/`.
- Ephemeral keys: `POST /v1/realtime/client_secrets`.
- A newer `/v1/live/sessions` API exists; it is out of scope (Later).

### 1.4 Not verified

OpenAI's server behaviour for: `input_audio_buffer.append` on a WebRTC session;
`audio.input.format` and `audio.output.format` under WebRTC; an offer without a data channel; a
`clear` with nothing playing; whether a `response.cancel` without a `clear` lets audio already
generated play out; and whether its own cut on an interruption also emits
`conversation.item.truncated`. Where this design meets one of them it picks the clean answer and
says so (§2.5, §5.4, §6.1, §6.2).

## 2. The route

### 2.1 Request shapes

- `Content-Type: application/sdp`: the body is the offer.
- `Content-Type: multipart/form-data`: the part `sdp` is the offer (required); the part `session`
  is a GA session object (optional), which must parse as a JSON object and is otherwise read by
  the session (§2.2). Other parts are ignored.
- Anything else: 415.
- Both shapes take an ordinary credential (§2.3). The raw shape is the one OpenAI documents for
  ephemeral keys; lmgw has none, so it takes the raw offer with whatever key the caller holds, as
  the Agents SDK sends it.
- The body is bounded by `max_body_mb`, like the JSON group's (`body_limit_mw`); an offer is a few
  KB. *Changed 2026-10-15 (the review of WP2 part B):* the `session` part is a session event, so
  `realtime.max_message_mb` bounds it too (0: no bound), as it bounds a WebSocket's
  `session.update`: one setting bounds a session event on both transports. The part is read up to
  the bound and refused past it, 400 `invalid_value` with `param: "session"`, naming the setting.

### 2.2 Query, `session`, model

- `?model=`, `?chat_thread=`, `?chat_folder=` with `new`, and `?takeover=` mean what they mean on
  the WebSocket upgrade (`RealtimeQuery`, shared); `takeover=ask` is the calls' own (§2.8). At the
  POST the same steps run in the same order: the beta check, the anonymous cross-origin rule, the
  query, then for a bound call the handshake's checks (§2.8) and for an unbound one
  `resolve_chat`, `resolve_asr`, `speech::initial` and `check_session`.
- **`?chat_folder=<id>[&new=true]` binds a folder's current thread** (C23), on the WebSocket and
  on calls alike:
  - beside `chat_thread` it is 400 `invalid_value`, and beside `model` 400 `owned_by_thread`, as
    for a thread; `new` without `chat_folder` is 400 `invalid_value`, as is `takeover` with neither
    a thread nor a folder;
  - the bind decides the folder's current thread by `current`'s rules and binds it in one step,
    under the folder's lock (`FolderLocks`), so no other client's `current` moves the folder
    between the decision and the bind. The current thread is kept, or a new one is made from the
    folder's defaults when there is none, when it is out of the caller's reach, when it idled past
    the folder's idle minutes, or on `new=true` unless it is empty. A rollover records
    `folder.current` on the feed with its reason, as `current` does, so the other devices follow;
  - its refusals are `current`'s in the handshake's error shape: 404 `chat_folder_not_found` (a
    folder the caller cannot see; `current` says `not_found`, the handshake names what was not
    found, as `chat_thread_not_found` does), 409 `not_ongoing`, 409 `folder_no_model`. Then come
    the thread's own (§2.8), checked on the thread the decision picks: the current one, or for a
    rollover the thread the folder's defaults make, before it is created;
  - the thread bound is reported where a `chat_thread` bind reports it,
    `session.lmgw.resolved.chat_thread` (id, title, temporary, admin_tools). A rollover's reason is
    on the feed's `folder.current`; `resolved` carries no copy;
  - on a call the POST decides without writing (no thread is made, nothing is bound) and the open
    decides again and binds (§2.8); on the WebSocket both happen before the 101.
- **`session.lmgw.turn_mode`** (C23), on every bound session, on either transport:
  - `"automatic"` or `"push_to_talk"`, resolved by lmgw at the bind from the thread's voice
    settings (`voice_resolved.turn_detection`): `push_to_talk` there is `"push_to_talk"`, and
    anything else `"automatic"`;
  - in automatic mode lmgw sets `audio.input.turn_detection` itself: the thread's rule (`server_vad`
    or `semantic_vad`), or `semantic_vad` where the thread names none (its value is `push_to_talk`,
    or one this build does not know). In push-to-talk mode it is `null`;
  - the session starts in that mode, so the first turn already runs in it, and `session.created`
    echoes both fields;
  - a bound client switches mode with `session.update` naming `session.lmgw.turn_mode`, resolved the
    same way. An update that also names `audio.input.turn_detection` must agree with it, as a
    client's echo of the whole session does, else `invalid_value`. Turn detection named alone
    still works, as today, and `turn_mode` follows it. A change to the thread's turn detection
    applies at the next bind;
  - on an unbound session the field is absent, and a client's is refused (`invalid_value`: it
    applies to a session bound to a Chat thread).

  So the client no longer re-derives the VAD rule from `voice_resolved`
  (`TurnDetection::requested`): it shows the mode and sends a switch.

  *Changed 2026-10-14 (WP2 build, a detail the text left open):* an update naming the mode
  already in effect, without `audio.input.turn_detection`, leaves the session's turn detection as
  it is (a client's own `server_vad` threshold or `semantic_vad` eagerness is kept); only a
  switch writes the thread's rule. A `turn_mode` of `null` is refused like an unknown one.
- **The `session` part is the session's first update.** It is not validated before the answer:
  `Core::session_update` applies it as the first event the session handles, right after
  `session.created` and before any event from the client, and answers it as it answers any update,
  with `session.updated` or an `error`. A model it names is a model change, as a `session.update`'s
  is. A bound call takes from it only what a bound session's client owns (turn detection or
  `session.lmgw.turn_mode`, `session.lmgw.half_duplex`, `session.lmgw.echo_tail_ms`), as a bound
  update does. The Android client sends its client-owned fields there, so the first turn already
  runs with them; with the turn mode lmgw's, that is `half_duplex` and `echo_tail_ms`.
  *Changed 2026-10-14 (consistency pass):* "`half_duplex` at most" was wrong; the code accepts `echo_tail_ms` on bound sessions
  (`realtime/thread/owned.rs`).
- Resolution, fallbacks and their echo in `session.lmgw.resolved` are the WebSocket's, unchanged
  (ruling 7).

### 2.3 Credentials, policy, the slot

- `Cap::Inference`, as `/v1/realtime`. Credentials: a bearer or `x-api-key`, a device key, or the
  dashboard's same-origin cookie (allowed; no browser client is planned). The subprotocol key does
  not apply: there is no WebSocket.
- With Require API key off an anonymous call is allowed, and an anonymous POST whose `Origin` is
  foreign is refused 403 `cross_origin_refused`, as an upgrade is (realtime §10.1). CORS is
  permissive, so the rule matters as much here.
- *Changed after the owner's answers, 2026-10-14:* the two bullets above hold for the plain
  listener, as written, for stock clients. On the device listener the principal is the
  certificate's device (device-link §4.7), and devices present no key anywhere, so "a device key"
  above no longer arises (device-link D12). An anonymous call, the cookie and the `Origin` rule do
  not arise there either: a connection without a certificate reaches no route but enrolment, and
  the device listener has no CORS layer (device-link §4.6).
- Everything that can fail cheaply fails before the answer, as before the 101 (realtime §10.2): no
  `Rtc` is made for a refused call.
- The gate takes `admit_session` for `/v1/realtime/calls` as for `/v1/realtime`; the handler takes
  the `SlotHandover`, and the call holds it from the answer until it ends: through a call that
  never opens, a hangup, the phone's close, a dead verdict, a takeover, a revocation or a stop.
- `refusal_label` labels `/v1/realtime/calls` refusals `realtime`.

### 2.4 The answer

- Each call gets an `Rtc` configured as §3.5 and one host candidate, the address of §3.2
  (`Candidate::host`, UDP, the port of its socket group, §3.1, *changed 2026-10-14*).
  `SdpApi::accept_offer` makes the answer: `a=ice-lite`, a fresh `ice-ufrag` and `ice-pwd`, the
  fingerprint of the call's DTLS certificate, `a=rtcp-mux`, BUNDLE. The DTLS role is what str0m
  answers to the offer's `actpass` (OpenAI answers `active`; libwebrtc takes either).
- Audio: Opus only, with RED for Opus when the offer carries `red/48000/2` for it. The Opus `fmtp`
  is str0m's (`minptime=10;useinbandfec=1`, verified in its codec config): it asks the phone for
  in-band FEC and, by naming no `usedtx`, for a continuous stream.
- `201 Created`, `Content-Type: application/sdp`, `Location: /v1/realtime/calls/rtc_<24 hex>`.
  Call ids are random and carry the prefix `rtc_`, apart from the `call_` of function calls
  (`realtime::ids`).
- The session does not start with the answer: it starts when `oai-events` opens (§2.8).

### 2.5 Refusals

| Status | `code` | When |
|---|---|---|
| 400 | `invalid_value` | the query does not parse; `chat_folder` beside `chat_thread`; `new` without `chat_folder`; `takeover` with neither; or the `session` part is not a JSON object, or is over `realtime.max_message_mb` (§2.1) |
| 400 | `owned_by_thread` | `model` beside `chat_thread` or `chat_folder` |
| 400 | `bad_request` | a multipart body that does not parse (*changed 2026-10-15*, the review of WP2 part B: the row was missing) |
| 400 | `missing_sdp` | a multipart body without `sdp` |
| 400 | `invalid_sdp` | the offer does not parse (str0m's reason quoted) |
| 400 | `unsupported_offer` | no Opus audio, or no data channel ("create the data channel `oai-events` before `createOffer`") |
| 400 | `beta_protocol_unsupported` | `OpenAI-Beta: realtime=v1` |
| 403 | `cross_origin_refused` | anonymous, foreign `Origin` |
| 404 | `chat_folder_not_found` | `chat_folder` names a folder the caller cannot see (§2.2) |
| 409 | `not_ongoing`, `folder_no_model` | `chat_folder` names a folder that is not an ongoing conversation, or whose defaults name no model (§2.2) |
| 409 | `chat_thread_bound` | `takeover=never` and the thread is bound now; `error.held_by` names the holder (§2.8). Under `ask` and the default a held thread is no refusal at the POST |
| 413 | the body-limit refusal | the body is over `max_body_mb` |
| 415 | `unsupported_media_type` | neither `application/sdp` nor `multipart/form-data` |
| 503 | `webrtc_unavailable` | the UDP listener is off (`realtime.webrtc_enabled`), or the socket group of the listener the POST came in on failed to bind (*changed 2026-10-14*, §3.1); the message names the setting or the bind error |

The WebSocket handshake's resolution and policy refusals (an unknown model, scope, budget, the
thread's reach, the device refusals) come unchanged. OpenAI's answer to an offer without a data
channel is unverified (§1.4); an events channel is how every client of lmgw talks to its session,
so lmgw requires one. One audio stream per call; a second offer (renegotiation, an ICE restart) is
not served (Later).

### 2.6 Hangup

- `POST /v1/realtime/calls/{call_id}/hangup`, `Cap::Inference`.
- Allowed: the key that created the call, and the owner (an owner key or the dashboard's cookie).
  Anyone else, and an unknown or ended call: 404 `call_not_found`, so a device never learns that
  another's call exists.
- `200` with an empty body once the call's end has begun **and its binding is marked closing**
  (the F-5 rule), so a call posted right after the 200 is neither refused (`takeover=never`) nor
  left pending (`takeover=ask`) by the call it ended. The session ends as if the phone had closed:
  the bound journal drains, the slot is released, the feed says `voice.ended {reason: "closed"}`. A
  bound call whose channel is still open gets `lmgw.session.closed {kind: "closed", by}` first,
  `by` naming who hung up (§8.2). A call whose channel never opened, or that waits in
  `bind_pending`, ends the same way, with no session to end.
- No slot and no rpm count (C3). The gate is told so by path, as it is told about `/v1/realtime`.
- The Android client's Reconnect starts with this for its stale call id; a 404 (lmgw ended the
  call already) is as good as a 200 to it (§12).

### 2.7 Not served

- **`?call_id=` on `GET /v1/realtime`**: 400 `sideband_unsupported` ("lmgw serves no sideband
  connection to a call"). Today the parameter is dropped and an unrelated session opens; with calls
  served that is a trap. It is the one refusal this design adds, and `/v1/realtime`'s `DocRoute`
  documents it among its responses. *Changed 2026-10-15 (WP2 part B):* refused on `POST
  /v1/realtime/calls` too: the field lives in the shared `RealtimeQuery`, whose documentation
  serves both routes, and no client sends it there (§3.8).
- Nothing else gets a route. What the router does without one, read from the router (not run):
  - `POST /v1/realtime`, the beta WebRTC endpoint (an SDP posted to the WebSocket's path): axum's
    405 with `Allow: GET` from the WebSocket route;
  - `POST /v1/realtime/client_secrets` and `POST /v1/realtime/calls/{call_id}/accept`, `/reject`,
    `/refer`: no `/v1` route matches, so the dashboard's GET-only catch-all does, and a POST gets
    405 with `Allow: GET,HEAD`; a GET there gets the catch-all's bare 404 for an API prefix.
- None of these has a body that explains, and the 405's `Allow` is no invitation to GET; a client
  surfaces the status (the Agents SDK says "Realtime call request failed with status 405"). They
  get no `DocRoute` (there is no route) and no `openapi/exclusions.rs` row (that list is for
  `CAPABILITY_TABLE` rows the document deliberately leaves undocumented). All are on the Later
  list.

### 2.8 Binding when the channel opens (C21)

- **At the POST a bound call only checks**, mutating nothing: the chat capability, the reach of the
  thread (for `chat_folder`, of the folder and of the thread `current`'s rules would pick, §2.2),
  Admin Chat, the thread's voice configuration, a device's scope and budget over the thread's
  aliases. Under `takeover=never` a thread bound by another session is the 409 at once; under the
  default and `ask` it is no refusal yet. No session is told anything, no thread is made and
  nothing is bound.
- **When `oai-events` opens, the bind runs for real**: the same checks again (seconds have passed,
  and reach, scope or budget may have moved; for `chat_folder` the decision itself, under the
  folder's lock), then by `takeover`:
  - left out (`always`): `bind_voice_as`, taking the thread from whoever holds it then, since the
    phone's user asked for it most recently;
  - `never`: `bind_voice_unless_bound`. A thread held now ends the call with
    `lmgw.session.closed {kind: "thread_bound", by}`, `by` the holder (§8.2);
  - `ask`: `bind_voice_unless_bound`. A thread held now makes the call **pending** (below).
- **A refusal at the open** ends the call before any session starts, and on a bound call its last
  event is `lmgw.session.closed` (§8.2): `thread_bound` as above; `out_of_reach` for a thread or
  folder that left the caller's reach meanwhile; `revoked` for a key revoked meanwhile. Any other
  refusal (a policy code, `chat_thread_admin`, `folder_no_model`, `not_ongoing`) is one ordinary
  `error` event carrying the handshake's code and message, followed by
  `lmgw.session.closed {kind: "closed"}`.
- **Pending, under `takeover=ask`.** A thread held when `oai-events` opens gets
  `lmgw.session.bind_pending {held_by}` instead of `session.created`, `held_by` naming the holder as
  `voice.bound.by` does ("device 'desktop'", "the dashboard"). No session runs:
  - the client's `lmgw.session.take_over` makes lmgw run the checks once more and take the thread
    (`bind_voice_as`). The holder closes as any taken-over session does: a WebSocket with 4000, a
    bound call with `lmgw.session.closed {kind: "taken_over", by}` naming this call's binder. Then
    this call's session starts with `session.created`. A check that fails now ends the call as a
    refusal at the open does. A `take_over` outside the wait (a second one, or one that crossed an
    automatic bind) is ignored with a DEBUG line;
  - a holder that ends meanwhile (a hangup, its own close, a dead verdict) lets the call bind on its
    own: `bind_voice_unless_bound` runs again, and `session.created` follows with no word from the
    client. `chat_live` gains a per-thread signal for a binding's release (WP2);
    *Changed 2026-10-14 (WP2 build):* the bind that runs again is the whole open-time bind,
    `thread::check_and_bind` under `ask`: the checks once more and, for `chat_folder`, the
    folder's decision again under its lock, as `take_over` has them. One rule for every bind a
    call makes; a check that fails ends the call as a refusal at the open does. The signal
    (`chat_live`'s `VoiceRelease`) wakes when the holder's session starts to end (`closing`, after
    which it no longer holds against the bind), when its binding goes, and when another client
    binds the thread (the new holder's `bind_pending`); it is subscribed under the slot's lock
    with the look at the binding, so nothing between the two is missed;
  - a holder that changes (a third client took the thread over) brings another `bind_pending`
    naming the new holder;
  - declining is the client's hangup: closing `oai-events`, or `hangup`;
  - every other client event while pending is answered `error` `bind_pending`, naming its type, and
    dropped. Inbound audio is discarded, the audio kept since the first frame included (§4.6): what
    the user says while deciding is not the thread's;
  - the wait is bounded by the never-opened interval (§8.1): one `realtime.ping_interval_s` after
    `bind_pending` the call ends with `lmgw.session.closed {kind: "thread_bound", by}`. With
    `ping_interval_s` at 0 there is no bound, as for a call that never opens; *(changed
    2026-10-15, WP2 part B: after the latest `bind_pending`, so a new holder is asked about for a
    whole interval, §3.8)*
  - for `chat_folder`, `take_over` decides the folder's current thread again under its lock and
    binds that one: if the folder moved on meanwhile, the new thread is bound and the old one's
    holder keeps the old one;
  - the slot is held throughout, as from the POST (C3), and the dead verdict applies as at any
    time.
- **Only then does the session start**: `session.created`, `voice.bound` on the feed, the device's
  Voice link, the bound warm, then the `session` part's update (§2.2), then the audio kept since the
  first frame when the bind did not wait (§4.6). An unbound call's session starts at the open too,
  so every call has one rule.
- **So a call that dies at ICE, DTLS or SCTP** (a closed firewall port) leaves the desktop's voice
  session alone; it ends by the never-connected bound (§8.1), and its slot goes back.
- `thread::handshake` splits into `thread::check` and `thread::bind`; `bind` gains the folder's
  decision under its lock and the pending outcome. The WebSocket upgrade runs both back to back,
  with no behaviour change for `chat_thread` (WP2). `takeover=ask` on the WebSocket is 400
  `invalid_value` ("takeover=ask applies to WebRTC calls"): the upgrade binds before its 101, with
  no channel to ask on.
- **The holder, as a field.** The 409 `chat_thread_bound` names the holder today only inside a
  sentence written for developers ("voice is in use on device 'desktop': this bind asked not to
  take it over (takeover=never)"). WP2 adds `error.held_by`, the holder exactly as
  `voice.bound.by` names it, so a client shows it as it is without parsing the message; the message
  stays. It is a field on lmgw's own refusal (`chat_thread` is lmgw's parameter, not OpenAI's), and
  `lmgw-client`'s `read_refusal` reads it. `bind_pending.held_by` and `lmgw.session.closed.by` name
  the holder the same way.
- **The Android client always binds with `takeover=ask`.** The question "take over from the
  desktop?" is asked on a call that is already connected: no 409 round trip, no second call, and
  the media path is up when the user taps Take over.

## 3. The UDP listener and the call driver

### 3.1 Sockets and ports

*Changed 2026-10-14, after the owner's answers (device-link §8) and with the socket groups (C4):*
v3 bound one socket at start, on `bind_addr`'s IP and `webrtc_port`, for every call. Calls now
arrive on two listeners, the plain one and the device link's, so the UDP sockets are **grouped
per listener**, each group follows its own listener, and one driver task reads them all (§3.3).

- **The plain group** serves calls POSTed to the plain listener, which are stock clients. It has
  one UDP socket per address of `bind_addr` (one today; a wildcard is one socket, whose datagrams'
  destinations §3.3 reads through quinn-udp), bound at start beside the HTTP listener. Its port is
  `realtime.webrtc_port`, which is `null` by default, meaning the HTTP port's number (C5, Q2); `0`
  asks the OS for one (the tests use it; the status shows the result).
- **The device group** serves calls POSTed to the device listener (device-link §4). It has one UDP
  socket per address the device listener binds: two for `loopback` (`127.0.0.1` and `[::1]`), one
  for `all` (`[::]`), one for a concrete address. Its port is the device listener's port number
  (`devices.port`); `webrtc_port` does not apply to it. It exists while the device listener runs.
  - *Why the device group:* phones POST to the device listener, whose local address becomes the
    candidate (§3.2), and it turns on at the first pairing without a restart; a socket bound only
    at start would fail the first call after it, and with `bind_addr` on loopback a socket on
    `bind_addr`'s IP has nothing behind a phone's candidate.
  - *Why the plain group beside it:* a stock client on a non-loopback `bind_addr` keeps a socket
    behind its candidate, and a change of the device listener never ends a stock call, which keeps
    the device link's "never the plain listener's" (device-link D10, §4.4).
- **A call's socket** is the one of its group (the listener its POST came in on, `Conn::via`)
  whose address matches its candidate, which is still the local address the POST arrived on
  (§3.2). A wildcard socket matches every address. The candidate's port is the group's port.
- `realtime.webrtc_enabled` (default on, Q3) off means no socket in either group.
- Each socket is a `tokio::net::UdpSocket` with `quinn-udp`'s `UdpSocketState`, which turns on
  `IP_PKTINFO` and `IPV6_RECVPKTINFO` and is driven from tokio as quinn drives it (`try_io` around
  quinn-udp's `recv` and `try_send`). A receive may carry several datagrams of one stride (GRO);
  the driver splits them.
- A failed bind does not stop lmgw. The HTTP side serves; `/calls` on the listener whose group
  failed answers 503 `webrtc_unavailable` with the bind error; the status and the Realtime
  settings show it for that group.
- **The device group follows the device listener live.** Each change of the device listener (on,
  off, scope, port) closes the device group's sockets and binds the new ones at once. Only device
  calls end, as at a stop: a bound call's last event is `lmgw.session.closed {kind:
  "shutting_down"}` (§8.2), an unbound call closes with nothing in band, and the device listener's
  confirmation names them (device-link §4.1). The plain group and its calls are untouched.
- **The plain group applies on restart**, like `bind_addr`, and so do `webrtc_enabled` and
  `webrtc_port`. The restart path that waits for the old TCP listener's release (`serve`'s
  `released`) covers the TCP listeners and both groups. *(Changed 2026-10-15, WP7: it covers the
  plain listener and the plain group; the device group, like the device listener's TCP side, is
  closed before it binds again by its one follower, so it needs no release wait, §3.9.)*
- **The firewall** must let the UDP port in on the interface the caller comes in on. A phone
  reaches lmgw through the device listener, so the firewall opens TCP and UDP of the device port
  in that interface's zone (device-link §12; on Fedora's Workstation zone both are open already).
  The plain group's UDP port matters only to stock clients on other machines, so only when
  `bind_addr` is not loopback. The settings group's firewall hint says so: the device port, TCP
  and UDP, for phones; the plain group's port only for a non-loopback `bind_addr`.

### 3.2 The candidate

- **A call's one host candidate is the local address of the TCP connection that carried its
  POST**, on the UDP port. The connect info the router is served with gains the connection's local
  address beside the peer's (read from `IncomingStream::io().local_addr()`), and the call reads it.
  *Changed after the owner's answers, 2026-10-14:* that connect info is
  `server::Conn { via, peer, local, device }`, shared with the device link (device-link §4.5): both
  listeners serve with it, and the call reads `Conn::local`. Whichever work package lands first
  introduces `Conn`. `Conn::via` picks the call's socket group, and the port is that group's
  (§3.1). An IPv4-mapped address (an IPv4 client on a `[::]` listener) is made canonical
  (`IpAddr::to_canonical`).
- *Why:* the phone reached lmgw at that address, normally over the tunnel, so the media follows
  the path the HTTP request already proved. Any other address would be unreachable from the phone
  or would bypass the tunnel. A loopback POST gets a loopback candidate, so a dev instance, the
  tests and the interop rig work as they are.
- No setting and no interface enumeration. The status lists each call's candidate.
- Not covered: a reverse proxy in front of lmgw, whose connection's local address is the
  proxy-facing one. v1 assumes the phone reaches lmgw's listener directly, as over WireGuard (TLS
  for devices is [the device link](2026-10-14-device-link-design.md), whose listener the phone
  reaches directly too).
- UDP host candidates only: no TCP candidates (OpenAI also offers TCP passive on 443; WireGuard is
  UDP itself), no server-reflexive, no relay.

### 3.3 The driver

- One tokio task owns the socket and every call's `Rtc`. str0m is sans-IO: it takes datagrams and
  timeouts, and gives back transmits, events and its next timeout. *Changed 2026-10-14 (the
  socket groups, C4):* the task owns every socket of both groups (§3.1). The set changes only when
  the device listener does, and the task then drops the device group's sockets and reads the new
  ones.
- **The destination of each datagram comes from the kernel.** str0m's ICE agent drops a STUN
  request whose destination is not one of the call's host candidates (address and protocol; `is`
  0.11.1, `agent.rs`, "Discarding STUN request on unknown interface"). A wildcard-bound socket's own
  address is `0.0.0.0` or `[::]`, and a `[::]` socket reports IPv4 traffic as `::ffff:a.b.c.d`. So
  the driver takes each datagram's destination IP from `quinn-udp`'s `RecvMeta.dst_ip` (the packet
  info), makes it and the source canonical, and gives str0m
  `Receive { proto: Udp, source, destination: (dst_ip, port), contents }`.
- **The source of each reply is set**: str0m's `Transmit.source` (the candidate) becomes
  quinn-udp's `Transmit.src_ip`, so a reply leaves from the address the phone sent to even on a
  host with several addresses, and an IPv4 destination is mapped back to `::ffff:` on a `[::]`
  socket.
- *Why one socket with packet info rather than one socket per address:* per-address sockets need
  a bind for each POST's local address on demand (an interface that came up after the start, a
  WireGuard interface among them), each kept while calls use it, and a select over a changing set.
  One socket bound like the HTTP listener needs none of that, quinn-udp reads packet info on Linux
  already, and the destination is one field per datagram. A concrete bind runs the same code; its
  destination is always its own address. *Changed 2026-10-14 (the socket groups, C4):* the
  sockets are now one per listener address (§3.1). Those are the listeners' own bound addresses,
  known when each listener binds, not each POST's local address, and a wildcard listener address
  is still one socket read through packet info. So the on-demand binds argued against here still
  do not arise.
- A datagram goes to the first call on the socket it arrived on whose `Rtc::accepts` it: str0m's
  demultiplexing by the nominated pair's source address, by the call's ufrag for STUN, else by a
  validated remote address (which str0m 0.24.0 recognises in ICE-lite before nomination). The
  call's `handle_input` runs, then its `poll_output` is drained: transmits go to the call's socket,
  events to the call.
- A datagram no call accepts is dropped; a DEBUG line counts them per minute. A timeout reaches
  every call that is due. Commands from the calls (an encoded frame, a channel message, a close)
  arrive on one channel and are applied to their call's `Rtc`, whose output is then drained.
- The driver does no audio work: Opus payloads go to the call's reader (§4), frames arrive from the
  call's frame clock already encoded (§5). A scan over the calls is enough: a personal gateway runs
  a handful at most, each a few hundred datagrams a second.

### 3.4 The registry

- `CallRegistry` in the gateway state: call id → the key that created it, its candidate, its socket
  group (*added 2026-10-14*, §3.1), its state
  (answered, connected, open, pending with its holder, bound), the session id once it runs, the
  bound thread, created, connected and opened times, the remote address, the latest stats (round
  trip, loss, the phone's jitter, the playout allowance and its source, the latest playout report's
  age), and the handle a hangup signals.
- In memory only: a call is live state; nothing persists and no table is added.

### 3.5 str0m's configuration per call

| str0m | lmgw sets | Why |
|---|---|---|
| `set_ice_lite` | `true` | the server answers checks and sends none |
| local candidates | the one of §3.2 | the only address the phone has proved |
| codecs | `clear_codecs`, then `enable_opus(true, red)` | C7; `red` when the offer carries it |
| `set_reordering_timeout_audio` | `webrtc_reorder_ms` | §4.2 |
| `set_reordering_size_audio` | the window over the packet duration, plus one | so the count never binds before the time does |
| `set_sctp_max_buffered_amount` | the phone's `max-message-size`, at least the default | a message larger than the buffer is never accepted (§6.3) |
| `set_stats_interval` | 1 s | a sampling period: the phone's receiver reports set the real cadence |
| `set_mtu` | left at str0m's 1150-byte target | fits every WireGuard MTU (§3.6) |

The library constants that remain, and where their effect is named: str0m's ICE-lite verdict (a
pair is `Disconnected` 15 s after its last binding request, `RECENT_BINDING_REQUEST` in its `is`
crate; logged, not acted on, §8.1); the `max-message-size` str0m advertises, 256 KiB (§6.3); the
64 KiB it assumes for a peer that advertises none (§6.3).

### 3.6 WireGuard, MTU, NAT and handovers

- WireGuard is a routed layer-3 tunnel: the phone's inner address reaches the server's WireGuard
  address directly, which is all ICE-lite with one host candidate needs.
- If the tunnel ends on another machine that masquerades the phone onto the LAN, the POST arrives
  on lmgw's LAN address, which becomes the candidate, and the checks arrive from that machine's
  address. ICE-lite answers where a check came from (a peer-reflexive pair), so the call works.
- **Handovers.** WireGuard keeps the inner address when the phone moves between cellular and Wi-Fi,
  so the server's candidate stays reachable. The phone's libwebrtc runs with continual gathering
  (`GATHER_CONTINUALLY`, the Android design), so after a network change it gathers its candidates
  again and checks new pairs, should its local socket or port have changed. The server needs
  nothing for it: a check with the call's credentials from a new source address or port makes a
  peer-reflexive pair, and the phone's nomination moves where lmgw sends. With C6 there is no LAN
  candidate that bypasses the tunnel. A test moves the client to a new source port mid-call (§13.1).
- MTU: WireGuard's usual 1420 leaves 1392 bytes of UDP payload in an IPv4 inner packet (1372 in
  IPv6); a 1280 tunnel MTU leaves 1252 and 1232. str0m's 1150-byte datagram target fits them all,
  so there is no setting. Opus packets are about 100 bytes; only the DTLS handshake and SCTP come
  near the target, and str0m fragments both.

### 3.7 As built: WP2 part A, the media plane (2026-10-15)

`realtime/calls.rs` and its children `sockets`, `driver` (`driver/call.rs`), `registry`, `offer`,
`handle` and `link`; `AppState::calls`; tests in `realtime_calls_net`. Details this design left
open, taken as follows. *Changed 2026-10-15 (the review of part A):* the group's close, the
routing, panics, the send window, the owner's budget, the hangup's answer and the dropped POST,
as below.

- **Groups.** `Calls::bind_group(group, addrs)` binds one socket per address (all or none: a
  failure, or sockets the driver did not take, leaves the group `Failed`, naming why). WP7's device
  group calls it and `Calls::close_group`; the driver needs nothing more. The driver closes sockets
  by their ids, so a group bound again meanwhile keeps its new ones. `Calls::group` answers `Off`,
  `Down`, `Bound(addrs)` or `Failed(why)`.
- **A group's close leaves room for the last event.** `close_group` (and `close_group_within`)
  marks the group `Down`, asks each of its calls' owners to end the call (`Ending::ShuttingDown`
  through the registry, so a bound call can send `lmgw.session.closed {kind: "shutting_down"}`,
  §8.2), keeps the sockets open until the calls are gone or `server::STOP_WITHIN` (10 s) has
  passed, and only then closes the rest by force (`Ended::GroupClosed`) and the sockets. An offer
  reads its group again once its call is in the registry and refuses when the group's socket is
  gone, so no call registers unasked behind a close.
- **The plain group's start and stop.** `server::serve` makes one call,
  `Calls::serve_plain(webrtc, listener_addr, released)`, which binds the group and hands back a
  sender that takes `released`'s place: when the listener closes, the group closes as above, and
  only then does `released` fire, so the shell's restart waits for both ports. With
  `webrtc_enabled` off or a failed bind, `released` passes through untouched.
- **Sockets.** A `[::]` socket is dual-stack as the system's `bindv6only` has it, as the HTTP
  listener on `[::]` is (no `socket2`). A wildcard `0.0.0.0` serves IPv4 candidates only: an IPv6
  candidate there is `OfferError::Unavailable` (503). The receive buffer is 64 KiB, the largest UDP
  datagram and the largest GRO batch, so nothing is cut; quinn-udp's stride splits a batch.
  A send the socket would block drops the datagram (UDP is lossy; str0m retransmits) rather than
  making the driver wait; sends bypass tokio's write readiness.
- **Routing.** A binding request goes to the call its USERNAME names (the local ufrag, through the
  driver's map), and to no other, once its MESSAGE-INTEGRITY checks out with that call's password,
  which the driver checks itself (`StunMessage::verify`, HMAC-SHA1 through aws-lc-rs). Both come
  before `accepts`: str0m's fast path takes anything from a call's nominated remote address, STUN
  included, so a stale call whose remote is a new call's source would swallow the new call's checks,
  and a request forged as the client with a bad integrity (the ufrags travel in the clear) would pass
  `accepts` and count as the client's traffic. *Changed 2026-10-15 (second review):* the check was
  left to `accepts`, which let such a forged request move `last_heard` and the source map. A source
  address belongs to the call whose binding request last passed from it, and any other datagram from
  it goes to that call first, then to the first call whose `accepts` takes it. Other STUN (responses,
  indications) is dropped. The classification, the parse and the integrity check run on a
  stranger's bytes under `catch_unwind`; a panic drops the datagram.
- **The driver** reads a socket for at most 64 receives per turn (`READS_PER_TURN`), a scheduling
  turn so a flood on one socket cannot starve the rest; nothing is dropped by it. Dropped
  datagrams are counted (`Calls::dropped`, for the status block) besides the per-minute DEBUG
  line. **A panic in str0m ends one call**: every step on a call's `Rtc` (`accepts`,
  `handle_input` and its drain, a write, the close, and the drop of an `Rtc` it panicked on) runs
  under `catch_unwind`, logged at ERROR, the call ended with `Ended::Failed` and nothing touching its
  `Rtc` again. A call that fails on the answer's first drain is refused (`OfferError::Unavailable`),
  no answer going out for a dead call. A driver that died anyway forgets its calls as its task
  unwinds (the task's `Drop` clears the registry, so nothing lists a dead call and a hangup's
  `Released` resolves), and is started again at the next use, its bound groups `Failed` until their
  listener binds them again.
- **Who ends a call.** The driver ends one by itself only when it never connects or never opens
  (the bound is `realtime.ping_interval_s` at the offer, `CallParams::never_opened`), when the
  client closes the peer connection (`Ended::PeerClosed`), when what waits for its owner passes
  the budget (`Ended::TooBig`, below), on a str0m error or panic, and at a group's close past its
  bound. Everything else is the owner's: `CallHandle` is returned by `Calls::offer` and its
  `CallControl` closes the call, or ends it when dropped; the handle exists before the offer
  waits for the driver, so a POST dropped mid-offer leaves no call (and the driver closes a call
  whose registration nobody hears). A hangup (`Calls::hangup(id, by)`) asks the owner
  (`CallControl::ending`, `Ending::HungUp {by}`; the first reason stands) and returns `Released`,
  which resolves once the call's registry entry is gone: the hangup's 200 waits on it (C3). The
  hangup has no bound of its own when the owner never reacts; part B gives it one
  (`server::STOP_WITHIN`, §8.2's dated note). `Offer::keep` is
  dropped with the call, whoever ends it, and before its registry entry goes, so the slot is free
  when `Released` resolves (C3).
- **What the owner reads** (`CallEvents`, one ordered channel): `Connected`, `Open`,
  `Message {message, at}` (`at`: when its last datagram came off the socket), `ChannelClosed`,
  `Audio(Frame)` (the Opus payload raw, its extended RTP time at 48 kHz, arrival, contiguity,
  sequence numbers), `SenderReport` (§16; its RTP time extended on str0m's own anchor, so WP3
  matches it with a frame's modulo 2^32) and last `Ended(why)`. Audio flows from the first frame
  after DTLS-SRTP, before `Open` (§4.6).
- **What the owner sends.** `CallControl::send` writes the next message only while no more than
  str0m's default send buffer (`RtcConfig::sctp_max_buffered_amount()`, 128 KiB, read from str0m)
  is in flight, else waits for `ChannelBufferedAmountLow` at that mark; sctp-proto says that only on
  a crossing from above, so each tick also looks whether a waiting write fits. The buffer is that
  window plus the largest message, so every write that passes the gate is taken and the
  back-pressure is the same whatever the client advertises. It answers `Sent`,
  `TooLarge {len, limit, by}` (`by`: the client's `max-message-size` or `realtime.max_message_mb`,
  which the log names; WP4's parts) or `Gone`.
- **The seam** (`calls/link.rs`): `split(handle)` gives `CallSink` and the reader, as
  `ws::split` does. The reader reads ahead at most `LOCAL_MAX_MESSAGE` (256 KiB, str0m's), passes
  messages and the end, and passes audio and the transport events by until WP3 decodes and the
  open path reads them; `Ended::TooBig` reaches the session as a failed read that
  `Transport::too_big` reads as `Close::TooBig` naming `realtime.max_message_mb`. `Sink::ping`
  sends nothing (§8.1). `last_heard` in the registry, WP5's evidence, moves only on authenticated
  traffic: a binding request whose integrity the driver checked with the call's password, or a
  datagram str0m made media, channel data, a channel's open or close, a sender report or the
  connection of (each past SRTP's or DTLS's checks).
- **§3.5's table, as configured.** The reorder count is the window over 10 ms plus one: 10 ms is
  the answer's `minptime`, the shortest packet the client may send, so the count never binds before
  the time. The window is `REORDER_UNTIL_WP3`, 60 ms, until WP3 adds `webrtc_reorder_ms`
  (*changed 2026-10-15, WP3 slice A:* `CallParams::from_settings` reads the setting, and the
  constant is gone). **The
  largest message a call sends** is the smaller of the client's `max-message-size` (str0m's
  assumed 64 KiB when it names none; for 0, "any size", sctp-proto's own largest, `u32::MAX`
  bytes, which str0m sets) and `realtime.max_message_mb` (0: no bound), so the buffer is always a
  finite number. The answer's DTLS role to an `actpass` offer is `passive` (tested).
- **The budget of what waits for the owner** (decided in the review): str0m reads SCTP out as it
  arrives with no back-pressure per channel, and RTP cannot be slowed (§4.5), so the bytes of the
  messages and audio payloads queued for a call's owner are counted, added as the driver queues
  them and taken off as `CallEvents` hands them out. Past `realtime.max_message_mb` (0: no bound,
  as the setting says; no setting of its own) the driver ends the call with
  `Ended::TooBig {queued, limit}` and a WARN line naming the setting; what was queued is still
  handed out first.
- **Handovers with a new port** (§3.6, tested): the client's audio and events reach the call from
  its new port at once, and the registry's remote follows its first checked binding request. lmgw's
  own traffic follows only once str0m lets go of the old pair. The mechanism, in str0m's ICE agent
  (`is`, `evaluate_nomination`): a controlled (and ICE-lite) agent sends on the nominated, succeeded
  pair of the highest priority, and on a tie keeps the current one; the new pair is peer-reflexive,
  of a lower priority than the host pair the client left, so it waits until the old pair is pruned,
  15 s after its last binding request (`RECENT_BINDING_REQUEST`, a constant, not configurable). So
  a port change costs up to 15 s of lmgw → phone media, healing by itself; a WireGuard roam that
  keeps the phone's inner address and port changes nothing. *Corrected 2026-10-15 (second
  review):* str0m does offer a way: `DirectApi::invalidate_candidate` drops a candidate and its
  pairs. **Not used, by the owner's decision:** a discarded remote candidate is refused for the rest
  of the call, so a phone that comes back to its old address and port in the same call would be cut
  off for good, which is worse than a 15 s gap that heals, and over WireGuard the address does not
  change.
- **Settings.** `realtime.webrtc_enabled` and `realtime.webrtc_port` live in
  `config/settings/webrtc.rs`, flattened into the realtime section; a stored port that is no port
  reads as `null` with a warning. The save takes `webrtc_port` as a number or `null`, refuses one
  outside 0–65535 naming the setting, and its answer adds "applies at the next start of lmgw" for
  each (both saves).

### 3.8 As built: WP2 part B, the route and the open path (2026-10-15)

`realtime/calls/route.rs` (with `route/body.rs`), `realtime/calls/open.rs` (with
`open/pending.rs`), `realtime/session/call.rs`; the handshake's shared door, query and unbound
resolution in `realtime/handshake.rs`; tests in `realtime_calls`. Details this design left open,
taken as follows (*changed 2026-10-15*):

- **The POST's order.** The door (beta, the anonymous cross-origin rule), then the body's shape
  (415), the query (its parse, its parameter rules, `call_id`), the listener's socket group (503,
  before anything that costs, the body's read included), the body (413, `missing_sdp`, a
  `session` part that is no JSON object or is over `realtime.max_message_mb`, 400 `invalid_value`
  with `param: "session"`), the bind's checks or the unbound resolution, and last the offer itself
  (`invalid_sdp`, `unsupported_offer`), which the media plane's own step reads. A multipart body
  that does not parse is 400 `bad_request`; an SDP body that is no UTF-8 is `invalid_sdp`. The 503
  has the type `server_error`. *(Changed 2026-10-15, the review of part B: the 503 came after the
  body's read.)*
- **`?call_id=` is refused on both routes** (§2.7): the query is shared, and its documented field
  would otherwise read wrong for one of them.
- **The device listener until WP7.** `Group::of(Conn::via)` has no group for the device listener
  yet: a call POSTed there is 503 `webrtc_unavailable`, the message naming the missing device
  socket group, and `/v1/models` does not list `/v1/realtime/calls` to a request on it. WP7 fills
  `Group::of` and binds the group with the listener. *(Changed 2026-10-15, WP7: done, §3.9;
  `Group::of` answers a group for each listener, and a device call's 503 names its group's state
  as a plain call's does.)*
- **The `session` part** reaches the session as the first event its call's reader passes, a
  `session.update` built from it, so `Core::session_update` stays the one path (§2.2).
- **The open path's events** (`bind_pending`, the refusal's `error`, `lmgw.session.closed`) carry
  `event_id`s of its own minting; the session's start with its own.
- **A bound call's key revoked before the open** is said at the open (`lmgw.session.closed {kind:
  "revoked"}`), not by closing quietly at once: the channel is the phone's only way to learn why,
  and the never-opened bound still caps the wait. An unbound call, which says nothing in band, is
  closed at once. The watch is subscribed at the POST (from the request's revocation mark) and
  looked at before `check_and_bind` and after it; a bind that came back while the key was revoked
  lets its binding go marked revoked (`voice.ended {reason: "revoked"}`).
- **The pending wait** runs one `ping_interval_s` from the **latest** `bind_pending`: a new holder
  asks the user anew. Its `lmgw.session.closed {kind: "thread_bound"}` names the holder and the
  setting.
- **The registry** gains `CallState::Pending` (with `held_by`) and `Running` (with the session's id
  and the bound thread), written by the open path and the session (§3.4).
- **A hangup's `by`** is the caller as `voice.bound.by` names it ("the dashboard", "device
  'phone'"); none for an anonymous caller. With Require API key off, an anonymous caller may hang up
  the anonymous calls (one principal), and no other.
- **A running session hears the call's question** (`session/call.rs`, `CallSide`): `HungUp` closes
  with `Close::HungUp`, `ShuttingDown` (its listener's stop) with `Close::GoingAway`. A bound call's
  `lmgw.session.closed` is queued at each of the session's close sites (taken over, out of reach,
  the server's stop, revoked, the call's question, a `TooBig` read), before the flush and the close,
  in the WebSocket's close order (§8.2). A `lmgw.session.take_over` that reaches a running bound
  session is ignored with a DEBUG line (§2.8).
- **Seams left.** WP3: the audio kept from the first frame is handed to the call's reader
  (`calls::link::Start::kept`) and dropped there until WP3 decodes it ahead of the live frames;
  under `bind_pending` the open path discards every frame; live frames still pass the reader by.
  *(Closed 2026-10-15 by WP3 slice B, §5.5: the reader decodes the kept audio ahead of the live
  frames, and the live frames after it.)* WP5: the wait for the last event's delivery before
  `Rtc::close` (`open::Call::end`, `CallSink::close`), the event on a dead or stalled verdict, and
  the liveness evidence: until WP5 a call's session gets no pong, so with `ping_interval_s` above 0
  it ends as unanswered after two intervals (the session's once-said "no pong echoed" line is a
  WebSocket's, and not said on a call). *(Built 2026-10-15, §8.5.)* WP7: `Group::of`, above
  (built, §3.9).

*Changed 2026-10-15 (the review of part B, decided):*

- **The audio kept before the open is counted** against the call's budget of what waits for its
  owner (`realtime.max_message_mb`, §3.7): `CallEvents` hands a frame out, and the open path's
  `Kept` counts its payload again until it is dropped (by the reader until WP3 decodes it, or at
  `bind_pending`). Past the budget the driver ends the call with `Ended::TooBig`, its WARN line
  naming the setting, as for anything else that waits; a client that streams faster than real
  time and never opens `oai-events` meets it. Under `bind_pending` the kept audio is dropped before
  the wait (§4.6), so it no longer counts while the user decides.
- **Nothing the open path says is waited for in one of its waits' arms.** `CallControl::queue`
  hands the message to the driver at once, behind every message sent before it (the driver keeps
  one queue of writes per call), so the open path's events still go out ahead of the session's
  first one, and only hears it taken. While one is not taken yet the pending wait reads no client
  event, so a client that floods events and takes nothing back fills the call's budget rather than
  lmgw's memory, and the wait's deadline, the holder's release, a revocation, the call's question
  and the server's stop all still end it. The close waits for the last events to be taken at most
  one `ping_interval_s` (none at 0), the grace a WebSocket's last events get (§8.2), then closes
  anyway.
- **The call's end is looked at around each bind**, as the revocation is: before
  `thread::check_and_bind` (at the open, after `bind_pending`'s release, and for `take_over`) and
  after it, whether lmgw asked the owner to end the call (`CallControl::ending`) and whether it is
  still in the registry (a close by force past `STOP_WITHIN`, the client's close, the driver's own
  end). An ended call ends as asked (`lmgw.session.closed` for the hangup or the stop, nothing for
  a call already gone). A binding that came back for it is marked closing and let go, its
  `voice.ended` saying `closed`, so no session starts and a `takeover=never` bind after the
  hangup's 200 is not refused by it. A hangup that lands while the bind runs still lets a
  `takeover` left out take the thread from its holder for that moment, as a revocation landing
  then does; the window is the bind's own reads.
- **str0m reads and answers the offer on a blocking thread** (`spawn_blocking`): an SDP of up to
  `max_body_mb` is parsed off the async workers, and a panic in str0m there is caught as the
  task's, logged at ERROR, and refused: 400 `invalid_sdp` for the read, 503 `webrtc_unavailable`
  for the answer (and for a task the runtime cancelled).
- **`/v1/models` does not list `/v1/realtime/calls` yet** (§9's dated note).
- **A hangup whose call let go as its bound passed** is answered without the WARN and the close by
  force.

### 3.9 As built: WP7, the device socket group (2026-10-15)

`realtime/calls/devices.rs` (the follower), `realtime/calls/status.rs` (the status block's
`webrtc`), `Group::Devices`; tests in `realtime_calls_net::devices` and `realtime_calls::devices`,
and the device form of `realtime_calls::binding`'s revocation. Details this design left open,
taken as follows (*changed 2026-10-15*):

- **The follower.** `Calls::follow_devices` reads `DeviceListener::watch` (`Bound {addrs,
  served_at}`, device-link §4.1): one task per gateway, started by the device listener's arm
  (`DeviceListener::claim`), so the gateway and a suite that arms the listener alone get the same
  group; it lives while the listener does. Each state the listener publishes is applied in order:
  a start binds `Bound.addrs`, the listener's own bound addresses and port; a move, a widening or a
  narrowing closes the old sockets as §3.7's group close does (the owners asked
  `Ending::ShuttingDown`, the sockets kept for the last events until the calls are gone or
  `STOP_WITHIN` has passed, the rest closed by force) and only then binds the new ones; a stop
  closes them and leaves the group `Down`. The latest state is read after the close
  (*review N1*), so a change that came while it waited is bound, not the one the cycle began on,
  and a burst of moves binds where the listener ended up.
- **Every change ends every device call**, a widening from `loopback` to `all` included, though
  `[::]` would serve the old addresses: the device listener moves its generation at each change and
  its sessions and open paths end with it (device-link §4.4), so a call kept on the UDP side would
  have no owner. The group ends what the generation ends.
- **Close, then bind.** §3.1's "closes the device group's sockets and binds the new ones at once"
  cannot keep both halves when the new addresses overlap the old (one port, `loopback` → `all`:
  `[::]` does not bind beside `127.0.0.1` on the same port), and the last event needs the old
  socket. So the close starts at once and the bind follows it. The gap is the time the device calls
  take to end, at most `STOP_WITHIN`; the group reads `Moving` from the follower's
  first step to the new bind, and a device call POSTed in it is refused 503 `webrtc_unavailable`
  with `Retry-After: STOP_WITHIN` (in seconds) and a message saying the group is moving with the
  device listener (*review N2*). The group also records the start it is bound for
  (`Bound.served_at`), and a `Via::Devices` POST on a connection of another start is refused the
  same way, before the body and again at the offer, so a POST in flight on the old connection is
  never answered on the new group's socket (*review N3*). A move that does not overlap
  takes the same path: one rule, not two.
- **A restart needs no release wait for the device group.** §3.1 has the restart path's
  `released` cover "the TCP listeners and both groups". As built it covers the plain listener and
  the plain group (§3.7); the device listener's TCP side needs no wait (its next arm stops what the
  last one left, device-link §4.1), and neither does its group, since the one follower closes the
  old sockets before it binds the new ones.
- **The IPv6 stack is the device listener's.** The device listener sets its own
  (`device_listener::bind`: `[::]` dual-stack, any other IPv6 address IPv6-only), and the device
  group's sockets set the same (`sockets::V6::DualOnWildcard`), so an IPv4 phone the listener took
  on `[::]` finds a UDP socket behind its candidate whatever the system's `bindv6only` says. The
  plain group keeps the system's default, as its HTTP listener does (§3.7). No address reuse on
  either: on UDP it would let a second socket share the port.
- **`realtime.webrtc_enabled` as the start read it** holds for the device group too
  (`serve_plain` records it): off, the group is `Off` while the device listener runs and a device
  call is refused 503 naming the setting. A change applies at the next start, as the setting says.
- **A failed bind** (the port held by another socket) leaves the group `Failed` naming the
  address, the device listener serving, and a device call refused 503 with the bind error. The
  group binds again at the listener's next change or the next start, or when the device settings
  are saved again with no change: the save wakes the follower (`DeviceListener::reconcile`), whose
  close is a no-op, and the bind is tried again. The reason names the remedy (free the port, then
  save the device settings again or restart), and the save's answer says a failed group where it
  says the listener listens (*review S1*). The plain group has no retry.
- **A call POSTed on a moved listener before the follower runs** is refused as the gap: its
  connection's `served_at` is not the one the group is bound for (*review N3*, which closes the
  window this bullet used to describe).
- **The switch with the listener off** (*review N4*): a group that is neither bound nor failed
  reads `Off` or `Down` from the switch as the start read it, so a restart that flipped
  `realtime.webrtc_enabled` while the device listener was off or in error shows in the status.
- **No port collision** (*review N8*): a save that makes `realtime.webrtc_port` (null: the HTTP
  port) and `devices.port` one UDP port on overlapping addresses (`bind_addr`'s address against the
  device listener's) is refused naming both settings, from the realtime save and the devices save,
  while the listener and the switch are on and neither port is 0.
- **The status block's `webrtc`** (§9), the groups' half: `enabled` (the switch as the start read
  it), each group with its rule, its state (`off`, `down`, `bound`, `failed`), its `udp://`
  addresses or its error, and `dropped` (§3.3). The live calls, the dashboard's WebRTC group and
  the firewall hint stay WP6's. The setting texts of `webrtc_enabled` and `webrtc_port` say that the
  switch covers the device listener's sockets and that those take its port.
- **Tests.** `realtime_calls_net::devices`: the group on a loopback device listener's pair at its
  port, a call over each address on its own socket; a move, a widening to `[::]` on the same port
  and the off each ending the device calls with their last word out, the group moving, a stock call
  on the plain group never asked; a held UDP port failing the group and not the listener; the
  switch off at the start. `realtime_calls::devices`: a paired device calling through the device
  listener with its certificate (the candidate on the listener's address and port, the takeover in
  the device's name, another device's hangup 404 and the owner's 200), a device at its limit, and
  §13.1's live rebind end to end (`lmgw.session.closed {kind: "shutting_down"}` at the move and at
  the off, the stock call serving on, the next device call on the new port). The suites' device
  port is free for TCP and UDP on both loopbacks (`support::device_link::free_port`), and a port
  another suite took meanwhile is let go for another, as for the TCP side.

## 4. Audio in

### 4.1 Decode

- A call's reader (a task per call, the WebSocket reader's counterpart, §7) receives each of
  str0m's `MediaData` (an Opus payload, its RTP time, its arrival, whether it follows the previous
  one) and decodes it with a libopus decoder created at 24 kHz mono. libopus resamples internally,
  so there is no rubato stage and no added delay; a stereo stream is downmixed by the decoder.
- The RTP clock is 48 kHz whatever the decode rate: a 20 ms frame is 960 ticks and 480 decoded
  samples.
- The PCM enters `AudioIn::append` through the same core method the base64 path ends in (§7.1).
  VAD, Smart Turn, the normaliser, commits and the ASR call are untouched.

### 4.2 Reorder, FEC, concealment, gaps

- str0m holds frames behind a missing packet until it arrives or the oldest waiting frame is
  `realtime.webrtc_reorder_ms` old. Frames in order are released at once, so the window costs
  latency only behind a loss.
- **Default 60 ms**: three packets, enough for the reordering a cellular link shows, and short
  against the silence that ends a turn. **0** turns the wait off: a frame behind a gap goes on at
  once and the gap is concealed.
- When the reader meets a gap (the frame's RTP time is past the one expected), the last missing
  frame is decoded from the next packet's in-band FEC (`decode` with `fec` on), every earlier
  missing frame by libopus's concealment (`decode` of an empty packet), then the next packet as
  usual. With RED negotiated, str0m recovers a single loss from the redundancy before lmgw sees a
  gap at all.
- **Concealment never exceeds the wall-clock time that passed**: the arrival gap between the frame
  before the gap and the frame after it, plus the reorder window. Within that bound a gap becomes
  samples, so the timeline stays aligned with RTP time and every `audio_start_ms` stays sample
  arithmetic; libopus fades its concealment to silence by itself.
- **An RTP jump beyond that bound is a sender discontinuity** (a clock reset, a replaced track, a
  jump backwards): the timeline gets the wall-clock gap's worth of concealment and no more, the
  RTP-to-wall mapping (§4.3) re-anchors on the frame after the jump, and an INFO line names the
  jump and the gap.
- A packet that arrives after its frame was concealed is dropped by the reader (its RTP time is
  behind the timeline); a DEBUG line counts such packets.
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/decode.rs`):* where this section was
  silent, decided as follows. **The wall-clock gap** a discontinuity conceals is the arrival gap
  less the frame before's own length, the time in which nothing arrived, so a clock reset in a
  steady stream conceals nothing. **FEC only after a loss:** a gap with no packet missing (str0m's
  `contiguous`, a phone that skipped frames under DTX) is concealed whole, since no packet's FEC
  describes a frame that was never sent. **Late or backwards:** a frame behind the timeline whose
  sequence number is newer than the frame before's is the sender's RTP clock jumping backwards, a
  discontinuity; only an older one is a late packet. str0m already drops a packet behind the last
  frame it released whenever it holds frames back, and a call's hold-back is never 0 (§3.7's count
  is the window over 10 ms plus one, 1 at `webrtc_reorder_ms` 0), so the reader's drop is a second
  line rather than the first. **A payload libopus cannot read** is a lost frame: nothing comes out,
  and the next frame's gap covers it. A remainder of a gap shorter than libopus's 2.5 ms
  concealment step is zeros, so the timeline stays sample-exact. *(Changed 2026-10-15, the review
  of slice B, §5.5: the sequence number decides only within the wall-clock bound, and further
  behind is a discontinuity whatever it says; a gap's rest is zeros once the concealment has faded
  to zeros.)*

### 4.3 Placing input on the wall clock

- The barge-in gate judges input by when it was captured (realtime §6.4). A WebSocket reader stamps
  each append's arrival. On a call, arrival carries the network's jitter and the reorder window's
  wait, so the stamp comes from RTP time instead:
  - a frame's time is its RTP offset from the stream's anchor (its first frame, or the frame after
    a discontinuity), plus the **transit floor**: the least `arrival − RTP offset` over the last
    10 s;
  - a frame is so stamped "capture plus the least transit", which is what a WebSocket stamp means
    ("late by the client's transit"), without the jitter.
- The 10 s window lets the floor follow a changed path and the drift between two crystal clocks
  (tens of ppm: milliseconds a minute). It is an estimator's horizon, named in the code like
  realtime §6.4's 0.8 s evidence gap: it bounds, drops and delays nothing.
- `AudioIn::append` takes the stamp as its `arrived`; the timeline's floor rules (realtime §6.4)
  apply unchanged.
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/stamp.rs`):* the timeline reads an
  append's `arrived` as the capture of its **last** sample (`audio_in::timeline`), so the offsets
  are taken at each frame's end, and a frame is stamped at its last sample. A gap's concealment
  goes in one append with the frame after it, stamped at that frame's end; the timeline places
  the samples before it back from there.

### 4.4 Events and audio in order: the commit window

- On a WebSocket, appends and other events are one ordered stream. On a call they are two streams
  (RTP and SCTP), and a push-to-talk `input_audio_buffer.commit` can overtake the turn's last
  frames: the commit leaves the phone at once, while audio captured just before it is still in the
  phone's capture pipeline (its input buffer, the 10 ms processing blocks, the 20 ms Opus framing)
  or waits behind a reorder window.
- **A commit takes every frame stamped up to its own stamp plus the commit window**
  (`realtime.webrtc_commit_window_ms`, default 120). It waits that full window: it is applied once
  a frame stamped after its cut has been released, or once the window plus the reorder window has
  passed since it arrived, whichever is first. The window is its own setting, not tied to
  `webrtc_reorder_ms`: it covers the phone's capture-to-send offset, which no report tells lmgw
  (the transit floor folds it in with the network's least delay), plus the jitter between the two
  streams. The default doubles a measured Android offset of about 50–60 ms; the rig and the phone
  refine it (WP6). A push-to-talk turn's end costs that window.
- **A `clear` cuts at its own stamp** and waits only for the frames stamped before it (or the
  reorder window). The asymmetry is deliberate: a frame of silence before a press that lands in the
  new turn is harmless, while a window there would cut the start of what the user says after it.
- **Every other event goes to the core at once**: a `response.cancel` or an
  `output_audio_buffer.clear` waits behind no audio. Events keep their order among themselves (an
  event behind a waiting commit waits with it).
- A frame later than the cut lands in the next turn (for push-to-talk, the next press's `clear`
  drops it); a DEBUG line says how late it was, so the window can be tuned from the log.
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/commit.rs`):* "a frame later than the
  cut" is one stamped before an applied cut but released after it; the DEBUG line names how far
  before the cut it was stamped and how long after the cut was applied it came. The order is
  `commit::Order`, generic over the event: the session loop classifies each event (commit, clear,
  anything else), hands it over with its stamp, and its timer waits for `Order::deadline`; the End
  takes whatever waits with `Order::flush`. *(Changed 2026-10-15, WP3 slice B: the call's reader
  holds the order, not the session loop, §5.5.)*
- On its side, the Android client does not hold its commit back: it commits at the release, and its
  microphone tail stays enabled after the release and must fit inside `webrtc_commit_window_ms`
  (the Android design). *Changed 2026-10-14 (consistency pass):* this replaces "keeps its microphone track enabled for a
  measured, visible tail after the release before it commits".
- **The exact cut, decided after WP0 and the rig.** The window covers an offset that no report
  states. The phone's own RTCP sender reports do state the mapping it needs: each one pairs the
  phone's NTP time with its RTP time, so a capture time on the phone names an RTP time exactly. The
  alternative, recorded for the decision after the Android design's WP0 and the interop rig:
  - a bound client cuts push-to-talk with `lmgw.input_audio_buffer.commit {captured_until_ntp_ms}`
    and `lmgw.input_audio_buffer.clear {captured_from_ntp_ms}`, times in the phone's NTP clock (the
    clock its sender reports carry);
  - lmgw maps the time through the phone's latest sender report to an RTP time and cuts exactly
    there, waiting only for the frames up to it (or the reorder window). No window to tune, and no
    frame on the wrong side of the cut;
  - OpenAI's `input_audio_buffer.commit` and `.clear` keep the window, for stock clients;
  - it needs str0m to hand over the NTP and RTP times of the sender reports it receives. No copy of
    str0m's source was at hand for this revision, so that is to check in WP2 (§16).

  Until then `webrtc_commit_window_ms` stays, and the Android client uses OpenAI's commit and clear
  (Later).

### 4.5 Flow control

RTP cannot be slowed: the phone sends in real time. Decoded audio waits for the core in memory for
as long as the core is busy, and nothing is dropped. What bounds it is the liveness verdict
(§8.1): a core stuck behind output that cannot leave ends the call within one `ping_interval_s`
(48 KB of PCM a second, under 1 MB at the default 20 s).

### 4.6 From the first frame (C13)

- The call's reader runs from the first RTP frame after DTLS-SRTP connects, before `oai-events`
  opens and before the bind. It decodes, orders and stamps each frame as §4.1–§4.3 have it, and
  keeps the PCM with its stamps until the session starts.
- **When the session starts** (§2.8), after `session.created` and the `session` part's update, the
  kept audio enters `AudioIn::append` in order, at its own stamps, and the live frames follow it.
  The timeline begins at the first frame, so a word the user began at DTLS-connected is in the
  first turn whole, and VAD, Smart Turn and the commit window treat it as any audio.
- The guarantee holds when the bind succeeds at the open directly. **Under `bind_pending`** the kept
  audio is discarded, and so is every frame until the session starts (§2.8): what the user says
  while deciding whether to take the thread over is not the thread's. The timeline then begins at
  the first frame after the session's start.
- **The bound** is §4.5's: the kept audio lives only until the open, which the never-opened interval
  bounds (§8.1), so it is at most one `ping_interval_s` of PCM, under 1 MB at the default.
  *Changed 2026-10-15 (the review of WP2 part B):* the interval bounds the time, not the bytes: a
  client may send faster than real time, and with `ping_interval_s` at 0 there is no interval. So
  the kept audio is also counted against the call's budget of what waits for its owner
  (`realtime.max_message_mb`, §3.7), and past it the call ends (`Ended::TooBig`, §3.8).
- So the Android client shows its listening cue at DTLS-connected, not at the channel's open or at
  `session.created`.
- Audio captured before the peer connection exists (the second after a wake gesture, carried as
  `lmgw.input_audio.preroll`) is Later: decided after the Android design's WP0 measurement, on its
  merits. The RTP from the first frame is kept already, so the question is only what precedes the
  peer connection.

## 5. Audio out

### 5.1 The frame clock

- A call's output is one RTP stream for the call's whole life. A frame clock in the call's link
  ticks every 20 ms. Due times are `start + k × 20 ms` and RTP times `k × 960`, so neither drifts
  with timer jitter; a late tick sends late with the right timestamp, which NetEQ absorbs.
- Each tick takes the next 20 ms (480 samples) of released audio from the writer's paced queue,
  across chunk and clause boundaries, or encodes silence when none is ready.
- **The paced queue is pulled, not timed**, on a call. The writer's `Pacer` is not used, the lead
  is 0 (`session.lmgw.output_lead_ms` echoes 0), and the queue's purgeable events (the transcript
  deltas paced with the audio) are released by the tick that reaches them, onto the data channel.
  The purges, the playback record, `progress()` (which bounds synthesis by `synthesis_ahead_s`) and
  the drained marker keep their meaning.
- *Why one clock:* a writer releasing on its own timer and a link sending on another would beat
  against each other, and put silence into the middle of a word whenever their phases slipped.
- *Why lead 0:* the lead stood in for a client buffer, and on a call NetEQ is that buffer. Frames
  sent ahead of real time arrive as a burst, which NetEQ answers by growing its delay or by
  speeding playback up.
- **An underrun.** A tick that finds a response open but no audio ready (synthesis is late, or a
  tool runs after the spoken preamble) pads with silence. Padding is never counted as the item's
  audio, so the heard table and `audio_end_ms` count only the TTS's samples; the playback record
  counts it, since the phone plays it. On a bound call the first padded tick sends
  `lmgw.output_audio.starved {response_id, starved: true}` and the next tick with the response's
  audio sends `starved: false`, both on the data channel in order with the transcript deltas (C22).
  The event leads the phone's own silence by about the playout allowance (§5.3).
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/clock.rs`, `frames.rs`):* each frame
  is written with its slot's due time as str0m's wallclock, which keeps lmgw's sender reports on
  one line (§5.3). The frame clock takes the paced queue's chunks whole and cuts them, so what a
  cut response sent is counted exactly by the frame (`Frames::purge`), where the paced queue counts
  a chunk as sent when it is taken, up to 80 ms more. A frame carries one response's audio. A frame
  whose audio runs out midway is padded for the rest and says nothing; `starved: true` goes with
  the first tick that finds no audio at all, and a response that ends or is cut while starved owes
  no `false` (its `response.done` ends it). What "a response open" means is the seam's
  (`frames::Pull::speaking`): a response that paces and whose last chunk has not been taken, so
  the wait for drained after the last frame is silence, not padding.

### 5.2 Encoding

- A libopus encoder for VoIP, 24 kHz mono input, 20 ms frames.
- Bitrate `realtime.webrtc_output_kbps` (default 32, libwebrtc's own for mono speech), capped by the
  phone's `maxaveragebitrate` when its offer names one; the cap shows in the call's status line.
- In-band FEC on. The expected loss handed to the encoder follows the loss fraction the phone
  reports in its receiver reports (str0m's `MediaEgressStats.loss`), so FEC spends nothing on a
  clean link. That fraction is why C20 pins a str0m revision with #1065.
- **DTX only when the phone asks for it.** RFC 7587's `usedtx` is the receiver's preference ("the
  decoder prefers the use of DTX"), so lmgw turns DTX on only when the phone's offer carries
  `usedtx=1` (str0m's Opus matching reads the parameter). Without it, silence goes out as ordinary
  frames, a few bytes each under VBR, every 20 ms. With it, a frame libopus encodes as DTX (two
  bytes or less) is not sent, as libwebrtc does it, the RTP time still advances, the first frame
  after a pause carries the marker bit, and a comfort-noise update goes every 400 ms. Either way the
  stream stays continuous: NetEQ keeps its clock and its delay estimate across the pauses between
  answers, an answer does not trail off into concealment, and sender and receiver reports keep
  flowing (the round trip, §5.3).
- RED when negotiated, one level (the previous frame): it doubles a 32 kbps stream, which a
  WireGuard link carries easily, and recovers a single loss at full quality.
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/encode.rs`):* **RED is str0m's**, read
  at `634e76bb`: `enable_opus(true, red)` folds a RED payload type onto Opus, and its send side
  (`RedSender`) wraps every written frame with the previous one (`set_red_distances`, one level by
  default), so the frame clock writes plain Opus either way. The comfort-noise update every 400 ms
  is libopus's own (it ends a run of DTX frames after 20). **Measured with libopus 1.6:** a 20 ms
  frame of digital silence is 32 bytes at 32 kbps without DTX (about 13 kbps), not "a few
  bytes"; the cost of a stream without `usedtx` between answers.

### 5.3 The playing window, drained, the barge-in margin

- Each frame is recorded as it leaves (`Playback::released`, as a WebSocket's chunks are), so a
  response's window opens with its first frame. The record keeps each frame's RTP time, its send
  time, and which item's audio it carried up to which offset (padding marked as such): heard (§5.4)
  and the playout reports below read it.
- The phone plays a frame later by the one-way transit and NetEQ's delay. lmgw sees the round trip,
  from the phone's receiver reports (str0m's `MediaEgressStats.rtt`, LSR/DLSR), and the phone's
  interarrival jitter J (`MediaEgressStats.remote.jitter`, in RTP units). No RTCP report carries
  NetEQ's delay; a bound client reports its playout instead (below).
- **The playout allowance** is NetEQ's part of the delay. Each term is counted once, whichever
  source the allowance comes from:
  - **the window's end** is the last frame's send end plus the allowance. That is the playback
    record's end on a call, what the barge-in gate judges against;
  - **drained**, which `output_audio.done` and `response.done` wait for, is the window's end plus
    half the round trip: the phone has played the audio ("the client has played it", realtime's
    Q1, for a call);
  - **the barge-in margin** is the round trip plus `echo_tail_ms`, as on a WebSocket, with the round
    trip from receiver reports (`RoundTrip`, least of the last five) instead of pongs. The round
    trip covers the transit both ways; the tail covers what no report sees, the phone's audio
    device buffers and the room.
- **Playout reports** (C10). While a response's audio plays, a bound client sends
  `lmgw.output_audio.playout {playout_ntp_ms?, jitter_buffer_ms?}` at its own cadence (the Android
  client's visible setting, about 1 s). *Changed 2026-10-14 (consistency pass):* both fields are optional and a report with neither
  is not sent; a report without `playout_ntp_ms` does not update the measured playout position, and
  one without `jitter_buffer_ms` keeps the last value:
  - `playout_ntp_ms` is libwebrtc's `estimatedPlayoutTimestamp` for the inbound audio: the NTP time
    of the sample playing now, in lmgw's own clock, because libwebrtc maps it through lmgw's RTCP
    sender reports. lmgw maps it back through the same reports to an RTP time, and so to a frame of
    the record, whose send time it knows;
  - the report left the phone about half a round trip before it arrived, so the named frame played
    then. The **playout delay** d, from a frame's send to its playout, is measured rather than
    modelled: d = (the report's arrival − half the round trip) − the frame's send time. The
    allowance is d less half the round trip, and drained is the last frame's send end plus d;
  - `jitter_buffer_ms` is NetEQ's own delay as `getStats` gives it (the change in
    `jitterBufferDelay` over the change in `jitterBufferEmittedCount` since the client's previous
    report). It is the allowance seen from the phone. The DEBUG line and the status show it beside
    the allowance from d, and a gap between the two is the transit's asymmetry, which "half the
    round trip" assumes away;
  - the latest report holds until the next. One whose position maps to no frame sent (before the
    stream, past its last frame) is ignored, with a DEBUG line naming it.
- **The formula, without reports.** On an unbound call, and on a bound one before its first report,
  the allowance is one frame plus 2 × J. NetEQ aims its delay at the high percentiles of the arrival
  jitter, and twice J (RFC 3550's smoothed mean deviation) is the first estimate of that. Before the
  first receiver report the round trip is unknown and the allowance is one frame.
- **When reports stop** (none arrived for one `ping_interval_s` while audio played, the setting that
  already says how long the phone may be silent), the call falls back to the formula and an INFO
  line says so, naming the last report's age. A report that comes again takes over again. With
  `ping_interval_s` at 0 the last report holds.
- WP6 validates the reports against the formula in the rig (§13.2): both allowances, and the
  `jitterBufferDelay` the page reads, side by side.
- A DEBUG line names the allowance, its source (report or formula), the round trip and their inputs
  when they change ("not measured yet" before the first report), beside realtime's margin line.
- The heard position's error of up to `output_lead_ms` (realtime §7.3) does not arise on a call:
  the server holds the buffer.
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/record.rs`, `line.rs`, `playout.rs`,
  `rtcp.rs`):* where this section was silent, decided as follows.
  - **The record keeps the latest response's frames** (audio and padding), from its first on: a
    response's first frame drops the last one's. Its own audio bounds it, as it bounds the paced
    queue. A frame between answers is not kept; its send time is its slot's due time, which the
    frame clock keeps exactly, so a position there still maps.
  - **The line.** lmgw's own pair is read from one sender report it sent (§16, WP2 part A):
    `line::SentLine`, from `RawPacket::RtcpTx` with `enable_raw_packets` on; its 32-bit RTP time
    is matched with the frame clock's modulo 2^32.
  - **d is measured to the named sample**: its frame's send time plus its offset in the frame. A
    report whose position lies after the report left the phone is ignored like one that maps to no
    frame sent, with its DEBUG line.
  - **"While audio played"** counts a response's frames (audio or padding) sent since the last
    report that measured a position; a report with only `jitter_buffer_ms` keeps the last d and
    does not reset the count.
  - **The round trip** is str0m's `MediaEgressStats.rtt`, taken once per new receiver report into
    `RoundTrip::reported` (least of five, the interval bound): str0m repeats the last report's
    values in every stats event until the next arrives, so a report is new when its highest
    sequence number moved, and lmgw's stream never stops. `rtcp::round_trip` does the same RFC
    3550 arithmetic over a raw report block, for a reader that takes the reports from raw packets.

### 5.4 Heard (C11)

- On a cancel, a barge-in, a stop or a `clear` the server stops sending at once. What NetEQ holds
  (about the allowance's worth) still plays on a client that does not mute: libwebrtc offers no
  flush of a remote track's buffer.
- **A bound client's stop with `playout_ntp_ms`** (§6.2) says what the phone played: libwebrtc's
  playout position at the tap, when the phone muted the remote track. lmgw maps it through its own
  sender reports to an RTP time, finds the frame in the record (§5.3), and cuts the heard table at
  that frame's item and offset. Padding counts as nothing, and an item whose first frame lies after
  the position was not heard at all. A position past the last frame sent is clamped to it with a
  DEBUG line: heard never exceeds sent. The precision is the frame, 20 ms, plus the estimate's own.
- **Without it** (a stop sent before the phone had a position, a stock client's `response.cancel`
  or `clear`), heard is what was sent, which `WriterHandle::purge` already reports. It is the
  WebSocket's rule for a cancel that is never truncated, exact here rather than "plus at most the
  lead".
- **A barge-in on a bound call.** The phone mutes when `output_audio_buffer.cleared` reaches it
  (§6.2), half a round trip after the cut, so what it played by then is what lmgw sent up to the cut
  less the allowance's worth (§5.3). That is the heard position. On an unbound call it is what was
  sent.
- No unsolicited `conversation.item.truncated`, as for a WebSocket's server cut (whether OpenAI
  sends one is unverified, §1.4).
- `conversation.item.truncate` works as on a WebSocket, for any client that knows better still.
- A bound session's stored reply (`lmgw.chat.reply`) follows the same cut.
- *Changed 2026-10-15 (WP3 slice A, `realtime/calls/audio/heard.rs`):* heard at a position cuts
  at the sample playing there, inside its frame, rather than at the frame's start. A barge-in's
  position is counted in audio time: the end of the response's last frame less the allowance in
  RTP ticks. A position before the stream itself (not only before the response) is no position,
  and heard is then what was sent. Each answer has `WriterHandle::purge`'s shape, samples per item,
  every item of the response named.

### 5.5 As built: WP3 slice B, the wiring (2026-10-15)

The reader in `realtime/calls/link.rs` with `link/input.rs`; the frame clock in
`realtime/calls/audio/out.rs`; the pulled paced queue in `realtime/writer/paced/pulled.rs`; the
driver's audio write and feedback in `driver/call.rs`; tests in `realtime_calls_audio` (and the unit
tests beside each). Details §4 and §5 left open, taken as follows (*changed 2026-10-15*):

- **The commit window runs in the call's reader, not the session loop** (§4.4's dated note said
  the loop). The reader meets frames and events in the one order the driver met them, so its own
  lag (a busy session, the inbox's read-ahead) never reorders them; when a cut's deadline passes,
  what already came off the socket is taken first, and only then is the wait over; the End flushes
  what waits ahead of itself. The session loop is untouched (WP5 works in it), and a commit reaches
  the core as any event, at its own stamp.
- **A frame's place in the order is judged on the socket's clock**: a frame counts as released at
  its arrival, an event at its message's, the deadline at the time the reader looks. So a reader
  that fell behind does not apply a commit ahead of frames that arrived inside its window.
- **The stamps are §4.3's transit floor**, as slice A built them. The client's sender reports are
  passed by: they would name the exact push-to-talk cut, which stays Later (§4.4).
- **Flow (§4.5)**: decoded PCM counts against the inbox's read-ahead (256 KiB, about five seconds);
  past it the reader waits, and the frames wait undecoded with the driver, counted against the
  call's budget (`realtime.max_message_mb`, §3.7). Nothing is dropped for the session's pace.
- **The reorder window the reader uses is the one the call's `Rtc` was made with** (the offer's
  `CallParams`), not the settings at the open, so the decoder's wall-clock bound matches str0m's
  hold-back.
- **The frame clock is a task of the call's link**, started when the writer hands the link its
  paced queue (`link::Sink::pull`, `writer::Pulled`) and stopped with the link. It is not the
  writer's loop: an event the writer sends waits for room in str0m's send buffer (§6.3), and audio
  never waits behind that. The transcript deltas and `starved` the clock releases are queued with
  the driver without a wait, each `event_id` minted as it leaves; the driver keeps one queue of
  writes per call, so they keep their place among the writer's events. The first frame is due at
  the session's start, at a random 32-bit RTP time (RFC 3550 §5.1). **The session's end stops the
  clock first**: a tick hands its events to the driver under the paced queue's lock, and the
  writer marks the queue stopped (`Shared::stop_pulled`, no tick after it, what it would have
  released dropped) before it sends the lane's last events, so nothing the clock sends follows a
  bound call's `lmgw.session.closed` (§8.2). What was sent stays counted for the heard the
  session's end settles. *(Corrected 2026-10-15, the review: the first build stopped it only at
  the writer's close, while the session queues `lmgw.session.closed` and flushes it before the
  close, so a tick between could send a delta or `starved` behind it. The writer now stops the
  clock where the event is produced, before it sends `lmgw.session.closed` — which covers
  `say_closed` and a verdict's `say_closed_now` alike — and on every way its own sending ends: the
  close, every handle gone, the client gone.)*
- **The paced queue on a call is pulled** (§5.1). No pacer times the audio: a chunk, and the deltas
  queued ahead of it, leave with the tick that takes it; deltas ahead of the drained marker (a
  clause with no audio) the writer releases at once. The record, the playout and the playing window
  are written by the tick under the queue's lock: the window opens with the response's first
  frame, `Playback::end` is the last frame's send end plus the allowance (what the barge-in gate
  judges), and the drained marker is due at `Playback::drained`, half a round trip later, or at the
  send end plus the measured delay while reports hold. Each tick sets both again, since the
  allowance moves with every report. The marker is never due while the clock still cuts the
  response's last chunk. `progress()` counts a chunk as released when the clock takes it, so
  synthesis stays within `synthesis_ahead_s` of what the phone was sent.
- **A purge on a call answers heard as its cut counts it** (`writer::Cut`, §5.4): `Sent` (what was
  sent, exact to the frame) for a cancel, a `clear`, a stop without a position and an unbound
  call's barge-in; `BargeIn` on a bound call (what left by the allowance before the cut, in whole
  samples; *corrected 2026-10-15*, below); `At(playout_ntp_ms)` for WP4's stop. A DEBUG line names
  each cut, heard against sent, per item.
- **lmgw's own sender reports.** str0m sends its first one as soon as the stream exists, before any
  frame was written: it names RTP time 0, no time of the stream. A report whose packet count is 0
  is therefore no line. str0m's audio RTCP interval stays its default, 5 s: the first line lands
  within 5 s of the first frame, and the phone's positions map through the same reports anyway.
- **The receiver reports** reach the clock through str0m's stats event (1 s): a new one (its highest
  sequence number moved) feeds `RoundTrip::reported`, the phone's jitter into the formula and the
  loss into the encoder. The registry's stats are as part A had them.
- **`lmgw.output_audio.playout`** is read by the session's core: on a bound call into the playout
  delay (its notes logged, the fallback and its end at INFO), anywhere else answered `error`
  `invalid_event` ("… applies to a call bound to a Chat thread"), as §6.4 has it. `starved` is sent
  on a bound call only.
- **The output's status line** is an INFO line when the clock starts, naming the bitrate (and the
  phone's `maxaveragebitrate` when it capped it) and whether DTX is on. A decoder libopus cannot
  make is an ERROR and the call's input is not heard; an encoder it cannot make is an ERROR, and the
  queue is still pulled, so every answer ends as if played.
- **Not here**: `session.lmgw.output_lead_ms` still echoes the setting (§6.1's echo of 0 is WP4's);
  on a call the lead times nothing. The stop that cuts at a position (`lmgw.response.stop`) is WP4's:
  it calls the purge with `Cut::At`. *(Both built 2026-10-15, §6.5.)*
- **Slice A, as wiring met it**: a barge-in's heard is taken off in whole samples (an odd tick count
  left heard one sample short of sent less the allowance); `FrameClock::slot_of`,
  `playout::margin` (the session's `echo_margin` is the margin) and `rtcp::round_trip` (str0m's
  stats carry the round trip) had no caller and are gone; `Frames` can say what it holds and drop
  it; the inspection helpers only the tests read are test-only.

*Changed 2026-10-15 (the review of slice B; the late tick's skip is the owner's decision):*

- **The reader's drain at a cut's deadline is bounded.** It takes what came off the socket before
  the cut's give-up time, and stops after the first event from at or after it (a frame's arrival, a
  message's), which ends the wait itself; what is behind it goes through the loop that waits for
  the read-ahead's room, as the kept audio now does per frame. The first build drained the whole
  backlog in one poll. A reader-level test pins that a reader lagging past the give-up time passes
  the frames that arrived inside the window before the commit.
- **Heard on a bound call's barge-in takes the cut's time into account.** The phone mutes half a
  round trip after the cut and plays a sample half a round trip plus the allowance after it left,
  so heard is up to the sample that left the allowance before the cut, from the record's send times
  (`Record::rtp_at`: each frame played from when it left, or from the end of the one before it if
  that is later, as `Playback` models a WebSocket's chunks), clamped to what was sent. The first
  build took the allowance off the end of what was sent whenever the cut came: a barge-in confirmed
  after the last frame left heard less than its tail played, one between two ticks more than had
  left.
- **A late tick skips what the phone would discard** (the owner's decision: such frames must not
  count as played). Of the slots a tick takes, those due longer ago than the current playout
  allowance (every one but the last) are never sent (`Pulled::skip_stale`): they would reach NetEQ
  after their playout time. Nothing of the answer is lost, since what was due in them leaves in
  the frames sent (the first with the marker bit, as after a DTX pause), and a DEBUG line counts
  them with the tick's lateness and the allowance. The record marks the skipped slots in the latest
  response's run, so a playout position there maps to no send time (its report ignored with its
  note) and a stop's heard counts what came before it; heard, the window and the playout's count
  read only what was sent. §5.1's "a late tick sends late with the right timestamp" holds for the
  slots within the allowance.
- **The pulled queue.** A response whose drained marker is queued anywhere is no longer speaking,
  so a delta ahead of the marker brings no spurious `starved: true`; a response's window is frozen
  once the writer released its marker, so an allowance that grows afterwards does not reopen it for
  the barge-in gate; a tick wakes the writer only when it made something due earlier (a delta ahead
  of the marker reaching the head, the marker no longer held by the clock's last chunk, drained
  moving earlier), once per marker, not every tick while one waits.
- **The driver takes nothing of the clock's from a call that let go or is closing** (§8.5's
  backstop covered only the session's writes): the clock's events arrive as their own command and
  are dropped there, as its audio frames are. What of them was not sent is said at WARN, as the
  session's sink says its own: a message over the largest a call sends, and once per call the
  channel gone while the client had not closed it.
- **What the phone says is bounded** as the round trip it stands in for is (`RoundTrip`'s bound): a
  measured playout delay, or a jitter whose formula allowance, of `ping_interval_s` or more is
  ignored with a DEBUG note and the last value kept; none at 0. The allowance's DEBUG line names
  what a report's delay was measured from (how long before the report arrived its sample left, and
  the round trip then).
- **The offer's `maxaveragebitrate`** caps the bitrate only within RFC 7587's 6000 to 510000; a
  value outside it (0 made no encoder at all, and the call silent) is ignored, the setting's
  bitrate kept, and named in the status line.
- **The decoder** (§4.2): a frame further behind the timeline than the wall-clock bound is a
  discontinuity whatever its sequence number, so a stream that restarts with both behind is heard
  rather than dropped for good (str0m already drops really late packets); once libopus's
  concealment has faded to a step of zeros the rest of a gap is zeros, still sample-exact.
- **The tests lean on no scheduling**: `realtime_calls_audio` bounds a frame's arrival by its slot
  (no frame leaves before it is due) and the measured delay by how long the phone held the frame,
  where fixed wall-clock margins stood.

## 6. Events on the data channel

### 6.1 What differs from a WebSocket session

- Events travel on the `oai-events` channel, one JSON text message each: the same types, the same
  `event_id`s and the same order as on a WebSocket, except:
  - no `response.output_audio.delta` (RTP carries the audio). `response.output_audio.done`, the
    transcript deltas (paced with the audio, §5.1) and every other event are sent;
  - `input_audio_buffer.append` is answered with `error` `invalid_event` ("this call's audio
    arrives on its RTP track"), and the session goes on;
  - `audio.input.format` and `audio.output.format` keep describing the session's PCM16 24 kHz
    timeline; the SDP decides the codec. A `session.update` naming G.711 is refused as today;
  - `session.lmgw.output_lead_ms` echoes 0 (§5.1);
  - a bound call also carries lmgw's own events (§6.4).
- The session starts when the channel opens (§2.8), so its first event, `session.created` (or
  `lmgw.session.bind_pending` under `takeover=ask`), never waits for the channel.
- A channel with another label is ignored (DEBUG), and so is a second `oai-events`.
- The `oai-events` channel's close ends the call, as a WebSocket's close ends its session, once
  every event that arrived before it has been applied (the End, §8.1).

### 6.2 `output_audio_buffer.*`, the stop, the barge-in verdict (C12)

- `started {response_id}`: when the response's first frame is sent, after its
  `response.content_part.added`, as OpenAI's.
- `stopped {response_id}`: right after the `response.done` of a response whose audio played to its
  end. The buffer has drained, and §5.3 makes that "the phone has played it".
- `cleared {response_id}`: instead of `stopped`, right after the `response.done` of a response
  whose audio was cut before its end: by server VAD (after `input_audio_buffer.speech_started`,
  with the barge-in's cancel), by a `response.cancel`, by a client `clear`, or by
  `lmgw.response.stop`. Each response that spoke ends with exactly one of the two.
- **The barge-in verdict is `cleared`.** The Android client mutes the remote track when it arrives
  and unmutes at the next `started`. Why `cleared`, and neither `speech_started` nor a new event:
  - `input_audio_buffer.speech_started` does not mean a cut. Inside the playing window lmgw already
    withholds it until the gate and its word check confirm the speech (realtime §6.4), so it is no
    raw onset. But it also goes out where nothing is cut: with `interrupt_response` off (the answer
    keeps coming), for a response that had played out when the deciding frame was captured
    ("finishing, not interrupted"), and outside the window. A phone that muted on it would silence
    an answer lmgw keeps sending, and wait for a `started` that never comes for it;
  - `cleared` goes out exactly when lmgw has stopped sending a response's audio, whatever stopped
    it: a confirmed barge-in, a stop, a cancel. It follows `speech_started`, the cut and the
    cancelled `response.done` within one step of the core (the cancel is synchronous,
    `lifecycle/cancel.rs`), so it trails the verdict by nothing that matters. It is OpenAI's own
    event for this ("on a VAD interruption"), so a stock client that mutes on it gets the same;
  - the cause, when a client wants it, is the cancelled `response.done`'s `status_details.reason`
    just before it (`turn_detected` for a barge-in, `client_cancelled` for a stop). So no field is
    added to an OpenAI event, and no lmgw event repeats one;
  - unmuting at `started` is safe: `started` leaves with the next response's first frame, and NetEQ
    holds that frame for its delay, so the event arrives before the frame plays.
- **The stop on a bound call: `lmgw.response.stop {playout_ntp_ms?}`.** One event, idempotent, for
  every user stop of the Android client (the stop button, and the first half of the End, §8.1):
  - it cancels whatever is open: the active response in any phase a cancel changes (thinking,
    generating, speaking, waiting on a server-side tool or an approval), the response owed to
    committed turns, and a client's held `response.create` (the Pending phase, realtime §4.3). The
    last two are dropped, so nothing starts speaking after the tap; the turns stay in the
    conversation, unanswered. The unsent audio is purged;
  - its events are a cancel's: the cancelled `response.done` (`client_cancelled`), then `cleared`
    for a response that spoke. With nothing open it sends nothing (a DEBUG line), and never
    `response_cancel_not_active`: a stop that crossed the response's end is not an error. A
    response past its generation with every item closed (a text response whose tool call the client
    may have run) finishes as generated, by `response.cancel`'s own rule;
  - `playout_ntp_ms`, when present, sets heard (§5.4); without it heard is what was sent;
  - the phone mutes the remote track at the tap, so the silence is instant and waits for no round
    trip, and unmutes at the next `started`;
  - on an unbound call it is answered `error` `invalid_event` (§6.4).
- **The stop for a stock client: OpenAI's form**, `response.cancel` when a response is open, then
  `output_audio_buffer.clear`, the Agents SDK's `interrupt()` order. Its events: `response.done`
  (cancelled), then the one `cleared`; the `clear` after the cancel is silent. When the response
  ended while the cancel was on its way, the cancel is answered `error`
  `response_cancel_not_active`, as with OpenAI.
- The client's `clear`:
  - while a response's audio plays (its response is still open, since `response.done` waits for
    drained), it cancels the response as `response.cancel` does (status `cancelled`, reason
    `client_cancelled`): the unsent audio is purged, and `cleared` follows its `response.done`;
  - with no audio playing (right after the `response.cancel` that already cut it, or with nothing
    spoken at all), nothing is sent, and a DEBUG line notes it. (OpenAI's own answer here is
    unverified, §1.4.)
- *A reading of OpenAI's text:* it separates the two events, `response.cancel` stopping the
  generation and `clear` cutting the buffered audio, which suggests that a cancel alone lets audio
  already generated play out. lmgw's cancel cuts the audio on both transports, as realtime §4.3
  has it for the WebSocket. A client that follows OpenAI's guidance (cancel, then clear) sees the
  same events from both servers.
- On a WebSocket session `clear` keeps its `error` `not_implemented_yet`, worded for what it is:
  "output_audio_buffer.clear applies to WebRTC calls (POST /v1/realtime/calls); on a WebSocket the
  client owns its playback buffer: flush it and send conversation.item.truncate". No
  `output_audio_buffer.*` event is sent on a WebSocket, and the bound call's own events are not
  served there either (§6.4).

### 6.3 Size bounds

- **Inbound.** str0m advertises `a=max-message-size:262144` (its `LOCAL_MAX_MESSAGE_SIZE`), and a
  conforming sender refuses a larger message itself (a browser's `send()` throws), so the error
  surfaces in the client. `realtime.max_message_mb` and `max_frame_mb` do not bound a call's single
  message: their smallest bound, 1 MiB, is above it. `max_message_mb` bounds what a bound client's
  parts join to (below). The dashboard shows the 256 KiB beside the WebSocket limits, named as the
  WebRTC stack's. A non-conforming sender's oversized message fails the SCTP association, and the
  call ends with a log line naming the bound, where a WebSocket closes 1009 naming its setting.
- **Outbound.** The phone's advertised `max-message-size` (libwebrtc advertises 256 KiB; str0m
  assumes 64 KiB for a peer that advertises none) is the real limit, and nothing below it is
  imposed. An event larger than it cannot be one message:
  - **on a bound call it is split into parts** (Q4, answered): consecutive
    `lmgw.event.part {id, index, count, data}` messages. `data` is a slice of the event's JSON text,
    cut at character boundaries so that each part's own JSON, with `data` escaped, fits the limit;
    `id` is the event's `event_id`, `index` counts from 0, and `count` is at least 2. The parts go
    out back to back, with nothing between them, so the event keeps its place in the stream.
    `lmgw-client` joins them and parses the whole (§8.3). Nothing is emptied and nothing is lost;
  - **on an unbound call** (a stock client cannot join parts) the event is still sent, with its
    largest fields emptied one by one, largest first, until it fits: a string becomes a short
    marker ("[omitted: 312 KiB over this call's 256 KiB event limit]"), an array `[]`, an object
    `{}`, so the event still parses as its type. Its `type`, `event_id` and every id are never
    emptied. For each emptied field one `error` follows, code `event_too_large`, `param` the
    field's JSON path, the message naming the event type, the field's size and the limit; it is
    logged too. The session goes on.
- **Parts from a bound client.** The split is symmetric: a bound client may send an event larger
  than lmgw's 256 KiB as parts of the same shape, with an `id` of its own. lmgw joins them before
  parsing:
  - a set's parts arrive back to back, in index order, all with one `count`. A set broken by
    another message, a wrong index or count, or a part on an unbound call is answered `error`
    `invalid_event` naming the `id`, and the set is dropped;
  - the joined event is bounded by `realtime.max_message_mb`, the setting that bounds a WebSocket
    message. A set whose joined text exceeds it is answered `event_too_large` naming the setting,
    and dropped;
  - either way the session goes on. A whole set is handled as the one event it joins to, which is
    never itself a part.
- **What can be that large**, which WP4 measures for a bound session with the self-admin toolset:
  `session.created` and `session.updated` with long instructions and many tools, a session's
  `mcp_list_tools` items, tool outputs (`function_call_output`, `mcp_call` items and their
  `conversation.item.*` events), and `lmgw.chat.frame`s that carry images. *(Measured 2026-10-15,
  §6.5: of a bound self-admin session's events only the chat turn's tool results pass 256 KiB — a
  text result from about 117 KiB of text, which it carries twice, an image result from a 192 KiB
  image; `session.created` is 1.7 KB, with no tools in it.)*
- **The send buffer.** str0m never accepts a message larger than its SCTP buffer (128 KiB by
  default), which would refuse every event or part between 128 and 256 KiB. The buffer is set to
  the phone's limit (§3.5), so every part fits.
- **Back-pressure.** A write str0m does not accept for lack of room waits for
  `ChannelBufferedAmountLow`. The writer's data lane (`WINDOW`) then fills and the core waits, as
  behind a full TCP socket. Audio never waits behind it: it is another stream on its own clock
  (§5.1).

*Changed 2026-10-15 (the review of WP2 part A, decided):*
- **The client does not size lmgw's send buffer.** The largest message a call sends is the smaller
  of the client's `max-message-size` (0: sctp-proto's own largest) and `realtime.max_message_mb`
  (0: no bound); a larger one is `TooLarge`, naming whose bound, and WP4 splits it on a bound call.
  The buffer is str0m's default (read from str0m) plus that largest message, and a write goes out
  only while no more than the default is in flight, so the back-pressure is the same for every
  client (§3.7).
- **What waits for the session has a budget.** Inbound there is no back-pressure (str0m reads SCTP
  out as it arrives), so the bytes of the messages and audio queued for a call's session count
  against `realtime.max_message_mb` (0: no bound); past it the call ends (`Ended::TooBig`), and the
  session closes with `Close::TooBig` naming the setting, as a WebSocket message over it does
  (§3.7).

### 6.4 The bound call's own events (C24)

Everything lmgw adds to a call for its own clients, in one place. Each travels as a JSON text
message on `oai-events` with its `type`, and lmgw's carry an `event_id`, as all its events do.

| Event | From | Shape | When | § |
|---|---|---|---|---|
| `lmgw.session.bind_pending` | lmgw | `{held_by}` | `takeover=ask`, the thread held when `oai-events` opens; again when the holder changes | §2.8 |
| `lmgw.session.take_over` | client | `{}` | while pending: take the thread | §2.8 |
| `lmgw.session.closed` | lmgw | `{kind, by, message}` | the last event before lmgw ends a bound call | §8.2 |
| `lmgw.response.stop` | client | `{playout_ntp_ms?}` | the user's stop, and the End's first half | §6.2, §8.1 |
| `lmgw.output_audio.playout` | client | `{playout_ntp_ms?, jitter_buffer_ms?}` | while audio plays, at the client's cadence; a report with neither field is not sent | §5.3 |
| `lmgw.output_audio.starved` | lmgw | `{response_id, starved}` | a response's audio ran dry, and came again | §5.1 |
| `lmgw.event.part` | both | `{id, index, count, data}` | an event over the receiver's `max-message-size` | §6.3 |

Beside them, on the session and the query:
- `session.lmgw.turn_mode: "automatic" | "push_to_talk"`, on every bound session, either transport
  (§2.2);
- `?chat_folder=<id>[&new=true]`, either transport (§2.2), and `?takeover=ask`, calls only (§2.8).

The rules:
- `*_ntp_ms` is a number of milliseconds on the NTP timescale (from 1900, as RTCP carries it), in
  the clock of the stream it describes: for playout, lmgw's own, as its sender reports carry it.
  `jitter_buffer_ms` is milliseconds. `by` and `held_by` name a client as `voice.bound.by` does.
- They are served on a call bound to a Chat thread or folder, beside the bound session's existing
  `lmgw.chat.*`, `lmgw.model.state`, `lmgw.response.timing`, `lmgw.approval.decided` and
  `lmgw.task.done` events.
- An unbound call sends none of them, and answers a client's with `error` `invalid_event`
  ("… applies to a call bound to a Chat thread"); the session goes on. Its OpenAI events are
  unchanged.
- A bound WebSocket session keeps its close codes and its client-side truncate, and these events
  are not served there either (Later).
- Errors they meet are OpenAI-shaped `error` events with lmgw's codes: `invalid_event`,
  `invalid_value`, `bind_pending`, `event_too_large`.
- Recorded for later decisions, not built: `lmgw.input_audio_buffer.commit` and `.clear` (§4.4) and
  `lmgw.input_audio.preroll` (§4.6).

### 6.5 As built: WP4, events on the data channel (2026-10-15)

The output buffer's events in `writer/paced/pulled.rs` (the tick's `started`, `Shared::done`) and
`writer.rs` (the event after `response.done`); the stop and `clear` in `lifecycle/stop.rs`; the
bound call's client events in `session/call.rs` (`Core::call_event`); the size bounds in
`realtime/calls/sizes.rs` with `sizes/split.rs`, `sizes/empty.rs` and `sizes/join.rs`, wired in
`calls/link.rs` (the sink and the reader) and the driver's writes; the wire type
`OutputAudioBuffer` in `lmgw-api-types` (`realtime/calls.rs`); tests in `realtime_calls_events`
(and the unit tests beside each), §13.1's End in `realtime_calls_close::end`. Details §6 left open,
taken as follows (*changed 2026-10-15*):

- **`started` comes from the frame clock**, in the tick that sends the response's first frame (a
  frame carrying audio of a response other than the last one that did), ahead of the transcript
  deltas that tick releases, on any call. The tick runs after the writer sent the response's
  `response.content_part.added`, so the order is OpenAI's.
- **`stopped` and `cleared` come from the writer**, after it sends the response's
  `response.done` (*owner's decision 2026-10-15*): they follow even when the shaper had to drop that
  `response.done` (it was too large to be made to fit), because the audio really stopped and the
  event is honest either way; and when an emptied `response.done` has `event_too_large` errors, those
  directly follow the event they describe and `stopped`/`cleared` follow them (the shaper's errors
  are written with their event, ahead of the writer's follow-up): `cleared` when a purge cut the response after its first frame (a barge-in, a
  cancel, a `clear`, a stop), `stopped` otherwise, once; a response that never sent a frame (cut
  while thinking) gets neither. A response cut after its last frame left but before drained (its
  tail still in the phone's buffer) gets `cleared`: the phone mutes it.
- **"While a response's audio plays"** (`clear`, §6.2) is read as: its `started` went out and its
  `response.done` has not (`WriterHandle::speaking`). A response still thinking has nothing in
  the buffer, so a `clear` then sends nothing and cancels nothing, a DEBUG line saying so.
- **`input_audio_buffer.append` on a call** is `invalid_event`, `param: "type"`, "input_audio_buffer.append
  does not apply to a WebRTC call: this call's audio arrives on its RTP track", echoing the event's
  `event_id`.
- **`session.lmgw.output_lead_ms` on a call is 0** in `session.created` and after every update,
  whatever the update asked: the value the echo shows is the one in effect, and on a call the lead
  times nothing. No refusal: a stock client never sends it.
- **The stop also drops a `response.create` queued behind the active response** (decision 6; tested in `lifecycle/tests/stop.rs`, with the held
  create of the Pending phase) (one sent while it
  closed or played, §4.3's Closing and Playing phases): it would start speaking right after the
  cancel, and nothing may speak after the tap. A response past its generation with every item
  closed finishes as generated (DEBUG), and what was owed, held or queued is dropped all the same.
  The dropped turns are timed from no response (`timing_unanswered`). One DEBUG line names what
  the stop stopped, or that nothing was open.
- **One rule for a bound call's client events elsewhere** (§6.4): on an unbound call and on a
  WebSocket, `lmgw.response.stop`, `lmgw.output_audio.playout`, `lmgw.session.take_over` and
  `lmgw.event.part` are each `invalid_event`, `param: "type"`, "<type> applies to a call bound to a
  Chat thread (POST /v1/realtime/calls with chat_thread or chat_folder)". An unbound call's
  parts are refused by its reader, once per set (below); a WebSocket's by the session.
- **The shaping is the call's link's** (`sizes::Shaper`, one for the session's sink and one for the
  frame clock, so a transcript delta or `started` over a tiny limit is shaped too). An event at or
  under the largest message goes as it is; anything else is shaped before the driver sees it.
- **Parts carry an `event_id` of their own** (§6.4: lmgw's events carry one), each minted from the
  session's ids as the part is made, the room for it the longest id the session mints (36 bytes
  of JSON). They are `{"event_id", "type", "id", "index", "count", "data"}`, `id` the event's own
  `event_id`. lmgw-client's splitter is a twin kept apart on purpose: lmgw's parts are joined by
  lmgw-client's joiner in the tests, and the phone's are split by lmgw-client's splitter.
- **An event's messages are one write** (the driver's `Command::Send` and `ClockEvent` carry a
  list): they go into the call's one queue of writes together, so a set's parts stay back to back
  even when the frame clock queues a delta meanwhile, and the writer's wait (the back-pressure,
  §6.3) is for the last of them.
- **Which field is emptied** (`sizes/empty.rs`): a string, an array or an object under a key that is
  neither the event's `type` nor its `event_id` nor an id (a string under `id`, or under a key ending
  in `_id`, at any depth; an object or an array under such a key is a schema's property, as
  `input_schema.properties.id`, not an id), an array or an object only with no id anywhere inside, so
  every id arrives. A field already emptied is not picked again. Of those, the
  one whose emptying saves the most; and when a field inside it alone would bring the event within
  the limit, the narrowest such field on the way in (its largest child, while that alone suffices).
  So a long `instructions` goes rather than the `session`, a list of tools goes as `[]` rather than
  tool by tool, and `response.done` keeps its items and their ids while their text goes
  (`response.output[0].content[0].text`). The marker is `[omitted: <the field's size> over this
  call's <limit> event limit]` (sizes in bytes, whole KiB or MiB to one decimal). An emptied event is
  written from a `serde_json::Value`, so its keys come sorted.
- **The `event_too_large` error** is an `invalid_request_error`, `param` the field's path (keys
  joined by `.`, indexes as `[n]`), its message "<type> <event_id>: its field <path> (<size>) was
  emptied to fit <the client's max-message-size of N bytes | realtime.max_message_mb (N bytes)>",
  each also a WARN line. An error over the limit itself, an event that cannot be made to fit (its
  ids alone over it) and one that cannot be split (a limit too small for a part) are not sent, at
  WARN naming the bound, as before WP4.
- **The joiner runs in the call's reader** ahead of the commit window (§4.4): the joined event is
  stamped as its last part came off the socket, and a refusal travels as `Incoming::Refused`, so
  it keeps its place among the events. The refusal's `event_id` echoes the part that broke the set
  (none when another message broke it). A set's parts count against the call's budget while they
  wait for the reader (§3.7) and the joined text against the read-ahead once passed on; in the
  joiner they are bounded by `realtime.max_message_mb` as the settings read at the open. The
  joiner remembers the latest set that broke, not every one, so a client that floods broken sets
  grows nothing; a conforming client's remaining parts follow the break at once, and an
  interleaving client's sets may each be answered more than once. A part 0 with the broken set's
  `id` opens a new set (a client may reuse its event's `event_id` as the set's `id` and retry after
  the error), only the later parts are dropped unanswered. A set still open when the call ends is
  dropped with a DEBUG line. One setting, `realtime.max_message_mb`, bounds both the join and the
  call's inbound budget (§3.7), so a legitimate set near the setting can end the call if the session
  lags behind its reader. `realtime_calls_events::sizes` sends its set past the setting (the setting plus
  one byte, in five parts, back to back, as the parts of one set cannot be read one by one) and so
  carries that one residual margin: the call ends `TooBig` only if the session has read none of the
  set when its fifth part is sent.
- **The pre-session events are not shaped**: `say()` in `calls/open.rs` (the bind's refusal, the
  `lmgw.session.closed` of a call that ends before its session) writes small fixed events straight
  to the driver, past the `Shaper`; their text is lmgw's own, never a client's data.
- **The large events, measured** (§6.3, §16; `realtime_calls_events::measure` on a test gateway with
  self-admin at `full`, the table on stderr):

  | Event | JSON | Over 256 KiB |
  |---|---|---|
  | a bound call's `session.created` (no tools: the thread's run in its chat turn) | 1,673 bytes | no |
  | `lmgw.chat.frame` of `lmgw__image_recipes`' result (71,007 bytes of text) | 155,034 bytes | no |
  | of `lmgw__settings` (7,400) / `lmgw__usage` (4,890) / `lmgw__status` (3,282) | 16,681 / 11,210 / 7,531 | no |
  | of `lmgw__logs`, `lmgw__models`, `lmgw__prices`, `lmgw__profiles`, `lmgw__upstreams`, `lmgw__agents`, `lmgw__docs_corpora`, `lmgw__mcp_servers` | 400 to 5,000 bytes | no |
  | an image result: its own 377 bytes plus the image base64-encoded | over from an image of 196,324 bytes (192 KiB) | from there |
  | a 100 KiB JPEG / a 500 KiB PNG / a 2 MiB screenshot | 136,913 / 683,045 / 2,796,581 bytes | no / 3 parts / 11 parts |

  So **only the chat turn's tool results pass the limit**. A text result carries its text twice
  (as `output`, and as the MCP `content` block an MCP Apps view reads), escaped once more inside the
  frame: its frame is about 2.2 times the text (2.18 for the largest), so a text result passes
  256 KiB from about 117 KiB of text. The gateway's own lists (models, prices, logs, usage) are a
  test gateway's here and grow with the box: a price table or a model list of that size travels as
  parts. Two catalogs are not measured: `lmgw__llama_flags` reads a container image and
  `lmgw__audio_catalog` a catalog refreshed from GitHub, neither of which a test gateway has. An
  image result passes from a 192 KiB image, so nearly every screenshot does. `realtime_calls_events::sizes`
  is sized on these: its parts carry an event as large as a 500 KiB image's result against the
  phone's real 256 KiB, its emptied fields a 100 KiB text against the 64 KiB of a phone that names
  no limit.
- **Tests** (`realtime_calls_events`): `buffer` (no audio on the channel, `append`, the lead,
  `started`/`stopped`, a barge-in's `cleared`), `openai` (the cancel-then-clear form, `clear` alone,
  `clear` with nothing playing, the WebSocket's wording), `stop` (at a position, heard cut at the
  sample it names, exact to the frame whatever the scheduling; with nothing open and twice; while
  the turn waits for its transcript; the `lmgw.*` events refused on an unbound call), `sizes` (parts
  both ways, a broken set, a set past the setting, emptied fields, a part on an unbound call),
  `measure`. One residual margin only: the answers the barge-in and the stop must catch
  (`buffer`, `end`) are six times the fixture's length (about 22 s) against the two seconds of the
  test's own pacing, and the set past the setting in `sizes` is only the setting plus one byte (the residual
  margin above). §13.1's End joined `realtime_calls_close::end`: a stop and a push-to-talk commit right
  before the phone's close both applied, the reply stored cut, the turn stored after it. Not tested
  end to end: a stop while a server-side tool runs and a truncate on a call (their paths are a
  cancel's and the WebSocket's), the cancel racing the response's end (a timing the suite cannot
  make without a margin), and heard before the item's first frame and past the last frame (the
  pulled queue's unit tests cover the mapping).

## 7. The transport seam (WP1)

### 7.1 Shape

```
realtime/link.rs         the seam: Incoming, Close, the link's reader, sink and liveness source
realtime/link/ws.rs      the WebSocket implementation: today's reader, sink and pings, moved
realtime/calls/link.rs   the WebRTC implementation (WP2 to WP5)
```

- **In.** `Inbox` yields `Read { what: Incoming, at }` with
  `Incoming::{Event(Utf8Bytes), Audio { pcm, captured }, Binary, Pong, End(End)}`. The WebSocket
  reader yields `Event`, `Binary`, `Pong` and `End`, exactly as its `Message` arms do today, and
  keeps its read-ahead budget. The call's reader yields `Event` (joined from parts on a bound call,
  §6.3), `Audio` and `End` (§4). `End` comes behind every `Event` that arrived before the channel's
  close, so the session applies them first (the End, §8.1).
- **Out.** `writer::spawn` takes the link's sink: send an event's text, ping, close. Audio is the
  one branch: the WebSocket releases it by the `Pacer` and encodes it as a delta; a call's frame
  clock pulls it (§5.1). The paced queue, purges, the playback record and the drained marker stay
  the writer's; only who releases the audio differs.
- **Close.** `WriterHandle::close(code, reason)` becomes `close(Close)`, with
  `Close::{TakenOver { by }, Revoked(reason), OutOfReach(reason), GoingAway, TooBig(reason),
  NoResponse(reason), HungUp { by }}`; only a call meets `HungUp` (the HTTP hangup). The WebSocket
  maps each to today's code and reason. A bound call sends `lmgw.session.closed` with the kind each
  maps to (§8.2) and closes its `Rtc` once the event is delivered; an unbound call closes its `Rtc`
  at once. Both log the reason. The refusals at the open (§2.8) come before any session, so the
  call's link sends their `lmgw.session.closed` itself.

*Changed 2026-10-14 (WP1 review):* the variant carries the new binder, which
`lmgw.session.closed {kind: taken_over, by}` names (§8.2); the WebSocket builds its reason from it.
`Incoming::Event` carries the frame's `Utf8Bytes` (axum's type), so a large message is not copied;
a call's reader builds it with `From<String>`.
- **Liveness.** `Liveness` takes its evidence and its round trips from the link: pings and pongs
  on a WebSocket, the phone's traffic and its receiver reports on a call (§8.1).
  `answered_meanwhile` keeps its meaning.
- **Limits.** `SessionInit.limits` becomes `Transport::{WebSocket(Limits), WebRtc(CallLimits)}`;
  `Stop::close` reads the WebSocket's.
- **The core** gains `on_audio(pcm, captured)`: `input_append` splits into the base64 decode and
  the append both paths share.
- **The session loop's arms** change type, not behaviour: `Message::Text` becomes
  `Incoming::Event` (the probe ping on the first frame stays the WebSocket's), and likewise
  `Binary`, `Pong` and the end.

### 7.2 WP1's rule

No behaviour change on the WebSocket. The whole `realtime_*` suite, `realtime_liveness` included,
runs unchanged and green, and the existing tests compare the close codes and reasons byte for byte.
WP1 merges with only the WebSocket implementation; the call side arrives with WP2.

## 8. Liveness and closes

### 8.1 Liveness

- A call sends no pings. The phone's ICE agent sends consent checks (STUN binding requests, RFC
  7675) every few seconds whatever its app does; RTP every 20 ms while its microphone track is on;
  RTCP every few seconds.
- **Dead**: one whole `realtime.ping_interval_s` with nothing from the phone, meaning no datagram
  its `Rtc` accepted. The session then ends as a WebSocket's does after an unanswered ping: logged,
  naming the setting. str0m's own ICE-lite verdict (`Disconnected` 15 s after the last check) is
  logged and not acted on: lmgw's verdict reads the setting.
- **Stalled output** keeps its WebSocket meaning: a channel that took nothing for a whole interval
  while events waited gets the same verdict (`flush_alive`).
- **Never opened**: a call whose ICE has not connected one interval after its answer, or whose
  `oai-events` channel has not opened one interval after ICE connected, ends with a log line saying
  which, and its slot goes back. It never bound its thread (§2.8). The same interval bounds a call
  waiting in `bind_pending` (§2.8), which ends with `lmgw.session.closed {kind: "thread_bound"}`,
  and the audio kept before the session starts (§4.6).
- With `ping_interval_s` at 0 there is no bound, as on a WebSocket, and the setting's text says
  that this covers calls too.
- **The End** (C16). The Android client ends a call by sending `lmgw.response.stop`, then closing
  `oai-events`. lmgw guarantees the order:
  - every event that arrived before the channel's close is applied first, in order. The channel is
    ordered and reliable, so what the phone sent before its close arrives before it, and the call's
    reader yields `End` behind those events (§7.1). The stop cancels and purges; a commit still
    inside its window (§4.4) is applied at once, with the frames already in;
  - then the call ends as a hangup ends it: the bound journal drains, the binding lets go
    (`voice.ended {reason: "closed"}` for the other devices), the slot goes back, `Rtc::close`;
  - nothing goes to the phone after its close, `lmgw.session.closed` included: the phone ended it.
    The same holds when the phone closes its whole peer connection: what SCTP delivered before the
    association ended is applied first.
- **The HTTP `hangup` stays** (§2.6): for a call id whose connection is gone (Reconnect starts with
  it) and for stock clients, since it is OpenAI's route (an app server ends its client's call with
  it).
- **The round trip** comes from the receiver reports (WP3 feeds `RoundTrip` from str0m's stats; its
  least-of-five rule and interval bound unchanged). Before the first report the margin is
  `echo_tail_ms` alone, as on a WebSocket before the first pong.
- **Handovers** (§3.6) pause the traffic while they last. A handover shorter than the interval
  (seconds in practice, against the default 20 s) keeps the call, and checks from a new source
  address or port carry it on.
- *Changed 2026-10-15 (WP5):* "nothing from the phone" and "took nothing while events waited" are
  two facts read apart, the client's authenticated traffic (alive) and SCTP acknowledging the
  session's output (reading), so the traffic, which proves nothing of reading, never lifts the
  stalled bound; each verdict comes exactly one interval after its last evidence (§8.5).

### 8.2 Close reasons: `lmgw.session.closed` (C17, Q1)

- OpenAI's WebRTC has no close reason: the peer connection just closes. An unbound call that lmgw
  ends closes the same way (`Rtc::close`: RTCP BYE, SCTP shutdown, DTLS close_notify, so the
  phone's channel closes at once rather than at its consent timeout), with nothing in band.
- **A bound call's last event says why** (Q1, answered). Before lmgw ends a bound call it sends
  `lmgw.session.closed {kind, by, message}` on `oai-events`, then closes the peer connection once
  SCTP has delivered the event. The wait is the grace a WebSocket's last events get: one
  `ping_interval_s`, unbounded with liveness off, and the slot goes back before it, as on a
  WebSocket.
  `by` names a client as `voice.bound.by` does, or is `null`. `message` is the sentence a
  WebSocket's close reason carries; for `revoked` it is led by the `RevokeKind` token, so
  `RevokeKind::of_close_reason` reads it unchanged.

  | `kind` | The call ended because | `by` | `CloseKind` | The Chat feed |
  |---|---|---|---|---|
  | `taken_over` | another client took the thread over | the new binder | `TakenOver` | `voice.ended {reason: "taken_over", taken_over_by}` |
  | `revoked` | the key was revoked, or the device certificate passed its notAfter (device-link D5; RevokeKind `certificate_expired`) (`message` led by the `RevokeKind` token) | `null` | `Revoked { kind }` | `voice.ended {reason: "revoked"}`; the device's own feed sends `revoked` and ends |
  | `out_of_reach` | the thread left the key's reach, or was deleted | `null` | `OutOfReach` | `voice.ended {reason: "revoked"}` or `{reason: "thread_gone"}`, at the levels that still see the thread |
  | `shutting_down` | lmgw is stopping or restarting; or (*changed 2026-10-14*, a device call only) the device listener changed and its socket group moved with it (§3.1) | `null` | `ShuttingDown` | the feed ends; on a device listener change, the device listener's feeds (device-link §4.4) |
  | `closed` | lmgw ended it for no reason above: a hangup over HTTP, a dead or stalled verdict, a refusal at the open that an `error` just before it explains, an internal failure (`message` says which) | who hung up, else `null` | `Other` | `voice.ended {reason: "closed"}` |
  | `thread_bound` | the thread is held: `takeover=never` at the open, or `ask`'s wait ran out (§2.8) | the holder | `ThreadBound` (new) | nothing: the call never bound |

- **The phone's own end** (closing `oai-events` or its peer connection) brings no event: the phone
  ended it (§8.1). The other devices' feed says `voice.ended {reason: "closed"}`.
- A dead call's `closed` is sent too, though it seldom arrives: the verdict means nothing comes
  back from the phone.
- An unbound call has none of this, as with OpenAI.
- **The close order is the WebSocket's**: the event is queued first, then the binding lets go
  (`end_bound`, which publishes `voice.ended`) and the slot goes back, then the writer delivers the
  event and the link closes. v2 let the binding go before the close so that the feed would hold the
  reason first; nothing depends on that order now.
- **The feed stays, for the other devices.** `voice.ended`, `revoked` and the thread's removal are
  published as before, and a device's own feed still carries them (§8.4). The calling phone no
  longer needs them for its own end.
- **The phone does not rebind by itself after a drop**, and sends no probe. It shows the reason and
  offers Reconnect. Reconnect first POSTs `hangup` for the stale call id (admitted at the
  concurrency limit, C3; a 404 means lmgw ended it already), then a new call with `takeover=ask`
  (§2.8).
- *Changed 2026-10-15 (WP2 part B, the hangup's bound):* the hangup asks the call's owner (the open
  path, or the session) and its 200 waits on the call's `Released`, which had no bound of its own
  when the owner never reacts — a session stuck in a flush of its loop behind a client that stopped
  reading, say. **The bound is `server::STOP_WITHIN` (10 s)**, the one a group's close already gives
  the same owners for the same question (§3.7, `Ending::ShuttingDown`): past it the call is closed by force
  (`Calls::hangup_within`), with a WARN line saying so, and the 200 follows; the session then reads
  the call's end as the client's. Neither `ping_interval_s` nor a constant of the route's own: the
  liveness bound belongs to the link, which the stuck session is the one not watching, and 0 there
  would leave the hangup unbounded. A forced close does not mark the stuck session's binding as
  ending (only the session can), so a `takeover=never` call right after such a 200 may still be
  refused by it until the session notices.
  *Corrected 2026-10-15 (the review of part B):* the example read "stuck in its flush", which is
  wrong for the flush of a close: a session marks its binding closing as it leaves its loop, before
  any close site's flush (`session.rs`), so one stuck there no longer holds against a
  `takeover=never` bind. The gap is an owner that has not reached its break: the open path inside
  `thread::check_and_bind` (narrowed now: its look after the bind lets a binding that came back for
  an ended call go, §3.8), or a session inside a `flush_alive` of its loop, the first one before
  the loop or the one after each event, behind a client that stopped reading. The hangup's
  `DocRoute` derives its bound from `STOP_WITHIN` and says the forced case so.
- *Changed 2026-10-15 (WP5, the delivery):* "once SCTP has delivered the event" is the call's
  close after delivery: lmgw's close lets the call go at once (its slot back, its `Released`
  resolved, so the hangup's 200 comes then, and it shows as `closing`) and the driver closes the
  `Rtc` once nothing written is unacknowledged, at most one `ping_interval_s` later; a hangup of a
  `closing` call closes it at once. A dead or stalled verdict's event is handed to the writer only
  if it takes it at once (§8.5).

### 8.3 `lmgw-client`

What the Android client needs from the crate, sans-IO, with owned types, for the UniFFI wrapper:
- **Requests**: the call's request (`POST /v1/realtime/calls`, the offer as `application/sdp` or
  with the `session` part as multipart, the query as the WebSocket builder makes it, with
  `chat_folder`, `new` and `takeover=ask`) and its reader (the call id from `Location`, the answer
  from the body, a refusal through `read_refusal`, which reads `error.held_by`), and the hangup's.
- **Parts**: a joiner that the call's reader passes every `oai-events` message through before
  parsing. A message that is no `lmgw.event.part` passes as it is; a part is held until its set is
  whole, and the joined text is parsed as one event; a broken set (another message inside it, a
  wrong index or count) is an error naming its `id`. And a splitter for the client's own events,
  given the limit lmgw advertised (`RTCSctpTransport.maxMessageSize` on the phone).
- **Readers**, typed `ServerEvent`s: `lmgw.session.bind_pending {held_by}`;
  `lmgw.session.closed {kind, by, message}`; `lmgw.output_audio.starved`; the three
  `output_audio_buffer.*` events; and `session.lmgw.turn_mode` in `session.created` and
  `session.updated`.
- **Builders**, `ClientEvent`s: `lmgw.response.stop {playout_ntp_ms?}`, `lmgw.session.take_over`,
  `lmgw.output_audio.playout {playout_ntp_ms?, jitter_buffer_ms?}` (both optional; a report with
  neither is not built), and `session.update` with
  `session.lmgw.turn_mode` for the mode switch. `ClientEvent::OutputAudioBufferClear` too, for a
  client that speaks OpenAI's form.
- **`CloseKind` from `lmgw.session.closed`**: a reader beside `close_kind(code, reason)` maps
  `taken_over`, `revoked` (its `RevokeKind` from the message's token, as a 4003's reason is read),
  `out_of_reach` and `shutting_down` onto the variants the WebSocket's codes already give;
  `thread_bound` onto a new `CloseKind::ThreadBound`; and `closed`, or a kind this build does not
  know, onto `Other`, whose `code` becomes optional (`None` from the event; ruling 5). It returns
  the kind with `by` and `message`, so a client handles one enum whatever its transport and shows
  the rest as it is.
- **The voice state**: `voice::Machine` reads `bind_pending` (waiting for the user's choice),
  `output_audio_buffer.started`, `.stopped` and `.cleared` as its player facts on a call (audio in
  the player, played out, cut), and `lmgw.output_audio.starved` into `starved(id, starved)`, so a
  tool turn after its spoken preamble reads as thinking (chat-voice §9.3). It also says when the
  remote track is to be muted: from a stop's tap or a `cleared` until the next `started` (§6.2).
- **Not needed any more** from v2's list: the reader that mapped the feed's events to a call's
  `CloseKind` (§8.2), and the expected `response_cancel_not_active` of OpenAI's stop form (§6.2).
  `TurnDetection::requested` stays for WebSocket clients that set turn detection themselves; a call
  bound by the Android client does not need it (§2.2).
- The truncate bookkeeping (`PlaybackCursor`) stays the WebSocket client's; on a call the server
  cuts (§5.4).

*Changed 2026-10-15 (WP5's `lmgw-client` build, details the text left open):*
- **The call's client events are `ClientEvent`'s**: `ResponseStop`, `TakeOver`, `Playout`,
  `BoundSessionUpdate` (the `session.update` of `BoundSession`, the mode switch among it) and
  `OutputAudioBufferClear`. `ClientEvent` is `#[non_exhaustive]` and only `PartialEq` now (the
  times are `f64`), and `ClientEvent::kind` names each event's `type`. A playout report is a
  `PlayoutReport`, which `new` does not build from neither field (a stat that is no finite number
  counts as missing); `ClientEvent::playout` builds the event. The `session` part and the mode
  switch are one type, `BoundSession {turn_mode, half_duplex, echo_tail_ms}`.
- **`error.held_by`** reads into the shared `ApiError`'s new `held_by` (`Option<String>`, left out
  of the dashboard plane's schema, which no `/api` route sends it on), through `read_refusal`, and
  `read_call` refuses with an `ApiError`. A hangup's 404 reads as `HungUp::AlreadyEnded` whatever
  its body.
- **The wire types** `ResponseStop`, `OutputAudioPlayout`, `OutputAudioStarved` and `EventPart`
  are in `lmgw-api-types` (`realtime/calls.rs`), for the core's WP3 and WP4 to share. `*_ntp_ms`
  and `jitter_buffer_ms` are JSON numbers read as `f64`: `estimatedPlayoutTimestamp` is a
  `DOMHighResTimeStamp`.
- **`CloseKind`**: `call_close(&SessionClosed)` answers `CallClose {kind, by, message}`;
  `Other.code` is `Option<u16>`. The event's own `kind` keeps an unknown kind's token for the
  log.
- **Parts.** A joiner holds one set. A broken set is one error naming its `id`, and its parts
  are dropped without another whenever they come (the joiner keeps the broken ids, so sets that
  interleave are each said once). A message that breaks a set still passes as the event it is.
  The doc gives a client no bound on what lmgw's parts join to: `Joiner::new` holds what arrives,
  and `Joiner::bounded(bytes, name)` refuses past a bound the client names (`0` is none, as for
  `max-message-size`). The splitter
  fills each part up to the receiver's limit, which `CallAnswer::max_message_size` reads from the
  answer's `a=max-message-size` (RFC 8841's 64 KiB when it is absent; 0 means no limit). A
  client's sets are `part_1`, `part_2`, and so on.
- **The voice machine on a call.** `interrupted` shows for the `cleared` of a barge-in (its
  cancelled `response.done` said `turn_detected`), not for a stop's. The mute from a stop's tap
  covers the responses open at the tap, so a `started` that crossed the stop leaves the mute on.
  A pending bind's holder is `Machine::held_by`, a fact beside the state, like the mute.

### 8.4 What the Android client relies on, checked against the code

1. **A device's own feed carries `voice.ended` for its call**, with `reason` and `taken_over_by`,
   for a takeover (`reason: "taken_over"`, `taken_over_by` the new binder as `voice.bound.by`
   names it; `by` the device itself) and for a plain end (`reason: "closed"`: a hangup, the phone's
   own close, a dead call). It does **not** for the two other ends, by design:
   - **revoked**: the device's stream sends `revoked {reason, message, kind}` and ends, and nothing
     queued goes out after it (`web/chat_feed/stream.rs`, client-apps §1.6); the `voice.ended`
     published as the session closes never reaches it;
   - **out of reach**: the session's `voice.ended {reason: "revoked"}` is published at the
     thread's new level, which the device no longer sees (`web/chat_feed/live.rs`); the device
     reads the thread's removal instead.

   v2 had the phone read its close reason from these three together. Since v3 it reads it from
   `lmgw.session.closed` (§8.2), and these are what its other screens and the other devices see.
   WP5's contract test still pins the per-device mapping (§8.2's last column); the feed itself
   does not change.
2. **The 409 `chat_thread_bound`** is OpenAI's error shape, `{"error": {"type":
   "invalid_request_error", "code": "chat_thread_bound", "message": "voice is in use on device
   'desktop': this bind asked not to take it over (takeover=never)", "param": null}}`
   (`realtime/thread/bind.rs`). The holder is named, as `voice.bound.by` names it ("device
   '<name>'", or "the dashboard" for the owner), but only inside that sentence. It is not fit to
   show as it is, so WP2 adds `error.held_by` (§2.8). The Android client meets it no more (it binds
   with `takeover=ask` and reads `bind_pending.held_by`); `takeover=never` keeps it.
3. **A `response.cancel` with nothing active** is answered `error`, type `invalid_request_error`,
   code `response_cancel_not_active` (`Core::response_cancel`, `realtime/lifecycle/cancel.rs`),
   message "there is no active response to cancel"; a cancel naming a response that is not the
   active one gets the same code. Nor does a cancel reach an owed or held response (the Pending
   phase). The Android client's `lmgw.response.stop` meets neither (§6.2); a stock client's form
   keeps OpenAI's answer.
4. **The underrun event's name is fixed**: `lmgw.output_audio.starved`, with
   `{response_id, starved}` (C22, §5.1).
5. **A folder's current thread** is decided by `current`'s rules under the folder's lock
   (`web/chat_folders/current.rs`), so binding by folder inside the same lock adds no rule of its
   own (§2.2). Its refusals are `not_found`, `not_ongoing` and `folder_no_model`; the handshake names
   the first `chat_folder_not_found`.
6. **A bound session's turn detection is the client's today**: `lmgw-client`'s
   `TurnDetection::requested` re-derives it from `voice_resolved`, which always has a value
   (thread, else Settings → Chat). `session.lmgw.turn_mode` moves that rule into lmgw (§2.2).

### 8.5 As built: WP5's core, liveness and closes (2026-10-15)

`realtime/liveness/call.rs`, the close after delivery in `calls/driver/call.rs` and `calls/handle.rs`,
the let-go state in `calls/registry.rs`, the session's call side (`session/call.rs`, `session.rs`);
tests in `realtime_calls_close`. Details §8.1 and §8.2 left open, taken as follows (*changed
2026-10-15*):

- **Liveness is two facts, not one.** A review warned that `last_heard` alone is no pong: on a
  WebSocket the pong travels behind the session's own output, so it proves the client reads; consent
  checks, RTP and RTCP come from the phone's ICE agent, microphone and stack whatever its app does,
  and taken as a pong they would lift §8.1's stalled-output bound (a phone with its mic and ICE up
  that stopped taking `oai-events` could hold a close's flush, its binding and its slot for good). So
  a call's verdict (`liveness::call::Watch`) reads two facts the driver keeps in the registry:
  - **alive**: `CallInfo::last_heard`, the client's last integrity-checked traffic (§3.7). One whole
    `ping_interval_s` with none is `Dead`, its reason "no traffic from the client within N s,
    setting realtime.ping_interval_s";
  - **reading**: `CallInfo::unacked_since`, since when the session's output on `oai-events` has
    waited with no SCTP acknowledgement. str0m counts written data as buffered until the client
    acknowledges it (§16); after each drain the call compares the bytes it wrote with the buffered
    amount, and each acknowledgement that takes some off moves the fact to now; nothing in flight
    clears it. One whole interval of output in flight with none acknowledged is `Dead` too, its
    reason "the client acknowledged none of the session's output within N s, setting
    realtime.ping_interval_s". With nothing in flight there is nothing to read, and the fact holds.

  Either failing ends the session as an unanswered ping ends a WebSocket's, in the session loop and
  in `flush_alive` alike, so the stalled-output bound stands: a writer waiting at str0m's send
  window has output in flight that is not acknowledged. `answered_meanwhile` keeps its meaning (no
  pong ever comes on a call; an end already read is the end).
- **Exactly one interval.** The watch reads the facts when the earlier of the two could first be
  due and again from there, so a verdict comes one interval after the last evidence, not between
  one and two as a WebSocket's ping and pong give, and the registry is read once per interval while
  the client keeps both. Each fact counts from the session's start at the earliest: the open path
  has its own bounds (never opened, pending), and a session gives its client one whole interval, as
  a WebSocket session's first ping does. A call sends no ping and gets no probe on its first event.
- **The never-opened and pending bounds** were part A's and part B's (the driver's
  `NeverConnected` and `NeverOpened`, each logged naming the setting; the pending wait from the
  latest `bind_pending`); WP5 adds nothing to them but their tests on the paused clock.
- **The close after delivery** (`CallControl::close_delivered`). lmgw's close of a call (the
  session's writer through `CallSink::close`, the open path's `end`) lets the call go at once and
  hands it to the driver, which closes its `Rtc` once nothing written on `oai-events` is
  unacknowledged (the writes still waiting at the window included) or the channel is gone, at most
  one `ping_interval_s` after the close began (the call's interval at its offer; none at 0, as on a
  WebSocket with liveness off). Past it the INFO line "the client did not acknowledge the last
  events within N s (realtime.ping_interval_s); closed without them". The end without a close of
  lmgw's own (the client closed first, the session went) closes at once. The open path's own wait
  for its last events to be taken (part B's `last_said`) is folded into this one bound: the driver
  keeps one queue of writes per call, so the close comes behind them, and once the close began
  nothing more is written on the call (a write answers `Sent::Gone`), so no event follows
  `lmgw.session.closed`. Tested: the event's first sending lost (the phone discards everything until
  the writer's close, so whatever the order of the 200 and that close), its retransmission still
  reaching the phone before the close; and a phone that never acknowledges it closed without it
  exactly one interval after the close began, with the INFO line, on the paused clock. SCTP
  retransmits a lost chunk after its timer (3 s at first, never below 1 s, doubling up to 60 s), so
  an interval of about 3 s or less can end a call on one lost chunk; `ping_interval_s`'s text says
  so.
- **Letting go before the delivery** (§8.2: "the slot goes back before it, as on a WebSocket").
  A call that let go has dropped what it kept (the slot) and is no longer owned: its `Released`
  resolves, so the hangup's 200 comes then, and its registry entry is `CallState::Closing` until
  the `Rtc` closes. *Corrected 2026-10-15 (the review of WP5's core):* the first build hid such a
  call from `Calls::get`, `list` and the hangup's lookup, so at `ping_interval_s` 0 a hung-up call
  whose phone stopped reading lived on, writer and `Rtc`, where nothing showed it and no hangup
  reached it (its `Released` had resolved at the let-go, so the hangup's force path never came).
  Nothing hides a live `Rtc` now: `get` and `list` show the call as `closing`, and a hangup of a
  `closing` call closes it at once (`Rtc::close`, an INFO line) and answers 200, since nobody owns
  it to ask (`Hangup::Closing`). Only the session's liveness no longer reads it (`evidence`): no
  session owns it. A group's close counts it (`wait_gone`), so the sockets stay open for its last
  event until `STOP_WITHIN`. The session lets its call go (`LetGo`) where a WebSocket session's core
  drops its slot, after `end_bound`, whatever its writer is doing; the close after delivery lets go
  too. What arrives on a call that let go goes to no owner and counts against no budget. Tested at
  0 with a frozen phone: shown `closing` after the hangup's 200, a second hangup closes it at once.
- **A dead or stalled verdict's `lmgw.session.closed`** (`Core::say_closed_now`, kind `closed`, the
  verdict's reason as its message) is handed to the writer only if it takes it at once: the output
  is what waits, and a stalled client's writer is stuck at the window, so a waiting flush would hold
  the session for nothing. A dead call's usually goes out (the writer has room), a stalled one's
  behind what waits when that is within the window. *Corrected 2026-10-15 (the review):* this said
  both were tested reaching the phone through the close's wait; their tests freeze the phone, which
  loses nothing, so they would pass after a close at once too. They test `say_closed_now`: the
  event is taken and sent ahead of the close. The delivery is tested with the close after delivery
  (above).
- **Too big** (`Close::TooBig`). *Corrected 2026-10-15 (the review):* the first build ended the call
  in the driver at the overrun (`Ended::TooBig`, its `Rtc` closed at once), so the session's
  `lmgw.session.closed` met a call that was gone and never went out. Now it goes the way of every
  other kind: past `realtime.max_message_mb` the driver queues nothing more for the owner (what
  the client sends from then on is dropped) and hands it `CallEvent::TooBig` behind what was
  queued, the call still up; the session reads it as the failed read it was (`Close::TooBig`), says
  `lmgw.session.closed {kind: "closed"}` with the message naming the setting (§8.2's table has no
  kind of its own for it, as a WebSocket's 1009 is `CloseKind::Other`), and its `close_delivered`
  closes the call under the same delivery bound. The open path ends a call over its budget too:
  quietly before the open (no channel to say it on), with `closed` while pending. Tested end to
  end: the phone reads nothing of lmgw's output (its network loses lmgw's DATA and lets the
  acknowledgements of its own messages through), so the writer waits at the window, the session
  in its flush and the reader behind the session's full inbox, while the phone's microphone fills
  the queue past 1 MiB (RTP needs no acknowledgement); once the phone reads again every answer
  comes, then the closed event, last.
- **The kinds tested end to end** are those a call on the plain listener meets, made by an owner
  key (the device link's WP3 ended bearer device keys there, and the device listener's calls are
  calls WP7's): `taken_over` (by device `tablet`'s WebSocket, so the new binder and the call's own
  are named apart), `closed` (a hangup, a dead and a stalled verdict, too big), `revoked` (the
  calling key disabled; its `RevokeKind` a non-device key's), `shutting_down` (lmgw's stop; the
  listener's stop is part B's test). A hangup's `by` and the binder's name coincide there: both are
  owner principals, "the dashboard". `out_of_reach` on a running call, and §8.4's view of a device's
  own call (its feed's `revoked` and end, the thread's removal), need a device's own call: an owner
  key never loses a thread's reach, and its session on a deleted thread is left alone. They take the
  same session close site and the same close after delivery, and are tested with WP7. Until then
  §8.4's contract is pinned for what a paired device's feed shows of a call it watches:
  `voice.ended` with `taken_over_by` for a takeover, `closed` for a hangup, a dead call and the
  phone's own close, `revoked` for the caller's key revoked, and the feed's end at a stop.
- **The End** (§8.1) was part A's and part B's (the reader's `End` behind every event before the
  channel's close; no close of lmgw's own after it, so nothing is said to the phone). Tested with
  what exists: an event before the close handled first, then the call ended as a hangup ends it
  (`voice.ended {reason: "closed"}`), nothing sent after. The stop and the push-to-talk commit of
  §13.1's case need WP4's `lmgw.response.stop` and WP3's commit window, and join this test with them.
  *(Joined 2026-10-15, §6.5.)*
- **`ping_interval_s`'s text** says what it bounds on a call (the settings class); the dashboard's
  hint is WP6's.
- **The test phone runs on tokio's clock** (`realtime_calls_net::client::now`): unpaused it is the
  system's, and a test that pauses time pauses the phone with lmgw, so the bounds are measured on
  one virtual timeline after a connect in real time.
- **`advertise::CALLS_READY` stays false**: WP3's wiring of the call's audio is not on main yet;
  whichever of WP3's wiring and this lands second sets it. *(Changed 2026-10-15: WP3 slice B
  landed second and set it, §5.5, §9.)*

## 9. Settings and surfaces

New `realtime` settings:

| Setting | Default | Meaning | Applies |
|---|---|---|---|
| `webrtc_enabled` | `true` | the UDP listener, both socket groups (§3.1); off: `/calls` answers 503 naming this setting | restart |
| `webrtc_port` | `null` | the plain group's port (§3.1, changed 2026-10-14): `null`: the HTTP port's number; `0`: chosen by the OS; else the port. The device group takes the device listener's port | restart |
| `webrtc_reorder_ms` | `60` | how long a frame waits for a missing earlier one; `0`: no wait (§4.2) | next call |
| `webrtc_commit_window_ms` | `120` | how far past its stamp a push-to-talk commit takes frames, and how long it waits for them (§4.4) | next call |
| `webrtc_output_kbps` | `32` | the Opus output bitrate, capped by the phone's `maxaveragebitrate` (§5.2) | next call |

*Changed 2026-10-15 (WP3 slice A):* the three audio settings' ranges, which the table left open,
each refused by name on a save and clamped on load with a warning
(`config/settings/webrtc_audio.rs`, flattened into the realtime section beside WP2's `webrtc.rs`):
`webrtc_reorder_ms` 0 to 40 950, because str0m holds at most 4096 packets of a stream behind a gap
(`MAX_BUFFERED_PACKETS`) and the reorder count is the window over the 10 ms `minptime` plus one, so
past that the count would bind first and str0m drop the oldest packet; `webrtc_output_kbps` 6 to
510, Opus's own range; `webrtc_commit_window_ms` none, the owner's call. They apply to the next
call as every realtime setting applies to the next session, so a save's answer adds no note for
them.

- Existing texts gain the call's meaning: `ping_interval_s` (§8.1: also the never-opened and
  pending bounds, and when playout reports count as stopped), `echo_tail_ms` (the call's margin,
  §5.3), `output_lead_ms` ("not used on a call"), `max_message_mb` ("on a call: what a bound
  client's parts may join to; a single message is bounded by the WebRTC stack, 256 KiB") and
  `max_frame_mb` ("WebSocket only").
  *Changed 2026-10-15 (WP2 part A's reviews):* on a call `max_message_mb` also bounds the largest
  event lmgw sends in one message (below the client's `max-message-size`) and the bytes waiting for
  the call's session (`Ended::TooBig`, §3.7, §6.3); its text says so in the settings class, the save,
  the wire type and the dashboard's hint.
- The save (`ops/realtime_settings.rs`, one patch for the dashboard and the tool plane) refuses a
  port out of range, and its answer says when a change applies.
- **Status** (`ops::reads::status`, `GatewayStatus`) gains a `webrtc` block: enabled, the bound UDP
  address or the bind error, and the live calls (call id, state, pending with its holder, session
  id, key name, candidate, remote address, connected since, round trip, loss, the phone's jitter,
  the playout allowance and its source, the latest report's `jitter_buffer_ms` and age, the bound
  thread). *Changed 2026-10-14, after the owner's answers and with the socket groups (C4):* the
  block shows both groups (§3.1), each with the rule that bound it (the plain listener's
  `bind_addr` and `webrtc_port`, or the device listener's addresses and port), its bound addresses
  or its bind error; each call names its group.
  *Changed 2026-10-15 (WP7):* the groups' half is built (`GatewayStatus.webrtc`: `enabled` as the
  start read it, `groups` with `group`, `rule`, `state`, `addrs`, `error`, and `dropped`, §3.9);
  the live calls are WP6's. *(Built 2026-10-15, WP6, §13.3: `webrtc.calls`. Changed in its review:
  the round trip is the session's, the receiver reports' age stands beside the figures they give,
  and a `closing` call shows none.)*
- **Dashboard.** Settings → Realtime gets a WebRTC group: the five rows; the listener line (bound,
  failed and why, or off); the firewall hint; the fixed 256 KiB event bound beside the WebSocket
  limits. *Changed 2026-10-14, after the owner's answers and with the socket groups (C4):* the
  listener line shows both groups, each with its rule and addresses. The firewall hint names the
  device port, TCP and UDP, for phones, and the plain group's UDP port only when `bind_addr` is not
  loopback (§3.1). A device's call shows as its Voice link on the Devices card, as a WebSocket
  session does (`LinkKind::Voice`). *(Built 2026-10-15, WP6, §13.3: the group with the live calls,
  the 256 KiB bound in the Connection group, the link saying "WebRTC call".)*
- **API docs.** `DocRoute`s for `POST /v1/realtime/calls` (both request bodies; the `201` with
  `application/sdp` and `Location`; §2.5's refusals) and for `hangup`; `/v1/realtime`'s
  `DocRoute` gains the `sideband_unsupported` response and corrected text (WebRTC served at
  `/calls`, `output_audio_buffer.clear` on calls only, the `output_audio_buffer.*` events).
  `chat_folder` and `new` are described from `RealtimeQuery`, as its other fields are; the folder
  refusals join the responses of both routes; `/calls`' text lists the bound call's events (§6.4)
  and `takeover=ask`. No new op and no new `x-lmgw` header.
- **`/v1/models`**: `realtime::advertise` adds `/v1/realtime/calls` beside `/v1/realtime` while the
  listener is up. *Changed 2026-10-15 (WP2 part B):* "the listener" is the socket group of the
  listener the `/v1/models` request came in on (`Conn::via`): bound, the route is listed; off,
  failed or down, it is not, so a client is told calls work where it asks. The device listener's
  group is WP7's, so until then a request there sees no calls route (§3.8). *(Changed 2026-10-15,
  WP7: built; a request on the device listener follows the device group by the same rule, behind
  `CALLS_READY` below.)*
  *Changed 2026-10-15 (the review of WP2 part B, the owner's decision):* the route is listed only
  once a call works end to end: not before WP3's reader and writer are wired (until then a call
  carries no audio) and WP5's liveness has landed (until then a call's session hears no pong and
  ends as unanswered after two `ping_interval_s`). `realtime::advertise::CALLS_READY` is `false`
  until then, the per-listener rule above kept behind it; WP5 sets it. The route itself answers
  meanwhile; it is only not offered. *(Changed 2026-10-15: set, by WP3 slice B, which landed on
  top of WP5 and WP7; the route is listed on each listener while its group is bound, tested on
  both, `realtime_calls::route`.)*
- **Logs**: a session's start line names its transport and, for a call, the candidate and the
  remote address. Request rows keep the `realtime` label.

## 10. Security

- **The SDP travels with the credential.** *Changed after the owner's answers, 2026-10-14:* the
  DTLS fingerprints that authenticate the media are in the offer and the answer. **Device calls
  exchange them inside mTLS**: a phone POSTs to the device listener, which it verified against the
  pairing link's `fp` and which verified the phone's key (device-link §8, §10), so the exposure is
  closed for them. **A stock client on a non-loopback plain listener keeps it**: over WireGuard the
  tunnel authenticates and encrypts that exchange; on a LAN without the tunnel an on-path attacker
  could swap the SDP and sit in the middle of the media, the exposure the bearer key in the same
  request already has. A TLS face for client keys is on the device link's Later list.
- **The media is better off than a WebSocket's.** DTLS-SRTP encrypts the audio and the events
  whatever the HTTP side does; a plain-HTTP WebSocket carries PCM and transcripts in the clear.
- **The UDP port answers no stranger.** In ICE-lite str0m answers only binding requests that carry
  a call's ufrag, arrive at its candidate and pass its MESSAGE-INTEGRITY check (the per-call
  password from the answer). Everything else is dropped without a reply, so the port neither
  amplifies nor tells a scanner that a call exists. A flood costs one parse per datagram in the
  driver.
- **Who may call** is the WebSocket's rule (§2.3), with the subprotocol key gone.
- **The answer names one address**, the one the caller's POST already reached (§3.2): it tells a
  caller nothing about the host's other interfaces.

## 11. Dependencies and build

- **str0m**, MIT, with `default-features = false, features = ["aws-lc-rs"]` (its default features
  also pull in its examples' web server). The released 0.24.1 (2026-10-03) lacks #1065, "Fix RTCP
  receiver report loss fraction wraparound", merged to main on 2026-10-04 (`02fc51b`) and listed
  under Unreleased; C9's FEC tuning reads that fraction. So the dependency is a git revision of
  main that includes it (`02fc51b` or newer; WP2 takes the newest that passes, which also brings
  #1066, "send on a nominated ICE pair only after its check succeeds", and #1070's DLRR RTT
  routing), and moves to the first release that ships #1065. `aws-lc-rs` is in the tree already
  (1.17, through rustls and quinn); WP2 checks with `cargo tree -d` that `str0m-aws-lc-rs` unifies
  with it. str0m's own crates come along: `is` (ICE), `dimpl` (DTLS), `sctp-proto`, `str0m-proto`.
  *As built, WP2 part A (2026-10-15):* pinned at `634e76bb` (main, 2026-10-11), which carries
  #1065, #1064, #1066, #1070 (NACK retries by RTT, and the DLRR RTT routing) and #1071 (TWCC
  receive timing). `cargo tree -d` shows one `aws-lc-rs` (1.18.2, raised from 1.17 by the device
  link's rcgen) and one `aws-lc-sys` (0.46.0): `str0m-aws-lc-rs` unifies, adding the
  `prebuilt-nasm` feature, which acts on Windows only. dimpl brings RustCrypto crates the tree did
  not have (`aes` 0.8, `cipher` 0.4, `der`, `x509-cert` and their kin), beside the newer ones it
  had.
- **`quinn-udp` 0.5**, MIT or Apache-2.0, as a direct dependency: 0.5.14 is compiled in already
  through quinn, so it adds no code to the build. It reads each datagram's destination (§3.3).
  *Corrected 2026-10-15 (WP2 part A):* 0.5.14 was in `Cargo.lock` only as an optional dependency
  of reqwest's `http3`, which lmgw does not enable, so nothing compiled it; the direct dependency
  builds quinn-udp itself (small, on the tree's `socket2`). The locked 0.5.14 is used.
- **Opus: `opusic-c` 1.6.1** (BSD-3-Clause, 2026-04) with `default-features = false`, which links
  the system libopus (`-lopus`, dynamically; `OPUS_LIB_DIR` and `OPUS_LIB_STATIC=true` steer it).
  - **The `opus` 0.4.0 crate cannot do that.** It depends on `opusic-sys` 0.7 with default
    features, and `opusic-sys`'s default `bundled` feature builds its vendored libopus 1.6.1 with
    cmake; Cargo cannot turn a dependency's default feature off from downstream. `opusic-c`, from
    `opusic-sys`'s author, forwards `bundled` as a feature of its own (default on), so it can.
  - `opusic-c` hands decoded samples back as `u16`; one cast makes them `i16`.
  - `audiopus` is unmaintained since 2021, and the pure-Rust decoders are not ready.
- **Build consequences**:
  - `ci/install-build-deps.sh` installs `opus-devel` (Fedora ships 1.6). No cmake is needed.
  - The RPM gains `opus` in `bundle.linux.rpm.depends` (`src-tauri/tauri.conf.json`); every Fedora
    desktop has it already, through PipeWire and GStreamer.
  - The AppImage bundler copies `libopus.so.0` like any linked library.
  - For a build host without `opus-devel`, a cargo feature of lmgw's forwards `opusic-c/bundled`
    (cmake, the vendored 1.6.1, statically). Not the default.
    *As built, 2026-10-15 (WP3 slice A):* lmgw-core's `opus-bundled`. The README's Fedora build
    list gains `opus-devel` beside `ci/install-build-deps.sh`, which every CI job that builds
    (the RPM build, GitHub's tests and release) runs on `fedora:latest`.
  - The str0m git dependency is fetched like a crate at build time.
- **No interface enumeration**: the candidate comes from the POST (§3.2).
- **Licences**: MIT for str0m and its crates, MIT or Apache-2.0 for `quinn-udp`, BSD-3-Clause for
  `opusic-c`; libopus is the system's. No new files under `/usr/share/licenses/lmgw/`: those are for
  what lmgw vendors (ONNX Runtime, Silero, Smart Turn).
- **Not chosen**: webrtc-rs 0.21.1 (2026-10-11), a fresh rewrite as an async layer over the
  sans-IO `rtc` crate with a browser-style API, whose ICE-lite is unverified: the fallback if
  str0m fails us. LiveKit's `libwebrtc` crate, which downloads Google's prebuilt libwebrtc: a
  browser's media engine for a server that needs none of it, built outside the distribution.

## 12. Clients

- **The Android client** is the user. Its media engine is libwebrtc (AEC, NS, NetEQ, Opus) with
  continual gathering (`GATHER_CONTINUALLY`); which build it uses is the Android design's. Through
  `lmgw-client` (UniFFI) it gets the request builders, the part joiner and splitter, the event
  readers and builders, `CloseKind` from `lmgw.session.closed`, and the voice state (§8.3). It:
  - binds with `takeover=ask`, by `chat_folder` for an ongoing conversation (lmgw picks the
    folder's current thread) or by `chat_thread`. On `bind_pending` it shows the holder and asks;
    Take over sends `lmgw.session.take_over`, declining hangs up (§2.8);
  - shows its listening cue at DTLS-connected: its first frames are kept (§4.6);
  - reads its turn mode from `session.lmgw.turn_mode` and switches it with the same field (§2.2);
  - sends its client-owned session fields in the `session` part (§2.2);
  - stops with `lmgw.response.stop` carrying its playout position, muting the remote track at the
    tap; mutes on `output_audio_buffer.cleared` and unmutes at the next `started` (§6.2);
  - reports its playout while audio plays, at its visible cadence (§5.3);
  - joins lmgw's parts and splits its own large events (§6.3);
  - commits at a push-to-talk release, its microphone tail staying enabled after the release and
    fitting inside `webrtc_commit_window_ms` (§4.4); *Changed 2026-10-14 (consistency pass):* it no longer holds the commit back;
  - ends a call with `lmgw.response.stop`, then closing `oai-events` (§8.1);
  - after a drop, shows the reason from `lmgw.session.closed` and offers Reconnect, which hangs up
    the stale call first and binds again with `takeover=ask`; it does not rebind by itself (§8.2).
- It also needs client-apps' pending WP10: the `message.*` feed records and the complete typed and
  documented Chat subset (the DTOs and `DocRoute`s the Android client reads). It also needs the
  pairing QR code, which is the device link's WP4, not client-apps WP10 (*changed 2026-10-14*).
  This design neither redesigns them nor depends on their details; they gate the Android client,
  not the calls route.
- **Kai desktop and the dashboard's voice panel stay on the WebSocket** (C19). A browser can call
  lmgw anyway (§13.2 runs OpenAI's own example against it), but no browser client is planned.
- **OpenAI SDK clients** work unchanged: the Agents SDK's WebRTC transport pointed at
  `http://<lmgw>/v1/realtime/calls` with an lmgw key (in a browser with `useInsecureApiKey`).
- **Reaching lmgw from the phone**: *changed after the owner's answers, 2026-10-14:* the phone
  reaches the device listener (device-link §4), not `bind_addr`, and the firewall opens TCP and UDP
  of the device port in the zone the phone comes from (§3.1). The pairing link v2 carries the URLs
  (device-link §3.1).

## 13. Tests

### 13.1 Integration tests (`tests/it`)

- Modules `realtime_calls` (route, refusals, answer, policy, hangup, binding at open, `ask`, the
  folder), `realtime_calls_net` (candidate, destination, handover), `realtime_calls_audio`,
  `realtime_calls_events` and `realtime_calls_close`; `ci/changed.py`'s naming rule picks them for
  any change under `src/realtime/`. `chat_folder` and `turn_mode` on the WebSocket go into
  `realtime_chat_thread`.
- **The client is str0m itself**, in-process. A test builds an offer (audio send-receive with Opus,
  the `oai-events` channel), POSTs it, applies the answer and drives its `Rtc` on its own UDP
  socket in the test task. Its audio comes from the committed fixtures
  (`tests/fixtures/realtime/audio/`), Opus-encoded with `opusic-c` on the RTP clock; received
  frames are decoded and checked. The session's models are the existing realtime fakes. The
  harness binds UDP with `webrtc_port: 0` and reads the port from the status block.
- The client owns its socket, so loss, reordering and delay are injected deterministically: drop
  the n-th packet, swap two, hold one past the window, delay its RTCP or vary the delay of what it
  receives (which sets the round trip and the jitter its receiver reports carry), send each frame a
  fixed time after its RTP time (a capture-to-send offset), jump its RTP clock, send RTP before the
  channel opens, hold the channel's open back.
- The client also speaks the bound contract: it joins and splits parts, and it builds playout
  positions from lmgw's sender reports as it received them and from the frames it decoded, so a
  test knows which frame each position names (it needs str0m's received sender reports, the WP2
  check of §16).
- Cases:
  - **route**: 201, `Location`, the answer (Opus only, `a=ice-lite`, one candidate); raw SDP and
    multipart; the `session` part as the first update (`session.created`, then `session.updated`,
    or an `error` for a part `session.update` would refuse); every refusal of §2.5, the folder's
    and the parameter clashes among them; 503 with the listener off; `?call_id=` and
    `takeover=ask` refused on the WebSocket route;
  - **binding at open**: no takeover at the POST (a WebSocket session bound to the thread keeps
    running while the call is answered); a call whose ICE never connects ends by the bound and
    never touches that session; the takeover when `oai-events` opens (4000 on the WebSocket); a
    bind by another client between the POST and the open, under the default (this call takes
    over) and under `takeover=never` (`lmgw.session.closed {kind: "thread_bound", by}` naming the
    holder, then the close, no `session.created`); a thread out of reach at the open
    (`out_of_reach`); a policy refusal at the open (its `error`, then `closed`);
  - **`takeover=ask`**: a free thread at the open binds at once; a held one gets
    `bind_pending {held_by}`, then `take_over` closes the holder (4000 on a WebSocket,
    `lmgw.session.closed {kind: "taken_over", by}` on a call) and `session.created` follows; the
    holder hanging up while pending lets the call bind with no word from it; a third client taking
    the thread meanwhile brings a second `bind_pending`; other events while pending answered
    `bind_pending`; RTP sent while pending is in no turn; the wait ends after one interval with
    `thread_bound`; a hangup while pending ends it;
  - **folder**: `chat_folder` binds the current thread on both transports, reported in
    `resolved.chat_thread`; a rollover at the bind for an idle thread and for `new=true`, an empty
    current thread kept under `new=true`, `folder.current` on the feed; two binds by folder at once
    get one thread (the lock); a call's POST makes no thread, its open does; under `ask`,
    `take_over` binds the folder's thread as it is then;
  - **turn mode**: a thread on `push_to_talk` starts with `turn_detection: null` and
    `turn_mode: "push_to_talk"`; one on `server_vad` starts automatic with `server_vad`; a switch
    to automatic on a push-to-talk thread gives `semantic_vad`; an update whose `turn_mode` and turn
    detection disagree refused; the field refused on an unbound session;
  - **policy**: a second call at a concurrency limit of 1 refused, a hangup admitted at the limit,
    the slot back after the hangup, after a call that never opened and after a pending one; a
    device's scope over the thread's aliases; 409 under `takeover=never`, its `error.held_by`
    naming the holder as `voice.bound.by` does;
  - **hangup**: 200 and the session's end, the bound call's `lmgw.session.closed {kind: "closed",
    by}` before it; a `takeover=never` or `ask` call right after the 200 neither refused nor
    pending; 404 for another key and for an ended call; the owner hanging up a device's call;
  - **net**: a wildcard bind (`0.0.0.0` and `[::]`) where the client posts over loopback: the
    candidate is the POST's local address, made canonical under `[::]`, and str0m accepts the
    checks because the destination comes from the packet info; a client that moves to a new
    source port mid-call keeps its audio both ways;
  - **the socket groups and the device group's live rebind** (*added 2026-10-14*, in
    `realtime_calls_net`, with WP7): a call POSTed on the device listener gets the device
    listener's address as its candidate, on the device port, and connects on the device group's
    socket for that address. On a loopback device listener, one call over `127.0.0.1` and one over
    `[::1]` each connect on their own socket. A stock call on a non-loopback plain listener
    connects on the plain group. Moving the device listener ends a live device call (a bound
    call's last event `lmgw.session.closed {kind: "shutting_down"}`), leaves a stock call on the
    plain group running, and the next device call connects on the new socket. Turning the device
    listener off ends its calls and closes the device group, and the plain group serves on;
  - **audio in**: a VAD turn end to end; one drop recovered by FEC (the samples continuous, the
    timeline as long as the RTP span); a burst concealed; a reordered pair inside the window, a
    frame past it; an RTP jump beyond the wall-clock bound (concealment capped at the wall time,
    the INFO line, stamps re-anchored);
  - **first frame**: speech whose RTP starts at DTLS-connected, with the channel's open and the
    bind held back: the word is whole in the first turn, at its RTP stamps, after the `session`
    part's update;
  - **commit window**: a push-to-talk turn whose frames leave 60 ms after their RTP time and whose
    commit leaves at the release: every frame captured before the release is in the committed
    turn; with the window set below the offset, the late frames land in the next turn and the
    DEBUG line says how late; a `clear` at a press keeps the speech after it;
  - **audio out**: frames every 20 ms on the RTP clock; silence between answers as ordinary
    frames, and as DTX only with `usedtx=1` in the offer; `started`, `response.done`, `stopped` in
    that order; the window's end, drained and the margin from the round trip and jitter the
    client's delays set, each term once;
  - **playout reports**: with the client's playout delay set, the window's end and drained follow
    the reports, not the formula; the formula before the first report and on an unbound call;
    reports that stop for one interval bring the fallback and its INFO line, and a new one takes
    over again; a report naming no frame sent ignored; the report refused on an unbound call;
  - **underrun**: a bound call's `lmgw.output_audio.starved` true at the first padded tick and false
    at the next real frame, none on an unbound call;
  - **barge-in**: `speech_started`, the cancel, the cancelled `response.done`
    (`turn_detected`), `cleared`; heard is the frames sent on an unbound call, and on a bound one
    the frames sent less the allowance's worth; a backchannel during playback sends neither
    `speech_started` nor `cleared`; with `interrupt_response` off, `speech_started` and no
    `cleared`;
  - **stop**: `lmgw.response.stop` while thinking (an owed response and a held `response.create`
    dropped, nothing speaks after it), while speaking (the cancelled `response.done`, `cleared`,
    the purge), while a server-side tool runs; sent twice (the second sends nothing); after the
    response ended (nothing, no error); with `playout_ntp_ms` (heard at the frame and offset it
    names; before the item's first frame, nothing heard; past the last frame sent, clamped);
    without it (heard as sent); on an unbound call, `invalid_event`;
  - **OpenAI's stop and `clear`**: the form (one `cleared`, nothing for the `clear`); the form
    racing the response's end (`response_cancel_not_active`); `clear` alone; a cancel alone;
    nothing playing (no event); `append` refused; a truncate on a call;
  - **closes**: a takeover from a WebSocket to a call and from a call to a WebSocket on one
    thread; for each kind of §8.2's table, the bound call's last event is `lmgw.session.closed`
    with that kind, `by` and message, the `RevokeKind` token leading a revocation's, and the close
    follows its delivery; an unbound call closes with nothing in band; `lmgw-client`'s reader maps
    each kind to its `CloseKind`; the device's feed carries what §8.2's last column says; the phone
    silent for one interval, ended; consent checks without RTP, kept; the channel never opened,
    ended;
  - **the End**: a stop and a push-to-talk commit sent right before the channel's close are both
    applied (the cancelled `response.done` journaled, the turn stored), then the call ends as after
    a hangup (`voice.ended {reason: "closed"}`), with no event sent after the close;
  - **sizes**: on a bound call an event over the client's advertised `max-message-size` (the test
    offers a small one) arrives as parts, each within the limit and back to back, that join to the
    event; a client's event sent as parts is applied, a broken set is refused with its `id`, and a
    set past `max_message_mb` is refused naming the setting; on an unbound call the event arrives
    with its largest fields emptied, followed by one `event_too_large` per field, and a part from
    the client is `invalid_event`.
- WP1's own gate is the existing suite, unchanged.

### 13.2 Interop with libwebrtc: `scripts/webrtc-interop.sh`

- **Real libwebrtc on the other side**: headless Chromium in the Playwright image, in a rootless
  Podman container, run as `scripts/media-probe-chrome.sh` runs it, against a dev instance
  (`scripts/dev-instance.sh`), never the real data directory.
- **Three pages.** First OpenAI's documented browser example, pointed at lmgw: an
  `RTCPeerConnection`, `createDataChannel("oai-events")`, the microphone track, the offer POSTed to
  `/v1/realtime/calls` with the key, the answer applied. Then the Agents SDK's WebRTC transport
  (`useInsecureApiKey`), its package cached under `target/`. Both are unbound and check that a
  stock client sees OpenAI's behaviour. Third, a page that speaks the bound contract as the Android
  client does, against an ongoing folder on the dev instance: `chat_folder` with `takeover=ask`,
  `lmgw.output_audio.playout` from `getStats` every second, `lmgw.response.stop` with
  `estimatedPlayoutTimestamp`, parts joined.
- **Chromium flags**: `--use-fake-ui-for-media-stream --use-fake-device-for-media-stream
  --use-file-for-fake-audio-capture=/probe/speech.wav --mute-audio
  --autoplay-policy=no-user-gesture-required --allow-loopback-in-peer-connection`. Chromium leaves
  loopback out of WebRTC unless told, and the dev instance binds 127.0.0.1.
- **Network.** Rootless pasta does not reach the host's loopback by default.
  `--network=pasta:-T,<port>,-U,<port>` forwards the HTTP port (TCP) and the same-numbered UDP port
  from the container's loopback to the host's. The POST reaches lmgw on its loopback, so the
  answer's candidate is `127.0.0.1:<port>`, which inside the container is pasta's forwarder, and
  ICE-lite answers whatever source pasta's packets carry. **Unverified**: that libwebrtc pairs over
  this path. WP6's first step proves it; if it fails, the fallback (host networking for the probe
  container) is the owner's call. *(Verified 2026-10-15, WP6: it pairs; no host networking, §13.3.)*
- The work directory is mounted alone, with `:Z`; the key's copy is removed at the end; Chromium's
  profile stays inside the container, so no profile is left in the host's tmpfs.
- **The page records**: the event sequence on `oai-events`; `getStats()` of the inbound audio
  (`packetsLost`, `jitter`, `jitterBufferDelay / jitterBufferEmittedCount`, `concealedSamples`),
  whose measured playout delay checks §5.3's allowance; the outbound stream's FEC and DTX as
  libwebrtc chose them, and the gap between a frame's capture and its send (the commit window's
  input, §4.4); a `MediaRecorder` capture of the remote track, saved for a level check and for
  listening.
- **The third page also checks** the playout reports against the formula (WP6): lmgw's DEBUG line
  of the allowance from the reports beside the formula's for the same stretch and beside the page's
  own `jitterBufferDelay`. It checks that `estimatedPlayoutTimestamp` is filled for audio once a
  sender report arrived, and that the frame a stop's position names is the one the capture shows
  playing at the tap. And it measures how much RTP arrives before `oai-events` opens (§4.6).
  *(Changed 2026-10-15, WP6 as built, §13.3: Chromium never fills `estimatedPlayoutTimestamp` on an
  audio-only call, so the position checks cannot run; the rest is built and passes. In WP6's
  review the field's check became a known finding tagged Q5: it passes while the field stays
  absent and fails, saying so, the day Chromium fills it.)*
- Optional: loss and jitter with `tc netem` on the container's loopback (it needs
  `--cap-add=NET_ADMIN` and `iproute2` in the image).
- Exit 0 when every check passed, 2 when one failed, as the media probe does.
- The phone over a real cellular link is the owner's acceptance, once the Android client exists.

### 13.3 As built: WP6, the surfaces and the interop rig (2026-10-15)

`scripts/webrtc-interop.sh` with `scripts/webrtc-interop.mjs` (the runner in the container),
`scripts/webrtc-interop-pages.js` (the three pages) and `scripts/webrtc_interop.py` (the dev
instance's setup and the checks only its log can answer); `scripts/mock-openai.py` answers speech
to text and text to speech too; the live calls in `realtime/calls/status.rs` (and the playout the
frame clock hands the registry, `CallInfo::playout`); Settings → Realtime → WebRTC calls
(`lmgw-ui`'s `pages/settings/webrtc.rs`); the Devices card's voice link. Details this design left
open, taken as follows:

- **The network step passed.** Chromium 149 (HeadlessChrome, Playwright 1.61.1's image) in a
  rootless Podman container pairs with a dev instance through `--network=pasta:-T,<port>,-U,<port>`:
  ICE connected about 60 ms after the answer, DTLS and `oai-events` open 130 to 210 ms after the
  POST, the nominated pair's round trip 1 to 2 ms, and every page's call carried audio both ways.
  No host networking.
- **The rig runs its own dev instance**: `scripts/dev-instance.sh` on a scratch data dir under
  `target/webrtc-interop-run/` (removed at the end; the dev log stays), on port 8897
  (`WEBRTC_INTEROP_PORT`), configured through its own API with the owner bearer from its log. The
  work dir `target/webrtc-interop/` is mounted alone; the dev instance's data dir and the owner
  bearer are not in it, and the two owner keys the pages use are removed at the end. Like every
  dev instance it never asks the desktop's Secret Service (`state::wrap_key_store`, device link
  D2's note). *(Changed 2026-10-15, review: the rig used to unset `DBUS_SESSION_BUS_ADDRESS` for
  that, which zbus takes to mean `$XDG_RUNTIME_DIR/bus`, the desktop's own bus. The rig never turns
  the device listener on, so it never made a wallet item.)*
- **No model**: the cascade is `scripts/mock-openai.py` as one generic upstream with three aliases
  (chat streaming 60 words, speech to text answering a fixed transcript, text to speech answering
  the committed `en_complete_short.wav`), so the rig needs no GPU and no download. Interop is about
  the transport; the models' own behaviour is the realtime suites'.
- **The fake microphone** loops the WAV the script is given (default: the committed
  `en_two_sentences_pause.wav`; never committed otherwise). A turn is the track enabled until lmgw
  commits it, then disabled (silence frames: the RTP stream goes on), so the answer plays out unless
  a page enables it again to barge in.
- **The pages.** *stock*: OpenAI's browser example, an answer that plays out (`started`,
  `response.done`, `stopped`), a barge-in (`speech_started`, the cancelled `response.done` with
  `turn_detected`, `cleared`), the HTTP hangup. *agents*: `@openai/agents-realtime` 0.20.0's UMD
  bundle (cached under `target/webrtc-interop/npm`, pinned by `WEBRTC_INTEROP_AGENTS_VERSION`) with
  `OpenAIRealtimeWebRTC({baseUrl, useInsecureApiKey})`, `connect()` resolving at `session.updated`,
  an answer that plays out, `interrupt()` (the SDK's cancel then `clear`) and `close()`. *bound*:
  a WebSocket session holds the ongoing folder's thread with **another owner key** (a holder bound
  with the call's own key does not hold the thread against it, `Held::holds_against`), the call
  with `chat_folder` and `takeover=ask` gets `bind_pending` naming it, `take_over` brings
  `session.created` on that thread and closes the holder with 4000; the POSTed offer's
  `a=max-message-size` is lowered to 1200 bytes (the description Chromium applies keeps its own),
  so lmgw's larger events arrive as parts, which the page joins (4 events from 8 parts); one
  `session.update` goes as three parts; playout reports every second while audio plays; a stop at
  the tap with the remote track muted; a push-to-talk turn (`clear` at the press,
  `input_audio_buffer.commit` and `response.create` at the release, as OpenAI's manual turn: lmgw
  does not answer a manual commit by itself, the microphone's tail on for 100 ms after it); the End
  (`lmgw.response.stop`, then the channel's close). Each page saves its remote track as
  `rec-<page>.webm`.
- **lmgw's log says what only it knows**, at DEBUG, and `webrtc_interop.py check` reads it: the
  allowance line names **the formula beside the reports** for the same stretch ("the formula would
  give N ms"); **the session's start** says how long after ICE, DTLS and the channel's open it came
  and how many frames (and ms of audio) arrived before the open; and **the capture offset**: every
  5 s the least, median and most of how long after its last sample's capture each frame arrived,
  by the client's own sender reports (`audio/capture.rs`). It is meaningful only where the client's
  clock is lmgw's, as on one host, so it is never an input of anything.
- **Exit codes**: 0 every check passed, 2 one failed, 1 the rig itself could not run. A known
  finding is a check like any other, tagged with the question that recorded it and printed with
  what it found either way: `estimatedPlayoutTimestamp stays absent on an audio-only call, as
  found` (Q5) passes while it does, so the rig gates again, and fails the day Chromium fills the
  field. *(Changed 2026-10-15, WP6 review: the field's check failed by design until then, which
  pinned the exit at 2.)*

**Measured** (one host, the debug build; the run after WP6's review fixes, 2026-10-15, whose
`target/webrtc-interop/report.json` this table and §16 quote; the spike's in brackets):

| What | Measured | Against |
|---|---|---|
| NetEQ's delay: `jitterBufferDelay / jitterBufferEmittedCount` over each report's second | 30 ms in all 10 reports; NetEQ's target 20 ms | the formula's allowance, 20 ms (one frame, J ≈ 0 on loopback): 10 ms short |
| the allowance from the playout reports | not measured: no report carried a position (below) | — |
| capture to arrival by Chromium's sender reports | −9 to −1 ms over 4 windows of 5 s, the median of their medians −8.5 | libwebrtc stamps an audio frame's RTP time with the end of its first 10 ms block, so the line reads 10 ms early: a frame leaves about 1.5 ms after its last sample (9 ms at most), the fake device having no input buffer. `webrtc_commit_window_ms` 120 |
| frames that missed a push-to-talk cut | 0 | — |
| a push-to-talk release to its commit applied | 123 ms | the window's full wait (§4.4), plus the read |
| RTP before `oai-events` opens | none: Chromium's channel opens 1.5 to 2.2 ms after DTLS connects | the session starts 0 ms after the open unbound, 19 ms through `bind_pending` (the page takes over at once) |
| a stop's heard (what was sent, there being no position) against the tap | 1820 ms sent, 1845 ms played at the tap | the 300 ms of the clip's lead silence the level tap cannot see are added; 25 ms is the tap's own error |
| late ticks that skipped frames | none (one in the spike: a tick 30 ms late at an allowance of 20 ms skipped a frame) | — |

**Not filled: `estimatedPlayoutTimestamp`** (§16). Chromium never fills the inbound audio's
`estimatedPlayoutTimestamp` on a call: it is absent from every `getStats()` reading, each second of
the bound call's 16 s (45 s in an earlier run), though Chromium received lmgw's sender reports
(`remote-outbound-rtp.reportsSent` 1, 2, 3) and knows the round trip
(`remote-inbound-rtp.roundTripTime` 1 ms). As far as the rig shows, libwebrtc fills
it only through its audio-video synchronization, which an audio-only call does not have. So the
bound page's reports carry `jitter_buffer_ms` alone, the allowance stays on the formula, a stop's
heard is what was sent, and the rig checks the field's absence as a known finding (Q5, above). What
libwebrtc does give is listed in Q5: the receiver's synchronization source (the RTP time of the
frame last played out, and when) and NetEQ's own delay.

- **The status block's live calls** (`webrtc.calls`, oldest first, closing ones included): id,
  group, state (`answered`, `connected`, `open`, `pending`, `running`, `closing`), `held_by`,
  session, the key's name (`null` anonymous), candidate, remote, `connected_at` (RFC 3339, as the
  Devices card's times), `round_trip_ms`, `egress_loss` and `ingress_loss` (fractions),
  `client_jitter_ms`, `receiver_report_age_ms`, `allowance_ms` with `allowance_source` (`report`,
  `formula`, `frame`), `jitter_buffer_ms`, `playout_report_age_ms` and the thread. The frame clock
  hands the registry the playout when it changes (the allowance and the round trip it was taken
  with in whole ms, its source, the jitter buffer, the latest report's arrival), through the driver
  (`Command::Playout`). `WebRtcStatus` is no longer `Eq` (its floats).
  *Changed 2026-10-15 (WP6 review):* `round_trip_ms` is the session's own round trip (`RoundTrip`,
  the least of the last five the receiver reports measured), the one the allowance and the
  barge-in margin take; it used to be str0m's `PeerStats.rtt`, which is its congestion feedback's.
  `egress_loss` and `client_jitter_ms` come from the same receiver reports, aged by
  `receiver_report_age_ms` (a client sends one every few seconds; nothing here guesses its
  cadence); `ingress_loss` is lmgw's own count over the last second, `null` for a second with no
  packet rather than the last value kept. A `closing` call shows none of the link's figures: its
  client may be gone. `playout_report_age_ms` (was `report_age_ms`) counts every playout report,
  so it ages `jitter_buffer_ms` too, a position or not. The floats are rounded at the wire (the
  losses to 0.001, the jitters to 0.1 ms): they reach LLMs through `lmgw__status`.
- **Settings → Realtime → WebRTC calls**, the category's last group: the listener line (each group
  with its state chip, its `udp://` addresses or its error, and its rule; the datagrams dropped),
  a notice while the saved switch differs from what the start read, the firewall hint (the device
  port, TCP and UDP, while the device listener is on, with Fedora's Workstation zone named; the
  plain group's UDP port beside the HTTP port only for a non-loopback `bind_addr`), the live calls
  as a table read again every 3 s while the group shows, and the five rows. *(Changed 2026-10-15,
  WP6 review: only while the window is visible too (`document.visibilityState`; the tray app
  hides the window on close), at once when it shows again, and never while the last read is still
  out. `/api/events` has no status frame, and the figures move with every report, so a push would
  be a timer on the gateway for every connected dashboard, a hidden one included.)* `webrtc_port` is a
  number field where empty is `null`, its placeholder "HTTP port" and its hint saying the rest
  (`Ctl::OptIntRange`). The fixed 256 KiB event bound is in the Connection group's summary, beside
  the WebSocket limits. *Seen:* a call whose client had just vanished showed `closing` with a
  round trip of 9979 ms. That was str0m's `PeerStats.rtt`: the time from a packet's send to the
  transport-wide congestion feedback that names it, the client's own hold of the feedback
  included, kept by the status block as the last value seen. It was never the receiver reports'
  round trip, which the session's margin and allowance use. *(Fixed 2026-10-15, WP6 review: the
  status block shows the session's round trip, and no link figures while `closing`, above.)*
- **The setting texts** gain a call's meaning, in the dashboard and in the settings class and wire
  type: `ping_interval_s` (also a call's silence, connect, open and pending bounds, and when playout
  reports count as stopped), `echo_tail_ms` (the round trip from RTCP on a call), `output_lead_ms`
  (not used on a call), `max_frame_mb` (WebSocket only); `max_message_mb` was WP2's.
- **The Devices card's voice link** says "voice (WebRTC call, since 09:00, from 192.0.2.10:40312)":
  `OpenLink.call` carries the call's id (as `webrtc.calls` lists it) for a voice link that is a
  call, `devices::connect_voice` opening it. The pairing form and the policy dialog are untouched.
- **API docs**: no new route, op or header; the status block's and `OpenLink`'s new fields reach
  the API document through their types.
- **`scripts/webkit-check.py --zoom 1.25`** shows a page as the app window does at its default
  interface scale (the webview's own zoom), for screenshots at a 125 % scale.

## 14. Work packages (build order)

| WP | Scope | Verifies | When |
|---|---|---|---|
| 1 | The seam (§7): the link types, the WebSocket implementation moved behind them, `Close`, the per-transport limits, `Core::on_audio`. No behaviour change | the full suite, unchanged and green | first |
| 2 | The UDP listener through `quinn-udp`: *(changed 2026-10-14)* the plain socket group and its restart release (§3.1), the device group being WP7; `Conn` as the connect info, its local address and the candidate (§3.2); the driver, for both groups, with per-datagram destination and source (§3.3); the registry (§3.4); str0m at its pinned revision and its configuration (§3.5), and the check of its sender-report access (§16: the received reports' NTP and RTP times, the pair its own carry); `/calls` with both bodies and §2.5's refusals; `hangup` with its slot exemption and closing mark; the `call_id` refusal; `chat_folder` with `new` in the shared `RealtimeQuery`, decided and bound under the folder's lock on both transports, with its refusals (§2.2); `session.lmgw.turn_mode`, resolved at the bind and switched by an update (§2.2); `thread::handshake` split into check and bind, with the bind at open, `takeover=ask` with `bind_pending`, `take_over` and `chat_live`'s per-thread release signal, and the open-time `lmgw.session.closed` (§2.8); `error.held_by` on the 409 (§2.8, §8.4); settings `webrtc_enabled` and `webrtc_port`; the `CAPABILITY_TABLE` rows, `refusal_label`, the `DocRoute`s, `/v1/models`; the `aws-lc-rs` unification check | `realtime_calls`, `realtime_calls_net`, `realtime_chat_thread` (folder and turn mode on the WebSocket); the route walk; `openapi_*` | the listener, the driver and the answer in parallel with WP1; the bind and the session after it |
| 3 | Opus in (§4: decode, reorder, FEC, concealment bounded by wall time, discontinuities, RTP stamps, the commit window, flow, the audio kept from the first frame and dropped under `bind_pending`) and out (§5.1, §5.2: the frame clock pulling the paced queue, padding, the encoder, FEC, DTX on `usedtx=1`, RED, bitrate); **the RTCP round trip** into `RoundTrip` and the allowance; the per-frame record and the mapping through lmgw's sender reports; `lmgw.output_audio.playout` into the playout delay, the formula before it and the fallback after it; the window's end, drained and the margin (§5.3); heard on a cut, a barge-in and a position (§5.4); settings `webrtc_reorder_ms`, `webrtc_commit_window_ms`, `webrtc_output_kbps`; `opusic-c` on the system libopus, `opus-devel`, the RPM's `opus` | `realtime_calls_audio` | after WP2 |
| 4 | Events (§6): no audio deltas, `append` refused, `output_audio_buffer.*` with `cleared` as the barge-in verdict, OpenAI's stop form and `clear`, the WebSocket's wording, `lmgw.response.stop` (the owed and held responses, the position into heard), `lmgw.output_audio.starved` at the padding (§5.1), the size bounds: parts both ways on a bound call, emptied fields and `event_too_large` on an unbound one, the send buffer, back-pressure; `lmgw.*` client events refused on unbound calls (§6.4); the large events measured (§6.3) | `realtime_calls_events` | after WP3 |
| 5 | Liveness and closes (§8): the phone's traffic as evidence, the never-opened and pending bounds, the End's order (every event before the channel's close applied first); `lmgw.session.closed` for each kind of a running session, delivered before `Rtc::close`, in the WebSocket's close order; the device-feed contract test of §8.4 (§8.2's last column); `lmgw-client` (§8.3): the requests with `chat_folder`, `new` and `takeover=ask`, `held_by` in `read_refusal`, the part joiner and splitter, the readers (`bind_pending`, `session.closed`, `starved`, `output_audio_buffer.*`, `turn_mode`), the builders (`lmgw.response.stop`, `lmgw.session.take_over`, `lmgw.output_audio.playout`, the `turn_mode` switch), `CloseKind` from the event with `ThreadBound` and the optional code, the voice machine's player facts and mute state | `realtime_calls_close`; `lmgw-client`'s unit tests | after WP2, in parallel with WP3 (disjoint files: liveness, the session's close path and `lmgw-client`, against the audio modules); it uses WP3's round trip, owns none |
| 6 | Surfaces and interop (§9, §13.2): the WebRTC settings group and listener line, the status block, the Devices card's Voice link, the setting texts; `scripts/webrtc-interop.sh` with its three pages; the playout reports validated against the formula fallback and both against `jitterBufferDelay`, the commit window against the measured capture offset, the RTP before the channel's open measured; the README's realtime paragraph; revision notes on client-apps' Later item and on realtime §2.1 and §19 | the interop script; `web_pages`; `openapi_coverage` | the rig's network step as an early spike beside WP2; the rest after WP4 |
| 7 | *(added 2026-10-14)* The device group (§3.1): one socket per device-listener address on the device port, read by WP2's driver; a call's socket by its group (`Conn::via`) and its candidate's address; the live rebind on each change of the device listener, ending device calls only (`shutting_down`, §8.2) under the listener's confirmation; the device group in the status block and the firewall hint (§9) | `realtime_calls_net` (the socket groups and the live rebind, §13.1) | with, or right after, the device link's WP2 (the device listener), after this WP2's driver; whichever of this WP and the device link's WP2 lands second rebases onto `Conn` |

The order: WP1 beside WP2's listener half, then WP2's bind and session, then WP3 beside WP5, then
WP4, then WP6. Live checks run on a dev instance only. The phone's acceptance follows the Android
client's first build.

*Changed 2026-10-14 (the socket groups, C4):* WP2 builds the driver and the plain group. The device
group and its live rebind are WP7, which lands with, or right after, the device link's WP2 (the
device listener); whichever of the two lands second rebases onto `Conn`. The device link's §14
states the same order.

*Changed 2026-10-15 (WP7, as built, §3.9):* the device group, its live rebind and the status
block's groups are built; the firewall hint belongs with the dashboard's WebRTC group, so it moves
to WP6.

*Changed 2026-10-15 (WP4, as built, §6.5):* the events on the data channel, the stop, the output
buffer's events, the size bounds both ways and the measured large events are built; WP6 is next.

*Changed 2026-10-15 (WP6, as built, §13.3):* the surfaces, the firewall hint, the setting texts, the
interop rig with its three pages and its measurements, the README and the revision notes are built.
The rig's network step passed. One assumption failed against real libwebrtc:
`estimatedPlayoutTimestamp` is not filled on an audio-only call, so C10's measured delay and C11's
heard at a position need another source of the playout position (Q5, the owner's call).

## 15. Questions for the owner, in priority order

All four are answered (below): Q1 and Q4 in one answer, Q2 and Q3 as recommended, all on
2026-10-14. Q5 came from WP6's rig (2026-10-15) and is open.

1. **Where a call's close reason comes from** (C17). *Answered 2026-10-14:* a bound call's last
   event, `lmgw.session.closed {kind, by, message}` (§8.2); an unbound call closes as OpenAI's does.
   v2 had recommended the Chat feed, which added nothing to the wire, with v1's closing `error` as
   the alternative.
2. **The UDP port's default** (C5). The same number as the HTTP port: one number to open in the
   firewall and to remember, and a dev instance on another HTTP port never collides. The
   alternative is a fixed default of its own (OpenAI's sample uses 3478, the STUN port), a second
   number everywhere.
   *Recommendation:* the HTTP port's number. *Answered 2026-10-14:* as recommended, refined by the
   device link's approved Q2: calls POSTed to the device listener use sockets on its addresses and
   port, which follow it live (the device group, C4, §3.1).
3. **WebRTC on by default.** It opens a UDP port on the bind address at the next start. On a
   loopback bind it reaches nothing outside; on any bind it answers only checks that carry a live
   call's credentials and arrive at its candidate (§10).
   *Recommendation:* on, with the switch in the WebRTC group. *Answered 2026-10-14:* as
   recommended.
4. **An event larger than the phone's limit** (C14). *Answered 2026-10-14:* on a bound call the
   event travels as `lmgw.event.part` messages and arrives whole, both ways (§6.3); an unbound call
   keeps v2's recommendation, the event with its largest fields emptied plus one `event_too_large`
   per field.

5. **Where a bound client's playout position comes from** (C10, C11; WP6, 2026-10-15, §13.3).
   libwebrtc does not fill `estimatedPlayoutTimestamp` on an audio-only call, so the phone has no
   `playout_ntp_ms` to send: the allowance stays on the formula (10 ms short of NetEQ on one host)
   and a stop's heard is what was sent. Three ways on:
   - (a) **the position in lmgw's own RTP clock**: `RTCRtpReceiver.getSynchronizationSources()`
     names the RTP time of the frame last played out and when (1 to 17 ms old in the rig, advancing
     at 48 kHz). `lmgw.output_audio.playout` and `lmgw.response.stop` would carry `playout_rtp`
     (and its age in ms) beside or instead of `playout_ntp_ms`, which lmgw maps to its frame record
     directly, without its sender reports. Exact to the 10 ms render block, and in the browser
     today; whether the Android SDK exposes the receiver's sources (the native API's
     `RtpReceiverInterface::GetSources`) is the Android design's WP0 to check;
   - (b) **NetEQ's delay as the allowance**: when a report has no position, `jitter_buffer_ms` sets
     the allowance (it is NetEQ's part, without the output device's), and a stop without a position
     cuts heard at what was sent less that allowance, as a bound call's barge-in does;
   - (c) leave C10 and C11 as built: the formula and heard as sent.

   *Recommendation:* (a) for the position, with (b) for a report that has none. Both change the
   contract shared with the Android design (the playout report's and the stop's fields), so they are
   the owner's call; nothing is built for them.

### Answers (the owner, 2026-10-14)

Q1 and Q4, in one answer: lmgw stays compatible with OpenAI on its OpenAI-shaped routes, and Kai
clients, which only work with lmgw, may use custom lmgw APIs where the cleaner approach needs one.

This design reads it as ruling 3. A stock OpenAI client sees OpenAI's behaviour unchanged. lmgw's
extensions appear only on calls bound to a Chat thread or folder, as `lmgw.*` events, or on lmgw's
own routes. And "minimal lmgw-only additions" is no longer a design goal for Kai-facing behaviour.

- **Q1** becomes the in-band event. The phone learns why its call ended from the call itself, and
  no longer from a second connection that may lag or be down. An unbound call is untouched, and the
  feed keeps telling the other devices.
- **Q4** becomes parts. Nothing is emptied, so a bound client never fetches a stripped tool output
  or image through the Chat API, and the limit stays the peer's real one. An unbound call, whose
  client cannot join parts, keeps the emptied fields.
- The same reading carries the rest of v3's contract: the stop as one event (C12), the playout
  reports and heard from a position (C10, C11), the audio kept from the first frame (C13), the End
  (C16), `takeover=ask` (C21), `chat_folder` and the turn mode (C23), and the event table (C24).

Q2 and Q3, later the same day: both approved as recommended. They are rulings 8 and 9.
- **Q2**: the UDP port's default is the HTTP port's number (C5). The device link's approved Q2
  refines it: calls POSTed to the device listener use sockets on its addresses and port, which
  follow each of that listener's changes live, its calls closing with `shutting_down` under the
  listener's confirmation (§3.1). The plain listener's calls keep C5's rule on sockets of their own
  (C4). The device link lists eight changes to this record (device-link §8); each is applied, per
  socket group (appendix "v3 → approved").
- **Q3**: WebRTC is on by default, with the switch in the WebRTC group (§3.1, §9).

## 16. Not verified, and where each is settled

| Claim | Settled in |
|---|---|
| OpenAI's answers to `append` on a call, the `audio.*.format` fields, an offer with no data channel, a `clear` with nothing playing, a `cancel` without a `clear`, an unsolicited `.truncated` (§1.4) | not needed to build; this design's choices stand unless a capture shows OpenAI does otherwise |
| The router's 405s for the unrouted OpenAI paths (§2.7), read from the router, not run | WP2's route tests |
| `str0m-aws-lc-rs` unifies with the tree's `aws-lc-rs` 1.17, and the pinned revision builds with it (§11) | WP2, `cargo tree -d` |
| The playout allowance (one frame + 2 × J) against NetEQ's real delay (§5.3) | WP6, `jitterBufferDelay` in the rig. *Measured 2026-10-15 (§13.3):* on one host NetEQ holds 30 ms (its target 20 ms) where the formula gives 20 ms (J ≈ 0): 10 ms short |
| The commit window's default against a real capture-to-send offset (§4.4) | WP6 in the rig; the owner's acceptance on the phone. *Measured 2026-10-15 (§13.3):* Chromium's fake microphone sends a frame about 1.5 ms after its last sample, 9 ms at most (its sender reports read 1 to 9 ms early, by libwebrtc's 10 ms block); no frame missed a push-to-talk cut at 120 ms. A phone's input buffer and processing are its own acceptance |
| Which events of a bound self-admin session exceed 256 KiB, and so travel as parts (§6.3) | WP4, measured. *Settled 2026-10-15 (§6.5):* the chat turn's tool results only — a text result from about 117 KiB of text (carried twice in its `lmgw.chat.frame`), an image result from a 192 KiB image |
| libwebrtc pairs through pasta's `-T`/`-U` forwarding to the host's loopback (§13.2) | WP6's first step. *Settled 2026-10-15 (§13.3):* it pairs, ICE connected about 60 ms after the answer |
| str0m hands over the NTP and RTP times of the sender reports it receives (the exact push-to-talk cut, §4.4; the test client's positions, §13.1), and lmgw can know the NTP–RTP pair of the reports it sends (the playout mapping, §5.3, §5.4). No copy of str0m's source was at hand for v3 | WP2, in str0m's source; where it lacks the access, the cut stays the window and the mapping reads lmgw's own sent reports |
| libwebrtc's `estimatedPlayoutTimestamp` is filled for inbound audio once a sender report arrived, and is in the sender's NTP clock (§5.3, §5.4) | WP6, the rig's third page; the Android design's WP0 on the phone. *Settled 2026-10-15 (§13.3), against the design:* Chromium 149 never fills it on an audio-only call, with lmgw's sender reports received and the round trip known; Q5 |
| str0m's channel buffered amount counts data until SCTP acknowledges it, so "delivered" before `Rtc::close` means acknowledged (§8.2) | WP5; if it counts only unsent data, the close waits one round trip more |
| How much RTP arrives before `oai-events` opens and the bind completes (§4.6) | WP6 in the rig; the Android design's WP0. *Measured 2026-10-15 (§13.3):* none from Chromium, whose channel opens about 2 ms after DTLS connects; the session starts 0 ms after the open (unbound), 19 ms (bound through `bind_pending`) |

*Settled by WP2 part A (2026-10-15)*, read in str0m's source at `634e76bb` and checked in
`realtime_calls_net::sender_reports`:
- **`aws-lc-rs`**: one in the tree, 1.18.2 (§11).
- **The sender reports lmgw receives**: str0m hands each over as `Event::SenderFeedback`
  (`sender_info.ntp_time`, a `SystemTime`; `sender_info.rtp_time`, extended past the 32-bit wrap,
  in the stream's clock; `received_at`), and the latest also rides every `MediaData` as
  `last_sender_info`. The call passes it on as `CallEvent::SenderReport`; the test finds it equal
  to the report the client sent. So the exact push-to-talk cut of §4.4 is open to WP3 and later,
  and the test client can make positions from lmgw's reports (§13.1).
- **The pair lmgw's own reports carry** has no accessor: str0m makes it at the `now` it sends the
  report at, the NTP time being its process-wide fixed `Instant` → wall-clock mapping of that
  `now`, and the RTP time the last written frame's media time carried forward by `now` minus that
  frame's wallclock. It is readable only as `RawPacket::RtcpTx(Rtcp::SenderReport)` with
  `enable_raw_packets` on, which str0m documents as for tests and troubleshooting because it
  copies every RTP and RTCP packet both ways (for one Opus stream about 100 packets of about 100
  bytes a second). Checked: what the client hears is what `RtcpTx` shows, and while frames are
  written each with its wallclock on the frame clock, NTP − RTP/48 kHz is the same for every
  report. So WP3 can read one sent report (raw packets on, the copies are small) and map every
  frame it wrote through that one line; the playout mapping (§5.3, §5.4) needs no other access.
- **The buffered amount** (WP5's row): str0m's own documentation of `set_sctp_max_buffered_amount`
  says written data counts as buffered "until the remote peer acknowledges it, including after it
  has been sent", so "delivered" before `Rtc::close` means acknowledged.

## Later

- Ephemeral client secrets (`/v1/realtime/client_secrets`) and the raw-SDP-with-token shape they
  belong to.
- The sideband WebSocket (`?call_id=`): a server-side controller attached to a phone's call.
- SIP: incoming calls, `accept`, `reject`, `refer`.
- Typed refusals for the OpenAI paths that only the router answers today (§2.7).
- OpenAI's `/v1/live/sessions` API.
- A reverse proxy's forwarded address as a call's candidate. *Changed after the owner's answers,
  2026-10-14:* TLS on lmgw's listener, which this item named beside it, left this list: for devices
  it is [the device link](2026-10-14-device-link-design.md), which closes the SDP's LAN exposure
  for device calls (C18); a TLS face for stock clients is on the device link's Later list.
- ICE-TCP and TURN, for a phone on a network that blocks UDP without the tunnel.
- Renegotiation and ICE restart (a second offer on a live call).
- An unsolicited `conversation.item.truncated` on a call's server-side cut, if OpenAI sends one.
- **The pre-roll event**: `lmgw.input_audio.preroll`, PCM the phone captured before its peer
  connection existed (the second after a power-button press), placed ahead of the first RTP frame.
  Decided after the Android design's WP0 measurement, on its merits. It is no longer a question of
  wire purity (ruling 3), and the RTP from the first frame is kept already (§4.6), so only what
  precedes the peer connection is in question.
- **The exact push-to-talk cut**: `lmgw.input_audio_buffer.commit {captured_until_ntp_ms}` and
  `lmgw.input_audio_buffer.clear {captured_from_ntp_ms}`, cut through the phone's RTCP sender
  reports instead of `webrtc_commit_window_ms` (§4.4). Decided after the Android design's WP0 and
  the interop rig; it needs str0m's received sender reports (WP2 checks, §16).
- **In-place rebind**: a session bound by folder moves to the folder's new thread, on
  `folder.current` or the user's New conversation, without a new call. Decided after the Android
  design's WP3.
- The bound call's events (§6.4) on a bound WebSocket session, should Kai desktop want them.
- G.711 on calls, together with realtime §19's G.711.
- Opus DRED (libopus's deep redundancy) once libwebrtc negotiates it.
- WebRTC for Kai desktop or the dashboard's voice panel, should Wi-Fi jitter ever call for it.
- The iOS client (ruling 1).

## Appendix: Review dispositions (v1 → v2, 2026-10-14)

| Item | Disposition |
|---|---|
| S1 takeover at the channel's open | **Applied**: C21, §2.8; the race settles at the open (the default takes over, `never` refuses with one `error`). *v3: `never` refuses with `lmgw.session.closed`; `ask` added* |
| S2 the stop's order | **Applied**: C12, §6.2 ("the WebRTC form"), §8.3 (`response_cancel_not_active` expected). *v3: the Android client stops with `lmgw.response.stop`; the form stays for stock clients* |
| S3 close reasons on the feed | **Applied**: C17, §8.2, Q1; the closing `error` is the alternative, with `shutting_down`; the phone does not rebind by itself, and Reconnect hangs up the stale call first (§2.6, §12). *v3: superseded by `lmgw.session.closed`* |
| S4 oversized events | **Applied**: C14, §6.3, Q4; WP4 measures tool outputs and image frames too. *v3: parts on bound calls* |
| S5 the output running dry | **Applied**: C22, §5.1, §8.3 (`lmgw.output_audio.starved` into `voice::Machine::starved`) |
| S6 the push-to-talk commit | **Applied**: C8, §4.4, the setting `webrtc_commit_window_ms`; tested with a capture offset (§13.1); `clear` cuts at its own stamp |
| S7 no `chat_folder=` | **Applied**: C23, §2.2, Later. *v3: superseded, `chat_folder` built* |
| S8 the `session` part as the first update | **Applied**: C1, §2.1, §2.2; v1's pre-answer validation and the model mismatch refusal are gone |
| S9 pre-roll | **Applied**: Later. *v3: RTP kept from the first frame; the pre-roll event stays Later* |
| 1 str0m drops STUN to a non-candidate destination | **Applied**: C4, C6, §3.2, §3.3 (one candidate from the POST's local address; `quinn-udp`'s destination; one socket argued over per-address sockets); `webrtc_addresses` and v1's Q3 removed; wildcard-bind test |
| 2 concealment beyond wall time | **Applied**: §4.2 (bound, discontinuity, re-anchor, INFO line), test |
| 3 handovers | **Applied**: §3.6, §8.1, §12 (`GATHER_CONTINUALLY`), new-source-port test |
| 4 str0m #1065 | **Applied**: C20, §11 (git revision `02fc51b` or newer until a release) |
| 5 DTX | **Applied**: C9, §5.2 (only on `usedtx=1`, RFC 7587's receiver preference) |
| 6 double counting | **Applied**: C10, §5.3 (window end, drained, margin, each term once) |
| 7 the round trip in two WPs | **Applied**: WP3 owns it; WP5 uses it |
| 8 refusal routes | **Applied**: C2, §2.7 (only `?call_id`, documented in `/v1/realtime`'s `DocRoute`; the router's 405s stated, not changed) |
| 9 the questions | **Applied**: Q1 close reasons, Q2 port, Q3 on by default, Q4 oversized events |
| Addendum 1: `voice.ended` on the device's own feed | **Checked**: holds for `taken_over` and `closed`; revocation gives `revoked` and the feed's end, out of reach the thread's removal (§8.4); WP5 pins it in a contract test |
| Addendum 2: the 409 names the holder | **Checked**: only inside a developer sentence; WP2 adds `error.held_by` (§2.8, §8.4) |
| Addendum 3: the cancel's code | **Checked**: `response_cancel_not_active`, `invalid_request_error` (§8.4) |
| Addendum: the underrun event's name | **Fixed**: `lmgw.output_audio.starved {response_id, starved}` (C22, §8.4) |

## Appendix: v2 → v3 (the owner's answer, 2026-10-14)

The contract items below are shared with the Android design, which is revised in parallel with the
same names and shapes.

| Item | Disposition |
|---|---|
| The answer to Q1 and Q4 | **Recorded**: ruling 3 and §15's Answers block. The standing rulings are renumbered 4–7, and ruling 4 (OpenAI's protocol) is bounded by ruling 3 |
| T0 close reason in band (Q1) | **Applied**: C17, §8.2 (`lmgw.session.closed {kind, by, message}`, six kinds with `thread_bound`, the close after delivery in the WebSocket's order), §7.1 (`Close`, `HungUp`), §8.3 (`CloseKind`), §8.4, §13.1. The closing `error` alternative, the feed-only reading and v2's binding-before-close order are gone; `voice.ended` stays for the other devices |
| T0b parts (Q4) | **Applied**: C14, §6.3 (`lmgw.event.part {id, index, count, data}` at the peer's real limit, both ways, joined up to `max_message_mb` inbound), §8.3, §9, WP4, WP5. Unbound calls keep the emptied fields and `event_too_large` |
| T1 `chat_folder` and the turn mode | **Applied**: C23 replaced, §2.2 (decided and bound under the folder's lock, `current`'s refusals, `session.lmgw.turn_mode` resolved and switchable), §2.5, §2.8, §8.3, §8.4, §13.1, WP2 |
| T2 the stop and the barge-in verdict | **Applied**: C11, C12, §5.4, §6.2. `lmgw.response.stop {playout_ntp_ms?}` reaches owed and held responses too, and heard follows the position. The confirmed verdict is `output_audio_buffer.cleared`, unchanged in shape (argued in §6.2). Added: a bound call's barge-in keeps what was sent less the allowance's worth, since the phone mutes on `cleared` |
| T3 `takeover=ask` | **Applied**: C21, §2.8 (`bind_pending`, `take_over`, a holder's release, a changed holder, the bound wait), §2.6, §8.1, §12, §13.1, WP2. `never` at the open is `lmgw.session.closed {kind: "thread_bound"}`; `ask` on the WebSocket is refused |
| T4 audio from the first frame | **Applied**: C13, §4.6, §2.8; the pre-roll event is Later, decided after the Android design's WP0 on its merits |
| T5 playout reports | **Applied**: C10, §5.3 (the measured playout delay, the formula before the first report, the fallback after one silent `ping_interval_s`), §9, WP3, WP6 (the reports validated against the formula) |
| T6 the exact push-to-talk cut | **Recorded**: C8, §4.4, §6.4, Later. `webrtc_commit_window_ms` stays; str0m's sender-report access is to check in WP2 (§16), no copy of its source being at hand |
| T7 in-place rebind | **Later**, decided after the Android design's WP3 |
| T8 the End | **Applied**: C16, §8.1 (every event before the channel's close applied first, then the hangup's end), §6.1, §7.1, §13.1; the HTTP `hangup` stays for stale call ids and stock clients |
| Re-check against "minimal lmgw-only additions" | **Applied**: ruling 4, §2.2 (the `session` part), §5.4 (truncate no longer the bound client's tool), §6.2, §8.3 ("Not needed any more"), §8.4 items 1–3, §12, Later (the pre-roll event argued on its merits) |
| `lmgw-client` | **Applied**: §8.3. Readers for `bind_pending`, `session.closed`, `starved`, `output_audio_buffer.*` and `turn_mode`; the part joiner and splitter; builders for `lmgw.response.stop`, `lmgw.session.take_over`, `lmgw.output_audio.playout` and the `turn_mode` switch; `CloseKind` from the event, with `ThreadBound` and an optional `Other.code` |
| Names chosen here | `chat_folder_not_found` (the handshake's 404 for a folder, after `chat_thread_not_found`; `current` says `not_found`); `error` code `bind_pending` for events sent while pending; `Close::HungUp`; `CloseKind::ThreadBound` |

## Appendix: v3 → approved (the owner's answers, 2026-10-14)

The owner approved Q2 and Q3 as recommended (rulings 8 and 9). The device link's approved Q2
refines Q2, and its §8 lists eight changes to this record:

| device-link §8 | Applied in |
|---|---|
| 1 `webrtc_port: null` is the device listener's port on its addresses while it runs; C5's rule with it off; an explicit port keeps its number | C5, §3.1, §9's `webrtc_port` row; per group (below): the device group takes the device listener's port, and `webrtc_port` is the plain group's alone |
| 2 the socket follows the device listener live; its calls close with `shutting_down` under the listener's confirmation; `webrtc_enabled` stays a restart setting; the release wait covers both sockets | §3.1, §8.2's `shutting_down` row; per group: a change moves the device group and ends device calls only, and the release wait covers the TCP listeners and both groups |
| 3 `Conn { via, peer, local, device }` as the shared connect info; the Later pointer for TLS | §3.2 (`Conn::via` picks the group), "What exists today", Later |
| 4 on the device listener the principal is the certificate's device; §2.3 for the plain listener only | §2.3 |
| 5 device calls exchange their SDP inside mTLS | C18, §10 |
| 6 the phone reaches the device listener; TCP and UDP of the device port open | §12, §3.1 |
| 7 the status block names the rule that bound the socket and its addresses; the firewall hint names the device port | §9, §3.1; both groups shown, the plain group's port in the hint only for a non-loopback `bind_addr` |
| 8 the live rebind and its tests | WP7 (WP2 builds the driver and the plain group), §13.1 |

*Changed 2026-10-14 (the socket groups, C4):* applied literally, items 1 and 2 left three things
open: the loopback pair needs two sockets where C4 had one; a stock call on a non-loopback plain
listener had no socket behind its candidate while the device listener ran; and a change of the
device listener ended stock calls too, against device-link D10 and §4.4. So the UDP sockets are
grouped per listener: one socket per listener address, a plain group that applies on restart and
a device group that follows the device listener live, and one driver task. A call uses the socket
of its group whose address matches its candidate. The device group and its live rebind are WP7,
landing with, or right after, the device link's WP2.
