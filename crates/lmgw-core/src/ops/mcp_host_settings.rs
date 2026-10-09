//! Settings → MCP: the device host link's limits (client-apps design §5.1),
//! `mcp.host_max_message_mb`, `mcp.host_max_frame_mb` and
//! `mcp.host_ping_interval_s`, which apply to links opened after a change;
//! and MCP Tasks' `mcp.task_poll_interval_s` (MCP Tasks design §5.2), which
//! applies from each task's next poll.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::McpSettings;

/// The sparse patch the dashboard's settings save takes under `mcp`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettingsPatch {
    /// In MiB; `0` = no bound of its own. Not `0` together with
    /// `host_max_frame_mb`.
    pub host_max_message_mb: Option<u32>,
    /// In MiB; `0` = bounded by `host_max_message_mb`. Not `0` together with
    /// it.
    pub host_max_frame_mb: Option<u32>,
    /// Seconds between the link's pings; `0` = none.
    pub host_ping_interval_s: Option<u32>,
    /// Seconds between two `tasks/get` of a task whose server suggests no
    /// `pollInterval`; at least 1.
    pub task_poll_interval_s: Option<u32>,
}

/// Apply `p` to `m`, or say why not (nothing is applied then).
pub fn apply_mcp_settings(m: &mut McpSettings, p: McpSettingsPatch) -> Result<(), String> {
    let mut next = *m;
    if let Some(v) = p.host_max_message_mb {
        next.host_max_message_mb = v;
    }
    if let Some(v) = p.host_max_frame_mb {
        next.host_max_frame_mb = v;
    }
    if let Some(v) = p.host_ping_interval_s {
        next.host_ping_interval_s = v;
    }
    if let Some(v) = p.task_poll_interval_s {
        if v == 0 {
            return Err(
                "mcp.task_poll_interval_s must be at least 1 (seconds between two \
                        tasks/get of a task whose server suggests no pollInterval)"
                    .into(),
            );
        }
        next.task_poll_interval_s = v;
    }
    if next.host_max_message_mb == 0 && next.host_max_frame_mb == 0 {
        return Err(
            "mcp.host_max_message_mb and mcp.host_max_frame_mb cannot both be 0 — a frame would \
             then be unbounded, and the WebSocket library reserves a frame's declared size \
             before reading it"
                .into(),
        );
    }
    *m = next;
    Ok(())
}

/// `mcp` as `GET /api/settings-full` reports it.
pub fn mcp_settings_view(m: &McpSettings) -> Value {
    json!(lmgw_api_types::mcp_host::HostSettings {
        host_max_message_mb: m.host_max_message_mb,
        host_max_frame_mb: m.host_max_frame_mb,
        host_ping_interval_s: m.host_ping_interval_s,
        task_poll_interval_s: m.task_poll_interval_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_limits_off_is_refused_and_nothing_moves() {
        let mut m = McpSettings::default();
        let e = apply_mcp_settings(
            &mut m,
            McpSettingsPatch {
                host_max_message_mb: Some(0),
                host_max_frame_mb: Some(0),
                host_ping_interval_s: Some(5),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("mcp.host_max_message_mb"), "{e}");
        assert_eq!(m, McpSettings::default());
        apply_mcp_settings(
            &mut m,
            McpSettingsPatch {
                host_max_message_mb: Some(0),
                host_ping_interval_s: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((m.host_max_message_mb, m.host_ping_interval_s), (0, 5));
    }

    #[test]
    fn the_task_poll_interval_is_at_least_one_second() {
        let mut m = McpSettings::default();
        let e = apply_mcp_settings(
            &mut m,
            McpSettingsPatch {
                task_poll_interval_s: Some(0),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("mcp.task_poll_interval_s"), "{e}");
        assert_eq!(m, McpSettings::default());
        apply_mcp_settings(
            &mut m,
            McpSettingsPatch {
                task_poll_interval_s: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(m.task_poll_interval_s, 2);
        assert_eq!(mcp_settings_view(&m)["task_poll_interval_s"], 2);
    }
}
