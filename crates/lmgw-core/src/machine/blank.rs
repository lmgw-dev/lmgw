//! A machine with nothing on it: what a test state's check reads unless the
//! test states one (`AppState::set_machine_for_tests`). Every file is
//! missing and every program is not there, so a test state never reads or
//! runs anything of the computer the suite runs on.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lmgw_machine::{CmdOut, Facts, Meta, Space};

pub(super) struct Blank;

fn missing(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: nothing on a blank machine", path.display()),
    )
}

impl Facts for Blank {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        Err(missing(path))
    }
    fn stat(&self, path: &Path) -> io::Result<Meta> {
        Err(missing(path))
    }
    fn lstat(&self, path: &Path) -> io::Result<Meta> {
        Err(missing(path))
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        Err(missing(path))
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Err(missing(path))
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        Err(missing(dir))
    }
    fn env(&self, _name: &str) -> Option<String> {
        None
    }
    fn uid(&self) -> Option<u32> {
        Some(0)
    }
    fn user_name(&self) -> Option<String> {
        None
    }
    fn run(&self, program: &Path, _args: &[&str]) -> io::Result<CmdOut> {
        Err(missing(program))
    }
    fn run_limited(&self, program: &Path, _args: &[&str], _limit: Duration) -> io::Result<CmdOut> {
        Err(missing(program))
    }
    fn space(&self, path: &Path) -> io::Result<Space> {
        Err(missing(path))
    }
}
