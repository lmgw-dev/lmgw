//! A video's sound as 16 kHz mono WAV (design §5.1), the form lmgw's
//! speech-to-text calls take (the realtime turns send the same).
//!
//! `ffmpeg -nostdin -hide_banner -loglevel error -protocol_whitelist file
//! -f mov|matroska -i file:<run>/in.media -map 0:a:0 -vn -sn -dn -ac 1 -ar
//! 16000 -c:a pcm_s16le -bitexact -fs <bound> -y -f wav <run>/out.wav`:
//! - each run has a **private directory** (mode 0700, `ff-<pid>-…`) under
//!   the data dir's `convert-spool`: the video as `in.media` and the sound
//!   as `out.wav` (both 0600), removed on every path once ffmpeg has been
//!   reaped (`run::Tool::keep`). An MP4 whose index (`moov`) is at its end
//!   cannot be read from a pipe; a file can;
//! - the WAV stays on disk: the gateway maps a copy of it that no tool can
//!   reach (`mapped`: a clone of its blocks where the filesystem shares
//!   them), so its pages are the page cache's, which the kernel can drop
//!   and read again, and ffmpeg's file is gone once the copy is made and
//!   the directory is removed. It is never copied into the gateway's
//!   memory;
//! - **confined** where lmgw's reader can confine it (`confine.rs`): its
//!   own directory, `/usr` and the loader cache, no network; elsewhere as
//!   before;
//! - `-protocol_whitelist file`: the input may open files only, so a
//!   playlist or reference movie inside the container cannot make ffmpeg
//!   fetch anything from the network (or open another protocol);
//! - the first sound stream only (`-map 0:a:0`), no video, subtitle,
//!   data or attachment stream is decoded;
//! - the demuxer is the one lmgw's sniffer found the container to be
//!   (`-f mov` for MP4/QuickTime, `-f matroska`), never one ffmpeg guesses
//!   from a name or probes for: a playlist, a concat list or an image
//!   sequence posing as a video is read as the container it claimed to be,
//!   and fails as one;
//! - the WAV is bounded by the smaller of `MAX_UNCOMPRESSED_BYTES` (at
//!   32,000 bytes a second about 9 h 19 min of sound, [`MAX_SECONDS`]) and
//!   what is left of the request's output budget: `-fs` stops ffmpeg there,
//!   and for a confined run `RLIMIT_FSIZE` a little above it is the hard
//!   stop; a longer video fails by name. The WAV counts against the output
//!   budget while it exists;
//! - [`FFMPEG_TIMEOUT`], [`FFMPEG_MEMORY_LIMIT_BYTES`], its own process
//!   group, a slot of the tool pool (`run.rs`).
//!
//! Fedora's `ffmpeg-free` lacks some decoders (D13): the probe lists the
//! audio decoders that matter, and a video whose sound has no decoder here
//! fails naming the decoder ffmpeg asked for.

mod mapped;
#[cfg(all(test, target_os = "linux"))]
mod tests_escaped;

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;

use super::super::pool;
use super::super::run::{self, Bounds, Input, Keep, RunError, Tool};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;

/// How long one ffmpeg run may take: five minutes, as poppler's
/// (`PDF_TIMEOUT`). Only the sound is decoded (the video stream is not):
/// an hour of a film's sound is resampled in seconds; a run still going
/// after five minutes is a file that makes the demuxer loop.
pub const FFMPEG_TIMEOUT: Duration = Duration::from_secs(300);

/// ffmpeg's address space: 2 GiB. Decoding one sound stream needs tens of
/// MiB (measured 2026-11-19, ffmpeg 9.0.2: under 100 MiB resident for an
/// MP4's AAC); the rest is room for a demuxer's index of a long file.
pub const FFMPEG_MEMORY_LIMIT_BYTES: u64 = 2 << 30;

/// The WAV's bytes a second: 16,000 samples of 2 bytes, one channel.
pub const WAV_BYTES_PER_SECOND: u64 = 32_000;

/// The longest sound a conversion takes, in seconds: what fits in
/// `MAX_UNCOMPRESSED_BYTES` at [`WAV_BYTES_PER_SECOND`].
pub const MAX_SECONDS: u64 = MAX_UNCOMPRESSED_BYTES / WAV_BYTES_PER_SECOND;

/// How a run failed, before it is worded for the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfmpegError {
    Missing,
    /// The file has no sound stream.
    NoSound,
    /// A decoder ffmpeg has not got here (ffmpeg-free, D13): its name.
    NoDecoder(String),
    Timeout,
    /// The sound passed its bound (the bytes): `MAX_UNCOMPRESSED_BYTES`, or
    /// what was left of the request's output budget.
    TooLong(u64),
    /// No private directory could be made, or the WAV not read back: why.
    Scratch(String),
    OutOfMemory,
    /// It could not read the file: its first stderr lines.
    Failed(String),
    /// lmgw's reader could not confine it or set its limits
    /// (`RunError::NotStarted`): the message.
    NotStarted(String),
}

