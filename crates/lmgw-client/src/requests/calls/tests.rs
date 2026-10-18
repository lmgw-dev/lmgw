use lmgw_api_types::realtime::TurnMode;

use super::*;

const OFFER: &str = "v=0\r\no=- 46117 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                     a=group:BUNDLE 0 1\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                     a=rtpmap:111 opus/48000/2\r\n\
                     m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                     a=sctp-port:5000\r\na=max-message-size:262144\r\n";

fn header<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|h| h.name == name)
        .map(|h| h.value.as_str())
}

/// The parts of a multipart body: `(headers, text)` each.
fn parts(r: &Request) -> Vec<(String, String)> {
    let ct = header(r, "Content-Type").unwrap();
    let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
    let body = r.body.as_deref().unwrap();
    assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");
    body.split(&format!("--{boundary}"))
        .filter(|p| p.starts_with("\r\n"))
        .map(|p| {
            let (head, text) = p[2..].split_once("\r\n\r\n").unwrap();
            (
                head.to_string(),
                text.strip_suffix("\r\n").unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_call_binds_by_folder_or_thread_with_the_websocket_s_query() {
    let r = call(
        CallBind::Folder { id: 12, new: false },
        Takeover::Ask,
        OFFER,
        None,
    );
    assert_eq!(r.method, Method::Post);
    assert_eq!(
        r.url("https://192.0.2.5:8787/"),
        "https://192.0.2.5:8787/v1/realtime/calls?chat_folder=12&takeover=ask"
    );
    assert_eq!(header(&r, "Content-Type"), Some("application/sdp"));
    assert_eq!(r.body.as_deref(), Some(OFFER));
    for (bind, takeover, query) in [
        (
            CallBind::Folder { id: 12, new: true },
            Takeover::Ask,
            "chat_folder=12&new=true&takeover=ask",
        ),
        (
            CallBind::Folder { id: 3, new: false },
            Takeover::default(),
            "chat_folder=3",
        ),
    ] {
        assert_eq!(
            call(bind, takeover, OFFER, None).path,
            format!("/v1/realtime/calls?{query}")
        );
    }
    // A thread binds with the WebSocket builder's own query.
    let query = |r: &Request| {
        let (path, query) = r.path.split_once('?').unwrap();
        (path.to_string(), query.to_string())
    };
    for (bind, takeover, ws) in [
        (
            CallBind::Thread(7),
            Takeover::Always,
            super::super::realtime(7),
        ),
        (
            CallBind::Thread(-2),
            Takeover::Never,
            super::super::realtime_unless_bound(-2),
        ),
    ] {
        let (path, q) = query(&call(bind, takeover, OFFER, None));
        assert_eq!(path, "/v1/realtime/calls");
        assert_eq!(q, query(&ws).1, "{bind:?}");
    }
}

#[test]
fn a_call_with_its_session_is_multipart_with_both_parts() {
    let session = BoundSession {
        turn_mode: Some(TurnMode::PushToTalk),
        half_duplex: Some(true),
        echo_tail_ms: Some(300),
    };
    let r = call(
        CallBind::Folder { id: 12, new: true },
        Takeover::Ask,
        OFFER,
        Some(&session),
    );
    assert_eq!(
        r.path,
        "/v1/realtime/calls?chat_folder=12&new=true&takeover=ask"
    );
    let p = parts(&r);
    assert_eq!(p.len(), 2);
    assert_eq!(
        p[0].0,
        "Content-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp"
    );
    assert_eq!(p[0].1, OFFER, "the offer whole, its own CRLFs kept");
    assert_eq!(
        p[1].0,
        "Content-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json"
    );
    let v: serde_json::Value = serde_json::from_str(&p[1].1).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"type": "realtime", "lmgw": {"turn_mode": "push_to_talk",
                           "half_duplex": true, "echo_tail_ms": 300}})
    );
    // A boundary the parts contain is never used.
    let odd = format!("{OFFER}a=x-note:lmgw-call-part lmgw-call-part-1\r\n");
    let r = call(CallBind::Thread(1), Takeover::Ask, &odd, Some(&session));
    assert_eq!(
        header(&r, "Content-Type"),
        Some("multipart/form-data; boundary=lmgw-call-part-2")
    );
    assert_eq!(parts(&r)[0].1, odd);
}

#[test]
fn an_answer_reads_as_its_call_and_its_sdp() {
    let answer = "v=0\r\no=- 1 2 IN IP4 192.0.2.1\r\na=ice-lite\r\n\
                  m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                  a=max-message-size:262144\r\n";
    let a = read_call(
        201,
        Some("/v1/realtime/calls/rtc_0123456789abcdef01234567"),
        answer,
    )
    .unwrap();
    assert_eq!(a.call_id, "rtc_0123456789abcdef01234567");
    assert_eq!(a.sdp, answer);
    assert_eq!(a.max_message_size(), 262_144);
    // An absolute Location, behind a path prefix, reads the same.
    let a = read_call(
        201,
        Some("https://gw.example/lmgw/v1/realtime/calls/rtc_ab?x=1"),
        answer,
    )
    .unwrap();
    assert_eq!(a.call_id, "rtc_ab");
    for location in [
        None,
        Some(""),
        Some("/v1/realtime/calls/"),
        Some("/v1/realtime/rtc_ab"),
        Some("/v1/realtime/calls/rtc ab"),
        Some("/v1/realtime/calls/../x"),
    ] {
        let e = read_call(201, location, answer).unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.held_by),
            ("unreadable", None),
            "{location:?}"
        );
    }
    let e = read_call(201, Some("/v1/realtime/calls/rtc_ab"), "").unwrap_err();
    assert_eq!(e.code, "unreadable");
}

