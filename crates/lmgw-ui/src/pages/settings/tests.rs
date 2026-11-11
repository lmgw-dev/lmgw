//! Settings — the page's unit tests.

use super::*;

fn sample() -> Value {
    json!({
        "bind_addr": "127.0.0.1:8899",
        "max_body_mb": 64,
        "global_budget_micro": 12_500_000,
        "hold": {"active": false},
        "router": {"extra_run_args": ["--device", "nvidia.com/gpu=all"], "image": "img"},
        "has_hf_token": true,
    })
}

#[test]
fn every_key_is_named_once_and_every_group_exists() {
    let mut seen = std::collections::BTreeSet::new();
    for d in fields() {
        assert!(seen.insert(d.key), "{} twice", d.key);
        assert!(
            GROUPS.iter().any(|g| g.id == d.group && g.cat == d.cat),
            "{}",
            d.key
        );
        assert!(CAT_SLUGS.contains(&d.cat), "{}", d.key);
    }
    assert_eq!(CATS.len(), CAT_SLUGS.len());
    for g in GROUPS {
        assert!(fields().any(|d| d.group == g.id), "empty group {}", g.id);
    }
}

/// Every place a gateway message sends the owner to
/// (`lmgw_api_types::settings_labels::PATHS`) is a row of this table: its
/// category, its card and — unless the card is the setting — its label.
#[test]
fn every_path_a_message_names_is_a_row() {
    for path in labels::PATHS {
        let parts = path.parts();
        let (cat, ..) = CATS
            .iter()
            .find(|(_, label, _)| *label == parts[0])
            .unwrap_or_else(|| panic!("{path}: no category {}", parts[0]));
        let group = GROUPS
            .iter()
            .find(|g| g.cat == *cat && g.title == parts[1])
            .unwrap_or_else(|| panic!("{path}: no card {} under {}", parts[1], parts[0]));
        let rows: Vec<&Def> = fields().filter(|d| d.group == group.id).collect();
        match parts.get(2) {
            Some(label) => assert!(
                rows.iter().any(|d| d.label == *label),
                "{path}: no row {label}"
            ),
            None => assert!(
                rows.iter().any(|d| d.label == parts[1]),
                "{path}: the card is not its own setting"
            ),
        }
        assert!(parts.len() <= 3, "{path}");
    }
}

/// The Engines category was Backends: its rows are still found by the old
/// name.
#[test]
fn the_engines_rows_are_found_as_backends() {
    for key in ["forge_tokens", "builds_dir", "build_update_check_hours"] {
        let d = fields().find(|d| d.key == key).unwrap();
        let hay = haystack(d);
        assert!(hay.contains("backends") && hay.contains("engines"), "{key}");
    }
}

#[test]
fn the_baseline_is_what_the_boxes_show() {
    let b = baseline(&sample());
    assert_eq!(b["global_budget_micro"], json!(12.5));
    assert_eq!(
        b["router.extra_run_args"],
        json!("--device\nnvidia.com/gpu=all")
    );
    assert_eq!(b["hf_token"], json!(""));
    assert_eq!(b["clear_hf_token"], json!(false));
    assert!(
        !b.contains_key("hold.active"),
        "the hold is never in the draft"
    );
    assert!(!b.contains_key("api_keys"));
}

#[test]
fn a_finished_patch_has_lists_and_micro_units() {
    let p = finish(json!({
        "global_budget_micro": 10.5,
        "router": {"extra_run_args": " --a \n\n--b\n"},
        "retention_days": 30,
    }));
    assert_eq!(
        p,
        json!({
            "global_budget_micro": 10_500_000,
            "router": {"extra_run_args": ["--a", "--b"]},
            "retention_days": 30,
        })
    );
    // empty money clears the budget
    assert_eq!(
        finish(json!({"global_budget_micro": null})),
        json!({"global_budget_micro": 0})
    );
}

