//! `/decision-lab/api/*`, the Decision lab (labs-store design §2): the
//! models that answer `/v1/systemone`, the token gauge, and Ask as a stored
//! group whose cells run one at a time through the real handler. The shapes
//! are `lmgw-api-types`' `decision_lab` and `lab_store` modules, the types
//! the handlers read and write. Admin routes (`Cap::Admin`), so they appear
//! in the admin document only. The history, export and deletes are the lab
//! store's (`labs_store`).

use lmgw_api_types::decision_lab as lab;
use lmgw_api_types::lab_store as store;
use lmgw_api_types::openapi_ext::model_task;

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
        tag: "decision-lab",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::NoContent,
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        errors: &[],
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::DecisionLabModels>()),
            ..route(
                "GET",
                "/decision-lab/api/models",
                "List the decision models",
                "Every model that answers /v1/systemone, as GET /v1/models publishes it: the \
                 local decide/* rows and every cloud alias or catalog entry of task decision. \
                 A local row also carries `facts`: from its GGUF header the decision type, \
                 decision.max_head_tokens and the shape (non_causal, causal_head, \
                 label_logits); from the row the ctx_size, the ubatch and batch in effect with \
                 their source (llama.cpp lowers the ubatch to the batch, embedding mode the \
                 batch to the ubatch), CPU or GPU and the thread count; from its running \
                 container's /props n_ctx_slot. `state` is its container's (null: not \
                 running), `held` whether the GPU hold pauses it now and `hold_fallback` the \
                 alias that answers for it then. `reads_images` is the running server's \
                 vision, else an image among its input modalities. `limits` says what a \
                 request must fit in how many tokens: the ubatch for a non-causal type, the \
                 option span and the context for a causal type with a head, the context for a \
                 label-logits type, the catalog's context for a cloud model. No per-type \
                 option limit is given: llama.cpp's 400 names it. `price` names what prices \
                 a request to it, as the gateway's own lookup does for its primary route: \
                 `local` (free whatever rows exist), else per unit the one row that prices \
                 that unit (the alias's manual row, the alias's catalog row, the upstream \
                 model's manual row, then its catalog row; a row without a usable rate is \
                 none), empty when no row covers it and Usage records its cost as unknown; \
                 `unresolved` says why a name has no one route. `currency` is the rows'.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::DecisionCount>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::DecisionCounted>()),
            model_task: Some(model_task::DECISION),
            writes: Some(false),
            ..route(
                "POST",
                "/decision-lab/api/count",
                "Count a form's tokens",
                "Counts the form on `model`'s resolved route, in the gateway, as \
                 /v1/count_tokens counts: the state, each question (its instructions and its \
                 options, levels or criteria) and each choice question's options alone, and \
                 sets the estimates (the state plus the largest question, or the largest \
                 option span) against the model's limits; `over` is a warning, nothing is \
                 refused. The numbers are approximate: `approx` always holds flattened \
                 (llama.cpp's template wraps the pieces), media_omitted with images, \
                 tokenizer_guess where a cloud model has no counter of its own. What is \
                 judged is the route the count would hit, after the hold's swap: a held row \
                 with a cloud fallback is counted there (`answered_by` names it), and a local \
                 model that is not ready is counted only with `load`, which starts its \
                 container; without it `not_counted` says so, as it does for a count that \
                 failed. A count that starts a model writes a request log row, a cheap one \
                 none. 404 not_found for a model that answers no /v1/systemone; the body size \
                 is not limited.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::DecisionAsk>()),
            response: Resp::Json(|g| g.root_schema_for::<store::LabGroupView>()),
            ..route(
                "POST",
                "/decision-lab/api/groups",
                "Ask: store a group and its plan",
                "Stores what one Ask will run and answers the group with its plan and no runs \
                 yet; the page then posts its cells one at a time. The plan is the models × \
                 (as_written, then each of `variants`), models outer: no_descriptions sends \
                 every choice description as null and no noul criteria, reversed each choice \
                 question's options in reverse. The form is checked first (a state, valid \
                 JSON in json mode, at least one question, unique ids, a choice with unique \
                 named options, a score with 2 to 10 levels: a 400 bad_request naming the \
                 field). An `override` is sent instead of the form, with only `model` \
                 rewritten per model; its only check is that it is a JSON object, and a \
                 perturbation with an override is a 400 (perturbations rewrite the form). The \
                 form's images (data URLs, or artifact ids of a re-opened form) and the \
                 override's data URLs are stored as the group's input artifacts and named by \
                 id in the stored form and override. The body size is not limited.",
            )
        },
        DocRoute {
            path_ints: &["id", "cell"],
            response: Resp::Json(|g| g.root_schema_for::<store::LabCellResult>()),
            ..route(
                "POST",
                "/decision-lab/api/groups/{id}/cells/{cell}",
                "Run one cell",
                "Builds the cell's request (DecisionForm's body, or the override with \
                 `model` rewritten, its images as data URLs), hands it to /v1/systemone's \
                 handler in the gateway — the hold, admission, fallbacks and the systemone \
                 request log row are real; only auth and the HTTP hop are skipped, and \
                 max_body_mb still bounds the bytes — and records the run. It answers 200 \
                 whatever the route said: the run carries `http_status`, `error_code` (the \
                 envelope's lmgw_code, else its code or type), `error_message`, the response \
                 verbatim, `answered_by` (x-lmgw-fallback, else the model asked) with \
                 `fallback_reason`, and the stored request with each image as \
                 lab-artifact:<id>. When the store cannot keep the run, the answer is the \
                 route's all the same: the run has no `id` and `store_error` says why. No \
                 body. 404 not_found for a group or cell that is not there, 409 \
                 cell_answered for a cell that has a run or is being answered, without a \
                 dispatch (a re-ask is a new group).",
            )
        },
    ]
    .into_iter()
    .chain(batch_routes())
    .collect()
}

