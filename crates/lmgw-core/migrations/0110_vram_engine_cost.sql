-- What the engine itself costs on each card (device-ledger design §4.2,
-- ruling 17, WP5): after a llama-server start on one card, what the probe
-- says its processes hold there less the buffers lmgw estimated there — the
-- CUDA context and whatever else llama.cpp keeps on every card it touches.
-- One row per card, the latest reading; `device` is the card's stable id
-- (`GpuMemory.id`), `learned_at` Unix seconds. A card without a row is
-- charged the backend's stated default (640 MiB for CUDA).
CREATE TABLE vram_engine_cost (
    device     TEXT    PRIMARY KEY NOT NULL,
    bytes      INTEGER NOT NULL,
    learned_at INTEGER NOT NULL
);
