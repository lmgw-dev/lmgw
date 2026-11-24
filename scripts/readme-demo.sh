#!/usr/bin/env bash
# Prepare a dev copy for the README screenshots: scrub it, start it, add demo chats and live /v1 traffic.
#
# Usage: scripts/readme-demo.sh [dir=target/dev-copy] [addr=127.0.0.1:8899]
#
# Run on a fresh `scripts/dev-copy.sh copy`, then `scripts/readme-shots.sh` while the models are
# still resident. It stops the copy to scrub it, starts it again (through scripts/build-slot.sh:
# the start builds the headless runner), seeds four chat threads through the UI
# (scripts/drive/readme-chat.json, one reply each) and sends six /v1 chat requests (OpenAI and
# Anthropic shaped, to the traffic model) and two embeddings. Every request goes to a local model,
# none to a cloud upstream. It uses the GPU for one chat model: check that the hold is off and the
# card has room first. The copy is left running. A request that does not finish in time (curl's
# timeout) or fails, a reply that ends in an error, and an interrupt (Ctrl-C exits 130, SIGTERM
# 143), stop the demo's models in the copy (the chat model, the traffic model and the CPU embedder)
# before the script ends.
#
# The scrub keeps what the gallery shows of the setup (models, prices, upstreams, engines,
# settings) and removes what those pages could show of the person and their network. It does not
# make the whole copy anonymous: what the copy needs to run stays, namely the models directories
# and the run arguments under Settings' model-class pages, the model rows' file names, voice
# names, agent provenance (which agent came from where), prices' notes and every other column the
# list below does not name. The gallery's pages do not show those, and every shot drive
# (scripts/drive/readme-*.json) checks before each shot that the page's text names no home
# directory (/home, /var/home). Before it changes anything it checks that every table and column
# it touches exists, and refuses when one does not (a schema change must not skip a scrub
# silently). It prints every count, zeros included, so a second run on a scrubbed copy shows
# zeros. It
# - deletes chat threads and folders (and their search index), request logs, the usage and
#   latency rollups, stored responses, the chat feed's records, the labs' runs, files, batches and
#   presets, benchmark runs, transfers, MCP tasks, the audit log, background jobs, document
#   conversion jobs, model fit notes, chat profiles that are not built in, sign-in sessions and
#   remembered browsers, device keys and their pairing tokens;
# - deletes the model rows of every class, aliases, candidate aliases, Hugging Face downloads,
#   model gets and prices that name a model matching README_HIDE_MODELS, takes such a model out
#   of candidate lists, fallbacks and the settings;
# - deletes MCP servers named device:*, matching README_HIDE_MCP, or whose command, arguments,
#   working directory or run arguments name a home directory (/home, /var/home, /root, ~) or a
#   private host, or whose image is not from a public registry; the others keep their names, their
#   environment and header values become placeholders (a URL becomes https://<server>.example), a
#   server URL on a private host becomes https://<server>.example/mcp (any other URL loses its
#   userinfo, query and fragment), and tools switched off by hand are switched on again (their
#   names would list as Stale);
# - gives an upstream on a private host the base URL https://<name>.example and every upstream's
#   extra header values the same placeholders as the MCP servers';
# - renames API keys of kind `key` app-1, app-2 ... (their names are the apps that use the
#   gateway) and empties their notes; empties every key's hosts, URLs, client and enrolment
#   details and the login that made it; an owner, agent or internal key whose name is not one the
#   gateway makes becomes <kind>:key-<id>;
# - empties the accounts' display names (the logins become owner, user-<id>), the agents' settings
#   (folders, labels, models), candidate alias notes, engine build notes and run logs and errors,
#   and puts a build's repository that is not on a public forge at git.example;
# - resets the settings' public URL, trusted proxies, TLS listener, device listener, tokens
#   (Hugging Face, update, self-admin, forges), builds dir, the Chat's system prompt and profile,
#   and the Realtime instructions to their defaults, and sets the Chat voice's spoken and reply
#   language to en.
#
# Models, overridable: README_CHAT_MODEL (qwen3.5-4b, for the threads), README_TRAFFIC_MODEL (the
# chat model, so one model on the card serves the whole demo), README_EMBED_MODEL
# (embed/Qwen3-Embedding-0.6B-Q8_0-cpu, on the CPU), README_HIDE_MODELS (a SQL LIKE pattern,
# %uncensored%), README_HIDE_MCP (a SQL LIKE pattern for more MCP servers to drop, none by
# default), README_PRIVATE_DOMAINS (comma-separated domain suffixes, none by default, that the
# private-host rule treats as private besides the IP literals, single-label names and the
# private-use suffixes such as .internal, .lan and .ts.net; example.org covers example.org and
# *.example.org). Gateway auth is switched off so the keyless traffic below is served.
set -euo pipefail
cd "$(dirname "$0")/.."

