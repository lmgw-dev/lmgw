use lmgw_api_types::{
    AgentBatchShape, AgentField, AgentImportReport, AgentProvenance, AgentRunSummary, AgentRuntime,
};
use serde_json::{json, Value};

use super::*;

/// Config is deployment state: it only rides along when asked for, and the
/// flag is the one the API documents.
#[test]
fn the_export_link_only_carries_config_when_asked() {
    assert_eq!(
        export_href("mail-labeler", false),
        "/api/agents/mail-labeler/export"
    );
    assert_eq!(
        export_href("mail-labeler", true),
        "/api/agents/mail-labeler/export?include_config=1"
    );
}

#[test]
fn a_copy_is_named_after_its_original() {
    assert_eq!(copy_id("docs-librarian"), "docs-librarian-copy");
}

/// A run that has not listed yet has no total; that is said, not guessed
/// at with a zero or a made-up denominator.
#[test]
fn progress_never_invents_a_total() {
    let mut r = AgentRunSummary::default();
    assert_eq!(progress_text(&r), "—");
    r.done = 7;
    assert_eq!(progress_text(&r), "7");
    r.total = Some(50);
    assert_eq!(progress_text(&r), "7 / 50");
}

/// "Still going" is not a duration, and neither is a made-up zero.
#[test]
fn a_duration_is_only_printed_once_there_is_one() {
    assert_eq!(duration_text(None), "—");
    assert_eq!(duration_text(Some(420)), "420 ms");
    assert_eq!(duration_text(Some(1500)), "1.5 s");
    assert_eq!(duration_text(Some(64_000)), "1 m 04 s");
}

/// The rendered row's key carries everything it *shows*, so a row that has
/// just been classified re-renders instead of sitting there blank: a keyed
/// `<For>` builds a view only for a key it has not seen, and a batch run
/// publishes every row before it has answered any of them.
#[test]
fn a_rows_view_key_changes_when_its_answer_lands() {
    let shape = AgentBatchShape {
        columns: vec!["subject".into()],
        editable: vec![lmgw_api_types::AgentReviewField {
            field: "category".into(),
            options: vec!["Work".into(), "Other".into()],
        }],
        ..Default::default()
    };
    let listed = review_rows(
        &[json!({ "id": "m1", "columns": { "subject": "Invoice" } })],
        &shape,
    );
    let answered = review_rows(&[row("m1", "Invoice", "Work")], &shape);
    assert_eq!(listed[0].id, answered[0].id, "the same row");
    assert_ne!(
        listed[0].view_key(),
        answered[0].view_key(),
        "…but not the same view"
    );
    assert!(!listed[0].answered && answered[0].answered);
}

/// Apply posts what the reviewer decided, not what the row happened to
/// carry: the overrides live outside the row list precisely because the
/// list is replaced on every progress frame.
#[test]
fn apply_takes_its_values_from_the_reviewers_overrides() {
    let shape = AgentBatchShape {
        columns: vec!["subject".into()],
        editable: vec![lmgw_api_types::AgentReviewField {
            field: "category".into(),
            options: vec!["Work".into(), "Finance".into()],
        }],
        ..Default::default()
    };
    let rows = review_rows(&[row("m1", "Invoice", "Other")], &shape);
    let mut overrides = Overrides::new();
    // Untouched: the server's own answer travels back.
    assert_eq!(
        rows[0].for_apply(&overrides)["output"]["category"],
        json!("Other")
    );
    overrides.insert(("m1".into(), "category".into()), "Finance".into());
    let out = rows[0].for_apply(&overrides);
    assert_eq!(out["output"]["category"], json!("Finance"));
    assert_eq!(out["output"]["confidence"], json!(0.9), "{out}");
    // An override for another row is not this row's.
    let mut other = Overrides::new();
    other.insert(("m2".into(), "category".into()), "Work".into());
    assert_eq!(
        rows[0].for_apply(&other)["output"]["category"],
        json!("Other")
    );
}

