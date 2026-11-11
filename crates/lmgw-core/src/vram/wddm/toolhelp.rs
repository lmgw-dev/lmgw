//! The process tree on Windows, for attribution's descendant walk
//! ([`crate::vram::ProcTree`]): one Toolhelp snapshot of every process and
//! its parent PID, walked by [`super::tree`]'s rule (a child counts only
//! when it was created no earlier than the parent it names). Creation times
//! are read only for the processes the parent PIDs reach from the root.
//!
//! A snapshot that cannot be taken is an error naming the Win32 error, never
//! an empty tree: an engine's children would then read as nobody's.

use std::collections::HashMap;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FILETIME, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::tree::{self, ProcRecord};
use crate::vram::ProcTree;

/// The real process tree on Windows.
#[derive(Default)]
pub struct Toolhelp;

/// The executable name of each of `pids` that runs, from one snapshot: for a
/// sentence that names a process (and the live test).
pub fn exe_names(pids: &[u32]) -> Result<HashMap<u32, String>, String> {
    let mut out = HashMap::new();
    each_process(|e| {
        if pids.contains(&e.th32ProcessID) {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            out.insert(
                e.th32ProcessID,
                String::from_utf16_lossy(&e.szExeFile[..len]),
            );
        }
    })?;
    Ok(out)
}

/// Every process in one snapshot but the idle process (PID 0), or the Win32
/// error the snapshot failed with.
fn each_process(mut f: impl FnMut(&PROCESSENTRY32W)) -> Result<(), String> {
    // SAFETY: plain call; the handle is checked and closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        // SAFETY: plain call.
        let e = unsafe { GetLastError() };
        return Err(format!(
            "CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS) failed with Win32 error {e}"
        ));
    }
    let mut e = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `e` is a PROCESSENTRY32W with its size set.
    let mut more = unsafe { Process32FirstW(snap, &mut e) } != 0;
    if !more {
        // A snapshot always holds the System process, so no first entry is
        // a failure.
        // SAFETY: plain call.
        let err = unsafe { GetLastError() };
        // SAFETY: the snapshot handle is closed once.
        unsafe { CloseHandle(snap) };
        return Err(format!("Process32FirstW failed with Win32 error {err}"));
    }
    while more {
        if e.th32ProcessID != 0 {
            f(&e);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &mut e) } != 0;
    }
    // SAFETY: the snapshot handle is closed once.
    unsafe { CloseHandle(snap) };
    Ok(())
}

/// A process's creation time in 100 ns ticks, or `None` when it cannot be
/// opened.
fn created(pid: u32) -> Option<u64> {
    // SAFETY: plain call; a null handle is checked.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut times = [FILETIME::default(); 4];
    let [c, e, k, u] = &mut times;
    // SAFETY: four valid FILETIME out-pointers.
    let ok = unsafe { GetProcessTimes(h, c, e, k, u) } != 0;
    // SAFETY: closed once.
    unsafe { CloseHandle(h) };
    ok.then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
}

impl ProcTree for Toolhelp {
    fn descendants(&self, pid: u32) -> Result<Vec<u32>, String> {
        let mut parents: Vec<(u32, u32)> = Vec::new();
        each_process(|e| parents.push((e.th32ProcessID, e.th32ParentProcessID)))?;
        let ppid: HashMap<u32, u32> = parents.iter().copied().collect();
        let records: Vec<ProcRecord> = tree::candidates(&parents, pid)
            .into_iter()
            .map(|p| ProcRecord {
                pid: p,
                ppid: ppid.get(&p).copied().unwrap_or(0),
                created: created(p),
            })
            .collect();
        Ok(tree::descendants(&records, pid))
    }

    /// The creation time: what tells a tombstone's process from a later one
    /// that was handed the same PID.
    fn started(&self, pid: u32) -> Option<u64> {
        created(pid)
    }
}
