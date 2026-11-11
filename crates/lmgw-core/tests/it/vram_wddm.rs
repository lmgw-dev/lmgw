//! Per-process GPU memory on Windows (Windows design §4.3): the decisions of
//! `vram::wddm` against a stated kernel, on every platform, and, on Windows,
//! the real D3DKMT and Toolhelp readers against this test's own process.

use std::collections::HashMap;

use lmgw_core::vram::wddm::tree::{self, ProcRecord};
use lmgw_core::vram::wddm::{
    self, Adapter, Adapters, FailedAdapter, Kernel, Opened, PciAddress, Read, Segment,
};
use lmgw_core::vram::ProcessMemory;

fn pci(bus: u32) -> PciAddress {
    PciAddress {
        bus,
        device: 0,
        function: 0,
    }
}

fn adapter(luid: u64, at: Option<PciAddress>, local: bool) -> Adapter {
    Adapter {
        luid,
        pci: at,
        segments: vec![
            Segment {
                id: 0,
                aperture: !local,
                commit_limit: 4 << 30,
                committed: 0,
            },
            Segment {
                id: 1,
                aperture: true,
                commit_limit: 8 << 30,
                committed: 0,
            },
        ],
    }
}

/// A kernel whose answers the test states: per PID, per adapter LUID, the
/// local bytes.
struct Stated {
    adapters: Vec<Adapter>,
    reads: HashMap<u32, Result<HashMap<u64, u64>, String>>,
}

impl Kernel for Stated {
    fn adapters(&self) -> Result<Adapters, String> {
        Ok(listed(self.adapters.clone()))
    }

    fn local_committed(&self, pid: u32, adapters: &[&Adapter]) -> Read {
        match self.reads.get(&pid) {
            None => Read::Gone,
            Some(Err(e)) => Read::Failed(e.clone()),
            Some(Ok(per)) => Read::Bytes(
                adapters
                    .iter()
                    .map(|a| per.get(&a.luid).copied().unwrap_or(0))
                    .sum(),
            ),
        }
    }
}

fn listed(listed: Vec<Adapter>) -> Adapters {
    Adapters {
        listed,
        failed: Vec::new(),
    }
}

fn pm(pid: u32, bytes: u64) -> ProcessMemory {
    ProcessMemory {
        pid,
        bytes: Some(bytes),
    }
}

#[test]
fn nvml_bus_ids_parse_to_pci_addresses() {
    assert_eq!(
        PciAddress::from_nvml_bus_id("00000000:01:00.0"),
        Some(PciAddress {
            bus: 1,
            device: 0,
            function: 0
        })
    );
    assert_eq!(
        PciAddress::from_nvml_bus_id("00000001:a3:1f.7\0"),
        Some(PciAddress {
            bus: 0xa3,
            device: 0x1f,
            function: 7
        })
    );
    assert_eq!(PciAddress::from_nvml_bus_id("01:00"), None);
    assert_eq!(PciAddress::from_nvml_bus_id("garbage"), None);
}

/// The software adapter and a second card NVML does not have are not
/// counted; the card at NVML's address is.
#[test]
fn only_the_adapters_at_nvmls_addresses_count() {
    let adapters = vec![
        adapter(1, None, true),
        adapter(2, Some(pci(1)), true),
        adapter(3, Some(pci(5)), true),
    ];
    let adapters = listed(adapters);
    let got = wddm::counted(&adapters, &[pci(1)]).unwrap();
    assert_eq!(got.iter().map(|a| a.luid).collect::<Vec<_>>(), vec![2]);
}

/// An NVML device with no adapter (or one with no local segment) there is
/// an error naming the address, never a smaller share.
#[test]
fn an_nvml_device_without_its_adapter_is_an_error() {
    let adapters = listed(vec![
        adapter(2, Some(pci(1)), true),
        adapter(4, Some(pci(2)), false),
    ]);
    let err = wddm::counted(&adapters, &[pci(1), pci(2)]).unwrap_err();
    assert!(err.contains("PCI 02:00.0"), "{err}");
    let err = wddm::counted(&adapters, &[pci(9)]).unwrap_err();
    assert!(err.contains("PCI 09:00.0"), "{err}");
}

