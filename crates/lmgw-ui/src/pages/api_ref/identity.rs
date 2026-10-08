//! Which credential the tester sends (api-docs design §6.5): the identity
//! itself, the `GET /api/session` probe result for a pasted key, and what
//! each identity is expected to hold — pure, so "expect 401" vs "expect 403"
//! is exercised without a fetch.

use super::doc::Principals;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Identity {
    #[default]
    Session,
    Key,
    None,
}

/// `GET /api/session`'s answer to a pasted key: `kind` is `"owner"` |
/// `"agent"` | `"key"` | `""` (core `web::session::SessionView`), mirrored
/// here verbatim rather than parsed into an enum, so a kind core adds later
/// is shown by its own name instead of failing to parse. (`"internal"` is
/// the one other kind core has; its rows never authenticate over HTTP, so a
/// pasted internal key answers `authenticated: false` like any unknown one.)
///
/// `refused` is a probe that did not come back `2xx` at all: core resolves
/// a bearer matching *no* row to `Anonymous` (`principal::from_bearer`), but
/// a matched row that is switched off is a `401` on every route — that key
/// holds nothing, not even `public`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyProbe {
    pub authenticated: bool,
    pub kind: String,
    pub name: String,
    pub refused: bool,
}

/// "owner" / "agent key" / "client key 'name'" / "matches no key" / "no
/// credential" — the identity picker's own label (§6.5), and the `<kind>`
/// half of a 403 expectation string. "matches no key" is only ever said of a
/// probe that did not authenticate: one that did matched a row, whatever its
/// kind (review R3 #8).
pub fn kind_label(identity: Identity, probe: Option<&KeyProbe>) -> String {
    match identity {
        Identity::Session => "owner".to_string(),
        Identity::None => "no credential".to_string(),
        Identity::Key => match probe {
            Some(p) if p.refused => "refused key".to_string(),
            Some(p) if p.authenticated => match p.kind.as_str() {
                "owner" => "owner key".to_string(),
                "agent" => "agent key".to_string(),
                "key" if !p.name.is_empty() => format!("client key '{}'", p.name),
                "key" => "client key".to_string(),
                other if !p.name.is_empty() => format!("{other} key '{}'", p.name),
                other => format!("{other} key"),
            },
            Some(_) => "matches no key".to_string(),
            None => "key (not checked yet)".to_string(),
        },
    }
}

/// The held capabilities for this identity (§6.5): the session cookie always
/// resolves to owner rows, and "no credential" reads the auth-on or auth-off
/// anonymous row. So does a key that matches no row (or none pasted yet):
/// core resolves an unmatched bearer to `Anonymous`, not to a refusal
/// (`principal::from_bearer`) — only a *matched, disabled* row holds nothing.
pub fn held_caps(
    identity: Identity,
    principals: &Principals,
    auth_enabled: bool,
    probe: Option<&KeyProbe>,
) -> Vec<String> {
    let anonymous = || {
        if auth_enabled {
            principals.anonymous.clone()
        } else {
            principals.anonymous_auth_off.clone()
        }
    };
    match identity {
        Identity::Session => principals.owner.clone(),
        Identity::None => anonymous(),
        Identity::Key => match probe {
            Some(p) if p.refused => Vec::new(),
            Some(p) if p.authenticated => match p.kind.as_str() {
                "owner" => principals.owner.clone(),
                "agent" => principals.agent.clone(),
                "key" => principals.key.clone(),
                "device" => principals.device.clone(),
                _ => Vec::new(),
            },
            _ => anonymous(),
        },
    }
}

/// Is this identity unauthenticated (anonymous, or a key that matches
/// nothing) — the 401 half of §6.5's expectation rule.
fn unauthenticated(identity: Identity, probe: Option<&KeyProbe>) -> bool {
    match identity {
        Identity::None => true,
        Identity::Key => !probe.is_some_and(|p| p.authenticated),
        Identity::Session => false,
    }
}

