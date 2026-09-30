//! `DIVERGENCES` (api-docs design §4.7 table B): the ops that share a
//! self-admin tool's *name* but not its *arguments* — the struct the
//! dispatcher parses into is what the page documents (finding §0.1); this is
//! the pinned record of exactly how each one differs from `lmgw__<name>`'s
//! flat projection.
//!
//! `openapi_ops.rs`'s `tool_and_op_arguments_agree_except_listed_divergences`
//! (§7.1) checks this list both ways: every other `x-lmgw-tool` op's
//! properties must be a superset of its tool's, and every op named here must
//! actually still disagree, on each of the [`Divergence::props`] it names —
//! an entry nobody needs any more is a stale claim, not a harmless one.

/// One divergence: `op` and `lmgw__<op>` (always the same name, table B's
/// whole premise) disagree on arguments, for the reason `note` gives.
///
/// `pub`, not `pub(crate)`: `tests/it/openapi_ops.rs` is a separate crate
/// (integration tests compile against `lmgw-core` as an external dependency)
/// and reads this list directly, the same way it reads `web::op_names`.
pub struct Divergence {
    pub op: &'static str,
    pub note: &'static str,
    /// The arguments `note` is about, each of which the test checks still
    /// differs between tool and op (one side only, a different type or enum,
    /// or a different required-ness). Empty only for an op that takes no
    /// body at all. Named per entry because "the two schemas differ
    /// somewhere" is true of almost every struct-derived op — its enums are
    /// `$ref`s where the tool's are inline, and nothing in it is `required`
    /// (review R2 #7).
    pub props: &'static [&'static str],
}

pub const DIVERGENCES: &[Divergence] = &[
    Divergence {
        op: "bench_plan",
        note: "`phases`: the tool takes a comma-separated string (every self-admin argument is \
               a flat scalar); the op takes an array of phase names.",
        props: &["phases"],
    },
    Divergence {
        op: "bench_start",
        note: "`phases`: the tool takes a comma-separated string (every self-admin argument is \
               a flat scalar); the op takes an array of phase names.",
        props: &["phases"],
    },
    Divergence {
        op: "local_model_set",
        note: "`ladder`: the tool takes a JSON-encoded string (every self-admin argument is a \
               flat scalar); the op takes a real array of `Rung`.",
        props: &["ladder"],
    },
    Divergence {
        op: "candidate_alias_set",
        note: "the op's `action` also accepts `preview` — a dry run the dashboard uses that the \
               tool plane has no need of.",
        props: &["action"],
    },
    Divergence {
        op: "price_set",
        note: "`scope_kind` is required by the tool; the op defaults it to `alias` when \
               omitted.",
        props: &["scope_kind"],
    },
    Divergence {
        op: "builds",
        note: "the op takes no arguments at all (it always answers `BuildsResponse`, the whole \
               list); the tool's `id`/`limit`/`before` page one build's runs, which is the \
               separate op `build_get`.",
        props: &[],
    },
    Divergence {
        op: "build_set",
        note: "the op takes `BuildSetArgs` (`action`, `id`, a structured `spec`); the tool takes \
               the flat `BuildPatch` projection of the same fields.",
        props: &["spec"],
    },
    Divergence {
        op: "build_check_merge",
        note: "the op also takes an unsaved `spec` (the build editor's \"check merge\" before \
               anything is saved); the tool only checks a saved build by `id`.",
        props: &["spec"],
    },
    Divergence {
        op: "container_images",
        note: "the op takes `disk` (skip the slow disk-footer scan); the tool always forces it \
               `true`.",
        props: &["disk"],
    },
    Divergence {
        op: "forge_prs",
        note: "the tool accepts `id` (a build, whose repository and forge are resolved from it) \
               instead of the op's `repo_url` + `forge`.",
        props: &["id"],
    },
    Divergence {
        op: "agent_run",
        note: "the op also takes `rows`, `base_job` and `values` (the review/apply loop and the \
               per-run config patch); the tool only starts `list`/`classify`.",
        props: &["rows", "base_job", "values", "phase"],
    },
];
