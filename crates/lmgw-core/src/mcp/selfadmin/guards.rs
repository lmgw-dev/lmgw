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

/// The settings no self-admin tool changes, read off Settings → Network &
/// access ("where the gateway listens and who may call it") and the one
/// origin setting lmgw has:
///
/// - `auth_enabled`: whether `/v1` and `/mcp` ask for a key at all;
/// - `self_admin`: the gateway's self-admin level, which caps every
///   caller's admin tools;
/// - `bind_addr`: the address lmgw listens on, loopback or the network;
/// - `agent_origin_suffix`: the names a service agent's UI answers under,
///   the setting to change for a gateway opened from another box.
///
/// None is among `lmgw__settings_set`'s fields; they are named here so a
/// call that asks for one hears why, not that the tool does not know it.
/// lmgw's CORS policy is fixed (`server.rs`), and the API keys are no
/// setting: no self-admin tool writes them.
pub const ACCESS_SETTINGS: [&str; 4] = [
    "auth_enabled",
    "self_admin",
    "bind_addr",
    "agent_origin_suffix",
];

/// Why the call `name(args)` may not run, from any caller: it names an
/// access setting ([`ACCESS_SETTINGS`]). A field given as `null` changes
/// nothing and is no request.
pub(super) fn access_refusal(name: &str, args: Option<&Map<String, Value>>) -> Option<String> {
    if name != "lmgw__settings_set" {
        return None;
    }
    let args = args?;
    let named: Vec<&str> = ACCESS_SETTINGS
        .into_iter()
        .filter(|k| args.get(*k).is_some_and(|v| !v.is_null()))
        .collect();
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

    #[test]
    fn an_ordinary_setting_a_null_and_another_tool_pass() {
        let a = args(json!({ "retention_days": 3, "auth_enabled": null }));
        assert_eq!(access_refusal("lmgw__settings_set", Some(&a)), None);
        assert_eq!(access_refusal("lmgw__settings_set", None), None);
        let a = args(json!({ "auth_enabled": false }));
        assert_eq!(access_refusal("lmgw__settings", Some(&a)), None);
    }
}
