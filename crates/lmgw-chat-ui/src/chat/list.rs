//! The thread list: dated groups, the filter, folders and pins, and the
//! list's view.

use super::*;

// ---------------------------------------------------------------------------
// Thread list
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Days since 1970-01-01 of a `YYYY-MM-DD` day (proleptic Gregorian — Howard
/// Hinnant's `days_from_civil`), so "yesterday" and "this week" are a
/// subtraction rather than a date library.
fn day_number(day: &str) -> Option<i64> {
    let y: i64 = day.get(0..4)?.parse().ok()?;
    let m: i64 = day.get(5..7)?.parse().ok()?;
    let d: i64 = day.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The heading a thread files under: its local day against today's.
fn day_group(day: &str, today: &str) -> String {
    let (Some(d), Some(t)) = (day_number(day), day_number(today)) else {
        return "Undated".to_string();
    };
    match t - d {
        // A clock a little ahead of the server's is still today.
        n if n <= 0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        n if n < 7 => "Previous 7 days".to_string(),
        _ => {
            let m: usize = day[5..7].parse().unwrap_or(1);
            format!("{} {}", MONTHS[(m - 1).min(11)], &day[0..4])
        }
    }
}

/// Today in the reader's own time, `YYYY-MM-DD`.
pub(crate) fn today_local() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:04}-{:02}-{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date()
    )
}

/// A stored `YYYY-MM-DD HH:MM:SS` (SQLite `datetime('now')`, UTC) as seconds
/// since the epoch — the same parsing [`lmgw_ui_kit::fmt::log_time`] does, needed
/// here as a number rather than a rendered string so [`lmgw_ui_kit::fmt::rel_time_at`]
/// can turn a future `purge_at` into "in 12d".
pub(crate) fn parse_utc_ts(ts: &str) -> Option<f64> {
    let zoned = ts.ends_with('Z') || ts.get(10..).is_some_and(|t| t.contains('+'));
    let iso = if zoned {
        ts.replacen(' ', "T", 1)
    } else {
        format!("{}Z", ts.get(0..19).unwrap_or(ts).replacen(' ', "T", 1))
    };
    let d = js_sys::Date::new(&JsValue::from_str(&iso));
    let t = d.get_time();
    (!t.is_nan()).then_some(t / 1000.0)
}

/// What an archived row says about its fate: "deleted in 12d" from
/// `purge_at`, verbatim when it will not parse rather than nothing. A
/// `purge_at` already in the past means the hourly sweep just has not run
/// yet (review nit: this used to read the confusing "deleted 2h ago" for a
/// thread that has not actually been deleted).
pub(crate) fn purge_text(purge_at: &str) -> String {
    let Some(epoch) = parse_utc_ts(purge_at) else {
        return format!("deleted {purge_at}");
    };
    let now = js_sys::Date::now() / 1000.0;
    if epoch <= now {
        return "deleted at the next sweep".to_string();
    }
    format!("deleted {}", lmgw_ui_kit::fmt::rel_time_at(epoch, now))
}

/// One line of the thread list: a date heading with its count, or a thread.
#[derive(Clone, PartialEq)]
// Short-lived render rows; boxing `Row` would ripple through every match arm for no gain.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ListItem {
    Head(String, usize),
    /// A folder's header (chat-complete §5); its threads follow as nested rows.
    Folder(FolderRow),
    Row {
        thread: ChatThread,
        /// Listed under its folder's header, so indented.
        nested: bool,
        /// The folder's name, shown in the flat archived view.
        folder: Option<String>,
        /// "14:32" under Today and Yesterday, "Thu 24 Sep" further back.
        when: String,
        /// The local date and time, and what was stored (UTC).
        when_title: String,
    },
}

impl ListItem {
    /// This row, listed under its folder.
    pub(crate) fn nested(mut self) -> Self {
        if let ListItem::Row { nested, .. } = &mut self {
            *nested = true;
        }
        self
    }

    /// This row, naming its folder.
    fn in_folder(mut self, name: Option<&str>) -> Self {
        if let ListItem::Row { folder, .. } = &mut self {
            *folder = name.map(str::to_string);
        }
        self
    }

