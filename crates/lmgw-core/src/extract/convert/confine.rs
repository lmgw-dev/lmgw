//! Which tools run confined, and what each may reach (document-conversion
//! design §5.1, §14 WP7 review round). The confinement itself is lmgw's
//! reader executable's (`lmgw-doc-reader run`, Landlock: the reader crate's
//! `sandbox` and `run` modules); the runner starts a tool through it when
//! its `Tool::confine` is set (`run/confined.rs`).
//!
//! **LibreOffice runs only confined.** A document can make LibreOffice read
//! files it names (an RTF's `INCLUDEPICTURE "/any/path"` is embedded into
//! the docx it writes), and no profile setting stops that, so the boundary
//! is the kernel's: where the confinement cannot be applied (no Landlock in
//! the kernel, Landlock not among its security modules, lmgw's reader
//! missing, a system other than Linux), LibreOffice's probe fails by name
//! ([`gate`]) and the capability table says so on every row that needs it;
//! LibreOffice is never run unconfined.
//!
//! **pandoc, ffmpeg, poppler and tesseract run confined where the
//! confinement can be applied,** and elsewhere with their memory limit and
//! closed descriptors alone (still started by lmgw's reader, `run/start.rs`),
//! in their own process group (pandoc with its `--sandbox`, ffmpeg with its
//! `file`-only protocol list). poppler and tesseract since the performance
//! round (2026-11-19): they read their input on stdin (a pipe, or the
//! file's spool, which poppler opens again as `/dev/stdin`: the run's rules
//! carry a read right on that one file, `--read-stdin`) and write stdout,
//! so their rules are the system's files alone ([`poppler`],
//! [`tesseract`]). Where they are installed outside `/usr` their rules
//! reach only what [`reach`] allows; where that is not enough, or their
//! confined check run at the probe failed ([`verdict`]), they run with
//! their limits alone and Settings › Documents says why.
//!
//! What each may reach ([`libreoffice`], [`filter`], [`poppler`],
//! [`tesseract`]):
//!
//! | path | rights | who |
//! |---|---|---|
//! | `/usr` | read, execute (LibreOffice: read only) | all |
//! | LibreOffice's install directory (never `/`, a home or a directory holding a home or lmgw's data, settings or cache: `reach::libreoffice_install`, else LibreOffice is refused by name), its start script's programs and the dynamic loader (`LIBREOFFICE_EXECS`) | read, execute | LibreOffice |
//! | the binary's directory and its prefix's `lib`, `lib64`, `libexec`, `share`, for a tool outside `/usr`, each never `/`, a home or a directory holding a home or lmgw's data, settings or cache (`reach::install_dirs`); `/nix/store` for a tool in it | read, execute | poppler, tesseract |
//! | `/etc/ld.so.cache` | read | all |
//! | `/etc/fonts` | read | LibreOffice, poppler |
//! | `/var/cache/fontconfig` (Debian's and Ubuntu's system font cache) | read | poppler |
//! | `/etc/passwd`, `/etc/group`, `/etc/nsswitch.conf` | read | LibreOffice |
//! | `/dev/null` | read, write | all |
//! | `/dev/urandom` | read | LibreOffice |
//! | the file that is the run's stdin (a spool file), that file alone | read | poppler |
//! | tesseract's language data, when it is outside `/usr`: only a directory holding `*.traineddata` and passing the same check (`reach::tessdata`) | read | tesseract |
//! | the run's own directory | read, write, create, remove (not execute) | LibreOffice, ffmpeg, poppler's page squares for the layout model |
//! | `/tmp` | create a Unix socket, nothing else | LibreOffice |
//!
//! poppler finds the fonts a PDF does not embed through fontconfig, which
//! reads `/etc/fonts` and the system's fonts and caches under `/usr` (or
//! `/var/cache/fontconfig`); fonts a user installed in their home directory
//! are not seen yet (no home is reachable, and fontconfig would need its
//! per-user cache there as well), so such a PDF renders with the system's
//! substitute.
//!
//! Nothing else: no other file under the data dir, no home directory
//! itself or any directory above one (a tool's own install directories
//! inside a home, such as `~/.local/bin`, pass [`reach`]'s check), nothing
//! of lmgw's own data, settings or cache directories, nothing else in
//! `/tmp`, nothing in `/run/user`, `/proc` or
//! `/sys`. `/tmp`'s one right is LibreOffice's single-instance socket
//! (`/tmp/OSL_PIPE_<uid>_SingleOfficeIPC_<hash of the profile's URL>`):
//! LibreOffice puts it in `/tmp` whenever `/tmp` is writable for the user,
//! which Landlock cannot hide, and does not start without it. It can neither
//! remove the socket nor anything else there, so the gateway removes it
//! after the run (`libreoffice.rs`). Each run starts from a copy of a
//! profile made once by a run without a document, so LibreOffice starts once
//! (a new profile makes it restart, and the second start would find the
//! first one's socket).
//!
//! A run directory's write right makes no symbolic link and moves nothing
//! in from another directory (the reader's `abi::WRITE`), and the gateway
//! opens what a tool wrote there without following one
//! (`crate::child::beneath`; security round B, L3), and no more of it than
//! such a result can be. A process of the run that left its process group
//! (`setsid`) survives the group kill, under the run's rules and limits:
//! ending it needs a supervising parent, which the reader is not yet. So
//! the gateway maps no file a tool can still write (ffmpeg's WAV is copied
//! first, `media/ffmpeg/mapped.rs`), and every run has a CPU time bound such
//! a process keeps (`run::cpu`; the review of round B, S3).
//!
//! **What the kernel's ABI decides** (security round B, M3; the reader's
//! `sandbox::abi`). TCP bind and connect are denied from Landlock ABI 4
//! (Linux 6.7), UDP bind and send from ABI 10; pathname Unix sockets are
//! denied from ABI 9 (their lookup), abstract ones and signals to processes
//! outside the run from ABI 6; truncating a file outside the rules from ABI
//! 3 (below it a tool running hostile code could empty any file the
//! gateway's user may write, its database included), `ioctl` on devices
//! from ABI 5. On an older kernel each of these stays open:
//! every one the running kernel lacks is named with the ABI found, once in
//! the log ([`available`]) and on Settings › Documents' "Programs run
//! confined" (`verdict::confinement`). LibreOffice, whose documents can make
//! it send requests, needs the network denied: below ABI 4
//! ([`LIBREOFFICE_MIN_ABI`]) its probe fails by name, as it does without
//! Landlock, and it is not used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use lmgw_doc_reader::sandbox::{self, Confine};

