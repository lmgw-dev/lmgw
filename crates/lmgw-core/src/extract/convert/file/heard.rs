//! Audio and video, heard by the speech-to-text model (design §4.2, WP7):
//! an audio file goes up as it is, a video's sound is taken out by ffmpeg
//! first (`media/ffmpeg.rs`), and the transcript is one part of
//! paragraphs marked `asr`. Its call is in the file's `stats` as the page
//! model's are (`stats.model_calls`, `stats.models`: who answered, and for
//! whom when a fallback did); a failed call that reached a model is kept
//! there too.

use std::sync::Arc;

use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertWarning, ErrorCode, PartsBySource, WarningCode};
use lmgw_api_types::documents::model_help::{BoundFrom, ModelCalls};

use super::super::budget::OutputBudget;
use super::super::caps::{Format, ToolId};
use super::super::doc::{Block, BlockKind, Doc, Part, PartKind, Source};
use super::super::media::{self, asr, ffmpeg};
use super::super::options::Plan;
use super::via::{failed_by, path};
use super::{fail, warning, Body, Failure, Read};
use crate::extract::convert::tools::ToolFacts;
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::sniff::Sniffed;

/// An audio file, heard by the speech-to-text model as it is.
pub(super) async fn audio(bytes: Bytes, sniffed: &Sniffed, plan: &Plan) -> Result<Read, Failure> {
    // A MIDI file holds notes, not sound: no model is paid to hear it.
    if sniffed.mime == "audio/midi" {
        return Err(fail(
            Some(Format::Audio),
            ErrorCode::Unsupported,
            "a MIDI file holds notes for an instrument, not recorded sound, so there is no \
             speech to transcribe",
        ));
    }
    let speech = speech(plan, Format::Audio)?;
    let heard = asr::transcribe(speech, bytes, sniffed.sub, sniffed.mime)
        .await
        .map_err(|u| unheard(Format::Audio, &speech.alias, u, None))?;
    let mut r = transcript(Format::Audio, heard, &speech.alias, None);
    r.warnings.extend(no_pictures(plan, Format::Audio));
    Ok(r)
}

/// A video's sound, taken out by ffmpeg and heard by the model.
pub(super) async fn video(
    bytes: Bytes,
    sniffed: &Sniffed,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
) -> Result<Read, Failure> {
    let tool = ToolId::Ffmpeg;
    let speech = speech(plan, Format::Video)?;
    let Some(bin) = path(facts, tool) else {
        return Err(failed_by(
            Format::Video,
            tool,
            ErrorCode::ToolMissing,
            "ffmpeg is not installed".to_string(),
        ));
    };
    let demuxer = ffmpeg::demuxer(sniffed.mime);
    let helper = super::super::confine::helper_for(facts, ToolId::Ffmpeg, &bin);
    let root = super::super::libreoffice::root(plan.spool_dir.as_deref());
    let bound = MAX_UNCOMPRESSED_BYTES.min(budget.left());
    let wav = ffmpeg::sound(
        &bin,
        helper.as_deref(),
        &root,
        bytes,
        demuxer,
        ffmpeg::bounds(),
        bound,
    )
    .await
    .map_err(|e| {
        let code = match e {
            ffmpeg::FfmpegError::Missing | ffmpeg::FfmpegError::NoDecoder(_) => {
                ErrorCode::ToolMissing
            }
            ffmpeg::FfmpegError::TooLong(_) => ErrorCode::OutputTooLarge,
            ffmpeg::FfmpegError::NotStarted(_) => ErrorCode::ToolNotStarted,
            _ => ErrorCode::Unreadable,
        };
        failed_by(Format::Video, tool, code, e.said())
    })?;
    // The WAV counts against the output budget while it exists.
    let held = wav.len() as u64;
    budget
        .take_u64(held)
        .map_err(|(code, m)| failed_by(Format::Video, tool, code, m))?;
    let heard = asr::transcribe(speech, wav, "wav", "audio/wav").await;
    budget.give_back(held);
    let heard = heard.map_err(|u| unheard(Format::Video, &speech.alias, u, Some(tool)))?;
    let mut r = transcript(Format::Video, heard, &speech.alias, Some(tool));
    r.warnings.extend(no_pictures(plan, Format::Video));
    Ok(r)
}

