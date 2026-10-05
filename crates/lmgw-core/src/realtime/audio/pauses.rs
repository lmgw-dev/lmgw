//! The longest pause in synthesized speech (realtime §8.2, TTS batches
//! 2026-10-05).
//!
//! A TTS pads every answer with silence of its own: Supertonic measured
//! 0.3–0.5 s before the first word and as much after the last. Each request
//! is spoken on its own, so every join between two requests was a hole of
//! 0.7–1.0 s, where the same engine pauses 0.25–0.4 s between two sentences
//! of one request — and audio.cpp, which splits a long input into chunks of
//! its own (300 characters for Supertonic) and joins their audio as it is,
//! leaves the same hole inside one answer.
//!
//! [`limit_pauses`] cuts every silence longer than the setting
//! (`longest_pause_ms`) down to it: the silence before the first sound and
//! after the last keep half of it each, so two requests join with the
//! longest pause at most, and a silence inside keeps half at either end.
//! Pauses the engine made as long or shorter are untouched.
//!
//! **Silence is judged against the clip's own level**: a 10 ms window whose
//! loudest sample stays under 1 % (-40 dB) of the clip's level is silent —
//! its level being the peak its loud windows reach, the 95th percentile of
//! the windows' peaks, so one click or pop louder than the voice does not
//! make the voice around it silence (review 2026-10-05). An engine that
//! pads with digital zero and one with a faint floor are judged alike, and a
//! quiet voice is not taken for silence because it is quiet. A clip with no
//! sound at all is left as it is: there is nothing to judge it against.
//!
//! **What it does not do**: a floor of noise or a DC offset above -40 dB is
//! no silence, so such an engine's padding stays as it is. And a pause the
//! engine made on purpose — a `[pause]` tag, a dramatic gap — is a silence
//! like any other: longer than the setting, it is shortened to it. Below
//! ~150 ms the setting reaches the gaps inside words (stop closures) and
//! makes speech choppy; that is the owner's call to make, so it is not
//! refused.

/// A silent window stays below this share of the clip's level (-40 dB).
const SILENT_SHARE: i32 = 100;
/// The clip's level: this percentile of its windows' peaks (module doc).
const LEVEL_PERCENTILE: usize = 95;
/// The window silence is judged in, in milliseconds.
const WINDOW_MS: u32 = 10;
/// The crossfade where a long silence is cut out of the middle of a clip,
/// in milliseconds: both sides are silent, so this only takes the step off.
const SPLICE_MS: u32 = 5;

/// `pcm` at `rate` with no silence longer than `longest_ms` (module doc).
/// `longest_ms` 0 keeps the engine's silences.
pub fn limit_pauses(pcm: Vec<i16>, rate: u32, longest_ms: u32) -> Vec<i16> {
    let window = (rate * WINDOW_MS / 1000).max(1) as usize;
    let longest = (u64::from(rate) * u64::from(longest_ms) / 1000) as usize;
    if longest_ms == 0 || pcm.len() <= longest {
        return pcm;
    }
    let peaks: Vec<i32> = pcm
        .chunks(window)
        .map(|w| w.iter().map(|&s| i32::from(s).abs()).max().unwrap_or(0))
        .collect();
    let mut sorted = peaks.clone();
    sorted.sort_unstable();
    if sorted.last().is_none_or(|&p| p == 0) {
        return pcm;
    }
    let level = sorted[(sorted.len() - 1) * LEVEL_PERCENTILE / 100];
    let threshold = (level / SILENT_SHARE).max(1);
    let loud: Vec<bool> = peaks.iter().map(|&p| p >= threshold).collect();
    let (Some(first), Some(last)) = (loud.iter().position(|&l| l), loud.iter().rposition(|&l| l))
    else {
        return pcm;
    };
    let head = longest / 2;
    let tail = longest - head;
    let start = (first * window).saturating_sub(head);
    let end = ((last + 1) * window + tail).min(pcm.len());
    // Never longer than the silence kept on either side of a cut.
    let splice = ((rate * SPLICE_MS / 1000) as usize).min(head).min(tail);
    let mut out = Vec::with_capacity(end - start);
    let mut from = start;
    // The silent runs between the first sound and the last, in windows.
    let mut k = first;
    while k <= last {
        if loud[k] {
            k += 1;
            continue;
        }
        let run = k;
        while !loud[k] {
            k += 1;
        }
        let (quiet_from, quiet_to) = (run * window, k * window);
        if quiet_to - quiet_from > longest {
            let keep_to = quiet_from + head;
            let resume = quiet_to - tail;
            out.extend_from_slice(&pcm[from..keep_to]);
            join(&mut out, &pcm[resume..], splice);
            from = resume + splice.min(pcm.len() - resume);
        }
    }
    out.extend_from_slice(&pcm[from..end]);
    out
}

