-- A per-model `extra_run_args` of `[]` becomes NULL: an empty override means
-- "inherit the class's run args" from now on, and is stored that way.
--
-- NULL has always meant "inherit" (0022, 0033). `[]` meant "run with no
-- extra args", which drops what the class's run args carry: the GPU
-- passthrough and `--security-opt label=disable`. A container started that
-- way cannot read its own model directory under SELinux ("Permission
-- denied" on /models) and has no card. Nobody asks for that by leaving the
-- field blank, which is how these rows came about — mostly the dashboard's
-- editors before a create honoured the `clear` they send with a blank
-- field — and every writer now folds an empty override into NULL. This
-- repairs the rows an install already has, so the next start of each such
-- model inherits the class's flags again.
--
-- `[]` is exactly what lmgw wrote for an empty list (serde_json), so the
-- comparison is exact. A row with args of its own is left as it is.
-- `mcp_servers.extra_run_args` is not a per-model override (it is NOT NULL,
-- with no class to inherit from) and is not touched.
UPDATE local_models SET extra_run_args = NULL WHERE extra_run_args = '[]';
UPDATE aux_models   SET extra_run_args = NULL WHERE extra_run_args = '[]';
UPDATE audio_models SET extra_run_args = NULL WHERE extra_run_args = '[]';
UPDATE image_models SET extra_run_args = NULL WHERE extra_run_args = '[]';