fn failed(luid: u64, at: Option<PciAddress>, why: &str) -> FailedAdapter {
    FailedAdapter {
        luid,
        pci: at,
        why: why.into(),
    }
}

/// An adapter that could not be described is kept with its NTSTATUS: when
/// it is the one at NVML's address, the error is that failure; when its
/// address is unknown, the "no adapter there" error names it as a possible
/// cause. Elsewhere it changes nothing.
#[test]
fn an_unreadable_adapter_is_reported_as_the_cause() {
    let segment_failed = "D3DKMTQueryStatistics(SEGMENT) failed with NTSTATUS 0xc0000001";
    let adapters = Adapters {
        listed: vec![adapter(2, Some(pci(1)), true)],
        failed: vec![failed(5, Some(pci(3)), segment_failed)],
    };
    let err = wddm::counted(&adapters, &[pci(3)]).unwrap_err();
    assert!(
        err.contains("at PCI 03:00.0")
            && err.contains("could not be read")
            && err.contains("0xc0000001"),
        "{err}"
    );
    // Not the card NVML has: counted as before.
    let got = wddm::counted(&adapters, &[pci(1)]).unwrap();
    assert_eq!(got.iter().map(|a| a.luid).collect::<Vec<_>>(), vec![2]);

    let pci_failed =
        "D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERADDRESS) failed with NTSTATUS 0xc00000bb";
    let adapters = Adapters {
        listed: vec![adapter(2, Some(pci(1)), true)],
        failed: vec![failed(6, None, pci_failed)],
    };
    let err = wddm::counted(&adapters, &[pci(4)]).unwrap_err();
    assert!(
        err.contains("PCI 04:00.0")
            && err.contains("0xc00000bb")
            && err.contains("LUID 0x0000000000000006"),
        "{err}"
    );
}

/// The same adapter listed twice counts once; two adapters at one NVML
/// address are an error, as is a readable and an unreadable one there.
#[test]
fn one_address_is_one_adapter_or_an_error() {
    let twice = listed(vec![
        adapter(2, Some(pci(1)), true),
        adapter(2, Some(pci(1)), true),
    ]);
    let got = wddm::counted(&twice, &[pci(1)]).unwrap();
    assert_eq!(got.iter().map(|a| a.luid).collect::<Vec<_>>(), vec![2]);

    let two = listed(vec![
        adapter(2, Some(pci(1)), true),
        adapter(7, Some(pci(1)), true),
    ]);
    let err = wddm::counted(&two, &[pci(1)]).unwrap_err();
    assert!(
        err.contains("2 WDDM adapters are at PCI 01:00.0") && err.contains("ambiguous"),
        "{err}"
    );

    let mixed = Adapters {
        listed: vec![adapter(2, Some(pci(1)), true)],
        failed: vec![failed(8, Some(pci(1)), "NTSTATUS 0xc0000001")],
    };
    let err = wddm::counted(&mixed, &[pci(1)]).unwrap_err();
    assert!(
        err.contains("2 WDDM adapters") && err.contains("not readable"),
        "{err}"
    );
}

fn rec(pid: u32, ppid: u32, created: Option<u64>) -> ProcRecord {
    ProcRecord { pid, ppid, created }
}