/// A linear crossfade of `out`'s last `n` samples into `next`'s first `n`
/// (fewer when either is shorter), written over `out`'s.
fn join(out: &mut [i16], next: &[i16], n: usize) {
    let n = n.min(out.len()).min(next.len());
    let at = out.len() - n;
    for i in 0..n {
        let t = (i + 1) as f32 / (n + 1) as f32;
        let mixed = f32::from(out[at + i]) * (1.0 - t) + f32::from(next[i]) * t;
        out[at + i] = mixed.round() as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::limit_pauses;

    const RATE: u32 = 24_000;

    /// `ms` of silence.
    fn quiet(ms: u32) -> Vec<i16> {
        vec![0; (RATE * ms / 1000) as usize]
    }

    /// `ms` of a loud square wave.
    fn sound(ms: u32) -> Vec<i16> {
        (0..(RATE * ms / 1000) as usize)
            .map(|i| if i % 48 < 24 { 12_000 } else { -12_000 })
            .collect()
    }

    fn clip(parts: &[Vec<i16>]) -> Vec<i16> {
        parts.concat()
    }

    fn ms(samples: usize) -> usize {
        samples * 1000 / RATE as usize
    }

    #[test]
    fn edges_keep_half_the_longest_pause() {
        let pcm = clip(&[quiet(450), sound(1000), quiet(520)]);
        let out = limit_pauses(pcm, RATE, 400);
        // 200 ms before the sound, 1 s of it, 200 ms after.
        assert_eq!(ms(out.len()), 1400);
        assert_eq!(out[..(RATE / 5) as usize].iter().max(), Some(&0));
        assert_ne!(out[(RATE / 5) as usize], 0, "the sound starts at 200 ms");
    }

    #[test]
    fn a_long_pause_inside_is_shortened_and_a_short_one_kept() {
        let pcm = clip(&[sound(500), quiet(350), sound(500), quiet(900), sound(500)]);
        let out = limit_pauses(pcm, RATE, 400);
        // 350 ms stays; 900 ms becomes 400, less the 5 ms splice.
        assert_eq!(ms(out.len()), 500 + 350 + 500 + 400 - 5 + 500);
    }

    #[test]
    fn short_silences_and_off_change_nothing() {
        let pcm = clip(&[quiet(100), sound(300), quiet(150)]);
        assert_eq!(limit_pauses(pcm.clone(), RATE, 400), pcm);
        let long = clip(&[quiet(800), sound(300), quiet(800)]);
        assert_eq!(limit_pauses(long.clone(), RATE, 0), long, "0 keeps them");
        let silent = quiet(2000);
        assert_eq!(limit_pauses(silent.clone(), RATE, 400), silent);
    }

    #[test]
    fn silence_is_judged_against_the_clip_s_peak() {
        // A faint floor (-50 dB of the peak) around the sound is silence; a
        // quiet voice on its own is not.
        let floor: Vec<i16> = quiet(600).iter().map(|_| 30).collect();
        let pcm = clip(&[floor.clone(), sound(500), floor]);
        assert_eq!(ms(limit_pauses(pcm, RATE, 400).len()), 900);
        let soft: Vec<i16> = sound(800).iter().map(|s| s / 100).collect();
        assert_eq!(limit_pauses(soft.clone(), RATE, 400), soft);
    }

    #[test]
    fn a_click_does_not_make_a_quiet_voice_silence() {
        // A full-scale click before 600 ms of a voice at -40 dB of it: the
        // voice is the clip's level, and nothing of it is cut.
        let mut click = quiet(10);
        click[5] = i16::MIN;
        let soft: Vec<i16> = sound(600).iter().map(|s| s / 60).collect();
        let pcm = clip(&[click, quiet(100), soft]);
        assert_eq!(limit_pauses(pcm.clone(), RATE, 400), pcm);
    }
}
