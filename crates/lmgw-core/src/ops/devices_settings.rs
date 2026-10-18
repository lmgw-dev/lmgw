//! Settings → Network & access → Devices (device link design §9.1): the
//! bounded numbers of the `devices` class, applied and checked here for both
//! save paths — the `settings_set` op and the dashboard's `settings_set_full`
//! — so the two cannot drift. Every one is refused by name outside its
//! stated range, and the save's answer says when each change applies.
//!
//! The device listener's switch, scope and port (`devices.listener`,
//! `devices.listen`, `devices.port`) act at once, binding, moving or closing
//! its sockets (§4.1): the save stores them, then
//! [`apply_devices_listener`] has the running listener follow. The
//! dashboard asks its confirmation before it sends one (the connections
//! that end, the exposure of a wider scope). The first save that turns the
//! listener on writes the port it will use (Q1). No self-admin tool changes
//! any `devices` setting: it is an access setting
//! (`mcp::selfadmin::ACCESS_SETTINGS`).

use lmgw_api_types::device_link as wire;
use serde::Deserialize;

use crate::config::{DevicesListen, DevicesSettings, DEVICES_BOUNDS};

/// The sparse patch both saves take under `devices`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DevicesSettingsPatch {
    /// The device listener: on or off. Applies at once: on binds it (making
    /// lmgw's device CA on its first start), off closes it and ends its
    /// connections — its feeds, realtime sessions and calls — never the
    /// plain listener's.
    pub listener: Option<bool>,
    /// Where it binds: `loopback` (127.0.0.1 and [::1]), `all` ([::], every
    /// address) or one IP address. Applies at once, moving it and ending its
    /// connections.
    pub listen: Option<String>,
    /// Its port, 1 to 65535. Applies at once, moving it and ending its
    /// connections. Unset, it is the HTTP port's number plus one, written
    /// here the first time the listener is turned on.
    pub port: Option<i64>,
    /// A pairing token's lifetime in minutes, 1 to 1440. Applies to the next
    /// token.
    pub token_minutes: Option<i64>,
    /// A device certificate's validity in days, 1 to 3650; the notice comes
    /// at two thirds of it. Applies to the next certificate issued.
    pub client_cert_days: Option<i64>,
    /// A server certificate's validity in days, 1 to 3650; it is re-issued
    /// from half of it. Applies to the next certificate issued.
    pub server_cert_days: Option<i64>,
    /// A TLS handshake's bound in seconds, 1 to 300. Applies to the next
    /// connection.
    pub handshake_timeout_s: Option<i64>,
    /// A request head's bound, and an idle kept-alive connection's, in
    /// seconds, 1 to 3600. Applies to the next connection.
    pub header_timeout_s: Option<i64>,
    /// A whole connection without a client certificate, in seconds, 1 to
    /// 300. Applies to the next connection.
    pub unauthenticated_timeout_s: Option<i64>,
    /// Failed enrolments per address (an IPv6 address by its /64) per
    /// minute before a 429, 1 to 100000. Applies at once.
    pub enrol_refuse_per_min: Option<i64>,
    /// The device listener's connections that have not authenticated yet
    /// (handshakes in progress included), at most, from every peer
    /// together, 1 to 4096; a new one past it closes the oldest. Applies to
    /// the next connection.
    pub unauthenticated_max: Option<i64>,
    /// The same, from one address (an IPv6 address by its /64), 1 to 4096;
    /// a new one past it closes that address's oldest. Applies to the next
    /// connection.
    pub unauthenticated_per_peer: Option<i64>,
}

impl DevicesSettingsPatch {
    /// The value this patch gives `key`, if any.
    fn value(&self, key: &str) -> Option<i64> {
        match key {
            "token_minutes" => self.token_minutes,
            "client_cert_days" => self.client_cert_days,
            "server_cert_days" => self.server_cert_days,
            "handshake_timeout_s" => self.handshake_timeout_s,
            "header_timeout_s" => self.header_timeout_s,
            "unauthenticated_timeout_s" => self.unauthenticated_timeout_s,
            "enrol_refuse_per_min" => self.enrol_refuse_per_min,
            "unauthenticated_max" => self.unauthenticated_max,
            "unauthenticated_per_peer" => self.unauthenticated_per_peer,
            _ => None,
        }
    }
}

