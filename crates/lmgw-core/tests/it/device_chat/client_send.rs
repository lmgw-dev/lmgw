//! `lmgw-client`'s send builder against the server: the request it builds
//! starts a turn whose frames its decoder reads, and a refusal reads with
//! `read_refusal`.

use lmgw_client::feed::{SseDecoder, SseItem};
use lmgw_client::requests::{read_refusal, send, SendRequest};

use super::{chat_thread, device_world};
use crate::support::realtime_fakes::Turn;

#[tokio::test]
async fn lmgw_client_s_send_streams_a_turn_and_reads_a_refusal() {
    let (w, d) = device_world().await;
    w.chat.push(Turn::text(&["Hello."]));
    let tid = chat_thread(&w, &d.client, "chatty").await;

    let go = |body: SendRequest| {
        let req = send(tid, &body);
        let client = d.client.clone();
        let url = req.url(&w.gw.to_string());
        async move {
            let mut r = client.post(url).body(req.body.unwrap());
            for h in &req.headers {
                r = r.header(&h.name, &h.value);
            }
            let resp = r.send().await.unwrap();
            let status = resp.status().as_u16();
            (status, resp.text().await.unwrap())
        }
    };

    let (s, text) = go(SendRequest {
        content: "hi".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(s, 200, "{text}");
    let events: Vec<String> = SseDecoder::new()
        .push(text.as_bytes())
        .into_iter()
        .filter_map(|i| match i {
            SseItem::Frame(f) => Some(f.event),
            _ => None,
        })
        .collect();
    assert!(events.contains(&"delta".to_string()), "{events:?}");
    assert_eq!(
        events.last().map(String::as_str),
        Some("done"),
        "{events:?}"
    );

    let (s, body) = go(SendRequest::default()).await;
    assert_eq!(s, 400);
    assert_eq!(read_refusal(s, &body).code, "empty_message");
}
