//! A transport for tests and for a host that has no lmgw at all: it keeps
//! every request it was sent and answers each from a function of it. A
//! streamed answer is its chunks as given, so a test can split an SSE
//! record across them or end a stream where it likes.

use std::cell::RefCell;
use std::rc::Rc;

use futures::future::LocalBoxFuture;
use futures::stream::StreamExt;

use super::transport::{Request, Response, Transport};

type Answer = dyn Fn(&Request) -> Result<Response, String>;

/// A transport answering from `answer`. Cloning shares the log.
#[derive(Clone)]
pub struct Fake {
    log: Rc<RefCell<Vec<Request>>>,
    answer: Rc<Answer>,
}

impl Fake {
    pub fn new(answer: impl Fn(&Request) -> Result<Response, String> + 'static) -> Self {
        Fake {
            log: Rc::default(),
            answer: Rc::new(answer),
        }
    }

    /// Every request sent so far, oldest first.
    pub fn sent(&self) -> Vec<Request> {
        self.log.borrow().clone()
    }

    /// A `200` whole answer of JSON text.
    pub fn json(status: u16, body: &str) -> Response {
        Response::whole(
            status,
            vec![("Content-Type".into(), "application/json".into())],
            body.as_bytes().to_vec(),
        )
    }

    /// A streamed answer of `chunks`, ended after the last.
    pub fn chunks(status: u16, chunks: Vec<&[u8]>) -> Response {
        let owned: Vec<Result<Vec<u8>, String>> =
            chunks.into_iter().map(|c| Ok(c.to_vec())).collect();
        Response::streamed(
            status,
            vec![("content-type".into(), "text/event-stream".into())],
            futures::stream::iter(owned).boxed_local(),
        )
    }
}

impl Transport for Fake {
    fn send(&self, req: Request) -> LocalBoxFuture<'static, Result<Response, String>> {
        let answer = (self.answer)(&req);
        self.log.borrow_mut().push(req);
        Box::pin(async move { answer })
    }
}

#[cfg(test)]
mod tests {
    use super::super::sse::Decoder;
    use super::super::transport::{Body, Http, Method, Read};
    use super::*;
    use futures::executor::block_on;
    use serde_json::{json, Value};

    #[test]
    fn the_json_helpers_send_plain_data_and_read_the_answer() {
        let fake = Fake::new(|req| {
            Ok(match req.path.as_str() {
                "/chat/api/threads/7" => Fake::json(200, r#"{"id":7,"title":"t"}"#),
                "/chat/api/threads/7/title" => Fake::json(200, r#"{"ok":true}"#),
                _ => Fake::json(404, r#"{"code":"not_found","message":"no such thread"}"#),
            })
        });
        let http = Http::new(fake.clone());
        let got: Value = block_on(http.get("/chat/api/threads/7")).unwrap();
        assert_eq!(got["title"], "t");
        let ack: Value =
            block_on(http.post("/chat/api/threads/7/title", &json!({"title": "n"}))).unwrap();
        assert_eq!(ack["ok"], true);
        let err = block_on(http.get::<Value>("/chat/api/threads/8")).unwrap_err();
        assert!(err.is_not_found(), "{err:?}");
        assert_eq!(err.to_string(), "no such thread");

        let sent = fake.sent();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            (sent[0].method, sent[0].path.as_str(), sent[0].read),
            (Method::Get, "/chat/api/threads/7", Read::Whole)
        );
        assert!(matches!(sent[0].body, Body::Empty));
        assert_eq!(sent[1].method, Method::Post);
        match &sent[1].body {
            Body::Json(text) => assert_eq!(text, r#"{"title":"n"}"#),
            other => panic!("{other:?}"),
        }
        // A fake has no link: the page fetches bytes and shows an object URL.
        assert_eq!(http.link("/chat/api/attachments/1"), None);
        // The browser's fetch is another handle.
        assert_ne!(http, Http::fetch());
    }

    #[test]
    fn a_refusal_in_another_shape_still_says_why() {
        let http = Http::new(Fake::new(|_| {
            Ok(Response::whole(413, vec![], b"body too large".to_vec()))
        }));
        let err = block_on(http.post::<Value, _>("/chat/api/x", &json!({}))).unwrap_err();
        assert_eq!(err.to_string(), "body too large");
        let http = Http::new(Fake::new(|_| Err("connection refused".into())));
        let err = block_on(http.get::<Value>("/chat/api/x")).unwrap_err();
        assert_eq!(err.to_string(), "connection failed: connection refused");
    }

    #[test]
    fn a_stream_arrives_in_chunks_and_reads_whole_too() {
        let http = Http::new(Fake::new(|req| {
            assert_eq!(req.read, Read::Stream);
            assert_eq!(req.header_value("last-event-id"), Some("12"));
            Ok(Fake::chunks(
                200,
                vec![
                    b"id: 13\nevent: hel",
                    b"lo\ndata: {}\n\nid: 14\n",
                    b"event: x\ndata: 1\n\n",
                ],
            ))
        }));
        let resp = block_on(
            http.send(
                Request::get("/chat/api/feed")
                    .header("Last-Event-ID", "12")
                    .read(Read::Stream),
            ),
        )
        .unwrap();
        assert_eq!(resp.header("Content-Type"), Some("text/event-stream"));
        let mut chunks = resp.stream();
        let mut dec = Decoder::default();
        let mut seen = Vec::new();
        block_on(async {
            while let Some(c) = chunks.next().await {
                for r in dec.push(&c.unwrap()) {
                    seen.push((r.id.unwrap_or_default(), r.event));
                }
            }
        });
        assert_eq!(
            seen,
            vec![
                ("13".to_string(), "hello".to_string()),
                ("14".into(), "x".into())
            ]
        );
    }

    #[test]
    fn bytes_answers_keep_their_type() {
        let http = Http::new(Fake::new(|req| {
            assert!(matches!(&req.body, Body::Json(_)));
            Ok(Response::whole(
                200,
                vec![("Content-Type".into(), "audio/wav".into())],
                vec![1, 2, 3],
            ))
        }));
        let (bytes, kind) =
            block_on(http.post_bytes("/chat/api/profiles/speak", &json!({}))).unwrap();
        assert_eq!((bytes, kind.as_str()), (vec![1, 2, 3], "audio/wav"));
    }
}
