//! `realtime_budget` (realtime design §12): what the configured voice
//! cascade is expected to hold on the GPU, stage by stage, against what lmgw
//! may use.
//!
//! A voice session keeps its chat model, its ASR model and its TTS model
//! resident together (§9), so on one card the LLM and the voice are a
//! sliding scale. Every figure here is one lmgw already charges at
//! admission — [`crate::vram::VramScheduler::footprint`]: a chat row's GGUF
//! weights + KV cache, an audio row's learned residency or, before it has
//! one, its on-disk size (§9.4). Nothing is guessed: a stage lmgw has no
//! figure for is reported unknown, and the total is then a lower bound. A
//! cloud stage holds nothing here; turn detection runs on the CPU.

use lmgw_api_types::realtime as dto;

use crate::config::{BargeInCheck, Snapshot, UpstreamKind};
use crate::hf::fmt_bytes;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::{classify, Target};

/// Where one stage's alias runs.
enum Place {
    /// A model lmgw runs on this GPU, and why it is that one (a candidate
    /// alias's primary) when that needs saying.
    Local(Target, Option<String>),
    /// A provider lmgw only forwards to.
    Cloud(String),
    /// A server lmgw forwards to but does not manage — it may be on this
    /// machine, and then its memory is outside lmgw's plan.
    External(String),
    Unresolved(String),
}

fn place(snap: &Snapshot, alias: &str) -> Place {
    if let Some(ca) = snap.candidate_alias(alias) {
        // A candidate alias picks per request (candidate-aliases §4.2); its
        // primary is what it answers with while it fits.
        return match ca.candidates.first() {
            Some(primary) => Place::Local(
                Target {
                    class: Class::Chat,
                    model_id: primary.clone(),
                },
                Some(match &ca.candidates[1..] {
                    [] => format!("candidate alias: its primary '{primary}'"),
                    rest => format!(
                        "candidate alias: its primary '{primary}' — {} may answer instead",
                        rest.join(", ")
                    ),
                }),
            ),
            None => Place::Unresolved(format!("candidate alias '{alias}' has no candidates")),
        };
    }
    match snap.resolve(alias) {
        Err(e) => Place::Unresolved(e.to_string()),
        Ok(route) => match classify(&route) {
            Some(t) => Place::Local(t, None),
            None => {
                let u = &route.upstream;
                let on_this_box =
                    matches!(u.kind, UpstreamKind::LlamaServer | UpstreamKind::AudioCpp)
                        || loopback(&u.base_url);
                if on_this_box {
                    Place::External(u.name.clone())
                } else {
                    Place::Cloud(u.name.clone())
                }
            }
        },
    }
}

