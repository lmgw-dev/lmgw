//! `/usr/libexec/lmgw/lmgw-machine-helper` (machine-check design §3.1): the
//! real process around [`lmgw_machine_helper::main`]. It reads who it is,
//! reads the machine as root does (`SystemFacts::for_root`: no inherited
//! environment, D60) and acts through the real host; everything it decides
//! is in the library, where the tests reach it with fakes.

use std::ffi::OsString;
use std::io::Write;

use lmgw_machine::helper::{Event, Exit};
use lmgw_machine::SystemFacts;
use lmgw_machine_helper::stdout::Queued;
use lmgw_machine_helper::{real, Caller};

fn main() {
    // Before anything else: pkexec runs the helper in root's home, or with
    // `--keep-cwd` in the directory of whoever asked, and nothing the helper
    // does may resolve a relative path there.
    let in_root = std::env::set_current_dir("/");
    // The modes of what it writes are set exactly; this keeps whatever umask
    // pkexec or systemd handed down from reaching a program it starts.
    // SAFETY: umask has no preconditions and cannot fail.
    unsafe {
        libc::umask(0o022);
    }
    // Lines go out through a queue of their own, so a reader that stops
    // reading never stops a step (D100).
    let mut out = Queued::stdout();
    let mut err = std::io::stderr().lock();
    let exit = match in_root {
        Ok(()) => {
            let args: Vec<OsString> = std::env::args_os().skip(1).collect();
            let caller = Caller::of_this_process();
            let facts = SystemFacts::for_root(Some(real::stdio_only));
            let host = real::RealHost;
            lmgw_machine_helper::main(&args, &caller, &facts, &host, &mut out, &mut err)
        }
        Err(e) => {
            let why = format!("the helper could not change to the directory /: {e}");
            let _ = writeln!(err, "lmgw-machine-helper: {why}");
            let _ = out.write_all(format!("{}\n", Event::Refused { why }.to_line()).as_bytes());
            Exit::Refused
        }
    };
    let _ = err.flush();
    // What the reader takes now goes out; the journal has the rest.
    out.settle();
    std::process::exit(exit.code());
}
