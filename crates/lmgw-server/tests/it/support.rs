//! What the in-process suites share: the host seams the core's tests use —
//! a podman that answers nothing and records every call, no GPU, a machine
//! with stated facts — and a container-mode environment on a scratch data
//! dir with its listener on a loopback port.

use std::sync::{Arc, Mutex};

use lmgw_core::config::deployment::{ContainerEnv, Deployment};
use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner};
use lmgw_core::state::Host;
use lmgw_core::vram::nvml::NoTelemetry;
use lmgw_server::env::ServerEnv;
use lmgw_server::start::Exit;

/// A podman with nothing in it; every call recorded.
#[derive(Default)]
pub struct EmptyPodman {
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl CommandRunner for EmptyPodman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(args.to_vec());
            let stdout = if args.first().map(String::as_str) == Some("ps") {
                "[]".to_string()
            } else {
                String::new()
            };
            Ok(CmdOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            })
        })
    }
}

/// The host seams with no wrapping key secret: its path is one that never
/// exists ([`host_with_secret`] for a test's own file).
pub fn host(podman: &Arc<EmptyPodman>) -> Host {
    host_with_secret(podman, std::path::Path::new("/nonexistent/lmgw-wrap-key"))
}

/// The host seams, the wrapping key secret read from `secret`.
pub fn host_with_secret(podman: &Arc<EmptyPodman>, secret: &std::path::Path) -> Host {
    Host {
        runner: podman.clone(),
        probe: Arc::new(NoTelemetry("test: no GPU".into())),
        machine: Arc::new(lmgw_machine::fake::FakeFacts::new()),
        wrap_key_secret: secret.to_path_buf(),
    }
}

/// An exit that records its code instead of ending the test.
pub fn recorder() -> (Exit, Arc<Mutex<Vec<i32>>>) {
    let codes: Arc<Mutex<Vec<i32>>> = Arc::default();
    let exit: Exit = Arc::new({
        let codes = codes.clone();
        move |code| codes.lock().unwrap().push(code)
    });
    (exit, codes)
}

/// A container-mode start on `data` with its plain listener on a loopback
/// port the OS picks.
pub fn container_env(data: &std::path::Path, dev: bool) -> ServerEnv {
    ServerEnv {
        deployment: Deployment::Container,
        dev,
        data_dir: Some(data.to_path_buf()),
        dev_container_prefix: None,
        container: ContainerEnv {
            bind_addr: Some("127.0.0.1:0".parse().unwrap()),
            stop_timeout: Some(95),
            ..Default::default()
        },
    }
}
