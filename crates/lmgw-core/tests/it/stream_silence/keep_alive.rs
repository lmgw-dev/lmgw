//! Where keep-alives still break a silence (`proxy::silence`): while the
//! model may be working out of sight, before the stream's first visible
//! output, inside an Anthropic thinking block and while an Anthropic server
//! tool runs; once the answer shows and no such span is open, a gap of
//! keep-alives only is cut. And the wire's
//! details the clock has to read right: events split across reads, CRLF
//! lines, a failure after half an event, an answer held open after its end.

use super::*;

/// An Anthropic thinking block whose only events are `ping`s for more than
/// three timeouts, after the answer had already shown text: relayed to its
/// end on the Messages relay and the chat relay.
#[tokio::test]
async fn pings_inside_an_anthropic_thinking_block_are_not_silence() {
    let relays = [Relay::Messages, Relay::Chat];
    let texts = uncut_on_every_relay(Says::ThinksHidden, Protocol::Anthropic, &relays).await;
    for (relay, text) in relays.iter().zip(texts) {
        assert!(text.contains("pong"), "{relay:?}: {text}");
        assert!(text.contains("done"), "{relay:?}: {text}");
    }
}

/// An Anthropic text, then a server tool (a web search) whose run sends only
/// `ping`s for more than three timeouts, then its result block and more
/// text: relayed to its end on the Messages relay and the chat relay.
#[tokio::test]
async fn pings_while_an_anthropic_server_tool_runs_are_not_silence() {
    let relays = [Relay::Messages, Relay::Chat];
    let texts = uncut_on_every_relay(Says::SearchesHidden, Protocol::Anthropic, &relays).await;
    for (relay, text) in relays.iter().zip(texts) {
        assert!(text.contains("pong"), "{relay:?}: {text}");
        assert!(text.contains("done"), "{relay:?}: {text}");
    }
}

/// An Anthropic text, then only `ping`s, also once a server tool's result
/// block closed its span: cut at the timeout, the client told in its
/// protocol's terms, the row a `timeout`.
#[tokio::test]
async fn pings_after_anthropic_text_are_silence() {
    for (says, relay) in [Says::Stalls, Says::SearchedThenStalls]
        .into_iter()
        .flat_map(|s| [(s, Relay::Messages), (s, Relay::Chat)])
    {
        let (base, up) = upstream(says).await;
        let (state, gw) = gateway(&base, Protocol::Anthropic, TIMEOUT_MS).await;
        let (text, _) = read_whole(relay.request(&gw)).await;
        assert!(text.contains("pong"), "{says:?} {relay:?}: {text}");
        match relay {
            Relay::Messages => {
                let evs = events(&text);
                let (name, data) = evs.last().unwrap();
                assert_eq!(name.as_deref(), Some("error"), "{text}");
                let data: Value = serde_json::from_str(data).unwrap();
                assert_eq!(data["error"]["message"], SILENT, "{text}");
            }
            _ => assert_eq!(error_chunk(&text)["error"]["message"], SILENT, "{text}"),
        }
        let row = row(&state).await;
        assert_eq!(
            (row.error_kind.as_deref(), row.error_msg.as_deref()),
            (Some("timeout"), Some(SILENT)),
            "{relay:?} {row:?}"
        );
        patience::until("the upstream stream to close", || {
            up.closed.load(Ordering::SeqCst) == 1
        })
        .await;
    }
}

/// Keep-alive comments before the first content, for more than three
/// timeouts: the model may be thinking, so every relay waits for it.
#[tokio::test]
async fn comments_before_the_first_content_are_not_silence() {
    let texts =
        uncut_on_every_relay(Says::ThinksFirst, Protocol::OpenaiCompatible, &Relay::ALL).await;
    for (relay, text) in Relay::ALL.iter().zip(texts) {
        assert!(text.contains("pong"), "{relay:?}: {text}");
    }
}

/// Every event split across two reads, its lines ended in CRLF (every other
/// one split between the CR and the LF of its blank line): each completed
/// event is heard, and the stream relayed whole.
#[tokio::test]
async fn split_crlf_events_are_heard() {
    let texts =
        uncut_on_every_relay(Says::SplitCrlf, Protocol::OpenaiCompatible, &Relay::ALL).await;
    for (relay, text) in Relay::ALL.iter().zip(texts) {
        assert_eq!(text.matches("\"a\"").count(), TRICKLES, "{relay:?}: {text}");
    }
}

