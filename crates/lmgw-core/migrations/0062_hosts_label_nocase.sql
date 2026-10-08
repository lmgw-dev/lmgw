-- A device's hosting label is unique case-insensitively (client-apps design
-- §1.5, review W2-25). Every check that writes one folds case
-- (`devices::label_refusal`, `mcp_server_set`); the index 0061 made compared
-- bytes, so two creates racing past the checks with `Desk` and `desk` both
-- passed it. The backstop now reads the label the way the checks do.
DROP INDEX api_keys_hosts_label;
CREATE UNIQUE INDEX api_keys_hosts_label
    ON api_keys(hosts_label COLLATE NOCASE) WHERE hosts_label IS NOT NULL;
