//! Learning an audio container's residency: one per-process reading after a
//! request it answered, folded with what the sampler saw while it worked
//! ([`super::sample`]), stored on the row when it is new or larger.

use crate::config::{AudioModel, AudioSettings, Snapshot};
use crate::hf::fmt_bytes;
use crate::runtime::registry::RuntimeState;
use crate::state::SharedState;

use super::super::{broadcast, VramScheduler};
use super::tracker::GenView;
use super::{residency_sentence, resident_key, Sampling};

/// Ends a generation's reading however its task ends — a store that failed,
/// a panic, a runtime shutting down — so the next request reads again.
struct Measuring<'a> {
    vram: &'a VramScheduler,
    generation: u64,
    outcome: Result<bool, String>,
}

impl Drop for Measuring<'_> {
    fn drop(&mut self) {
        let outcome = std::mem::replace(&mut self.outcome, Ok(true));
        self.vram.residency.measured(self.generation, outcome);
    }
}

/// What [`VramScheduler::store_resident`] did with a figure.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Stored {
    /// It is the row's figure now.
    Raised,
    /// The row's figure for this configuration is already at least as large.
    Kept,
    /// Not this row's figure to keep: the container stopped, runs a previous
    /// configuration, or the owner reset the row since it was read.
    Dropped,
}

impl VramScheduler {
    /// Whether audio container `generation`, started for row `m`, holds its
    /// model now as far as lmgw can tell without asking it: once it has
    /// answered an inference request (or was read loaded at rest), until
    /// audio.cpp's `idle_unload_ms` drops it ([`super::holds`]); before that,
    /// only a row that loads at start ([`super::is_eager`]). A lazy row's
    /// container that answered nothing yet has not loaded it — what the
    /// realtime warm loads for (live run 3b, D3').
    pub(crate) fn audio_model_loaded(
        &self,
        m: &AudioModel,
        s: &AudioSettings,
        generation: u64,
    ) -> bool {
        match self.residency.get(generation) {
            Some(g) if g.last_answered.is_some() => {
                super::holds(g.last_answered, std::time::Instant::now(), s)
            }
            _ => super::is_eager(m, s),
        }
    }

    /// An audio container answered an inference request with a 2xx
    /// ([`super::super::LocalHold::note_inference`]): it has loaded its
    /// model, so nothing is pending for it any more, and one reading of what
    /// it holds is taken — unless one is already in flight for it, or this
    /// host's probe has no per-process figures to read.
    pub(crate) fn audio_inference(&self, state: &SharedState, generation: u64, model_id: &str) {
        let noted = self.residency.note(generation);
        // On the CPU, "it has answered" is all there is to know — what keeps
        // `audio_model_loaded` right for the realtime load. Nothing is on the
        // card to read, so no reading, no figure and no log line.
        let on_cpu = state
            .runtime()
            .placement_of(crate::runtime::Class::Audio, model_id)
            == Some(crate::runtime::Placement::Cpu);
        if on_cpu {
            // Skipped, not failed: nothing for the notes to say.
            self.residency.measured(generation, Ok(false));
            return;
        }
        if noted.newly_loaded {
            // The free figure just grew by what was pending, and no other
            // event tells the dashboard.
            broadcast(state);
        }
        if !noted.measure {
            return;
        }
        if self.probe().process_support().is_err() {
            // Said on every surface instead ("cannot learn: …").
            self.residency.measured(generation, Ok(false));
            return;
        }
        let epoch = self.residency.epoch(model_id);
        let st = state.clone();
        let model_id = model_id.to_string();
        tokio::spawn(async move {
            let mut measuring = Measuring {
                vram: &st.vram,
                generation,
                outcome: Err("the reading ended before it finished".into()),
            };
            let read = match st.vram.container_pids(&st, generation, &model_id).await {
                Ok(pids) => st.vram.read_processes(&pids, true).await,
                Err(why) => Err(why),
            };
            measuring.outcome = match read {
                Ok(Some(bytes)) if bytes > 0 => {
                    // What the sampler saw while the request ran is part of
                    // what this answer cost: audio.cpp frees some compute
                    // buffers once it has answered (the WP7 live gate).
                    let sampled = st.vram.residency.sampled(generation, epoch).unwrap_or(0);
                    st.vram
                        .store_resident(&st, generation, &model_id, bytes.max(sampled), epoch)
                        .await;
                    Ok(true)
                }
                Ok(_) => Err("the driver lists none of its processes holding anything".into()),
                Err(why) => Err(why),
            };
            if let Err(why) = &measuring.outcome {
                // Once per container start and reason at INFO: a container
                // that can never be read (the CPU backend) fails after every
                // request, and the same line each time buried the log. The
                // row's note keeps saying it.
                match st.vram.residency.failure_is_new(generation, why) {
                    true => tracing::info!(
                        "audio/{model_id}: residency not read after its request — {why}; the \
                         next answered request reads again (said once for this container \
                         while the reason stays the same)"
                    ),
                    false => tracing::debug!(
                        "audio/{model_id}: residency not read after its request — {why}"
                    ),
                }
                // The note now says why.
                broadcast(&st);
            }
        });
    }

