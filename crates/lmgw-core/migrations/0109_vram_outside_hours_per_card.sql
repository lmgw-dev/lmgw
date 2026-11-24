-- What VRAM outside lmgw used, hour by hour, per card (device-ledger design
-- §9.1, WP3): 0106's record gains the card, and its key becomes the hour and
-- the card. `device` is the card's stable id (`GpuMemory.id`: NVML's
-- `GPU-<uuid>`, an AMD card's PCI address). Auto headroom is learned per card:
-- each card's largest rise over the last seven days (`vram::headroom::growth`).
--
-- 0106's readings were the cards' sum and cannot be told apart by card, so
-- they are dropped here: every install is on the fixed `headroom_mb` (kept
-- free on every card) until it has watched its cards for 24 hours again. The
-- columns are 0106's otherwise; `hour` is Unix time / 3600.
DROP TABLE vram_outside_hours;
CREATE TABLE vram_outside_hours (
    hour       INTEGER NOT NULL,
    device     TEXT    NOT NULL,
    low_bytes  INTEGER NOT NULL,
    high_bytes INTEGER NOT NULL,
    rise_bytes INTEGER NOT NULL,
    PRIMARY KEY (hour, device)
);