/// `.chip.warn.cap-warn` text when `capability` is not held; `None` when it
/// is. Sending stays enabled either way (§6.5) — this only decides the
/// warning chip's text.
pub fn expectation(
    held: &[String],
    capability: &str,
    identity: Identity,
    probe: Option<&KeyProbe>,
) -> Option<String> {
    if capability.is_empty() || held.iter().any(|c| c == capability) {
        return None;
    }
    if identity == Identity::Key && probe.is_some_and(|p| p.refused) {
        Some("expect 401 — the key is refused".to_string())
    } else if unauthenticated(identity, probe) {
        Some("expect 401 — no credential".to_string())
    } else {
        Some(format!(
            "expect 403 — {} does not hold {capability}",
            kind_label(identity, probe)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principals() -> Principals {
        Principals {
            owner: vec!["public".into(), "inference".into(), "admin".into()],
            agent: vec!["public".into(), "inference".into(), "agent-self".into()],
            key: vec!["public".into(), "inference".into()],
            device: vec!["public".into(), "inference".into(), "chat".into()],
            anonymous: vec!["public".into()],
            anonymous_auth_off: vec!["public".into(), "inference".into()],
        }
    }

    #[test]
    fn session_always_holds_owner_rows() {
        let p = principals();
        assert_eq!(held_caps(Identity::Session, &p, true, None), p.owner);
        assert!(expectation(
            &held_caps(Identity::Session, &p, true, None),
            "admin",
            Identity::Session,
            None
        )
        .is_none());
        assert_eq!(
            expectation(
                &held_caps(Identity::Session, &p, true, None),
                "ledger",
                Identity::Session,
                None
            ),
            Some("expect 403 — owner does not hold ledger".to_string())
        );
    }

    #[test]
    fn no_credential_follows_auth_enabled() {
        let p = principals();
        assert_eq!(held_caps(Identity::None, &p, true, None), vec!["public"]);
        assert_eq!(
            held_caps(Identity::None, &p, false, None),
            vec!["public", "inference"]
        );
        assert_eq!(
            expectation(
                &held_caps(Identity::None, &p, true, None),
                "inference",
                Identity::None,
                None
            ),
            Some("expect 401 — no credential".to_string())
        );
        assert!(expectation(
            &held_caps(Identity::None, &p, false, None),
            "inference",
            Identity::None,
            None
        )
        .is_none());
    }

    #[test]
    fn an_unmatched_key_is_401_not_403() {
        let p = principals();
        let probe = KeyProbe::default();
        assert_eq!(
            expectation(
                &held_caps(Identity::Key, &p, true, Some(&probe)),
                "inference",
                Identity::Key,
                Some(&probe)
            ),
            Some("expect 401 — no credential".to_string())
        );
        assert_eq!(kind_label(Identity::Key, Some(&probe)), "matches no key");
        assert_eq!(kind_label(Identity::Key, None), "key (not checked yet)");
    }

    #[test]
    fn an_unmatched_key_holds_the_anonymous_row() {
        // Core resolves a bearer matching no row to `Anonymous`: a public
        // route answers, and with auth off so does inference.
        let p = principals();
        let probe = KeyProbe::default();
        for auth_enabled in [true, false] {
            let held = held_caps(Identity::Key, &p, auth_enabled, Some(&probe));
            assert!(expectation(&held, "public", Identity::Key, Some(&probe)).is_none());
        }
        let held = held_caps(Identity::Key, &p, false, Some(&probe));
        assert!(expectation(&held, "inference", Identity::Key, Some(&probe)).is_none());
    }

    #[test]
    fn a_refused_key_expects_401_everywhere() {
        let p = principals();
        let probe = KeyProbe {
            refused: true,
            ..KeyProbe::default()
        };
        let held = held_caps(Identity::Key, &p, false, Some(&probe));
        assert!(held.is_empty());
        assert_eq!(
            expectation(&held, "public", Identity::Key, Some(&probe)),
            Some("expect 401 — the key is refused".to_string())
        );
        assert_eq!(kind_label(Identity::Key, Some(&probe)), "refused key");
    }

    #[test]
    fn a_client_key_outside_its_scope_is_403() {
        let p = principals();
        let probe = KeyProbe {
            authenticated: true,
            kind: "key".into(),
            name: "ci".into(),
            refused: false,
        };
        assert_eq!(kind_label(Identity::Key, Some(&probe)), "client key 'ci'");
        assert_eq!(
            expectation(
                &held_caps(Identity::Key, &p, true, Some(&probe)),
                "admin",
                Identity::Key,
                Some(&probe)
            ),
            Some("expect 403 — client key 'ci' does not hold admin".to_string())
        );
        assert!(expectation(
            &held_caps(Identity::Key, &p, true, Some(&probe)),
            "inference",
            Identity::Key,
            Some(&probe)
        )
        .is_none());
    }

    #[test]
    fn an_authenticated_key_of_another_kind_is_named_not_unmatched() {
        let probe = KeyProbe {
            authenticated: true,
            kind: "internal".into(),
            name: "internal:agents".into(),
            refused: false,
        };
        assert_eq!(
            kind_label(Identity::Key, Some(&probe)),
            "internal key 'internal:agents'"
        );
        let unnamed = KeyProbe {
            name: String::new(),
            ..probe
        };
        assert_eq!(kind_label(Identity::Key, Some(&unnamed)), "internal key");
    }

    #[test]
    fn owner_and_agent_keys_read_their_own_rows() {
        let p = principals();
        let owner_probe = KeyProbe {
            authenticated: true,
            kind: "owner".into(),
            name: "dashboard".into(),
            refused: false,
        };
        assert_eq!(kind_label(Identity::Key, Some(&owner_probe)), "owner key");
        assert_eq!(
            held_caps(Identity::Key, &p, true, Some(&owner_probe)),
            p.owner
        );

        let agent_probe = KeyProbe {
            authenticated: true,
            kind: "agent".into(),
            name: "folder-chat".into(),
            refused: false,
        };
        assert_eq!(kind_label(Identity::Key, Some(&agent_probe)), "agent key");
        assert_eq!(
            held_caps(Identity::Key, &p, true, Some(&agent_probe)),
            p.agent
        );
    }
}
