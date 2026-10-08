//! Agent lifecycle mutations: `lmgw__agent_set` through
//! `lmgw__agent_delete`.

use crate::mcp::selfadmin::{bool_p, enum_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__agent_set",
            writes: true,
            description:
                "Install or replace an agent from a manifest. `manifest` is the whole JSON \
                 document as a string — schema_version 1, an id matching [a-z0-9][a-z0-9-]*, a \
                 name, a model (`{{config.model}}` lets whoever configures the agent pick), an optional config \
                 schema, the MCP tool labels it may reach, and a run block (`chat` or `batch`). \
                 Unknown fields are refused naming the key, and every {{config.<field>}} must \
                 name a field of the config schema. This is exactly the import path the \
                 dashboard uses, so the report it returns is the same: warnings for tool labels \
                 this gateway does not have (the agent still installs; it just cannot run yet). \
                 Validate first with validate_only=true. Replacing keeps the stored config. \
                 Pass the manifest as a JSON *string*, not as an object: an object is accepted \
                 but arrives with its keys re-sorted, which reorders the config form the author \
                 wrote, and the report says so.",
            props: vec![
                (
                    "manifest",
                    str_p(
                        "The whole manifest document, as a JSON string. An object works too, \
                         but re-sorts the config form — send the string.",
                    ),
                ),
                (
                    "replace",
                    bool_p("Overwrite an existing agent with this id. Default true."),
                ),
                (
                    "validate_only",
                    bool_p("Report what would happen and write nothing. Default false."),
                ),
            ],
            required: &["manifest"],
        },
        Builtin {
            name: "lmgw__agent_install",
            writes: true,
            description:
                "Install an agent from an **image**: an OCI image carrying its manifest at \
                 /lmgw/agent.json. lmgw reads the manifest out of the image without starting it \
                 (podman create, cp, rm) and installs it through exactly the same path \
                 lmgw__agent_set uses, so the report is the same one — including the warnings \
                 for MCP labels this gateway does not have. Use this instead of pasting a \
                 manifest when the agent ships as an image; the row then records which image and \
                 digest it came from, and 'Pull image' on the dashboard can tell an administrator when \
                 that image has moved. `pull` defaults to 'never': an image that is not already \
                 on the box is reported rather than downloaded, because a multi-gigabyte pull is \
                 a decision for an administrator. Pass pull='missing' to let lmgw fetch it. Replacing an \
                 existing agent needs replace=true and keeps its stored config.",
            props: vec![
                (
                    "image",
                    str_p("The image reference, tag and all (e.g. localhost/mail-labeler:1)."),
                ),
                (
                    "pull",
                    enum_p(
                        "What to do when the image is not on this box. Default 'never' — report \
                         it, download nothing.",
                        &["never", "missing", "always"],
                    ),
                ),
                (
                    "replace",
                    bool_p(
                        "Overwrite an existing agent with the id inside the image (its config is \
                         kept). Default false.",
                    ),
                ),
                (
                    "validate_only",
                    bool_p("Report what the image would install and write nothing. Default false."),
                ),
            ],
            required: &["image"],
        },
        Builtin {
            name: "lmgw__agent_run",
            writes: true,
            description:
                "Start one run of a `batch` agent and return its job id. `list` runs the source \
                 and fetch steps only — no model call, no tokens, just the rows, which is the \
                 cheap way to see what the agent would work on. `classify` also makes one \
                 structured model call per row. Both are **read-only**: nothing an agent writes \
                 happens until a person presses Apply on the dashboard, and apply is not \
                 startable from here. The run is a job: poll it with lmgw__status or \
                 GET /api/agents/runs/<job_id>, which carries the rows. One run per agent at a \
                 time; starting a second returns the one already in flight.",
            props: vec![
                ("id", str_p("Agent id, from lmgw__agents.")),
                (
                    "phase",
                    enum_p(
                        "What to run. Apply stays a human action and is refused here.",
                        &["list", "classify"],
                    ),
                ),
            ],
            required: &["id", "phase"],
        },
        Builtin {
            name: "lmgw__agent_delete",
            writes: true,
            description: "Remove an agent from the catalog. A shipped agent stays deleted across \
                 restarts; the `agents_restore` op on the dashboard is the deliberate way back. \
                 Export it first (GET /api/agents/<id>/export) if it was hand-written — the \
                 manifest is the only copy.",
            props: vec![("id", str_p("Agent id, from lmgw__agents."))],
            required: &["id"],
        },
    ]
}
