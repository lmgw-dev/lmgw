//! The D3DKMT calls behind [`super`]: the WDDM kernel thunks in `gdi32.dll`
//! that Task Manager's per-process "Dedicated GPU memory" reads (Windows
//! design §4.3).
//!
//! Every function here is one or a few system calls with no state kept
//! between them: an adapter handle is opened and closed within
//! [`adapters`], and a process handle lives as long as its [`Process`].

use std::mem::{size_of, zeroed};
use std::ptr::{addr_of_mut, null_mut};

use windows_sys::Wdk::Graphics::Direct3D as kmt;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, HANDLE, LUID, NTSTATUS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_INFORMATION,
};

use super::{Adapter, Adapters, FailedAdapter, PciAddress, Segment};

/// `STATUS_BUFFER_TOO_SMALL`: an adapter appeared between the count and the
/// list.
const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023_u32 as NTSTATUS;

/// `GetExitCodeProcess`'s code for a process that runs (`STILL_ACTIVE`).
const STILL_ACTIVE: u32 = 259;

/// `STATUS_INVALID_PARAMETER`: what a per-adapter process query answers for
/// an adapter the process has no device on (and every process query for a
/// handle without `PROCESS_QUERY_INFORMATION`).
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000D_u32 as NTSTATUS;

fn ok(what: &str, status: NTSTATUS) -> Result<(), String> {
    if status >= 0 {
        Ok(())
    } else {
        Err(format!(
            "{what} failed with NTSTATUS {:#010x}",
            status as u32
        ))
    }
}

fn luid_key(l: LUID) -> u64 {
    (u64::from(l.HighPart as u32) << 32) | u64::from(l.LowPart)
}

fn luid_of(key: u64) -> LUID {
    LUID {
        LowPart: key as u32,
        HighPart: (key >> 32) as u32 as i32,
    }
}

/// Every adapter the kernel enumerates, with its PCI address and its memory
/// segments; one that could not be described is kept with why.
pub fn adapters() -> Result<Adapters, String> {
    let infos = enum_adapters()?;
    let mut out = Adapters::default();
    for info in &infos {
        let read = describe(info);
        // Closed whatever the reads answered.
        let close = kmt::D3DKMT_CLOSEADAPTER {
            hAdapter: info.hAdapter,
        };
        // SAFETY: the handle came from D3DKMTEnumAdapters2 and is closed once.
        unsafe { kmt::D3DKMTCloseAdapter(&close) };
        match read {
            Ok(a) => out.listed.push(a),
            Err(f) => out.failed.push(f),
        }
    }
    Ok(out)
}

/// Whether the D3DKMT statistics can be asked at all: the adapter count,
/// one system call that opens nothing. For the probe's availability check;
/// which adapters count is read with each process answer.
pub fn available() -> Result<(), String> {
    let mut e = kmt::D3DKMT_ENUMADAPTERS2 {
        NumAdapters: 0,
        pAdapters: null_mut(),
    };
    // SAFETY: a null list asks for the count only.
    ok("D3DKMTEnumAdapters2 (count)", unsafe {
        kmt::D3DKMTEnumAdapters2(&mut e)
    })?;
    if e.NumAdapters == 0 {
        return Err("D3DKMTEnumAdapters2 lists no adapter".into());
    }
    Ok(())
}

fn enum_adapters() -> Result<Vec<kmt::D3DKMT_ADAPTERINFO>, String> {
    loop {
        let mut e = kmt::D3DKMT_ENUMADAPTERS2 {
            NumAdapters: 0,
            pAdapters: null_mut(),
        };
        // SAFETY: a null list asks for the count only.
        ok("D3DKMTEnumAdapters2 (count)", unsafe {
            kmt::D3DKMTEnumAdapters2(&mut e)
        })?;
        // SAFETY: D3DKMT_ADAPTERINFO is plain data; all zeroes is valid.
        let mut list: Vec<kmt::D3DKMT_ADAPTERINFO> =
            vec![unsafe { zeroed() }; e.NumAdapters as usize];
        e.pAdapters = list.as_mut_ptr();
        // SAFETY: the list holds NumAdapters entries.
        let status = unsafe { kmt::D3DKMTEnumAdapters2(&mut e) };
        if status == STATUS_BUFFER_TOO_SMALL {
            // An adapter arrived in between: count again.
            continue;
        }
        ok("D3DKMTEnumAdapters2", status)?;
        list.truncate(e.NumAdapters as usize);
        return Ok(list);
    }
}

fn describe(info: &kmt::D3DKMT_ADAPTERINFO) -> Result<Adapter, FailedAdapter> {
    let luid = luid_key(info.AdapterLuid);
    let failed = |pci, why| FailedAdapter { luid, pci, why };
    let pci = pci_address(info.hAdapter).map_err(|why| failed(None, why))?;
    let segments = segments(info.AdapterLuid).map_err(|why| failed(pci, why))?;
    Ok(Adapter {
        luid,
        pci,
        segments,
    })
}

