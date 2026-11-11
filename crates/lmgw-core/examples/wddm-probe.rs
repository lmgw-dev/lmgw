//! Per-process GPU memory on Windows from D3DKMT, beside NVML's device totals
//! (Windows design §4.3): what `vram::wddm` reads, printed for a person.
//!
//! ```text
//! cargo run --example wddm-probe --features lmgw-core/opus-bundled -- [--every-ms MS --times N] [PID ...]
//! ```
//!
//! Without PIDs it lists the WDDM adapters (LUID, PCI address, every segment
//! with its commit limit and what all processes together committed), NVML's
//! devices (total, used, free, PCI address) and NVML's own process list.
//! With PIDs it adds, per PID and counted adapter, the bytes committed in each
//! local and aperture segment, the local segment group's usage and budget,
//! and the sum `vram::wddm::processes` answers; `--every-ms` repeats the
//! NVML used figure and those sums `--times` times. Windows only (the
//! `opus-bundled` feature lets the example link where no `opus.lib` is
//! installed).

#[cfg(not(windows))]
fn main() {
    eprintln!("wddm-probe reads Windows' D3DKMT statistics; it runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(e) = windows::run() {
        eprintln!("wddm-probe: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows {
    use lmgw_core::vram::wddm::{self, d3dkmt, Adapter};
    use lmgw_core::vram::{GpuProbe, NvmlProbe};

    fn mib(b: u64) -> String {
        format!("{:.1} MiB ({b} B)", b as f64 / 1048576.0)
    }

    struct Args {
        pids: Vec<u32>,
        every_ms: Option<u64>,
        times: u64,
    }

    fn args() -> Result<Args, String> {
        let mut a = Args {
            pids: Vec::new(),
            every_ms: None,
            times: 1,
        };
        let mut it = std::env::args().skip(1);
        while let Some(w) = it.next() {
            let mut num = |name: &str| -> Result<u64, String> {
                it.next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| format!("{name} needs a number"))
            };
            match w.as_str() {
                "--every-ms" => a.every_ms = Some(num("--every-ms")?),
                "--times" => a.times = num("--times")?,
                pid => a
                    .pids
                    .push(pid.parse().map_err(|_| format!("not a PID: {pid}"))?),
            }
        }
        Ok(a)
    }

    pub fn run() -> Result<(), String> {
        let args = args()?;
        let nvml = NvmlProbe::detect();
        println!("NVML: {}", nvml.source());
        let devices = nvml.devices().unwrap_or_default();
        let pcis = nvml.pci_addresses().unwrap_or_default();
        for (d, p) in devices.iter().zip(pcis.iter()) {
            println!(
                "  device {} {} at PCI {p}: total {}, used {}, free {}",
                d.index,
                d.name,
                mib(d.total_bytes),
                mib(d.used_bytes),
                mib(d.free_bytes)
            );
        }
        match nvml.processes(&[], &[]) {
            Ok(list) => {
                for p in list {
                    let bytes = p.bytes.map_or("[N/A]".to_string(), mib);
                    println!("  NVML process {}: {bytes}", p.pid);
                }
            }
            Err(e) => println!("  NVML processes: {e}"),
        }

        let all = d3dkmt::adapters()?;
        println!(
            "D3DKMT adapters: {} read, {} not readable",
            all.listed.len(),
            all.failed.len()
        );
        for f in &all.failed {
            let pci = f
                .pci
                .map_or("PCI address unknown".into(), |p| p.to_string());
            println!("  adapter LUID {:#018x} at {pci}: {}", f.luid, f.why);
        }
        let adapters = &all.listed;
        for a in adapters {
            let pci = a.pci.map_or("no PCI address".into(), |p| p.to_string());
            println!("  adapter LUID {:#018x} at {pci}", a.luid);
            for s in &a.segments {
                println!(
                    "    segment {} {}: commit limit {}, committed {}",
                    s.id,
                    if s.aperture { "aperture" } else { "local" },
                    mib(s.commit_limit),
                    mib(s.committed)
                );
            }
        }
        let counted: Vec<&Adapter> = match wddm::counted(&all, &pcis) {
            Ok(c) => c,
            Err(e) => {
                println!("counted adapters: {e}");
                Vec::new()
            }
        };
        println!(
            "counted adapters (NVML's devices): {}",
            counted
                .iter()
                .map(|a| format!("{:#018x}", a.luid))
                .collect::<Vec<_>>()
                .join(", ")
        );

        for &pid in &args.pids {
            println!("PID {pid}, NTSTATUS of each process query type, by the handle's access:");
            for (name, access) in [
                ("QUERY_LIMITED_INFORMATION", 0x1000u32),
                ("QUERY_INFORMATION", 0x0400),
                ("QUERY_INFORMATION|VM_READ", 0x0410),
                ("ALL_ACCESS", 0x001F_FFFF),
            ] {
                let Ok(p) = d3dkmt::Process::open_with(pid, access) else {
                    println!("  {name}: OpenProcess failed");
                    continue;
                };
                let a = counted.first().copied().or(adapters.first());
                let Some(a) = a else { continue };
                let statuses: Vec<String> = [
                    (1, "PROCESS"),
                    (2, "PROCESS_ADAPTER"),
                    (4, "PROCESS_SEGMENT"),
                    (9, "PROCESS_SEGMENT_GROUP"),
                ]
                .iter()
                .map(|(k, n)| format!("{n} {:#010x}", p.query_status(a.luid, *k, 1) as u32))
                .collect();
                println!("  {name}: {}", statuses.join(", "));
            }
            println!("PID {pid}:");
            let p = match d3dkmt::Process::open(pid) {
                Ok(p) => p,
                Err(d3dkmt::OpenError::Gone) => {
                    println!("  no such process");
                    continue;
                }
                Err(d3dkmt::OpenError::Denied(code)) => {
                    println!("  OpenProcess(PROCESS_QUERY_INFORMATION): Win32 error {code}");
                    continue;
                }
            };
            for a in adapters {
                println!("  adapter {:#018x}:", a.luid);
                for s in &a.segments {
                    let kind = if s.aperture { "aperture" } else { "local" };
                    match p.segment_committed(a.luid, s.id) {
                        Ok(b) => println!("    segment {} {kind}: committed {}", s.id, mib(b)),
                        Err(e) => println!("    segment {} {kind}: {e}", s.id),
                    }
                }
                match p.local_group(a.luid) {
                    Ok((budget, usage)) => println!(
                        "    local segment group: usage {}, budget {}",
                        mib(usage),
                        mib(budget)
                    ),
                    Err(e) => println!("    local segment group: {e}"),
                }
            }
        }

        if args.pids.is_empty() {
            return Ok(());
        }
        let rounds = if args.every_ms.is_some() {
            args.times
        } else {
            1
        };
        for round in 0..rounds {
            if round > 0 {
                std::thread::sleep(std::time::Duration::from_millis(args.every_ms.unwrap_or(0)));
            }
            let used = nvml
                .devices()
                .map(|d| d.iter().map(|d| d.used_bytes).sum::<u64>())
                .map_or_else(|e| e, mib);
            let listed = wddm::processes(&d3dkmt::Real, &pcis, &args.pids, &[]);
            let listed = match listed {
                Ok(l) => l
                    .iter()
                    .map(|p| format!("{}: {}", p.pid, p.bytes.map_or("?".into(), mib)))
                    .collect::<Vec<_>>()
                    .join("; "),
                Err(e) => e,
            };
            println!("round {round}: NVML used {used}; vram::wddm::processes {listed}");
        }
        Ok(())
    }
}
