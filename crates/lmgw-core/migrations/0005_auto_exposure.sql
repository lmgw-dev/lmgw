-- Automatic model exposure (QOL): public local models are routable without a
-- manual alias (via the built-in router upstream), and upstreams can expose
-- their whole model catalog under an optional prefix.
ALTER TABLE local_models ADD COLUMN public INTEGER NOT NULL DEFAULT 1;
ALTER TABLE upstreams ADD COLUMN expose_all INTEGER NOT NULL DEFAULT 0;
ALTER TABLE upstreams ADD COLUMN expose_prefix TEXT NOT NULL DEFAULT '';
