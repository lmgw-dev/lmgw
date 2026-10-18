//! `POST /v1/realtime/calls` and `POST /v1/realtime/calls/{call_id}/hangup`
//! (realtime calls design §9): OpenAI's WebRTC transport, its two bodies,
//! the `201` with the SDP answer and `Location`, §2.5's refusals, the bound
//! call's events (§6.4) and `takeover=ask`.

use std::sync::OnceLock;

use lmgw_api_types::openapi_ext::endpoint_group;

use super::super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::base;
use crate::server::STOP_WITHIN;

/// The hangup's description, its bound read from [`STOP_WITHIN`].
fn hangup_description() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        format!(
            "Ends a call: its session ends as if the client had closed it, a bound call's \
             last event being lmgw.session.closed {{kind: closed, by}} naming who hung up. \
             Answered 200 with an empty body once the call has let go — its Chat thread's \
             binding marked as ending and its concurrency slot back — so a call posted right \
             after it is neither refused (takeover=never) nor left pending (takeover=ask) by \
             the call it ended. A call whose session or bind has not let go {} s after the \
             hangup (the bound a stopping server gives its sessions) is closed by force and \
             answered 200 then; its binding is marked as ending only once its session \
             notices, so a takeover=never call right after such a 200 may still be refused \
             by it for that moment. A call that has let go but still waits for its last \
             events' delivery (closing) is closed at once, without them, and answered 200. \
             Takes no concurrency slot and counts no request, so a key at its limit can \
             still hang up. Allowed for the key that made the call and for the owner; anyone \
             else, and a call that has ended, gets 404 call_not_found.",
            STOP_WITHIN.as_secs()
        )
    })
}

