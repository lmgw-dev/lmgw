//! What the engine itself costs on a card (device-ledger design §4.2, D5,
//! ruling 17): the CUDA context and whatever else llama.cpp keeps on every
//! card it touches, beside the model's weights, KV cache and compute
//! buffers. WP0 measured it a figure of the card, not of the model or the
//! number of cards: 621 MiB on an RTX PRO 6000 Blackwell and 480 on an RTX
//! 6000 Ada, the same on every start (§1.1).
//!
//! **Learned per card.** After a llama-server start on one card (chat or
//! aux), once its container is ready and idle, what the probe lists for its
//! processes on that card less the buffers lmgw estimated there is a
//! reading of that card's engine cost. The last [`KEPT`] readings of each
//! card are kept, in memory and stored (migration 0113), and the card is
//! charged the largest of them (owner, 2026-11-18): a reading taken beside
//! an estimate that ran high does not lower it, and one wrong reading
//! leaves after five more. A reading above [`MOST`] is not kept at all: it
//! says the model's estimate was off, not what the engine costs, and is
//! logged as such. Read once per container, as soon as it is ready and
//! idle: when a start settles, beside the readings at rest of audio and
//! image containers ([`VramScheduler::cache_pids`]), and on the headroom's
//! periodic reading (every second, `headroom::growth::sample`), so a start
//! whose own first request was still in flight is read once it is idle;
//! nothing is read without per-process figures.
//!
//! **Until a card has one** it is charged the stated default of its
//! backend: [`CUDA_DEFAULT`] for an NVIDIA card, the largest WP0 saw
//! rounded up. A card of another backend (AMD) has no measured figure and
//! is charged [`OTHER_DEFAULT`] until it is learned (the owner, 2026-11-16).
//!
//! **Charged** on a llama.cpp model's charge, on each card it is placed on
//! (§4.2): the reservation's and the running entry's. Other engines (image,
//! audio) have their own context in their figures already.

use std::collections::{HashMap, HashSet};

use crate::hf::fmt_bytes;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::nvml::{DeviceId, DeviceProcesses};
use super::super::scheduler::MIB;
use super::super::{broadcast, VramScheduler};

/// An NVIDIA card's engine cost until it is learned (ruling 17).
pub const CUDA_DEFAULT: u64 = 640 * MIB;

/// Any other card's engine cost until it is learned (the owner,
/// 2026-11-16): what left `LLAMA_MARGIN` when the cost was charged on its
/// own (807 MiB less 327), so such a card keeps as much free as it did
/// before; nothing was measured on one.
pub const OTHER_DEFAULT: u64 = 480 * MIB;

/// How many readings of a card are kept (module doc, owner, 2026-11-18).
pub const KEPT: usize = 5;

/// The largest reading kept (module doc, owner, 2026-11-18): no llama.cpp
/// engine measured so far took more than 1 GiB on a card, so more
/// than twice that is the model's estimate being off.
pub const MOST: u64 = 2 * 1024 * MIB;

/// A card's last readings, oldest first, each (bytes, Unix seconds).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Readings(std::collections::VecDeque<(u64, i64)>);

impl Readings {
    /// Add a reading, dropping the oldest beyond [`KEPT`].
    pub(crate) fn push(&mut self, bytes: u64, at: i64) {
        self.0.push_back((bytes, at));
        while self.0.len() > KEPT {
            self.0.pop_front();
        }
    }

    /// What the card is charged: the largest reading, and when the newest
    /// was taken; `None` without readings.
    pub(crate) fn cost(&self) -> Option<(u64, i64)> {
        let most = self.0.iter().map(|(b, _)| *b).max()?;
        let newest = self.0.iter().map(|(_, at)| *at).max()?;
        Some((most, newest))
    }
}

/// Whether a reading of `held` against `estimate` is kept (module doc):
/// `Ok(cost)` where the difference is at most [`MOST`], else `Err` with
/// what the log says of the model `label` on card `card`.
pub(crate) fn reading(label: &str, card: &str, held: u64, estimate: u64) -> Result<u64, String> {
    let bytes = held.saturating_sub(estimate);
    if bytes <= MOST {
        return Ok(bytes);
    }
    Err(format!(
        "{label}: its estimate looks off on {card}: its processes hold {}, lmgw's estimate of \
         its buffers there is {}, {} apart — more than the {} an engine's own cost is taken to \
         be at most, so this reading is not kept as the card's engine cost",
        fmt_bytes(held),
        fmt_bytes(estimate),
        fmt_bytes(bytes),
        fmt_bytes(MOST)
    ))
}

