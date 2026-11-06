//! Judging lmgw's spec and writing it again (machine-check design §3.6.1,
//! §3.6.2). Synchronous: [`super::OwnSpec::ensure_fresh`] runs it on a
//! blocking thread, one at a time, within the start's load timeout.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Where the driver lists the cards it drives, one dir per PCI address.
const NVIDIA_GPUS_DIR: &str = "/proc/driver/nvidia/gpus";

use lmgw_machine::cdi::own::{
    judge_against, paths_gone, Current, FileFingerprint, Judgement, OnDisk, Stale, DEV_DIR,
    NVIDIA_CDI_HOOK, NVIDIA_CTK, NVIDIA_MODPROBE, NVIDIA_UVM, OWN_SPEC_FILE,
};
use lmgw_machine::cdi::spec::{Format, Spec};
use lmgw_machine::facts::{exists, read_text};
use lmgw_machine::{CmdOut, Facts};

use super::{reasons, CdiError, Ensured, SpecPaths};

/// How often a program that is still running says so in the log: a cadence,
/// not a bound.
const STILL_RUNNING_EVERY: Duration = Duration::from_secs(10);

/// The staging dirs' prefix, `<root>/.generating-<pid>`.
const STAGING_PREFIX: &str = ".generating-";

/// The time one write may take: the start's load timeout, from when the
/// start asked for the spec (the wait for another write included).
#[derive(Debug, Clone, Copy)]
pub(super) struct Limit {
    deadline: Instant,
    budget: Duration,
}

impl Limit {
    pub(super) fn from_now(budget: Duration) -> Self {
        Self {
            deadline: Instant::now() + budget,
            budget,
        }
    }

    /// The whole time this limit allows.
    pub(super) fn budget(&self) -> Duration {
        self.budget
    }

    /// What is left of it now.
    pub(super) fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Judge the spec at `p` and write it again when it is stale.
pub(super) fn ensure(
    f: &dyn Facts,
    p: &SpecPaths,
    limit: Limit,
    hook_flag: &HookFlagCache,
) -> Result<Ensured, CdiError> {
    for dir in p.private_dirs() {
        ensure_private(&dir)?;
    }
    let current = Current::read(f);
    match judge_against(f, &current, on_disk(p)) {
        Judgement::Fresh => Ok(Ensured::Fresh),
        Judgement::Blocked(b) => Err(CdiError::Blocked(b)),
        Judgement::Stale(why) => {
            write(f, p, limit, hook_flag)?;
            Ok(Ensured::Written(why))
        }
    }
}

/// lmgw's own files, from the real file system: the spec, the stamp and the
/// names in the spec dir.
fn on_disk(p: &SpecPaths) -> OnDisk {
    OnDisk {
        spec: std::fs::read(&p.spec),
        stamp: std::fs::read(&p.stamp),
        spec_dir: names_in(&p.spec_dir),
    }
}

fn names_in(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<String>>>()?;
    names.sort();
    Ok(names)
}

/// `dir`, made `0700` when it is not there (its parent is), and checked to be
/// a real directory of this user that nobody else may write into or, after
/// this, enter: the spec names the machine's devices and driver files, and
/// Podman runs what its hooks name. A dir of this user's own that others can
/// only read (a `0755` an older umask left) is closed to `0700` and used; a
/// symbolic link, a file, another user's dir, or one others can write into
/// is refused, never used or changed.
fn ensure_private(dir: &Path) -> Result<(), CdiError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(CdiError::Io(format!("creating {}: {e}", dir.display()))),
    }
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| CdiError::Io(format!("reading {}: {e}", dir.display())))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let why = if meta.file_type().is_symlink() {
        Some("it is a symbolic link".to_string())
    } else if !meta.is_dir() {
        Some("it is not a directory".to_string())
    } else if meta.uid() != uid {
        Some(format!(
            "it belongs to user {}, not to this user ({uid})",
            meta.uid()
        ))
    } else if meta.mode() & 0o022 != 0 {
        Some(format!(
            "its mode {:04o} lets other users write into it",
            meta.mode() & 0o7777
        ))
    } else {
        None
    };
    if let Some(why) = why {
        return Err(CdiError::NotPrivate {
            dir: dir.to_path_buf(),
            why,
        });
    }
    if meta.mode() & 0o077 != 0 {
        // Through the opened dir, not the path: a link swapped in after the
        // check is not followed.
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)
            .map_err(|e| CdiError::Io(format!("opening {}: {e}", dir.display())))?;
        opened
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| CdiError::Io(format!("closing {} to other users: {e}", dir.display())))?;
        tracing::info!(
            "{} was open to other users to read (mode {:04o}); lmgw closed it (mode 0700)",
            dir.display(),
            meta.mode() & 0o7777
        );
    }
    Ok(())
}

