//! A stand-in for llama-server that needs nothing downloaded: what
//! `tests/it/runtime_launcher_native.rs` starts through the real native
//! launcher. It reads the flags lmgw renders (`--host`, `--port`) and a few
//! of its own, passed as a row's freeform args:
//!
//! - `--fake-load-ms <n>`: `/health` answers 503 for this long, then 200;
//! - `--fake-exit <code>`: says why on stderr and exits with `code` after a
//!   moment, without serving;
//! - `--fake-say <text>`: prints `text` on stderr first (a line a test wants
//!   in the log, such as CUDA's "no kernel image" without the exit);
//! - `--fake-compute-fails`: serves, and the first request that is not
//!   `/health` prints ggml's "CUDA error: no kernel image is available …"
//!   and exits 1, as a CUDA build without kernels does at its first compute;
//! - `--fake-alloc-mib <n>`: commits and touches `n` MiB at the start, and
//!   says whether it could (a job's memory limit, Windows design §4.4);
//!
//! A build directory (its working directory) holding a file
//! `fake-no-kernel-image` plays a CUDA build without kernels for the card:
//! it prints CUDA's "no kernel image is available" and exits 1
//! (`tests/it/runtime_engine_assets.rs`, the Vulkan fallback).
//!
//! On Windows it also says the limits of the job it is in (its immediate
//! one, the limits job when it has its own): the CPU rate cap and the job
//! memory limit, or none.
//!
//! A taken port is reported in llama-server's own words (`couldn't bind HTTP
//! server socket`) with exit code 1, as b12236 does. Every request is a line
//! on stdout. std only (and on Windows `windows-sys` for the job's limits),
//! so it builds wherever lmgw-core does.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let host = value("--host").unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = value("--port").and_then(|p| p.parse().ok()).unwrap_or(8080);
    let load = Duration::from_millis(
        value("--fake-load-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
    println!("fake engine: argv {}", args.join(" "));
    println!("fake engine: pid {}", std::process::id());
    for var in [
        "CUDA_VISIBLE_DEVICES",
        "HIP_VISIBLE_DEVICES",
        "GGML_VK_VISIBLE_DEVICES",
        "LMGW_FAKE_MARK",
    ] {
        if let Ok(v) = std::env::var(var) {
            println!("fake engine: env {var}={v}");
        }
    }
    if let Ok(dir) = std::env::current_dir() {
        println!("fake engine: cwd {}", dir.display());
    }
    #[cfg(windows)]
    report_job();
    if let Some(text) = value("--fake-say") {
        eprintln!("{text}");
    }
    let compute_fails = args.iter().any(|a| a == "--fake-compute-fails");

    if std::path::Path::new("fake-no-kernel-image").exists() {
        eprintln!("[E] ggml: CUDA error: no kernel image is available for execution on the device");
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(1);
    }

    if let Some(mib) = value("--fake-alloc-mib").and_then(|v| v.parse::<usize>().ok()) {
        let mut block: Vec<u8> = Vec::new();
        match block.try_reserve_exact(mib << 20) {
            Ok(()) => {
                block.resize(mib << 20, 1);
                println!("fake engine: allocated {mib} MiB");
            }
            Err(_) => println!("fake engine: could not allocate {mib} MiB"),
        }
    }

    if let Some(code) = value("--fake-exit").and_then(|v| v.parse::<i32>().ok()) {
        eprintln!("error: the fake engine was told to fail while loading");
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(code);
    }

    let listener = match TcpListener::bind((host.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("couldn't bind HTTP server socket, hostname: {host}, port: {port} ({e})");
            std::process::exit(1);
        }
    };
    println!("fake engine: listening on http://{host}:{port}");
    let started = Instant::now();
    for stream in listener.incoming().flatten() {
        let loaded = started.elapsed() >= load;
        std::thread::spawn(move || serve(stream, loaded, compute_fails));
    }
}

fn serve(mut stream: TcpStream, loaded: bool, compute_fails: bool) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    // The headers, up to the blank line.
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
        line.clear();
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    println!("fake engine: request {}", request.trim());
    if compute_fails && path != "/health" {
        eprintln!("CUDA error: no kernel image is available for execution on the device");
        eprintln!("  current device: 0, in function ggml_cuda_compute_forward");
        std::process::exit(1);
    }
    let (status, body) = match path.as_str() {
        "/health" if loaded => ("200 OK", r#"{"status":"ok"}"#),
        "/health" => ("503 Service Unavailable", r#"{"error":"loading"}"#),
        _ => ("404 Not Found", r#"{"error":"not found"}"#),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The limits of the job this process is in (module doc): a null job handle
/// asks about the caller's own, the innermost when jobs are nested.
#[cfg(windows)]
fn report_job() {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
        JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
    };
    fn query<T: Default>(class: i32) -> Result<T, std::io::Error> {
        let mut info = T::default();
        // SAFETY: `info` is the struct `class` names, at its own size.
        let ok = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                class,
                (&mut info as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info)
    }
    let capped = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
    match query::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>(JobObjectCpuRateControlInformation) {
        // SAFETY: with these flags the union holds CpuRate.
        Ok(i) if i.ControlFlags & capped == capped => {
            println!("fake engine: job cpu rate {} hard cap", unsafe {
                i.Anonymous.CpuRate
            })
        }
        Ok(_) => println!("fake engine: job cpu rate none"),
        Err(e) => println!("fake engine: job cpu rate not read: {e}"),
    }
    match query::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(JobObjectExtendedLimitInformation) {
        Ok(i) if i.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0 => {
            println!("fake engine: job memory limit {}", i.JobMemoryLimit)
        }
        Ok(_) => println!("fake engine: job memory limit none"),
        Err(e) => println!("fake engine: job memory limit not read: {e}"),
    }
}