fn row(id: &str, subject: &str, category: &str) -> Value {
    json!({
        "id": id,
        "columns": { "subject": subject, "date": "18 Sep" },
        "output": { "category": category, "confidence": 0.9 },
        "prompt": "Subject: {subject}",
        "attention": category == "Other",
    })
}

/// The columns come out in the manifest's order, not the alphabet's — the
/// server sends that order because a JSON object arrives sorted here.
#[test]
fn the_review_columns_keep_the_authors_order() {
    let columns = vec!["subject".to_string(), "date".to_string()];
    assert_eq!(
        row_cells(&row("m1", "Invoice", "Finance"), &columns),
        vec![
            ("subject".to_string(), "Invoice".to_string()),
            ("date".to_string(), "18 Sep".to_string()),
        ]
    );
    // A column the row does not carry is empty, not missing: the table has
    // to keep its shape.
    assert_eq!(
        row_cells(&json!({ "columns": {} }), &columns),
        vec![
            ("subject".to_string(), String::new()),
            ("date".to_string(), String::new()),
        ]
    );
}

/// Apply posts the server's own row back, with only the override patched
/// in: an output field this build does not render must still reach the
/// apply step.
#[test]
fn an_override_is_patched_into_the_row_the_server_sent() {
    let out = patched_row(
        &row("m1", "Invoice", "Other"),
        &[("category".to_string(), "Finance".to_string())],
    );
    assert_eq!(out["output"]["category"], json!("Finance"));
    assert_eq!(out["output"]["confidence"], json!(0.9), "{out}");
    assert_eq!(out["id"], json!("m1"));
    assert_eq!(out["columns"]["date"], json!("18 Sep"));

    // No editable fields: the row travels back byte for byte.
    let raw = row("m2", "Hi", "Work");
    assert_eq!(patched_row(&raw, &[]), raw);

    // A list-only row has no output object yet; the override makes one
    // rather than being dropped.
    let out = patched_row(
        &json!({ "id": "m3" }),
        &[("category".to_string(), "Work".to_string())],
    );
    assert_eq!(out["output"], json!({ "category": "Work" }));
}

/// The progress line names the executor's own stage and never invents a
/// denominator.
#[test]
fn the_run_line_says_the_stage_and_never_guesses_a_total() {
    let mut r = AgentRunSummary {
        phase: "classify".into(),
        ..Default::default()
    };
    assert_eq!(run_line(&r), "classify —");
    r.stage = "listing".into();
    assert_eq!(run_line(&r), "listing —");
    r.stage = "classifying".into();
    r.done = 23;
    r.total = Some(50);
    assert_eq!(run_line(&r), "classifying 23 / 50");
}

/// An attention row says *why* it is one: the call's error if there was
/// one, else what the model actually replied.
#[test]
fn an_attention_row_explains_itself() {
    assert_eq!(
        attention_note(Some("503 model is not loaded"), Some("ignored")),
        Some("call failed: 503 model is not loaded".to_string())
    );
    assert_eq!(
        attention_note(None, Some("Billing")),
        Some("raw: Billing".to_string())
    );
    assert_eq!(attention_note(None, None), None);
}

/// Cancel applies to a run that is going, and to nothing else.
#[test]
fn only_a_live_run_can_be_cancelled() {
    for (status, want) in [
        ("queued", true),
        ("running", true),
        ("done", false),
        ("failed", false),
        ("canceled", false),
    ] {
        let r = AgentRunSummary {
            status: status.into(),
            ..Default::default()
        };
        assert_eq!(is_live(&r), want, "{status}");
    }
}

/// "Classified" is a claim, so it waits until every row in the table has an
/// answer — a list-only run and a run mid-flight both have rows that were
/// never sent to a model.
#[test]
fn the_settled_table_only_claims_classified_when_it_is() {
    assert_eq!(settled_heading(true), "Classified");
    assert_eq!(settled_heading(false), "Rows");
}