/// A base URL on this machine's loopback.
fn loopback(base_url: &str) -> bool {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let host = rest.split(['/', '?']).next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// One alias-backed stage, sized. `seen` holds the local models counted so
/// far: a model two stages share is held once.
async fn stage(
    state: &SharedState,
    snap: &Snapshot,
    seen: &mut Vec<Target>,
    (id, label): (&str, &str),
    alias: Option<String>,
    unset: &str,
) -> dto::RealtimeBudgetStage {
    let mut out = dto::RealtimeBudgetStage {
        stage: id.into(),
        label: label.into(),
        alias: alias.clone(),
        ..Default::default()
    };
    let Some(alias) = alias else {
        out.placement = "unset".into();
        out.bytes = Some(0);
        out.note = unset.into();
        return out;
    };
    match place(snap, &alias) {
        Place::Unresolved(why) => {
            out.placement = "unresolved".into();
            out.note = format!("'{alias}' does not resolve: {why}");
        }
        Place::Cloud(upstream) => {
            out.placement = "cloud".into();
            out.bytes = Some(0);
            out.note = format!("served by the upstream '{upstream}' — nothing on this GPU");
        }
        Place::External(upstream) => {
            out.placement = "external".into();
            out.bytes = Some(0);
            out.note = format!(
                "served by the upstream '{upstream}', a server lmgw forwards to but does not \
                 run: whatever it holds is outside lmgw's plan, and on this GPU it is part of \
                 what other programs hold"
            );
        }
        Place::Local(target, why) => {
            out.model = Some(format!("{}/{}", target.class.as_str(), target.model_id));
            out.running = state.vram.is_up(state, &target);
            if seen.contains(&target) {
                out.placement = "shared".into();
                out.bytes = Some(0);
                out.note = "the same model as a stage above, which holds it once".into();
                return out;
            }
            seen.push(target.clone());
            // An audio row on the CPU holds nothing on this GPU.
            let on_cpu = snap
                .audio_models
                .iter()
                .find(|m| target.class == Class::Audio && m.model_id == target.model_id)
                .filter(|m| !crate::runtime::audio::placement(m, &snap.settings.audio).is_gpu());
            if let Some(m) = on_cpu {
                let host = crate::host::cpu();
                let (threads, source) =
                    crate::runtime::audio::threads_in_effect(m, &snap.settings.audio, host);
                out.placement = "cpu".into();
                out.bytes = Some(0);
                out.note = format!(
                    "runs on this machine's CPU ({threads} threads, {}): nothing on this GPU; \
                     host RAM is not measured by lmgw",
                    crate::runtime::audio::threads_source_label(source, host)
                );
                return out;
            }
            out.placement = "local".into();
            let fp = state
                .vram
                .footprint(snap, target.class, &target.model_id)
                .await;
            let mut parts: Vec<String> = why.into_iter().collect();
            match fp {
                None => parts.push(format!(
                    "no {} row describes '{}', so lmgw has no figure for it",
                    target.class.as_str(),
                    target.model_id
                )),
                Some(fp) => {
                    out.bytes = Some(fp.total_bytes);
                    parts.push(basis(state, snap, &target, &fp).await);
                }
            }
            out.note = parts.join("; ");
        }
    }
    out
}

/// What a local stage's figure is made of, in the words the models page and
/// the ledger use.
async fn basis(
    state: &SharedState,
    snap: &Snapshot,
    target: &Target,
    fp: &crate::vram::Footprint,
) -> String {
    match target.class {
        Class::Audio => match snap
            .audio_models
            .iter()
            .find(|m| m.model_id == target.model_id)
        {
            Some(m) => state.vram.audio_residency_note(state, snap, m).await,
            None => fp.note.clone().unwrap_or_default(),
        },
        Class::Chat => {
            let mut s = match fp.ctx_tokens {
                Some(ctx) => format!(
                    "GGUF weights {} + KV cache {} at {ctx} tokens — a lower bound: compute \
                     buffers and the CUDA context are the headroom's",
                    fmt_bytes(fp.weights_bytes),
                    fmt_bytes(fp.kv_cache_bytes)
                ),
                None => format!("GGUF weights {}", fmt_bytes(fp.weights_bytes)),
            };
            if let Some(n) = &fp.note {
                s.push_str(&format!(" ({n})"));
            }
            let ladder = snap
                .local_models
                .iter()
                .find(|m| m.model_id == target.model_id)
                .is_some_and(|m| !m.ladder.is_empty());
            if ladder {
                s.push_str(
                    "; a ladder model: this is its base rung, and a climb to a larger one \
                     holds more",
                );
            }
            s
        }
        Class::Aux | Class::Image => fp.note.clone().unwrap_or_default(),
    }
}

/// An argument given (`""` included) wins over the saved setting.
fn pick(arg: Option<String>, saved: &str) -> Option<String> {
    let v = arg.unwrap_or_else(|| saved.to_string());
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// `realtime_budget` (module doc).
pub async fn realtime_budget(
    state: &SharedState,
    a: dto::RealtimeBudgetArgs,
) -> Result<dto::RealtimeBudget, String> {
    let snap = state.snapshot();
    let rt = &snap.settings.realtime;
    let check = match a.barge_in_check.as_deref().map(str::trim) {
        None => rt.barge_in_check,
        Some("words") => BargeInCheck::Words,
        Some("duration") => BargeInCheck::Duration,
        Some(other) => {
            return Err(format!(
                "barge_in_check is 'words' or 'duration', not '{other}'"
            ))
        }
    };
    let chat = pick(a.default_model, &rt.default_model);
    // As a session resolves it (§5.2): the realtime setting, else the Chat's
    // transcription model.
    let own_asr = pick(a.asr_alias, &rt.asr_alias);
    let asr_from_chat = own_asr.is_none();
    let asr = own_asr.or_else(|| pick(None, &snap.settings.chat_stt_alias));
    let tts = pick(a.tts_alias, &rt.tts_alias);
    let check_alias = pick(a.barge_in_check_alias, &rt.barge_in_check_alias);

    let mut seen: Vec<Target> = Vec::new();
    let mut stages = vec![dto::RealtimeBudgetStage {
        stage: "turn".into(),
        label: "Turn detection".into(),
        placement: "cpu".into(),
        bytes: Some(0),
        note: "Silero VAD and Smart Turn run inside lmgw on the CPU (ONNX Runtime) — no GPU \
               memory"
            .into(),
        ..Default::default()
    }];
    stages.push(
        stage(
            state,
            &snap,
            &mut seen,
            ("chat", "Chat model"),
            chat,
            "realtime.default_model is not set: a session that names no chat alias has none",
        )
        .await,
    );
    let mut asr_stage = stage(
        state,
        &snap,
        &mut seen,
        ("asr", "Speech to text"),
        asr.clone(),
        "no ASR alias (realtime.asr_alias, or the Chat's transcription model): audio turns \
         fail at their commit",
    )
    .await;
    if asr_from_chat && asr.is_some() {
        asr_stage.note = format!(
            "the Chat's transcription model (realtime.asr_alias is empty); {}",
            asr_stage.note
        );
    }
    stages.push(asr_stage);
    // The word check holds a model of its own only when it uses another
    // alias than the turns (§6.4).
    match (check, &check_alias) {
        (BargeInCheck::Words, Some(alias)) if Some(alias) != asr.as_ref() => {
            stages.push(
                stage(
                    state,
                    &snap,
                    &mut seen,
                    ("check", "Barge-in word check"),
                    Some(alias.clone()),
                    "",
                )
                .await,
            );
        }
        _ => {}
    }
    stages.push(
        stage(
            state,
            &snap,
            &mut seen,
            ("tts", "Text to speech"),
            tts,
            "realtime.tts_alias is not set: audio responses fail before they start",
        )
        .await,
    );

    let total_bytes: u64 = stages.iter().filter_map(|s| s.bytes).sum();
    let unknown: Vec<String> = stages
        .iter()
        .filter(|s| s.bytes.is_none())
        .map(|s| s.stage.clone())
        .collect();
    let view = state.vram.view(state).await;
    let headroom_bytes = view.headroom_bytes;
    let needed_bytes = total_bytes.saturating_add(headroom_bytes);
    let (capacity_bytes, capacity_source) = if view.capacity_bytes > 0 {
        let vs = &snap.settings.vram;
        let source = if vs.budget_mb > 0 {
            format!("vram.budget_mb ({} MiB)", vs.budget_mb)
        } else {
            format!("the GPU's total, from {}", view.telemetry)
        };
        let source = if vs.enabled {
            source
        } else {
            format!("{source}; admission is off (vram.enabled), so nothing is refused for it")
        };
        (Some(view.capacity_bytes), source)
    } else {
        (
            None,
            view.inactive_reason
                .clone()
                .unwrap_or_else(|| "no capacity figure".into()),
        )
    };
    let outside_bytes = view.outside_share_bytes;
    let outside_note = view.external_trigger_reason.clone();

    let lower = if unknown.is_empty() { "" } else { "at least " };
    let (verdict, summary) = match capacity_bytes {
        None => (
            "unknown",
            format!(
                "the cascade needs {lower}{} with the headroom; lmgw has no capacity to compare \
                 it with: {capacity_source}",
                fmt_bytes(needed_bytes)
            ),
        ),
        Some(cap) if needed_bytes > cap => (
            "too_large",
            format!(
                "the cascade needs {lower}{} ({} + {} headroom) and lmgw may use {} — not all \
                 of it can be resident at once, so its models will evict each other",
                fmt_bytes(needed_bytes),
                fmt_bytes(total_bytes),
                fmt_bytes(headroom_bytes),
                fmt_bytes(cap)
            ),
        ),
        Some(cap) if !unknown.is_empty() => (
            "unknown",
            format!(
                "the known stages need {} ({} + {} headroom) of the {} lmgw may use; {} has no \
                 figure, so whether all of it fits is unknown",
                fmt_bytes(needed_bytes),
                fmt_bytes(total_bytes),
                fmt_bytes(headroom_bytes),
                fmt_bytes(cap),
                unknown.join(", ")
            ),
        ),
        Some(cap) => match outside_bytes {
            Some(out) if needed_bytes > cap.saturating_sub(out) => (
                "tight",
                format!(
                    "the cascade needs {} ({} + {} headroom) of the {} lmgw may use, but other \
                     programs hold {} now — beside them it does not fit, and a start waits or \
                     falls back",
                    fmt_bytes(needed_bytes),
                    fmt_bytes(total_bytes),
                    fmt_bytes(headroom_bytes),
                    fmt_bytes(cap),
                    fmt_bytes(out)
                ),
            ),
            _ => (
                "fits",
                format!(
                    "the cascade needs {} ({} + {} headroom) of the {} lmgw may use",
                    fmt_bytes(needed_bytes),
                    fmt_bytes(total_bytes),
                    fmt_bytes(headroom_bytes),
                    fmt_bytes(cap)
                ),
            ),
        },
    };
    Ok(dto::RealtimeBudget {
        stages,
        total_bytes,
        unknown,
        headroom_bytes,
        needed_bytes,
        capacity_bytes,
        capacity_source,
        outside_bytes,
        outside_note,
        verdict: verdict.into(),
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::loopback;

    #[test]
    fn a_loopback_base_url_is_this_machine() {
        assert!(loopback("http://127.0.0.1:9292/v1"));
        assert!(loopback("http://localhost:8080"));
        assert!(loopback("http://[::1]:8080/v1"));
        assert!(!loopback("https://api.openai.com/v1"));
        assert!(!loopback("http://192.168.1.20:8080/v1"));
    }
}
