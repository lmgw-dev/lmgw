//! The theme, one choice for the window: "dark", "light", or (in the
//! Windows app window, whose shell marks the system's mode on the page as
//! `data-system-theme`, src-tauri's desktop_palette/win.rs) "system", which
//! follows Windows. Stored under `lmgw-theme`, where boot.js reads it before
//! first paint; Settings' Theme field and the main menu both set it here.

use leptos::prelude::*;

/// Where the choice lives between runs (boot.js reads the same key).
const KEY: &str = "lmgw-theme";

#[derive(Clone, Copy)]
pub struct Theme {
    /// "dark", "light" or "system".
    pub choice: RwSignal<String>,
    /// The page can follow the system's mode ("system" is offered).
    pub follows: bool,
}

fn system() -> Option<String> {
    document()
        .document_element()
        .and_then(|el| el.get_attribute("data-system-theme"))
}

/// Install the theme state and apply it. Called once from `App`.
pub fn provide_theme() {
    let follows = system().is_some();
    let choice = RwSignal::new(
        window()
            .local_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item(KEY).ok().flatten())
            .filter(|t| follows || t != "system")
            .unwrap_or_else(|| if follows { "system" } else { "dark" }.into()),
    );
    Effect::new(move |_| {
        let t = choice.get();
        let shown = match t.as_str() {
            "system" => system().unwrap_or_else(|| "dark".into()),
            _ => t.clone(),
        };
        let _ = document()
            .document_element()
            .map(|el| el.set_attribute("data-theme", &shown));
        if let Ok(Some(storage)) = window().local_storage() {
            let _ = storage.set_item(KEY, &t);
        }
    });
    provide_context(Theme { choice, follows });
}

pub fn use_theme() -> Theme {
    expect_context::<Theme>()
}

impl Theme {
    /// The choices, as (value, label).
    pub fn options(&self) -> Vec<(String, String)> {
        let system = self.follows.then_some(("system", "System"));
        system
            .into_iter()
            .chain([("dark", "Dark"), ("light", "Light")])
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect()
    }
}
