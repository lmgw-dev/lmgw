//! A failed step's plain reason (§3.5, D128, D141), from the step's own
//! lines as dnf5, rpm and librepo word them.

use super::*;

/// What dnf5 says of a package (libdnf5 5.4.6's wording).
const OPENPGP: &str =
    "OpenPGP check for package \"nvidia-container-toolkit-base-1.20.1-1.x86_64\" \
                       (/var/cache/libdnf5/lmgw-nvidia-container-toolkit-8d1f/packages/\
                       nvidia-container-toolkit-base-1.20.1-1.x86_64.rpm) from repo \
                       \"lmgw-nvidia-container-toolkit\" has failed: ";

/// Exit 3: the failed step and its plain reason, the steps before it done.
/// A signature that does not verify is told from a key lmgw lacks (D141),
/// whatever generic words come with either.
#[tokio::test]
async fn a_failed_step_says_why_in_plain_words() {
    const KEY: &str = "NVIDIA's packages are signed with a key lmgw doesn't know yet. An lmgw \
                       update will fix this.";
    const BAD: &str = "The download didn't match NVIDIA's signature, so nothing was installed. \
                       Try again later, and if it keeps happening, tell lmgw's developer.";
    let missing = format!("{OPENPGP}Public key is not installed.");
    let opening = format!("{OPENPGP}Problem occurred when opening the package.");
    let unsigned = format!("{OPENPGP}The package is not signed.");
    let untrusted = format!("{OPENPGP}The signature is valid, but the key is not trusted.");
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["Curl error (6): Couldn't resolve host name for https://nvidia.github.io/…"],
            "NVIDIA's download server didn't answer. Try again later.",
        ),
        (
            vec!["Failed to obtain rpm transaction lock. Another transaction is in progress."],
            "Another program is installing updates right now.",
        ),
        (
            vec!["something no pattern names"],
            "\"Install NVIDIA's tool that lets models use the graphics card\" didn't work: it \
             ended with exit code 1.",
        ),
        // A key lmgw doesn't know: the key missing, the key file's key not
        // the one that signed, rpm's NOKEY, a key not trusted.
        (
            vec![
                &missing,
                "Import of the key didn't help, wrong key?",
                "Signature verification failed.",
            ],
            KEY,
        ),
        (
            vec![
                "package nvidia-container-toolkit-base-1.20.1-1.x86_64 does not verify: Header \
                 V4 RSA/SHA256 Signature, key ID 1a2b3c4d: NOKEY",
            ],
            KEY,
        ),
        (vec![&untrusted, "Signature verification failed."], KEY),
        // A download that is not what NVIDIA signed: no update fixes it.
        (vec![&opening, "Signature verification failed."], BAD),
        (vec![&unsigned, "Signature verification failed."], BAD),
        (
            vec![
                "package nvidia-container-toolkit-base-1.20.1-1.x86_64 does not verify: Payload \
                 SHA256 digest: BAD (Expected 4a5e…91 != 9f03…c2)",
            ],
            BAD,
        ),
        (
            vec![
                "package nvidia-container-toolkit-base-1.20.1-1.x86_64 does not verify: Header \
                 V4 RSA/SHA256 Signature, key ID f796ecb0: BAD",
            ],
            BAD,
        ),
        (
            vec![
                "Librepo error: Downloading successful, but checksum doesn't match. Calculated: \
                 1f2e…(sha256) Expected: 2e3f…(sha256)",
            ],
            BAD,
        ),
        (
            vec!["Librepo error: repomd.xml GPG signature verification error: Bad GPG signature"],
            BAD,
        ),
        // Both in one step's output: the one no update fixes.
        (
            vec![
                &missing,
                "package nvidia-container-toolkit-base-1.20.1-1.x86_64 does not verify: Payload \
                 SHA256 digest: BAD (Expected 4a5e…91 != 9f03…c2)",
            ],
            BAD,
        ),
        // dnf5's closing words alone name neither: the step's own words.
        (
            vec!["Signature verification failed."],
            "\"Install NVIDIA's tool that lets models use the graphics card\" didn't work",
        ),
    ];
    for (dnf_says, reason) in cases {
        let w = Fake::new().running(&applied_lines(run_lines(1, &dnf_says)), exit(3));
        let (r, _) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::StepFailed, "{r:#?}");
        assert_eq!(r.step, Some(PlanStepKind::S4Install));
        assert!(r.sentence.contains(reason), "{reason}: {}", r.sentence);
        assert!(r.sentence.contains("the steps before it stay done"));
        assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);
        assert!(r.output.contains(&dnf_says[0].to_string()));
    }
}
