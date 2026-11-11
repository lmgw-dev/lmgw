//! Where an engine sees its files and which address it listens on — the one
//! input of the engine argv that depends on how the engine is launched
//! (Windows design §4.1).

/// The place a model's engine runs in, as its argv names it.
///
/// A container sees the models directory at `/models`, its config directory
/// at `/config` and listens on `0.0.0.0:8080` inside its own network
/// namespace, the published host port mapping to it ([`Self::container`]).
/// A launcher that starts the engine as a host process names the host
/// directories and the host port instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineView {
    /// Where the models directory is, as the engine sees it.
    pub models_dir: String,
    /// Where the per-model config directory is (audio.cpp's `server.json`).
    pub config_dir: String,
    /// The address the server binds.
    pub listen_host: String,
    /// The port the server binds.
    pub listen_port: u16,
    /// The engine is reached through its image's entrypoint, not as its own
    /// executable. audio.cpp's image takes the server as a subcommand
    /// (`server --config …`); `audiocpp_server` itself takes its flags
    /// directly.
    pub in_container: bool,
}

impl EngineView {
    /// The server's port inside a model's container: fixed, because the
    /// published host port (§3.5) is what varies per model.
    pub const CONTAINER_PORT: u16 = 8080;

    /// A model's container (module doc).
    pub fn container() -> Self {
        Self {
            models_dir: "/models".into(),
            config_dir: "/config".into(),
            listen_host: "0.0.0.0".into(),
            listen_port: Self::CONTAINER_PORT,
            in_container: true,
        }
    }

    /// An engine started as a host process: the host's models directory and
    /// config directory, listening on the host's loopback at `port`.
    pub fn host(models_dir: &str, config_dir: &str, port: u16) -> Self {
        Self {
            models_dir: models_dir.into(),
            config_dir: config_dir.into(),
            listen_host: "127.0.0.1".into(),
            listen_port: port,
            in_container: false,
        }
    }

    /// A models-dir-relative path, as the engine sees it.
    pub fn model_path(&self, p: &str) -> String {
        format!(
            "{}/{}",
            self.models_dir.trim_end_matches('/'),
            p.trim_start_matches('/')
        )
    }

    /// A file in the config directory, as the engine sees it.
    pub fn config_path(&self, file: &str) -> String {
        format!("{}/{file}", self.config_dir.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::EngineView;

    #[test]
    fn the_container_view_is_the_mounts() {
        let v = EngineView::container();
        assert_eq!(v.model_path("/a/b.gguf"), "/models/a/b.gguf");
        assert_eq!(v.model_path("b.gguf"), "/models/b.gguf");
        assert_eq!(v.config_path("server.json"), "/config/server.json");
    }

    #[test]
    fn a_host_view_joins_onto_its_own_dirs() {
        let v = EngineView::host("/srv/models/", "/var/lib/lmgw/audio/x", 41234);
        assert_eq!(v.model_path("a.gguf"), "/srv/models/a.gguf");
        assert_eq!(
            v.config_path("server.json"),
            "/var/lib/lmgw/audio/x/server.json"
        );
    }
}