use super::caps::ToolId;
use super::probe::Located;
use super::tools::{Fact, ToolFacts};

/// How long `lmgw-doc-reader run --probe` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// What LibreOffice's probe says when it cannot be confined; the reason
/// follows.
pub const NEEDS_LANDLOCK: &str = "LibreOffice runs only confined with its network denied, which \
                                  needs Landlock ABI 4 or newer (Linux 6.7, with Landlock enabled \
                                  among the kernel's security modules) and lmgw's document \
                                  reader; it is not used here";

/// The oldest Landlock ABI LibreOffice runs under: the first that denies
/// TCP (module doc).
pub const LIBREOFFICE_MIN_ABI: u32 = lmgw_doc_reader::sandbox::abi::TCP_ABI;

/// `Err` naming the ABI found when it is too old for LibreOffice.
fn libreoffice_floor(abi: u32) -> Result<(), String> {
    if abi >= LIBREOFFICE_MIN_ABI {
        return Ok(());
    }
    Err(format!(
        "this kernel's Landlock ABI is {abi}, which cannot deny a program TCP connections \
         (LIBREOFFICE_MIN_ABI, {LIBREOFFICE_MIN_ABI})"
    ))
}

mod reach;
pub mod verdict;

pub use reach::{own_data_dirs, protect};

/// One probe's answer, and whether it is the kernel's own (an ABI, or the
/// reader's line saying Landlock is missing or refused): only those stand
/// for the process's life; a probe that timed out or could not start is
/// asked again at the next tool check (performance round review S5).
#[derive(Clone)]
struct Answer {
    said: Result<u32, String>,
    definitive: bool,
}

/// The probes' answers, by the reader that was asked.
static ASKED: Mutex<Option<HashMap<PathBuf, Answer>>> = Mutex::new(None);

