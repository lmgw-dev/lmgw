//! Folder settings: one ongoing conversation and the folder's own retention
//! (client-apps design §3.6, §11 Q2, Q3).
//!
//! - **Ongoing conversation**: clients continue the folder's current thread;
//!   a new one starts after the idle minutes shown here (0: only when
//!   asked, the folder menu's "New conversation"). It needs the model.
//! - **Also apply to the current thread**: a defaults change reaches the
//!   conversation's thread too; checked by default.
//! - **Retention**: days idle before the sweep archives a thread, days
//!   archived before it deletes one; empty is the global setting, named in
//!   the placeholder with its value.

use leptos::prelude::*;
use serde_json::{json, Map, Value};

use super::FolderInfo;

/// The form's ongoing and retention fields, as typed, and as the folder
/// had them when the form opened.
#[derive(Clone, Copy)]
pub(super) struct OngoingDraft {
    pub on: RwSignal<bool>,
    idle: RwSignal<String>,
    pub apply: RwSignal<bool>,
    archive: RwSignal<String>,
    purge: RwSignal<String>,
    seeded: Seeded,
}

/// The fields as the folder had them when the form opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seeded {
    /// `None`: not ongoing; else its idle minutes.
    ongoing: Option<i64>,
    archive: Option<i64>,
    purge: Option<i64>,
}

fn days(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

/// A count typed into a field: empty is `None`, else a whole number of zero
/// or more, refused by the field's name.
fn count(text: &str, name: &str) -> Result<Option<i64>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<i64>() {
        Ok(n) if n >= 0 => Ok(Some(n)),
        _ => Err(format!("{name}: a whole number, 0 or more")),
    }
}

impl OngoingDraft {
    pub(super) fn of(f: &FolderInfo) -> Self {
        let ongoing = f.ongoing.as_ref().map(|o| o.idle_minutes);
        Self {
            on: RwSignal::new(ongoing.is_some()),
            idle: RwSignal::new(days(ongoing)),
            apply: RwSignal::new(true),
            archive: RwSignal::new(days(f.archive_days)),
            purge: RwSignal::new(days(f.purge_days)),
            seeded: Seeded {
                ongoing,
                archive: f.archive_days,
                purge: f.purge_days,
            },
        }
    }

    /// The patch's `ongoing`, `archive_days` and `purge_days` — only those
    /// the form changed (review W5-17): a save that changes the prompt
    /// must not undo what a client set meanwhile, such as the ongoing
    /// conversation it just marked — and `apply_to_current`; or the first
    /// field that is not a count.
    pub(super) fn body(&self) -> Result<Map<String, Value>, String> {
        let ongoing = if self.on.get_untracked() {
            Some(
                count(
                    &self.idle.get_untracked(),
                    "New thread after (idle minutes)",
                )?
                .unwrap_or(0),
            )
        } else {
            None
        };
        let archive = count(&self.archive.get_untracked(), "Archive after (days idle)")?;
        let purge = count(&self.purge.get_untracked(), "Delete after (days archived)")?;
        let mut m = changed(
            self.seeded,
            Seeded {
                ongoing,
                archive,
                purge,
            },
        );
        m.insert("apply_to_current".into(), json!(self.apply.get_untracked()));
        Ok(m)
    }
}

/// The fields of `now` that differ from `seeded`, as a patch spells them.
fn changed(seeded: Seeded, now: Seeded) -> Map<String, Value> {
    let mut m = Map::new();
    if now.ongoing != seeded.ongoing {
        m.insert(
            "ongoing".into(),
            match now.ongoing {
                Some(minutes) => json!({ "idle_minutes": minutes }),
                None => Value::Null,
            },
        );
    }
    if now.archive != seeded.archive {
        m.insert("archive_days".into(), json!(now.archive));
    }
    if now.purge != seeded.purge {
        m.insert("purge_days".into(), json!(now.purge));
    }
    m
}

/// The global retention, for the placeholders: `(archive, purge)` days.
fn globals() -> LocalResource<Option<(i64, i64)>> {
    LocalResource::new(|| async {
        let v = crate::api::get::<Value>("/api/settings-full").await.ok()?;
        Some((
            v["chat_archive_days"].as_i64()?,
            v["chat_purge_days"].as_i64()?,
        ))
    })
}

