//! A failed step in plain words (machine-check design §3.5): read from
//! what the step's program printed, its exit code and which step it was.
//! The patterns are words dnf5, librepo and rpm use for these failures,
//! read in the strings of libdnf5 5.4.6, librepo 1.21.2 and rpm 6.0.2 on
//! Fedora 44 (2026-11-03); no failing dnf5 run was watched for them yet
//! (the design's "Unverified"), so a failure none of them names gets the
//! step's own sentence and its exit, and the output is always in the
//! detail.
//!
//! A signature that does not verify is not a key lmgw lacks (WP7 review
//! L5, D141): dnf5's "OpenPGP check for package … has failed: " and its
//! closing "Signature verification failed." come with both, so only the
//! reason after them decides. A digest or signature that is BAD, a package that
//! is not signed, a download whose checksum doesn't match: the download is
//! not what NVIDIA signed, and an lmgw update would not change that.
//!
//! Upstream failures are reported, not worked around: each reason says
//! what happened and what the person can do, nothing more.

use lmgw_api_types::machine::PlanStepKind;

use super::progress::FailedStep;

/// What a pattern means for the person.
const DOWNLOAD: &str = "NVIDIA's download server didn't answer. Try again later.";
const LOCKED: &str = "Another program is installing updates right now. Try again when it's done.";
const KEY: &str =
    "NVIDIA's packages are signed with a key lmgw doesn't know yet. An lmgw update will fix this.";
const BAD: &str = "The download didn't match NVIDIA's signature, so nothing was installed. Try \
                   again later, and if it keeps happening, tell lmgw's developer.";
const STILL_NEEDED: &str =
    "Something on this computer still needs the old piece, so lmgw couldn't replace it.";

/// Each reason with the words that name it, matched case-insensitively in
/// the step's lines. The first reason one of whose words is in any line
/// wins: a download that does not verify before a key lmgw lacks (a step
/// can print both, and the first is the one an update would not fix), both
/// before a plain download failure (librepo's "Librepo error: Bad GPG
/// signature" names librepo too).
const PATTERNS: &[(&str, &[&str])] = &[
    (
        LOCKED,
        &[
            "transaction lock",
            "another transaction is in progress",
            "waiting for process with pid",
        ],
    ),
    (
        BAD,
        &[
            // rpm: "… Signature, key ID f796ecb0: BAD", "Payload SHA256
            // digest: BAD (Expected … != …)".
            ": bad",
            // libdnf5's OpenPGP check results other than a missing or
            // untrusted key.
            "the package is not signed",
            "problem occurred when opening the package",
            // librepo: the repository metadata's signature, a download's
            // checksum.
            "bad gpg signature",
            "gpg signature is not available",
            "checksum doesn't match",
            "checksum of file doesn't match",
        ],
    ),
    (
        KEY,
        &[
            // libdnf5: "… has failed: Public key is not installed.", the
            // key file's key not the one that signed ("Import of the key
            // didn't help, wrong key?"), a key rpm does not trust.
            "public key is not installed",
            "wrong key",
            "key is not trusted",
            // rpm: "… Signature, key ID f796ecb0: NOKEY", "NOTTRUSTED".
            ": nokey",
            ": nottrusted",
        ],
    ),
    (
        DOWNLOAD,
        &[
            "curl error",
            "failed to download",
            "cannot download",
            "could not resolve host",
            "couldn't resolve host",
            "timeout was reached",
            "librepo",
        ],
    ),
];

/// The words of a dependency problem: what keeps the swap (S4″) from
/// removing Fedora's retired package.
const DEPENDENCY: &[&str] = &["problem:", "requires", "conflicts with", "is needed by"];

/// The plain reason step `failed` did not finish, from `lines` (that step's
/// output, in order) and `sentence` (the step's bullet in the plan the
/// person confirmed, when the command has it).
pub(crate) fn plain(failed: &FailedStep, lines: &[&str], sentence: Option<&str>) -> String {
    let lower: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let said = |words: &[&str]| lower.iter().any(|l| words.iter().any(|w| l.contains(w)));
    for (reason, words) in PATTERNS {
        if said(words) {
            return (*reason).to_string();
        }
    }
    if failed.kind == PlanStepKind::S4Swap && said(DEPENDENCY) {
        return STILL_NEEDED.to_string();
    }
    // The bullet without its download size ("… · 8.5 MB download").
    let what = sentence
        .and_then(|s| s.split(" · ").next())
        .map_or_else(|| step_words(failed.kind).to_string(), str::to_string);
    let how = match (&failed.error, failed.exit, failed.signal) {
        (Some(_), _, _) => "lmgw couldn't carry it out".to_string(),
        (None, _, Some(sig)) => format!("it was stopped (signal {sig})"),
        (None, Some(code), None) => format!("it ended with exit code {code}"),
        (None, None, None) => "it ended without saying how".to_string(),
    };
    format!("\"{what}\" didn't work: {how}.")
}

/// A step in plain words, for a failure the command has no plan sentence
/// for (a setup it follows from before a restart).
fn step_words(kind: PlanStepKind) -> &'static str {
    match kind {
        PlanStepKind::S1AddRepo => "Add NVIDIA's software source",
        PlanStepKind::S1EnableRepo => "Switch on NVIDIA's software source",
        PlanStepKind::S2AddKey => "Add NVIDIA's signing key",
        PlanStepKind::S4Install => "Install NVIDIA's tool that lets models use the graphics card",
        PlanStepKind::S4Update => "Update NVIDIA's tool that lets models use the graphics card",
        PlanStepKind::S4Swap => "Replace the piece Fedora stopped updating",
    }
}