/// One `devices` setting a save changed: its full key and when it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicesChange {
    /// `devices.<key>`.
    pub key: String,
    pub applies: &'static str,
}

impl DevicesChange {
    /// The save's note: "devices.token_minutes applies to the next pairing
    /// token".
    pub fn note(&self) -> String {
        format!("{} {}", self.key, self.applies)
    }
}

/// `v` for the setting `key`, within its stated range, or the refusal that
/// names both.
fn bounded(key: &str, what: &str, b: wire::Bound, v: i64) -> Result<u32, String> {
    u32::try_from(v)
        .ok()
        .filter(|n| (b.min..=b.max).contains(n))
        .ok_or_else(|| {
            format!(
                "devices.{key} must be from {} to {} ({what}); got {v}",
                b.min, b.max
            )
        })
}

/// When a change of the listener's switch, scope or port applies.
const LISTENER_APPLIES: &str = "applies at once";

/// The listener's three settings in `p`, applied to `next`.
fn apply_listener(
    next: &mut DevicesSettings,
    p: &DevicesSettingsPatch,
    bind_addr: &str,
    changed: &mut Vec<DevicesChange>,
) -> Result<(), String> {
    let mut change = |key: &str| {
        changed.push(DevicesChange {
            key: format!("devices.{key}"),
            applies: LISTENER_APPLIES,
        })
    };
    if let Some(v) = p.listener {
        next.listener = v;
        change("listener");
    }
    if let Some(v) = p.listen.as_deref() {
        next.listen = DevicesListen::parse(v).ok_or_else(|| {
            format!(
                "devices.listen must be loopback, all or an IP address (where the device \
                 listener binds); got '{}'",
                v.trim()
            )
        })?;
        change("listen");
    }
    if let Some(v) = p.port {
        let port = u16::try_from(v).ok().filter(|n| *n >= 1).ok_or_else(|| {
            format!("devices.port must be from 1 to 65535 (the device listener's port); got {v}")
        })?;
        next.port = Some(port);
        change("port");
    }
    // The first time it is turned on, its port is written as a number
    // (§4.1, Q1): a later change of bind_addr's port never moves it.
    if next.listener && next.port.is_none() {
        match next.effective_port(bind_addr) {
            Ok(port) => {
                next.port = Some(port);
                changed.push(DevicesChange {
                    key: "devices.port".into(),
                    applies: "is written as the HTTP port's number plus one: the device \
                              listener's port from now on",
                });
            }
            // A save of the listener's own settings is refused by name; any
            // other save leaves it to the listener's start, which says it.
            Err(e) if touches_listener(p) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(())
}

/// Whether `p` changes the device listener's switch, scope or port: a save
/// of it then has the running listener follow
/// ([`apply_devices_listener`]).
pub fn touches_listener(p: &DevicesSettingsPatch) -> bool {
    p.listener.is_some() || p.listen.is_some() || p.port.is_some()
}

/// Have the running device listener follow the settings just saved: start,
/// move or stop it. The sentence the save's answer adds, if any.
///
/// Where it says the device listener listens, it also says a failed UDP
/// group for calls (nothing binds it again by itself; the save is the
/// retry).
pub async fn apply_devices_listener(state: &crate::state::SharedState) -> Option<String> {
    use crate::realtime::calls::{Group, GroupState};
    use crate::server::device_listener::Applied;
    // A save of the same settings changes nothing for the listener and
    // wakes the follower of a failed group (the retry): what failed is read
    // before, as the retry has begun by the time `reconcile` returns.
    let failed_before = match state.calls.group(Group::Devices) {
        GroupState::Failed(why) => Some(why),
        _ => None,
    };
    let applied = state.devices.listener.reconcile(state).await;
    let note = applied.note();
    let failed = match applied {
        Applied::Unchanged => failed_before,
        Applied::Listening { .. } => match state.calls.group(Group::Devices) {
            GroupState::Failed(why) => Some(why),
            _ => None,
        },
        _ => None,
    };
    match failed {
        Some(why) => {
            let calls = format!(
                "its UDP group for WebRTC calls is failed ({why}); this save tried to bind it \
                 again"
            );
            Some(match note {
                Some(n) => format!("{n}; {calls}"),
                None => format!("the device listener serves, but {calls}"),
            })
        }
        None => note,
    }
}

/// Apply `p` to `d`: what it changed, or the first refusal (nothing is
/// applied then). `bind_addr` is the one the save leaves: the first start's
/// port is its number plus one.
pub fn apply_devices(
    d: &mut DevicesSettings,
    p: DevicesSettingsPatch,
    bind_addr: &str,
) -> Result<Vec<DevicesChange>, String> {
    let mut next = d.clone();
    let mut changed = Vec::new();
    apply_listener(&mut next, &p, bind_addr, &mut changed)?;
    for b in &DEVICES_BOUNDS {
        if let Some(v) = p.value(b.key) {
            *(b.field)(&mut next) = bounded(b.key, b.what, b.bound, v)?;
            changed.push(DevicesChange {
                key: format!("devices.{}", b.key),
                applies: b.applies,
            });
        }
    }
    *d = next;
    Ok(changed)
}

/// Stored values a save would refuse now (written by hand, or before a
/// range moved) are clamped into range on load, with a warning naming the
/// setting, rather than used out of range or failing every start.
pub fn clamp_loaded_devices(d: &mut DevicesSettings) {
    for b in &DEVICES_BOUNDS {
        let v = (b.field)(d);
        let fixed = (*v).clamp(b.bound.min, b.bound.max);
        if fixed != *v {
            tracing::warn!(
                "stored setting devices.{} = {} is out of range ({} to {}: {}); using {fixed} \
                 until it is saved again",
                b.key,
                *v,
                b.bound.min,
                b.bound.max,
                b.what
            );
            *v = fixed;
        }
    }
    // Port 0 is no port a device could be given: the default stands in.
    if d.port == Some(0) {
        tracing::warn!(
            "stored setting devices.port = 0 is no port; using the HTTP port's number plus one \
             until it is saved again"
        );
        d.port = None;
    }
}

/// `devices` as `GET /api/settings-full` reports it; `bind_addr` gives the
/// port a `null` `devices.port` stands for.
pub fn devices_settings_view(d: &DevicesSettings, bind_addr: &str) -> wire::DevicesSettings {
    wire::DevicesSettings {
        listener: d.listener,
        listen: d.listen.as_text(),
        port: d.port,
        port_effective: d.effective_port(bind_addr).ok(),
        token_minutes: d.token_minutes,
        client_cert_days: d.client_cert_days,
        server_cert_days: d.server_cert_days,
        handshake_timeout_s: d.handshake_timeout_s,
        header_timeout_s: d.header_timeout_s,
        unauthenticated_timeout_s: d.unauthenticated_timeout_s,
        enrol_refuse_per_min: d.enrol_refuse_per_min,
        unauthenticated_max: d.unauthenticated_max,
        unauthenticated_per_peer: d.unauthenticated_per_peer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIND: &str = "127.0.0.1:8787";

    #[test]
    fn the_listener_s_settings_are_checked_and_the_first_on_writes_its_port() {
        let mut d = DevicesSettings::default();
        let changed = apply_devices(
            &mut d,
            DevicesSettingsPatch {
                listener: Some(true),
                ..Default::default()
            },
            BIND,
        )
        .unwrap();
        assert!(d.listener);
        assert_eq!(d.port, Some(8788), "the HTTP port's number plus one");
        assert_eq!(
            changed.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            ["devices.listener", "devices.port"]
        );
        // Written once: a later bind_addr leaves it.
        apply_devices(
            &mut d,
            DevicesSettingsPatch {
                listen: Some("all".into()),
                ..Default::default()
            },
            "127.0.0.1:9999",
        )
        .unwrap();
        assert_eq!((d.port, d.listen), (Some(8788), DevicesListen::All));
        let before = d.clone();
        for (p, words) in [
            (
                DevicesSettingsPatch {
                    listen: Some("everywhere".into()),
                    ..Default::default()
                },
                "devices.listen must be loopback, all or an IP address",
            ),
            (
                DevicesSettingsPatch {
                    port: Some(0),
                    ..Default::default()
                },
                "devices.port must be from 1 to 65535",
            ),
            (
                DevicesSettingsPatch {
                    port: Some(65536),
                    ..Default::default()
                },
                "devices.port must be from 1 to 65535",
            ),
        ] {
            let e = apply_devices(&mut d, p, BIND).unwrap_err();
            assert!(e.starts_with(words), "{e}");
            assert_eq!(d, before);
        }
        // No port above 65535 to derive: refused, naming the setting.
        let mut off = DevicesSettings::default();
        let e = apply_devices(
            &mut off,
            DevicesSettingsPatch {
                listener: Some(true),
                ..Default::default()
            },
            "127.0.0.1:65535",
        )
        .unwrap_err();
        assert!(e.contains("set devices.port"), "{e}");
        assert!(!off.listener);
        assert!(touches_listener(&DevicesSettingsPatch {
            port: Some(9000),
            ..Default::default()
        }));
        assert!(!touches_listener(&DevicesSettingsPatch {
            token_minutes: Some(9),
            ..Default::default()
        }));
    }

    #[test]
    fn each_bound_is_checked_by_name_and_nothing_moves_on_a_refusal() {
        let mut d = DevicesSettings::default();
        let changed = apply_devices(
            &mut d,
            DevicesSettingsPatch {
                token_minutes: Some(1440),
                server_cert_days: Some(1),
                ..Default::default()
            },
            BIND,
        )
        .unwrap();
        assert_eq!((d.token_minutes, d.server_cert_days), (1440, 1));
        assert_eq!(
            changed.iter().map(DevicesChange::note).collect::<Vec<_>>(),
            [
                "devices.token_minutes applies to the next pairing token",
                "devices.server_cert_days applies to the next server certificate issued",
            ]
        );
        let before = d.clone();
        for b in &DEVICES_BOUNDS {
            for bad in [i64::from(b.bound.min) - 1, i64::from(b.bound.max) + 1] {
                let mut p = DevicesSettingsPatch {
                    // A valid change beside the bad one: refused together.
                    token_minutes: Some(5),
                    ..Default::default()
                };
                match b.key {
                    "token_minutes" => p.token_minutes = Some(bad),
                    "client_cert_days" => p.client_cert_days = Some(bad),
                    "server_cert_days" => p.server_cert_days = Some(bad),
                    "handshake_timeout_s" => p.handshake_timeout_s = Some(bad),
                    "header_timeout_s" => p.header_timeout_s = Some(bad),
                    "unauthenticated_timeout_s" => p.unauthenticated_timeout_s = Some(bad),
                    "enrol_refuse_per_min" => p.enrol_refuse_per_min = Some(bad),
                    "unauthenticated_max" => p.unauthenticated_max = Some(bad),
                    "unauthenticated_per_peer" => p.unauthenticated_per_peer = Some(bad),
                    other => panic!("{other} has no patch field"),
                }
                let e = apply_devices(&mut d, p, BIND).unwrap_err();
                assert!(
                    e.starts_with(&format!(
                        "devices.{} must be from {} to {}",
                        b.key, b.bound.min, b.bound.max
                    )),
                    "{e}"
                );
                assert_eq!(d, before, "{e}");
            }
        }
    }

    #[test]
    fn out_of_range_stored_values_are_clamped_on_load() {
        let mut d = DevicesSettings {
            token_minutes: 0,
            enrol_refuse_per_min: 1_000_000,
            ..Default::default()
        };
        clamp_loaded_devices(&mut d);
        assert_eq!((d.token_minutes, d.enrol_refuse_per_min), (1, 100_000));
        let mut fine = DevicesSettings::default();
        clamp_loaded_devices(&mut fine);
        assert_eq!(fine, DevicesSettings::default());
    }

    #[test]
    fn the_view_says_which_port_a_null_stands_for() {
        let d = DevicesSettings::default();
        assert_eq!(
            devices_settings_view(&d, "127.0.0.1:8787").port_effective,
            Some(8788)
        );
        assert_eq!(devices_settings_view(&d, "[::]:65535").port_effective, None);
        assert!(d
            .effective_port("[::]:65535")
            .unwrap_err()
            .contains("65535"));
        let set = DevicesSettings {
            port: Some(9000),
            ..Default::default()
        };
        assert_eq!(
            devices_settings_view(&set, "127.0.0.1:8787").port_effective,
            Some(9000)
        );
    }
}
