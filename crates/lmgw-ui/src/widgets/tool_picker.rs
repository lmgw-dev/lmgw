//! The tool picker: toolsets as rows, their tools as ticks. Shared by the
//! Chat's thread settings (which toolsets a thread attaches) and the key
//! editor (which tools a client key may see).
//!
//! Both speak one selection model, a list of [`ThreadMcp`] — a source's label
//! plus `allowed_tools`, where `None` is the source's whole surface and
//! `Some(list)` is exactly those tools. What each does with it differs:
//!
//! - the Chat stores the list as it is (labels + allowed names);
//! - the key editor turns it into `(mode, patterns)` lines with
//!   [`to_patterns`] and reads them back with [`from_patterns`]. A whole
//!   prefixed source is `<prefix>__*`, so it follows tools the source gains
//!   later; single tools are exact names. Lines the picker cannot represent
//!   (a hand-written `git*`, a source that is not in the inventory) come back
//!   as the *advanced* text and go out again untouched.
//!
//! The conversions are plain functions over [`ScopeGroup`]s, so the native
//! tests in this file cover them.

use leptos::prelude::*;
use lmgw_api_types::{McpServerView, McpServersResponse, ToolInventory};
use serde::{Deserialize, Serialize};

/// One attached source. `allowed_tools: None` is its whole surface — the
/// picker only writes a list when the owner unticks something, so a server
/// that later gains a tool offers it without the thread being edited again.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ThreadMcp {
    pub server_label: String,
    pub allowed_tools: Option<Vec<String>>,
}

/// The selection a picker edits: read it, and be told the new list.
#[derive(Clone, Copy)]
pub struct Selection {
    pub list: Signal<Vec<ThreadMcp>>,
    pub set: Callback<Vec<ThreadMcp>>,
}

impl Selection {
    /// A selection that lives in one signal (the Chat's).
    pub fn of(sig: RwSignal<Vec<ThreadMcp>>) -> Self {
        Self {
            list: sig.into(),
            set: Callback::new(move |v| sig.set(v)),
        }
    }
}

/// One tool of a group.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub name: String,
    pub description: Option<String>,
}

/// One source as the key editor's picker sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeGroup {
    /// The source's label — the key of its [`ThreadMcp`] entry.
    pub label: String,
    /// The namespace its exposed names share (`docs` for `docs__query`), or
    /// `None` for a server registered without a tool prefix, whose tools are
    /// exposed bare and so have no glob of their own.
    pub prefix: Option<String>,
    pub tools: Vec<ToolItem>,
    /// Why the source offers nothing right now, when it does not.
    pub reason: Option<String>,
}

impl ScopeGroup {
    fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|t| t.name.as_str())
    }

    /// The pattern that stands for the whole source, when it has one.
    fn whole_glob(&self) -> Option<String> {
        self.prefix.as_ref().map(|p| format!("{p}__*"))
    }
}

/// The groups a client key can be scoped over: every source on the `/mcp`
/// plane, minus stale disable records. The self-admin `lmgw` source is on
/// `/mcp/admin` and is never offered — a client key cannot reach it.
///
/// The prefix comes from the tool names (built-ins) or from the server's
/// registered `tool_prefix` — never from the label, which is the prefix *or*
/// the name.
pub fn scope_groups(inv: &ToolInventory, servers: &[McpServerView]) -> Vec<ScopeGroup> {
    inv.sources
        .iter()
        .filter(|s| s.plane == "/mcp")
        .filter_map(|s| {
            let tools: Vec<ToolItem> = inv
                .tools
                .iter()
                .filter(|t| t.plane == "/mcp" && !t.stale && t.source_label == s.label)
                .map(|t| ToolItem {
                    name: t.name.clone(),
                    description: t.description.clone(),
                })
                .collect();
            if tools.is_empty() {
                return None;
            }
            let prefix = if s.kind == "server" {
                s.server_id
                    .and_then(|id| servers.iter().find(|m| m.id == id))
                    .map(|m| m.tool_prefix.trim().to_string())
                    .filter(|p| !p.is_empty())
            } else {
                common_prefix(&tools)
            }
            // Defensive: a prefix its own tools do not carry is no namespace.
            .filter(|p| {
                tools
                    .iter()
                    .all(|t| starts_with_ci(&t.name, &format!("{p}__")))
            });
            Some(ScopeGroup {
                label: s.label.clone(),
                prefix,
                tools,
                reason: s.reason.clone().filter(|_| !s.available),
            })
        })
        .collect()
}