dir="$(realpath -m "${1:-target/dev-copy}")"
addr="${2:-127.0.0.1:8899}"
chat_model="${README_CHAT_MODEL:-qwen3.5-4b}"
traffic_model="${README_TRAFFIC_MODEL:-$chat_model}"
embed_model="${README_EMBED_MODEL:-embed/Qwen3-Embedding-0.6B-Q8_0-cpu}"
hide="${README_HIDE_MODELS:-%uncensored%}"
hide_mcp="${README_HIDE_MCP:-}"
private_domains="${README_PRIVATE_DOMAINS:-}"

[ -e "$dir/lmgw.sqlite" ] || { echo "no copy at $dir (scripts/dev-copy.sh copy first)" >&2; exit 1; }

scripts/dev-copy.sh stop "$dir"
python3 - "$dir/lmgw.sqlite" "$hide" "$hide_mcp" "$private_domains" "$chat_model" "$traffic_model" <<'PY'
import ipaddress
import json
import re
import sqlite3
import sys
from urllib.parse import urlsplit

db, hide, hide_mcp, private_domains, chat_model, traffic_model = sys.argv[1:7]
c = sqlite3.connect(db)
c.execute("PRAGMA foreign_keys=ON")

# Every table and column the scrub reads or changes (EMPTIED: tables emptied whole). A schema
# change that drops or renames one stops the run here, before anything is changed.
EMPTIED = ["chat_threads", "chat_folders", "chat_fts", "request_logs", "usage_hourly",
           "usage_latency_hourly", "native_responses", "responses", "chat_feed", "lab_artifacts",
           "lab_runs", "lab_batches", "lab_groups", "lab_presets", "bench_runs", "transfers",
           "mcp_tasks", "tool_disabled", "audit_log", "jobs", "convert_jobs", "convert_job_files",
           "convert_calls", "model_fit_notes", "sessions", "known_browsers", "account_tokens",
           "device_pairing_tokens"]
COLUMNS = {
    "local_models": ["model_id", "gguf_path", "fallback", "params"],
    "aux_models": ["model_id", "gguf_path", "fallback"],
    "audio_models": ["model_id", "path", "fallback"],
    "image_models": ["model_id", "files", "fallback"],
    "models": ["alias", "upstream_model_id", "fallback"],
    "candidate_aliases": ["id", "alias", "candidates", "fallback", "notes"],
    "hf_models": ["repo", "file", "dest_path"],
    "model_gets": ["repo", "file", "model_id"],
    "prices": ["scope_key"],
    "upstreams": ["id", "name", "base_url", "extra_headers"],
    "mcp_servers": ["id", "name", "command", "args", "env", "cwd", "container_image",
                    "extra_run_args", "url", "headers"],
    "api_keys": ["id", "kind", "name", "note", "hosts_label", "made_by_login", "urls",
                 "client_info", "enrolled_from", "contested_from", "device_name",
                 "waiting_enrolled_from", "waiting_client_info", "waiting_device_name"],
    "chat_profiles": ["builtin"],
    "accounts": ["id", "login", "display_name"],
    "agents": ["id", "config"],
    "builds": ["id", "repo_url", "notes"],
    "build_runs": ["log_path", "error"],
    "settings": ["key", "value"],
    # What the end of this script reads and changes once the traffic ran.
    "request_logs": ["status"],
    "usage_hourly": ["outcome", "bucket_utc"],
    "usage_latency_hourly": ["alias", "bucket_utc"],
}
SETTINGS_RESET = ["public_url", "trusted_proxies", "tls", "devices", "hf_token", "update_token",
                  "self_admin_token", "forge_tokens", "builds_dir", "chat_system_prompt",
                  "chat_profile"]
REALTIME_RESET = ["default_instructions", "speech_instructions"]

tables = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
missing = [t for t in EMPTIED if t not in tables]
for t, cols in COLUMNS.items():
    have = {r[1] for r in c.execute(f"PRAGMA table_info({t})")} if t in tables else set()
    missing += [f"{t}.{col}" for col in cols if col not in have] if have else [t]


