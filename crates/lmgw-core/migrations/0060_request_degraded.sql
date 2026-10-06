-- What a request's content lost on its way to a model that lacks a
-- capability (the owner's requirement of 2026-10-06): "fallback 'x' lacks
-- vision: 3 images sent as placeholders", "'m' lacks audio: transcript
-- sent", several joined by "; " (`crate::degraded`). Shown on the Traffic
-- page and in the row's JSON. NULL when nothing was degraded.
ALTER TABLE request_logs ADD COLUMN degraded TEXT;
