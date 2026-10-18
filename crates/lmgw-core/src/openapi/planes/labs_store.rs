//! `/labs/api/*`, the lab store (labs-store design §1.6): the history, usage,
//! deletes, export and artifacts the three labs share. The shapes are
//! `lmgw-api-types`' `lab_store` module, the types the handlers read and
//! write. Admin routes (`Cap::Admin`), so they appear in the admin document
//! only.

use lmgw_api_types::lab_store as store;

use super::super::registry::{Dialect, DocRoute, Req, Resp};

fn route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "labs",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Json(|g| g.root_schema_for::<store::LabUsage>()),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        errors: &[],
        example: None,
    }
}

fn export_line(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    g.root_schema_for::<store::LabExportLine>()
}

/// The files export's zip: binary, with the schema of its manifest's lines
/// (`LabFilesManifestLine`, a component of the document) beside it.
fn files_zip(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let line = g.subschema_for::<store::LabFilesManifestLine>();
    schemars::json_schema!({
        "type": "string",
        "format": "binary",
        (lmgw_api_types::openapi_ext::ZIP_MANIFEST): line,
    })
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            query: Some(|g| g.root_schema_for::<store::LabGroupsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<store::LabGroupPage>()),
            ..route(
                "GET",
                "/labs/api/{lab}/groups",
                "Read a lab's history",
                "One page of what the lab kept, newest first: each group (one click: the \
                 form, the plan of cells, the override) with its runs in cell order and its \
                 artifacts. A set run's items are not the history's: they are read by their \
                 run. `{lab}` is decision, image or audio; any other is a 404 \
                 not_found. `before` takes the groups older than that id (the next page), \
                 `limit` the page's size (absent, every group left: the server sets no \
                 maximum), `model` only the groups whose plan asks that model. `total` \
                 counts every group the filter takes, `older` those older than the page's \
                 last. `in_flight` says a cell of the group is being answered now; a group \
                 with fewer runs than cells that is not in flight was stopped. A run's `request` and \
                 `response` are the texts that went and came, never re-serialized; an image \
                 sent as a data URL is `lab-artifact:<id>` in the stored request, and an \
                 artifact's `href` is where its bytes are.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<store::LabGroupView>()),
            ..route(
                "GET",
                "/labs/api/{lab}/groups/{id}",
                "Read one group",
                "One group with its runs and artifacts, as in the history; a set run's item \
                 too, with its `item`. 404 not_found for a lab that is none, or a group that \
                 is not there or is another lab's.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            confirm_note: Some("The group's runs and files are deleted, not archived."),
            ..route(
                "POST",
                "/labs/api/{lab}/groups/{id}/delete",
                "Delete a group",
                "Deletes the group, its runs and its artifact rows, then the files no other \
                 row names. Answers the lab's usage after it. 404 not_found for a group that \
                 is not there or is another lab's, 409 batch_item for a set run's item, which \
                 goes with its run, 409 in_flight for a group whose run is on its way (stop \
                 it, or delete it once its run is in; Clear is not refused, and a run that \
                 lands after it is answered as not kept).",
            )
        },
        DocRoute {
            confirm_note: Some(
                "Every group and set run of the lab is deleted, with its runs and files.",
            ),
            ..route(
                "POST",
                "/labs/api/{lab}/clear",
                "Clear a lab",
                "Deletes every group and set run of the lab as their deletes do (a running set \
                 run's job drops its calls in flight first), then the files no remaining row \
                 names. \
                 Answers the lab's usage after it, which then counts nothing. The lab's \
                 presets stay. Nothing is deleted by age or count otherwise: the lab store has \
                 no retention rule.",
            )
        },
        DocRoute {
            ..route(
                "GET",
                "/labs/api/{lab}/usage",
                "Read what a lab keeps",
                "The counts of the lab's groups (the history's and the set runs' items), set \
                 runs, runs and artifact rows, the distinct files those rows name, the stored \
                 text's size (titles, forms, plans, overrides, expected answers, requests, \
                 responses and headers, the set runs' sets and mappings, in bytes) and the \
                 files' size. \
                 `orphan_files` and `orphan_bytes` count the artifacts directory's files \
                 that no row of any lab names, left by a crash between a write and its \
                 commit or a commit and its unlink.",
            )
        },
        DocRoute {
            ..route(
                "POST",
                "/labs/api/{lab}/orphans/delete",
                "Remove the orphan files",
                "Removes every file in the artifacts directory that no row of any lab names, \
                 and answers the lab's usage after it. Nothing removes them on its own.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<store::LabExportQuery>()),
            response: Resp::Download(&[
                ("application/x-ndjson", Some(export_line)),
                ("text/csv; charset=utf-8", None),
                ("application/zip", Some(files_zip)),
                ("audio/*", None),
                ("image/*", None),
                ("text/plain; charset=utf-8", None),
            ]),
            ..route(
                "GET",
                "/labs/api/{lab}/export",
                "Export a lab",
                "The scope as one file with Content-Disposition: attachment \
                 (lmgw-lab-{lab}-{date}, with -group-{id}, -batch-{id} or -{model} for a \
                 scope) and Cache-Control: no-store. No scope is the whole lab, the history \
                 and every set run's items; `group` one group (404 not_found when the lab has \
                 none by that id); `batch` one set run's items in file order (404 not_found \
                 likewise); `model`, `from` and `to` (RFC 3339, `to` exclusive; another \
                 spelling is a 400 bad_request) a filter. format=jsonl, the default, is \
                 lossless: one lmgw.lab.v1 line per group, oldest first, its runs inside with \
                 `request` and `response` as the exact texts and a set run's item with its \
                 `item` (its expected answers among it), application/x-ndjson; when the scope \
                 has artifacts the file is a zip of that JSONL and \
                 artifacts/<sha256>.<ext>, and each artifact's `href` is its path inside the \
                 zip (a file missing from the data dir is named in a MISSING.txt). \
                 format=csv is the lab's flat table, text/csv: the Decision lab's has a row \
                 per run and question (group_id, batch_id, item_ix, item_key, run_id, \
                 created_at, model_asked, answered_by, upstream_model, variant, http_status, \
                 error_message, latency_ms, input_tokens, question_id, question_type, answer, \
                 expected, correct, confidence, probabilities, state), a failed run of the \
                 history one row with the question columns empty. A set run's item names its \
                 run, its index in the file and its key (the three are empty for the \
                 history), and has a row per question of its set whatever its run answered: \
                 `expected` its label for the question, `correct` whether the answer matches \
                 it (true or false), empty without a label and, for `correct`, when the run \
                 failed or did not answer the question readably. The Image and Audio \
                 labs' tables have a row per run (group_id, run_id, created_at, endpoint, \
                 model_asked, answered_by, fallback_reason, http_status, error_code, \
                 error_message, latency_ms, response_type), then the Image lab's prompt, \
                 negative_prompt, size, n, steps, seed, output_format, images, image_bytes \
                 and artifact_ids, the Audio lab's task, input, voice, language, text (a \
                 transcription's), files, file_bytes and artifact_ids; a group with no run \
                 has no row. format=files, the Image and Audio labs' only (the Decision lab \
                 makes no files: 400 bad_request), is the files the runs made, to keep and \
                 play: each named <group id, six digits>-<model>-<voice or task>-<first \
                 words of the text>.<ext>, lower case, anything but letters, digits, `.` \
                 and `_` a dash, a run with several outputs adding each one's id or place, \
                 a name taken twice `-2`, `-3`; a picture or an audio file as kept, a \
                 streamed or raw PCM speech answer as a WAV (the form's PCM format, the \
                 x-lmgw-sample-rate it was answered with winning), a transcription's or a \
                 task's text answer as a .txt. `group` naming a group whose runs made one \
                 file answers that file as it is, typed as it is; anything else is a zip \
                 (lmgw-lab-{lab}…zip) of the files, oldest group first, and manifest.jsonl, \
                 one lmgw.lab.files.v1 line (LabFilesManifestLine) per file: its name, kind \
                 (output, wav, transcript), group, run, time, route, model asked and \
                 answering, task, voice, the text it was given, the request's parameters as \
                 sent, type, size, its artifact and the group's inputs, which the zip does \
                 not carry (the JSONL export does). A file missing from the data dir is \
                 named in a MISSING.txt and has `missing` true. As large as the scope is, \
                 and not capped.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Binary(&["*/*"]),
            ..route(
                "GET",
                "/labs/api/artifacts/{id}",
                "Read an artifact's bytes",
                "The file as stored, with the media type sniffed from its bytes, \
                 X-Content-Type-Options: nosniff and Content-Security-Policy: sandbox, so a \
                 file opened in a browser runs nothing. 404 not_found for an artifact that \
                 is not there, 404 file_missing when its row is there and its file is not.",
            )
        },
    ]
    .into_iter()
    .chain(preset_routes())
    .collect()
}

/// Presets (labs-store design §3.2).
fn preset_routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            query: Some(|g| g.root_schema_for::<store::LabPresetsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<store::LabPresetList>()),
            ..route(
                "GET",
                "/labs/api/{lab}/presets",
                "List a lab's presets",
                "The lab's presets by name, of `kind` only when given: each its name, its \
                 `kind` and its `body`, the kind's type (a question_set is the Decision lab's \
                 questions, the model it picks and a threshold per question with where it was \
                 measured). 404 not_found for a lab that is none.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<store::LabPresetWrite>()),
            response: Resp::Json(|g| g.root_schema_for::<store::LabPresetView>()),
            ..route(
                "POST",
                "/labs/api/{lab}/presets",
                "Save a preset",
                "Saves a named preset and answers it. 400 bad_request for a kind of another \
                 lab, a blank name or a body that does not check (a question set's questions \
                 as the form's: ids, options, levels; each threshold naming one of its \
                 questions, between 0 and 1); 409 name_taken when the lab has a preset of \
                 that kind by that name. The body size is not limited.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<store::LabPresetView>()),
            ..route(
                "GET",
                "/labs/api/{lab}/presets/{id}",
                "Read a preset",
                "One preset. 404 not_found for one that is not there or is another lab's.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<store::LabPresetUpdate>()),
            response: Resp::Json(|g| g.root_schema_for::<store::LabPresetView>()),
            ..route(
                "POST",
                "/labs/api/{lab}/presets/{id}",
                "Update a preset",
                "Rewrites the preset whole, as a save checks it, and answers it, if it is \
                 still the one read: `updated_at` is the preset's as read, and 409 \
                 set_changed answers when it moved since (another edit, a threshold \
                 written; every write moves it). A question set's question the update \
                 changes (its type, options or levels, its text) loses its threshold's \
                 measurement, the value staying, not measured. The set runs that ran it keep \
                 their snapshot. 404 not_found for one that is not there, 400 bad_request \
                 and 409 name_taken as for a save. The body size is not limited.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<store::LabPresetList>()),
            confirm_note: Some("The set runs that ran it keep their own copy."),
            ..route(
                "POST",
                "/labs/api/{lab}/presets/{id}/delete",
                "Delete a preset",
                "Deletes the preset and answers the lab's presets after it. The set runs that \
                 ran it keep their snapshot and their results. 404 not_found for one that is \
                 not there or is another lab's.",
            )
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::super::super::build::admin_doc;

    #[test]
    fn the_files_zips_manifest_line_is_a_component_the_export_names() {
        let doc = admin_doc();
        assert!(doc
            .pointer("/components/schemas/LabFilesManifestLine")
            .is_some());
        let zip = doc
            .pointer("/paths/~1labs~1api~1{lab}~1export/get/responses/200/content/application~1zip/schema")
            .expect("the zip's schema");
        assert_eq!(
            zip["x-lmgw-zip-manifest"]["$ref"],
            "#/components/schemas/LabFilesManifestLine"
        );
    }
}