def fields(path, struct):
    """The `pub` fields of a Rust struct, read from its source (the settings JSON leaves out the
    unset optional ones, so it cannot say whether a key still exists)."""
    m = re.search(rf"^pub struct {struct} \{{\n(.*?)^\}}", open(path).read(), re.S | re.M)
    return set(re.findall(r"^    pub (\w+):", m.group(1), re.M)) if m else set()


settings_fields = fields("crates/lmgw-core/src/config/settings.rs", "Settings")
realtime_fields = fields("crates/lmgw-core/src/config/settings_classes.rs", "RealtimeSettings")
llama_fields = fields("crates/lmgw-core/src/config/llama_params.rs", "LlamaParams")
missing += [f"Settings.{k}" for k in SETTINGS_RESET + ["chat_voice_language",
            "chat_voice_reply_language", "realtime"] if k not in settings_fields]
missing += [f"RealtimeSettings.{k}" for k in REALTIME_RESET if k not in realtime_fields]
missing += [f"LlamaParams.{k}" for k in ["reasoning"] if k not in llama_fields]
row = c.execute("SELECT value FROM settings WHERE key = 'settings'").fetchone() if "settings" in tables else None
if not row:
    missing.append("the settings row")
s = json.loads(row[0]) if row else {}
if missing:
    sys.exit("readme-demo: the scrub names what this database does not have (the schema changed; "
             "update the scrub in scripts/readme-demo.sh first): " + ", ".join(sorted(set(missing))))


def like(pattern):
    """SQL LIKE as a regex (case-insensitive like SQLite's for ASCII), for values in JSON."""
    return re.compile("".join(".*" if ch == "%" else "." if ch == "_" else re.escape(ch)
                              for ch in pattern), re.I | re.S)


HIDDEN = like(hide)
HOME = re.compile(r"/home/|/var/home/|/root(/|\b)|(^|[\s\"'=:,\[])~(/|$|[\s\"',\]])")
PRIVATE_SUFFIX = (".internal", ".local", ".lan", ".home", ".home.arpa", ".localdomain", ".ts.net",
                  ".corp", ".intranet", ".private")
# README_PRIVATE_DOMAINS: more domain suffixes (comma-separated) that name a private network.
PRIVATE_DOMAINS = tuple(d.strip().lower().strip(".") for d in private_domains.split(",") if d.strip())
PRIVATE_SUFFIX += tuple("." + d for d in PRIVATE_DOMAINS)
PUBLIC_REGISTRIES = {"docker.io", "registry-1.docker.io", "ghcr.io", "quay.io", "gcr.io",
                     "mcr.microsoft.com", "public.ecr.aws", "registry.fedoraproject.org",
                     "registry.access.redhat.com", "registry.gitlab.com", "localhost"}
PUBLIC_FORGES = {"github.com", "gitlab.com", "codeberg.org", "bitbucket.org", "huggingface.co",
                 "git.sr.ht"}


def private_host(host):
    """A host of this machine's network rather than a public service: an IP literal that is not
    loopback, a single-label name other than localhost, a private-use suffix or a domain named in
    README_PRIVATE_DOMAINS."""
    host = host.strip("[]").lower().rstrip(".")
    if not host or host == "localhost" or host.endswith(".localhost"):
        return False
    if host in PRIVATE_DOMAINS:
        return True
    try:
        return not ipaddress.ip_address(host).is_loopback
    except ValueError:
        return "." not in host or host.endswith(PRIVATE_SUFFIX)


def text_private(text):
    """Whether a command line, path or argument list names a home directory or a private host."""
    if HOME.search(text):
        return True
    if any(private_host(m.group(0)) for m in re.finditer(r"\b(?:\d{1,3}\.){3}\d{1,3}\b", text)):
        return True
    if any(private_host(urlsplit(m.group(0)).hostname or "")
           for m in re.finditer(r"\w+://[^/\s\"']+", text)):
        return True
    return any(re.search(rf"[\w-]{re.escape(sfx)}\b", text.lower()) for sfx in PRIVATE_SUFFIX)


def image_private(image):
    """An image reference from a registry that is not a public one, or with a private part."""
    if not image:
        return False
    first = image.split("/")[0]
    named = "/" in image and ("." in first or ":" in first or first == "localhost")
    registry = first.split(":")[0].lower() if named else "docker.io"
    return registry not in PUBLIC_REGISTRIES or text_private(image)