/// An adapter's memory segments.
fn segments(adapter: LUID) -> Result<Vec<Segment>, String> {
    // SAFETY: plain data, zeroes valid.
    let mut q: kmt::D3DKMT_QUERYSTATISTICS = unsafe { zeroed() };
    q.Type = kmt::D3DKMT_QUERYSTATISTICS_ADAPTER;
    q.AdapterLuid = adapter;
    query(&mut q, "D3DKMTQueryStatistics(ADAPTER)")?;
    // SAFETY: the ADAPTER query fills AdapterInformation.
    let segments = unsafe { q.QueryResult.AdapterInformation.NbSegments };
    let mut out = Vec::with_capacity(segments as usize);
    for id in 0..segments {
        let mut q: kmt::D3DKMT_QUERYSTATISTICS = unsafe { zeroed() };
        q.Type = kmt::D3DKMT_QUERYSTATISTICS_SEGMENT;
        q.AdapterLuid = adapter;
        q.Anonymous.QuerySegment.SegmentId = id;
        query(&mut q, "D3DKMTQueryStatistics(SEGMENT)")?;
        // SAFETY: the SEGMENT query fills SegmentInformation (the Windows 8
        // and later layout; lmgw runs on Windows 10 and later).
        let s = unsafe { q.QueryResult.SegmentInformation };
        out.push(Segment {
            id,
            aperture: s.Aperture != 0,
            commit_limit: s.CommitLimit,
            committed: s.BytesCommitted,
        });
    }
    Ok(out)
}

/// The adapter's PCI bus, device and function, `None` for one that has none
/// (the Microsoft Basic Render Driver, an indirect display), or the NTSTATUS
/// the query failed with.
fn pci_address(adapter: u32) -> Result<Option<PciAddress>, String> {
    let mut addr = kmt::D3DKMT_ADAPTERADDRESS {
        BusNumber: 0,
        DeviceNumber: 0,
        FunctionNumber: 0,
    };
    let mut q = kmt::D3DKMT_QUERYADAPTERINFO {
        hAdapter: adapter,
        Type: kmt::KMTQAITYPE_ADAPTERADDRESS,
        pPrivateDriverData: addr_of_mut!(addr).cast(),
        PrivateDriverDataSize: size_of::<kmt::D3DKMT_ADAPTERADDRESS>() as u32,
    };
    // SAFETY: the buffer is a D3DKMT_ADAPTERADDRESS of the size given.
    let status = unsafe { kmt::D3DKMTQueryAdapterInfo(&mut q) };
    ok("D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERADDRESS)", status)?;
    Ok((addr.BusNumber != u32::MAX).then_some(PciAddress {
        bus: addr.BusNumber,
        device: addr.DeviceNumber,
        function: addr.FunctionNumber,
    }))
}

fn query(q: &mut kmt::D3DKMT_QUERYSTATISTICS, what: &str) -> Result<(), String> {
    // SAFETY: `q` is a valid, writable D3DKMT_QUERYSTATISTICS; the call
    // writes only its QueryResult.
    ok(what, unsafe {
        kmt::D3DKMTQueryStatistics(addr_of_mut!(*q).cast_const())
    })
}

/// A process opened for the statistics queries.
pub struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Why a process could not be opened.
pub enum OpenError {
    /// No process has this PID (any more).
    Gone,
    /// It exists and this process may not query it: the Win32 error code.
    Denied(u32),
}

impl Process {
    /// `PROCESS_QUERY_INFORMATION`: the statistics queries refuse a handle
    /// with only `PROCESS_QUERY_LIMITED_INFORMATION` (every process query
    /// answers `STATUS_INVALID_PARAMETER`; measured 2026-11-10, Windows 11
    /// build 26100, NVIDIA driver 560.81). A process of the same user grants
    /// it without elevation.
    pub fn open(pid: u32) -> Result<Self, OpenError> {
        Self::open_with(pid, PROCESS_QUERY_INFORMATION)
    }

    /// [`Self::open`] with another access mask, for the probe example.
    pub fn open_with(pid: u32, access: u32) -> Result<Self, OpenError> {
        // SAFETY: plain call; a null result is checked.
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() {
            // SAFETY: plain call.
            let e = unsafe { GetLastError() };
            return Err(if e == ERROR_INVALID_PARAMETER {
                OpenError::Gone
            } else {
                OpenError::Denied(e)
            });
        }
        Ok(Self(h))
    }

    /// The bytes this process has committed in one segment of one adapter
    /// (`D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT`'s `BytesCommitted`).
    pub fn segment_committed(&self, adapter: u64, segment: u32) -> Result<u64, String> {
        // SAFETY: plain data, zeroes valid.
        let mut q: kmt::D3DKMT_QUERYSTATISTICS = unsafe { zeroed() };
        q.Type = kmt::D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT;
        q.AdapterLuid = luid_of(adapter);
        q.hProcess = self.0;
        q.Anonymous.QueryProcessSegment.SegmentId = segment;
        query(&mut q, "D3DKMTQueryStatistics(PROCESS_SEGMENT)")?;
        // SAFETY: the PROCESS_SEGMENT query fills ProcessSegmentInformation.
        Ok(unsafe { q.QueryResult.ProcessSegmentInformation.BytesCommitted })
    }