pub(super) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            description: "OpenAI's WebRTC transport for the Realtime API. The client POSTs \
                its SDP offer — audio with Opus, and the data channel oai-events created \
                before createOffer — as application/sdp, or as multipart/form-data with the \
                part sdp and an optional session part: a JSON object, the session as \
                session.update takes it. The answer is 201 Created with the SDP answer \
                (application/sdp: ICE-lite, one UDP host candidate — the local address this \
                POST arrived on, on the WebRTC UDP port: realtime.webrtc_port, by default the \
                HTTP port's number, or on the device listener its own port — Opus only, with \
                RED when the offer carries it) and \
                Location: /v1/realtime/calls/{call_id}, the call's id rtc_ and 24 hex digits. \
                Behind it runs the session GET /v1/realtime runs — the same cascade, binding \
                and policy — its events as JSON text on oai-events and its audio on RTP. \
                lmgw's audio is one Opus stream for the call's whole life, a frame every 20 ms \
                (realtime.webrtc_output_kbps, capped by the offer's maxaveragebitrate; DTX \
                only when the offer's Opus format says usedtx=1; in-band FEC following the \
                client's receiver reports), silence between answers; the client's audio is \
                heard from its first frame after DTLS connects, what it sent before oai-events \
                opened included, and an input_audio_buffer.commit takes the frames captured up \
                to realtime.webrtc_commit_window_ms after it arrived. oai-events carries no \
                audio: no response.output_audio.delta, input_audio_buffer.append is answered \
                invalid_event, and session.lmgw.output_lead_ms is 0. Every call hears OpenAI's \
                output_audio_buffer.started ({response_id}) with an answer's first frame, and \
                .stopped right after the response.done of one that played out, or .cleared \
                after the response.done of one that was cut (a barge-in, a cancel, a clear, a \
                stop); output_audio_buffer.clear cancels the answer whose audio plays and is \
                silent with none. An event over the client's max-message-size (or \
                realtime.max_message_mb below it) arrives on an unbound call with its largest \
                fields emptied, each followed by an error event_too_large naming the field's \
                path. A call bound to a Chat \
                thread also hears lmgw.output_audio.starved ({response_id, starved}) when an \
                answer's audio runs dry and when it comes again, and may report its playout \
                with lmgw.output_audio.playout ({playout_ntp_ms?, jitter_buffer_ms?}), which \
                the answer's end follows; it stops with lmgw.response.stop ({playout_ntp_ms?}: \
                the open response, the one owed and a held response.create cancelled, heard \
                cut at the client's playout position when it sends one and what was sent \
                otherwise, as on an audio-only call, where libwebrtc does not fill \
                estimatedPlayoutTimestamp (calls design Q5); nothing with nothing open); an event \
                over the limit travels both ways as lmgw.event.part ({id, index, count, \
                data}, back to back, id the event's event_id), joined by the receiver, a \
                client's up to realtime.max_message_mb. An unbound call answers these lmgw \
                events invalid_event. The session starts when oai-events opens: \
                session.created, then the \
                session part applied as the session's first update (session.updated, or an \
                error as for any update; on a call bound to a Chat thread it may change only \
                what a bound session's client owns). Credentials, model resolution and the key's scope, \
                budget and concurrency slot are /v1/realtime's — the slot held from this \
                POST until the call ends — the subprotocol key aside; with Require API key \
                off, an anonymous call from another page's Origin is refused with 403 \
                cross_origin_refused. The query is /v1/realtime's: model, chat_thread or \
                chat_folder with new, and takeover. A call bound to a Chat thread or folder \
                only checks at this POST — nothing is bound and no thread is made — and \
                binds when oai-events opens, checking again: takeover left out takes the \
                thread over (its holder closes as taken over); never ends the call with \
                lmgw.session.closed {kind: thread_bound, by} when the thread is held then, \
                and is the 409 chat_thread_bound, the holder in error.held_by, when it is \
                held at this POST; ask, this route's own, makes the call wait with \
                lmgw.session.bind_pending ({held_by}): the client's lmgw.session.take_over \
                ({}) takes the thread, a holder that ends lets the call bind by itself, a \
                third client taking the thread brings another bind_pending, every other \
                event meanwhile is answered with error bind_pending and the audio is \
                discarded, and one realtime.ping_interval_s after the latest bind_pending \
                the call ends with thread_bound (no bound at 0). A take_over that crosses \
                the bind is ignored. A bound call's last event before lmgw ends it is \
                lmgw.session.closed ({kind, by, message}: kind taken_over, revoked, \
                out_of_reach, shutting_down, closed or thread_bound; by the new binder, the \
                holder or who hung up, else null; message the sentence a WebSocket close's \
                reason carries, led by the revocation token for revoked). A refusal when \
                oai-events opens is that event — out_of_reach for a thread or folder out of \
                the caller's reach, revoked for a key revoked since this POST, closed after \
                an error carrying the refusal's code otherwise. by and held_by name a client \
                as the Chat feed's voice.bound.by does. An unbound call sends no lmgw event \
                and closes with nothing in band, as OpenAI's do. A call whose ICE, DTLS or \
                data channel never comes up ends one realtime.ping_interval_s after its \
                answer, binding nothing. Refused before the answer: 400 invalid_value (the \
                query, a parameter rule, or a session part that is no JSON object or is over \
                realtime.max_message_mb, which bounds a session event on either transport), \
                400 bad_request (a multipart body that does not parse), 400 \
                sideband_unsupported (call_id), 400 owned_by_thread, 400 missing_sdp (a \
                multipart body without sdp), 400 invalid_sdp (the offer does not parse; the \
                reason quoted), 400 unsupported_offer (no Opus audio, or no data channel), \
                400 beta_protocol_unsupported, 403 cross_origin_refused, the bind's refusals \
                (403 chat_thread_not_allowed, 404 chat_thread_not_found and \
                chat_folder_not_found, 409 not_ongoing, folder_no_model, chat_thread_admin \
                and chat_thread_bound), the key's 401, 403 and 429, 413 over max_body_mb, 415 \
                unsupported_media_type, and 503 webrtc_unavailable when \
                realtime.webrtc_enabled was off at lmgw's start, or the listener's UDP sockets \
                failed to bind or are not bound (the message names the setting or the \
                error), and, with a Retry-After header, while the device listener's group \
                moves with it or when the POST's connection belongs to an earlier start of it. The device listener's UDP sockets follow its addresses and port: a \
                change of the device listener ends its calls, a bound call's last event \
                lmgw.session.closed {kind: shutting_down}, and leaves the plain listener's \
                running. Not served: the sideband WebSocket, SIP's accept, reject and refer, \
                and ephemeral client secrets.",
            query: Some(|g| g.root_schema_for::<crate::realtime::RealtimeQuery>()),
            request: Req::RawOrMultipart {
                raw: "application/sdp",
                multipart: |g| g.root_schema_for::<lmgw_api_types::realtime::CallRequest>(),
            },
            response: Resp::Created {
                mime: "application/sdp",
                location: "The call's path, /v1/realtime/calls/{call_id}: its last segment \
                    is the call id the hangup takes.",
            },
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            ..base(
                "POST",
                "/v1/realtime/calls",
                "openai",
                "Start a realtime call (WebRTC)",
            )
        },
        DocRoute {
            description: hangup_description(),
            response: Resp::Empty,
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            ..base(
                "POST",
                "/v1/realtime/calls/{call_id}/hangup",
                "openai",
                "Hang up a realtime call",
            )
        },
    ]
}
