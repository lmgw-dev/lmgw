//! The rescued draft's open of its made chat, as it lands: [`flow`]'s step
//! applied to the page (reviews CL-19, CL-21, CF-3, CF-6).
//!
//! [`flow`]: super::flow

use std::future::Future;

use leptos::prelude::*;
use serde_json::{json, Value};

use super::super::super::chat::ChatThread;
use super::super::ModelPicks;
use super::flow::{Opened, Rescue, Step};
use super::step;
use crate::scope::Scope;
use crate::widgets::Toasts;

/// What a chat another writer has written in meanwhile says.
const WRITTEN: &str = "another writer has written in the new chat meanwhile — your draft is \
                       kept in the composer: Send sends it there";

/// What the page hands the landing. `Copy`.
#[derive(Clone, Copy)]
pub(in crate::pages) struct Landing {
    pub rescue: RwSignal<Rescue>,
    pub current: RwSignal<Option<ChatThread>>,
    pub model_sel: RwSignal<String>,
    /// The picker's picks: a model set from here is no pick of its own.
    pub picks: StoredValue<ModelPicks>,
    pub scope: Scope,
    pub toasts: Toasts,
    /// The page's ordinary send of what the composer holds.
    pub send: Callback<()>,
}

/// What [`WRITTEN`] adds when the owner's pick was not set (review CF-3):
/// the chat stays on `model`.
fn pick_dropped(pick: &str, model: &str) -> String {
    format!(" on {model} (the model you picked, {pick}, was not set)")
}

/// Send `ticket`'s open of the made chat landed as `o`. `set_model` saves a
/// thread's settings as the picker's pick does, saying nothing of a
/// refusal: the warning here says it, once (review CF-6).
pub(in crate::pages) fn rescued_open<F, Fut>(env: Landing, set_model: F, ticket: u64, o: Opened)
where
    F: FnOnce(i64, Value) -> Fut + 'static,
    Fut: Future<Output = Result<(), crate::api::Error>> + 'static,
{
    let Some(next) = step(env.rescue, |r| r.opened(ticket, &o)) else {
        return;
    };
    match next {
        Step::Left | Step::ModelRefused => {}
        Step::Send => env.send.run(()),
        Step::Written { dropped } => {
            let model = match &o {
                Opened::Shown { model, .. } => model.as_str(),
                _ => "",
            };
            let note = dropped.map_or(String::new(), |p| pick_dropped(&p, model));
            env.toasts.warn(format!("{WRITTEN}{note}"));
        }
        Step::Failed => {
            let error = match &o {
                Opened::Failed { error, .. } => error.as_str(),
                _ => "",
            };
            env.toasts.err(format!(
                "opening the new chat failed: {error} — the draft is kept, and Send tries again"
            ));
        }
        // The step names the chat and its model: nothing is read back from
        // the page, so no order of the page's updates can leave the Send
        // held (review CF-6).
        Step::SetModel { id, pick, was } => {
            // The picker shows the pick again at once; it is saved here,
            // not a second time by the picker.
            env.picks.update_value(|p| p.follow(id, &pick));
            env.model_sel.set(pick.clone());
            env.scope.spawn(async move {
                let set = set_model(id, json!({ "model_alias": pick })).await;
                match step(env.rescue, |r| r.model_set(ticket, set.is_ok())) {
                    Some(Step::Send) => env.send.run(()),
                    Some(Step::ModelRefused) => {
                        // The picker shows the model the chat is on.
                        if env.current.with_untracked(|c| c.as_ref().map(|t| t.id)) == Some(id) {
                            env.picks.update_value(|p| p.follow(id, &was));
                            env.model_sel.set(was.clone());
                        }
                        // A chat deleted meanwhile: the delete elsewhere says
                        // so, and keeps the draft.
                        match set {
                            Err(e) if !e.is_not_found() => env.toasts.warn(format!(
                                "setting the new chat's model to {pick} failed: {e} — it stays \
                                 on {was}, and your draft is kept in the composer: Send sends \
                                 it on {was}, or pick another model first"
                            )),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_pick_is_said_with_the_model_the_chat_stays_on() {
        assert_eq!(
            format!("{WRITTEN}{}", pick_dropped("fake-novision", "fake-echo")),
            "another writer has written in the new chat meanwhile — your draft is kept in the \
             composer: Send sends it there on fake-echo (the model you picked, fake-novision, \
             was not set)"
        );
    }
}