/// `p` when every tool is `p__…`.
fn common_prefix(tools: &[ToolItem]) -> Option<String> {
    let first = tools.first()?.name.split_once("__")?.0.to_string();
    tools
        .iter()
        .all(|t| t.name.split_once("__").is_some_and(|(p, _)| p == first))
        .then_some(first)
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    lmgw_api_types::scope::fold(s).starts_with(&lmgw_api_types::scope::fold(prefix))
}

fn same(a: &str, b: &str) -> bool {
    lmgw_api_types::scope::fold(a) == lmgw_api_types::scope::fold(b)
}

/// Read a stored pattern list into the picker's selection, and the lines it
/// cannot represent (as text, one per line, in their original order).
///
/// `<prefix>__*` of a known group ticks the whole group; the exact name of a
/// group's tool ticks that tool; anything else is *advanced*. Matching is
/// case-insensitive, like the gate.
pub fn from_patterns(patterns: &str, groups: &[ScopeGroup]) -> (Vec<ThreadMcp>, String) {
    let mut whole = vec![false; groups.len()];
    let mut picked: Vec<Vec<&str>> = vec![Vec::new(); groups.len()];
    let mut advanced: Vec<&str> = Vec::new();
    for line in patterns.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(i) = groups
            .iter()
            .position(|g| g.whole_glob().is_some_and(|w| same(&w, line)))
        {
            whole[i] = true;
        } else if let Some((i, name)) = groups
            .iter()
            .enumerate()
            .find_map(|(i, g)| g.names().find(|n| same(n, line)).map(|n| (i, n)))
        {
            picked[i].push(name);
        } else {
            advanced.push(line);
        }
    }
    let list = groups
        .iter()
        .enumerate()
        .filter_map(|(i, g)| {
            if whole[i] {
                return Some(ThreadMcp {
                    server_label: g.label.clone(),
                    allowed_tools: None,
                });
            }
            if picked[i].is_empty() {
                return None;
            }
            // In the group's order, whatever order the lines came in.
            let allowed: Vec<String> = g
                .names()
                .filter(|n| picked[i].contains(n))
                .map(str::to_string)
                .collect();
            Some(ThreadMcp {
                server_label: g.label.clone(),
                allowed_tools: Some(allowed),
            })
        })
        .collect();
    (list, advanced.join("\n"))
}

/// The pattern lines a selection stands for, followed by the advanced lines.
///
/// A whole prefixed source is its glob. A whole source without a prefix has
/// none, so its tools are enumerated — and so it does not follow new tools.
pub fn to_patterns(selection: &[ThreadMcp], groups: &[ScopeGroup], advanced: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for g in groups {
        let Some(sel) = selection.iter().find(|s| s.server_label == g.label) else {
            continue;
        };
        match &sel.allowed_tools {
            None => match g.whole_glob() {
                Some(w) => lines.push(w),
                None => lines.extend(g.names().map(str::to_string)),
            },
            Some(list) => lines.extend(
                g.names()
                    .filter(|n| list.iter().any(|l| l == n))
                    .map(str::to_string),
            ),
        }
    }
    lines.extend(
        advanced
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string),
    );
    lines.join("\n")
}

/// What one group's tool list is right now.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupTools {
    Loading,
    Failed(String),
    /// Nothing to tick, and why.
    Empty(String),
    List(Vec<ToolItem>),
}