/// The engine costs learned per card ([`VramScheduler::engine_on`]).
#[derive(Debug, Default)]
pub(crate) struct EngineCosts {
    /// Read from the store once.
    loaded: bool,
    /// Card → its last readings.
    cards: HashMap<DeviceId, Readings>,
    /// The containers read, by generation: once each.
    read: HashSet<u64>,
    /// What a test states in place of [`CUDA_DEFAULT`] and
    /// [`OTHER_DEFAULT`] ([`VramScheduler::set_engine_default`]).
    stated_default: Option<u64>,
}

/// One card's engine cost as Settings shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCost {
    /// What a llama.cpp model is charged on the card for it.
    pub bytes: u64,
    /// Learned on this card, and when (Unix seconds); `None` while the
    /// stated default (or nothing) is charged.
    pub learned_at: Option<i64>,
}

/// Whether `card` is an NVIDIA card (its id is NVML's `GPU-<uuid>`).
pub(crate) fn is_cuda(card: &DeviceId) -> bool {
    card.as_str().starts_with("GPU-")
}

/// The stated default of `card`'s backend: CUDA's for an NVIDIA card,
/// [`OTHER_DEFAULT`] for any other.
pub(crate) fn default_for(card: &DeviceId) -> u64 {
    if is_cuda(card) {
        CUDA_DEFAULT
    } else {
        OTHER_DEFAULT
    }
}

/// Whether `class` runs llama.cpp, whose cost per card is charged.
fn charged(class: Class) -> bool {
    matches!(class, Class::Chat | Class::Aux)
}

impl VramScheduler {
    /// What a `class` model's engine costs on `card` (module doc): learned,
    /// else the backend's stated default; nothing for an engine other than
    /// llama.cpp.
    pub(crate) fn engine_on(&self, class: Class, card: &DeviceId) -> u64 {
        if !charged(class) {
            return 0;
        }
        self.engine_cost(card).bytes
    }

    /// `card`'s engine cost for a llama.cpp model, and whether it is
    /// learned (Settings → GPU).
    pub fn engine_cost(&self, card: &DeviceId) -> EngineCost {
        let (known, default) = {
            let e = self.engine.lock().unwrap();
            (e.cards.get(card).and_then(Readings::cost), e.stated_default)
        };
        match known {
            Some((bytes, at)) => EngineCost {
                bytes,
                learned_at: Some(at),
            },
            None => EngineCost {
                bytes: default.unwrap_or_else(|| default_for(card)),
                learned_at: None,
            },
        }
    }

    /// State what a card is charged until it learns its own, in place of
    /// [`CUDA_DEFAULT`] and [`OTHER_DEFAULT`]: the test state charges nothing, as every
    /// test written before the engine's cost was charged expects
    /// (`AppState::init_for_tests`); a test of the cost states it.
    #[doc(hidden)]
    pub fn set_engine_default(&self, bytes: u64) {
        self.engine.lock().unwrap().stated_default = Some(bytes);
    }

    /// Read the stored figures once (the first ledger read).
    pub(in crate::vram) async fn load_engine_costs(&self, db: &sqlx::SqlitePool) {
        if self.engine.lock().unwrap().loaded {
            return;
        }
        match crate::store::list_vram_engine_readings(db).await {
            Ok(rows) => {
                let mut e = self.engine.lock().unwrap();
                // Each card's oldest first; a card read meanwhile keeps
                // what it read.
                let mut stored: HashMap<DeviceId, Readings> = HashMap::new();
                for (id, bytes, at) in rows {
                    stored.entry(id).or_default().push(bytes, at);
                }
                for (id, r) in stored {
                    e.cards.entry(id).or_insert(r);
                }
                e.loaded = true;
            }
            Err(err) => tracing::debug!(
                "VRAM: the engine cost per card could not be read: {err}; the defaults are \
                 charged until it is"
            ),
        }
    }

    /// Take `bytes` as card `card`'s engine cost, learned now, its only
    /// reading: for tests that state it.
    #[doc(hidden)]
    pub fn seed_engine_cost(&self, card: &DeviceId, bytes: u64) {
        let mut e = self.engine.lock().unwrap();
        e.loaded = true;
        let mut r = Readings::default();
        r.push(bytes, chrono::Utc::now().timestamp());
        e.cards.insert(card.clone(), r);
    }

