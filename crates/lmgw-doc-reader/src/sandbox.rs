//! The reader executable's second job: starting another program confined
//! (document-conversion design §5.1, §14 WP7 review round). The gateway
//! starts `lmgw-doc-reader run [rules] -- <program> <args…>`; the reader
//! closes its inherited descriptors above stdio, sets the limits it was
//! given, applies a Landlock ruleset built from the rules and `exec`s the
//! program in its own place (the same process, so the gateway's process
//! group and its kill reach the program and everything it starts). It
//! touches no document byte before the `exec`, and none of the reader's
//! parsers is on that path.
//!
//! This module is the command line both sides share: the gateway builds it
//! with [`Confine::args`], the reader reads it with [`Confine::parse`]. The
//! work itself is the binary's `run` module (Linux only: other systems have
//! no confinement yet, and the reader refuses a `run` there).
//!
//! **Why a helper and not a hook between the gateway's fork and exec:** a
//! hook makes the gateway fork, and a fork copies the gateway's page tables,
//! whose cost grows with the gateway's memory. The reader is started without
//! a fork (`posix_spawn`), so every tool moved behind this helper: since the
//! performance round (2026-11-19) every foreign tool of the conversion path
//! starts through it, with a Landlock ruleset or, for a tool that is not
//! confined yet or where Landlock is unavailable, with the limits and the
//! closed descriptors alone ([`Confine::limits_only`]).

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub mod abi;

/// The subcommand.
pub const RUN: &str = "run";

/// `run --probe`: the reader applies a ruleset to itself and prints
/// [`probe_line`] (exit 0), or says on stderr why it cannot and exits
/// [`EXIT_UNCONFINED`].
pub const PROBE: &str = "--probe";

/// The exit status when the confinement asked for could not be applied,
/// or the command line was no `run`: the program was not started. The
/// numbers are the ones GNU `env` and `timeout` use for their own failures
/// (125, 126, 127), which a tool's own exit status is rarely.
pub const EXIT_UNCONFINED: i32 = 125;

/// The exit status when the program could not be started otherwise.
pub const EXIT_NOT_STARTED: i32 = 126;

/// The exit status when the program to start does not exist.
pub const EXIT_NOT_FOUND: i32 = 127;

/// The probe's line before the ABI number.
const PROBE_PREFIX: &str = "landlock-abi ";

/// What a confined program may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confine {
    /// Read and execute beneath these (`--read-exec`).
    pub read_exec: Vec<PathBuf>,
    /// Read beneath these (`--read`): files read, directories listed.
    pub read: Vec<PathBuf>,
    /// Read, write, create and remove beneath these (`--read-write`), but
    /// not execute: what the program writes it cannot run.
    pub read_write: Vec<PathBuf>,
    /// Create Unix sockets beneath these and nothing else (`--make-sock`):
    /// LibreOffice's single-instance socket, which it puts in `/tmp`.
    pub make_sock: Vec<PathBuf>,
    /// TCP (Landlock ABI 4) and UDP (ABI 10) denied (`--deny-net`).
    pub deny_net: bool,
    /// `RLIMIT_AS`, in bytes (`--address-space`).
    pub address_space: Option<u64>,
    /// `RLIMIT_FSIZE`, in bytes (`--file-size`): a file written past it
    /// ends the program with `SIGXFSZ`.
    pub file_size: Option<u64>,
    /// `RLIMIT_CPU`, in seconds (`--cpu-seconds`): past it the program gets
    /// `SIGXCPU`, a second later `SIGKILL`. Every process it starts has the
    /// bound of its own, also one that left its process group.
    pub cpu_seconds: Option<u64>,
    /// No Landlock ruleset (`--limits-only`): the descriptors are closed and
    /// the limits set, and the program is `exec`ed as it is. A tool that is
    /// not confined yet, or one run where Landlock is unavailable; the path
    /// rules and `deny_net` are refused with it.
    pub limits_only: bool,
    /// Read the regular file that is stdin, and nothing else beside it
    /// (`--read-stdin`, performance round review B2): a file rule on the
    /// inode descriptor 0 is, so the program may open `/dev/stdin` again
    /// and read it by position (poppler does so for a path; told `fd://0`
    /// it reads the descriptor to its end into memory). Read only: the
    /// spool cannot be written through it. Refused when stdin is not a
    /// regular file.
    pub read_stdin: bool,
    /// The run's own word (`--status-token`), which the reader puts in each
    /// line it writes about its own failure ([`status_prefix`]), so a
    /// program's stderr cannot pass for the reader's (review N1): the
    /// program never sees it (its `exec` replaces the reader's arguments).
    pub status_token: Option<String>,
}