/// One source's row: attach/detach by its label, plus the per-tool ticks once
/// attached.
///
/// `collapse` decides what ticking every tool means. The Chat collapses it to
/// the whole source (`None`), so a thread follows the toolset as it gains
/// tools. The key editor does not: a list of every tool typed out by hand is
/// not the same rule as `<prefix>__*`, and reading it back must not rewrite
/// it.
#[component]
pub fn ToolGroupPick(
    label: String,
    /// The count or status next to the label.
    badge: String,
    /// Why the source offers nothing, shown dim beside it.
    #[prop(optional, into)]
    reason: String,
    /// A line under the ticks once attached.
    #[prop(optional, into)]
    note: String,
    tools: Signal<GroupTools>,
    selection: Selection,
    #[prop(default = true)] collapse: bool,
    #[prop(into, default = false.into())] disabled: Signal<bool>,
) -> impl IntoView {
    let attached = {
        let label = label.clone();
        Memo::new(move |_| selection.list.get().iter().any(|p| p.server_label == label))
    };
    let toggle = {
        let label = label.clone();
        move |on: bool| {
            let mut list = selection.list.get_untracked();
            list.retain(|p| p.server_label != label);
            if on {
                list.push(ThreadMcp {
                    server_label: label.clone(),
                    allowed_tools: None,
                });
            }
            selection.set.run(list);
        }
    };
    let has_reason = !reason.is_empty();
    let label_for_tools = label.clone();
    view! {
        <div class="mcp-pick">
            <label class="row dim" style="gap:6px">
                <input
                    type="checkbox"
                    prop:checked=move || attached.get()
                    disabled=move || disabled.get()
                    on:change=move |ev| toggle(event_target_checked(&ev))
                />
                <span class="mono">{label.clone()}</span>
                <span class="type-badge">{badge}</span>
                <Show when=move || has_reason>
                    <span class="dim">{reason.clone()}</span>
                </Show>
            </label>
            <Show when=move || attached.get()>
                {
                    let label = label_for_tools.clone();
                    let note = Some(note.clone()).filter(|n| !n.is_empty());
                    move || match tools.get() {
                        GroupTools::List(rows) if !rows.is_empty() => {
                            let all: Vec<String> = rows.iter().map(|t| t.name.clone()).collect();
                            let label = label.clone();
                            view! {
                                <div class="mcp-tools">
                                    <For each=move || rows.clone() key=|t| t.name.clone() let:tool>
                                        <ToolPick
                                            name=tool.name.clone()
                                            description=tool.description.clone()
                                            all=all.clone()
                                            label=label.clone()
                                            selection=selection
                                            collapse=collapse
                                            disabled=disabled
                                        />
                                    </For>
                                </div>
                                {note.clone().map(|n| view! { <div class="mcp-tools dim">{n}</div> })}
                            }
                                .into_any()
                        }
                        GroupTools::List(_) => {
                            view! { <div class="mcp-tools dim">"No tools are offered right now."</div> }
                                .into_any()
                        }
                        GroupTools::Empty(why) => {
                            view! { <div class="mcp-tools dim">{why}</div> }.into_any()
                        }
                        GroupTools::Failed(e) => {
                            view! { <div class="mcp-tools dim">"Tools unavailable: " {e}</div> }
                                .into_any()
                        }
                        GroupTools::Loading => {
                            view! { <div class="mcp-tools dim">"Loading tools…"</div> }.into_any()
                        }
                    }
                }
            </Show>
        </div>
    }
}

/// One tool's tick inside an attached source.
#[component]
fn ToolPick(
    name: String,
    description: Option<String>,
    all: Vec<String>,
    label: String,
    selection: Selection,
    collapse: bool,
    disabled: Signal<bool>,
) -> impl IntoView {
    let checked = {
        let (name, label) = (name.clone(), label.clone());
        Memo::new(move |_| {
            selection.list.get().iter().any(|p| {
                p.server_label == label
                    && p.allowed_tools
                        .as_ref()
                        .is_none_or(|a| a.iter().any(|t| t == &name))
            })
        })
    };
    let toggle = {
        let (name, label, all) = (name.clone(), label.clone(), all.clone());
        move |on: bool| {
            let mut list = selection.list.get_untracked();
            let Some(entry) = list.iter_mut().find(|p| p.server_label == label) else {
                return;
            };
            // `None` means the whole surface, so the first untick has to be
            // written out as "everything except this one".
            let mut allowed = entry.allowed_tools.clone().unwrap_or_else(|| all.clone());
            if on {
                if !allowed.iter().any(|t| t == &name) {
                    allowed.push(name.clone());
                }
            } else {
                allowed.retain(|t| t != &name);
            }
            if allowed.is_empty() {
                // A source narrowed to nothing would resolve to nothing;
                // detaching says the same thing in a way the send can act on.
                list.retain(|p| p.server_label != label);
            } else if collapse && all.iter().all(|t| allowed.iter().any(|a| a == t)) {
                entry.allowed_tools = None;
            } else {
                entry.allowed_tools = Some(allowed);
            }
            selection.set.run(list);
        }
    };
    let title = description.unwrap_or_default();
    view! {
        <label class="row dim" style="gap:6px" title=title>
            <input
                type="checkbox"
                prop:checked=move || checked.get()
                disabled=move || disabled.get()
                on:change=move |ev| toggle(event_target_checked(&ev))
            />
            <span class="mono">{name}</span>
        </label>
    }
}

