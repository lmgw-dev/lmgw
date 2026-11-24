//! The shared fake machine (device-ledger design §12): cards with a total and
//! an id, the processes on each card and what each holds there, and memory
//! in use that no process list shows. The suite's fake probes build their
//! answers on it, each keeping what it does today: they decide what is on
//! the card (a container loaded at its file's size, a process that grows on
//! every read, a scripted reading) and hand that to a [`Machine`] for the
//! [`GpuMemory`] and [`DeviceProcesses`] they answer with.
//!
//! A machine of one card is what every test before the device ledger
//! stated: [`Machine::one_card`], whose card is index 0 with the id
//! [`card_id`]`(0)`.

#![allow(dead_code)]

use lmgw_core::vram::nvml::{DeviceId, DeviceProcesses};
use lmgw_core::vram::{GpuMemory, ProcessMemory};

/// The fake UUID of card `index`, in NVML's form (`GPU-…`).
pub fn card_id(index: u32) -> DeviceId {
    DeviceId(format!("GPU-00000000-0000-4000-8000-{index:012x}"))
}

/// One card of a [`Machine`].
#[derive(Debug, Clone)]
pub struct Card {
    pub id: DeviceId,
    pub name: String,
    pub total: u64,
    /// The processes the driver lists here, and what each holds here.
    pub processes: Vec<ProcessMemory>,
    /// Memory in use here that no listed process accounts for: what a probe
    /// without process lists reads as used, a process the driver does not
    /// list.
    pub unlisted: u64,
}

impl Card {
    fn used(&self) -> u64 {
        self.processes
            .iter()
            .filter_map(|p| p.bytes)
            .sum::<u64>()
            .saturating_add(self.unlisted)
    }
}

/// A machine with its cards, in the driver's order.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub cards: Vec<Card>,
}

impl Machine {
    pub fn new() -> Self {
        Self::default()
    }

    /// One card named `name` with `total` bytes: the machine of every test
    /// before the device ledger.
    pub fn one_card(name: &str, total: u64) -> Self {
        Self::new().card(card_id(0), name, total)
    }

    /// Add a card after the others.
    pub fn card(mut self, id: DeviceId, name: &str, total: u64) -> Self {
        self.cards.push(Card {
            id,
            name: name.into(),
            total,
            processes: Vec::new(),
            unlisted: 0,
        });
        self
    }

    /// `pid` holds `bytes` on card `index` (`None`: the driver lists it
    /// there without a figure, which no used figure counts). A PID already
    /// listed there keeps its first figure, as a driver lists a process once
    /// per card.
    pub fn hold(&mut self, index: usize, pid: u32, bytes: Option<u64>) -> &mut Self {
        let card = &mut self.cards[index];
        if !card.processes.iter().any(|p| p.pid == pid) {
            card.processes.push(ProcessMemory { pid, bytes });
        }
        self
    }

    /// `bytes` in use on card `index` that no process list shows.
    pub fn unlisted(&mut self, index: usize, bytes: u64) -> &mut Self {
        let card = &mut self.cards[index];
        card.unlisted = card.unlisted.saturating_add(bytes);
        self
    }

    /// The devices, as a probe answers them: each card's used memory is
    /// what its processes hold there and what is unlisted.
    pub fn devices(&self) -> Vec<GpuMemory> {
        self.cards
            .iter()
            .enumerate()
            .map(|(index, c)| {
                let used = c.used();
                GpuMemory {
                    id: c.id.clone(),
                    index: index as u32,
                    name: c.name.clone(),
                    total_bytes: c.total,
                    used_bytes: used,
                    free_bytes: c.total.saturating_sub(used),
                }
            })
            .collect()
    }

    /// The process lists, per card, each ordered by PID as NVML's merge
    /// orders them.
    pub fn processes(&self) -> Vec<DeviceProcesses> {
        self.cards
            .iter()
            .map(|c| {
                let mut processes = c.processes.clone();
                processes.sort_by_key(|p| p.pid);
                DeviceProcesses {
                    device: c.id.clone(),
                    processes,
                }
            })
            .collect()
    }

    /// The cards a container's rendered run arguments name with `--device
    /// nvidia.com/gpu=…`, as indices: `all` names every card, a UUID the
    /// card with that id, a number the card at that index. A name that is
    /// no card here names none, and so does a container without such an
    /// argument: CDI hands a container no card it was not named.
    pub fn cards_named(&self, argv: &[String]) -> Vec<usize> {
        let named: Vec<&str> = argv
            .windows(2)
            .filter(|w| w[0] == "--device")
            .filter_map(|w| w[1].strip_prefix("nvidia.com/gpu="))
            .collect();
        if named.contains(&"all") {
            return (0..self.cards.len()).collect();
        }
        let mut out: Vec<usize> = named
            .iter()
            .filter_map(|n| match n.parse::<usize>() {
                Ok(i) => (i < self.cards.len()).then_some(i),
                Err(_) => self.cards.iter().position(|c| c.id.as_str() == *n),
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}
