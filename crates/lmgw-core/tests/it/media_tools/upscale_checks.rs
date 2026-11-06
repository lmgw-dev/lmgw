//! What an upscale checks before it runs (design §14): a running image
//! server whose last answer said no upscaler is asked again (sd-server
//! rescans `upscalers/` on every capabilities read) and runs it; a picture
//! the chosen row could not make is refused with the numbers before
//! anything is admitted; and the shipped upscaler's download is a row of
//! its own beside an `hf_add` of the same repo file, whichever came first.

use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::MockServer;

use super::upscale::{caps_with_upscaler, mount_upscale, put_upscaler, upscales, UPSCALER};
use super::{b64, call, gateway, image_row, job, mount_sd, png, text_of, Z_IMAGE};

/// A PNG header saying `w` x `h`: all an upscale's size check reads.
fn png_header(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 2, 0, 0, 0]);
    b
}

fn data_url(bytes: &[u8]) -> String {
    format!("data:image/png;base64,{}", b64(bytes))
}

#[tokio::test]
async fn a_server_that_started_before_the_file_finds_it_when_asked_again() {
    let sd = MockServer::start().await;
    // Started with 0518b34's own answer: `upscale: false`.
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "a"}),
    )
    .await;
    assert_ne!(r["isError"], json!(true), "{r}");
    let caps = state.runtime().image_capabilities("z").unwrap();
    assert!(!caps.upscale);

    // The file arrives; the running server finds it at its next read.
    put_upscaler(&state);
    sd.reset().await;
    mount_upscale(&sd, caps_with_upscaler()).await;
    let r = call(&gw, "image__upscale", json!({"image": data_url(&png())})).await;
    assert_ne!(r["isError"], json!(true), "{r}");
    assert!(text_of(&r).contains("by upscaling"), "{}", text_of(&r));
    assert_eq!(upscales(&sd).await.len(), 1);
    assert!(state.runtime().image_capabilities("z").unwrap().upscale);
}

#[tokio::test]
async fn a_picture_over_the_rows_limit_is_refused_with_the_numbers_before_admission() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    // The row's limit is known from its first start (8192 x 8192).
    call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "a"}),
    )
    .await;
    put_upscaler(&state);

    let r = call(
        &gw,
        "image__upscale",
        json!({"image": data_url(&png_header(3000, 1000)), "upscaler": UPSCALER}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    let text = text_of(&r);
    assert!(
        text.contains(
            "upscaling this 3000x1000 picture 4x makes 12000x4000, and image/z's image server \
             makes at most 8192x8192"
        ),
        "{text}"
    );
    let r = call(
        &gw,
        "image__upscale",
        json!({"image": data_url(&png_header(1024, 512)), "repeats": 2}),
    )
    .await;
    assert!(
        text_of(&r).contains("picture 4x 2 times makes 16384x8192"),
        "{}",
        text_of(&r)
    );
    assert!(upscales(&sd).await.is_empty(), "nothing was sent");
}

/// `(id, dest_path, status)` of every tracked download.
async fn rows(db: &sqlx::SqlitePool) -> Vec<(i64, String, String)> {
    store::list_hf_models(db)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.id, r.dest_path, r.status))
        .collect()
}

#[tokio::test]
async fn the_upscalers_download_and_an_hf_add_of_the_same_file_are_rows_of_their_own() {
    let u = lmgw_core::image_upscalers::find("realesrgan-x4plus").unwrap();
    let upscaler_dest = u.dest_rel_path();
    let hf_dest = lmgw_core::hf::dest_rel_path(u.repo, u.file).unwrap();
    for upscaler_first in [true, false] {
        let db = store::open_in_memory().await.unwrap();
        let queue = |dest: String| {
            let db = db.clone();
            async move {
                store::upsert_hf_model_at(&db, u.repo, u.file, &dest, "image", "main")
                    .await
                    .unwrap()
            }
        };
        let (up, hf) = if upscaler_first {
            let up = queue(upscaler_dest.clone()).await;
            (up, queue(hf_dest.clone()).await)
        } else {
            let hf = queue(hf_dest.clone()).await;
            (queue(upscaler_dest.clone()).await, hf)
        };
        assert_ne!(up, hf, "upscaler first: {upscaler_first}");
        store::mark_hf_done(&db, up, Some("e1"), 7, "main", None)
            .await
            .unwrap();
        store::mark_hf_done(&db, hf, Some("e1"), 7, "main", None)
            .await
            .unwrap();
        let mut want = vec![
            (up, upscaler_dest.clone(), "done".to_string()),
            (hf, hf_dest.clone(), "done".to_string()),
        ];
        want.sort();
        let mut got = rows(&db).await;
        got.sort();
        assert_eq!(got, want, "upscaler first: {upscaler_first}");

        // Queueing one place again re-queues that row only.
        assert_eq!(queue(upscaler_dest.clone()).await, up);
        let got = rows(&db).await;
        let status = |id: i64| got.iter().find(|r| r.0 == id).unwrap().2.clone();
        assert_eq!((status(up), status(hf)), ("queued".into(), "done".into()));

        // The upscaler listing follows its own row, and a delete of the
        // other leaves it.
        let tracked = store::list_hf_models(&db).await.unwrap();
        let listing = lmgw_core::image_upscalers::listing("", &tracked);
        assert_eq!(listing.shipped[0].download_id, Some(up));
        store::delete_hf_model(&db, hf).await.unwrap();
        let tracked = store::list_hf_models(&db).await.unwrap();
        let listing = lmgw_core::image_upscalers::listing("", &tracked);
        assert_eq!(listing.shipped[0].download_id, Some(up));
        let left: Vec<Value> = tracked.iter().map(|r| json!(r.dest_path)).collect();
        assert_eq!(left, vec![json!(upscaler_dest)]);
    }
}
