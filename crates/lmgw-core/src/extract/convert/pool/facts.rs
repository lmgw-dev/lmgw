//! The pool's facts for Settings › Documents' expert row and a running
//! job's state ([`ToolPool::facts`]).

use lmgw_api_types::documents::{PoolCharges, PoolFacts, PoolInputs, StepCharge};

use super::{charge, share, ToolPool};

impl ToolPool {
    /// The size in force, what it was derived from, and what runs and waits
    /// now: Settings › Documents' expert row.
    pub fn facts(&self) -> PoolFacts {
        let g = self.lock();
        let slots = u32::try_from(g.size()).unwrap_or(u32::MAX);
        let memory = g.memory();
        let size = g.size();
        let fit = |bytes: u64| {
            memory.map_or(slots, |m| {
                u32::try_from(m / bytes.max(1))
                    .unwrap_or(u32::MAX)
                    .clamp(1, slots.max(1))
            })
        };
        let step = |bytes: u64| StepCharge {
            bytes,
            fit: fit(bytes),
            per_caller: if g.shared {
                u32::try_from(share::steps_of(size, memory, bytes)).unwrap_or(u32::MAX)
            } else {
                fit(bytes)
            },
        };
        PoolFacts {
            slots,
            derived: g.derived,
            derived_from: PoolInputs {
                cores: g.machine.cores,
                memory_bytes: g.machine.memory.map(|m| m.bytes),
                memory_source: g.machine.memory.map(|m| m.source),
                charges: PoolCharges {
                    pdf_file: step(charge::PDF_FILE),
                    page_crops: step(charge::PAGE_CROPS),
                    ocr_page: step(charge::OCR_PAGE),
                    reader_run: step(charge::READER_RUN),
                },
            },
            by_hand: g.by_hand > 0,
            memory_counted: memory.is_some(),
            charged_bytes: g.charged,
            running: u32::try_from(g.running).unwrap_or(u32::MAX),
            waiting: u32::try_from(g.waiters()).unwrap_or(u32::MAX),
            per_caller_slots: if g.shared {
                u32::try_from(share::slots_of(size)).unwrap_or(u32::MAX)
            } else {
                slots
            },
        }
    }
}