/// How the reader's own failure lines start: `lmgw-doc-reader run: `, or
/// with the run's token `lmgw-doc-reader run <token>: `.
pub fn status_prefix(token: Option<&str>) -> String {
    match token {
        Some(t) => format!("lmgw-doc-reader run {t}: "),
        None => "lmgw-doc-reader run: ".to_string(),
    }
}

/// The `--status-token` among `args` (before `--`), read before the rest
/// is parsed, so a command line the reader cannot parse is still answered
/// with the run's prefix.
pub fn token_of(args: &[OsString]) -> Option<String> {
    let mut it = args.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if a == "--status-token" {
            return it.next().and_then(|t| t.to_str()).map(str::to_string);
        }
    }
    None
}

impl Confine {
    /// The reader's arguments (after its own name) that start `program`
    /// with `args` under this confinement.
    pub fn args(&self, program: &OsStr, args: &[&OsStr]) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![RUN.into()];
        let paths = [
            ("--read-exec", &self.read_exec),
            ("--read", &self.read),
            ("--read-write", &self.read_write),
            ("--make-sock", &self.make_sock),
        ];
        for (flag, list) in paths {
            for p in list {
                out.push(flag.into());
                out.push(p.clone().into_os_string());
            }
        }
        if self.deny_net {
            out.push("--deny-net".into());
        }
        if self.limits_only {
            out.push("--limits-only".into());
        }
        if self.read_stdin {
            out.push("--read-stdin".into());
        }
        if let Some(t) = &self.status_token {
            out.push("--status-token".into());
            out.push(t.into());
        }
        if let Some(n) = self.address_space {
            out.push("--address-space".into());
            out.push(n.to_string().into());
        }
        if let Some(n) = self.file_size {
            out.push("--file-size".into());
            out.push(n.to_string().into());
        }
        if let Some(n) = self.cpu_seconds {
            out.push("--cpu-seconds".into());
            out.push(n.to_string().into());
        }
        out.push("--".into());
        out.push(program.to_os_string());
        out.extend(args.iter().map(|a| a.to_os_string()));
        out
    }

    /// The confinement, the program and its arguments `args` (after
    /// [`RUN`]) name; `Err` says what is wrong with them.
    pub fn parse(args: &[OsString]) -> Result<(Self, PathBuf, Vec<OsString>), String> {
        let mut c = Self::default();
        let mut it = args.iter();
        let number = |v: Option<&OsString>, flag: &str| -> Result<u64, String> {
            v.and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("{flag} needs a number"))
        };
        loop {
            let Some(a) = it.next() else {
                return Err("no `--` before the program".to_string());
            };
            let flag = a.to_str().unwrap_or("");
            let mut path = |list: &mut Vec<PathBuf>| match it.next() {
                Some(p) if !p.is_empty() => {
                    list.push(PathBuf::from(p));
                    Ok(())
                }
                _ => Err(format!("{flag} needs a path")),
            };
            match flag {
                "--read-exec" => path(&mut c.read_exec)?,
                "--read" => path(&mut c.read)?,
                "--read-write" => path(&mut c.read_write)?,
                "--make-sock" => path(&mut c.make_sock)?,
                "--deny-net" => c.deny_net = true,
                "--limits-only" => c.limits_only = true,
                "--read-stdin" => c.read_stdin = true,
                "--status-token" => match it.next().and_then(|t| t.to_str()) {
                    Some(t) if !t.is_empty() && !t.contains(char::is_whitespace) => {
                        c.status_token = Some(t.to_string())
                    }
                    _ => return Err("--status-token needs a word".to_string()),
                },
                "--address-space" => c.address_space = Some(number(it.next(), flag)?),
                "--file-size" => c.file_size = Some(number(it.next(), flag)?),
                "--cpu-seconds" => c.cpu_seconds = Some(number(it.next(), flag)?),
                "--" => break,
                other => return Err(format!("unknown option {other:?}")),
            }
        }
        let Some(program) = it.next() else {
            return Err("no program after `--`".to_string());
        };
        let program = PathBuf::from(program);
        let rules = !(c.read_exec.is_empty()
            && c.read.is_empty()
            && c.read_write.is_empty()
            && c.make_sock.is_empty())
            || c.deny_net
            || c.read_stdin;
        if c.limits_only && rules {
            return Err(
                "--limits-only takes no path rules, no --read-stdin and no --deny-net".to_string(),
            );
        }
        if !program.is_absolute() {
            return Err("the program must be named by an absolute path".to_string());
        }
        Ok((c, program, it.cloned().collect()))
    }
}