#[test]
fn anchors_and_deep_links() {
    assert_eq!(anchor("vram.headroom_mb"), "f-vram-headroom-mb");
    assert_eq!(href("hold.active"), "/settings/gpu#f-hold-active");
    assert_eq!(
        href("audio.voice_dir"),
        "/settings/runtimes#f-audio-voice-dir"
    );
}

/// The machine check's card is no field: a search finds its own rows, each
/// a link to `#machine`, by the words §2.3 names, and never the card; the
/// category shows the card and never those rows (machine-check design
/// §2.3).
#[test]
fn the_machine_check_is_found_by_its_own_rows() {
    let card = fields().find(|d| d.key == "machine").unwrap();
    assert!(!in_search(card) && in_category(card));
    assert_eq!(card.cat, "gpu");
    let links: Vec<&Def> = fields()
        .filter(|d| matches!(d.ctl, Ctl::MachineLink))
        .collect();
    assert!(links.iter().all(|d| in_search(d) && !in_category(d)));
    let labels: Vec<&str> = links.iter().map(|d| d.label).collect();
    assert_eq!(
        labels,
        [
            "Graphics card",
            "Graphics driver",
            "NVIDIA",
            "Container toolkit",
            "Graphics card description (CDI)"
        ]
    );
    for q in [
        "graphics card",
        "driver",
        "nvidia",
        "toolkit",
        "cdi",
        "nvidia-ctk",
    ] {
        assert!(
            links
                .iter()
                .any(|d| q.split(' ').all(|w| haystack(d).contains(w))),
            "{q}"
        );
    }
    // The card comes first in its category, across its columns.
    let gpu: Vec<&Group> = GROUPS.iter().filter(|g| g.cat == "gpu").collect();
    assert_eq!(gpu[0].id, "machine");
    assert!(gpu[0].wide);
    assert_eq!(MACHINE_ANCHOR, "machine");
}

/// A search finds the machine check by one link, not one per row (review
/// L11), named by the rows whose own label has the words.
#[test]
fn a_search_finds_the_machine_check_once() {
    let w = |q: &str| q.split(' ').map(str::to_string).collect::<Vec<_>>();
    for q in ["nvidia", "gpu", "graphics card", "cdi"] {
        let found = machine_hits(&w(q));
        assert!(!found.is_empty(), "{q}");
        let shown: Vec<_> = fields()
            .filter(|d| matches!(d.ctl, Ctl::MachineLink))
            .filter(|d| found.first().is_some_and(|f| f.key == d.key))
            .collect();
        assert_eq!(shown.len(), 1, "{q}");
    }
    assert_eq!(link_labels(&w("nvidia")), ["NVIDIA"]);
    assert_eq!(link_labels(&w("gpu")), ["Graphics card"]);
    assert_eq!(
        link_labels(&w("graphics card")),
        ["Graphics card", "Graphics card description (CDI)"]
    );
    assert_eq!(link_labels(&w("nvidia-ctk")), ["Container toolkit"]);
    assert!(machine_hits(&[]).is_empty());
    assert_eq!(link_words(&["NVIDIA"]), "NVIDIA is in");
    assert_eq!(
        link_words(&["Graphics card", "Graphics card description (CDI)"]),
        "Graphics card and Graphics card description (CDI) are in"
    );
    assert_eq!(link_words(&["A", "B", "C"]), "A, B and C are in");
}

#[test]
fn the_keys_link_is_found_as_a_credential() {
    let d = fields().find(|d| d.key == "api_keys").unwrap();
    let hay = haystack(d);
    for q in ["api key", "token", "credential"] {
        assert!(q.split(' ').all(|w| hay.contains(w)), "{q}");
    }
}

