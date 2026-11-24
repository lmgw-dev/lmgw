//! Agents (agent-catalog design §6): the catalog as a list beside the
//! selected agent (modern-sample.html, on the shared split view
//! [`crate::widgets::nav_split`]), one route for both, `agents/:id?/:tab?`.
//!
//! The list ([`list`]) has a row per installed agent under its run kind's
//! heading: the name, its state (a dot and a word: ready, running, disabled,
//! what keeps it from running) and a line of facts (the model it will ask
//! for, the tool labels it attaches, what it has done). The add menu holds
//! the three ways to get one ([`modals`]): New (a starter manifest, edited on
//! its Definition tab), Import (a file or a paste, one endpoint) and Install
//! from an image; and Restore shipped. With nothing selected the detail is
//! a status page ([`status`]); a selected agent's detail is
//! [`super::agent_detail`], its tabs paths under `/agents/<id>`.
//!
//! The rule the row exists to show: **a missing MCP server is a warning, not a
//! refusal** (§5). An agent imports before its server is wired, the way a
//! server is registered before its image is pulled; the row says what is
//! missing and the detail links the MCP page instead of hiding the agent.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_navigate, use_params_map};
use lmgw_api_types::{AgentCard, AgentsRestored};
use serde_json::{json, Value};

use crate::scope::Scope;
use crate::widgets::list_pane::{query_keeping, ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Density, PageFrame};

/// The list pane: the header, the search, the rows by run kind.
mod list;
/// Import, Install from an image, New.
mod modals;
/// Nothing selected: what the catalog holds and the ways to add.
mod status;

pub(crate) use list::kind_icon;

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested; there is no DOM harness in this crate)
// ---------------------------------------------------------------------------

/// Does this card match what was typed in the search box?
///
/// Everything the card shows is searchable — id, name, description, model and
/// tool labels — because "the one that uses gws" is as likely a way to look for
/// an agent as its name.
pub fn matches_query(c: &AgentCard, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = [
        c.id.as_str(),
        c.name.as_str(),
        c.description.as_str(),
        c.kind.as_str(),
        c.effective_model.as_str(),
    ];
    hay.iter().any(|h| h.to_lowercase().contains(&q))
        || c.labels.iter().any(|l| l.to_lowercase().contains(&q))
}

/// The card's activity line: what this agent has actually done.
///
/// A `chat` agent counts threads, a `batch` agent names its last run. Neither
/// is invented: an agent that has done nothing says so, rather than showing a
/// zero that reads like a failure.
pub fn activity_line(c: &AgentCard) -> String {
    if c.kind == "chat" {
        return match c.threads {
            0 => "no threads yet".to_string(),
            1 => "1 thread".to_string(),
            n => format!("{n} threads"),
        };
    }
    match &c.last_run {
        Some(r) => format!(
            "last run: {} {}",
            r.status,
            short_ts(r.finished_at.as_deref().unwrap_or(&r.created_at))
        ),
        None => "never run".to_string(),
    }
}

/// `2026-09-18 09:12:07` → `2026-09-18 09:12`. SQLite writes seconds nobody
/// reads; where they might matter the caller keeps the stored value in a
/// `title` attribute (the Runs and Threads tables do).
pub fn short_ts(ts: &str) -> String {
    ts.get(0..16).unwrap_or(ts).to_string()
}