/// Mail-labeler's review (ux:U-1): the RFC date was 38 characters and
/// took its whole width at every size, so the subject — what a reviewer
/// triages by — clipped at ~20 characters on a 1024 window. The date is
/// now short, the sender clips, and the subject takes the rest and wraps.
#[test]
fn the_subject_is_read_whole_and_the_date_is_short() {
    let shape = AgentBatchShape {
        columns: vec!["date".into(), "from".into(), "subject".into(), "n".into()],
        ..Default::default()
    };
    let rows = review_rows(
        &[
            json!({ "id": "a", "columns": {
                "date": "Thu, 17 Sep 2026 11:03:57 -0500 (CDT)",
                "from": "Team Wiki <wiki-notifications@example.com>",
                "subject": "Quarterly planning notes and one other page you follow were updated this week!",
                "n": "7",
            } }),
            json!({ "id": "b", "columns": {
                "date": "Fri, 18 Sep 2026 15:19:23 -0700",
                "from": "NVIDIA <news@nvidia.com>",
                "subject": "NEW: CUDA 13.4 is Available Now.",
                "n": "12",
            } }),
        ],
        &shape,
    );
    let fits = column_widths(&rows, 4);
    assert_eq!(
        fits,
        vec![
            ColumnFit::Date,
            ColumnFit::Clip(CLIP_WIDTH),
            ColumnFit::Wrap,
            ColumnFit::Fit(2)
        ]
    );
    assert_eq!(lead_column(&fits), 2, "the subject opens the details");
    // Short columns only: nothing wraps, the first leads.
    let fits = column_widths(&rows, 1);
    assert_eq!(fits, vec![ColumnFit::Date]);
    assert_eq!(lead_column(&fits), 0);
    assert!(column_widths(&[], 2)
        .iter()
        .all(|f| *f == ColumnFit::Fit(0)));
}

#[test]
fn mail_dates_parse_to_the_same_instant_in_any_zone() {
    // 2026-09-17 16:08:29 UTC, three ways.
    let utc = mail_date("Thu, 17 Sep 2026 16:08:29 +0000").unwrap();
    assert_eq!(utc, 1_789_661_309);
    assert_eq!(
        mail_date("Thu, 17 Sep 2026 18:08:29 +0200 (CEST)"),
        Some(utc)
    );
    assert_eq!(mail_date("17 Sep 2026 09:08:29 PDT"), Some(utc));
    assert_eq!(mail_date("Thu, 17 Sep 2026 16:08 GMT"), Some(utc - 29));
    assert_eq!(mail_date("1 Jan 1970 00:00:00 +0000"), Some(0));
    // Not dates: a subject, a bare day, an ISO stamp, a bad zone.
    for s in [
        "Willkommen zur 24. Kielux!",
        "Fri, 18 Sep",
        "2026-09-17 16:08:29",
        "17 Sep 2026 16:08 XYZ",
        "",
    ] {
        assert_eq!(mail_date(s), None, "{s}");
    }
}

/// A tab is a path; the old `?tab=` links and a spelled-out `/run` are
/// moved onto it, and a path that already is one stays put.
#[test]
fn old_tab_links_land_on_the_tab_path() {
    // Not `/app`: the gateway answers that path itself (origins §4.2).
    assert_eq!(tab_href("folder-chat", "app"), "/agents/folder-chat/ui");
    assert_eq!(tab_href("folder-chat", "ui"), "/agents/folder-chat/ui");
    assert_eq!(tab_href("folder-chat", "run"), "/agents/folder-chat");
    assert_eq!(tab_href("folder-chat", "bogus"), "/agents/folder-chat");
    assert!(TABS.iter().all(|t| *t != "app" && *t != "mcp"));
    assert_eq!(
        canonical_href("x", None, Some("app")).as_deref(),
        Some("/agents/x/ui")
    );
    assert_eq!(
        canonical_href("x", Some("runs"), Some("definition")).as_deref(),
        Some("/agents/x/definition")
    );
    assert_eq!(
        canonical_href("x", Some("run"), None).as_deref(),
        Some("/agents/x")
    );
    assert_eq!(canonical_href("x", Some("runs"), None), None);
    assert_eq!(canonical_href("x", None, None), None);
}