    /// Read every ready, idle llama.cpp container on one card whose engine
    /// cost was not read yet (module doc).
    pub(in crate::vram) fn read_engine_costs(&self, state: &SharedState) {
        if self.probe().process_support().is_err() {
            return;
        }
        let entries = state.runtime().list();
        let mut todo = Vec::new();
        {
            let mut e = self.engine.lock().unwrap();
            e.read
                .retain(|g| entries.iter().any(|x| x.generation == *g));
            for x in entries {
                let one = x
                    .card_charge
                    .as_ref()
                    .is_some_and(|c| c.parts().len() == 1 && c.cards().len() == 1);
                if !charged(x.class)
                    || x.state != RuntimeState::Ready
                    || x.in_flight > 0
                    || !x.placement.is_gpu()
                    || !one
                    || !e.read.insert(x.generation)
                {
                    continue;
                }
                todo.push(x);
            }
        }
        for x in todo {
            let st = state.clone();
            tokio::spawn(async move { st.vram.engine_reading(&st, x).await });
        }
    }

    async fn engine_reading(&self, state: &SharedState, e: RuntimeView) {
        let label = format!("{}/{}", e.class.as_str(), e.model_id);
        let Some(card) = e.card_charge.as_ref().and_then(|c| c.cards().pop()) else {
            return;
        };
        let held = match self
            .container_pids_labeled(state, e.generation, label.clone())
            .await
        {
            Ok(pids) => self.read_processes_on(&pids, &card).await,
            Err(why) => Err(why),
        };
        let busy = state
            .runtime()
            .list()
            .iter()
            .any(|x| x.generation == e.generation && (x.in_flight > 0 || x.claims != e.claims));
        let held = match held {
            _ if busy => {
                // Read again at the next start's `cache_pids`.
                self.engine.lock().unwrap().read.remove(&e.generation);
                return;
            }
            Ok(Some(b)) if b > 0 => b,
            Ok(_) => return,
            Err(why) => {
                tracing::debug!("{label}: its engine's cost on the card could not be read: {why}");
                return;
            }
        };
        let snap = state.snapshot();
        let estimate = match &e.grant {
            Some(g) => g.bytes,
            None => match self.footprint_of(&snap, &e).await {
                Some(fp) => fp.total_bytes,
                None => return,
            },
        };
        let name = self
            .last_devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == card)
            .map(|d| format!("{} (card {})", d.name, d.index))
            .unwrap_or_else(|| card.to_string());
        let bytes = match reading(&label, &name, held, estimate) {
            Ok(bytes) => bytes,
            Err(off) => {
                tracing::info!("{off}");
                return;
            }
        };
        let at = chrono::Utc::now().timestamp();
        let before = self.engine_cost(&card);
        let now = {
            let mut e = self.engine.lock().unwrap();
            let r = e.cards.entry(card.clone()).or_default();
            r.push(bytes, at);
            r.cost().map_or(bytes, |(b, _)| b)
        };
        if let Err(err) =
            crate::store::put_vram_engine_reading(&state.db, &card, bytes, at, KEPT).await
        {
            tracing::warn!("could not record the engine cost of card {card}: {err}");
        }
        tracing::info!(
            "{label}: its processes hold {} on {name}, {} more than lmgw's estimate of its \
             buffers there: the engine's cost on that card is now {}, the largest of its last \
             {KEPT} readings (was {}{})",
            fmt_bytes(held),
            fmt_bytes(bytes),
            fmt_bytes(now),
            fmt_bytes(before.bytes),
            if before.learned_at.is_some() {
                ", learned"
            } else {
                ", the default"
            }
        );
        broadcast(state);
    }

    /// What the driver lists for `pids` on `card` right now; `Ok(None)` where
    /// it lists none of them there.
    async fn read_processes_on(
        &self,
        pids: &[u32],
        card: &DeviceId,
    ) -> Result<Option<u64>, String> {
        let probe = self.probe();
        let own = pids.to_vec();
        let listed = tokio::task::spawn_blocking(move || probe.processes(&own, &[]))
            .await
            .map_err(|e| format!("GPU probe thread failed: {e}"))?
            .map_err(|e| format!("no per-process GPU memory right now: {e}"))?;
        on_card(pids, &listed, card)
    }
}