/// Run `program` within what is left of `limit`, saying in the log that it
/// runs and, every [`STILL_RUNNING_EVERY`], that it still does. Killed when
/// the limit runs out: [`CdiError::TimedOut`], naming `what` and how long it
/// ran.
fn run_logged(
    f: &dyn Facts,
    program: &str,
    args: &[&str],
    limit: Limit,
    what: &str,
) -> Result<std::io::Result<CmdOut>, CdiError> {
    let started = Instant::now();
    let left = limit.deadline.saturating_duration_since(started);
    let timed_out = || CdiError::TimedOut {
        program: what.to_string(),
        ran: started.elapsed(),
        limit: limit.budget,
    };
    if left.is_zero() {
        return Err(timed_out());
    }
    tracing::debug!(
        "lmgw's CDI spec: running `{what}` (at most {:.1} s)",
        left.as_secs_f64()
    );
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let ticker = {
        let done = done.clone();
        let what = what.to_string();
        std::thread::spawn(move || {
            let (lock, cv) = &*done;
            let mut finished = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                // Not a bare wait: a program that ended before this thread
                // got here has already said so, and nobody would wake it.
                let (guard, waited) = cv
                    .wait_timeout_while(finished, STILL_RUNNING_EVERY, |done| !*done)
                    .unwrap_or_else(|e| e.into_inner());
                finished = guard;
                if *finished {
                    return;
                }
                if waited.timed_out() {
                    tracing::warn!(
                        "lmgw's CDI spec: `{what}` is still running after {:.0} s",
                        started.elapsed().as_secs_f64()
                    );
                }
            }
        })
    };
    let answer = f.run_limited(Path::new(program), args, left);
    *done.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    done.1.notify_all();
    let _ = ticker.join();
    match answer {
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Err(timed_out()),
        answer => Ok(answer),
    }
}

/// The minor numbers of the cards the driver lists in
/// [`NVIDIA_GPUS_DIR`]: each card's `Device Minor`, else its position.
fn card_minors(f: &dyn Facts) -> Vec<u32> {
    let Ok(mut cards) = f.list(Path::new(NVIDIA_GPUS_DIR)) else {
        return Vec::new();
    };
    cards.sort();
    cards
        .iter()
        .enumerate()
        .map(|(at, card)| {
            read_text(
                f,
                &Path::new(NVIDIA_GPUS_DIR).join(card).join("information"),
            )
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|l| l.strip_prefix("Device Minor:"))
                    .and_then(|m| m.trim().parse::<u32>().ok())
            })
            .unwrap_or(at as u32)
        })
        .collect()
}

/// CUDA's unified-memory node and one node per card, which nvidia-ctk names
/// only when they are there. NVIDIA's refresh creates the control nodes
/// before it generates, and on a machine just booted a second card's node
/// would otherwise appear while nvidia-ctk runs (its NVML opens every card).
/// `nvidia-modprobe` (setuid root, a Requires of the driver package) does the
/// same without a password. Best effort: without it the spec is still
/// written, and the log says what the containers will miss. Only running out
/// of time is an error.
fn device_nodes(f: &dyn Facts, limit: Limit) -> Result<(), CdiError> {
    let uvm_missing = !exists(f, Path::new(NVIDIA_UVM));
    let missing_cards: Vec<u32> = card_minors(f)
        .into_iter()
        .filter(|m| !exists(f, Path::new(&format!("{DEV_DIR}/nvidia{m}"))))
        .collect();
    if !uvm_missing && missing_cards.is_empty() {
        return Ok(());
    }
    if !exists(f, Path::new(NVIDIA_MODPROBE)) {
        tracing::warn!(
            "{NVIDIA_UVM} or a card's node is not there and {NVIDIA_MODPROBE} is not installed to              create it: lmgw's CDI spec is written without it, and CUDA in a container may find              no device"
        );
        return Ok(());
    }
    // `-c=0` also makes the first card's node.
    let mut calls: Vec<Vec<String>> = Vec::new();
    if uvm_missing {
        calls.push(vec!["-u".into(), "-c=0".into()]);
    }
    calls.extend(
        missing_cards
            .into_iter()
            .filter(|m| !(uvm_missing && *m == 0))
            .map(|m| vec![format!("-c={m}")]),
    );
    for call in calls {
        let args: Vec<&str> = call.iter().map(String::as_str).collect();
        let what = format!("nvidia-modprobe {}", call.join(" "));
        match run_logged(f, NVIDIA_MODPROBE, &args, limit, &what)? {
            Ok(out) if out.ok() => {}
            Ok(out) => tracing::warn!(
                "`{what}` exited {}: {}",
                out.status_text(),
                out.stderr.trim()
            ),
            Err(e) => tracing::warn!("{NVIDIA_MODPROBE} could not be run: {e}"),
        }
    }
    Ok(())
}

