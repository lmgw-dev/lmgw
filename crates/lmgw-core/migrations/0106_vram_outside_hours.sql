-- What VRAM outside lmgw used, hour by hour (VRAM admission's auto
-- headroom, per-model containers §4, "Headroom"): the lowest and highest reading of
-- the hour, and the largest rise within it (a reading less the lowest one
-- before it in the same hour). Auto headroom is the largest rise over the
-- last seven days, across hours as well (`vram::headroom::growth`). The
-- readings are the measured attribution's: the card's used memory less what
-- lmgw's own containers hold, per process. Hours older than the window are
-- deleted as new ones are written. `hour` is Unix time / 3600.
CREATE TABLE vram_outside_hours (
    hour       INTEGER PRIMARY KEY,
    low_bytes  INTEGER NOT NULL,
    high_bytes INTEGER NOT NULL,
    rise_bytes INTEGER NOT NULL
);