/// The label the gateway resolves a thread's server by: its tool prefix, or
/// its name when it has none (`mcp::exec::server_label`).
fn server_label(s: &McpServerView) -> String {
    if s.tool_prefix.trim().is_empty() {
        s.name.clone()
    } else {
        s.tool_prefix.trim().to_string()
    }
}

/// The key editor's picker body: every `/mcp` source as a row over the
/// [`scope_groups`] the caller derived. A source without a tool prefix gets a
/// note that new tools will not be included automatically.
#[component]
pub fn ScopeToolPicker(
    groups: Signal<Vec<ScopeGroup>>,
    selection: Selection,
    #[prop(into, default = false.into())] disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        {move || {
            let gs = groups.get();
            if gs.is_empty() {
                return view! { <div class="mini-note dim">"No tools are offered on /mcp right now."</div> }
                    .into_any();
            }
            view! {
                <For each=move || gs.clone() key=|g| (g.label.clone(), g.tools.len()) let:g>
                    {
                        let note = if g.prefix.is_none() {
                            "No tool prefix: ticking the whole source lists its current tools, \
                             so tools it gains later are not included automatically."
                                .to_string()
                        } else {
                            String::new()
                        };
                        let tools = Signal::derive({
                            let rows = g.tools.clone();
                            move || GroupTools::List(rows.clone())
                        });
                        view! {
                            <ToolGroupPick
                                label=g.label.clone()
                                badge=format!("{} tools", g.tools.len())
                                reason=g.reason.clone().unwrap_or_default()
                                note=note
                                tools=tools
                                selection=selection
                                collapse=false
                                disabled=disabled
                            />
                        }
                    }
                </For>
            }
                .into_any()
        }}
    }
}

