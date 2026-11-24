-- The engine's cost per card kept as its last readings (device-ledger design
-- §4.2, the owner's decision of 2026-11-18): 0110 kept the latest reading
-- alone, so one start whose estimate was off by some amount moved the card's
-- figure by as much until the next. Now the last five readings of each card
-- are kept and the card is charged the largest of them; a reading above
-- 2 GiB is not kept at all (`vram::place::engine`). `id` orders a card's
-- readings, oldest first; `device` is the card's stable id
-- (`GpuMemory.id`), `learned_at` Unix seconds.
--
-- 0110's figure of each card is carried over as that card's first reading.
CREATE TABLE vram_engine_readings (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    device     TEXT    NOT NULL,
    bytes      INTEGER NOT NULL,
    learned_at INTEGER NOT NULL
);
CREATE INDEX vram_engine_readings_device ON vram_engine_readings (device, id);
INSERT INTO vram_engine_readings (device, bytes, learned_at)
    SELECT device, bytes, learned_at FROM vram_engine_cost ORDER BY device;
DROP TABLE vram_engine_cost;