def slug(name):
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-") or "mcp"


counts = {t: c.execute(f"DELETE FROM {t}").rowcount for t in EMPTIED}

# Models matching README_HIDE_MODELS, in every place that can name one.
counts["hidden model rows"] = sum(c.execute(q, (hide,) * q.count("?")).rowcount for q in [
    "DELETE FROM local_models WHERE model_id LIKE ? OR gguf_path LIKE ?",
    "DELETE FROM aux_models WHERE model_id LIKE ? OR gguf_path LIKE ?",
    "DELETE FROM audio_models WHERE model_id LIKE ? OR path LIKE ?",
    "DELETE FROM image_models WHERE model_id LIKE ? OR files LIKE ?",
    "DELETE FROM models WHERE alias LIKE ? OR upstream_model_id LIKE ?",
    "DELETE FROM candidate_aliases WHERE alias LIKE ?",
    "DELETE FROM hf_models WHERE repo LIKE ? OR file LIKE ? OR dest_path LIKE ?",
    "DELETE FROM model_gets WHERE repo LIKE ? OR file LIKE ? OR model_id LIKE ?",
    "DELETE FROM prices WHERE scope_key LIKE ?",
])
for t in ["local_models", "aux_models", "audio_models", "image_models", "models", "candidate_aliases"]:
    c.execute(f"UPDATE {t} SET fallback = NULL WHERE fallback LIKE ?", (hide,))
for cid, cands in c.execute("SELECT id, candidates FROM candidate_aliases").fetchall():
    kept = [m for m in json.loads(cands or "[]") if not HIDDEN.fullmatch(str(m))]
    if kept:
        c.execute("UPDATE candidate_aliases SET candidates = ? WHERE id = ?", (json.dumps(kept), cid))
    else:
        c.execute("DELETE FROM candidate_aliases WHERE id = ?", (cid,))


def unhide(v):
    """A settings value without any key or string naming a hidden model."""
    named = lambda x: isinstance(x, str) and HIDDEN.fullmatch(x)
    if isinstance(v, dict):
        return {k: unhide(x) for k, x in v.items() if not named(k) and not named(x)}
    if isinstance(v, list):
        return [unhide(x) for x in v if not named(x)]
    return v


def placeholders(name, pairs):
    """[[name, value]] with every value made up: a URL becomes https://<server>.example."""
    return json.dumps([[k, f"https://{slug(name)}.example" if re.match(r"\w+://", str(v)) else "demo-value"]
                       for k, v in json.loads(pairs or "[]")])


# MCP servers.
counts["mcp servers"] = 0
for row in c.execute("SELECT id, name, command, args, cwd, container_image, extra_run_args, url, "
                     "env, headers FROM mcp_servers").fetchall():
    sid, name, command, args, cwd, image, run_args, url, env, headers = row
    if (name.startswith("device:") or (hide_mcp and like(hide_mcp).fullmatch(name))
            or any(text_private(v or "") for v in (command, args, cwd, run_args))
            or image_private(image or "")):
        counts["mcp servers"] += c.execute("DELETE FROM mcp_servers WHERE id = ?", (sid,)).rowcount
        continue
    if url:
        parts = urlsplit(url)
        if private_host(parts.hostname or ""):
            url = f"https://{slug(name)}.example/mcp"
        else:  # no credentials, no query (a token often rides there)
            url = parts._replace(netloc=parts.netloc.rpartition("@")[2], query="", fragment="").geturl()
    c.execute("UPDATE mcp_servers SET env = ?, headers = ?, url = ? WHERE id = ?",
              (placeholders(name, env), placeholders(name, headers), url, sid))

# Upstreams: a base URL on a private host becomes https://<name>.example, the headers' values are
# made up (the names stay, the keys are the gateway's own and not shown).
counts["upstreams"] = 0
for uid, name, base, headers in c.execute("SELECT id, name, base_url, extra_headers FROM upstreams").fetchall():
    if private_host(urlsplit(base).hostname or ""):
        base = f"https://{slug(name)}.example"
        counts["upstreams"] += 1
    c.execute("UPDATE upstreams SET base_url = ?, extra_headers = ? WHERE id = ?",
              (base, placeholders(name, headers), uid))