/// The Chat's picker: which built-in toolsets and registered servers a thread
/// attaches, and optionally which of their tools.
///
/// Per thread rather than "everything registered, always": each attached tool
/// is schema and description in every prompt of that conversation, so a chat
/// about code should not carry a mail server's surface. Unticking individual
/// tools writes an `allowed_tools` list; leaving them all ticked keeps it
/// `None`, so the thread follows the toolset as it gains tools.
///
/// The gateway's own built-in toolsets (`lmgw` self-admin, `docs` quickdoc,
/// and whatever is added later) attach exactly like a registered server —
/// same label, same per-tool ticks — and are never on unless a thread asks
/// for them.
#[component]
pub fn McpPicker(picked: RwSignal<Vec<ThreadMcp>>) -> impl IntoView {
    let servers = LocalResource::new(|| crate::api::get::<McpServersResponse>("/api/mcp-servers"));
    let inventory = LocalResource::new(|| crate::api::get::<ToolInventory>("/api/tools"));
    let selection = Selection::of(picked);
    view! {
        <div class="field">
            <label>"Built-in toolsets"</label>
            {move || match inventory.get() {
                None => view! { <div class="dim">"Loading…"</div> }.into_any(),
                Some(Err(e)) => {
                    view! { <div class="dim">"Failed to load: " {e.to_string()}</div> }.into_any()
                }
                Some(Ok(r)) => {
                    let groups: Vec<(String, String, String, Vec<ToolItem>)> = r
                        .sources
                        .iter()
                        .filter(|s| s.kind == "builtin")
                        .map(|s| {
                            // Only the tools the gateway is actually offering: a
                            // tool the owner switched off is not something a
                            // thread can opt back in to, and a tick the send
                            // would then refuse would be a lie. The source's
                            // own reason is shown instead when it offers nothing.
                            let tools = r
                                .tools
                                .iter()
                                .filter(|t| !t.stale && t.source_label == s.label && t.available)
                                .map(|t| ToolItem {
                                    name: t.name.clone(),
                                    description: t.description.clone(),
                                })
                                .collect::<Vec<_>>();
                            (
                                s.label.clone(),
                                format!("{} tools", tools.len()),
                                s.reason.clone().unwrap_or_default(),
                                tools,
                            )
                        })
                        .collect();
                    view! {
                        <For
                            each=move || groups.clone()
                            key=|(label, _, _, tools)| (label.clone(), tools.len())
                            let:group
                        >
                            <ToolGroupPick
                                label=group.0
                                badge=group.1
                                reason=group.2
                                tools=Signal::derive({
                                    let rows = group.3;
                                    move || GroupTools::List(rows.clone())
                                })
                                selection=selection
                            />
                        </For>
                    }
                        .into_any()
                }
            }}
        </div>
        <div class="field">
            <label>"MCP servers"</label>
            {move || match servers.get() {
                None => view! { <div class="dim">"Loading…"</div> }.into_any(),
                Some(Err(e)) => {
                    view! { <div class="dim">"Failed to load: " {e.to_string()}</div> }.into_any()
                }
                Some(Ok(r)) => {
                    let rows: Vec<McpServerView> =
                        r.mcp_servers.iter().filter(|s| s.enabled).cloned().collect();
                    if rows.is_empty() {
                        return view! {
                            <div class="dim">
                                "No enabled MCP servers — register one on the MCP page."
                            </div>
                        }
                            .into_any();
                    }
                    view! {
                        <For each=move || rows.clone() key=|s| s.id let:s>
                            <McpServerPick s=s selection=selection/>
                        </For>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// One registered server's row in the Chat's picker.
#[component]
fn McpServerPick(s: McpServerView, selection: Selection) -> impl IntoView {
    let label = server_label(&s);
    let (id, status, count) = (s.id, s.status.clone(), s.tool_count);
    let attached = {
        let label = label.clone();
        Memo::new(move |_| selection.list.get().iter().any(|p| p.server_label == label))
    };
    // Only fetched once the server is attached: listing connects a stdio
    // server (it is how `tools/list` works), and opening thread settings
    // should not spawn every registered server's process.
    let fetched = LocalResource::new(move || async move {
        if !attached.get() {
            return Ok(lmgw_api_types::McpServerToolsResponse::default());
        }
        crate::api::get::<lmgw_api_types::McpServerToolsResponse>(format!(
            "/api/mcp-servers/{id}/tools"
        ))
        .await
    });
    let tools = Signal::derive(move || match fetched.get() {
        Some(Ok(r)) if !r.tools.is_empty() => GroupTools::List(
            r.tools
                .iter()
                .map(|t| ToolItem {
                    name: t.name.clone(),
                    description: t.description.clone(),
                })
                .collect(),
        ),
        Some(Ok(_)) => GroupTools::Empty("No tools — the server has not connected yet.".into()),
        Some(Err(e)) => GroupTools::Failed(e.to_string()),
        None => GroupTools::Loading,
    });
    let badge = if status == "ready" {
        format!("{count} tools")
    } else {
        status
    };
    view! { <ToolGroupPick label=label badge=badge tools=tools selection=selection/> }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(n: &str) -> ToolItem {
        ToolItem {
            name: n.into(),
            description: None,
        }
    }

    fn groups() -> Vec<ScopeGroup> {
        vec![
            ScopeGroup {
                label: "docs".into(),
                prefix: Some("docs".into()),
                tools: vec![item("docs__query"), item("docs__list")],
                reason: None,
            },
            ScopeGroup {
                label: "gh".into(),
                prefix: Some("gh".into()),
                tools: vec![item("gh__search"), item("gh__open")],
                reason: None,
            },
            ScopeGroup {
                label: "bare".into(),
                prefix: None,
                tools: vec![item("ping"), item("pong")],
                reason: None,
            },
        ]
    }

    fn sel(label: &str, allowed: Option<&[&str]>) -> ThreadMcp {
        ThreadMcp {
            server_label: label.into(),
            allowed_tools: allowed.map(|a| a.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn a_whole_prefixed_source_is_its_glob_and_back() {
        let g = groups();
        let s = vec![sel("docs", None)];
        assert_eq!(to_patterns(&s, &g, ""), "docs__*");
        assert_eq!(from_patterns("docs__*", &g), (s, String::new()));
    }

    #[test]
    fn single_tools_are_exact_names_in_group_order() {
        let g = groups();
        let (s, adv) = from_patterns("gh__open\ngh__search", &g);
        assert_eq!(s, vec![sel("gh", Some(&["gh__search", "gh__open"]))]);
        assert_eq!(adv, "");
        assert_eq!(to_patterns(&s, &g, ""), "gh__search\ngh__open");
    }

    #[test]
    fn every_tool_ticked_by_hand_stays_a_list_not_a_glob() {
        let g = groups();
        let (s, _) = from_patterns("docs__query\ndocs__list", &g);
        assert_eq!(s, vec![sel("docs", Some(&["docs__query", "docs__list"]))]);
        assert_eq!(to_patterns(&s, &g, ""), "docs__query\ndocs__list");
    }

    #[test]
    fn a_source_without_a_prefix_enumerates_its_tools() {
        let g = groups();
        assert_eq!(to_patterns(&[sel("bare", None)], &g, ""), "ping\npong");
        let (s, adv) = from_patterns("ping\npong", &g);
        assert_eq!(s, vec![sel("bare", Some(&["ping", "pong"]))]);
        assert_eq!(adv, "");
    }

    #[test]
    fn what_the_picker_cannot_represent_is_kept_as_advanced() {
        let g = groups();
        let (s, adv) = from_patterns("git*\ndocs__*\n kb__*\n\nnope", &g);
        assert_eq!(s, vec![sel("docs", None)]);
        assert_eq!(adv, "git*\nkb__*\nnope");
        assert_eq!(to_patterns(&s, &g, &adv), "docs__*\ngit*\nkb__*\nnope");
    }

    #[test]
    fn matching_ignores_case_like_the_gate() {
        let g = groups();
        let (s, adv) = from_patterns("DOCS__*\nGH__Search", &g);
        assert_eq!(s, vec![sel("docs", None), sel("gh", Some(&["gh__search"]))]);
        assert_eq!(adv, "");
    }

    #[test]
    fn a_whole_glob_wins_over_exact_names_of_the_same_source() {
        let g = groups();
        let (s, _) = from_patterns("docs__query\ndocs__*", &g);
        assert_eq!(s, vec![sel("docs", None)]);
    }

    #[test]
    fn prefixes_come_from_names_and_servers_not_labels() {
        use lmgw_api_types::{ToolEntryView, ToolSourceView};
        let src = |label: &str, kind: &str, id: Option<i64>, plane: &str| ToolSourceView {
            label: label.into(),
            name: label.into(),
            kind: kind.into(),
            server_id: id,
            plane: plane.into(),
            available: true,
            ..Default::default()
        };
        let tool = |n: &str, label: &str, plane: &str, stale: bool| ToolEntryView {
            name: n.into(),
            source_label: label.into(),
            plane: plane.into(),
            available: true,
            stale,
            ..Default::default()
        };
        let inv = ToolInventory {
            sources: vec![
                src("lmgw", "builtin", None, "/mcp/admin"),
                src("docs", "builtin", None, "/mcp"),
                src("tools", "server", Some(7), "/mcp"),
            ],
            tools: vec![
                tool("lmgw__status", "lmgw", "/mcp/admin", false),
                tool("docs__query", "docs", "/mcp", false),
                tool("old__gone", "docs", "", true),
                // Label "tools" is the *name* of a server with no prefix.
                tool("a__b", "tools", "/mcp", false),
                tool("c", "tools", "/mcp", false),
            ],
        };
        let server = |prefix: &str| McpServerView {
            id: 7,
            name: "tools".into(),
            tool_prefix: prefix.into(),
            ..Default::default()
        };
        let g = scope_groups(&inv, &[server("")]);
        assert_eq!(g.len(), 2, "the self-admin source is never offered");
        assert_eq!(g[0].prefix.as_deref(), Some("docs"));
        assert_eq!(g[0].tools.len(), 1, "stale records are excluded");
        assert_eq!(g[1].prefix, None);
        // A registered prefix its tools do not carry is no namespace.
        assert_eq!(scope_groups(&inv, &[server("x")])[1].prefix, None);
    }
}