#[test]
fn out_of_range_numbers_are_caught() {
    let load = fields()
        .find(|d| d.key == "vram.load_timeout_seconds")
        .unwrap();
    assert!(range_error(load, "0").is_some());
    assert!(range_error(load, "5").is_none());
    let money = fields().find(|d| d.key == "global_budget_micro").unwrap();
    assert!(range_error(money, "-1").is_some());
    assert!(range_error(money, "").is_none());
    // The update-check interval: 0 (off) to a year, same as the server
    // (`lmgw_core::backends::updates::validate_check_hours`).
    let hours = fields()
        .find(|d| d.key == "build_update_check_hours")
        .unwrap();
    assert!(matches!(hours.ctl, Ctl::IntRange(0, 8760)));
    assert!(range_error(hours, "0").is_none());
    assert!(range_error(hours, "8760").is_none());
    assert!(range_error(hours, "-1").is_some());
    let over = range_error(hours, "8761").unwrap();
    assert!(over.contains("0") && over.contains("8760"), "{over}");
}

#[test]
fn the_four_class_images_are_pickers_for_their_class() {
    let class = |k: &str| match fields().find(|d| d.key == k).unwrap().ctl {
        Ctl::Image(c) => Some(c),
        _ => None,
    };
    assert_eq!(class("router.image"), Some(ImageClass::Chat));
    assert_eq!(class("aux_router.image"), Some(ImageClass::Aux));
    assert_eq!(class("audio.image"), Some(ImageClass::Audio));
    assert_eq!(class("image.image"), Some(ImageClass::Image));
    // still a plain string in the draft and the patch
    let b = baseline(&sample());
    assert_eq!(b["router.image"], json!("img"));
    assert_eq!(b["image.image"], json!(""));
}

#[test]
fn forge_tokens_list_github_first_and_never_the_token() {
    let rows = forge_baseline(&json!({"git.example.com": "<set>", "codeberg.org": "<set>"}));
    let hosts: Vec<(&str, bool)> = rows.iter().map(|r| (r.host.as_str(), r.stored)).collect();
    assert_eq!(
        hosts,
        [
            ("github.com", false),
            ("codeberg.org", true),
            ("git.example.com", true)
        ]
    );
    assert!(rows.iter().all(|r| r.token.is_empty() && !r.added()));
    let b = baseline(&json!({"forge_tokens": {"github.com": "<set>"}}));
    assert!(forge_rows(&b["forge_tokens"])[0].stored);
    assert_eq!(forge_rows(&b["forge_tokens"]).len(), 1);
}

#[test]
fn a_forge_token_save_sets_by_host_and_clears_stored_ones() {
    let mut rows = forge_baseline(&json!({"github.com": "<set>", "git.example.com": "<set>"}));
    rows[0].token = " ghp_new ".into();
    rows[1].clear = true;
    rows.push(ForgeRow {
        key: "new-1".into(),
        host: "Codeberg.org".into(),
        token: "cb".into(),
        ..ForgeRow::default()
    });
    let p = finish(json!({"forge_tokens": rows, "retention_days": 3}));
    assert_eq!(
        p,
        json!({
            "forge_tokens": {"github.com": "ghp_new", "codeberg.org": "cb"},
            "clear_forge_tokens": ["git.example.com"],
            "retention_days": 3,
        })
    );
    // Nothing typed and nothing cleared: nothing sent.
    let untouched = forge_baseline(&json!({"github.com": "<set>"}));
    assert_eq!(finish(json!({"forge_tokens": untouched})), json!({}));
    // Read back failed: what the server holds now.
    let after = forge_after(&rows);
    let hosts: Vec<(&str, bool)> = after.iter().map(|r| (r.host.as_str(), r.stored)).collect();
    assert_eq!(hosts, [("github.com", true), ("codeberg.org", true)]);
}

#[test]
fn an_added_forge_host_has_to_be_a_host_with_a_token() {
    let with = |host: &str, token: &str| {
        let mut rows = forge_baseline(&json!({}));
        rows.push(ForgeRow {
            key: "new-1".into(),
            host: host.into(),
            token: token.into(),
            ..ForgeRow::default()
        });
        forge_error(&rows)
    };
    assert_eq!(with("git.example:8443", "t"), None);
    assert!(with("", "").unwrap().contains("remove the row"));
    assert!(with("https://git.example", "t")
        .unwrap()
        .contains("not a host name"));
    assert!(with("git.example", "").unwrap().contains("empty"));
    assert!(with("github.com", "t").unwrap().contains("twice"));
    assert!(with("git.example", "a b").unwrap().contains("whitespace"));
    assert!(forge_host_ok("GIT.Example.com"));
    assert!(!forge_host_ok("git.example/path"));
    assert!(!forge_host_ok("git.example:0"));
}