/// The Toolhelp walk's rule (`vram::wddm::tree`): a child counts only when
/// it was created no earlier than the parent it names. The engine's
/// `conhost.exe` is its child; an older process naming the engine's PID (its
/// real parent died and the PID went to the engine) is not, nor anything
/// under it; neither is one whose creation time could not be read.
#[test]
fn descendants_follow_creation_times() {
    let records = [
        // The engine, started at 1000.
        rec(100, 4, Some(1000)),
        // Its conhost.exe, started with it.
        rec(101, 100, Some(1000)),
        // A worker the engine started later, and that one's child.
        rec(102, 100, Some(1200)),
        rec(103, 102, Some(1300)),
        // Started at 500 by an earlier process 100, which is gone: not the
        // engine's, and neither is its child of 1500.
        rec(50, 100, Some(500)),
        rec(51, 50, Some(1500)),
        // Another user's process naming 100: no creation time.
        rec(60, 100, None),
        // Unrelated.
        rec(200, 4, Some(900)),
    ];
    let mut d = tree::descendants(&records, 100);
    d.sort();
    assert_eq!(d, vec![101, 102, 103]);
    assert!(
        tree::descendants(&records, 999).is_empty(),
        "not in the snapshot"
    );
    let unknown = [rec(100, 4, None), rec(101, 100, Some(1000))];
    assert!(
        tree::descendants(&unknown, 100).is_empty(),
        "the root's time is unknown"
    );
    // A parent-PID cycle (two reused PIDs naming each other) ends.
    let cycle = [rec(1, 2, Some(10)), rec(2, 1, Some(10))];
    assert_eq!(tree::descendants(&cycle, 1), vec![2]);

    let mut c = tree::candidates(&records.map(|r| (r.pid, r.ppid)), 100);
    c.sort();
    assert_eq!(
        c,
        vec![50, 51, 60, 100, 101, 102, 103],
        "what the reader opens"
    );
}

#[test]
fn processes_follow_the_probe_contract() {
    let kernel = Stated {
        adapters: vec![
            adapter(2, Some(pci(1)), true),
            adapter(3, Some(pci(5)), true),
        ],
        reads: HashMap::from([
            // An engine on the counted card, and memory on the other card
            // that is not counted.
            (100, Ok(HashMap::from([(2, 700 << 20), (3, 50 << 20)]))),
            // A CPU-only process: nothing committed, so not listed.
            (200, Ok(HashMap::new())),
            // A tombstone's PID now someone else's process.
            (300, Err("OpenProcess failed with Win32 error 5".into())),
            (400, Ok(HashMap::from([(2, 1 << 20)]))),
        ]),
    };
    // 500 is gone: not listed.
    let got = wddm::processes(&kernel, &[pci(1)], &[100, 200, 500], &[300, 400]).unwrap();
    assert_eq!(got, vec![pm(100, 700 << 20), pm(400, 1 << 20)]);

    // One of lmgw's own that cannot be read is an error.
    let err = wddm::processes(&kernel, &[pci(1)], &[100, 300], &[]).unwrap_err();
    assert!(
        err.contains("process 300") && err.contains("error 5"),
        "{err}"
    );

    // Asked as own and retired at once: read once, listed once.
    let got = wddm::processes(&kernel, &[pci(1)], &[100], &[100]).unwrap();
    assert_eq!(got, vec![pm(100, 700 << 20)]);
}

/// An opened process the test states: what its statistics queries answer,
/// and whether it has exited.
struct StatedProcess {
    committed: Result<u64, String>,
    exited: bool,
}

impl Opened for StatedProcess {
    fn committed(&self, _: &[&Adapter]) -> Result<u64, String> {
        self.committed.clone()
    }

    fn exited(&self) -> bool {
        self.exited
    }
}

/// A kernel that opens every PID it states and reads it the way the real
/// one does, through `read_opened`.
struct Opening(HashMap<u32, StatedProcess>);

impl Kernel for Opening {
    fn adapters(&self) -> Result<Adapters, String> {
        Ok(listed(vec![adapter(2, Some(pci(1)), true)]))
    }

    fn local_committed(&self, pid: u32, adapters: &[&Adapter]) -> Read {
        match self.0.get(&pid) {
            None => Read::Gone,
            Some(p) => wddm::read_opened(p, adapters),
        }
    }
}