/// The Runtime block folds to the image and the bounds a run is held to,
/// each spelled the way the table spells it; a row with only a package
/// says where it came from.
#[test]
fn the_runtime_folds_to_the_image_and_its_bounds() {
    let r = AgentRuntime {
        image: "localhost/folder-chat:0.2.0".into(),
        memory_mb: 2048,
        cpus: 2.0,
        deadline_seconds: 0,
        read_only: true,
        podman: true,
        ..Default::default()
    };
    assert_eq!(
        runtime_summary(Some(&r), None),
        "localhost/folder-chat:0.2.0 · 2048 MB · 2 CPU · no deadline · read-only"
    );
    let p = AgentProvenance {
        image: "ghcr.io/x/agent:1".into(),
        ..Default::default()
    };
    assert_eq!(
        runtime_summary(None, Some(&p)),
        "installed from ghcr.io/x/agent:1"
    );
}

/// No model call, no cost line; an unpriced one says so rather than
/// printing a zero.
#[test]
fn the_cost_line_only_speaks_for_a_run_that_called_a_model() {
    assert_eq!(cost_text(&json!({ "model_calls": 0 }), "USD"), None);
    let line = cost_text(
        &json!({ "model_calls": 3, "tool_calls": 1, "usage": { "prompt_tokens": 10, "completion_tokens": 2 } }),
        "USD",
    )
    .unwrap();
    assert!(
        line.starts_with("3 model call(s), 1 tool call(s), 10 in / 2 out tokens"),
        "{line}"
    );
    assert!(line.ends_with("unpriced"), "{line}");
}

/// The inline report says what happened and then every warning verbatim —
/// errors block a save, warnings never do (§5).
#[test]
fn the_report_leads_with_the_outcome_then_the_warnings() {
    let r = AgentImportReport {
        ok: true,
        id: "mail-labeler".into(),
        warnings: vec!["no MCP server with label 'gws' is registered".into()],
        replaced: true,
        ..Default::default()
    };
    assert_eq!(
        report_lines(&r),
        vec![
            "saved 'mail-labeler'.".to_string(),
            "no MCP server with label 'gws' is registered".to_string(),
        ]
    );

    let checked = AgentImportReport {
        id: "x".into(),
        validate_only: true,
        ..Default::default()
    };
    assert_eq!(
        report_lines(&checked),
        vec!["'x' checks out — nothing written."]
    );

    // A slot the file could not carry is the one thing left to do, and it
    // is said after the warnings rather than folded into them.
    let unbound = AgentImportReport {
        id: "notes-desk".into(),
        config_unbound: vec!["notes".into(), "archive".into()],
        ..Default::default()
    };
    let lines = report_lines(&unbound);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].starts_with("slots to bind after import: notes, archive"),
        "{lines:?}"
    );
}

/// The start summary prints the host path and the path the container will
/// actually see, with the mode the manifest declared — and `ro` when it
/// declared none, never a blank.
#[test]
fn a_mount_line_names_both_paths_and_the_mode() {
    let mut notes = AgentField {
        name: "notes".into(),
        ty: "string".into(),
        format: "directory".into(),
        access: "rw".into(),
        ..Default::default()
    };
    assert_eq!(
        mount_line(&notes, "/home/alice/Notes"),
        "/home/alice/Notes → /lmgw/mounts/notes (rw, directory)"
    );
    notes.access = String::new();
    notes.format = "file".into();
    assert_eq!(
        mount_line(&notes, "/etc/hostname"),
        "/etc/hostname → /lmgw/mounts/notes (ro, file)"
    );
}

/// Only the two mount formats, in the author's order — the rest of the
/// form is not this list's business.
#[test]
fn the_mount_list_is_the_two_formats_in_order() {
    let f = |name: &str, format: &str| AgentField {
        name: name.into(),
        ty: "string".into(),
        format: format.into(),
        ..Default::default()
    };
    let fields = vec![
        f("notes", "directory"),
        f("token", "secret"),
        f("digest", "file"),
    ];
    let names: Vec<String> = mount_fields(&fields)
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert_eq!(names, vec!["notes".to_string(), "digest".to_string()]);
    assert!(mount_fields(&[]).is_empty());
}