#[test]
fn a_refused_call_reads_its_code_and_the_holder() {
    let e = read_call(
        409,
        None,
        r#"{"error":{"message":"voice is in use on device 'desktop': this bind asked not to take it over (takeover=never)","type":"invalid_request_error","param":null,"code":"chat_thread_bound","held_by":"device 'desktop'"}}"#,
    )
    .unwrap_err();
    assert_eq!(e.code, super::super::code::CHAT_THREAD_BOUND);
    assert_eq!(e.held_by.as_deref(), Some("device 'desktop'"));
    // The WebSocket upgrade's refusal reads the same.
    assert_eq!(
        super::super::read_refusal(
            409,
            r#"{"error":{"message":"m","type":"invalid_request_error","param":null,"code":"chat_thread_bound","held_by":"the dashboard"}}"#,
        )
        .held_by
        .as_deref(),
        Some("the dashboard")
    );
    for (status, code) in [
        (404, "chat_folder_not_found"),
        (409, "not_ongoing"),
        (409, "folder_no_model"),
        (400, "unsupported_offer"),
        (503, "webrtc_unavailable"),
    ] {
        let body = serde_json::json!({"error": {"message": "m", "type": "invalid_request_error",
                                                "param": null, "code": code}});
        let e = read_call(status, None, &body.to_string()).unwrap_err();
        assert_eq!((e.code.as_str(), e.held_by), (code, None));
    }
    // The flat shape and a body of neither shape carry no holder.
    let e = super::super::read_refusal(409, r#"{"code":"x","message":"m"}"#);
    assert_eq!((e.code.as_str(), e.held_by), ("x", None));
    let e = super::super::read_refusal(502, r#"{"error":"bad gateway"}"#);
    assert_eq!((e.code.as_str(), e.held_by), ("http_502", None));
}

#[test]
fn a_hangup_is_posted_to_the_call_and_a_404_means_it_ended_already() {
    let r = hangup("rtc_0123456789abcdef01234567");
    assert_eq!(r.method, Method::Post);
    assert_eq!(
        r.path,
        "/v1/realtime/calls/rtc_0123456789abcdef01234567/hangup"
    );
    assert_eq!(r.body, None);
    assert_eq!(
        hangup("a/b?c d").path,
        "/v1/realtime/calls/a%2Fb%3Fc%20d/hangup"
    );
    assert_eq!(read_hangup(200, ""), Ok(HungUp::Ended));
    let gone = r#"{"error":{"message":"call rtc_x not found","type":"invalid_request_error","param":null,"code":"call_not_found"}}"#;
    assert_eq!(read_hangup(404, gone), Ok(HungUp::AlreadyEnded));
    let e = read_hangup(
        401,
        r#"{"error":{"message":"no key","type":"authentication_error","param":null,"code":"device_key_unknown"}}"#,
    )
    .unwrap_err();
    assert_eq!(e.code, "device_key_unknown");
}