fn asked() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, Answer>>> {
    ASKED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the reader at `helper` can confine a program here: the Landlock
/// ABI it applied, or why not. Asked once per reader and process (the
/// kernel's Landlock does not change while it runs), except that an answer
/// that was not the kernel's ([`Answer::definitive`]) is asked again at the
/// next tool check ([`recheck`]).
pub async fn available(helper: &Path) -> Result<u32, String> {
    if let Some(known) = asked().as_ref().and_then(|m| m.get(helper)) {
        return known.said.clone();
    }
    let answer = probe(helper).await;
    match &answer.said {
        Ok(abi) => {
            tracing::info!("document tools are confined with Landlock ABI {abi}");
            let lacks = lmgw_doc_reader::sandbox::abi::lacking_said(*abi);
            if !lacks.is_empty() {
                tracing::warn!(
                    "this kernel's Landlock leaves parts of the document tools' confinement \
                     open: {}",
                    lacks.join("; ")
                );
            }
        }
        Err(why) if answer.definitive => tracing::warn!("document tools cannot be confined: {why}"),
        Err(why) => tracing::warn!(
            "document tools run without confinement until the next tool check: {why}"
        ),
    }
    asked()
        .get_or_insert_with(HashMap::new)
        .insert(helper.to_path_buf(), answer.clone());
    answer.said
}

/// [`available`], asking again when the answer it has was not the
/// kernel's: every tool check does (at start, "Check again", a tool that
/// changed).
pub async fn recheck(helper: &Path) -> Result<u32, String> {
    if let Some(m) = asked().as_mut() {
        if m.get(helper).is_some_and(|a| !a.definitive) {
            m.remove(helper);
        }
    }
    available(helper).await
}

/// The answer [`available`] has for `helper`, without asking.
fn answer(helper: &Path) -> Option<Result<u32, String>> {
    asked()
        .as_ref()
        .and_then(|m| m.get(helper))
        .map(|a| a.said.clone())
}

/// The ABI [`available`] found for `helper`, without asking.
fn known(helper: &Path) -> Option<u32> {
    answer(helper).and_then(Result::ok)
}

async fn probe(helper: &Path) -> Answer {
    let transient = |why: String| Answer {
        said: Err(why),
        definitive: false,
    };
    match probe_run(helper).await {
        Ok(out) => out,
        Err(why) => transient(why),
    }
}

async fn probe_run(helper: &Path) -> Result<Answer, String> {
    let mut cmd = tokio::process::Command::new(helper);
    cmd.args([sandbox::RUN, sandbox::PROBE])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::child::system_tool_tokio(&mut cmd);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "lmgw's document reader did not answer within {} s",
                PROBE_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("lmgw's document reader could not be run: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match sandbox::parse_probe(&stdout) {
        Some(abi) if out.status.success() => Ok(Answer {
            said: Ok(abi),
            definitive: true,
        }),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("");
            // The reader's own line about the kernel (exit 125) stands;
            // anything else (a crash, a stray answer) is asked again.
            let own = first.starts_with(&sandbox::status_prefix(None))
                && out.status.code() == Some(sandbox::EXIT_UNCONFINED);
            let why = Some(super::run::start::reader_line(&err))
                .filter(|l| !l.is_empty())
                .map_or_else(|| out.status.to_string(), str::to_string);
            Ok(Answer {
                said: Err(why),
                definitive: own,
            })
        }
    }
}

/// LibreOffice's fact once its confinement is asked about: a found
/// LibreOffice that cannot be confined is a failed probe naming why, so
/// nothing runs it (module doc). `own` is where lmgw's reader is looked
/// up. Other tools' facts pass as they are.
pub async fn gate(id: ToolId, fact: Fact, own: &std::ffi::OsStr) -> Fact {
    // The reader found: whether it confines here is asked now (again,
    // when its last answer was not the kernel's), so pandoc and ffmpeg
    // know it when they run ([`helper`]).
    if let (ToolId::Reader, Fact::Found(f)) = (id, &fact) {
        let _ = recheck(&f.located.path).await;
    }
    // poppler, tesseract, pandoc and ffmpeg run once under their rules
    // (`verdict`).
    if let (ToolId::Poppler | ToolId::Tesseract | ToolId::Pandoc | ToolId::Ffmpeg, Fact::Found(f)) =
        (id, &fact)
    {
        let reader = super::caps::tool(ToolId::Reader);
        if let Some(helper) =
            super::probe::lookup(reader.probe.binaries[0], own, reader.probe.install_dirs)
        {
            if recheck(&helper.path).await.is_ok() {
                verdict::check(id, f, &helper.path).await;
            }
        }
    }
    if id != ToolId::LibreOffice {
        return fact;
    }
    let Fact::Found(found) = fact else {
        return fact;
    };
    let failed = |located: Located, why: String| Fact::ProbeFailed {
        located,
        message: format!("{NEEDS_LANDLOCK}: {why}"),
    };
    let reader = super::caps::tool(ToolId::Reader);
    let Some(helper) =
        super::probe::lookup(reader.probe.binaries[0], own, reader.probe.install_dirs)
    else {
        return failed(
            found.located,
            format!(
                "lmgw's document reader ({}) was not found beside lmgw",
                super::reader::READER_BIN
            ),
        );
    };
    // Asked again when the last answer was not the kernel's (the reader's
    // own gate may not have run yet in this check).
    if let Err(why) = recheck(&helper.path).await.and_then(libreoffice_floor) {
        return failed(found.located, why);
    }
    // Its install directory must be one a rule may hold (review S5).
    match reach::libreoffice_install(&found.located.path, &reach::Places::now()) {
        Ok(_) => Fact::Found(found),
        Err(why) => Fact::ProbeFailed {
            located: found.located,
            message: why,
        },
    }
}

