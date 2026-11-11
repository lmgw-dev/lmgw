//! Every `Admin` write route reaches the gate's writer (§5.8): one request
//! to each `CAPABILITY_TABLE` row that is not a `GET`, and one entry for
//! each, named by the method and the route's template. The rows another
//! writer covers are `audit::at_gate::BY_OTHERS`, the ones that only read
//! `audit::at_gate::READS`, each with why; `READS` is the API document's
//! `x-lmgw-writes: false` among them, exactly.

use std::collections::BTreeSet;

use super::*;
use lmgw_core::audit::at_gate::{BY_OTHERS, OP_ROUTE, READS};
use lmgw_core::principal::Cap;
use lmgw_core::state::AppState;

#[tokio::test]
async fn every_admin_write_route_is_written_at_the_gate() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let mut walked = 0;
    let mut writes = 0;
    for (method, path, cap) in lmgw_core::server::CAPABILITY_TABLE {
        if *cap != Cap::Admin || *method == "GET" || *path == OP_ROUTE {
            continue;
        }
        writes += 1;
        if BY_OTHERS.iter().any(|(m, p, _)| m == method && p == path) {
            continue;
        }
        let reads = READS.iter().any(|(p, _)| p == path);
        // A row that matches nothing: what each route answers to an id that
        // is no row's and an empty body is its business, not this walk's.
        let concrete = path
            .split('/')
            .map(|seg| match seg {
                s if s.starts_with("{*") => "x",
                s if s.starts_with('{') => "999999",
                s => s,
            })
            .collect::<Vec<_>>()
            .join("/");
        let before = count(&state).await;
        let method_ = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let resp = client
            .request(method_, format!("{gw}{concrete}"))
            .bearer_auth(&gw.key)
            .json(&json!({}))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{method} {concrete}: {e}"));
        let status = resp.status().as_u16();
        if reads {
            assert_eq!(count(&state).await, before, "{method} {path} only reads");
            continue;
        }
        assert_eq!(
            count(&state).await,
            before + 1,
            "{method} {path} wrote no entry"
        );
        let e = entries(&gw, 1).await;
        assert_eq!(
            e[0]["action"],
            format!("{method} {path}"),
            "{method} {path}"
        );
        assert_eq!(e[0]["outcome"], status);
        if concrete != *path {
            assert_eq!(e[0]["target"]["path"], concrete);
        }
        walked += 1;
    }
    assert_eq!(
        walked,
        writes - BY_OTHERS.len() - READS.len(),
        "every Admin write row but BY_OTHERS and READS has an entry"
    );
}

#[test]
fn reads_is_the_documents_reading_admin_posts() {
    let doc = lmgw_core::openapi::admin_doc();
    let marked: BTreeSet<&str> = lmgw_core::server::CAPABILITY_TABLE
        .iter()
        .filter(|(m, _, cap)| *cap == Cap::Admin && *m == "POST")
        .filter(|(_, p, _)| {
            doc["paths"][*p]["post"][lmgw_api_types::openapi_ext::WRITES] == json!(false)
        })
        .map(|(_, p, _)| *p)
        .collect();
    let listed: BTreeSet<&str> = READS.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        listed, marked,
        "audit::at_gate::READS must name exactly the Admin POST routes the API document marks \
         x-lmgw-writes: false, each with why"
    );
}