/// What `nvidia-ctk --version` says its version is (`NVIDIA Container
/// Toolkit CLI version 1.20.1` → `1.20.1`), for the stamp's detail.
fn ctk_version(f: &dyn Facts, limit: Limit) -> Result<Option<String>, CdiError> {
    let Ok(out) = run_logged(f, NVIDIA_CTK, &["--version"], limit, "nvidia-ctk --version")? else {
        return Ok(None);
    };
    Ok(out.stdout.lines().next().and_then(|line| {
        line.rsplit_once("version ")
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }))
}

/// The lines of a failed generation that say why: its error-level log lines,
/// else its last line. The whole output goes to lmgw's log.
fn why_lines(out: &CmdOut) -> Vec<String> {
    let lines: Vec<&str> = out
        .stderr
        .lines()
        .chain(out.stdout.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let errors: Vec<String> = lines
        .iter()
        .filter(|l| l.contains("level=error") || l.contains("level=fatal"))
        .map(|l| l.to_string())
        .collect();
    if !errors.is_empty() {
        return errors;
    }
    lines
        .last()
        .map(|l| vec![l.to_string()])
        .unwrap_or_default()
}

/// Write `bytes` to `path` through a temporary file in the same directory
/// and a rename, so a reader sees the old file or the new one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CdiError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    let io = |e: std::io::Error| CdiError::Io(format!("writing {}: {e}", path.display()));
    {
        let mut file = std::fs::File::create(&tmp).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

/// Whether the process `pid` runs (or exists as someone else's): `kill` with
/// no signal.
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 sends nothing; `kill` only checks that `pid` exists
    // and may be signalled, and touches no memory of ours.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The staging dirs of lmgw processes that ended mid-write (killed, crashed),
/// removed. Run under the one-generation lock; another process's dir is left
/// while that process runs.
fn sweep_staging(root: &Path) {
    let Ok(names) = names_in(root) else {
        return;
    };
    let me = std::process::id();
    for name in names {
        let Some(pid) = name
            .strip_prefix(STAGING_PREFIX)
            .and_then(|p| p.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if u32::try_from(pid).is_ok_and(|p| p == me) || alive(pid) {
            continue;
        }
        let dir = root.join(&name);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(
                "removed {}, left by lmgw process {pid}, which is gone",
                dir.display()
            ),
            Err(e) => tracing::warn!("could not remove {}: {e}", dir.display()),
        }
    }
}

/// Everything in the spec dir but the spec: Podman would read it beside
/// lmgw's (an older lmgw's `nvidia.yaml`, anything put there). lmgw's own
/// dir, so it goes.
fn remove_extra(spec_dir: &Path) -> Result<(), CdiError> {
    let names = names_in(spec_dir)
        .map_err(|e| CdiError::Io(format!("listing {}: {e}", spec_dir.display())))?;
    for name in names.into_iter().filter(|n| n != OWN_SPEC_FILE) {
        let path = spec_dir.join(&name);
        let gone = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(e) => Err(e),
        };
        match gone {
            Ok(()) => tracing::info!(
                "removed {} from lmgw's CDI spec dir: Podman would read it beside lmgw's spec",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CdiError::Io(format!("removing {}: {e}", path.display()))),
        }
    }
    Ok(())
}

/// Write the spec and its stamp again. When the machine changed while
/// nvidia-ctk ran (on a machine just booted its NVML makes the second card's
/// node, a package transaction may finish), once more from a fresh reading,
/// within the same limit; a second change is the start's answer.
fn write(
    f: &dyn Facts,
    p: &SpecPaths,
    limit: Limit,
    hook_flag: &HookFlagCache,
) -> Result<(), CdiError> {
    match write_once(f, p, limit, hook_flag) {
        Err(CdiError::Changed(first)) => {
            tracing::info!(
                "the machine changed while lmgw's CDI spec was written ({}); writing it again",
                reasons(&first)
            );
            write_once(f, p, limit, hook_flag)
        }
        done => done,
    }
}

fn write_once(
    f: &dyn Facts,
    p: &SpecPaths,
    limit: Limit,
    hook_flag: &HookFlagCache,
) -> Result<(), CdiError> {
    device_nodes(f, limit)?;
    // Read after the device nodes are made, so the stamp records the nodes the
    // spec will name.
    let current = Current::read(f);
    let written_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let Some(stamp) = current.stamp(None, written_at) else {
        // Between the judgement and now the toolkit went, or the driver
        // started keeping a spec from being written.
        return Err(match current.nvidia_ctk {
            None => CdiError::Blocked(lmgw_machine::cdi::Blocked::NoToolkit { stale: Vec::new() }),
            Some(_) => CdiError::Changed(vec![Stale::Machine]),
        });
    };
    sweep_staging(&p.root);
    // Private to this process: two lmgw processes on one prefix (dev copies
    // share theirs) each stage their own and rename a whole file into place.
    let staging = p
        .root
        .join(format!("{STAGING_PREFIX}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    ensure_private(&staging)?;
    let written = generate_into(f, p, &current, stamp, &staging, limit, hook_flag);
    let _ = std::fs::remove_dir_all(&staging);
    written
}

/// What `nvidia-ctk cdi generate --help` said about [`HOOK_FLAG`], for the
/// `nvidia-ctk` file it was asked of.
pub(super) type HookFlagCache = Mutex<Option<(FileFingerprint, bool)>>;

/// The flag that names the hook binary in the spec.
const HOOK_FLAG: &str = "--nvidia-cdi-hook-path";

/// Whether this `nvidia-ctk cdi generate` has a `--nvidia-cdi-hook-path`
/// flag (toolkits from 1.17 on; older ones call their hooks through
/// `nvidia-ctk` itself and have neither the flag nor the binary). Asked of
/// its `--help` once per `nvidia-ctk` file, remembered under the file's
/// fingerprint; an answer that cannot be read counts as "has it", which is
/// the toolkit lmgw was written against.
fn knows_hook_flag(
    f: &dyn Facts,
    asked: &HookFlagCache,
    ctk: Option<FileFingerprint>,
    limit: Limit,
) -> Result<bool, CdiError> {
    if let Some(ctk) = ctk {
        if let Some((seen, knows)) = *asked.lock().unwrap_or_else(|e| e.into_inner()) {
            if seen == ctk {
                return Ok(knows);
            }
        }
    }
    let answer = run_logged(
        f,
        NVIDIA_CTK,
        &["cdi", "generate", "--help"],
        limit,
        "nvidia-ctk cdi generate --help",
    )?;
    let Ok(out) = answer else {
        return Ok(true);
    };
    let knows = out.stdout.contains(HOOK_FLAG) || out.stderr.contains(HOOK_FLAG);
    if let Some(ctk) = ctk {
        *asked.lock().unwrap_or_else(|e| e.into_inner()) = Some((ctk, knows));
    }
    Ok(knows)
}

fn generate_into(
    f: &dyn Facts,
    p: &SpecPaths,
    current: &Current,
    mut stamp: lmgw_machine::cdi::Stamp,
    staging: &Path,
    limit: Limit,
    hook_flag: &HookFlagCache,
) -> Result<(), CdiError> {
    stamp.nvidia_ctk.version = ctk_version(f, limit)?;
    let staged = staging.join(OWN_SPEC_FILE);
    let output = format!("--output={}", staged.display());
    // Pin the hook binary when this toolkit installed one: the spec then
    // names a path that exists. A toolkit without the flag has no such binary
    // and generates without it; one with the flag and no binary at the
    // expected path is left to its own default, and says so in the log.
    let mut gen_args: Vec<String> = vec!["cdi".into(), "generate".into(), "--format=json".into()];
    gen_args.push(output);
    if knows_hook_flag(f, hook_flag, current.nvidia_ctk, limit)? {
        if exists(f, Path::new(NVIDIA_CDI_HOOK)) {
            gen_args.push(format!("{HOOK_FLAG}={NVIDIA_CDI_HOOK}"));
        } else {
            tracing::warn!(
                "{NVIDIA_CDI_HOOK} is not there although this nvidia-ctk takes {HOOK_FLAG}: \
                 lmgw's CDI spec is written with the toolkit's own default for the hook"
            );
        }
    }
    let gen_refs: Vec<&str> = gen_args.iter().map(String::as_str).collect();
    tracing::info!(
        "writing lmgw's CDI spec with `nvidia-ctk cdi generate` (stopped after {} s, the load \
         timeout)",
        limit.budget.as_secs()
    );
    let out = run_logged(f, NVIDIA_CTK, &gen_refs, limit, "nvidia-ctk cdi generate")?
        .map_err(|e| CdiError::Io(format!("{NVIDIA_CTK} could not be run: {e}")))?;
    if !out.ok() {
        tracing::warn!(
            "`nvidia-ctk cdi generate` exited {}:\n{}{}",
            out.status_text(),
            out.stderr,
            out.stdout
        );
        return Err(CdiError::Generate {
            status: out.status_text(),
            said: why_lines(&out),
        });
    }
    let bytes = std::fs::read(&staged).map_err(|e| {
        CdiError::Io(format!(
            "`nvidia-ctk cdi generate` exited 0 and {} could not be read: {e}",
            staged.display()
        ))
    })?;
    let spec = Spec::parse(&bytes, Format::Json).map_err(|e| {
        CdiError::Io(format!(
            "what `nvidia-ctk cdi generate` wrote is not a spec: {e}"
        ))
    })?;
    let gone = paths_gone(f, &spec);
    if !gone.is_empty() {
        return Err(CdiError::Incomplete(gone));
    }
    // A package transaction, a driver or toolkit update that ran while
    // nvidia-ctk did: what it wrote may describe neither machine.
    let changed = Current::read(f).changes_since(current);
    if !changed.is_empty() {
        return Err(CdiError::Changed(changed));
    }
    remove_extra(&p.spec_dir)?;
    std::fs::rename(&staged, &p.spec)
        .map_err(|e| CdiError::Io(format!("moving the spec to {}: {e}", p.spec.display())))?;
    let json = serde_json::to_vec_pretty(&stamp)
        .map_err(|e| CdiError::Io(format!("the stamp could not be encoded: {e}")))?;
    write_atomic(&p.stamp, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_generation_names_its_error_lines_else_its_last_line() {
        let out = CmdOut::failure(
            1,
            "time=\"…\" level=info msg=\"Selecting /dev/nvidia0\"\n\
             time=\"…\" level=error msg=\"failed to initialize NVML: Driver/library version mismatch\"\n",
        );
        assert_eq!(why_lines(&out).len(), 1);
        assert!(why_lines(&out)[0].contains("version mismatch"));
        let plain = CmdOut::failure(2, "first\nlast\n");
        assert_eq!(why_lines(&plain), vec!["last".to_string()]);
    }

    #[test]
    fn a_dir_that_is_not_private_is_refused_and_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let open = base.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Ours and only readable by others: closed and used.
        ensure_private(&open).unwrap();
        assert_eq!(
            std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Others may write into it: refused, left alone.
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let e = ensure_private(&open).unwrap_err();
        assert!(
            e.to_string()
                .contains("its mode 0777 lets other users write into it"),
            "{e}"
        );
        assert_eq!(
            std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
            0o777,
            "not changed"
        );

        let link = base.path().join("link");
        std::os::unix::fs::symlink(base.path(), &link).unwrap();
        let e = ensure_private(&link).unwrap_err();
        assert!(e.to_string().contains("symbolic link"), "{e}");

        let file = base.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(ensure_private(&file)
            .unwrap_err()
            .to_string()
            .contains("not a directory"));

        let new = base.path().join("new");
        ensure_private(&new).unwrap();
        assert_eq!(
            std::fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn a_staging_dir_of_a_process_that_is_gone_is_swept_and_a_live_ones_kept() {
        let root = tempfile::tempdir().unwrap();
        // pid_max is at most 2^22 on Linux: this pid cannot exist.
        let dead = root.path().join(format!("{STAGING_PREFIX}{}", 1 << 23));
        let mine = root
            .path()
            .join(format!("{STAGING_PREFIX}{}", std::process::id()));
        let other = root.path().join("cdi");
        for d in [&dead, &mine, &other] {
            std::fs::create_dir(d).unwrap();
        }
        sweep_staging(root.path());
        assert!(!dead.exists());
        assert!(mine.exists() && other.exists());
    }
}