    /// Identity plus everything it shows: a keyed list never updates an
    /// entry in place — pin and archive state included, so toggling either
    /// swaps the row instead of leaving its mark stale.
    fn key(&self) -> String {
        match self {
            ListItem::Head(label, n) => format!("h:{label}:{n}"),
            ListItem::Folder(f) => f.key(),
            ListItem::Row {
                thread: t,
                when,
                nested,
                folder,
                ..
            } => format!(
                "t:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                t.id,
                t.title,
                t.model_alias,
                t.kind,
                t.agent_id.as_deref().unwrap_or(""),
                when,
                t.pinned,
                t.archived_at.as_deref().unwrap_or(""),
                t.purge_at.as_deref().unwrap_or(""),
                nested,
                folder.as_deref().unwrap_or(""),
            ),
        }
    }
}

/// Every word of `query` in the title or the model — shared by the pinned,
/// dated and archived buckets below.
pub(crate) fn matches_query(t: &ChatThread, words: &[String]) -> bool {
    matches_query_in(t, None, words)
}

/// [`matches_query`], with the name of the thread's folder counting as part
/// of its text: filtering by a folder's name finds what is inside it.
pub(crate) fn matches_query_in(t: &ChatThread, folder: Option<&str>, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {}",
        t.title,
        t.model_alias,
        folder.unwrap_or_default()
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// A thread as a row: "14:32" under Today and Yesterday, "Thu 24 Sep"
/// further back — its last-active time either way, pinned or not.
pub(crate) fn thread_row(t: &ChatThread, today: &str) -> ListItem {
    let lt = lmgw_ui_kit::fmt::log_time(&t.updated_at);
    let group = day_group(&lt.day, today);
    let recent = group == "Today" || group == "Yesterday";
    ListItem::Row {
        when: if recent {
            lt.time.get(0..5).unwrap_or(&lt.time).to_string()
        } else {
            lt.day_label.clone()
        },
        when_title: format!("{} {} · {}", lt.day, lt.time, lt.utc),
        thread: t.clone(),
        nested: false,
        folder: None,
    }
}

/// The threads that match `query` (every word, over title, model and folder
/// name).
///
/// The active view: the Folders block first (each folder with its threads,
/// collapsed ones as a header only — [`chat_folders::folder_block`]); then a
/// "Pinned" group of the pinned threads *without* a folder (pinning never
/// touches `updated_at`, so pinned rows keep the server's own order rather
/// than being re-bucketed by day); then the rest under date headings, newest
/// first, as the server lists them. A thread is listed exactly once: one in a
/// folder is not repeated under Pinned or a date.
///
/// The archived view (`archived`) is flat — no date headings, no folder
/// blocks, each row naming its folder. Archived rows are ordered by
/// `archived_at`, which does not correlate with `updated_at` (the day a row's
/// last message landed), so bucketing them by day would scatter the same day
/// across the list instead of grouping it.
pub(crate) fn thread_items(
    threads: &[ChatThread],
    folders: &[FolderInfo],
    collapsed: &std::collections::HashSet<i64>,
    query: &str,
    today: &str,
    archived: bool,
) -> Vec<ListItem> {
    group_threads(
        threads,
        folders,
        collapsed,
        query,
        archived,
        &|t| thread_row(t, today),
        &|t| day_group(&lmgw_ui_kit::fmt::log_time(&t.updated_at).day, today),
    )
}

/// [`thread_items`] with the two things that read the clock — a thread's row
/// and its date heading — passed in, so the grouping itself is plain logic.
pub(crate) fn group_threads(
    threads: &[ChatThread],
    folders: &[FolderInfo],
    collapsed: &std::collections::HashSet<i64>,
    query: &str,
    archived: bool,
    row: &dyn Fn(&ChatThread) -> ListItem,
    date_group: &dyn Fn(&ChatThread) -> String,
) -> Vec<ListItem> {
    let words = lmgw_ui_kit::widgets::filter_words(query);
    let mut out: Vec<ListItem> = Vec::new();
    let name_of = |t: &ChatThread| chat_folders::folder_name(t, folders);

    if archived {
        out.extend(
            threads
                .iter()
                .filter(|t| matches_query_in(t, name_of(t), &words))
                .map(|t| row(t).in_folder(name_of(t))),
        );
        return out;
    }

    out.extend(chat_folders::folder_block(
        threads, folders, collapsed, &words, row,
    ));
    // What the folder block did not list.
    let unfiled = |t: &&ChatThread| name_of(t).is_none();

    let pinned: Vec<&ChatThread> = threads
        .iter()
        .filter(|t| t.pinned && matches_query(t, &words))
        .filter(unfiled)
        .collect();
    if !pinned.is_empty() {
        out.push(ListItem::Head("Pinned".to_string(), pinned.len()));
        out.extend(pinned.into_iter().map(row));
    }

    let mut head_at: Option<usize> = None;
    for t in threads.iter().filter(|t| !t.pinned).filter(unfiled) {
        if !matches_query(t, &words) {
            continue;
        }
        let group = date_group(t);
        let open_new = match head_at.and_then(|i| out.get(i)) {
            Some(ListItem::Head(label, _)) => *label != group,
            _ => true,
        };
        if open_new {
            head_at = Some(out.len());
            out.push(ListItem::Head(group, 0));
        }
        if let Some(ListItem::Head(_, n)) = head_at.and_then(|i| out.get_mut(i)) {
            *n += 1;
        }
        out.push(row(t));
    }
    out
}

/// The conversations, filterable, under date headings, each with when it was
/// last active and the model it talks to.
#[component]
pub(crate) fn ThreadList(
    items: Memo<Vec<ListItem>>,
    /// The list's read: loading, failed (with why), or in.
    #[prop(into)]
    state: Signal<Option<Result<(), String>>>,
    retry: Callback<()>,
    #[prop(into)] total: Signal<usize>,
    query: RwSignal<String>,
    current: Memo<Option<i64>>,
    on_open: Callback<i64>,
    on_new: Callback<&'static str>,
    /// What New chat says it does: "New chat in <folder>" while a thread of
    /// a folder is open (chat-plane design §9.10).
    #[prop(into)]
    new_label: Signal<String>,
    on_delete: Callback<i64>,
    on_pin: Callback<(i64, bool)>,
    on_archive: Callback<(i64, bool)>,
    renaming: RwSignal<Option<i64>>,
    on_rename: Callback<(i64, String)>,
    view_archived: RwSignal<bool>,
    #[prop(into)] archived_count: Signal<i64>,
    folders: FolderEnv,
    on_pick: Callback<(i64, Option<i64>)>,
) -> impl IntoView {
    let saver = crate::chat_bytes::Saver::new();
    let filtering = move || !query.with(|q| q.trim().is_empty());
    let q_box: NodeRef<leptos::html::Input> = NodeRef::new();
    use_slash_focus(q_box);
    view! {
        <div class="density-dense">
            <div class="thread-tools">
                <div class="row">
                    <button
                        class="btn"
                        title=move || new_label.get()
                        aria-label=move || new_label.get()
                        on:click=move |_| on_new.run("chat")
                    >
                        "New chat"
                    </button>
                    <button
                        class="btn ghost"
                        title="New Admin Chat — wired to the lmgw self-admin tools"
                        on:click=move |_| on_new.run("admin")
                    >
                        "Admin chat"
                    </button>
                </div>
                <button
                    type="button"
                    class="btn ghost sm"
                    title="New temporary chat — kept in memory only, discarded when you leave it"
                    on:click=move |_| on_new.run("temporary")
                >
                    "New temporary chat"
                </button>
                <button
                    type="button"
                    class="btn ghost sm"
                    title="New folder — group conversations, with settings new chats in it start from"
                    on:click=move |_| folders.create()
                >
                    "New folder"
                </button>
                <button
                    type="button"
                    class="btn ghost sm"
                    class:active=move || view_archived.get()
                    title=move || {
                        if view_archived.get() {
                            "Back to conversations"
                        } else {
                            "Threads the auto-archive sweep (or a manual Archive) filed away"
                        }
                    }
                    on:click=move |_| view_archived.update(|v| *v = !*v)
                >
                    {move || {
                        if view_archived.get() {
                            "Active conversations".to_string()
                        } else {
                            format!("Archived ({})", archived_count.get())
                        }
                    }}
                </button>
                // The list's own ⋯ (export every chat) ends the filter's line:
                // beside New chat and Admin chat it took the room the panel's
                // floor does not have, and widened the list (a sideways bar).
                <div class="thread-filter">
                    <input
                        class="input thread-q"
                        type="search"
                        node_ref=q_box
                        data-slash
                        placeholder="Filter by title or model"
                        title="Filter · / focuses, Esc clears"
                        autocomplete="off"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Escape" && filtering() {
                                ev.stop_propagation();
                                query.set(String::new());
                            }
                        }
                    />
                    <crate::chat_export::ExportAll/>
                </div>
            </div>
            <div class="thread-list">
                <Show when=move || filtering() && items.with(Vec::is_empty)>
                    <div class="empty">
                        {move || format!("None of {} conversations match.", total.get())}
                        " "
                        <button class="link-btn" on:click=move |_| query.set(String::new())>
                            "Clear"
                        </button>
                    </div>
                </Show>
                {move || match state.get() {
                    None => view! { <div class="empty">"Loading…"</div> }.into_any(),
                    Some(Err(e)) => {
                        view! {
                            <div class="empty status-err">
                                "Could not read the conversations: " {e} " "
                                <button class="link-btn" on:click=move |_| retry.run(())>
                                    "Retry"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                    Some(Ok(())) => ().into_any(),
                }}
                <Show when=move || {
                    !filtering() && total.get() == 0 && matches!(state.get(), Some(Ok(())))
                }>
                    <div class="empty">"No conversations yet."</div>
                </Show>
                <For each=move || items.get() key=ListItem::key let:item>
                    {match item {
                        ListItem::Head(label, n) => {
                            view! {
                                <div class="thread-group">
                                    {label}
                                    <span class="count">{n}</span>
                                </div>
                            }
                                .into_any()
                        }
                        ListItem::Folder(row) => {
                            view! { <FolderHeader row=row env=folders/> }.into_any()
                        }
                        ListItem::Row { thread: t, when, when_title, nested, folder } => {
                            let id = t.id;
                            let is_open = move || current.get() == Some(id);
                            // The name, not the upstream's prefix: the row
                            // is narrow, and the full id is in the tooltip.
                            let name = t.model_alias.rsplit('/').next().unwrap_or_default();
                            let model = if name.is_empty() {
                                "no model".to_string()
                            } else {
                                name.to_string()
                            };
                            let model_title = t.model_alias.clone();
                            let pinned = t.pinned;
                            let archived = t.archived_at.is_some();
                            let purge = t.purge_at.clone();
                            let temp = t.temporary;
                            // An ongoing folder's current thread: where its
                            // clients continue, marked (chat-plane §9.10).
                            let current_here = Memo::new(move |_| {
                                folders.folders.with(|f| crate::chat_folders::is_current(f, id))
                            });
                            let menu = Signal::derive(move || {
                                vec![
                                    MenuItem::new(
                                        if pinned { "Unpin" } else { "Pin" },
                                        move || on_pin.run((id, !pinned)),
                                    ),
                                    MenuItem::new("Rename", move || renaming.set(Some(id))),
                                    MenuItem::new("Move to…", move || folders.open_move(id)),
                                    MenuItem::new(
                                        if archived { "Restore" } else { "Archive" },
                                        move || on_archive.run((id, !archived)),
                                    ),
                                ]
                                    .into_iter()
                                    .chain(crate::chat_export::thread_items(saver, id))
                                    .chain([MenuItem::new("Delete", move || on_delete.run(id)).danger()])
                                    .collect()
                            });
                            view! {
                                <div
                                    class=move || {
                                        let mut c = String::from("thread-row");
                                        if is_open() {
                                            c.push_str(" open");
                                        }
                                        if nested {
                                            c.push_str(" nested");
                                        }
                                        if current_here.get() {
                                            c.push_str(" ongoing-current");
                                        }
                                        c
                                    }
                                    draggable=(!temp).then_some("true")
                                    on:dragstart=move |ev| {
                                        if let Some(dt) = ev.data_transfer() {
                                            let _ = dt.set_data("text/plain", &id.to_string());
                                            dt.set_effect_allowed("move");
                                        }
                                        folders.dragged.set(Some(id));
                                    }
                                    on:dragend=move |_| folders.dragged.set(None)
                                    role="button"
                                    tabindex="0"
                                    aria-current=move || is_open().then_some("true")
                                    data-dock-pick
                                    data-thread=id.to_string()
                                    title=t.title.clone()
                                    on:click=move |_| on_open.run(id)
                                    on:keydown=move |ev| {
                                        // Not an Enter meant for the row menu inside.
                                        if ev.target() == ev.current_target() {
                                            if ev.key() == "Enter" {
                                                on_open.run(id);
                                            } else if ev.key() == "F2" && !temp {
                                                ev.prevent_default();
                                                renaming.set(Some(id));
                                            }
                                        }
                                    }
                                >
                                    <span class="thread-title">
                                        {pinned
                                            .then(|| {
                                                view! {
                                                    // Monochrome, `currentColor`, like every
                                                    // other icon in the app — the pin emoji
                                                    // rendered in its own colour, out of step
                                                    // with the sidebar's line icons (review nit).
                                                    <span class="pin-mark" title="Pinned">
                                                        <svg viewBox="0 0 16 16">
                                                            <path d="M8 1.6a3.4 3.4 0 0 0-3.4 3.4c0 2.6 3.4 8 3.4 8s3.4-5.4 3.4-8A3.4 3.4 0 0 0 8 1.6z"/>
                                                            <circle cx="8" cy="5" r="1.3"/>
                                                        </svg>
                                                    </span>
                                                }
                                            })}
                                        {
                                            let title = t.title.clone();
                                            move || {
                                                if renaming.get() == Some(id) && !temp {
                                                    view! {
                                                        <crate::chat_rename::RenameBox
                                                            id=id
                                                            start=title.clone()
                                                            renaming=renaming
                                                            on_rename=on_rename
                                                        />
                                                    }
                                                        .into_any()
                                                } else {
                                                    title.clone().into_any()
                                                }
                                            }
                                        }
                                    </span>
                                    <span class="thread-row-menu" on:click=|ev| ev.stop_propagation()>
                                        {if temp {
                                            view! {
                                                <button
                                                    type="button"
                                                    class="thread-x"
                                                    title="Discard this temporary chat"
                                                    aria-label="Discard this temporary chat"
                                                    on:click=move |_| on_delete.run(id)
                                                >
                                                    "✕"
                                                </button>
                                            }
                                                .into_any()
                                        } else {
                                            view! { <RowMenu items=menu title="Conversation actions"/> }
                                                .into_any()
                                        }}
                                    </span>
                                    <span class="thread-meta">
                                        <span title=when_title>{when}</span>
                                        {(t.kind == "admin")
                                            .then(|| {
                                                view! {
                                                    <span
                                                        class="thread-kind"
                                                        title="Admin Chat — the lmgw self-admin tools are attached"
                                                    >
                                                        "admin"
                                                    </span>
                                                }
                                            })}
                                        <Show when=move || current_here.get()>
                                            <span
                                                class="thread-current"
                                                title="The current thread of this ongoing conversation: every client continues here"
                                            >
                                                "current"
                                            </span>
                                        </Show>
                                        <span class="thread-model" title=model_title>
                                            {model}
                                        </span>
                                        {t
                                            .agent_id
                                            .clone()
                                            .map(|a| {
                                                view! {
                                                    <span title="Opened from this agent">
                                                        {format!("· {a}")}
                                                    </span>
                                                }
                                            })}
                                        {folder
                                            .map(|f| {
                                                view! {
                                                    <span class="thread-folder-name" title="Folder">
                                                        {f}
                                                    </span>
                                                }
                                            })}
                                        {archived
                                            .then(|| {
                                                let txt = purge
                                                    .as_deref()
                                                    .map(purge_text)
                                                    .unwrap_or_else(|| "kept".to_string());
                                                view! { <span class="thread-purge">{txt}</span> }
                                            })}
                                    </span>
                                </div>
                            }
                                .into_any()
                        }
                    }}
                </For>
                <NoFolderZone env=folders/>
                <MessageHits query=query folders=folders.folders on_pick=on_pick/>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_numbers_count_days_across_months_and_leap_years() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("2026-09-24"), Some(20_720));
        assert_eq!(
            day_number("2024-03-01").unwrap() - day_number("2024-02-28").unwrap(),
            2
        );
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("garbage"), None);
    }

    #[test]
    fn threads_file_under_today_yesterday_the_week_then_their_month() {
        let today = "2026-09-24";
        assert_eq!(day_group("2026-09-24", today), "Today");
        assert_eq!(day_group("2026-09-25", today), "Today");
        assert_eq!(day_group("2026-09-23", today), "Yesterday");
        assert_eq!(day_group("2026-09-18", today), "Previous 7 days");
        assert_eq!(day_group("2026-09-17", today), "September 2026");
        assert_eq!(day_group("2026-08-31", today), "August 2026");
        assert_eq!(day_group("2025-12-31", "2026-01-02"), "Previous 7 days");
        assert_eq!(day_group("", today), "Undated");
    }
}