/// A CRLF keep-alive comment split across two reads, after the content, is
/// still only a keep-alive: cut at the timeout.
#[tokio::test]
async fn split_crlf_comments_after_content_are_silence() {
    for relay in [Relay::Chat, Relay::Legacy] {
        let (base, _up) = upstream(Says::CrlfStalls).await;
        let (state, gw) = gateway(&base, Protocol::OpenaiCompatible, TIMEOUT_MS).await;
        let (text, _) = read_whole(relay.request(&gw)).await;
        assert!(text.contains("pong"), "{relay:?}: {text}");
        assert_eq!(error_chunk(&text)["error"]["message"], SILENT, "{text}");
        assert_eq!(row(&state).await.error_kind.as_deref(), Some("timeout"));
    }
}

/// Half an event relayed by the legacy passthrough, then a silence or a
/// broken body: the error chunk the client gets is an event of its own and
/// parses.
#[tokio::test]
async fn a_legacy_failure_after_half_an_event_is_an_event_of_its_own() {
    for (says, kind) in [
        (Says::HalfThenHolds, "timeout"),
        (Says::HalfThenBreaks, "transport"),
    ] {
        let (base, _up) = upstream(says).await;
        let (state, gw) = gateway(&base, Protocol::OpenaiCompatible, TIMEOUT_MS).await;
        let (text, _) = read_whole(Relay::Legacy.request(&gw)).await;
        assert!(text.contains(HALF_EVENT), "{says:?}: {text}");
        let error = error_chunk(&text);
        assert_eq!(error["error"]["type"], "api_error", "{says:?}: {text}");
        assert_eq!(row(&state).await.error_kind.as_deref(), Some(kind));
    }
}

/// An answer whose finish came, its connection then held open past the
/// timeout, after the whole finish or after only its first frame (keep-alives
/// going on), or its body broken after that first frame: the client's stream
/// ends whole in its protocol's own way, the row has no error and the
/// upstream's usage where it came (where it did not, the note a body that
/// ended before its usage leaves), and the upstream's connection is closed.
/// On the chat relay as `/v1/chat/completions`, `/v1/messages` and the
/// Responses loop answer it, and on the legacy relay.
#[tokio::test]
async fn an_answer_held_open_or_broken_after_its_finish_is_whole() {
    let chat_relays = [Relay::Chat, Relay::Messages, Relay::Responses];
    let anthropic_relays = [Relay::Chat, Relay::Messages];
    let cases: [(Protocol, Says, &[Relay]); 6] = [
        (
            Protocol::OpenaiCompatible,
            Says::StopsThenBreaks,
            &chat_relays,
        ),
        (
            Protocol::Anthropic,
            Says::StopsThenBreaks,
            &anthropic_relays,
        ),
        (
            Protocol::OpenaiCompatible,
            Says::FinishesThenHolds,
            &Relay::ALL,
        ),
        (
            Protocol::OpenaiCompatible,
            Says::StopsThenHolds,
            &chat_relays,
        ),
        (
            Protocol::Anthropic,
            Says::FinishesThenHolds,
            &anthropic_relays,
        ),
        (Protocol::Anthropic, Says::StopsThenHolds, &anthropic_relays),
    ];
    let runs = cases
        .into_iter()
        .flat_map(|(p, s, relays)| relays.iter().map(move |&r| (p, s, r)))
        .map(|(protocol, says, relay)| async move {
            let what = format!("{protocol:?} {says:?} {relay:?}");
            let (base, up) = upstream(says).await;
            let (state, gw) = gateway(&base, protocol, TIMEOUT_MS).await;
            let (text, _) = read_whole(relay.request(&gw)).await;
            assert!(text.contains("pong"), "{what}: {text}");
            assert!(!text.contains("sent nothing"), "{what}: {text}");
            let evs = events(&text);
            let (name, data) = evs.last().unwrap();
            match relay {
                Relay::Chat | Relay::Legacy => assert_eq!(data, "[DONE]", "{what}: {text}"),
                Relay::Messages => {
                    assert_eq!(name.as_deref(), Some("message_stop"), "{what}: {text}")
                }
                Relay::Responses => {
                    assert_eq!(
                        name.as_deref(),
                        Some("response.completed"),
                        "{what}: {text}"
                    )
                }
            }
            assert_whole_rows(&state, &what).await;
            if matches!(relay, Relay::Chat | Relay::Messages) {
                let rows = rows(&state).await;
                let usage_came = protocol == Protocol::Anthropic || says == Says::FinishesThenHolds;
                match usage_came {
                    true => assert_eq!(rows[0].prompt_tokens, Some(5), "{what} {rows:?}"),
                    // As a stream whose body ended before its usage.
                    false => assert_eq!(
                        rows[0].price_note.as_deref(),
                        Some("stream ended before the upstream reported usage"),
                        "{what} {rows:?}"
                    ),
                }
            }
            patience::until("the upstream stream to close", || {
                up.closed.load(Ordering::SeqCst) == 1
            })
            .await;
        });
    futures::future::join_all(runs).await;
}