/// Set runs (labs-store design §3.3, §3.4).
fn batch_routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::BatchPreview>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchPreviewed>()),
            writes: Some(false),
            ..route(
                "POST",
                "/decision-lab/api/batches/preview",
                "Preview a set run's file",
                "Reads `text` as `format` (csv: RFC 4180 with a header row; jsonl: one JSON \
                 object per line, its columns its top-level keys in the order first met) and \
                 answers the columns (each with `json`: every non-empty value is a JSON object \
                 or array, which the state's json mode fits), the number of rows and the first \
                 `sample` rows (20 when absent). With a `mapping` and the set's `questions`, \
                 every row is checked as a start checks it and `problems` lists each refusal \
                 with its row, line, column and value: a mapping that names no state column \
                 or a column with an empty name (a column whose header is empty cannot be \
                 mapped), a mapped column the file lacks or names twice, an empty state, a \
                 state that is no JSON in json mode, a label that is no answer of its \
                 question, a score's label column that reads both as level texts and as \
                 indices with different meanings or mixes the two (set `score_labels`). \
                 Each problem's `kind` says which of these it is (file, mapping, state, \
                 label, score_labels_needed) and `question` the question it is about, so a \
                 client can act on a problem without reading its message. Without a \
                 mapping, `problems` holds the file's own (a line that does not read). \
                 Nothing is stored; the body size is not limited.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::BatchStart>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchView>()),
            model_task: Some(model_task::DECISION),
            ..route(
                "POST",
                "/decision-lab/api/batches",
                "Start a set run",
                "Runs a question set over every row of a CSV or JSONL file as a job (kind \
                 lab_batch, key lab-batch:<id>, one live job per run). The set is a saved one \
                 (`preset_id`, whose body becomes the run's snapshot and takes \"Use as \
                 threshold\") or one sent whole (`set`), not both; `model` blank asks the \
                 set's default model. The mapping names the state column (sent as text, or as \
                 JSON in json mode), optionally a `key` column whose value each item keeps \
                 and both exports carry, and per question the column of its expected answer: \
                 an option's name (choice); yes, no, true, false, 1 or 0, any case (noul); a \
                 level's exact text or its index from 0 (score), as the whole column reads: \
                 texts when every value is a level's text, indices when every value is an \
                 index, and refused when both fit with different meanings or the column \
                 mixes them, until `score_labels` says which; an empty value is no label. \
                 Every problem refuses the whole file, none is dropped: a 400 bad_input \
                 naming each row, line, column and value. The run and its items (each a lab \
                 group of one cell, its form the set with the item's state, its labels kept \
                 as the option's name, the level's index or yes and no) are written in \
                 several short write transactions, the run not ready (not run, listed or \
                 counted) until the last; a boot deletes one a crash left half written. Then \
                 the job answers the items through the same path as a lab cell, \
                 `concurrency` at once (1 when absent; admission and the model's slots are \
                 the only other bound). 400 bad_request for a set that does not check or a \
                 concurrency of 0, 404 not_found for a preset the lab does not have or a \
                 model that answers no /v1/systemone, 409 batch_deleted when a Clear took \
                 the run while it was written. The body size is not limited.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::BatchFromAgent>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchFromAgentSaved>()),
            model_task: Some(model_task::DECISION),
            ..route(
                "POST",
                "/decision-lab/api/batches/from-agent",
                "Save an agent run as a labelled set",
                "Copies a batch agent's finished apply run (`job_id`, required, not queued or \
                 running) into a set run that is not run yet (status not_started, no job; \
                 Resume runs it). The labels are the rows the apply job received, the checked \
                 rows with the reviewer's edits: each item's state is the prompt the row's \
                 classify call sent (as text), its expected answer the row's output field, its \
                 key the row's id (source name `agent run <job id> of <agent id>`, mapping \
                 state `prompt`, key `id`). Answered in `skipped`, with the reason: a row the \
                 model was never asked about (its fetch failed, or it has no id: no state), and \
                 a row whose call failed with its output still the fallback (no answer, no \
                 label). The set is the one stamped on the rows (each row's `set`: the \
                 question set that classified it), or `preset_id` as an explicit override; \
                 rows of several sets, or stamped rows beside rows the model call classified, \
                 are a 400 mixed_sets naming each set's rows. Its question of the output field \
                 must be a choice, and its default model asks; each item's form, and the run's \
                 set, is the set restricted to that question, the request the agent sends. \
                 `set_changed` says the set was edited since its rows were classified. A label \
                 that is no option of that question refuses the save, a 400 bad_input naming \
                 each row and value (add the option to the set). 400 bad_request for a run \
                 that is no apply run, an agent whose output is no single field, rows no set \
                 classified without `preset_id`, a set that does not check, has no such \
                 question, asks it as another type or names no model, or a run none of whose \
                 rows has an answer or a label; 422 for a body without `job_id`; 404 not_found \
                 for an agent run, agent or preset that is not there, or a model that answers \
                 no /v1/systemone; 409 run_live while the run applies, 409 batch_deleted when \
                 a Clear took the run while it was written.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchList>()),
            ..route(
                "GET",
                "/decision-lab/api/batches",
                "List the set runs",
                "Every set run, newest first: the set as run, the model, the file's name, \
                 the mapping, the concurrency and the progress (`done` items with a run of \
                 `total`, `failed` runs that are no 2xx, `unreadable` 2xx runs that do not \
                 read as an answer to every question, `labelled` items). `status` is running \
                 while its job runs (`job_id` the live job), done once every item has a run, \
                 canceled, interrupted when its job ended with items left (a gateway restart \
                 marks the job failed with `job_error` interrupted by shutdown; lmgw refusing \
                 an item before asking the model, such as the GPU hold, admission or the \
                 container's start, ends the job failed with that reason and leaves the item \
                 without a run; a pruned job row reads the same), or not_started. The job is \
                 found by its key, the newest row of it. `cancel_requested` is true while \
                 the job running it was already asked to cancel: the next Cancel aborts the \
                 calls in flight.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchView>()),
            ..route(
                "GET",
                "/decision-lab/api/batches/{id}",
                "Read a set run",
                "One set run as in the list. 404 not_found for a run that is not there.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            query: Some(|g| g.root_schema_for::<lab::BatchItemsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchItemPage>()),
            ..route(
                "GET",
                "/decision-lab/api/batches/{id}/items",
                "Read a set run's items",
                "One page of the run's items in file order, each a lab group with its `item` \
                 (the run, its index and its expected answers) and its run once answered. \
                 `after` takes the items after that index (the next page), `limit` the \
                 page's size (absent, every item left: the server sets no maximum). `total` \
                 counts every item; `next_after` is the cursor of the next page, the page's \
                 last index, null when no item follows. 404 not_found for a run that is not \
                 there.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchView>()),
            ..route(
                "POST",
                "/decision-lab/api/batches/{id}/cancel",
                "Cancel a set run",
                "Asks the run's job to stop: no new item is taken, the calls in flight finish \
                 and are kept, then the run reads canceled and Resume runs the rest. A second \
                 Cancel while the job still runs drops the calls in flight (a call may hang): \
                 the request log keeps their 499 rows as for any client gone, and their items \
                 stay without a run. Answers the run, whose `cancel_requested` says the next \
                 Cancel aborts. 404 not_found for a run that is not there, 409 \
                 batch_not_running when no job runs it.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::OptionalJson(|g| g.root_schema_for::<lab::BatchResume>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchView>()),
            ..route(
                "POST",
                "/decision-lab/api/batches/{id}/resume",
                "Resume a set run",
                "A new job for the items that have no run, after a cancel, a restart, a \
                 refusal or a failure, at `concurrency` when given (kept as the run's), else \
                 the run's. With `retry_failed` the items whose run failed (no 2xx) are asked \
                 again too, each new run replacing the failed one. Answers the run. 404 \
                 not_found for a run that is not there, 409 batch_running while a job runs \
                 it, 409 batch_done when every item has a run (and, with retry_failed, none \
                 failed), 400 bad_request for a concurrency of 0.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<store::LabUsage>()),
            confirm_note: Some("The run, its items and their answers are deleted."),
            ..route(
                "POST",
                "/decision-lab/api/batches/{id}/delete",
                "Delete a set run",
                "Stops a running job and drops its calls in flight (499 rows in the request \
                 log, as for any client gone), then deletes the run, its items, their runs \
                 and artifact rows, and the files no other row names. The saved set it ran \
                 stays. Answers the lab's usage after it. 404 not_found for a run that is not \
                 there.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            query: Some(|g| g.root_schema_for::<lab::MetricsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::BatchMetrics>()),
            ..route(
                "GET",
                "/decision-lab/api/batches/{id}/metrics",
                "Read a set run's metrics",
                "Computed from the stored runs, per question of the set as run (`question` \
                 one of them): items, answered, failed (a run that is no 2xx), unreadable (a \
                 2xx without a readable answer of the question's type), labelled and scored \
                 (answered and labelled) counts; accuracy, the winner (choice), P(yes) at or \
                 above 0.5 (noul) or the nearest level (score) equal to the label, and for a \
                 score the mean absolute distance in levels between its expected level and \
                 the label; the confusion matrix, labels by predictions in `classes` order \
                 (the options, the levels' indices, yes and no); the reliability diagram of \
                 the labelled answers over `bins` equal-width bins of the predicted \
                 probability (the winner's, the nearest level's, P(yes) for noul), only the \
                 bins that hold an answer, each with its index, edges (index / bins to \
                 (index + 1) / bins, the values an answer is placed by), count, mean \
                 prediction and `observed`, the rate of the winner or level being right (for \
                 noul, of the label being yes); 10 bins when absent, at least 1, no maximum \
                 (a 400 bad_request for 0), any number costing what the answers do; and the \
                 full threshold curve, a point per distinct confidence (certainty max(p, 1 − \
                 p) for noul) with the answered items at or above it, their share of all \
                 answered, the labelled among them and how many of those are right. 404 \
                 not_found for a run or question that is not there.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<lab::ThresholdWrite>()),
            response: Resp::Json(|g| g.root_schema_for::<store::LabPresetView>()),
            ..route(
                "POST",
                "/decision-lab/api/batches/{id}/threshold",
                "Use as threshold",
                "Measures the cut at `value` for `question` on this run (coverage of the \
                 answered items, accuracy and count of the labelled ones at or above it) and \
                 writes the value with that measurement into the saved set the run ran. \
                 Answers the set. 400 bad_request for a value outside 0 to 1, 404 not_found \
                 for a run or question that is not there, 409 set_deleted when the run ran an \
                 unsaved set or the set is deleted, 409 set_changed when the set's question \
                 differs from the one the run asked (or the set changed while measuring), 409 \
                 no_labels when no labelled answer lies at or above the cut. The body size is \
                 not limited.",
            )
        },
    ]
}
