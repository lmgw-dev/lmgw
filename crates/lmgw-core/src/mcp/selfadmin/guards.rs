//! The refusal every `lmgw__*` call meets, whoever makes it — a paired
//! device's turn, an agent's run, Admin Chat, a model on `/mcp/admin`
//! (client-apps design L5's notes, 2026-10-07): **the access settings**
//! ([`ACCESS_SETTINGS`]) are not changed through the admin tools at all.
//! They decide who may reach lmgw and with what credential, and a model's
//! call, steered or not, must not widen that. They change on Settings, with
//! the owner's credential.
//!
//! Its sibling, a stored credential following its host, is checked where
//! the move is applied, on the row about to be written
//! (`ops::credential_move`), so every action that can move a row meets it
//! (the branch review's verification, V-1).
//!
//! The dashboard's own ops (`/api/op/*`) reach `ops` directly and meet
//! neither: they are the owner's.

use serde_json::{Map, Value};

mod outbound;
pub(super) use outbound::refusal as outbound_refusal;

/// The settings no self-admin tool changes, read off Settings → Network &
/// access ("where the gateway listens and who may call it") and the one
/// origin setting lmgw has:
///
/// - `auth_enabled`: whether `/v1` and `/mcp` ask for a key at all;
/// - `self_admin`: the gateway's self-admin level, which caps every
///   caller's admin tools;
/// - `bind_addr`: the address lmgw listens on, loopback or the network;
/// - `agent_origin_suffix`: the names a service agent's UI answers under,
///   the setting to change for a gateway opened from another box;
/// - `devices`: the device listener, where it listens, and how long a
///   pairing token and a device's certificate live (device link design
///   §9.1).
/// - `accounts`: who signs in to the dashboard and for how long (headless
///   server design §5, D17).
///
/// None is among `lmgw__settings_set`'s fields; they are named here so a
/// call that asks for one hears why, not that the tool does not know it.
/// lmgw's CORS policy is fixed (`server.rs`), and the API keys are no
/// setting: no self-admin tool writes them.
pub const ACCESS_SETTINGS: [&str; 6] = [
    "auth_enabled",
    "self_admin",
    "bind_addr",
    "agent_origin_suffix",
    "devices",
    "accounts",
];

/// The settings that decide where lmgw itself sends requests, or a
/// caller's documents, on its own:
///
/// - the docling adapter's web sources, Settings › Documents' "Let callers
///   send web addresses" and "Allowed sites" (document-conversion design
///   ruling 9, D28): a model's call that turned them on could make lmgw
///   fetch from the owner's own network for anyone holding a key;
/// - the page-reading model, "Read pages with a model" and its answer
///   format (D5: off until the *user* picks): a model's call that set it to
///   a cloud alias would send every caller's scanned and table pages there
///   from then on, the docling callers' included;
/// - the picture-description model, "Describe pictures with" (ruling 5,
///   D5): the same for every picture a caller asks to have described;
/// - the speech-to-text model (WP7, D5): the same for every caller's audio
///   file and video.
///
/// So they change on Settings only, like the access settings: the
/// dashboard's `settings_set` op takes them, and an `lmgw__settings_set`
/// call that names one is refused here.
pub const OUTBOUND_SETTINGS: [&str; 6] = [
    "documents_fetch_sources",
    "documents_source_hosts",
    "documents_page_reader",
    "documents_page_reader_doctags",
    "documents_picture_alias",
    "documents_asr_alias",
];