/// A process that opens and whose query fails is gone when it has exited
/// (an engine its supervisor still holds a handle to) and a failure when it
/// runs; one that answers is its bytes whether it has exited or not.
#[test]
fn a_failing_query_is_gone_only_for_an_exited_process() {
    let failing = || Err("D3DKMTQueryStatistics(PROCESS) failed with NTSTATUS 0xc000000d".into());
    let kernel = Opening(HashMap::from([
        (
            100,
            StatedProcess {
                committed: failing(),
                exited: true,
            },
        ),
        (
            200,
            StatedProcess {
                committed: failing(),
                exited: false,
            },
        ),
        (
            300,
            StatedProcess {
                committed: Ok(5 << 20),
                exited: true,
            },
        ),
    ]));
    let all = kernel.adapters().unwrap();
    let all: Vec<&Adapter> = all.listed.iter().collect();
    assert_eq!(kernel.local_committed(100, &all), Read::Gone);
    let read = kernel.local_committed(200, &all);
    assert!(
        matches!(&read, Read::Failed(why) if why.contains("0xc000000d")),
        "{read:?}"
    );
    assert_eq!(kernel.local_committed(300, &all), Read::Bytes(5 << 20));

    // Through the probe contract: the exited one of lmgw's own is not
    // listed, the running one fails the answer.
    assert_eq!(
        wddm::processes(&kernel, &[pci(1)], &[100, 300], &[]),
        Ok(vec![pm(300, 5 << 20)])
    );
    let err = wddm::processes(&kernel, &[pci(1)], &[100, 200], &[]).unwrap_err();
    assert!(err.contains("process 200"), "{err}");
}

/// The real readers on this machine: D3DKMT lists adapters (a CI runner
/// without a card still has the Basic Render Driver), this process (which
/// opened no GPU device) reads as bytes, not an error, a PID nobody has is
/// gone, and the Toolhelp tree finds a child this test started.
#[cfg(windows)]
#[test]
fn the_real_readers_answer_for_this_process() {
    use lmgw_core::vram::ProcTree;

    let adapters = wddm::d3dkmt::adapters().expect("D3DKMTEnumAdapters2");
    assert!(
        !adapters.listed.is_empty(),
        "no WDDM adapter read: {adapters:?}"
    );
    wddm::d3dkmt::available().expect("the D3DKMT availability check");
    let me = std::process::id();
    let all: Vec<&Adapter> = adapters.listed.iter().collect();
    let read = wddm::d3dkmt::Real.local_committed(me, &all);
    assert!(matches!(read, Read::Bytes(_)), "{read:?}");
    // No process has this PID: gone, not an error.
    assert_eq!(
        wddm::d3dkmt::Real.local_committed(u32::MAX - 3, &all),
        Read::Gone
    );

    let mut child = std::process::Command::new("cmd")
        .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
        .spawn()
        .expect("spawn cmd");
    let found = wddm::toolhelp::Toolhelp
        .descendants(me)
        .expect("Toolhelp snapshot");
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        found.contains(&child.id()),
        "the child {} is not among {found:?}",
        child.id()
    );
    assert!(
        wddm::toolhelp::Toolhelp.started(me).is_some(),
        "this process's creation time"
    );
}

/// A process that has exited while a handle to it is still open (`Child`
/// holds one until it is dropped, as the native supervisor does while it
/// drains the log readers) still opens by PID; it reads as gone, never as a
/// failure that would fail the whole pass.
#[cfg(windows)]
#[test]
fn an_exited_process_with_an_open_handle_reads_as_gone() {
    let adapters = wddm::d3dkmt::adapters().expect("D3DKMTEnumAdapters2");
    let all: Vec<&Adapter> = adapters.listed.iter().collect();
    let mut child = std::process::Command::new("cmd")
        .args(["/c", "exit 3"])
        .spawn()
        .expect("spawn cmd");
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(3));
    let pid = child.id();
    let p = wddm::d3dkmt::Process::open(pid)
        .ok()
        .expect("the exited process still opens");
    assert!(p.exited());
    // The statistics queries fail for it (NTSTATUS 0xc000000d on
    // 2026-11-10, Windows 11 build 26100, driver 560.81), so this is the
    // exited branch of `read_opened` on a real handle, not a zero read.
    let query = p.committed(&all);
    let read = wddm::d3dkmt::Real.local_committed(pid, &all);
    eprintln!("exited pid {pid}: the query {query:?}, read {read:?}");
    assert!(query.is_err(), "the query answered: {query:?}");
    assert_eq!(read, Read::Gone);
    drop(child);
}

