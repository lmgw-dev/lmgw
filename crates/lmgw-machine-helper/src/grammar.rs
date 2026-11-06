//! The helper's whole argument grammar (§3.1): one verb, and for `apply` and
//! `run` one plan hash. pkexec checks no arguments (its man page says so),
//! so anything else is refused before anything is read or run.

use std::ffi::OsString;

use lmgw_machine::helper::is_plan_hash;

/// What the helper was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verb {
    /// Print root's plans for this computer; changes nothing.
    Plan,
    /// As root through pkexec: compare, then start the system task.
    Apply(String),
    /// As root inside the system task: compare, then run the steps.
    Run(String),
}

pub const USAGE: &str = "\
usage: lmgw-machine-helper plan
       lmgw-machine-helper apply <sha256>   (as root, through /usr/bin/pkexec)
       lmgw-machine-helper run <sha256>     (as root, inside lmgw-machine-setup.service)
<sha256> is 64 lowercase hex digits, the hash of the plan lmgw showed.";

/// `args` without the program's name, or why they are not the grammar.
pub fn parse(args: &[OsString]) -> Result<Verb, String> {
    let words: Vec<&str> = args
        .iter()
        .map(|a| {
            a.to_str()
                .ok_or_else(|| format!("an argument is not valid UTF-8: {a:?}"))
        })
        .collect::<Result<_, _>>()?;
    match words.as_slice() {
        ["plan"] => Ok(Verb::Plan),
        [verb @ ("apply" | "run"), hash] => {
            if !is_plan_hash(hash) {
                return Err(format!(
                    "'{hash}' is not a plan hash (64 lowercase hex digits)"
                ));
            }
            Ok(match *verb {
                "apply" => Verb::Apply(hash.to_string()),
                _ => Verb::Run(hash.to_string()),
            })
        }
        ["apply" | "run"] => Err(format!("'{}' needs the plan's hash", words[0])),
        ["plan" | "apply" | "run", ..] => Err(format!(
            "'{}' takes {} argument(s), not {}",
            words[0],
            if words[0] == "plan" { 0 } else { 1 },
            words.len() - 1
        )),
        [] => Err("no verb given".into()),
        [other, ..] => Err(format!("unknown verb '{other}'")),
    }
}