/// What `pids` hold on `card` in `listed`.
fn on_card(
    pids: &[u32],
    listed: &[DeviceProcesses],
    card: &DeviceId,
) -> Result<Option<u64>, String> {
    let mut total: Option<u64> = None;
    for d in listed.iter().filter(|d| &d.device == card) {
        for p in d.processes.iter().filter(|p| pids.contains(&p.pid)) {
            let b = p.bytes.ok_or_else(|| {
                format!(
                    "the driver lists its process {} without saying how much memory it holds",
                    p.pid
                )
            })?;
            total = Some(total.unwrap_or(0).saturating_add(b));
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vram::nvml::ProcessMemory;

    /// An NVIDIA card's stated default is CUDA's; another card's is what
    /// left the margin, so it keeps as much free as before.
    #[test]
    fn the_stated_default_per_backend() {
        assert_eq!(default_for(&DeviceId::new("GPU-1234")), 640 * MIB);
        assert_eq!(default_for(&DeviceId::new("0000:17:00.0")), 480 * MIB);
        assert_eq!(
            OTHER_DEFAULT + crate::vram::headroom::LLAMA_MARGIN,
            807 * MIB
        );
    }

    /// A card is charged the largest of its last five readings (owner,
    /// 2026-11-18): a low reading after higher ones does not lower it, a
    /// sixth drops the oldest, and the time is the newest reading's.
    #[test]
    fn the_largest_of_the_last_five_readings() {
        let mut r = Readings::default();
        assert_eq!(r.cost(), None);
        r.push(600 * MIB, 1);
        r.push(620 * MIB, 2);
        assert_eq!(r.cost(), Some((620 * MIB, 2)));
        r.push(100 * MIB, 3);
        assert_eq!(
            r.cost(),
            Some((620 * MIB, 3)),
            "a low reading does not lower it"
        );
        r.push(500 * MIB, 4);
        r.push(510 * MIB, 5);
        assert_eq!(r.0.len(), KEPT);
        assert_eq!(r.cost(), Some((620 * MIB, 5)));
        // The sixth drops the oldest (600); the seventh the 620.
        r.push(400 * MIB, 6);
        assert_eq!(r.0.front(), Some(&(620 * MIB, 2)));
        r.push(400 * MIB, 7);
        assert_eq!(r.0.len(), KEPT);
        assert_eq!(r.cost(), Some((510 * MIB, 7)));
    }

    /// A reading above 2 GiB is not kept: it says the model's estimate is
    /// off, and the log names the model, the card, what it holds, the
    /// estimate and the difference.
    #[test]
    fn a_reading_above_two_gib_is_the_estimate_being_off() {
        assert_eq!(
            reading("chat/m", "card 0", 5 * 1024 * MIB, 4 * 1024 * MIB),
            Ok(1024 * MIB)
        );
        assert_eq!(reading("chat/m", "card 0", 3 * MIB, 5 * MIB), Ok(0));
        assert_eq!(
            reading("chat/m", "card 0", 2 * 1024 * MIB, 0),
            Ok(MOST),
            "2 GiB itself is kept"
        );
        let off = reading("chat/m", "RTX (card 1)", 9 * 1024 * MIB, 6 * 1024 * MIB).unwrap_err();
        assert!(
            off.contains("chat/m")
                && off.contains("its estimate looks off on RTX (card 1)")
                && off.contains("hold 9.0 GiB")
                && off.contains("is 6.0 GiB")
                && off.contains("3.0 GiB apart"),
            "{off}"
        );
    }

    /// What a container's processes hold on one card: its own processes'
    /// figures there, nothing of another card's; no figure is an error.
    #[test]
    fn what_the_processes_hold_on_a_card() {
        let a = DeviceId::new("GPU-a");
        let b = DeviceId::new("GPU-b");
        let listed = vec![
            DeviceProcesses {
                device: a.clone(),
                processes: vec![
                    ProcessMemory {
                        pid: 1,
                        bytes: Some(5),
                    },
                    ProcessMemory {
                        pid: 9,
                        bytes: Some(100),
                    },
                ],
            },
            DeviceProcesses {
                device: b.clone(),
                processes: vec![ProcessMemory {
                    pid: 1,
                    bytes: Some(7),
                }],
            },
        ];
        assert_eq!(on_card(&[1], &listed, &a), Ok(Some(5)));
        assert_eq!(on_card(&[1], &listed, &b), Ok(Some(7)));
        assert_eq!(on_card(&[2], &listed, &a), Ok(None));
        let blind = vec![DeviceProcesses {
            device: a.clone(),
            processes: vec![ProcessMemory {
                pid: 1,
                bytes: None,
            }],
        }];
        assert!(on_card(&[1], &blind, &a).is_err());
    }
}