impl FfmpegError {
    pub fn said(&self) -> String {
        match self {
            Self::Missing => "ffmpeg was not found when it was run".to_string(),
            Self::NotStarted(m) => m.clone(),
            Self::NoSound => "this video has no sound to transcribe".to_string(),
            Self::NoDecoder(d) => format!(
                "the ffmpeg here cannot decode this video's sound ({d}): Fedora's ffmpeg-free \
                 lacks some decoders; the ffmpeg package from RPM Fusion has them (Settings › \
                 Documents lists the decoders found)"
            ),
            Self::Timeout => format!(
                "ffmpeg did not finish within FFMPEG_TIMEOUT ({} s) and was stopped",
                FFMPEG_TIMEOUT.as_secs()
            ),
            Self::TooLong(bound) => format!(
                "the video's sound is longer than {} ({}, the smaller of \
                 MAX_UNCOMPRESSED_BYTES, {}, and what is left of this request's output budget, \
                 of 16 kHz mono sound): cut it into parts and send those",
                hms(bound / WAV_BYTES_PER_SECOND),
                crate::hf::fmt_bytes(*bound),
                crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
            ),
            Self::Scratch(e) => format!(
                "no private directory for ffmpeg could be made under the data dir, or its \
                 sound not read back ({e})"
            ),
            Self::OutOfMemory => format!(
                "ffmpeg ran out of FFMPEG_MEMORY_LIMIT_BYTES ({}) and was stopped",
                crate::hf::fmt_bytes(FFMPEG_MEMORY_LIMIT_BYTES)
            ),
            Self::Failed(m) => format!("ffmpeg could not read this video: {m}"),
        }
    }
}

/// `s` seconds as `9 h 19 min`.
fn hms(s: u64) -> String {
    format!("{} h {} min", s / 3600, s % 3600 / 60)
}

/// The demuxer for a container lmgw's sniffer named by its MIME type.
pub fn demuxer(mime: &str) -> &'static str {
    if mime == "video/x-matroska" {
        "matroska"
    } else {
        "mov"
    }
}

