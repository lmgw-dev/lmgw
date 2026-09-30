-- Per-upstream model hide list: passthrough models in this table are excluded
-- from GET /v1/models and the admin UI's visible list, but remain routable.
CREATE TABLE hidden_passthrough_models (
    upstream_id INTEGER NOT NULL REFERENCES upstreams(id) ON DELETE CASCADE,
    model_id    TEXT    NOT NULL,
    PRIMARY KEY (upstream_id, model_id)
);
