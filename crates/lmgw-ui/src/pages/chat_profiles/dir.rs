//! The personality profiles as the Chat page knows them: one list, fetched
//! with the page and read again when the window regains focus, when the
//! open thread changes, and after the page's own profile writes. The
//! dashboard's `chat` frame carries no profile change, so a profile edited
//! in another window shows here at the next of those, never on a timer.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{Profile, ProfileList};
use lmgw_ui_kit::profiles::api;

#[derive(Clone, Copy)]
pub(in crate::pages) struct ProfileDir {
    list: RwSignal<ProfileList>,
}

impl ProfileDir {
    /// Install the directory for the page below and fetch it.
    pub(in crate::pages) fn provide() -> Self {
        let dir = Self {
            list: RwSignal::new(ProfileList::default()),
        };
        provide_context(dir);
        dir.refresh();
        window_event_listener(leptos::ev::focus, move |_| dir.refresh());
        dir
    }

    pub(in crate::pages) fn refresh(&self) {
        let list = self.list;
        leptos::task::spawn_local(async move {
            if let Ok(l) = api::list().await {
                list.try_set(l);
            }
        });
    }

    /// The profile `id` (tracked).
    pub(in crate::pages) fn get(&self, id: i64) -> Option<Profile> {
        self.list
            .with(|l| l.profiles.iter().find(|p| p.id == id).cloned())
    }

    /// The profile new threads start with when their folder names none
    /// (Settings → Chat's `chat_profile`), tracked; `None` when that is
    /// empty.
    pub(in crate::pages) fn default_profile(&self) -> Option<Profile> {
        self.list.with(|l| {
            l.default_profile_id
                .and_then(|id| l.profiles.iter().find(|p| p.id == id).cloned())
        })
    }

    /// The profile a draft value (`""` none, else an id) names (tracked).
    pub(in crate::pages) fn of_value(&self, value: &str) -> Option<Profile> {
        value.trim().parse().ok().and_then(|id| self.get(id))
    }

    /// `(value, label)` for a [`Select`](crate::widgets::Select): "Default"
    /// (none), then the profiles in the gateway's order (tracked).
    pub(in crate::pages) fn options(&self) -> Vec<(String, String)> {
        self.list.with(|l| {
            std::iter::once((String::new(), "Default".to_string()))
                .chain(
                    l.profiles
                        .iter()
                        .map(|p| (p.id.to_string(), p.name.clone())),
                )
                .collect()
        })
    }
}

/// The page's directory, where the Chat page installed one.
pub(in crate::pages) fn use_profile_dir() -> Option<ProfileDir> {
    use_context::<ProfileDir>()
}