/// The one-line warning under a card whose tools do not resolve here.
pub fn requires_warning(c: &AgentCard) -> Option<String> {
    if c.requires_ok {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for r in &c.requires {
        if !r.registered {
            parts.push(format!("needs MCP server '{}'", r.label));
        } else if !r.missing_tools.is_empty() {
            parts.push(format!(
                "'{}' is missing {}",
                r.label,
                r.missing_tools.join(", ")
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Every warning line under one card: the tool gaps first (their own sentence,
/// with the MCP link), then §4.3's — podman missing, an image that is not on
/// the box, a `localhost/` image that arrived in an import.
///
/// One list rather than two renders, because to the owner they are the same
/// question: is there anything stopping this agent from running here.
pub fn card_warnings(c: &AgentCard) -> Vec<String> {
    c.warnings.iter().map(|w| w.message.clone()).collect()
}

/// The starter manifest New writes. A `chat` agent with a model picker and an
/// empty system prompt: the smallest thing that is valid, runnable and obvious
/// to edit. A batch agent starts from this one too — the Definition tab is
/// where `run.kind` changes, and `agent_set` validates it there.
pub fn starter_manifest(id: &str, name: &str) -> String {
    let id = Value::String(id.to_string());
    let name = Value::String(name.to_string());
    format!(
        r#"{{
  "schema_version": 1,
  "id": {id},
  "name": {name},
  "description": "",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{
    "schema": {{
      "type": "object",
      "properties": {{
        "model": {{ "type": "string", "format": "model_alias", "title": "Model" }}
      }},
      "required": ["model"]
    }}
  }},
  "tools": [],
  "run": {{
    "kind": "chat",
    "system": "You are a helpful assistant."
  }}
}}"#
    )
}

/// The pull policy picker's three words, with what each one does spelled out —
/// the choice is never implicit (§4.1), and "never" is the default because a
/// multi-gigabyte download is not something an install should start on its own.
pub fn pull_note(pull: &str) -> &'static str {
    match pull {
        "missing" => "Download it if it is not on this box",
        "always" => "Download it every time, even if it is already here",
        _ => "Never download: if the image is not on this box, say so and stop",
    }
}

/// The pull policy's three choices, as the install dialog offers them.
fn pull_options() -> Vec<(String, String)> {
    [
        ("never", "never — do not download"),
        ("missing", "missing — download it once"),
        ("always", "always — download every time"),
    ]
    .into_iter()
    .map(|(v, l)| (v.to_string(), l.to_string()))
    .collect()
}

// ---------------------------------------------------------------------------
// The page: the list beside the detail
// ---------------------------------------------------------------------------

/// Below this pane width the view is one pane at a time (a 1024 px window at
/// 125 % leaves the pane about 770 px), as Models'.
const NARROW_BELOW: f64 = 800.0;

const CONF: SplitConf = SplitConf {
    // Dense: the agent pages' forms and tables are the older controls, which
    // take their height from the page's density.
    class: "agents-split density-dense",
    width_key: "agents.list.width",
    list_width: (260, 32, 380),
    detail_min: 440,
    narrow_below: NARROW_BELOW,
    handle_label: "Resize the list",
};

/// What the list, the status page, the add dialogs and the detail share: the
/// catalog as last read, the way to read it again, the dialogs' switches and
/// the page's navigation (asking the unsaved-edits guard first).
#[derive(Clone, Copy)]
pub(crate) struct AgentsCtx {
    pub cards: RwSignal<Option<Vec<AgentCard>>>,
    pub error: RwSignal<Option<String>>,
    /// Read `/api/agents` again (after anything that changes a row).
    pub reload: Callback<()>,
    pub import_open: RwSignal<bool>,
    pub install_open: RwSignal<bool>,
    pub new_open: RwSignal<bool>,
    /// Put the shipped agents back (`agents_restore`).
    pub restore: Callback<()>,
    pub restoring: RwSignal<bool>,
    nav: Nav,
}

impl AgentsCtx {
    /// Go to `to`, unless unsaved edits it would lose put the question up
    /// first (whose "Leave and discard" then goes there); `false` then.
    pub fn go(self, to: &str, replace: bool) -> bool {
        self.nav.go(to, replace)
    }

    /// Go without asking: what would be asked about is gone already (the
    /// edits of an agent that was deleted).
    pub fn go_unasked(self, to: &str, replace: bool) {
        self.nav.go_unasked(to, replace);
    }
}

/// The list's own query keys, carried from one selection to the next.
const LIST_KEYS: &[&str] = &["q"];

/// The list's own query (`q`) out of a search string.
pub fn list_query(search: &str) -> String {
    query_keeping(search, LIST_KEYS)
}

/// Where a row of another agent opens from the tab being shown: Runs and
/// Definition every agent has, so the tab is kept; the App tab only some.
pub fn row_href(id: &str, tab: Option<&str>) -> String {
    match tab {
        Some(t @ ("runs" | "definition")) => format!("/agents/{id}/{t}"),
        _ => format!("/agents/{id}"),
    }
}

#[component]
pub fn Agents() -> impl IntoView {
    let toasts = use_toasts();
    let params = use_params_map();
    let sel = Memo::new(move |_| {
        params
            .with(|p| p.get("id"))
            .filter(|s| !s.trim().is_empty())
    });
    let has_sel = Memo::new(move |_| sel.with(Option::is_some));
    let cards = RwSignal::new(None::<Vec<AgentCard>>);
    let error = RwSignal::new(None::<String>);
    let scope = Scope::new();
    let reload = Callback::new(move |()| {
        scope.spawn(async move {
            match crate::api::get::<Vec<AgentCard>>("/api/agents").await {
                Ok(list) => {
                    error.set(None);
                    cards.set(Some(list));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    });
    reload.run(());
    let restoring = RwSignal::new(false);
    let restore = Callback::new(move |()| {
        if restoring.get_untracked() {
            return;
        }
        restoring.set(true);
        spawn_local(async move {
            let res =
                crate::api::post::<AgentsRestored, _>("/api/op/agents_restore", &json!({})).await;
            restoring.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    });
    let page = AgentsCtx {
        cards,
        error,
        reload,
        import_open: RwSignal::new(false),
        install_open: RwSignal::new(false),
        new_open: RwSignal::new(false),
        restore,
        restoring,
        nav: Nav::new(),
    };
    provide_context(page);

    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let refs = ListRefs::new();
    let open_row = Callback::new(move |(id, replace): (String, bool)| {
        // Another agent's row opens on the tab being shown, where it has it.
        if sel.get_untracked().as_deref() != Some(id.as_str()) {
            let tab = params.with_untracked(|p| p.get("tab"));
            page.nav
                .open(&row_href(&id, tab.as_deref()), LIST_KEYS, replace);
        }
    });
    let back = Callback::new(move |()| page.nav.back("/agents", LIST_KEYS, refs));

    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=refs.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=ViewFn::from(move || {
                view! { <list::ListPane page=page sel=sel narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! {
                    <Show
                        when=move || has_sel.get()
                        fallback=move || view! { <status::StatusPage page=page narrow=narrow back=back/> }
                    >
                        <super::agent_detail::AgentDetailPage narrow=narrow back=back/>
                    </Show>
                }
            })
        />
        <div class="density-dense agents-modals">
            <modals::ImportModal open=page.import_open on_change=reload/>
            <modals::InstallModal open=page.install_open on_change=reload/>
            <modals::NewAgentModal open=page.new_open page=page/>
        </div>
    }
}

// ---------------------------------------------------------------------------
// `/workflows` → `/agents`
// ---------------------------------------------------------------------------

/// The old Workflows deep link. The mail workflow becomes the first catalog
/// entry (§7), so the path lands on the catalog rather than 404ing — and says
/// so for the half-second before the router moves.
///
/// The navigate runs in an effect rather than during render: writing the
/// router's location while the route that owns it is still rendering is the
/// kind of thing that works until it doesn't.
#[component]
pub fn WorkflowsMoved() -> impl IntoView {
    let navigate = use_navigate();
    Effect::new(move |_| navigate("/agents", Default::default()));
    view! {
        <PageFrame title="Agents" density=Density::Dense>
            <div class="card dim">"Workflows are now Agents — taking you there…"</div>
        </PageFrame>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::{AgentRequirement, AgentRunSummary};

    fn card() -> AgentCard {
        AgentCard {
            id: "mail-labeler".into(),
            name: "Mail labeler".into(),
            description: "Classifies unread Gmail into labels.".into(),
            kind: "batch".into(),
            effective_model: "gemma4-e4b".into(),
            labels: vec!["gws".into()],
            enabled: true,
            requires_ok: true,
            ..Default::default()
        }
    }

    /// Search covers what the card shows, so "the one that uses gws" and "the
    /// one on gemma" both find it.
    #[test]
    fn search_matches_every_field_the_card_shows() {
        let c = card();
        assert!(matches_query(&c, ""));
        assert!(matches_query(&c, "  "));
        assert!(matches_query(&c, "MAIL"));
        assert!(matches_query(&c, "gws"));
        assert!(matches_query(&c, "gemma4"));
        assert!(matches_query(&c, "batch"));
        assert!(matches_query(&c, "unread"));
        assert!(!matches_query(&c, "calendar"));
    }

    /// A chat agent counts threads, a batch agent names its last run, and
    /// neither invents activity it has not had.
    #[test]
    fn the_activity_line_follows_the_run_kind() {
        let mut c = card();
        assert_eq!(activity_line(&c), "never run");
        c.last_run = Some(AgentRunSummary {
            job_id: 4,
            status: "done".into(),
            finished_at: Some("2026-09-18 09:12:07".into()),
            ..Default::default()
        });
        assert_eq!(activity_line(&c), "last run: done 2026-09-18 09:12");

        c.kind = "chat".into();
        assert_eq!(activity_line(&c), "no threads yet");
        c.threads = 1;
        assert_eq!(activity_line(&c), "1 thread");
        c.threads = 3;
        assert_eq!(activity_line(&c), "3 threads");
    }

    /// The gap is named on the card: which server, or which tools of a server
    /// that is registered but does not list them.
    #[test]
    fn the_warning_names_the_gap_and_stays_quiet_when_there_is_none() {
        let mut c = card();
        assert_eq!(requires_warning(&c), None);

        c.requires_ok = false;
        c.requires = vec![AgentRequirement {
            label: "gws".into(),
            registered: false,
            ..Default::default()
        }];
        assert_eq!(
            requires_warning(&c).as_deref(),
            Some("needs MCP server 'gws'")
        );

        c.requires = vec![AgentRequirement {
            label: "gws".into(),
            registered: true,
            missing_tools: vec!["gws__gmail_batchModify".into()],
            ..Default::default()
        }];
        assert_eq!(
            requires_warning(&c).as_deref(),
            Some("'gws' is missing gws__gmail_batchModify"),
        );
    }

    #[test]
    fn a_row_keeps_the_tab_every_agent_has_and_the_search() {
        assert_eq!(row_href("a", None), "/agents/a");
        assert_eq!(row_href("a", Some("runs")), "/agents/a/runs");
        assert_eq!(row_href("a", Some("definition")), "/agents/a/definition");
        // Not every agent has an app.
        assert_eq!(row_href("a", Some("ui")), "/agents/a");
        assert_eq!(list_query("?q=mail&tab=runs"), "?q=mail");
        assert_eq!(list_query(""), "");
    }

    /// What New writes has to be a manifest the server accepts, with the two
    /// values quoted rather than pasted — an id is validated server-side, but a
    /// name with a quote in it must not produce broken JSON on the way there.
    #[test]
    fn the_starter_manifest_is_json_with_the_fields_the_schema_requires() {
        let text = starter_manifest("receipt-filer", "Alice's \"filer\"");
        let v: Value = serde_json::from_str(&text).expect("starter manifest is not JSON");
        assert_eq!(v["schema_version"], json!(1));
        assert_eq!(v["id"], json!("receipt-filer"));
        assert_eq!(v["name"], json!("Alice's \"filer\""));
        assert_eq!(v["run"]["kind"], json!("chat"));
        // The model comes from the config form, so a fresh agent is runnable on
        // a machine that has never heard of whatever alias was authored.
        assert_eq!(v["model"]["alias"], json!("{{config.model}}"));
        assert_eq!(
            v["config"]["schema"]["properties"]["model"]["format"],
            json!("model_alias")
        );
    }
}
