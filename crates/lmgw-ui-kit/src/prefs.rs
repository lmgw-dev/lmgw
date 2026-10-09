//! View preferences that outlive a reload: the sidebar rail, which sections
//! are open, "show all" choices, recent picks (UX plan §4).
//!
//! All of them live in localStorage under `lmgw.ui.` — callers name the rest
//! (`sidebar`, `open.models.group.chat`, `recent.chat`). The two keys
//! index.html reads before first paint, `lmgw-theme` and `lmgw-ui-scale`,
//! keep their old names and their own code: renaming them would reset every
//! existing install's theme and scale.
//!
//! Storage failing (private mode, a full quota) is never an error: the value
//! simply does not survive the reload, which is what the default promises.

use leptos::prelude::*;

const PREFIX: &str = "lmgw.ui.";

/// How many recent picks a picker keeps. The Recent group is a shortcut above
/// the full list, never a replacement for it, so nothing is out of reach.
#[allow(dead_code)] // the ModelPicker (Phase 1b task 9) reads it
pub const RECENT_MAX: usize = 8;

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

fn read(key: &str) -> Option<String> {
    storage().and_then(|s| s.get_item(&format!("{PREFIX}{key}")).ok().flatten())
}

fn write(key: &str, value: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(&format!("{PREFIX}{key}"), value);
    }
}

/// A string preference: starts from what was stored (or `default`), and every
/// later change is written back.
///
/// The write compares against the last value it saw rather than skipping the
/// first run, so a default is never persisted just by being read — a changed
/// default in a later build still reaches everyone who never touched it.
pub fn persisted_string(key: &str, default: &str) -> RwSignal<String> {
    let key = key.to_string();
    let initial = read(&key).unwrap_or_else(|| default.to_string());
    let sig = RwSignal::new(initial.clone());
    let mut last = initial;
    Effect::new(move |_| {
        let v = sig.get();
        if v != last {
            write(&key, &v);
            last = v;
        }
    });
    sig
}

/// A yes/no preference, stored as `true`/`false`.
#[allow(dead_code)] // Section, Explain, ShowMore persist through it
pub fn persisted_bool(key: &str, default: bool) -> RwSignal<bool> {
    let key = key.to_string();
    let initial = read(&key).map(|v| v == "true").unwrap_or(default);
    let sig = RwSignal::new(initial);
    let mut last = initial;
    Effect::new(move |_| {
        let v = sig.get();
        if v != last {
            write(&key, if v { "true" } else { "false" });
            last = v;
        }
    });
    sig
}

/// Store a yes/no preference now, for a link that lands on a folded
/// Explain or Section and wants it open (read when that widget is built).
pub fn store_bool(key: &str, value: bool) {
    write(key, if value { "true" } else { "false" });
}

/// The picks remembered under `recent.<key>`, newest first.
#[allow(dead_code)] // the ModelPicker (Phase 1b task 9) reads it
pub fn recent(key: &str) -> Vec<String> {
    read(&format!("recent.{key}"))
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

/// Move `id` to the front of `recent.<key>`, keeping [`RECENT_MAX`].
#[allow(dead_code)] // the ModelPicker (Phase 1b task 9) writes it
pub fn recent_push(key: &str, id: &str) {
    let list = push_front(recent(key), id);
    if let Ok(raw) = serde_json::to_string(&list) {
        write(&format!("recent.{key}"), &raw);
    }
}

fn push_front(mut list: Vec<String>, id: &str) -> Vec<String> {
    list.retain(|x| x != id);
    list.insert(0, id.to_string());
    list.truncate(RECENT_MAX);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pick_moves_to_the_front_once() {
        let l = push_front(vec!["a".into(), "b".into(), "c".into()], "b");
        assert_eq!(l, ["b", "a", "c"]);
        let l = push_front(l, "d");
        assert_eq!(l, ["d", "b", "a", "c"]);
    }

    #[test]
    fn the_oldest_pick_falls_off_past_the_maximum() {
        let full: Vec<String> = (0..RECENT_MAX).map(|i| i.to_string()).collect();
        let l = push_front(full, "new");
        assert_eq!(l.len(), RECENT_MAX);
        assert_eq!(l[0], "new");
        assert!(!l.contains(&(RECENT_MAX - 1).to_string()));
    }
}