#[test]
fn the_builds_dir_is_absolute_or_empty() {
    let d = fields().find(|d| d.key == "builds_dir").unwrap();
    assert!(range_error(d, "").is_none());
    assert!(range_error(d, "/srv/builds").is_none());
    assert!(range_error(d, "builds").is_some());
    assert!(range_error(d, "/").is_some());
    assert_eq!(href("forge_tokens"), "/settings/backends#f-forge-tokens");
}

#[test]
fn what_the_server_always_refuses_is_refused_here_first() {
    let def = |k: &str| fields().find(|d| d.key == k).unwrap();
    // `settings_set_full` refuses a 0 for these two and an empty value
    // for the three names: with several keys dirty the whole save would
    // fail with a message that belongs to none of the fields in view.
    for k in ["docs_ingest_reply_tokens", "docs_embed_batch"] {
        assert!(range_error(def(k), "0").is_some(), "{k}");
        assert!(range_error(def(k), "1").is_none(), "{k}");
    }
    for k in ["bind_addr", "container_prefix", "agent_origin_suffix"] {
        assert!(range_error(def(k), "  ").is_some(), "{k}");
        assert!(range_error(def(k), "x").is_none(), "{k}");
    }
}

/// A form whose baseline is `flat` — what the server holds, as the
/// page's boxes show it.
fn stored(flat: Value) -> FormState {
    FormState::new(flat.as_object().unwrap().clone())
}

/// The keys Save waits for with the form's changes.
fn waits_for(form: FormState) -> Vec<&'static str> {
    blocking(form, &judged_keys(&form.dirty_keys()))
        .into_keys()
        .collect()
}

/// Settings a hand edit broke: the low row's floor above its threshold,
/// both WebSocket limits at 0, the bind address emptied.
fn hand_edited() -> Value {
    json!({
        "bind_addr": "",
        "realtime.barge_in_min_ms": 200,
        "realtime.ping_interval_s": 20,
        "realtime.max_message_mb": 0,
        "realtime.max_frame_mb": 0,
        "realtime.semantic_floor_window_ms": 300,
        "realtime.semantic_vad.high.threshold": 0.5,
        "realtime.semantic_vad.high.floor": 0.25,
        "realtime.semantic_vad.high.max_wait_ms": 2000,
        "realtime.semantic_vad.low.threshold": 0.5,
        "realtime.semantic_vad.low.floor": 0.9,
        "realtime.semantic_vad.low.max_wait_ms": 4000,
    })
}

#[test]
fn a_broken_stored_value_is_shown_but_does_not_block_an_unrelated_save() {
    // WP8 review: the page judged every field, the server only what a
    // save changes — so one broken row blocked every other change.
    let owner = Owner::new();
    owner.with(|| {
        let def = |k: &str| fields().find(|d| d.key == k).unwrap();
        let form = stored(hand_edited());
        assert!(
            waits_for(form).is_empty(),
            "nothing changed, nothing judged"
        );
        form.set_text("realtime.barge_in_min_ms", "250");
        form.set_text("realtime.ping_interval_s", "30");
        assert_eq!(form.dirty_count(), 2);
        assert!(waits_for(form).is_empty(), "{:?}", waits_for(form));
        assert_eq!(form.invalid_count(), 0);
        // Each broken value is said at its field, as a warning.
        for (k, says) in [
            ("realtime.semantic_vad.low.floor", "above the threshold 0.5"),
            ("realtime.max_frame_mb", "cannot both be 0"),
            ("bind_addr", "cannot be empty"),
        ] {
            let (error, warn) = messages(form, def(k), false);
            assert_eq!(error, None, "{k}");
            let warn = warn.unwrap_or_else(|| panic!("{k}: no warning"));
            assert!(
                warn.starts_with("as stored: ") && warn.contains(says),
                "{k}: {warn}"
            );
            assert!(warn.ends_with("other changes still save"), "{k}: {warn}");
        }
        // A sound value says nothing either way.
        let row = def("realtime.semantic_vad.high.floor");
        assert_eq!(messages(form, row, false), (None, None));
    });
}

