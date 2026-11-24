//! Nothing selected: what the catalog holds, by run kind, and the ways to
//! add an agent — the empty catalog's explanation included.

use leptos::prelude::*;

use lmgw_api_types::AgentCard;

use super::list::KINDS;
use super::AgentsCtx;
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{ActionRow, Ico, Icon, PrefGroup};

/// The catalog by run kind, in the list's order: each kind's heading, how
/// many rows it has and what is off or in the way among them. Rows of no
/// kind this build knows (an unreadable manifest says none) count too, under
/// the list's own heading for them.
fn summary_of(cs: &[AgentCard]) -> Vec<(&'static str, usize, String)> {
    let known = |c: &AgentCard| KINDS.iter().any(|(k, _, _)| *k == c.kind);
    let groups: Vec<(&'static str, Vec<&AgentCard>)> = KINDS
        .iter()
        .map(|(kind, heading, _)| (*heading, cs.iter().filter(|c| c.kind == *kind).collect()))
        .chain(std::iter::once((
            super::list::kind_heading(""),
            cs.iter().filter(|c| !known(c)).collect(),
        )))
        .collect();
    groups
        .into_iter()
        .filter(|(_, mine)| !mine.is_empty())
        .map(|(heading, mine)| {
            let unreadable = mine.iter().filter(|c| c.error.is_some()).count();
            let off = mine
                .iter()
                .filter(|c| c.error.is_none() && !c.enabled)
                .count();
            let gaps = mine
                .iter()
                .filter(|c| {
                    c.error.is_none() && c.enabled && (!c.requires_ok || !c.warnings.is_empty())
                })
                .count();
            let mut parts = Vec::new();
            if unreadable > 0 && unreadable < mine.len() {
                parts.push(format!("{unreadable} not readable"));
            }
            if off > 0 {
                parts.push(format!("{off} disabled"));
            }
            if gaps > 0 {
                parts.push(format!("{gaps} with something in the way of a run"));
            }
            (heading, mine.len(), parts.join(" · "))
        })
        .collect()
}

#[component]
pub(super) fn StatusPage(page: AgentsCtx, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    let summary = move || {
        page.cards
            .with(|cs| cs.as_deref().map(summary_of).unwrap_or_default())
    };
    let empty = move || page.cards.with(|c| c.as_ref().is_some_and(Vec::is_empty));
    view! {
        <DetailHead narrow=narrow back=back title="Agents" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow astatus">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Bot/>
                    </div>
                    <div class="hero-text">
                        <h1>"Agents"</h1>
                        <div class="sub">
                            "A manifest each: the model, the prompts, the tools and a config form. "
                            "Pick one in the list, or add one."
                        </div>
                    </div>
                </div>
                <PrefGroup title="In the list">
                    <Show
                        when=move || !empty()
                        fallback=|| {
                            view! {
                                <div class="prow notice-row">
                                    "No agents yet. Add one below, or put back the ones lmgw ships with. "
                                    "An agent reaches its tools through MCP servers."
                                </div>
                            }
                        }
                    >
                        {move || {
                            summary()
                                .into_iter()
                                .map(|(label, n, note)| {
                                    view! {
                                        <div class="prow">
                                            <div class="prow-text">
                                                <span class="prow-title">{label}</span>
                                                {(!note.is_empty())
                                                    .then(|| view! { <span class="prow-sub">{note}</span> })}
                                            </div>
                                            <span class="prow-value">{n}</span>
                                        </div>
                                    }
                                })
                                .collect_view()
                        }}
                    </Show>
                    <a class="prow action" href="/mcp-servers">
                        <div class="prow-text">
                            <span class="prow-title">"MCP servers"</span>
                            <span class="prow-sub">"Where an agent's tools come from"</span>
                        </div>
                        <Icon ico=Ico::Right class="prow-chevron"/>
                    </a>
                </PrefGroup>
                <PrefGroup title="Add">
                    <ActionRow
                        title="New agent"
                        sub="Writes the smallest valid manifest, a chat agent with a model picker, and opens it"
                        chevron=true
                        on_click=Callback::new(move |()| page.new_open.set(true))
                    />
                    <ActionRow
                        title="Import a manifest"
                        sub="An .agent.json someone exported from this or another gateway, as a file or pasted"
                        chevron=true
                        on_click=Callback::new(move |()| page.import_open.set(true))
                    />
                    <ActionRow
                        title="Install from an image"
                        sub="An agent package: an OCI image carrying its manifest at /lmgw/agent.json"
                        chevron=true
                        on_click=Callback::new(move |()| page.install_open.set(true))
                    />
                    <ActionRow
                        title="Restore shipped agents"
                        sub="Puts the agents lmgw ships with back, the ones deleted included"
                        on_click=Callback::new(move |()| page.restore.run(()))
                    >
                        <Show when=move || page.restoring.get()>
                            <span class="prow-value">"Restoring…"</span>
                        </Show>
                    </ActionRow>
                </PrefGroup>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, kind: &str) -> AgentCard {
        AgentCard {
            id: id.into(),
            kind: kind.into(),
            enabled: true,
            requires_ok: true,
            ..Default::default()
        }
    }

    /// Every row is counted somewhere: one whose manifest this build cannot
    /// read has no kind, and shows under the list's heading for it.
    #[test]
    fn unreadable_rows_count_in_the_list() {
        let mut odd = card("future", "");
        odd.error = Some("schema_version 9".into());
        let mut off = card("mail", "batch");
        off.enabled = false;
        let cs = vec![card("chat-a", "chat"), off, odd, card("next", "pipeline")];
        let got = summary_of(&cs);
        assert_eq!(
            got,
            vec![
                ("Chat agents", 1, String::new()),
                ("Batch agents", 1, "1 disabled".to_string()),
                ("Not readable", 2, "1 not readable".to_string()),
            ]
        );
        let total: usize = got.iter().map(|(_, n, _)| n).sum();
        assert_eq!(total, cs.len());
    }
}