/// The note for `describe_pictures` on a file that is heard: an audio file
/// has no pictures; a video's frames are not described yet (its sound
/// alone is read).
fn no_pictures(plan: &Plan, format: Format) -> Option<ConvertWarning> {
    plan.describe.as_ref()?;
    let (code, message) = if format == Format::Video {
        (
            WarningCode::NotYet,
            "a video's frames are not described yet: only its sound is transcribed, so no \
             picture of it was sent to the picture-description model",
        )
    } else {
        (
            WarningCode::OptionNotApplicable,
            "describe_pictures: an audio file has no pictures, so nothing of it was sent to \
             the picture-description model",
        )
    };
    let mut w = warning(code, message.to_string(), Vec::new());
    w.option = Some("describe_pictures".to_string());
    Some(w)
}

/// The plan's speech-to-text model, or the file's `model_not_chosen`.
fn speech(plan: &Plan, format: Format) -> Result<&media::Speech, Failure> {
    match &plan.speech {
        Some(s) if !s.alias.is_empty() => Ok(s),
        _ => {
            let (code, m) = media::not_chosen();
            Err(fail(Some(format), code, m))
        }
    }
}

/// One call's entry in `stats.models`: `transcribed` is 1 when the
/// transcript is its answer.
fn calls(answered: &str, chosen: &str, transcribed: u32) -> ModelCalls {
    ModelCalls {
        alias: answered.to_string(),
        answered_for: (answered != chosen).then(|| chosen.to_string()),
        calls: 1,
        parts_read: 0,
        pictures_described: 0,
        files_transcribed: transcribed,
        in_flight_bound: 1,
        bound_from: BoundFrom::OneCall,
    }
}

/// How a file that was not heard fails: its code and message, and the call
/// when one reached a model.
fn unheard(format: Format, chosen: &str, u: asr::Unheard, tool: Option<ToolId>) -> Failure {
    let mut f = fail(Some(format), u.code, u.message);
    f.tool = tool;
    if let Some(answered) = u.called {
        f.model = (1, vec![calls(&answered, chosen, 0)]);
    }
    f
}

/// A transcript as one part of paragraphs, the model's own line breaks.
fn transcript(format: Format, heard: asr::Heard, alias: &str, tool: Option<ToolId>) -> Read {
    let blocks: Vec<Block> = heard
        .text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Block::new(BlockKind::Paragraph, l.to_string(), None, Source::Asr))
        .collect();
    let mut warnings = Vec::new();
    if blocks.is_empty() {
        warnings.push(warning(
            WarningCode::PartFailed,
            format!(
                "the speech-to-text model {} heard no speech in this file",
                heard.answered
            ),
            vec![1],
        ));
    }
    if let Some(fb) = &heard.fallback {
        warnings.push(warning(
            WarningCode::ModelHeld,
            format!(
                "the speech-to-text model {alias} did not answer; its fallback {fb} transcribed \
                 the file, as configured"
            ),
            Vec::new(),
        ));
    }
    let read = u32::from(!blocks.is_empty());
    let model = super::super::model_help::pdf::Stage {
        models: vec![calls(&heard.answered, alias, read)],
        calls: 1,
        ..Default::default()
    };
    Read {
        format,
        parts: 1,
        by_source: PartsBySource {
            asr: 1,
            ..Default::default()
        },
        body: Body::Doc(Doc {
            parts: vec![Part {
                kind: PartKind::Document,
                number: 1,
                name: None,
                size: None,
                source: Some(Source::Asr),
                blocks,
            }],
        }),
        warnings,
        partial: false,
        tool,
        also: Vec::new(),
        part_headings: 0,
        charged: 0,
        model,
    }
}