/// What an empty retention field means, with the global value.
fn global_text(days: Option<i64>, never: &str) -> String {
    match days {
        Some(0) => format!("global: {never}"),
        Some(n) => format!("global: {n}"),
        None => "the global setting".to_string(),
    }
}

/// "Ongoing conversation" and "Retention", for [`super::FolderSettingsForm`].
#[component]
pub(super) fn OngoingFields(draft: OngoingDraft) -> impl IntoView {
    let g = globals();
    let global = move || g.get().flatten();
    view! {
        <div class="field">
            <label>"Ongoing conversation"</label>
            <label class="row" style="gap:6px">
                <input
                    type="checkbox"
                    prop:checked=move || draft.on.get()
                    on:change=move |ev| draft.on.set(event_target_checked(&ev))
                />
                "One conversation: every client continues its current thread"
            </label>
            <Show when=move || draft.on.get()>
                <div class="field-grid ongoing-grid" style="--field-min:220px">
                    <div class="field">
                        <label>"New thread after (idle minutes)"</label>
                        <input
                            class="input mono"
                            inputmode="numeric"
                            placeholder="0"
                            prop:value=move || draft.idle.get()
                            on:input=move |ev| draft.idle.set(event_target_value(&ev))
                        />
                    </div>
                </div>
                <p class="dim">
                    "A new thread starts once the current one has had no message for this long; 0 starts one only when asked (New conversation)."
                </p>
            </Show>
        </div>
        <div class="field">
            <label>"Retention"</label>
            <div class="field-grid" style="--field-min:200px">
                <div class="field">
                    <label>"Archive after (days idle)"</label>
                    <input
                        class="input mono"
                        inputmode="numeric"
                        placeholder=move || global_text(global().map(|g| g.0), "never")
                        prop:value=move || draft.archive.get()
                        on:input=move |ev| draft.archive.set(event_target_value(&ev))
                    />
                </div>
                <div class="field">
                    <label>"Delete after (days archived)"</label>
                    <input
                        class="input mono"
                        inputmode="numeric"
                        placeholder=move || global_text(global().map(|g| g.1), "keep")
                        prop:value=move || draft.purge.get()
                        on:input=move |ev| draft.purge.set(event_target_value(&ev))
                    />
                </div>
            </div>
            <p class="dim">
                "For this folder's chats. Empty: the global setting (Settings → Retention). 0: never. A current thread is never archived."
            </p>
        </div>
    }
}

/// "Also apply to the current thread": for an ongoing folder that has one.
#[component]
pub(super) fn ApplyToCurrent(draft: OngoingDraft, has_current: bool) -> impl IntoView {
    view! {
        <Show when=move || has_current && draft.on.get()>
            <label class="row folder-apply" style="gap:6px">
                <input
                    type="checkbox"
                    prop:checked=move || draft.apply.get()
                    on:change=move |ev| draft.apply.set(event_target_checked(&ev))
                />
                "Also apply the changes to the current thread"
            </label>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_save_sends_only_the_fields_the_form_changed() {
        let seeded = Seeded {
            ongoing: Some(30),
            archive: None,
            purge: Some(365),
        };
        assert!(changed(seeded, seeded).is_empty(), "nothing changed");
        let m = changed(
            seeded,
            Seeded {
                ongoing: None,
                ..seeded
            },
        );
        assert_eq!(Value::Object(m), json!({ "ongoing": null }));
        let m = changed(
            seeded,
            Seeded {
                ongoing: Some(10),
                archive: Some(0),
                purge: None,
            },
        );
        assert_eq!(
            Value::Object(m),
            json!({ "ongoing": { "idle_minutes": 10 }, "archive_days": 0, "purge_days": null })
        );
    }

    #[test]
    fn counts_are_whole_and_not_negative_and_empty_is_global() {
        assert_eq!(count(" ", "x"), Ok(None));
        assert_eq!(count("365", "x"), Ok(Some(365)));
        assert_eq!(count("0", "x"), Ok(Some(0)));
        assert!(count("-1", "Archive").unwrap_err().starts_with("Archive"));
        assert!(count("1.5", "x").is_err());
    }
}