/// The reader that confines tools here, when the confinement was found to
/// work: what pandoc and ffmpeg run through (`None`: they run as before).
pub fn helper(facts: &ToolFacts) -> Option<PathBuf> {
    let helper = super::reader::path(facts)?;
    known(&helper).map(|_| helper)
}

/// [`helper`] for tool `id` at `bin` (pandoc, ffmpeg): `None` also when
/// its confined check run at the probe failed (`verdict`), so it runs with
/// its limits alone, as Settings › Documents says.
pub fn helper_for(facts: &ToolFacts, id: ToolId, bin: &Path) -> Option<PathBuf> {
    helper(facts).filter(|_| verdict::failure(id, bin).is_none())
}

/// The reader LibreOffice runs through: it must be found and confine.
pub async fn required(facts: &ToolFacts) -> Result<PathBuf, String> {
    let helper = super::reader::path(facts).ok_or_else(|| {
        format!(
            "{NEEDS_LANDLOCK}: lmgw's document reader ({}) was not found beside lmgw",
            super::reader::READER_BIN
        )
    })?;
    available(&helper)
        .await
        .and_then(libreoffice_floor)
        .map(|()| helper)
        .map_err(|why| format!("{NEEDS_LANDLOCK}: {why}"))
}

/// What every confined tool reads and executes from: the system's programs
/// and libraries.
const SYSTEM: &[&str] = &["/usr"];

/// What every confined tool reads: the dynamic loader's cache.
const LOADER: &[&str] = &["/etc/ld.so.cache"];

/// What LibreOffice reads besides (fonts, the user database its start
/// script and Java ask).
const LIBREOFFICE_READS: &[&str] = &[
    "/etc/fonts",
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/dev/urandom",
];

/// Where LibreOffice's single-instance socket goes (module doc).
pub const LIBREOFFICE_SOCKET_DIR: &str = "/tmp";

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

/// What LibreOffice executes besides its own install (security round B,
/// hardening 8): Fedora's `soffice` start script and the programs it
/// calls (`basename`, `dirname`, `grep`, `ls`, `paperconf`, `sed`,
/// `uname`, seen with `strace -f -e trace=execve` on LibreOffice 26.8.2.2,
/// 2026-11-19), its shell, and the dynamic loader of the usual systems
/// (the kernel checks the loader's execute right too). A path that does not
/// exist is skipped by the reader. Java, which the start probes for, is not
/// among them: the conversions are the same without it (§14).
const LIBREOFFICE_EXECS: &[&str] = &[
    "/usr/bin/sh",
    "/usr/bin/bash",
    "/usr/bin/dash",
    "/usr/bin/basename",
    "/usr/bin/dirname",
    "/usr/bin/grep",
    "/usr/bin/ls",
    "/usr/bin/paperconf",
    "/usr/bin/sed",
    "/usr/bin/uname",
    "/usr/lib64/ld-linux-x86-64.so.2",
    "/usr/lib/ld-linux-aarch64.so.1",
    "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
    "/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
];

/// LibreOffice's rules: `soffice` at `bin` (its install's top directory
/// read and executed, `/usr` read only, [`LIBREOFFICE_EXECS`] executed),
/// the run's directory `run`, files up to `file_size` bytes.
/// `Err` when its install directory may not be in the rules
/// (`reach::libreoffice_install`): LibreOffice is then not run.
pub fn libreoffice(bin: &Path, run: &Path, file_size: u64) -> Result<Confine, String> {
    let mut read_exec = paths(LIBREOFFICE_EXECS);
    // `/usr/lib64/libreoffice/program/soffice`: the install's top directory.
    read_exec.extend(reach::libreoffice_install(bin, &reach::Places::now())?);
    let mut read = paths(SYSTEM);
    read.extend(paths(LOADER));
    read.extend(paths(LIBREOFFICE_READS));
    Ok(Confine {
        read_exec,
        read,
        read_write: vec![run.to_path_buf(), PathBuf::from("/dev/null")],
        make_sock: vec![PathBuf::from(LIBREOFFICE_SOCKET_DIR)],
        deny_net: true,
        file_size: Some(file_size),
        ..Confine::default()
    })
}