# API keys.
counts["device keys"] = c.execute("DELETE FROM api_keys WHERE kind = 'device'").rowcount
c.execute("UPDATE api_keys SET hosts_label = NULL, made_by_login = NULL, urls = '[]', "
          "client_info = NULL, enrolled_from = NULL, contested_from = NULL, device_name = NULL, "
          "waiting_enrolled_from = NULL, waiting_client_info = NULL, waiting_device_name = NULL")
for n, (kid,) in enumerate(c.execute("SELECT id FROM api_keys WHERE kind = 'key' ORDER BY id").fetchall(), 1):
    c.execute("UPDATE api_keys SET name = ?, note = '' WHERE id = ?", (f"app-{n}", kid))
agents = {r[0] for r in c.execute("SELECT id FROM agents")}
made = re.compile(r"owner:(?:dashboard|self-admin|account-\d+|agent-(?P<a>.+))|agent:(?P<b>.+)"
                  r"|internal:[a-z0-9-]+")
for kid, kind, name in c.execute("SELECT id, kind, name FROM api_keys WHERE kind != 'key'").fetchall():
    m = made.fullmatch(name)
    agent = m and (m.group("a") or m.group("b"))
    if not m or (agent and agent not in agents):
        c.execute("UPDATE api_keys SET name = ? WHERE id = ?", (f"{kind}:key-{kid}", kid))

# Accounts, agents, profiles, notes, engine builds.
for n, (aid,) in enumerate(c.execute("SELECT id FROM accounts ORDER BY id").fetchall()):
    c.execute("UPDATE accounts SET login = ?, display_name = '' WHERE id = ?",
              ("owner" if n == 0 else f"user-{aid}", aid))
c.execute("UPDATE agents SET config = '{}'")
counts["chat profiles"] = c.execute("DELETE FROM chat_profiles WHERE coalesce(builtin, '') = ''").rowcount
c.execute("UPDATE candidate_aliases SET notes = ''")
c.execute("UPDATE builds SET notes = ''")
c.execute("UPDATE build_runs SET log_path = NULL, error = NULL")
for bid, repo in c.execute("SELECT id, repo_url FROM builds").fetchall():
    if (urlsplit(repo).hostname or "").lower() not in PUBLIC_FORGES:
        c.execute("UPDATE builds SET repo_url = ? WHERE id = ?",
                  ("https://git.example/" + repo.rstrip("/").rsplit("/", 1)[-1], bid))

# Settings: defaults (a missing key reads as its default), a neutral voice language, no hidden model.
for k in SETTINGS_RESET:
    s.pop(k, None)
for k in REALTIME_RESET:
    s.setdefault("realtime", {}).pop(k, None)
s["chat_voice_language"] = s["chat_voice_reply_language"] = "en"
c.execute("UPDATE settings SET value = ? WHERE key = 'settings'", (json.dumps(unhide(s)),))

# The demo's replies are answers, not long thought: with reasoning on, qwen3.5-4b thought for over
# 80 000 tokens (ten minutes of the card) on the merge-sort prompt (2026-11-23).
c.execute("UPDATE local_models SET params = json_set(params, '$.reasoning', 'off') "
          "WHERE model_id IN (?, ?)", (chat_model, traffic_model))
c.commit()
print("scrubbed: " + ", ".join(f"{k} {v}" for k, v in counts.items()))
PY
scripts/build-slot.sh scripts/dev-copy.sh start "$dir" "$addr"
token="$(cat "$dir.token")"

# The demo's models in the copy (the chat model, the traffic model and the CPU embedder), stopped:
# when a reply does not finish (it would go on streaming in the gateway) and on an interrupt.
# Says whether each stop worked.
stop_models() {
    local m rc=0 body
    for m in $(printf '%s\n' "$chat_model" "$traffic_model" "$embed_model" | sort -u); do
        body="{\"model\":\"$m\",\"action\":\"stop\"}"
        if [ "$m" = "$embed_model" ]; then
            # The aux class's rows carry the id without the alias prefix.
            body="{\"target\":\"aux\",\"model\":\"${m#embed/}\",\"action\":\"stop\"}"
        fi
        if curl -fsS -m 60 -o /dev/null -X POST "http://$addr/api/op/container" \
            -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$body"; then
            echo "$m is stopped" >&2
        else
            echo "STOPPING $m FAILED: stop it in the copy (http://$addr/models) or stop the copy (scripts/dev-copy.sh stop $dir)" >&2
            rc=1
        fi
    done
    return "$rc"
}
child=""
interrupted() {  # interrupted <exit status>: 130 for Ctrl-C, 143 for SIGTERM
    trap - INT TERM
    if [ -n "$child" ]; then kill -TERM "$child" 2>/dev/null || true; fi
    echo "interrupted: stopping the demo's models" >&2
    stop_models || true
    exit "$1"
}
trap 'interrupted 130' INT
trap 'interrupted 143' TERM
# A command in the background, waited for: a signal reaches the trap at once, not after it.
bg() { "$@" & child=$!; local rc=0; wait "$child" || rc=$?; child=""; return "$rc"; }
# A request that did not finish (curl's timeout, a refused connection) ends the demo, models stopped.
request_failed() {
    trap - INT TERM
    echo "a demo request failed (curl exit $1): stopping the demo's models" >&2
    stop_models || true
    exit 1
}

