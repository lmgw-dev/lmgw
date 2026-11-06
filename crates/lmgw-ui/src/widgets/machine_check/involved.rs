//! Are local models involved (machine-check design §2.2, D170, D184)? The
//! dashboard says it from the admin plane's rows, never from `/v1/models`:
//! that list names an alias onto a local model as lmgw's own, leaves out a
//! local row that is not public, and is empty when a provider's catalog
//! could not be read, which is not "nothing configured".
//!
//! Involved: a local model that runs on the graphics card is configured
//! and on (a chat or image row, an aux row without `backend: cpu`, an audio
//! row whose placement is the card), whether clients reach it by its own
//! name or only through an alias, a candidate alias or a fallback; or
//! nothing is configured at all, no local row, no alias and no upstream (a
//! first launch, where the person may still pick a local one). Rows on the
//! processor alone, or cloud upstreams alone, are not nagged.
//!
//! One read for the app ([`Involvement`], provided with the machine
//! state), of `/api/models/full` and `/api/upstreams` again each time a
//! surface that reaches out mounts, one read at a time.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{ModelsFull, UpstreamsResponse};

/// Does the check concern these rows (module doc)? `upstreams`: how many
/// upstream rows there are, cloud or not.
pub(crate) fn involved_in(full: &ModelsFull, upstreams: usize) -> bool {
    let on_card = full.local.iter().any(|m| m.model.enabled)
        || full.image.iter().any(|m| m.model.enabled)
        || full
            .aux
            .iter()
            .any(|m| m.model.enabled && m.model.backend.as_deref() != Some("cpu"))
        || full
            .audio
            .iter()
            .any(|m| m.model.enabled && m.runs_on != "cpu");
    let nothing = full.local.is_empty()
        && full.image.is_empty()
        && full.aux.is_empty()
        && full.audio.is_empty()
        && full.aliases.is_empty()
        && full.candidate_aliases.is_empty()
        && upstreams == 0;
    on_card || nothing
}

/// Whether local models are involved, for the app's surfaces. `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct Involvement {
    /// `None` until a read answered: nothing reaches out before.
    known: RwSignal<Option<bool>>,
    reading: StoredValue<bool>,
}

impl Involvement {
    pub(super) fn new() -> Self {
        Self {
            known: RwSignal::new(None),
            reading: StoredValue::new(false),
        }
    }

    /// Read the rows again (a surface mounted): skipped while a read is
    /// out. A read that fails keeps what the last one said.
    pub(crate) fn refresh(self) {
        if self.reading.get_value() {
            return;
        }
        self.reading.set_value(true);
        spawn_local(async move {
            let (full, ups) = futures::join!(
                crate::api::get::<ModelsFull>("/api/models/full"),
                crate::api::get::<UpstreamsResponse>("/api/upstreams")
            );
            match (full, ups) {
                (Ok(f), Ok(u)) => {
                    self.known.try_set(Some(involved_in(&f, u.upstreams.len())));
                }
                (Err(e), _) | (_, Err(e)) => {
                    leptos::logging::warn!("the machine check's surfaces: {e}")
                }
            }
            self.reading.try_set_value(false);
        });
    }

    /// Involved, once known (tracked).
    pub(crate) fn get(self) -> bool {
        self.known.get() == Some(true)
    }
}

/// The app's [`Involvement`], read again for the surface that asks.
pub(crate) fn use_involved() -> Involvement {
    let i = use_context::<Involvement>().unwrap_or_else(Involvement::new);
    i.refresh();
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full(v: serde_json::Value) -> ModelsFull {
        let mut base = json!({"aliases": [], "local": [], "aux": [], "audio": []});
        base.as_object_mut()
            .unwrap()
            .extend(v.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn chat_row(id: &str, public: bool, enabled: bool) -> serde_json::Value {
        json!({"public_name": id, "model": {
            "id": 1, "model_id": id, "gguf_path": "demo/qwen-demo-8b.gguf",
            "idle_seconds": 300, "enabled": enabled, "public": public
        }})
    }

    fn alias(name: &str, upstream: i64, model: &str) -> serde_json::Value {
        json!({"id": 1, "alias": name, "upstream_id": upstream, "upstream_model_id": model,
               "enabled": true})
    }

    #[test]
    fn nothing_configured_is_a_first_launch() {
        assert!(involved_in(&full(json!({})), 0));
    }

    #[test]
    fn a_local_model_on_the_card_is_involved() {
        assert!(involved_in(
            &full(json!({"local": [chat_row("qwen-demo-8b", true, true)]})),
            1
        ));
    }

    /// M2: `/v1/models` lists an alias onto a local row as lmgw's own and
    /// leaves the row itself out when it is not public; the rows say it.
    #[test]
    fn an_alias_only_setup_is_involved() {
        let f = full(json!({
            "local": [chat_row("qwen-demo-8b", false, true)],
            "aliases": [alias("assistant-demo", 7, "qwen-demo-8b")],
        }));
        assert!(involved_in(&f, 1));
    }

    /// M2: a cloud upstream whose catalog could not be read leaves
    /// `/v1/models` empty; it is still configured, so a cloud-only person
    /// with a bad key is not nagged about the graphics card.
    #[test]
    fn an_empty_catalog_with_an_upstream_configured_is_not_involved() {
        assert!(!involved_in(&full(json!({})), 1));
        let f = full(json!({"aliases": [alias("gpt-demo", 2, "gpt-demo")]}));
        assert!(!involved_in(&f, 1));
    }

    #[test]
    fn rows_off_or_on_the_processor_are_not_involved() {
        let f = full(json!({"local": [chat_row("qwen-demo-8b", true, false)]}));
        assert!(!involved_in(&f, 0), "a configured row that is off");
        let aux = json!({"public_name": "embed/demo", "model": {
            "id": 1, "model_id": "embed-demo", "gguf_path": "e.gguf", "backend": "cpu",
            "idle_seconds": 300, "enabled": true
        }, "threads_in_effect": 8, "threads_source": "row"});
        assert!(!involved_in(&full(json!({"aux": [aux.clone()]})), 0));
        let mut on_card = aux;
        on_card["model"]["backend"] = serde_json::Value::Null;
        assert!(involved_in(&full(json!({"aux": [on_card]})), 0));
    }
}