    /// A process-level query of any `D3DKMT_QUERYSTATISTICS_TYPE`, with
    /// `segment` as its segment id where the type takes one: the NTSTATUS it
    /// answered, the result discarded.
    pub fn query_status(&self, adapter: u64, kind: i32, segment: u32) -> NTSTATUS {
        // SAFETY: plain data, zeroes valid.
        let mut q: kmt::D3DKMT_QUERYSTATISTICS = unsafe { zeroed() };
        q.Type = kind;
        q.AdapterLuid = luid_of(adapter);
        q.hProcess = self.0;
        q.Anonymous.QueryProcessSegment.SegmentId = segment;
        // SAFETY: as in `query`.
        unsafe { kmt::D3DKMTQueryStatistics(addr_of_mut!(q).cast_const()) }
    }

    /// The process's local segment group on one adapter: `(budget, usage)`,
    /// what DXGI's `QueryVideoMemoryInfo` reports for the calling process
    /// (`D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP`). Read by the probe
    /// example beside the segment figures.
    pub fn local_group(&self, adapter: u64) -> Result<(u64, u64), String> {
        // SAFETY: plain data, zeroes valid.
        let mut q: kmt::D3DKMT_QUERYSTATISTICS = unsafe { zeroed() };
        q.Type = kmt::D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP;
        q.AdapterLuid = luid_of(adapter);
        q.hProcess = self.0;
        q.Anonymous.QueryProcessSegmentGroup = kmt::D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL;
        query(&mut q, "D3DKMTQueryStatistics(PROCESS_SEGMENT_GROUP)")?;
        // SAFETY: the PROCESS_SEGMENT_GROUP query fills
        // ProcessSegmentGroupInformation.
        let g = unsafe { q.QueryResult.ProcessSegmentGroupInformation };
        Ok((g.Budget, g.Usage))
    }
}

/// The real [`super::Kernel`].
pub struct Real;

impl super::Kernel for Real {
    fn adapters(&self) -> Result<Adapters, String> {
        adapters()
    }

    /// Measured 2026-11-10 (Windows 11 build 26100, driver 560.81, see the
    /// Windows design §4.3): with a `PROCESS_QUERY_INFORMATION` handle the
    /// process-wide query (`D3DKMT_QUERYSTATISTICS_PROCESS`) answers for any
    /// process, and the per-adapter ones answer `STATUS_INVALID_PARAMETER` for
    /// an adapter the process has no device on (a PowerShell, or an engine on another adapter). So the process-wide query
    /// proves the handle is usable, after which `STATUS_INVALID_PARAMETER`
    /// from `D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER` means "nothing on this
    /// adapter", and any other failure is an error, unless the process has
    /// exited by then ([`super::Opened::exited`]), which makes it gone
    /// ([`super::read_opened`]).
    fn local_committed(&self, pid: u32, adapters: &[&Adapter]) -> super::Read {
        let p = match Process::open(pid) {
            Ok(p) => p,
            Err(OpenError::Gone) => return super::Read::Gone,
            Err(OpenError::Denied(code)) => {
                return super::Read::Failed(format!(
                    "OpenProcess(PROCESS_QUERY_INFORMATION) failed with Win32 error {code}"
                ))
            }
        };
        super::read_opened(&p, adapters)
    }
}

impl super::Opened for Process {
    /// By the reading in [`Real::local_committed`]'s docs.
    fn committed(&self, adapters: &[&Adapter]) -> Result<u64, String> {
        let Some(first) = adapters.first() else {
            return Ok(0);
        };
        let status = self.query_status(first.luid, kmt::D3DKMT_QUERYSTATISTICS_PROCESS, 0);
        ok("D3DKMTQueryStatistics(PROCESS)", status)?;
        let mut total: u64 = 0;
        for a in adapters {
            match self.query_status(a.luid, kmt::D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER, 0) {
                STATUS_INVALID_PARAMETER => continue,
                s => ok("D3DKMTQueryStatistics(PROCESS_ADAPTER)", s)?,
            }
            for s in a.local() {
                total = total.saturating_add(self.segment_committed(a.luid, s.id)?);
            }
        }
        Ok(total)
    }

    /// A handle keeps an exited process's object (and its PID) alive until
    /// the last one closes, so an engine its supervisor has not reaped yet
    /// still opens. `false` when the exit code cannot be read, and for a
    /// process that exited with code 259, which reads as `STILL_ACTIVE`.
    fn exited(&self) -> bool {
        let mut code = 0u32;
        // SAFETY: a valid handle and out-pointer.
        let read = unsafe { GetExitCodeProcess(self.0, &mut code) } != 0;
        read && code != STILL_ACTIVE
    }
}