    /// Keep `bytes` as the row's residency when it is the first figure for
    /// the row's configuration or larger than the one stored — and only when
    /// the container read is still the one running, was started with the
    /// row's current configuration, and the owner has not reset the row
    /// since the figure was read (`epoch`).
    pub(super) async fn store_resident(
        &self,
        state: &SharedState,
        generation: u64,
        model_id: &str,
        bytes: u64,
        epoch: u64,
    ) -> Stored {
        if bytes == 0 {
            return Stored::Dropped;
        }
        let _store = self.residency.store.lock().await;
        if self.residency.epoch(model_id) != epoch {
            tracing::debug!(
                "audio/{model_id}: its residency was reset while this reading ran, so the \
                 reading is dropped"
            );
            return Stored::Dropped;
        }
        let snap = state.snapshot();
        let Some(row) = snap.audio_models.iter().find(|m| m.model_id == model_id) else {
            return Stored::Dropped;
        };
        let key = resident_key(row, &snap.settings.audio);
        let running = state.runtime().list().into_iter().find(|e| {
            e.generation == generation
                && e.class == crate::runtime::Class::Audio
                && e.state == RuntimeState::Ready
        });
        let Some(entry) = running else {
            tracing::debug!(
                "audio/{model_id}: its container stopped before its residency was recorded"
            );
            return Stored::Dropped;
        };
        if entry.resident_key.as_deref() != Some(key.as_str()) {
            tracing::debug!(
                "audio/{model_id}: its container runs a previous configuration, so its reading \
                 is not this configuration's residency"
            );
            return Stored::Dropped;
        }
        let previous = row.residency.as_ref().filter(|r| r.key == key);
        if previous.is_some_and(|r| r.bytes >= bytes) {
            return Stored::Kept;
        }
        let previous = previous.map(|r| r.bytes);
        if let Err(e) =
            crate::store::set_audio_model_residency(&state.db, row.id, Some((bytes, &key))).await
        {
            tracing::warn!("could not record the residency of audio/{model_id}: {e}");
            return Stored::Dropped;
        }
        if let Err(e) = state.reload_snapshot().await {
            tracing::warn!("residency of audio/{model_id} recorded but not reloaded: {e}");
            return Stored::Dropped;
        }
        tracing::info!(
            "audio/{model_id}: its container holds {} once loaded{} — admission charges that \
             from now on",
            fmt_bytes(bytes),
            match previous {
                Some(p) => format!(" (was {})", fmt_bytes(p)),
                None => String::new(),
            }
        );
        // Still under the store lock (`Residency::raised`).
        self.residency.raised(generation);
        broadcast(state);
        Stored::Raised
    }

    /// The owner's reset of a row's learned residency (`audio_model_set`
    /// with `clear: "residency"`): forget the stored figure, and drop every
    /// reading and sampler that began before it — under the store lock, so a
    /// reading that was about to write cannot write the old figure back
    /// after the reset (WP7 review, low).
    pub async fn reset_audio_residency(
        &self,
        state: &SharedState,
        id: i64,
        model_id: &str,
    ) -> Result<(), String> {
        let _store = self.residency.store.lock().await;
        self.residency.reset(model_id);
        crate::store::set_audio_model_residency(&state.db, id, None)
            .await
            .map_err(|e| e.to_string())
    }

    /// The models page's residency sentence for one audio row
    /// ([`residency_sentence`]): the learned figure, or what the row is
    /// charged instead and why — with why its running container's last
    /// reading failed, when it did.
    pub async fn audio_residency_note(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        m: &AudioModel,
    ) -> String {
        let s = &snap.settings.audio;
        if !crate::runtime::audio::placement(m, s).is_gpu() {
            return cpu_sentence(m, s, crate::host::cpu());
        }
        let on_disk = self
            .plans
            .audio(&snap.settings.audio.models_dir, m)
            .await
            .weights_bytes;
        let cannot = self.probe().process_support().err();
        let sentence = residency_sentence(
            m,
            &snap.settings.audio,
            on_disk,
            cannot.as_deref(),
            self.sampling(m, &snap.settings.audio),
        );
        let running: Vec<u64> = state
            .runtime()
            .list()
            .into_iter()
            .filter(|e| e.class == crate::runtime::Class::Audio && e.model_id == m.model_id)
            .map(|e| e.generation)
            .collect();
        let failure = running
            .into_iter()
            .find_map(|g| self.residency.get(g).and_then(|v: GenView| v.failure));
        match failure {
            Some(why) => format!("{sentence}. {}", super::failure_note(&why)),
            None => sentence,
        }
    }

    /// Where the row's current configuration stands with the sampler.
    pub(in crate::vram) fn sampling(
        &self,
        m: &AudioModel,
        s: &crate::config::AudioSettings,
    ) -> Sampling {
        let key = resident_key(m, s);
        Sampling {
            calm: self.residency.calm(&m.model_id, &key),
        }
    }
}

/// The models page's sentence for a row on the CPU: nothing is charged on
/// the GPU and nothing is learned, and how many threads it runs.
pub fn cpu_sentence(m: &AudioModel, s: &AudioSettings, host: crate::host::HostCpu) -> String {
    let (threads, source) = crate::runtime::audio::threads_in_effect(m, s, host);
    format!(
        "runs on the CPU ({threads} threads, {}): nothing is charged on the GPU, and no \
         residency is learned",
        crate::runtime::audio::threads_source_label(source, host)
    )
}