/// The rules of a tool that reads its input and writes its output in
/// `run` (ffmpeg) or on stdin and stdout (pandoc, `run` `None`), files up
/// to `file_size` bytes.
pub fn filter(run: Option<&Path>, file_size: u64) -> Confine {
    let mut read_write = vec![PathBuf::from("/dev/null")];
    read_write.extend(run.map(Path::to_path_buf));
    Confine {
        read_exec: paths(SYSTEM),
        read: paths(LOADER),
        read_write,
        deny_net: true,
        file_size: Some(file_size),
        ..Confine::default()
    }
}

/// Why a tool runs without confinement, said once per tool and reason in
/// the log (Settings › Documents says it on the tool's row,
/// `verdict::state`).
fn unconfined(bin: &Path, why: &str) {
    static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let line = format!("{}: {why}", bin.display());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&line) {
        tracing::warn!("document tool {line}; it runs with its limits alone, not confined");
        said.push(line);
    }
}

/// poppler's rules for the tool at `bin` (module doc): the system's files
/// and fonts, the directories it is installed in when it is outside `/usr`
/// (`reach::install_dirs`), and `write` when the run writes its pages into
/// a private directory (the layout model's squares). `None`: it runs with
/// its limits alone, the reason said in the log and on Settings ›
/// Documents ([`poppler_rules`]).
pub fn poppler(bin: &Path, write: Option<&Path>) -> Option<Confine> {
    poppler_rules(bin, write)
        .map_err(|why| unconfined(bin, &why))
        .ok()
}

/// [`poppler`], or why the tool cannot be confined: where it is installed
/// (S1), or a confined check run that failed (`verdict`).
pub fn poppler_rules(bin: &Path, write: Option<&Path>) -> Result<Confine, String> {
    if let Some(why) = verdict::failure(ToolId::Poppler, bin) {
        return Err(why);
    }
    let mut read = paths(LOADER);
    read.extend(paths(POPPLER_READS));
    let mut read_write = vec![PathBuf::from("/dev/null")];
    read_write.extend(write.map(Path::to_path_buf));
    let mut read_exec = paths(SYSTEM);
    read_exec.extend(reach::install_dirs(bin)?);
    Ok(Confine {
        read_exec,
        read,
        read_write,
        deny_net: true,
        ..Confine::default()
    })
}

/// What poppler reads besides the system: fontconfig's configuration, and
/// the system font cache where Debian and Ubuntu keep it (Fedora's is under
/// `/usr`; review N6).
const POPPLER_READS: &[&str] = &["/etc/fonts", "/var/cache/fontconfig"];

/// tesseract's rules for the tool at `bin` (module doc): the system's
/// files, the directories it is installed in when it is outside `/usr`, and
/// its language data `tessdata` (the directory its probe named) when that
/// is outside `/usr` and a language data directory (S2). `None`: it runs
/// with its limits alone, said as [`poppler`] says it.
pub fn tesseract(bin: &Path, tessdata: Option<&Path>) -> Option<Confine> {
    tesseract_rules(bin, tessdata)
        .map_err(|why| unconfined(bin, &why))
        .ok()
}

/// [`tesseract`], or why the tool cannot be confined.
pub fn tesseract_rules(bin: &Path, tessdata: Option<&Path>) -> Result<Confine, String> {
    if let Some(why) = verdict::failure(ToolId::Tesseract, bin) {
        return Err(why);
    }
    let mut read = paths(LOADER);
    if let Some(dir) = tessdata {
        read.extend(reach::tessdata(dir)?);
    }
    let mut read_exec = paths(SYSTEM);
    read_exec.extend(reach::install_dirs(bin)?);
    Ok(Confine {
        read_exec,
        read,
        read_write: vec![PathBuf::from("/dev/null")],
        deny_net: true,
        ..Confine::default()
    })
}

/// The reader beside the tests, when it confines here; `None` (said on
/// stderr, naming `test`) skips a test that needs it.
#[cfg(all(test, unix))]
pub(crate) async fn test_helper(test: &str) -> Option<PathBuf> {
    let reader = super::caps::tool(ToolId::Reader);
    let own = super::reader::own_dirs();
    let Some(found) =
        super::probe::lookup(reader.probe.binaries[0], &own, reader.probe.install_dirs)
    else {
        eprintln!("{test}: skipped, lmgw's document reader is not built beside the tests");
        return None;
    };
    match available(&found.path).await {
        Ok(_) => Some(found.path),
        Err(why) => {
            eprintln!("{test}: skipped, no confinement here: {why}");
            None
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
