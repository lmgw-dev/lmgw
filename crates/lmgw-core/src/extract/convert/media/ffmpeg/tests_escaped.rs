//! The review of security round B, S3: a process of ffmpeg's run that left
//! its process group (`setsid`) survives the group kill; when it cuts the
//! WAV it still holds open, the conversion ends with its answer, never a
//! signal (the gateway maps a copy of the WAV no tool can reach).

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use bytes::Bytes;

use super::*;

#[tokio::test]
async fn a_process_that_left_the_group_and_cuts_the_wav_ends_nothing() {
    if run::start::starter().is_none()
        || std::process::Command::new("setsid")
            .arg("--version")
            .output()
            .is_err()
    {
        eprintln!("skipped: no reader beside the tests, or no setsid");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    std::fs::create_dir_all(&root).unwrap();
    // A stand-in ffmpeg: it writes the WAV (its last argument), leaves a
    // process behind in a session of its own holding the WAV open, which
    // empties it a moment after the run ended, and exits.
    let fake = t.path().join("ffmpeg");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor a; do out=\"$a\"; done\nprintf 'RIFF....WAVE' > \"$out\"\n\
         head -c 1048576 /dev/zero >> \"$out\"\n\
         setsid sh -c 'sleep 0.3; truncate -s 0 /proc/self/fd/3' 3<>\"$out\" \
         >/dev/null 2>&1 </dev/null &\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wav = sound(
        &fake,
        None,
        &root,
        Bytes::from_static(b"example video"),
        "mov",
        bounds(),
        64 << 20,
    )
    .await
    .unwrap();
    // The process behind has emptied ffmpeg's file by now.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(wav.len(), 12 + (1 << 20));
    assert!(is_wav(&wav));
    assert!(wav[12..].iter().all(|b| *b == 0));
}
