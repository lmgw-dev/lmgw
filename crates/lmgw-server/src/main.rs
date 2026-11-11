//! `lmgw-server` (headless server design §1): the command line, the
//! logging, the refusal outside a container, then [`lmgw_server::start`].

#[cfg(target_os = "linux")]
use std::io::IsTerminal;
#[cfg(target_os = "linux")]
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use lmgw_server::cli::{self, Command};
#[cfg(target_os = "linux")]
use lmgw_server::env::ServerEnv;
#[cfg(target_os = "linux")]
use lmgw_server::start;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    // 1. The machine check's NVML probe (`--machine-probe nvml`): the check
    // starts this program itself for it, and it answers before the async
    // runtime and its worker threads start.
    if let Some(code) = lmgw_core::machine::probe::run_if_asked() {
        std::process::exit(code);
    }
    let command = match cli::parse(std::env::args_os().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lmgw-server: {e}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Version => {
            println!("{}", cli::version_line());
            return ExitCode::SUCCESS;
        }
        Command::Help => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Command::Serve => {}
        Command::Account(cmd) => return account(cmd),
    }

    // 2. Logging, on stderr: what the service manager shows of a unit, so
    // colour escapes only when stderr is a terminal (the journal stores
    // them as they are).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lmgw_server=info,lmgw_core=info".into()),
        )
        .init();

    // 3. The environment, the refusal outside a container (D1), PID 1 (D5).
    let env = match ServerEnv::from_process() {
        Ok(env) => env,
        Err(e) => return fail(&e),
    };
    if let Some(why) = start::refusal(&env) {
        return fail(&why);
    }
    tracing::info!("{} starting", cli::version_line());
    if let Some(warning) = start::pid1_warning(std::process::id()) {
        tracing::warn!("{warning}");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(&format!("starting the async runtime: {e}")),
    };
    let code = runtime.block_on(async {
        // Before the first step that can take a while: a quit during the
        // start is said and handled, never the default's silent kill.
        let signals = start::quit_signals();
        let exit: start::Exit = Arc::new(|code| std::process::exit(code));
        match start::serve(env, lmgw_core::state::Host::real(), signals, exit).await {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("{e:#}");
                1
            }
        }
    });
    // Ended here rather than by returning: dropping the runtime waits for
    // every blocking task, and one stuck in a podman call or a file read
    // would hold the process past the stop budget the teardown kept to.
    std::process::exit(code)
}

/// One of §5.7's commands: run once on the data dir, printed, exit. The
/// start's refusals come first ([`lmgw_server::account::data_dir`]).
#[cfg(target_os = "linux")]
fn account(cmd: cli::Account) -> ExitCode {
    let data_dir =
        match ServerEnv::from_process().and_then(|env| lmgw_server::account::data_dir(&env)) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("lmgw-server: {e}");
                return ExitCode::FAILURE;
            }
        };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("lmgw-server: starting the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(lmgw_server::account::run(
        cmd,
        &data_dir,
        &mut lmgw_server::account::Stdin,
    )) {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("lmgw-server: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The start ends: said in the log, which is what the service manager shows
/// for a unit that failed.
#[cfg(target_os = "linux")]
fn fail(why: &str) -> ExitCode {
    tracing::error!("{why}");
    ExitCode::FAILURE
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("lmgw-server runs on Linux only (not on Windows yet)");
    std::process::ExitCode::FAILURE
}
