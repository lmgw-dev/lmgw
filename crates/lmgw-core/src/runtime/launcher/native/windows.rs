//! The native launcher's Windows side: the kill-on-close job every child is
//! in, the creation flags, the run args' limits as a job of the child's own,
//! and the stop.
//!
//! **No moment outside the job** (Windows design §4.4, WP6). Every child is
//! created suspended ([`configure`]: `CREATE_SUSPENDED`), put in the job,
//! and only then let run ([`Job::assign`] resumes its one thread), so not a
//! line of the engine runs, and nothing it could start exists, before it is
//! in the job. A child that cannot be put in the job is killed while still
//! suspended (the caller's `start_kill`). Not closed yet: lmgw dying in the
//! moment between the spawn and the assignment leaves the suspended child
//! outside the job, where nothing kills it; `PROC_THREAD_ATTRIBUTE_JOB_LIST`,
//! which creates a process in a job, closes that once std can pass it.
//!
//! **Limits** (`limits.rs`): a child whose run args set `--memory` or
//! `--cpus` gets a job of its own with those limits, nested in the
//! kill-on-close job (Windows 8 and later nest jobs), so the outer job still
//! kills it and the limits are its alone.
//!
//! `windows-sys` (already in the lock through tokio and Tauri) is the one
//! dependency this needs: std has no job objects.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

use super::limits::{cpu_rate, Limits};

/// A job object handle, closed on drop. The kill-on-close job is one
/// ([`Job`]); a child's own limits job is one for as long as its assignment
/// takes (the job lives on while its process does).
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A new, unnamed job object.
fn new_job() -> io::Result<Handle> {
    // SAFETY: a plain kernel call; a null name makes an unnamed job.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(job))
}

/// `info` set on `job` as class `class`.
fn set_info<T>(job: &Handle, class: i32, info: &T) -> io::Result<()> {
    // SAFETY: `info` is the struct `class` names, alive for the call.
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            class,
            (info as *const T).cast(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Its handle is
/// not inheritable, so only this launcher holds it: closing it — the drop,
/// or lmgw's exit or crash — kills every process in the job, the ones in
/// its nested limits jobs too.
pub(super) struct Job(Handle);

// SAFETY: a job handle is a kernel object reference, usable from any thread;
// the launcher only passes it to thread-safe kernel calls.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub(super) fn new() -> io::Result<Self> {
        let job = new_job()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        set_info(&job, JobObjectExtendedLimitInformation, &info)?;
        Ok(Self(job))
    }

    /// Put `child` (created suspended, [`configure`]) in the job, and in a
    /// job of its own with `limits` when there are any; then let it run.
    pub(super) fn assign(&self, child: &tokio::process::Child, limits: &Limits) -> io::Result<()> {
        let Some(process) = child.raw_handle() else {
            return Err(io::Error::other(
                "the engine process has already been reaped",
            ));
        };
        let process = process as HANDLE;
        // SAFETY: both handles are live: the job is ours, the process
        // handle the child's own until it is dropped.
        if unsafe { AssignProcessToJobObject(self.0 .0, process) } == 0 {
            return Err(io::Error::other(format!(
                "it could not be put in lmgw's kill-on-close job, so it would outlive lmgw: {}",
                io::Error::last_os_error()
            )));
        }
        if !limits.is_empty() {
            let own = limits_job(limits).map_err(|e| {
                io::Error::other(format!(
                    "its limits ({}) could not be set: {e}",
                    limits.describe()
                ))
            })?;
            // SAFETY: as above; the process is in the outer job, so the
            // empty new one nests inside it.
            if unsafe { AssignProcessToJobObject(own.0, process) } == 0 {
                return Err(io::Error::other(format!(
                    "it could not be put in the job of its limits ({}): {}",
                    limits.describe(),
                    io::Error::last_os_error()
                )));
            }
        }
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("the engine process has already been reaped"))?;
        resume(pid)
    }
}

/// A job with `limits` (module doc).
fn limits_job(limits: &Limits) -> io::Result<Handle> {
    let job = new_job()?;
    if let Some(bytes) = limits.memory_bytes {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
        info.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
        set_info(&job, JobObjectExtendedLimitInformation, &info)?;
    }
    if let Some(cpus) = limits.cpus {
        // Every processor group's: the rate is a share of all of them.
        let logical = lmgw_machine::cpu::logical();
        match cpu_rate(cpus, logical) {
            Some(rate) => {
                let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                    ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
                        | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                    ..Default::default()
                };
                info.Anonymous.CpuRate = rate;
                set_info(&job, JobObjectCpuRateControlInformation, &info)?;
                tracing::info!(
                    "--cpus {cpus} of this machine's {logical} logical processors (every \
                     processor group's): the engine's job has a CPU rate hard cap of {rate} in \
                     10000 ({:.2} % of all processor time)",
                    f64::from(rate) / 100.0
                );
            }
            None => tracing::info!(
                "--cpus {cpus} is at least this machine's {logical} logical processors (every \
                 processor group's): the engine's CPU is not capped"
            ),
        }
    }
    Ok(job)
}

/// Let the suspended process `pid` run: resume its threads (a process made
/// with `CREATE_SUSPENDED` has its one first thread, suspended once).
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: plain kernel calls; the snapshot and each thread handle are
    // closed once; `entry` is the struct the walk names, its size set.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snap = Handle(snap);
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut resumed = 0;
        let mut more = Thread32First(snap.0, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = Handle(thread);
                if ResumeThread(thread.0) == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed += 1;
            }
            more = Thread32Next(snap.0, &mut entry) != 0;
        }
        if resumed == 0 {
            return Err(io::Error::other(format!(
                "no thread of the suspended engine process {pid} was found to resume"
            )));
        }
    }
    Ok(())
}

/// No console window per engine (lmgw is a GUI app, and a console child of
/// it would otherwise get a window of its own), and created suspended, so
/// it runs only once it is in the job ([`Job::assign`]).
pub(super) fn configure(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}