/// The arguments: the input read from `input` by `demuxer`, the WAV
/// written to `output`, no larger than `bound` bytes.
fn args(input: &str, demuxer: &str, output: &str, bound: u64) -> Vec<String> {
    let bound = bound.to_string();
    [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file",
        "-f",
        demuxer,
        "-i",
        input,
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        "-bitexact",
        "-fs",
        bound.as_str(),
        "-y",
        "-f",
        "wav",
        output,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// How far past the `-fs` bound `RLIMIT_FSIZE` stops a confined ffmpeg:
/// `-fs` ends the file at the chunk that passes it, and the WAV's trailer
/// is written after that.
const FSIZE_SLACK: u64 = 1 << 20;

/// A run's private directory (module doc).
struct Scratch(tempfile::TempDir);

/// The directory a run makes under `root`, with the video in it.
fn prepare(root: &Path, video: &[u8]) -> std::io::Result<Scratch> {
    super::super::libreoffice::scratch::make_private(root)?;
    super::super::libreoffice::scratch::sweep_once(root);
    let dir = tempfile::Builder::new()
        .prefix(&format!("ff-{}-", std::process::id()))
        .tempdir_in(root)?;
    let private = |p: &Path| -> std::io::Result<std::fs::File> {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(p)
    };
    std::io::Write::write_all(&mut private(&dir.path().join("in.media"))?, video)?;
    private(&dir.path().join(OUTPUT))?;
    Ok(Scratch(dir))
}

/// The WAV's name in the run's directory.
const OUTPUT: &str = "out.wav";

/// The sound of `video` (a container `demuxer` reads) as WAV, by the
/// ffmpeg at `bin`, in a private directory under `root`, within `bounds`
/// ([`bounds`] in a conversion) and `bound` bytes of WAV; confined by
/// `helper` when it is given. The WAV's bytes are the file's, mapped
/// (module doc).
pub async fn sound(
    bin: &Path,
    helper: Option<&Path>,
    root: &Path,
    video: Bytes,
    demuxer: &str,
    bounds: Bounds,
    bound: u64,
) -> Result<Bytes, FfmpegError> {
    let at = root.to_path_buf();
    let made = tokio::task::spawn_blocking(move || prepare(&at, &video))
        .await
        .map_err(|e| FfmpegError::Scratch(e.to_string()))?
        .map_err(|e| FfmpegError::Scratch(e.to_string()))?;
    let dir = made.0.path().to_path_buf();
    let keep: Keep = std::sync::Arc::new(made);
    let input = format!("file:{}", dir.join("in.media").display());
    let output = dir.join(OUTPUT);
    let argv = args(&input, demuxer, &output.display().to_string(), bound);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let rules = super::super::confine::filter(Some(&dir), bound.saturating_add(FSIZE_SLACK));
    let tool = Tool {
        dir: Some(&dir),
        keep: Some(&keep),
        confine: helper.map(|helper| run::Confined {
            helper,
            rules: &rules,
        }),
        ..Tool::new(bin, &argv)
    };
    let slot = pool::global().acquire().await;
    let out = run::run_held(&slot, tool, Input::Bytes(Bytes::new()), bounds).await;
    drop(slot);
    match out {
        Ok(_) => {}
        Err(RunError::Missing) => return Err(FfmpegError::Missing),
        Err(RunError::NoReader) => {
            return Err(FfmpegError::Failed(super::super::run::start::no_reader()))
        }
        Err(RunError::Timeout) => return Err(FfmpegError::Timeout),
        Err(RunError::TooLarge) => {
            return Err(FfmpegError::Failed(
                "it printed more than it may on stdout".to_string(),
            ))
        }
        Err(RunError::FileTooLarge(_)) => return Err(FfmpegError::TooLong(bound)),
        Err(RunError::OutOfMemory) => return Err(FfmpegError::OutOfMemory),
        Err(RunError::NotStarted(m)) => return Err(FfmpegError::NotStarted(m)),
        Err(RunError::Failed(m)) => return Err(failed(&m)),
    }
    let (at, under) = (dir.clone(), root.to_path_buf());
    // One byte past the bound is enough to tell a WAV that passed it.
    let limit = bound.saturating_add(1);
    let wav = tokio::task::spawn_blocking(move || mapped::map(&at, OUTPUT, &under, limit))
        .await
        .map_err(|e| FfmpegError::Scratch(e.to_string()))?
        .map_err(|e| FfmpegError::Scratch(e.to_string()))?;
    // The mapping holds the file's pages; its name and directory go now.
    drop(keep);
    if wav.len() as u64 >= bound {
        return Err(FfmpegError::TooLong(bound));
    }
    if !is_wav(&wav) {
        return Err(FfmpegError::Failed("it wrote no sound".to_string()));
    }
    Ok(wav)
}

/// The bounds of a real run.
pub fn bounds() -> Bounds {
    Bounds {
        timeout: FFMPEG_TIMEOUT,
        // Its stdout carries nothing (the WAV is a file): error lines only.
        output: 64 << 10,
        memory: FFMPEG_MEMORY_LIMIT_BYTES,
    }
}

/// A failed run's stderr, as what it says.
fn failed(m: &str) -> FfmpegError {
    let lower = m.to_ascii_lowercase();
    if lower.contains("matches no streams") || lower.contains("does not contain any stream") {
        return FfmpegError::NoSound;
    }
    if let Some(at) = lower.find("decoder (codec ") {
        let rest = &m[at + "decoder (codec ".len()..];
        let name: String = rest.chars().take_while(|c| *c != ')').collect();
        return FfmpegError::NoDecoder(name);
    }
    if lower.contains("decoding requested, but no decoder found") {
        return FfmpegError::NoDecoder("unknown".to_string());
    }
    FfmpegError::Failed(m.to_string())
}

/// Whether `wav` starts as a WAV does.
fn is_wav(wav: &[u8]) -> bool {
    wav.len() >= 12 && &wav[0..4] == b"RIFF" && &wav[8..12] == b"WAVE"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_wav_is_sound() {
        assert!(is_wav(b"RIFF\0\0\0\0WAVEfmt "));
        assert!(!is_wav(b"not a wav at all"));
    }

    #[test]
    fn what_ffmpeg_says_is_named() {
        assert_eq!(
            failed("Stream map '0:a:0' matches no streams.\n(exit status: 1)"),
            FfmpegError::NoSound
        );
        assert_eq!(
            failed("[aist#0:1/aac] Decoder (codec aac) not found for input stream #0:1"),
            FfmpegError::NoDecoder("aac".to_string())
        );
        assert!(matches!(
            failed("moov atom not found"),
            FfmpegError::Failed(_)
        ));
        assert_eq!(MAX_SECONDS, 33_554);
        let said = FfmpegError::TooLong(MAX_UNCOMPRESSED_BYTES).said();
        assert!(
            said.contains("9 h 19 min") && said.contains("output budget"),
            "{said}"
        );
    }
}