/// What `run --probe` prints for the Landlock ABI it applied.
pub fn probe_line(abi: u32) -> String {
    format!("{PROBE_PREFIX}{abi}")
}

/// The ABI a `run --probe` output names.
pub fn parse_probe(out: &str) -> Option<u32> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix(PROBE_PREFIX))
        .and_then(|n| n.trim().parse().ok())
        .filter(|&n| n >= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_line_reads_back_as_it_was_built() {
        let c = Confine {
            read_exec: vec!["/usr".into()],
            read: vec!["/etc/fonts".into(), "/etc/ld.so.cache".into()],
            read_write: vec!["/srv/run dir".into()],
            make_sock: vec!["/tmp".into()],
            deny_net: true,
            address_space: Some(4 << 30),
            file_size: Some(1 << 30),
            cpu_seconds: Some(9600),
            limits_only: false,
            read_stdin: true,
            status_token: Some("0a1b".into()),
        };
        let args = c.args(OsStr::new("/usr/bin/soffice"), &[OsStr::new("--headless")]);
        assert_eq!(args[0], RUN);
        let (back, program, rest) = Confine::parse(&args[1..]).unwrap();
        assert_eq!(back, c);
        assert_eq!(program, PathBuf::from("/usr/bin/soffice"));
        assert_eq!(rest, vec![OsString::from("--headless")]);
    }

    #[test]
    fn a_command_line_without_a_program_or_with_a_relative_one_is_refused() {
        let s = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(Confine::parse(&s(&["--read", "/usr"])).is_err());
        assert!(Confine::parse(&s(&["--"])).is_err());
        assert!(Confine::parse(&s(&["--", "soffice"])).is_err());
        assert!(Confine::parse(&s(&["--deny-net", "--", "/bin/true"])).is_ok());
        assert!(Confine::parse(&s(&["--file-size", "x", "--", "/bin/true"])).is_err());
        assert!(Confine::parse(&s(&["--cpu-seconds", "-1", "--", "/bin/true"])).is_err());
        assert!(Confine::parse(&s(&["--what", "--", "/bin/true"])).is_err());
        // Limits alone take no rules.
        let (c, _, _) = Confine::parse(&s(&[
            "--limits-only",
            "--address-space",
            "4096",
            "--",
            "/bin/true",
        ]))
        .unwrap();
        assert!(c.limits_only && c.address_space == Some(4096));
        assert!(Confine::parse(&s(&["--limits-only", "--deny-net", "--", "/bin/true"])).is_err());
        assert!(
            Confine::parse(&s(&["--limits-only", "--read", "/usr", "--", "/bin/true"])).is_err()
        );
        assert!(Confine::parse(&s(&["--limits-only", "--read-stdin", "--", "/bin/true"])).is_err());
        // The token is read before the rest, even of a line that fails.
        let bad = s(&["--status-token", "f00d", "--what", "--", "/bin/true"]);
        assert!(Confine::parse(&bad).is_err());
        assert_eq!(token_of(&bad).as_deref(), Some("f00d"));
        assert_eq!(status_prefix(Some("f00d")), "lmgw-doc-reader run f00d: ");
        assert!(Confine::parse(&s(&["--status-token", "", "--", "/bin/true"])).is_err());
    }

    #[test]
    fn the_probe_line_names_the_abi() {
        assert_eq!(parse_probe(&probe_line(11)), Some(11));
        assert_eq!(parse_probe("landlock-abi 0"), None);
        assert_eq!(parse_probe("something else"), None);
    }
}
