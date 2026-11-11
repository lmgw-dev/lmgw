//! Settings → About: where this lmgw runs and — in the server image — how
//! long its quit can take against what its unit gives it (headless server
//! design D20), the update notice (§6.3, ruling 13) and the database copies
//! made before a migration (ruling 14, §6.4). Read from the settings answer,
//! never part of the draft. The version and the data dir are in the page's
//! subtitle, so not here again.

use leptos::prelude::*;
use lmgw_api_types::units::fmt_bytes;
use lmgw_api_types::{Deployment, MigrationCopy, StopBudget, UpdateNotice};
use serde_json::Value;

use super::{f, Ctl, Def, Page};

/// The category's one row: the panel.
pub(super) const ABOUT: &[Def] = &[f("about", "about", "about", "This lmgw", Ctl::About).terms(
    "about runs as deployment container server image dev instance stop timeout unit budget quit \
     update version newer notice database copy copies backup backups migration restore",
)];

/// What the answer says about this lmgw.
fn read<T: serde::de::DeserializeOwned>(page: Page, key: &str) -> Option<T> {
    page.data
        .with(|d| d.as_ref().map(|v| v[key].clone()))
        .and_then(|v: Value| serde_json::from_value(v).ok())
}

/// What "Runs as" says: the deployment, and whether this is a dev instance
/// (a debug build of the tray app, the headless runner, `lmgw-server` with
/// `LMGW_DEV`) rather than the installed app or the image's own start.
pub(super) fn runs_as(deployment: Deployment, dev: bool) -> &'static str {
    match (deployment, dev) {
        (Deployment::Desktop, false) => "this computer's desktop app",
        (Deployment::Desktop, true) => "a dev instance on this computer",
        (Deployment::Container, false) => "the server image's container",
        (Deployment::Container, true) => "a dev instance in container mode",
    }
}

#[component]
pub(super) fn AboutPanel(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let runs = move || {
        runs_as(
            read::<Deployment>(page, "deployment").unwrap_or_default(),
            read::<bool>(page, "dev").unwrap_or_default(),
        )
    };
    let budget = move || {
        read::<Option<StopBudget>>(page, "stop_budget")
            .flatten()
            .map(|b| {
                let class = if b.over { "notice warn" } else { "dim" };
                view! {
                    <div class="set-line">
                        <span class="set-line-label">"Stop budget"</span>
                    </div>
                    <div class=class>{b.sentence}</div>
                }
            })
    };
    let update = move || {
        read::<Option<UpdateNotice>>(page, "update_notice")
            .flatten()
            .map(|n| {
                view! {
                    <div class="set-line">
                        <span class="set-line-label">"Update"</span>
                    </div>
                    <div class="notice" title=format!("checked {} UTC", n.checked_at)>
                        {n.sentence}
                    </div>
                }
            })
    };
    let copies = move || {
        let container = read::<Deployment>(page, "deployment").unwrap_or_default();
        container.is_container().then(|| {
            let list = read::<Vec<MigrationCopy>>(page, "migration_copies").unwrap_or_default();
            let unread = read::<Option<String>>(page, "migration_copies_error")
                .flatten()
                .map(|e| {
                    view! {
                        <div class="notice warn">
                            {format!("Not every copy can be listed: {e}.")}
                        </div>
                    }
                });
            view! {
                <div class="set-line">
                    <span class="set-line-label">"Copies before migrating"</span>
                </div>
                <div class="dim">{COPIES_NOTE}</div>
                {unread}
                {if list.is_empty() {
                    view! { <div class="dim">"None yet."</div> }.into_any()
                } else {
                    list.into_iter()
                        .map(|c| {
                            view! {
                                <div class="set-line" title=c.path.clone()>
                                    <span class="set-line-label">{c.file.clone()}</span>
                                    <span>{copy_line(&c)}</span>
                                </div>
                            }
                        })
                        .collect_view()
                        .into_any()
                }}
            }
        })
    };
    view! {
        <div class="set-about" id=id hidden=move || hidden.get()>
            <div class="set-line">
                <span class="set-line-label">"Runs as"</span>
                <span>{runs}</span>
            </div>
            {budget}
            {update}
            {copies}
        </div>
    }
}

/// What the copies' rows are, above them.
const COPIES_NOTE: &str = "A start that migrates a database copies it into the data \
     volume's backups folder first: each copy is that database as the start found it, before it \
     changed anything. A start that was refused and finds the database unchanged makes no second \
     copy; one that finds it changed makes the next number. Restoring a copy, with the service \
     stopped, loses everything written since it was made. lmgw never deletes or restores a \
     copy.";

/// One copy's row: its size, when it was made, and the database and
/// schema step it was made before.
pub(super) fn copy_line(c: &MigrationCopy) -> String {
    let made = if c.made_at.is_empty() {
        String::new()
    } else {
        format!(", made {} UTC", c.made_at)
    };
    format!(
        "{}{made}, {} before migration {:04} → {:04}",
        fmt_bytes(c.bytes),
        c.database,
        c.from,
        c.to
    )
}
