-- VRAM admission control (quickdoc §9b) moves eviction policy out of
-- llama-server and into lmgw, so both routers have to stop running their own.
--
-- `--models-max` is a *count* limit and counting is the wrong unit: a sleeping
-- model holds no VRAM yet still occupies a slot, and a load that fails at
-- capacity has already spent an eviction with no rollback — one OOM costs a
-- healthy model and leaves the slot empty. At 0 the router's LRU no-ops
-- entirely and `crate::vram` decides, against actual GPU memory.
--
-- This rewrites the stored value rather than only the code default, because an
-- install that has ever saved settings carries its own copy of the chat
-- router's `models_max = 1`. The flag lives in the `podman run` argv, so it
-- takes effect when the container is next restarted — the settings page says
-- exactly that for every container-definition change.
UPDATE settings
SET value = json_set(value, '$.router.models_max', 0)
WHERE key = 'settings' AND json_extract(value, '$.router.models_max') IS NOT NULL;

UPDATE settings
SET value = json_set(value, '$.aux_router.models_max', 0)
WHERE key = 'settings' AND json_extract(value, '$.aux_router.models_max') IS NOT NULL;
