//! What a start says about where it runs (headless server design D1, D2):
//! container mode with Podman's marker beside it, or a dev instance outside
//! a container and the container variables it does not apply. Lines rather
//! than log calls, so the rules are tested on a stated environment.

use lmgw_core::config::deployment::{Deployment, Pinned};

use crate::env::ServerEnv;

/// Podman's marker file in a container's root.
pub const CONTAINERENV: &str = "/run/.containerenv";

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

/// What this start is, with [`CONTAINERENV`] `present` or not: container
/// mode with the marker beside it, which never decides (D2) but is warned
/// about when it is absent, since D1's refusal rests on the variable alone;
/// or a dev instance outside a container, and the container variables it
/// does not apply (`LMGW_BIND_ADDR` it does: it is the dev instance's
/// address).
pub fn deployment_lines(env: &ServerEnv, containerenv_present: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    match env.deployment {
        Deployment::Container => {
            let marker = if containerenv_present {
                "present"
            } else {
                "absent"
            };
            lines.push(Line::Info(format!(
                "container mode (LMGW_DEPLOYMENT=container; {CONTAINERENV} {marker}){}",
                if env.dev {
                    ", as a dev instance (LMGW_DEV)"
                } else {
                    ""
                }
            )));
            if !containerenv_present {
                lines.push(Line::Warn(format!(
                    "LMGW_DEPLOYMENT=container, but {CONTAINERENV} is absent: this may not be a \
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
        assert!(warnings(&deployment_lines(&env, true)).is_empty());
        let lines = deployment_lines(&env, false);
        let warned = warnings(&lines);
        assert_eq!(warned.len(), 1, "{lines:?}");
        assert!(
            warned[0].contains("/run/.containerenv is absent") && warned[0].contains("tray app"),
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
        let lines = deployment_lines(&env, false);
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
        assert!(warnings(&deployment_lines(&env, false)).is_empty());
    }
}