# Chat threads go through the UI, so they look like any other thread. The first reply starts the
# chat model's container, so the drive's container guard is off. A reply that does not finish in
# time or ends in an error ends the drive ("abort") and then the demo, with the models stopped.
if ! bg python3 scripts/ui-drive.py scripts/drive/readme-chat.json --var "CHAT_MODEL=$chat_model" \
    --base "http://$addr" --token-file "$dir.token" --size 1536x864 --dsf 1.25 --podman-guard ''; then
    trap - INT TERM
    stop_models || true
    echo "the demo threads could not be seeded (see above); the copy still runs" >&2
    exit 1
fi

# The copy inherits the source database's "Require API key"; the keyless traffic below would log 401s.
curl -fsS -X POST "http://$addr/api/op/settings_set" -H "authorization: Bearer $token" \
    -H 'content-type: application/json' -d '{"auth_enabled":false}' > /dev/null

base="http://$addr/v1"
chat() {  # chat <dialect> <prompt> <stream>
    if [ "$1" = openai ]; then
        bg curl -s -m 180 -o /dev/null -w "openai    %{http_code} %{time_total}s\n" "$base/chat/completions" \
            -H 'content-type: application/json' \
            -d "{\"model\":\"$traffic_model\",\"stream\":$3,\"max_tokens\":1024,\"messages\":[{\"role\":\"user\",\"content\":\"$2\"}]}" \
            || request_failed "$?"
    else
        bg curl -s -m 180 -o /dev/null -w "anthropic %{http_code} %{time_total}s\n" "$base/messages" \
            -H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' \
            -d "{\"model\":\"$traffic_model\",\"max_tokens\":1024,\"messages\":[{\"role\":\"user\",\"content\":\"$2\"}]}" \
            || request_failed "$?"
    fi
}
embed() {
    bg curl -s -m 180 -o /dev/null -w "embedding %{http_code} %{time_total}s\n" "$base/embeddings" \
        -H 'content-type: application/json' -d "{\"model\":\"$embed_model\",\"input\":[\"$1\"]}" \
        || request_failed "$?"
}
chat openai "Write a regex that matches ISO 8601 dates." false
chat anthropic "Give me three name ideas for a home server." false
embed "a lighthouse at dusk"
chat openai "Explain what a KV cache is in two sentences." true
chat anthropic "Rewrite this commit message to be concise: fixed the thing where the stuff broke" false
embed "vector search basics"
chat openai "List five uses for a Raspberry Pi." true
chat anthropic "What is the capital of Australia? One word." false
trap - INT TERM

# The start-up probe (dev-copy.sh asks /v1/models before auth is off) logs one keyless 401 that is
# not demo traffic: drop it. The Usage page's window ends at the current UTC hour, exclusive, so
# traffic from this hour would not show on it: file the rollups under the previous hour.
sqlite3 -cmd '.timeout 5000' "$dir/lmgw.sqlite" "DELETE FROM request_logs WHERE status = 401;
    DELETE FROM usage_hourly WHERE outcome = 'client_error';
    DELETE FROM usage_latency_hourly WHERE alias = '?';
    UPDATE usage_hourly SET bucket_utc = strftime('%Y-%m-%dT%H', bucket_utc || ':00:00', '-1 hour');
    UPDATE usage_latency_hourly SET bucket_utc = strftime('%Y-%m-%dT%H', bucket_utc || ':00:00', '-1 hour');"

echo "ready: scripts/readme-shots.sh $dir $addr (while the models are still resident)"
