//! The audio class's device index, read once as a pin (device-ledger design
//! §8.1, WP5).
//!
//! `AudioSettings.device` is written into every audio model's `server.json`
//! and numbers a card among those the engine sees. Where lmgw hands the
//! engine its cards (§7.1) it sees one, numbered 0, so the index cannot
//! choose the card any more: a start handed its cards is rendered with 0
//! (`runtime::audio::handed_settings`). The choice it made is kept as what
//! it is now, a pin: on the first device read that lists the card the index
//! names, every audio row without a pin of its own whose engine lmgw can
//! hand that card is pinned to it, and the setting goes to 0.
//!
//! **Only where it applies.** The class must hand NVIDIA cards through CDI
//! (its backend `cuda`, its run arguments passing NVIDIA cards): a Vulkan,
//! HIP or Metal build sees every card and its index still chooses. A row
//! whose own run arguments lmgw cannot hand a card keeps the setting for
//! all: it is left as it is, and said in the log. An index that names no
//! card the read lists is left as it is, said once per run. One read as a
//! pin goes to 0, and a non-zero index saved later is read again.
//!
//! **What moves with it.** An audio row's learned residency is keyed by its
//! configuration, the device index included
//! ([`crate::vram::residency::resident_key`]): the figures learned under
//! the old index are kept under the new key, so nothing is learned again.
//! A running container is adopted as it is at the next boot until it is
//! started again: its `server.json` names the old index, the rendered one
//! 0, and adoption restarts it once (§8.1).

use std::sync::atomic::Ordering;

use crate::config::Snapshot;
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::{GpuMemory, Target, VramScheduler};

impl VramScheduler {
    /// Read the audio class's device index as a pin, once (module doc),
    /// where `snap` has one and `devices` lists cards. The work is spawned:
    /// the ledger read that saw it goes on as it is.
    pub(in crate::vram) fn audio_device_once(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        devices: &[GpuMemory],
    ) {
        let a = &snap.settings.audio;
        if a.device == 0
            || devices.is_empty()
            || !a.backend.trim().eq_ignore_ascii_case("cuda")
            || !crate::runtime::argv::passes_cdi(&a.extra_run_args)
            || self.audio_device_read.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let st = state.clone();
        let devices = devices.to_vec();
        tokio::spawn(async move {
            if let Err(e) = st.vram.audio_device_to_pin(&st, &devices).await {
                tracing::warn!("the audio device index could not be read as a GPU pin: {e}");
            }
        });
    }

    async fn audio_device_to_pin(
        &self,
        state: &SharedState,
        devices: &[GpuMemory],
    ) -> Result<(), String> {
        // Held across the read-modify-write, as every settings write is.
        let _guard = state.settings_write.lock().await;
        let snap = state.snapshot();
        let old = snap.settings.audio.clone();
        if old.device == 0 {
            self.audio_device_read.store(false, Ordering::Release);
            return Ok(());
        }
        let Some(card) = devices
            .iter()
            .find(|d| i64::from(d.index) == old.device && d.id.as_str().starts_with("GPU-"))
        else {
            tracing::warn!(
                "the audio class's device index {} names no NVIDIA card this computer lists; it \
                 is left as it is",
                old.device
            );
            return Ok(());
        };
        let mut pinned = Vec::new();
        let mut kept = Vec::new();
        for m in &snap.audio_models {
            let target = Target {
                class: Class::Audio,
                model_id: m.model_id.clone(),
            };
            let cand = self.candidates(&snap, &target, devices);
            if cand.pin.is_some() {
                continue;
            }
            if !cand.hand_out {
                kept.push(m.model_id.clone());
                continue;
            }
            pinned.push(m);
        }
        if !kept.is_empty() {
            tracing::warn!(
                "the audio class's device index {} is left as it is: lmgw cannot hand a card to \
                 {} (its run arguments), which the index still chooses for",
                old.device,
                kept.join(", ")
            );
            return Ok(());
        }
        let new = crate::runtime::audio::handed_settings(&old);
        for m in &pinned {
            crate::store::set_gpu_pin(
                &state.db,
                Class::Audio,
                m.id,
                Some(std::slice::from_ref(&card.id)),
            )
            .await
            .map_err(|e| e.to_string())?;
            // The learned residency moves to the key it has now (module
            // doc).
            if let Some(r) = &m.residency {
                if r.key == crate::vram::residency::resident_key(m, &old) {
                    let key = crate::vram::residency::resident_key(m, &new);
                    crate::store::set_audio_model_residency(
                        &state.db,
                        m.id,
                        Some((r.bytes, key.as_str())),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        let mut settings = snap.settings.clone();
        settings.audio.device = 0;
        crate::store::save_settings(&state.db, &settings)
            .await
            .map_err(|e| e.to_string())?;
        state.reload_snapshot().await.map_err(|e| e.to_string())?;
        // A non-zero index saved later is read again.
        self.audio_device_read.store(false, Ordering::Release);
        tracing::info!(
            "the audio class's device index {} is now a GPU pin: {} pinned to {} (card {}), and \
             the index is 0 (device-ledger design §8.1)",
            old.device,
            match pinned.len() {
                0 => "no audio model was".to_string(),
                1 => "1 audio model was".to_string(),
                n => format!("{n} audio models were"),
            },
            card.name,
            card.index
        );
        Ok(())
    }
}
