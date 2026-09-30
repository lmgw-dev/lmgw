//! The real `model_specs` catalog, fetched from GitHub and parsed here.
//!
//! Every other catalog test feeds the parser a spec this repo wrote, which
//! proves the mapping and nothing about the shape upstream actually ships.
//! audio.cpp adds families weekly and has changed the spec schema twice
//! (`status`, then the typed `options` groups); a parser that silently drops a
//! new key looks perfectly healthy in a mock.
//!
//! Gated on `LMGW_LIVE_AUDIO_CATALOG=1` because it needs the network. Run it
//! after an upstream bump:
//!
//! ```sh
//! LMGW_LIVE_AUDIO_CATALOG=1 cargo test -p lmgw-core --test it audio_catalog_live:: -- --ignored
//! ```
//!
//! Wants `LMGW_AUDIO_CATALOG_ENDPOINT` *unset*, to reach the real catalog —
//! so it takes `common::process_env_lock` too, the same guard
//! `audio_catalog.rs` holds while it points that variable at a mock.

#[tokio::test]
#[ignore = "needs the network; set LMGW_LIVE_AUDIO_CATALOG=1"]
async fn the_published_catalog_still_parses_into_everything_lmgw_shows() {
    let _env = crate::common::process_env_lock().await;
    if std::env::var("LMGW_LIVE_AUDIO_CATALOG").unwrap_or_default() != "1" {
        eprintln!("SKIP: set LMGW_LIVE_AUDIO_CATALOG=1 to fetch the real catalog");
        return;
    }
    let http = reqwest::Client::new();
    let snapshot = lmgw_core::audio::fetch_catalog(&http)
        .await
        .expect("fetch the live catalog");
    let specs = &snapshot.specs;
    assert!(
        specs.len() >= 60,
        "the published catalog has grown well past this ({} parsed)",
        specs.len()
    );

    // A package either names a repo to download from or says why it does not
    // (a licence upstream may not redistribute under). Silence is the bug:
    // the browser then shows a package with nothing to click and no reason.
    for s in specs {
        for p in &s.packages {
            assert!(
                s.package_repo(p).is_some() || s.package_reason(p).is_some(),
                "{}/{} has neither a download source nor a reason",
                s.family,
                p.id
            );
        }
    }
    let undistributed = specs
        .iter()
        .filter(|s| s.packages.iter().any(|p| s.package_reason(p).is_some()))
        .count();

    // The three fields the parser grew for this schema, each asserted on the
    // population rather than on one family: a rename upstream shows up as
    // "none of them have it", which is the failure worth catching.
    let with_status = specs.iter().filter(|s| !s.status.is_empty()).count();
    assert!(
        with_status >= specs.len() / 2,
        "only {with_status}/{} specs carry a status — did the key move?",
        specs.len()
    );
    let with_options = specs.iter().filter(|s| !s.options.is_empty()).count();
    assert!(
        with_options >= specs.len() / 2,
        "only {with_options}/{} specs carry typed options — did the key move?",
        specs.len()
    );
    let expanded_presets = specs
        .iter()
        .flat_map(|s| s.options.session.iter().chain(s.options.load.iter()))
        .filter(|o| o.kind == "enum" && !o.values.is_empty())
        .count();
    assert!(
        expanded_presets > 0,
        "no enum option resolved to a value list — the shared preset table drifted"
    );

    // Tasks: everything a spec tags must map onto a server task name, since
    // that is what the editor prefills and what `--task` accepts.
    let known = [
        "tts",
        "asr",
        "gen",
        "clon",
        "vc",
        "svc",
        "s2s",
        "sep",
        "vad",
        "diar",
        "align",
        "vdes",
        "spk",
        "midi",
        "clone",
        "music",
        "sfx",
        "edit",
        "design",
        "speaker",
        "audio_generation",
    ];
    let mut unknown: Vec<String> = Vec::new();
    for s in specs {
        for t in &s.tasks {
            if !known.contains(&t.as_str()) {
                unknown.push(format!("{}: {t}", s.family));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "spec task tags lmgw cannot map: {unknown:?}"
    );

    eprintln!(
        "{} families, {} with typed options, {} with a status, {} not distributed",
        specs.len(),
        with_options,
        with_status,
        undistributed
    );
}