/// Why the call `name(args)` may not run, from any caller: it names an
/// access setting ([`ACCESS_SETTINGS`]) or an outbound one
/// ([`OUTBOUND_SETTINGS`]). A field given as `null` changes nothing and is
/// no request.
pub(super) fn access_refusal(name: &str, args: Option<&Map<String, Value>>) -> Option<String> {
    if name != "lmgw__settings_set" {
        return None;
    }
    let args = args?;
    let given = |k: &&str| args.get(*k).is_some_and(|v| !v.is_null());
    let outbound: Vec<&str> = OUTBOUND_SETTINGS.into_iter().filter(given).collect();
    let named: Vec<&str> = ACCESS_SETTINGS.into_iter().filter(given).collect();
    if named.is_empty() && !outbound.is_empty() {
        return Some(format!(
            "{} cannot be changed through lmgw's admin tools: they decide which addresses lmgw \
             fetches from for its callers, which model its callers' document pages are sent to, \
             which model describes their pictures and which model their audio is sent to. Change them on Settings › Documents in the dashboard. Nothing was changed",
            outbound.join(" and ")
        ));
    }
    let (last, rest) = named.split_last()?;
    let (list, them) = if rest.is_empty() {
        ((*last).to_string(), ("it decides", "it"))
    } else {
        (
            format!("{} and {last}", rest.join(", ")),
            ("they decide", "them"),
        )
    };
    Some(format!(
        "{list} cannot be changed through lmgw's admin tools: {} who may reach lmgw and with \
         what credential. Change {} on Settings in the dashboard. Nothing was changed",
        them.0, them.1
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn every_access_setting_is_refused_by_name() {
        for key in ACCESS_SETTINGS {
            let mut a = args(json!({ "retention_days": 3 }));
            a.insert(key.to_string(), json!("x"));
            let why = access_refusal("lmgw__settings_set", Some(&a)).unwrap();
            assert!(
                why.starts_with(&format!(
                    "{key} cannot be changed through lmgw's admin tools: it decides"
                )),
                "{why}"
            );
            assert!(why.contains("Change it on Settings"), "{why}");
        }
        let a = args(json!({ "self_admin": "full", "auth_enabled": false }));
        let why = access_refusal("lmgw__settings_set", Some(&a)).unwrap();
        assert!(
            why.starts_with("auth_enabled and self_admin cannot be changed through"),
            "{why}"
        );
        assert!(why.contains("Change them on Settings"), "{why}");
    }

    /// S3: choosing where document pages are sent is the owner's own act.
    #[test]
    fn the_page_reading_model_is_refused_by_name() {
        for (key, value) in [
            ("documents_page_reader", json!("example-cloud/vision")),
            ("documents_page_reader", json!("")),
            ("documents_page_reader_doctags", json!(true)),
        ] {
            let mut a = args(json!({ "retention_days": 3 }));
            a.insert(key.to_string(), value);
            let why = access_refusal("lmgw__settings_set", Some(&a)).unwrap();
            assert!(
                why.starts_with(&format!(
                    "{key} cannot be changed through lmgw's admin tools"
                )),
                "{why}"
            );
            assert!(why.contains("document pages are sent to"), "{why}");
            assert!(why.contains("Nothing was changed"), "{why}");
        }
        // Not named, or named as null: no request, no refusal.
        let a = args(json!({ "retention_days": 3, "documents_page_reader": null }));
        assert_eq!(access_refusal("lmgw__settings_set", Some(&a)), None);
        assert!(OUTBOUND_SETTINGS.contains(&"documents_page_reader"));
        assert!(OUTBOUND_SETTINGS.contains(&"documents_page_reader_doctags"));
    }

    /// WP11, WP7: choosing where document pictures and audio are sent is
    /// the owner's own act too, one guard for every model a conversion
    /// sends a caller's content to.
    #[test]
    fn the_picture_and_speech_models_are_refused_by_name() {
        for (key, said) in [
            (
                "documents_picture_alias",
                "which model describes their pictures",
            ),
            ("documents_asr_alias", "their audio is sent to"),
        ] {
            for value in [json!("example-cloud/model"), json!("")] {
                let mut a = args(json!({ "retention_days": 3 }));
                a.insert(key.to_string(), value);
                let why = access_refusal("lmgw__settings_set", Some(&a)).unwrap();
                assert!(
                    why.starts_with(&format!(
                        "{key} cannot be changed through lmgw's admin tools"
                    )),
                    "{why}"
                );
                assert!(why.contains(said), "{why}");
                assert!(why.contains("Nothing was changed"), "{why}");
            }
            let mut a = args(json!({ "retention_days": 3 }));
            a.insert(key.to_string(), Value::Null);
            assert_eq!(access_refusal("lmgw__settings_set", Some(&a)), None);
            assert!(OUTBOUND_SETTINGS.contains(&key));
        }
    }

    #[test]
    fn the_web_source_settings_are_refused_by_name() {
        for key in OUTBOUND_SETTINGS {
            let mut a = args(json!({ "retention_days": 3 }));
            a.insert(key.to_string(), json!(true));
            let why = access_refusal("lmgw__settings_set", Some(&a)).unwrap();
            assert!(
                why.starts_with(&format!(
                    "{key} cannot be changed through lmgw's admin tools"
                )),
                "{why}"
            );
            assert!(why.contains("Settings › Documents"), "{why}");
        }
    }

    /// The layout model sends nothing out (it runs on the processor, its
    /// file comes through the download queue): its switch and its download
    /// stay the tools' to use, beside the outbound settings that are not.
    #[test]
    fn the_layout_model_s_switch_and_download_pass() {
        let a = args(json!({ "documents_onnx_layout": true }));
        assert_eq!(access_refusal("lmgw__settings_set", Some(&a)), None);
        let a = args(json!({ "id": "layout" }));
        assert_eq!(access_refusal("lmgw__doc_model_add", Some(&a)), None);
        assert!(!OUTBOUND_SETTINGS.contains(&"documents_onnx_layout"));
    }

    #[test]
    fn an_ordinary_setting_a_null_and_another_tool_pass() {
        let a = args(json!({ "retention_days": 3, "auth_enabled": null }));
        assert_eq!(access_refusal("lmgw__settings_set", Some(&a)), None);
        assert_eq!(access_refusal("lmgw__settings_set", None), None);
        let a = args(json!({ "auth_enabled": false }));
        assert_eq!(access_refusal("lmgw__settings", Some(&a)), None);
    }
}