#[test]
fn editing_a_broken_value_or_what_it_is_judged_with_is_still_refused() {
    let owner = Owner::new();
    owner.with(|| {
        let low_floor = ["realtime.semantic_vad.low.floor"];
        // The broken row itself: its threshold moves, still below its
        // floor — the error is the floor's, where it can be fixed.
        let form = stored(hand_edited());
        form.set_text("realtime.semantic_vad.low.threshold", "0.6");
        assert_eq!(waits_for(form), low_floor);
        let floor = fields().find(|d| d.key == low_floor[0]).unwrap();
        let (error, warn) = messages(form, floor, true);
        assert!(error.unwrap().contains("above the threshold 0.6"));
        assert_eq!(warn, None);
        // Another cell of the table, or the floor window: the server
        // judges the whole table then (`SemanticVadTable::problems`).
        for (k, v) in [
            ("realtime.semantic_vad.high.max_wait_ms", "2500"),
            ("realtime.semantic_floor_window_ms", "250"),
        ] {
            let form = stored(hand_edited());
            form.set_text(k, v);
            assert_eq!(waits_for(form), low_floor, "{k}");
        }
        // Fixed, it saves.
        let form = stored(hand_edited());
        form.set_text("realtime.semantic_vad.low.floor", "0.4");
        assert!(waits_for(form).is_empty(), "{:?}", waits_for(form));
        // The two limits are judged together once either moves.
        let form = stored(hand_edited());
        form.set_text("realtime.max_message_mb", "8");
        assert!(waits_for(form).is_empty());
        let form = stored(json!({"realtime.max_message_mb": 16, "realtime.max_frame_mb": 0}));
        form.set_text("realtime.max_message_mb", "0");
        assert_eq!(waits_for(form), ["realtime.max_frame_mb"]);
        // A field edited to a value the server refuses is refused.
        let form = stored(json!({"bind_addr": "127.0.0.1:8001"}));
        form.set_text("bind_addr", " ");
        assert_eq!(waits_for(form), ["bind_addr"]);
    });
}

#[test]
fn the_smart_turn_table_and_the_limits_are_judged_as_wholes() {
    let judged = judged_keys(&["realtime.semantic_vad.medium.threshold".to_string()]);
    assert!(judged.contains("realtime.semantic_vad.low.floor"));
    assert!(judged.contains("realtime.semantic_floor_window_ms"));
    assert!(!judged.contains("realtime.semantic_vad_engine"));
    assert!(!judged.contains("realtime.max_frame_mb"));
    let judged = judged_keys(&["realtime.max_frame_mb".to_string()]);
    assert_eq!(
        judged.into_iter().collect::<Vec<_>>(),
        ["realtime.max_frame_mb", "realtime.max_message_mb"]
    );
    let judged = judged_keys(&["bind_addr".to_string()]);
    assert_eq!(judged.into_iter().collect::<Vec<_>>(), ["bind_addr"]);
}