/// A real llama-server GPU build through the registry and the native
/// launcher, measured the way the outside-VRAM pass measures it: the PID the
/// launcher answers, D3DKMT's bytes for it through `WddmProbe`, against
/// NVML's used-memory delta. Skips unless both are set:
///
/// - `LMGW_LIVE_WDDM_ENGINES`: an engines dir holding exactly one llama GPU
///   build, `<dir>\llama\<build>\llama-server.exe` (the CUDA 12.4 or the
///   Vulkan zip of llama.cpp b12236, which `scripts\windows-engine-probe.ps1`
///   pins, unpacked there);
/// - `LMGW_LIVE_WDDM_GGUF`: a small chat GGUF (Qwen3.5-0.8B UD-Q4_K_XL for
///   the record in the Windows design §4.3).
///
/// The card must hold nothing else that changes while the test runs. Run
/// (through `scripts/windows-remote-check.sh`): `cargo test --workspace
/// --features lmgw-core/opus-bundled --test it -- vram_wddm --nocapture`.
#[cfg(windows)]
#[tokio::test]
async fn wddm_live_engine_bytes_match_nvmls_used_delta() {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use lmgw_core::config::LlamaParams;
    use lmgw_core::erased::Erased;
    use lmgw_core::runtime::argv::LlamaArgs;
    use lmgw_core::runtime::descriptor::ModelRuntime;
    use lmgw_core::runtime::engine_assets::Engines;
    use lmgw_core::runtime::launcher::Native;
    use lmgw_core::runtime::registry::{
        ephemeral_port, AcquireSpec, CmdOutput, CommandRunner, Registry,
    };
    use lmgw_core::runtime::{container_name, Class};
    use lmgw_core::vram::wddm::WddmProbe;
    use lmgw_core::vram::{GpuProbe, NvmlProbe, ProcTree};

    struct NoPodman;
    impl CommandRunner for NoPodman {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
        ) -> Erased<'a, std::io::Result<CmdOutput>> {
            panic!("the registry shelled `{program} {}`", args.join(" "))
        }
    }

    let (Some(engines), Some(gguf)) = (
        std::env::var_os("LMGW_LIVE_WDDM_ENGINES"),
        std::env::var_os("LMGW_LIVE_WDDM_GGUF"),
    ) else {
        eprintln!("skipped: LMGW_LIVE_WDDM_ENGINES and LMGW_LIVE_WDDM_GGUF are not set");
        return;
    };
    let probe = WddmProbe::new(NvmlProbe::detect());
    probe.process_support().expect("per-process support");
    let used = || -> u64 { probe.devices().unwrap().iter().map(|d| d.used_bytes).sum() };
    let before = used();

    let gguf = PathBuf::from(gguf);
    let models = gguf.parent().unwrap().display().to_string();
    let file = gguf.file_name().unwrap().to_string_lossy().into_owned();
    let native = Native::with_engines(Engines::hand_installed(PathBuf::from(engines))).unwrap();
    let reg = Arc::new(Registry::with_launcher(
        Arc::new(native),
        Arc::new(NoPodman),
        reqwest::Client::new(),
        Arc::new(ephemeral_port),
    ));
    let data = tempfile::tempdir().unwrap();
    let rt = ModelRuntime {
        class: Class::Chat,
        model_id: "live-wddm".into(),
        image: "unused-by-the-native-launcher".into(),
        // The card, as a container row passes it (§4.1): without it the
        // native launcher hides every GPU.
        extra_run_args: vec!["--device".into(), "nvidia.com/gpu=all".into()],
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: Some(LlamaArgs::Chat {
            gguf_path: file,
            params: Box::new(LlamaParams {
                n_gpu_layers: Some(99),
                ctx_size: Some(4096),
                jinja: true,
                ..LlamaParams::default()
            }),
            args: vec![],
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    };
    let spec = AcquireSpec {
        runtime: &rt,
        container_prefix: "lmgw",
        models_dir: &models,
        data_dir: data.path(),
        may_write_models_dir: false,
        load_timeout: Duration::from_secs(600),
        stop_timeout: Duration::from_secs(30),
    };
    let name = container_name("lmgw", Class::Chat, "live-wddm");
    let guard = match reg.acquire(&spec).await {
        Ok(g) => g,
        Err(e) => panic!(
            "{e}\n--- log ---\n{}",
            reg.logs_tail(&name, 80).await.unwrap_or_default()
        ),
    };
    // The allocations settle right after /health answers.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pid = *reg
        .host_pids(std::slice::from_ref(&name))
        .await
        .unwrap()
        .get(&name)
        .expect("the engine's PID");
    // As the ledger reads a member: the engine and everything under it
    // (Windows starts a console host for a console program, even one
    // without a window).
    let tree = lmgw_core::vram::wddm::toolhelp::Toolhelp
        .descendants(pid)
        .expect("Toolhelp snapshot");
    let names = descendant_names(&tree);
    let pids: Vec<u32> = std::iter::once(pid).chain(tree.iter().copied()).collect();
    let delta = used().saturating_sub(before);
    let listed = probe.processes(&pids, &[]).unwrap();
    // A handle of this test's own keeps the engine's process object after
    // it exits, as the native supervisor's does until the log readers are
    // drained: the engine reads as gone then, as one of lmgw's own too.
    let held = wddm::d3dkmt::Process::open(pid)
        .ok()
        .expect("open the engine");
    drop(guard);
    reg.stop(Class::Chat, "live-wddm", false).await.unwrap();
    let after_stop = probe.processes(&[], &pids).unwrap();
    assert!(held.exited(), "the engine has not exited after its stop");
    let exited_read = {
        let adapters = wddm::d3dkmt::adapters().unwrap();
        let all: Vec<&Adapter> = adapters.listed.iter().collect();
        wddm::d3dkmt::Real.local_committed(pid, &all)
    };
    let exited_own = probe.processes(&[pid], &[]);
    drop(held);
    eprintln!(
        "wddm live: the exited engine with a handle held: {exited_read:?}, as own {exited_own:?}"
    );
    assert_eq!(
        exited_own,
        Ok(vec![]),
        "the exited engine as one of lmgw's own"
    );

    assert!(
        listed.iter().any(|p| p.pid == pid),
        "the engine is not listed: {listed:?}"
    );
    let bytes: u64 = listed.iter().map(|p| p.bytes.unwrap()).sum();
    let mib = |b: u64| b as f64 / 1048576.0;
    eprintln!(
        "wddm live: pid {pid}, descendants {names:?}, listed {listed:?}; D3DKMT {bytes} B \
         ({:.1} MiB), NVML used delta {delta} B ({:.1} MiB), difference {} B",
        mib(bytes),
        mib(delta),
        bytes as i64 - delta as i64
    );
    assert!(
        delta > 64 << 20,
        "the engine took {delta} B: not on the card"
    );
    // The tolerance of this check, not of lmgw: 2026-11-10 measured the
    // probe 0.95 MiB (CUDA) and 1.45 MiB (Vulkan) above the delta.
    assert!(
        bytes.abs_diff(delta) <= delta / 20,
        "D3DKMT {bytes} B against NVML's delta {delta} B"
    );
    assert!(
        after_stop.is_empty(),
        "the stopped engine is still listed: {after_stop:?}"
    );
}

/// `pid (exe)` for each of `pids`.
#[cfg(windows)]
fn descendant_names(pids: &[u32]) -> Vec<String> {
    let names = wddm::toolhelp::exe_names(pids).expect("Toolhelp snapshot");
    pids.iter()
        .map(|p| format!("{p} ({})", names.get(p).map_or("gone", String::as_str)))
        .collect()
}