/// One line, naming the slots — and nothing at all when there are none, so
/// an agent without mounts says nothing about them.
#[test]
fn the_unbound_line_names_the_slots_or_stays_away() {
    assert_eq!(unbound_line(&[]), None);
    let line = unbound_line(&["notes".to_string(), "archive".to_string()]).unwrap();
    assert!(
        line.starts_with("slots to bind after import: notes, archive"),
        "{line}"
    );
    assert_eq!(line.lines().count(), 1, "{line}");
}

/// The App tab reads the mounts out of the document, so a build whose DTO
/// has no `mounts` field still draws the list — and a slot with no host
/// path is not a mount and is not listed.
#[test]
fn the_service_mounts_come_out_of_the_document() {
    let doc = json!({
        "id": "notes-desk",
        "service": {
            "port": 8080,
            "mounts": [
                {
                    "field": "notes",
                    "host": "/home/alice/Notes",
                    "inside": "/lmgw/mounts/notes",
                    "kind": "directory",
                    "access": "rw",
                },
                { "field": "archive", "inside": "/lmgw/mounts/archive", "kind": "directory" },
                { "field": "empty", "host": "", "inside": "/lmgw/mounts/empty" },
            ],
        },
    });
    let mounts = service_mounts(&doc);
    assert_eq!(mounts.len(), 1, "{mounts:?}");
    assert_eq!(mounts[0].field, "notes");
    assert_eq!(mounts[0].host.as_deref(), Some("/home/alice/Notes"));
    assert_eq!(mounts[0].inside, "/lmgw/mounts/notes");
    assert_eq!(mounts[0].access, "rw");

    // Nothing to read is not an error: no service, no key, or a null.
    assert!(service_mounts(&json!({ "id": "x" })).is_empty());
    assert!(service_mounts(&json!({ "service": { "port": 8080 } })).is_empty());
    assert!(service_mounts(&json!({ "service": { "mounts": Value::Null } })).is_empty());
}

/// An origin is a URL and a resolver is asked for a name: the App tab
/// prints the second, including the `/etc/hosts` line that fixes it.
#[test]
fn the_hosts_line_names_the_host_and_not_the_origin() {
    assert_eq!(
        origin_host("http://board.localhost:8001/"),
        "board.localhost"
    );
    assert_eq!(
        origin_host("http://board.lmgw.lan:8001/?v=3"),
        "board.lmgw.lan"
    );
    assert_eq!(origin_host("http://board.localhost/"), "board.localhost");
    assert_eq!(origin_host(""), "");
}

/// The LAN line is about a gateway someone else can reach while its agent
/// origins still say `*.localhost` — a loopback bind is fine, and a suffix
/// the owner has already moved is their DNS's business.
#[test]
fn the_lan_line_is_for_a_gateway_others_can_reach() {
    assert_eq!(lan_origin_host("127.0.0.1:8001", "localhost"), None);
    assert_eq!(lan_origin_host("[::1]:8001", "localhost"), None);
    assert_eq!(lan_origin_host("localhost:8001", "localhost"), None);
    assert_eq!(
        lan_origin_host("0.0.0.0:8001", "localhost"),
        Some("0.0.0.0".to_string())
    );
    assert_eq!(
        lan_origin_host("192.168.1.10:8001", "localhost"),
        Some("192.168.1.10".to_string())
    );
    assert_eq!(lan_origin_host("192.168.1.10:8001", "lmgw.lan"), None);
}

/// The Run tab's own refusal, before the click: a required field the form
/// has nothing in would start a run whose only act is to fail with
/// "'model' is required", which is the form's business and not a run's.
#[test]
fn a_required_field_the_form_has_nothing_in_blocks_the_start() {
    assert_eq!(config_gap(&[]), None);
    assert_eq!(
        config_gap(&["MODEL".into()]),
        Some("set MODEL in Config above — this agent ships no default for it".to_string())
    );
    // Every one of them, not just the first: filling one in and being
    // refused again for the next is the worst way to learn there were two.
    let two = config_gap(&["MODEL".into(), "Mailbox".into()]).unwrap();
    assert!(two.starts_with("set MODEL and Mailbox in Config"), "{two}");
}
