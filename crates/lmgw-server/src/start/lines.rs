//! What a start says about where it runs (headless server design D1, D2):
//! container mode with the runtime's marker beside it, or a dev instance outside
//! a container and the container variables it does not apply. Lines rather
//! than log calls, so the rules are tested on a stated environment.

use lmgw_core::config::deployment::{Deployment, Pinned};

use crate::env::ServerEnv;

/// Podman's marker file in a container's root.
pub const CONTAINERENV: &str = "/run/.containerenv";
/// Docker's marker file in a container's root (the image under Docker,
/// `packaging/server/docker/`).
pub const DOCKERENV: &str = "/.dockerenv";

/// The container runtime's marker file that is there, Podman's first.
pub fn container_marker() -> Option<&'static str> {
    [CONTAINERENV, DOCKERENV]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

/// A line of what a start says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Info(String),
    Warn(String),
}

impl Line {
    /// Said in the log at its level.
    pub fn log(&self) {
        match self {
            Line::Info(l) => tracing::info!("{l}"),
            Line::Warn(l) => tracing::warn!("{l}"),
        }
    }
}

/// What this start is, with `marker` the runtime's marker file that is
/// there ([`container_marker`]): container mode with the marker beside it,
/// which never decides (D2) but is warned about when neither is there,
/// since D1's refusal rests on the variable alone;
/// or a dev instance outside a container, and the container variables it
/// does not apply (`LMGW_BIND_ADDR` it does: it is the dev instance's
/// address).
pub fn deployment_lines(env: &ServerEnv, marker: Option<&str>) -> Vec<Line> {
    let mut lines = Vec::new();
    match env.deployment {
        Deployment::Container => {
            let said = match marker {
                Some(m) => format!("{m} present"),
                None => format!("{CONTAINERENV} and {DOCKERENV} absent"),
            };
            lines.push(Line::Info(format!(
                "container mode (LMGW_DEPLOYMENT=container; {said}){}",
                if env.dev {
                    ", as a dev instance (LMGW_DEV)"
                } else {
                    ""
                }
            )));
            if marker.is_none() {
                lines.push(Line::Warn(format!(
                    "LMGW_DEPLOYMENT=container, but neither {CONTAINERENV} (Podman) nor \
                     {DOCKERENV} (Docker) is there: this may not be a \
                     container. Container mode goes on as the variable says; outside the server \
                     image it shares this user's Podman, its image store and its containers \
                     under the production prefix with the tray app. Unset LMGW_DEPLOYMENT \
                     unless this is a container"
                )));
            }
        }
        Deployment::Desktop => {
            lines.push(Line::Info(
                "dev instance (LMGW_DEV) outside a container".to_string(),
            ));
            let ignored: Vec<&str> = env
                .container
                .set_variables()
                .into_iter()
                .filter(|v| *v != Pinned::BindAddr.variable())
                .collect();
            if !ignored.is_empty() {
                lines.push(Line::Warn(format!(
                    "{} apply in container mode only (LMGW_DEPLOYMENT=container); this start \
                     leaves the stored settings as they are",
                    ignored.join(", ")
                )));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_core::config::deployment::ContainerEnv;

    fn warnings(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Warn(w) => Some(w.as_str()),
                Line::Info(_) => None,
            })
            .collect()
    }

    #[test]
    fn container_mode_without_the_marker_warns() {
        let env = ServerEnv {
            deployment: Deployment::Container,
            ..Default::default()
        };
        // Either runtime's marker: Podman's, or Docker's (the image under Docker).
        for marker in [CONTAINERENV, DOCKERENV] {
            let lines = deployment_lines(&env, Some(marker));
            assert!(warnings(&lines).is_empty(), "{lines:?}");
            assert!(
                matches!(&lines[0], Line::Info(l) if l.contains(&format!("{marker} present"))),
                "{lines:?}"
            );
        }
        let lines = deployment_lines(&env, None);
        let warned = warnings(&lines);
        assert_eq!(warned.len(), 1, "{lines:?}");
        assert!(
            warned[0].contains("neither /run/.containerenv (Podman) nor /.dockerenv (Docker)")
                && warned[0].contains("tray app"),
            "{}",
            warned[0]
        );
    }

    #[test]
    fn a_dev_instance_outside_a_container_names_the_variables_it_ignores() {
        let mut env = ServerEnv {
            dev: true,
            container: ContainerEnv {
                bind_addr: Some("127.0.0.1:18801".parse().unwrap()),
                require_api_key: Some(true),
                stop_timeout: Some(95),
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = deployment_lines(&env, None);
        let warned = warnings(&lines);
        assert_eq!(warned.len(), 1, "{lines:?}");
        assert!(
            warned[0].starts_with(
                "LMGW_REQUIRE_API_KEY, LMGW_STOP_TIMEOUT apply in container mode only"
            ),
            "{}",
            warned[0]
        );
        // Its own address is no container variable here.
        env.container = ContainerEnv {
            bind_addr: Some("127.0.0.1:18801".parse().unwrap()),
            ..Default::default()
        };
        assert!(warnings(&deployment_lines(&env, None)).is_empty());
    }
}