/// The settings answer's `pinned` list locks its rows (headless server
/// design §2.2): the row's sentence is found by its key, and nothing else
/// is pinned.
#[test]
fn a_pinned_setting_is_found_by_its_key() {
    let data = json!({
        "pinned": [
            {"setting": "bind_addr", "variable": "LMGW_BIND_ADDR", "source": "variable",
             "why": "Set by this server's container (LMGW_BIND_ADDR)."},
            {"setting": "realtime.webrtc_port", "variable": "LMGW_WEBRTC_PORT",
             "source": "kept", "why": "Kept as stored; LMGW_WEBRTC_PORT is not set on this start."},
        ]
    });
    assert_eq!(
        pinned_why_in(&data, "realtime.webrtc_port").as_deref(),
        Some("Kept as stored; LMGW_WEBRTC_PORT is not set on this start.")
    );
    assert!(pinned_why_in(&data, "bind_addr").is_some());
    assert_eq!(pinned_why_in(&data, "retention_days"), None);
    assert_eq!(pinned_why_in(&json!({}), "bind_addr"), None);
    // Both are rows of this table, of a kind `def_view` locks.
    for key in ["bind_addr", "realtime.webrtc_port"] {
        let d = fields().find(|d| d.key == key).unwrap();
        assert!(matches!(d.ctl, Ctl::Mono | Ctl::OptIntRange(..)), "{key}");
    }
}

/// "restart to apply" is the tray's Restart gateway: shown on the desktop,
/// not in the server image (headless server design §6.2); other units stay.
#[test]
fn the_restart_unit_is_the_desktops() {
    let desktop = json!({"deployment": "desktop"});
    let container = json!({"deployment": "container"});
    assert_eq!(unit_shown(RESTART_UNIT, Some(&desktop)), RESTART_UNIT);
    assert_eq!(unit_shown(RESTART_UNIT, None), RESTART_UNIT);
    assert_eq!(unit_shown(RESTART_UNIT, Some(&container)), "");
    assert_eq!(unit_shown("MiB", Some(&container)), "MiB");
    let bind = fields().find(|d| d.key == "bind_addr").unwrap();
    assert_eq!(bind.unit, RESTART_UNIT);
}

#[test]
fn about_says_what_runs() {
    use lmgw_api_types::Deployment;
    assert_eq!(
        about::runs_as(Deployment::Desktop, false),
        "this computer's desktop app"
    );
    assert_eq!(
        about::runs_as(Deployment::Desktop, true),
        "a dev instance on this computer"
    );
    assert_eq!(
        about::runs_as(Deployment::Container, false),
        "the server image's container"
    );
}

/// A copy's row on Settings → About (headless server design ruling 14):
/// binary units, the time in UTC, the database and the migration step it
/// was made before.
#[test]
fn a_migration_copy_reads_as_its_size_time_and_step() {
    let c = lmgw_api_types::MigrationCopy {
        file: "lmgw-0101-0102.sqlite".into(),
        path: "/data/backups/lmgw-0101-0102.sqlite".into(),
        database: "lmgw.sqlite".into(),
        from: 101,
        to: 102,
        bytes: 3 * 1024 * 1024 / 2,
        made_at: "2026-11-08 10:00:00".into(),
    };
    assert_eq!(
        about::copy_line(&c),
        "1.5 MiB, made 2026-11-08 10:00:00 UTC, lmgw.sqlite before migration 0101 → 0102"
    );
    let unknown = lmgw_api_types::MigrationCopy {
        made_at: String::new(),
        ..c
    };
    assert_eq!(
        about::copy_line(&unknown),
        "1.5 MiB, lmgw.sqlite before migration 0101 → 0102"
    );
}

/// The TLS rows (headless server design §4.7): each key the server image
/// pins is a row the pinned list can lock, and the listener's own settings
/// say they apply at the next start on the desktop.
#[test]
fn the_tls_rows_are_the_settings_the_image_pins() {
    for key in [
        "tls.cert_path",
        "tls.key_path",
        "tls.listen",
        "public_url",
        "realtime.webrtc_tls_port",
    ] {
        let d = fields()
            .find(|d| d.key == key)
            .unwrap_or_else(|| panic!("{key}"));
        assert!(matches!(d.ctl, Ctl::Mono | Ctl::OptIntRange(..)), "{key}");
    }
    for key in ["tls.cert_path", "tls.key_path", "tls.listen"] {
        let d = fields().find(|d| d.key == key).unwrap();
        assert_eq!(d.unit, RESTART_UNIT, "{key}");
        assert_eq!((d.cat, d.group), ("network", "tls"), "{key}");
    }
}
