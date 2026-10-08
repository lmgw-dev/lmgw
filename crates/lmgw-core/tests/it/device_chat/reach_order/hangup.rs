//! A write whose client hangs up before its commit (the branch review's
//! N-3). A delete narrows what devices hear before its commit, and an attach
//! of the self-admin toolset does too; the step after the commit closes a
//! device's session, or the undo gives the thread back. The request that
//! waits for the commit is dropped when its client hangs up, and the write
//! used to be cut in two there. It runs to its end now.

use std::time::Duration;

use serde_json::json;

use super::super::{chat_thread, get};
use super::{bind, ended, file_world, hold_writes, out_of_reach, settle};
use crate::chat_feed::{Feed, Frame};
use crate::realtime_chat_thread::World;

/// `w`'s gateway served once more on a port of its own, the way the app
/// serves it: every request counted in flight until its response is sent
/// or dropped (`Stops::open_requests`), which is how a test sees the server
/// drop a request whose client hung up. The owner's client for it, and the
/// stop, held for as long as the server is to run.
pub(super) async fn counted(
    w: &World,
) -> (String, reqwest::Client, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = lmgw_core::server::build_router(w.state.clone());
    let state = w.state.clone();
    tokio::spawn(async move {
        lmgw_core::server::serve_app(&state, listener, app, async {
            let _ = stopped.await;
        })
        .await
    });
    (base, super::super::bearer(&w.gw.key), stop)
}

/// Whether a `state` frame says thread `tid` has no bound session the
/// device hears of.
fn no_voice_on(f: &Frame, tid: i64) -> bool {
    f.event == "state"
        && f.data["voice"]
            .as_array()
            .is_some_and(|v| v.iter().all(|s| s["thread_id"] != tid))
}

/// The owner deletes a thread a device is bound to, or attaches the
/// self-admin toolset to it, and hangs up while the write waits for its
/// commit — after the device's feed heard the write's first step. The
/// write still commits once the store lets it, and the step after it runs:
/// the device's session closes with the 4004, and the thread is out of the
/// device's reach (deleted, or carrying the toolset).
#[tokio::test]
async fn a_write_whose_client_hangs_up_before_its_commit_runs_to_its_end() {
    let (w, d, _dir) = file_world(json!({})).await;
    let (base, owner, _stop) = counted(&w).await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    for how in ["attach", "delete"] {
        let tid = chat_thread(&w, &d.client, "chatty").await;
        let (mut ws, mut links) = bind(&w, &d, tid).await;
        feed.until(10, |f| {
            f.iter()
                .any(|f| f.event == "voice.bound" && f.data["thread_id"] == tid)
        })
        .await;
        let (path, body) = match how {
            "attach" => (
                format!("/chat/api/threads/{tid}/settings"),
                json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
            ),
            _ => (format!("/chat/api/threads/{tid}/delete"), json!({})),
        };

        let held = hold_writes(&w).await;
        let from = feed.frames.len();
        let req = owner.post(format!("{base}{path}")).json(&body);
        let write = tokio::spawn(async move { req.send().await.map(|r| r.status().as_u16()) });
        // Its first step is done: the device no longer hears the session.
        feed.until(10, |f| f[from..].iter().any(|f| no_voice_on(f, tid)))
            .await;
        // The client hangs up, and the server drops its request.
        write.abort();
        let _ = write.await;
        let request = format!("POST {path}");
        let at = w.state.stops.now();
        let dropped = tokio::time::timeout(Duration::from_secs(10), async {
            while w.state.stops.open_requests(at).contains(&request) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(dropped.is_ok(), "{how}: the server kept the request");

        held.rollback().await.unwrap();
        let end = ended(&mut ws, Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("{how}: the write committed, and the session closes"));
        assert_eq!((end.code, end.reason.clone()), out_of_reach(tid), "{how}");
        let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
        assert_eq!(s, 404, "{how}: out of the device's reach");
        let (s, v) = get(&w, &w.gw.client(), &format!("/chat/api/threads/{tid}")).await;
        match how {
            "attach" => {
                assert_eq!(s, 200, "{how}: {v}");
                assert_eq!(
                    v["thread"]["mcp_tools"][0]["server_label"], "lmgw",
                    "{how}: {v}"
                );
            }
            _ => assert_eq!(s, 404, "{how}: deleted"),
        }
        settle(&mut ws, &mut links).await;
    }
}
