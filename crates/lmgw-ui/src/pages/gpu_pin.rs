//! The GPU pin in a model's editor (device-ledger design §8): a choice of
//! the machine's graphics cards by name and index, each with its room
//! beside it. Empty, lmgw picks the card; one card, the model runs there or
//! not at all; several, lmgw picks among them. The cards are the ones the
//! GPU panel shows (the live `vram` frame); a pinned card this computer does
//! not list stays in the list, checked, so it can be taken out.
//!
//! The value is the pin as the op takes it: card ids (`gpu_pin`), here
//! comma-separated in one text signal so an editor's change tracking reads
//! it like any other field.

use leptos::prelude::*;

use crate::fmt::fmt_bytes;

/// What the field says it is for (§8).
pub const PIN_HINT: &str = "Run this model on a card of your choosing. Empty: lmgw picks the card.";

/// The card ids in `text`, in their order, each once.
pub fn pin_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// `ids` as the field's text.
pub fn pin_text(ids: &[lmgw_api_types::DeviceId]) -> String {
    ids.iter()
        .map(|d| d.as_str().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The pin as a save sends it: the list of ids, an empty list clearing it.
pub fn pin_value(text: &str) -> serde_json::Value {
    serde_json::Value::from(pin_ids(text))
}

/// `id` taken into, or out of, the pin `text`.
fn toggled(text: &str, id: &str, on: bool) -> String {
    let mut ids = pin_ids(text);
    ids.retain(|i| i != id);
    if on {
        ids.push(id.to_string());
    }
    ids.join(",")
}

/// The cards, each a checkbox with its name, index and room (module doc).
#[component]
pub fn GpuPinChoice(value: RwSignal<String>) -> impl IntoView {
    let live = crate::live::use_live();
    move || {
        let chosen = pin_ids(&value.get());
        let devices = live.vram.get().map(|v| v.devices).unwrap_or_default();
        let mut rows: Vec<AnyView> = devices
            .iter()
            .map(|d| {
                let id = d.id.as_str().to_string();
                let checked = chosen.contains(&id);
                let tip = id.clone();
                let label = format!("{} (card {})", d.name, d.index);
                let room = format!(
                    "{} free of {}",
                    fmt_bytes(d.free_bytes),
                    fmt_bytes(d.total_bytes)
                );
                view! {
                    <label class="row dim" style="gap:5px" title=tip>
                        <input
                            type="checkbox"
                            prop:checked=checked
                            on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                value.update(|v| *v = toggled(v, &id, on));
                            }
                        />
                        <span>{label}</span>
                        <span class="field-unit">{room}</span>
                    </label>
                }
                .into_any()
            })
            .collect();
        for id in chosen
            .iter()
            .filter(|id| !devices.iter().any(|d| d.id.as_str() == id.as_str()))
        {
            let id = id.clone();
            let label = format!("a card this computer does not list ({id})");
            rows.push(
                view! {
                    <label class="row dim" style="gap:5px">
                        <input
                            type="checkbox"
                            prop:checked=true
                            on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                value.update(|v| *v = toggled(v, &id, on));
                            }
                        />
                        <span class="warn">{label}</span>
                    </label>
                }
                .into_any(),
            );
        }
        if rows.is_empty() {
            rows.push(
                view! { <span class="dim">"No graphics card is listed now."</span> }.into_any(),
            );
        }
        view! { <div style="display:flex;flex-direction:column;gap:3px">{rows}</div> }
    }
}

/// The pin as a field of an editor's card (the aux, image and audio
/// editors): the choice, and under it what the row's read says of its card
/// (`command_line_note`).
#[component]
pub fn GpuPinField(
    value: RwSignal<String>,
    #[prop(into)] note: Signal<Option<String>>,
) -> impl IntoView {
    view! {
        <div class="field">
            <label>"Graphics card" <span class="field-unit">{PIN_HINT}</span></label>
            <GpuPinChoice value=value/>
            {move || note.get().map(|n| view! { <p class="dim mini-note">{n}</p> })}
        </div>
    }
}

/// An editor's pin, read from the row once its read arrives (`read`: the
/// row's read, `None` while it has not), and whether that has happened: a
/// save sends the pin only then, so a save before the read never clears a
/// pin it did not show. A new row has nothing to read and sends it at once.
#[derive(Clone, Copy)]
pub struct EditorPin {
    pub value: RwSignal<String>,
    loaded: RwSignal<bool>,
}

impl EditorPin {
    pub fn new(create: bool) -> Self {
        Self {
            value: RwSignal::new(String::new()),
            loaded: RwSignal::new(create),
        }
    }

    /// The row's pin as its read gives it, the first time it does.
    pub fn read(&self, pin: Option<&[lmgw_api_types::DeviceId]>) {
        if self.loaded.get_untracked() {
            return;
        }
        self.value.set(pin_text(pin.unwrap_or_default()));
        self.loaded.set(true);
    }

    /// `body` (a save's JSON object) with the pin, once it was read.
    pub fn add_to(&self, body: &mut serde_json::Value) {
        if !self.loaded.get_untracked() {
            return;
        }
        if let Some(b) = body.as_object_mut() {
            b.insert("gpu_pin".into(), pin_value(&self.value.get_untracked()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_text_round_trips_and_toggles() {
        assert_eq!(pin_ids(" GPU-a , ,GPU-b,GPU-a"), vec!["GPU-a", "GPU-b"]);
        assert_eq!(toggled("GPU-a", "GPU-b", true), "GPU-a,GPU-b");
        assert_eq!(toggled("GPU-a,GPU-b", "GPU-a", false), "GPU-b");
        assert_eq!(pin_value(""), serde_json::json!([]));
        assert_eq!(
            pin_text(&[
                lmgw_api_types::DeviceId::new("GPU-a"),
                lmgw_api_types::DeviceId::new("GPU-b")
            ]),
            "GPU-a,GPU-b"
        );
    }
}
