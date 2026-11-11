# lmgw on a server: the Podman image, TLS, and accounts

**Draft, 2026-11-08, revised the same day, and revised again the same
day after the owner's answers to every open question (rulings 8–21).** How lmgw ships and runs
headless on a server, as a second release artifact beside the desktop RPM: a Podman image in which
lmgw runs its model, agent and build containers with a nested Podman, started by a Quadlet unit.
It covers the production start the image needs, TLS on the main listener, configuration without a
dashboard, the machine check inside a container, and accounts: several people who sign in with a
password and all administer the same gateway. The server image always has accounts; the desktop
gets them behind a setting that is off by default (ruling 17), and with it TLS on its main listener
too. "Decisions" separates the owner's rulings from this record's own calls; §11 has no open
question left. "Not yet" lists what this version leaves out, and nothing there is ruled out.
WP1 to WP10 are built (§13 says what each covers and what ran live).

The factual base is the spike in `spike/headless-image/` (its `REPORT.md`, `Containerfile`,
`entrypoint.sh`, `containers.conf`, `storage.conf`, `run.sh`): lmgw as root inside a rootless outer
container, starting llama.cpp containers with the inner Podman, the GPU through lmgw's own CDI
spec, without `--privileged`. This record does not re-derive what the spike measured; it cites it
as "the report".

Companion records, cited by short name:
- "principals §n": [principals, origins and mounts](2026-09-22-principals-origins-mounts-design.md)
  (one principal, one gate, the login routes, agent origins, host mounts);
- "machine check §n / Dn": [the machine check](2026-11-03-machine-check-design.md);
- "client-apps §n": [client apps](2026-10-06-client-apps-design.md) (device keys, `Chat`);
- "device link §n": [the device link](2026-10-14-device-link-design.md) (the device listener, its
  CA, mTLS);
- "calls §n": [WebRTC calls](2026-10-14-realtime-calls-design.md) (the UDP socket groups);
- "container builds §n": [container builds](2026-09-26-container-builds-design.md).

| § | Content |
|---|---|
| 1 | The production server start |
| 2 | Configuration: environment, pinned and seeded settings |
| 3 | The image, the volumes, the unit, the host |
| 4 | TLS and the public origin |
| 5 | Accounts and sign-in |
| 6 | Require API key, restart, the update notice |
| 7 | The machine check in a container |
| 8 | The spike's other findings |
| 9 | Secrets at rest |
| 10 | Building the image in CI |
| 11 | Open questions (none open) |
| 12 | Tests |
| 13 | Work packages |

**Today** (paths under `crates/lmgw-core/src/` unless they say otherwise):
- **No production start without the shell.** The only headless entry point is the dev example
  `crates/lmgw-core/examples/headless.rs`. It is always a dev instance (`:25-34`,
  `AppState::init_with(data_dir, true)` at `:91`), refuses to run without `LMGW_DATA_DIR` and on the
  installed data dir (`:65-82`), and moves a fresh dir off the production `container_prefix`
  (`ensure_dev_instance_safety`, `:106`, `:145-172`). Its own docs say "cargo examples are never
  included in an RPM build, so nothing else can reach this binary in production" (`:20-22`); the
  spike's image ships it, as a debug build. What a server start needs from it already exists:
  `server::run` (`server.rs:671`), the two-signal stop through `server::quit_signal`
  (`server/stop.rs:479`, SIGTERM and Ctrl-C) and `signal_exit_code` (`:498`), and the quit's
  teardown `runtime::lifecycle::shutdown` after the server returned (`headless.rs:127-133`).
- **The shell's start and quit.** The tray app reads `bind_addr` with a `127.0.0.1:8787` fallback
  (`src-tauri/src/main.rs:39-45`), marks debug builds and `LMGW_DEV` as dev (`:574-575`), checks the
  prefix (`:580`), binds before it builds a window (`:585-600`), offers **Restart gateway**
  (`:659`, `:794` → `src-tauri/src/gateway.rs:123`: stop and start the server in-process, the model
  containers kept warm, `main.rs:912-916`), runs the updater (`:886`) and runs the quit's teardown
  on every exit (`:918-950`). The settings row marked "restart to apply" is `bind_addr`
  (`crates/lmgw-ui/src/pages/settings/rows.rs:7-11`); `realtime.webrtc_enabled` and
  `realtime.webrtc_port` also apply at the next start and say so in their own rows and save
  sentence (`config/settings/webrtc.rs:6-18`, `crates/lmgw-ui/src/pages/settings/webrtc.rs:59,73,
  287`).
- **The quit's bounds.** `server::serve` returns within `STOP_WITHIN` (10 s, `server/stop.rs:71`).
  `lifecycle::shutdown` then first aborts a running benchmark (`runtime/lifecycle.rs:620`), bounded
  by `STOP_GRACE_SECONDS` (10 s, `runtime/registry.rs:150`) plus `vram.unload_timeout_seconds` (a
  setting, default 60, `config/settings_classes.rs:110`; `bench/runner.rs:796-800`), and then stops
  the managed containers within `SHUTDOWN_TIMEOUT` (15 s, `runtime/lifecycle.rs:58`). Worst case
  with the defaults: 95 s.
- **Plain HTTP only.** `serve` wraps a `TcpListener` in `conn::PlainListener` (`server.rs:725`)
  and logs `lmgw listening on http://{addr}` (`:726`), then the owner's login link with the durable
  `owner:dashboard` key in it, built from `net::primary_base_url` (`:731-736`), which is loopback
  for a wildcard bind (`net.rs:76-84`). Every URL builder writes `http://` (`net.rs:32-72`):
  `primary_base_url`, which also feeds the wiring view's `base_url` (`web/wiring.rs:98`) and an
  agent's `/agents/<id>/mcp` URL (`agents/service.rs:1713-1720`); `reachable_urls`, which feeds
  `/api/connect` (`web/api.rs:162-170`); and `dashboard_url`, a device `hello`'s dashboard address
  (`net.rs:94-118`, `web/chat_feed.rs:360-366`). The pairing form proposes
  `https://<address>:<devices port>` from the listener's setting or the interfaces
  (`devices/pairing.rs:76-127`), and every device's URL hosts go into the device listener's leaf
  certificates (`devices.rs:211-216`). The session cookie has no `Secure` (`web/session.rs:
  161-172`). The same-origin rule accepts only an `Origin` equal to `http://` + `Host`
  (`principal.rs:612-627`). The `/mcp` DNS-rebinding guard requires the `Origin`'s port to equal
  `bind_addr`'s, reads a portless `Origin` as port 80, and requires its host to be a loopback name
  or an agent origin (`mcp/ingress.rs:1225-1290`). Agent origins are `http://<id>.<suffix>:<bind
  port>` (`agents/service.rs:194-196`), and the agent proxy always sends `X-Forwarded-Proto: http`
  (`web/agent_proxy.rs:569-572`). Nothing reads an incoming `X-Forwarded-*`: the proxy drops a
  client's own and sets its own for agents (`web/agent_proxy.rs:73-90`).
- **Agent origins' suffix** defaults to `localhost` (`config/settings_classes.rs:185-187`), which
  every browser resolves to its own machine. A suffix that is one of the gateway's own host names
  or under one of their cookie parents is refused (`agents/service.rs:330-380`, against
  `net::own_host_names`, `net.rs:200`).
- **Outside a secure context** (a LAN address over `http`), the Chat's dictation is disabled
  (`crates/lmgw-chat-ui/src/chat_voice/controls.rs:143-152`) and copy to the clipboard is refused
  (`crates/lmgw-ui-kit/src/clipboard.rs:1-17`).
- **TLS exists on the device listener**: rustls with aws-lc-rs, TLS 1.3, ALPN `http/1.1`, no
  resumption (`devices/tls/server_config.rs:30-91`), lmgw's own CA (`devices/tls/ca.rs:169-170`:
  `pathlen 0`, no name constraints), an accept loop per socket (`server/device_listener/
  tls_listener.rs:26-98`), certificates chosen per local address (device link §2.2). rustls is
  declared with `default-features = false` and the features `aws_lc_rs`, `std`
  (`crates/lmgw-core/Cargo.toml:187`): its default `tls12` feature is not declared.
- **WebRTC calls** have two UDP socket groups (`realtime/calls.rs:86-100`): the plain group binds
  the HTTP listener's IP on `realtime.webrtc_port`, the HTTP port's number by default
  (`realtime/calls.rs:386`, `config/settings/webrtc.rs:31-34,49-51`); the device group follows the
  device listener. `Group::of` maps `Via::Plain` and `Via::Devices` and nothing else (`:95-100`). A
  save that makes the two groups collide is refused (`webrtc_port_collision`,
  `config/settings/webrtc.rs:85`).
- **Require API key** (`auth_enabled`) is off by default (`config/settings.rs:545`); off, an
  anonymous caller holds `Inference` (`principal.rs:159-163`).
- **Credentials.** Every credential is an `api_keys` row (principals §3.1), `name` UNIQUE
  (migration 0037). Owner and agent keys keep their plaintext (`config/keys.rs:58-66`); client and
  device keys are hash-only. The two owner rows are seeded after the migrations, on every start
  (`seed_owner_keys`, `agents/token.rs:443-500`, called at `state.rs:771`), by insert-or-nothing on
  the name (`store/keys.rs:143`); rotation is an upsert on the name that keeps the row's id
  (`store/keys.rs:169`); `key_create kind=owner` names its row `owner:<name>` and refuses a taken
  name (`web/api_settings.rs:1488-1545`). `key_reveal` hands back any owner row's plaintext
  (`web/api_settings.rs:1655-1700`); `key_delete` refuses only `owner:dashboard`, by name (`:1612`).
- **The session.** The cookie `lmgw_session` carries an owner key itself (principals §3.3,
  `from_cookie`, `principal.rs:577-596`), honoured for owner rows only, with a `Max-Age` of one
  year (`SESSION_MAX_AGE_SECS`, `web/session.rs:50`). Sign-out clears this browser's jar and nothing
  else (`web/session.rs:330`). The shell's window signs in through a single-use nonce
  (`LoginNonces`, `web/session.rs:56-151`, valid `LOGIN_NONCE_TTL` = 60 s), which always resolves to
  `owner:dashboard` (`:232-275`). Keys are compared by sha256 of the presented string
  (`config/snapshot.rs:723-728`). There are no accounts and no passwords.
- **Body limits.** `max_body_mb` bounds the `/v1` JSON router only (`body_limit_mw`,
  `server.rs:58-110`); the `/api/session` routes' `Json` extractor keeps axum's default of 2 MiB.
- **The self-admin read/write class** exists for the `lmgw__*` tool catalog only
  (`mcp/selfadmin.rs:466` → `ops::check_mode`, `ops/common.rs:453`); `/api/op/{name}` has no
  read/write class, and `CAPABILITY_TABLE` (`server.rs:292`) lists 55 non-`GET` `Admin` routes,
  `/api/op/{name}` and `/mcp/admin` among them.
- **Secrets at rest.** Upstream provider keys are plaintext in `upstreams.api_key`
  (`migrations/0001_init.sql:9`, `store/upstreams.rs:15`), the Hugging Face token in the settings
  JSON (`config/settings.rs:193`). A production start wraps the device CA's key under a Secret
  Service item (`wrap_key_store`, `state.rs:414-419`); with no Secret Service answering it keeps the
  key plain (0600) with a warn line and tries again at the next start (`devices/tls/trust.rs:
  231-240`). Each wrap generates a new wrapping key and adds it to the store, then prunes the old
  items (`key_file_for`, `trust.rs:211-243`; `:295-307`).
- **Machine check.** Its fix runs only from the app's own window (machine check D3), so a browser
  session already sees steps without a button. `machine/running.rs:55` words a root Podman "as
  root", `:103-108` gives subuid hints; the pre-fetch's metered check reads `/proc/net/route` and
  calls `/usr/bin/nmcli` (`machine/prefetch.rs:16-20`, `:59`), and when it cannot tell, counts the
  connection as not metered and logs why (`:192-195`).
- **Builds** keep their dir under the data dir in production (`backends/paths.rs:17-28`) and their
  buildah scratch and cache mounts under `/var/tmp` (`backends/run/mod.rs:259-265`,
  `backends/run/podman.rs:311-316`); `podman build` argv at `backends/run/podman.rs:436`.
- **Migrations are one-way**: a start runs the repairs (`store.rs:159`), marks the database
  (`:172`) and runs the migrator (`:173`), which refuses a database that carries a migration this
  build does not know (sqlx's `VersionMissing`): an older build does not start on a database a
  newer one migrated. Nothing locks the data dir against a second process.
- **CI**: GitLab CE, every job on the `docker` runner (`.gitlab-ci.yml:20-21`), `fedora:latest`,
  thin LTO for its builds (`.gitlab-ci.yml:70-77`), building the RPM with `ci/build.sh` (the UI
  first: `ci/build.sh:135`, then `cargo tauri build`) and publishing it to the project's generic
  package registry (`ci/publish.sh`). Public releases build on GitHub from `v*` tags with the
  workspace's fat LTO (`.github/workflows/release.yml`, `Cargo.toml:20-23`); the GitHub test job
  already had to drop to one compile job because two ran the runner out of memory . No
  job builds an image.

## Decisions

*The owner, 2026-11-08:*
1. **Servers get a container image, not distribution packages.** So the image has no self-updater
   (an update is a new image: Quadlet `AutoUpdate=registry`, `podman auto-update`), and the machine
   check has no root fix in it: pkexec, polkit and the root helper do not apply, and the host's
   prerequisites are the administrator's job.
2. **lmgw runs inside the container** and starts its model, agent and build containers with a
   **nested Podman** inside it, as the spike proved.
3. **The confined variant**: `--security-opt label=type:container_engine_t`, engine images in the
   inner store on a volume. The read-only shared host store (the "shared" variant, which needs
   `label=disable`) is not the design goal; at most a later option for a second instance on a
   desktop.
4. **User management and sign-in are part of this design** (replacing the first brief's "out of
   scope").
5. **Accounts administer the same gateway** (option (a) of the draft's fork): several accounts,
   each with its own sign-in and its own credentials, all managing one gateway's data. Not real
   multi-user. The app's direction does not change; the goal is to make it simpler to use as a gateway in a shared environment.
6. **Every account is a full admin.** No read-only or other roles.
7. **The desktop RPM and the tray app stay as they are**; the image is a second release artifact.

*The owner, 2026-11-08, answering the draft's open questions (the question's old number in
brackets):*

8. **TLS** [Q1]: native TLS on the main listener with the administrator's own certificate, read
   from a read-only mounted directory and reloaded by itself, comes first; trusted-proxy mode
   second (WP5). lmgw's own CA is not for browsers. Built-in ACME is not yet. The main listener
   speaks TLS 1.2 and 1.3 (§4, D7, D8).
9. **Require API key is on by default in the image** [Q2], seeded as an ordinary setting the
   administrator can switch off on the dashboard. The desktop default stays off (§6.1).
10. **A rootless user unit for a dedicated service account with linger** [Q3] (§3.3). A rootful
    system unit is not yet, and not verified.
11. **No registry for now** [Q4]: CI builds the image as an OCI archive artifact, which the
    administrator loads by hand (`podman load`). Publishing to a registry (ghcr.io, the GitLab
    container registry) is not yet and is not yet; nothing publishes automatically.
    Consequence: `podman auto-update` has nothing to follow until a registry exists, so ruling 1's
    "an update is a new image" means, for now, "load the new archive, restart the service"
    (§3.3, §6.3, §6.4), and the `AutoUpdate=registry` route waits for a registry.
12. **Provider API keys at rest get a design of their own** [Q5], covering the desktop and the
    server alike. This design only warns, in the server docs, that the data volume holds them in
    cleartext and must be protected like a password file (§9).
13. **The newer-version notice** [Q6]: one line on Settings → About and in the log, from the
    existing feed, with no download, and a setting that switches the check off (the existing
    `update_check_enabled`, §6.3).
14. **A database copy before a migrating release** [Q7], in container mode: `VACUUM INTO` before the
    first migration runs, listed with its size on Settings, never deleted by lmgw. Rolling back is a
    manual step, never automatic (§6.4).
15. **The container's log never prints a durable key** [Q8]: while account 1 has no password it
    prints a single-use set-password link, afterwards only the `lmgw-server login-link` command.
    The desktop keeps today's log line (§5.3).
16. **No hard lockout** [Q9]: growing delays, known browsers and visible counts (§5.4). **A password
    has at least 12 characters** (§5.2).
17. **Accounts are a desktop setting, off by default** [Q10, changed from the draft's
    recommendation]. The server image (container mode) always has accounts on. With the setting off
    the desktop behaves exactly as today: one owner, the tray's nonce sign-in, today's cookie,
    today's log line. With it on, the desktop gets the same accounts, sessions, audit log and
    sign-in UI as the image (§5.11, D25, D29).
18. **The audit log's scope** [Q11]: every non-`GET` `Admin` route from `CAPABILITY_TABLE`, written
    at the gate; writing `lmgw__*` calls; sign-in, sign-out and failed attempts; account and key
    changes. Secrets redacted. It is attribution among admins, not tamper evidence. A change of the
    log's retention is itself audited first (§5.8). The retention default is not ruled; §5.8 keeps
    the draft's recommendation. **The Activity list** (the owner, 2026-11-08, ruling 18's
    follow-up: "Please include the activity list"): the audit log on the Accounts card, filterable
    by account (everyone, one account, or the entries of no account), as the original draft's §5.8
    had it.
19. **Sessions** [Q12]: a `sessions` row per sign-in wherever accounts are on (the image always,
    the desktop when the setting is on), with a visible lifetime setting; 30 days is the draft's
    default. The desktop with accounts off keeps today's cookie (§5.2).
20. **No password blocklist** [Q13, Q14]. NIST SP 800-63B revision 4 makes a blocklist check a SHALL
    (§3.1.1.2), so this departs from it on purpose. No composition rules, NFKC normalization, length
    counted in code points, and the named 2 MiB `SESSION_BODY_LIMIT` stay as drafted (§5.2).
21. **An admin may set another account's password directly** [Q15]: audited, and it ends that
    account's sessions. A set-password link for one's own account stays refused (§5.5, §5.6).

*Standing rules that bind this design:* no hidden or guessed limits (every bound has a real source
or is shown); confirm, then do; no config keys as labels; binary units; nothing here changes the
`/v1` wire or any OpenAI/Anthropic shape; publishing anything to a registry is the owner's call.

*Design calls (this record's own):*

- **D1 — A binary of its own, `lmgw-server`.** A new crate `crates/lmgw-server`, no Tauri, no
  WebKit; the UI comes embedded through `lmgw-core` as today (`web/ui.rs:19-21`). It is a
  production start (`AppState::init_with(data_dir, false)`) unless `LMGW_DEV` is set. It starts
  only where `LMGW_DEPLOYMENT=container` (set by the image) or `LMGW_DEV` is set, and otherwise
  refuses with one sentence: a production start on a desktop would share the user's Podman, image
  store and containers under the production prefix with the tray app. Why not promote the example:
  the example's whole point is its dev guards, and the desktop's dev workflow keeps using it.
- **D2 — Container mode is explicit.** `LMGW_DEPLOYMENT=container` in the image's `ENV`, read once
  into `AppState` (`Deployment::{Desktop, Container}`). `/run/.containerenv` is only logged beside
  it, never decides. What this record adds because lmgw runs in a container asks this flag; what
  is about accounts asks whether accounts are on, which container mode implies (D25).
- **D3 — Pinned and seeded settings** (§2). Pinned: the environment wins on every start and the
  dashboard shows the value read-only. Seeded: the environment fills a fresh data dir once, then the
  stored setting rules.
- **D4 — The production `container_prefix` is right inside the image.** The inner Podman's store
  and its containers are private to the outer container in the confined variant, so nothing else
  lists or removes containers there; `lmgw` stays the default and is not pinned. The dev prefix
  rules (`config/settings.rs:795`) are unchanged.
- **D5 — catatonit stays PID 1** (the report: with lmgw as PID 1, conmon and pasta stayed zombies).
  `lmgw-server` logs a warning when it finds itself PID 1. Making lmgw a subreaper is not needed
  while the image ships an init.
- **D6 — With TLS, two listeners**, on both deployments (ruling 17 needs TLS on the desktop, §4.7).
  The TLS listener takes the public address. In container mode the plain listener then moves to
  the container's loopback, where agents (`gateway_access`'s `pasta:-T,<port>` path,
  `agents/container.rs:1011-1040`), the health check and the CLI (§5.7) reach it; without TLS it
  binds `0.0.0.0` as the spike did. On the desktop the plain listener stays where `bind_addr` puts
  it (loopback by default), because the app's own window loads it. The WebRTC socket groups follow
  (D22).
- **D7 — The main listener speaks TLS 1.2 and 1.3** (ruling 8) with ALPN `http/1.1`, the provider
  of `devices::tls::provider()`, and `Cargo.toml` declares rustls' `tls12` feature explicitly
  rather than relying on another crate unifying it in. TLS 1.2 is there for clients whose TLS stack
  predates 1.3 (older OpenSSL, Java or .NET runtimes). The device listener stays TLS 1.3 only with
  client certificates, unchanged.
- **D8 — The certificate is read from files and reloads by itself** (ruling 8). In the image the
  pair is bind-mounted read-only from a dedicated host directory
  (`Volume=%h/lmgw-tls:/etc/lmgw/tls:ro,z`), which the host's ACME client or deploy hook writes; on
  the desktop the two paths are ordinary settings pointing at files the user's account can read
  (§4.7). Not a Podman secret: Podman copies a secret
  into the container when it creates it (`podman-run(1)` `--secret`; `podman secret create
  --replace` changes only newly created containers), so a renewed secret reaches lmgw only at the
  next start. The resolver keeps the loaded pair and the files' modification times; an accept that
  finds either changed loads both and checks that the key matches the certificate; a failed load
  keeps the old pair, logs, and shows on Settings, and the next accept tries again. An
  administrator who prefers secrets can use them: renewal is then `podman secret create --replace`
  plus `systemctl --user restart lmgw`, and the docs say so.
- **D9 — One public origin.** `public_url` (e.g. `https://lmgw.example:8443`) is what every URL lmgw
  prints or hands out to a person or another machine is built from (§4.3); origin checks compare
  against the request's own `Host` and the scheme of the listener it came in on. URLs that lmgw
  dials itself while carrying a credential (an agent's `/agents/<id>/mcp`, which carries the
  `owner:dashboard` bearer) stay on the plain loopback listener.
- **D10 — Accounts are principals on the existing model** (§5.1): an `accounts` row, and its
  credentials are `owner`-kind keys belonging to it. No parallel credential system. Account 1 is
  made by `seed_owner_keys`, not by the migration; door keys are named by account id, never by
  login.
- **D11 — The first account is the door.** Its door key is `owner:dashboard`; the desktop shell's
  nonce signs it in as today (into a session while accounts are on, §5.11), and in container mode
  `lmgw-server login-link` does (§5.7). It cannot be disabled or deleted, as `owner:dashboard`
  cannot today (principals §3.1).
- **D12 — argon2id, RFC 9106's second recommended parameter set** (t = 3, p = 4, m = 64 MiB), PHC
  strings, rehash at sign-in when the parameters changed. Verification on the blocking pool, at
  most `available_parallelism()` at once (§5.10), the number and the memory it can take logged at
  start and shown on the Accounts card.
- **D13 — No account enumeration.** A sign-in name that is unknown, disabled, has no password or
  is no sign-in name at all (it fails §5.2's alphabet or D60's 64 characters) runs a verification
  against a fixed dummy hash and answers the same `401 login_invalid` in the same time as a wrong
  password: one store lookup for every name (an enabled account with a password, by its folded
  name), the dummy hash made at the start rather than by the first unknown name, and a failure's
  bookkeeping (`failed_logins`) written after the answer, with no snapshot reload. The throttle
  (§5.4) keys on the sha256 of the name as presented, folded as logins are, existing or not, so a
  `429` says nothing about whether the name exists either, and the map never holds a presented
  name; the failure's `debug` line names the hash's first 12 characters and the name's length.
- **D14 — Throttling shows itself** (§5.4): `429 login_throttled` with `Retry-After` and a sentence;
  failed sign-ins per account shown on the Accounts card.
- **D15 — Set-password links, no e-mail.** A new account and a password reset both get a one-time
  link the admin hands over; the token sits in the fragment (`#token=`), so it reaches neither a
  server log nor a `Referer`; the link's validity is printed with it.
- **D16 — The CLI goes through the running gateway** (§5.7): `podman exec` into the running
  container, reading account 1's door key from the database and calling the loopback listener.
  With the service stopped, the same commands run in a one-off container on the data volume and
  write the database themselves, under the data-dir lock (D28).
- **D17 — Account ops are `/api` only**, absent from the `lmgw__*` plane, as the key ops are
  (principals §3.12). So is the desktop's accounts switch (D29).
- **D18 — Disabling an account ends its own owner keys and sessions only.** Client and device keys
  it created keep working (they are the gateway's); the confirmation says so.
- **D19 — The image's toolkit repository checks packages**: `gpgcheck=1`, `includepkgs=
  nvidia-container-toolkit-base`, NVIDIA's key pinned, as machine check D5 does for the desktop
  (the spike's repo file has `gpgcheck=0`).
- **D20 — The unit's stop timeout is derived** from the four bounds of a quit (§3.3); the unit
  mirrors it into `LMGW_STOP_TIMEOUT`, and lmgw says on Settings and in the log when the stored
  `vram.unload_timeout_seconds` makes its quit need more than that.
- **D21 — `/var/tmp` is a volume**, so build caches and scratch survive the container being
  recreated, which Quadlet does on every start (`podman run --rm --replace`) and so on every update.
- **D22 — WebRTC on the TLS listener**, on both deployments. `Via::Tls` gets a socket group of its
  own (calls §3.1), `Group::Tls`: UDP on `tls.listen`'s IP, on a new key `realtime.webrtc_tls_port`,
  whose unset value is the TLS port's number (8443/udp), as `realtime.webrtc_port`'s is the HTTP
  port's. A key of its own because on the desktop both groups can be bound at once (the plain
  listener stays where `bind_addr` puts it, D6), and one port for two groups would collide.
  In container mode with TLS the plain listener is loopback-only, and its plain group is not bound
  (nothing outside the container can reach a loopback UDP socket). `webrtc_port_collision` checks
  the TLS group against the plain and the device group as it checks those two today. The unit
  publishes UDP for the groups it binds (§3.3).
- **D23 — *Retired*: now ruling 15** (no durable key in the container's log; §5.3 has the lines).
- **D24 — *Retired*: now ruling 19** (sessions wherever accounts are on; §5.2).
- **D25 — Two gates: "accounts on" and container mode.** *Accounts on* is container mode, or the
  desktop's `accounts.enabled` setting (ruling 17, D29). It gates every account surface: the
  sign-in name and password fields, the set-password banner and page, the Accounts card, sessions
  and known browsers (ruling 19), the throttle, the door-key refusals, the audit log (§5.8), and
  the session-only cookie rule (§5.2). *Container mode* gates what is about running in a container:
  D1's refusal, pinned and seeded settings (§2), the plain listener's move to loopback (D6), the
  stop budget (D20), the log's sign-in lines (ruling 15), the machine check's words (§7), the
  migration copy (ruling 14, §6.4: an update path that only the image has, unrelated to accounts),
  and the CLI (§5.7). TLS and the public origin (§4) are gated by neither: they are listener
  settings on both deployments, pinned in container mode, edited on the dashboard on the desktop
  (§4.7). The schema is the same everywhere (the store is shared), and account 1's row exists on
  the desktop while accounts are off, unseen; with the setting off nothing a desktop user sees or
  does changes (ruling 17).
- **D26 — Passwords only over a secure transport**, wherever accounts are on. A password sign-in,
  a password change and a set-password submit are refused with `403 login_insecure` when the
  request's scheme is `http` and its peer is not loopback, naming what fixes it (TLS, or proxy
  mode). On the desktop that is why the TLS settings exist there too (§4.7). For WP6: a request
  that came through a trusted proxy (`Conn.proxy` set, D51) is never loopback here, whatever
  client address the proxy forwarded; the exemption reads `Conn::is_direct_loopback`.
- **D27 — Agent origins seen from other machines.** Wherever the dashboard is reachable from other
  machines (container mode; a desktop with a TLS listener or a non-loopback `bind_addr`), the
  default suffix `localhost` resolves to the remote browser's own machine, so Settings → Network &
  access and every agent's card say "agent pages are unreachable from other machines: the agent
  origin suffix is `localhost`". A working setup needs a suffix outside the dashboard's registrable
  domain (the existing refusal enforces it once `public_url`'s host is one of the gateway's own
  names, §4.3), a wildcard DNS record for it, and a certificate that covers it.
- **D28 — The data dir is locked.** `lmgw-server` holds an exclusive `flock` on
  `<data dir>/lmgw-server.lock` while it runs; the CLI's offline path takes the same lock and
  refuses, naming the holder's pid, when it cannot. A flock on a file in a shared volume holds
  across containers on one host.
- **D29 — The desktop's accounts switch is an op with consequences, not a plain field** (ruling
  17, §5.11). `accounts.enabled`, off by default, is written only by `accounts_switch { enabled }`
  (`/api`, `Admin`, absent from the `lmgw__*` plane, D17), after a confirmation that says what
  follows. Every settings save path refuses the key with one sentence naming the switch (the same
  shared check that refuses pinned fields, §2.2), because a field write would skip what the switch
  does to sessions and keys. In container mode the row reads "always on in the server image" and
  the op refuses. Turning it off keeps every account, password hash, known browser and audit row
  and deletes nothing but the sessions, which it ends; owner keys of accounts other than account 1
  are paused, not deleted, until accounts are on again (§5.11).
- **D30 — The machine check's words in the server image** (WP3, §7).
  - Each step is said twice: a short "On the host: …" step in the sentence, and the commands
    (`nvidia-ctk cdi generate …`, `systemctl --user restart lmgw`, `podman load -i …`) in the
    item's detail.
  - A driver change is placed by its versions. Libraries newer than the running module are a host
    that has not restarted since its driver update: "On the host: restart the machine to load the
    new driver (module X, libraries Y)", verdict `needs_restart`. A module newer than the
    libraries is a CDI spec the container started with before the update: the spec's refresh and
    the service's restart, verdict `needs_steps`. NVML's own `version_mismatch`, with no versions
    to place it, names both steps (`needs_steps`).
  - A card that stopped answering during a spec write is the machine's restart on the host; a
    restart of the lmgw service alone does not bring it back (`needs_restart`).
  - The host's driver unreadable or NVML failing otherwise: "check that nvidia-smi answers" on the
    host, then the spec and the service's restart. A spec folder open to others or a spec file
    that could not be saved: the service's restart, which recreates the container (its
    `/run/user/0` is the container's own); a file that keeps failing, a current image.
  - A uid map without a range is a problem even when `podman info` answers, since the start of an
    image with other users fails. A map whose root is not host uid 0 is said as "its root is host
    uid N", never as rootless: a rootful Podman with a user namespace of its own maps root the same
    way. Rootful only when root maps to 0.
  - An AMD card without its render node gets the started-without-the-card sentence, as NVIDIA's
    does, and §1.2 then reads "the graphics card comes first", not "the driver comes first".
  - Each place in §1.5 is named by the mount it lies on, from `/proc/self/mountinfo`: the volume
    when the deepest mount holding it is one, the mount when it is another (a host folder at
    `/data/models` is not the data volume), and a place on no mount says in its detail that what
    is kept there goes when the container is recreated.
  - The Overview's and the Chat's check card is headed "This server", and its banner names "the
    server lmgw runs on", wherever the browser is.
- **D31 — An explicit `update_check` while the check is off still asks** (§6.3). Switching
  `update_check_enabled` off clears the notice in that save, and the background check asks nothing
  while it is off and leaves the notice alone. `update_check` is the administrator's own request:
  it asks the feed and keeps what it found, off or on.
- **D32 — Unset TLS variables** (WP4, §2.2). The TLS pair and
  the public URL are cleared when their variable is unset (the image's default is none): the unit
  is where TLS is switched on and off, and a pair kept from an earlier start would serve TLS the
  unit no longer names. `tls.listen` is kept, or `0.0.0.0:8443` when none is stored. The bind
  address keeps WP1's rule (kept, or the image's `0.0.0.0:8787` on a data dir this start seeds),
  and then, while its address is one of the two the image itself gives it (`0.0.0.0` or
  `127.0.0.1`), follows TLS on its port (D6): loopback with TLS on, every address with it off. Any
  address other than `0.0.0.0` and `127.0.0.1` stays as stored, and so does one `LMGW_BIND_ADDR`
  pins. The WebRTC ports are kept, as WP1 keeps `realtime.webrtc_port`.
- **D33 — Agent origins under the suffix `localhost` stay on the plain listener** (`http`,
  `bind_addr`'s port). That suffix resolves to the browser's own machine, so its agent pages only
  ever work on this one, where the app's window loads the plain listener and no certificate covers
  `*.localhost`; any other suffix follows the public origin's scheme and port (§4.3), and
  `LMGW_APP_ORIGIN`, `X-Forwarded-Host` and the App tab's frame with it. The scheme's default port
  is not spelled in an origin (`https://board.lmgw-apps.example`, as a browser's `Host` writes it).
  `X-Forwarded-Proto` is the scheme of the listener the request came in on.
- **D34 — The reload's stamps and its retry.** Both files' modification time, change time (with
  its nanoseconds), size, inode and device, read through symlinks, are compared at each accepted
  connection. A failed reload is tried again once either file changes again, not at every
  connection: a deploy hook that renames the certificate in first and the key a moment later is
  picked up at the key's write, a fix that only changes a file's mode, owner or label (`chmod`,
  `chown`, `restorecon`, `chcon`) moves the change time and is picked up too, and a pair that stays
  broken is not re-read and re-logged per connection. Files that return to the pair that serves
  clear the error. The stat and the load run on the blocking pool, one check at a time; a
  connection accepted while a check runs is served the pair loaded now without waiting, so a
  stalled mount holds one blocking thread and the handshake that started the check (within its
  bound), never the runtime's workers. A renewed pair is served "from the next connection" in
  that sense: the first connection whose check sees it (a test waits for no check to be in
  flight before the connection it asserts on).
- **D35 — A pair that does not load at the start.** The server's start ends on it, naming the file,
  as on a failed bind (§1 step 6). The desktop serves on without the TLS listener, logs it, shows it
  on Settings → Network & access → TLS, and the next start (Restart gateway) tries again; until
  then every URL is built on the plain listener (D45).
- **D36 — The handshake bound is a setting**, `tls.handshake_timeout_s`, default 10 s (the device
  listener's default for its own bound), at least 1, on the TLS card. Each handshake runs on a task
  of its own; their number is not capped beyond the process's file descriptors, as the plain
  listener's connections are not (the owner's no-hidden-limits rule; alternative not taken yet: a cap of
  concurrent handshakes, shown on the card, is the alternative). Running out of descriptors
  surfaces: the accept loop logs it once per episode, retries every second, shows it on the TLS
  card until an accept works again ("the TLS listener cannot accept connections since …", with
  the hint to raise `LimitNOFILE=`), and logs when it accepts again.
- **D37 — The names check warns, never refuses.** At each load the leaf's DNS and IP subject
  alternative names are read by kind (RFC 6125: a wildcard is the whole left-most label for exactly
  one label, under a parent of two labels or more, so `*.example` covers nothing; an IP only
  matches an IP entry, and an IP address written as a DNS entry is no IP entry); a `public_url`
  host they do not cover, and a suffix other than `localhost` whose exact `*.<suffix>` is not
  among them (or that is a single label, which no wildcard can cover), are each a warning on the
  TLS card and in the start's log. The card and the log warn too of a leaf that ended or ends
  within 14 days (with its date; the 14 days are a display choice, nothing is refused by them), a
  private key file its group or everyone can read (with its mode), a `public_url` that is `https`
  while no TLS listener serves and no trusted proxy is set (with one, the proxy serves TLS for it,
  D52) or `http` while one does, and a failed accept (D36).
- **D38 — `/mcp`'s guard on both planes.** The `Origin`'s scheme must be the listener's; its port
  (a portless one is 80 or 443 by its scheme, where today's code left it to the host test) is
  paired with its host: a loopback name with the listener's own port only, `public_url`'s host
  with `public_url`'s port or the listener's, an agent origin with the scheme and port it is
  published on (D33). `public_url`'s host joins the loopback names on `/mcp` and on `/mcp/admin`;
  agent origins stay aggregate-only.
- **D39 — `EXPOSE` names every listener the image may serve** (8787, 8443, 8788, each TCP and UDP),
  not the shipped unit's `PublishPort=` lines (§3.1 said equal): behind TLS the plain 8787 is the
  container's loopback and not published. The packaging test reads the unit's `Environment=` lines
  as the start reads them, applies them to a fresh data dir and checks that `PublishPort=` is
  exactly what that start binds, and that each is `EXPOSE`d under its own number.
- **D40 — The shipped unit is the TLS unit** of §3.3, with `lmgw.example` as the placeholder the
  administrator replaces; its header says how to run without TLS (drop the three variables and the
  `~/lmgw-tls` volume, publish 8787 in place of 8443).
- **D41 — The TLS section and `public_url` are dashboard-only,** like `bind_addr`: the full save
  takes them, `settings_set` and `lmgw__settings_set` do not. `realtime.webrtc_tls_port` goes
  through every save path as `realtime.webrtc_port` does. `public_url` is stored as the origin it
  reads as (`https://LMGW.example:443/` is saved as `https://lmgw.example`) and applies at once,
  since every builder reads it per request; the listener's own settings apply at the next start.
  A save of paths whose pair does not load now goes through with a note saying so. A `public_url`
  save that makes the stored agent origin suffix shadow the dashboard is refused with
  `origin_suffix_shadows_gateway`, as the suffix's own save is; a shadow the stored settings had
  before is the boot warning's (principals §4.1).
- **D42 — `/api/connect`'s bases**: `public_url` first (labelled "public URL"), then the serving
  TLS listener's addresses, then the plain listener's. In the server image a loopback-only plain
  listener is left out behind them (it is the container's own loopback); on the desktop it stays,
  because this machine's clients keep using it. A remote
  device's `dashboard_url` without `public_url` but with a TLS listener is today's rule on the TLS
  listener (`https`); "this computer" keeps the plain loopback its window loads. The pairing
  proposal's public entry is labelled "public URL".
- **D43 — Each scheme reads only its own cookie.** On `https` only `__Host-lmgw_session` counts
  and on `http` only `lmgw_session`: a plain cookie is sent to every port and scheme of a host, so
  on the TLS listener it is nobody's. The device listener refuses either name as before.
- **D44 — The calls status lists the TLS group once a start configured it**; a gateway without a
  TLS listener shows the two groups it always had. Behind TLS in the server image the plain group
  is not bound only while the plain listener is on loopback; an address pinned off loopback keeps
  its group.
- **D45 — URLs follow the TLS listener that serves, not the stored one.** Every builder of §4.3
  (`public_base_url`, `public_scheme_port`, `device_dashboard_url`, `/api/connect`, agent
  origins, the login line) reads the TLS part from what a start brought up: the bound address and
  the settings it started with, published on the snapshot while it serves and cleared when it
  stops or the next start serves none. A desktop's TLS save and a pair that did not load (D35)
  leave every URL on the plain listener; `public_url` still applies at once. The agents' origins
  move at the start that brings TLS up or down, which stops the running app containers then
  (they hold the old origin in `LMGW_APP_ORIGIN`), not at the save. The login line names the
  plain address as bound (a port of 0 as the OS chose it) or the bound TLS address.
- **D46 — Two listeners on one TCP port are refused.** The plain listener, the TLS listener and
  the device listener on one port on overlapping addresses (equal, or one a wildcard of the
  other's family) are refused by every save that touches one of them, at the container start
  (naming the unit's variables) and by `lmgw-server`'s environment when `LMGW_BIND_ADDR` and
  `LMGW_TLS_LISTEN` meet; the second bind would fail at the start otherwise. A port of 0 is never
  one port. Every refusal names the rows on Settings, as the UDP check's does.
- **D47 — `trusted_proxies`: a list of networks, applied at once** (WP5, §4.5, §4.7). Each entry is
  an address or a CIDR network, stored as the network it is (`127.0.0.1` as `127.0.0.1/32`, blank
  and repeated entries dropped); a network with bits set past its prefix (`192.0.2.5/24`) is refused
  naming both readings. A peer is compared as its canonical address, IPv4 with IPv4 networks only:
  an IPv4-mapped peer is its IPv4 address, `::ffff:192.0.2.0/120` is read as `192.0.2.0/24`, and no
  other IPv6 network, `::/0` included, holds an IPv4 peer. On the desktop it is a dashboard setting
  on the TLS card that every request reads, so a save applies from the next request on, without a
  restart; like `public_url` (D41) only the full save takes it, `settings_set` and
  `lmgw__settings_set` do not. In container mode `LMGW_TRUSTED_PROXIES` (commas or spaces between
  entries) pins it, and an unset variable turns proxy mode off, as an unset TLS pair or public URL
  clears them (D32): a peer the unit no longer names is trusted no more. An empty list is off.
- **D48 — One header family per request.** From a trusted peer lmgw reads `X-Forwarded-For`,
  `-Proto` and `-Host`, or RFC 7239's `Forwarded`, never a mix; a request that carries both is
  refused, `400 forwarded_headers_invalid`, naming why. A proxy that writes one family usually
  passes a client's own of the other through untouched (nginx passes `Forwarded`, a
  `Forwarded`-writing proxy passes `X-Forwarded-*`), so preferring either family would let a client
  choose its own address and scheme behind the other kind of proxy; the refusal hurts only the
  client that sent the extra family. Alternative not taken yet: a setting naming the family the proxy
  writes is the alternative.
- **D49 — The client walk.** The chain is `X-Forwarded-For`'s hops across all its lines, or the
  `for` of each `Forwarded` element; it is walked from the right, trusted hops skipped, and the
  first hop that is not a trusted proxy is the client. With every hop trusted the client is the
  left-most hop. Once a trusted peer sends any forwarded header, a chain with no hops (only
  `X-Forwarded-Proto` or `-Host`, an `X-Forwarded-For` of empty elements, `Forwarded: ,,`) is the
  client unknown too: the request crossed the proxy, and its client is not the proxy. A trusted peer
  that sends no forwarded header at all talks to lmgw directly and stays itself (the health check
  and the CLI inside the pod). A hop the walk reaches that names no address (RFC 7239's `unknown`,
  an obfuscated `_name`, an element without `for`, text that is no address) ends the walk with the
  client unknown, written as `0.0.0.0:0` (`forwarded::UNKNOWN_CLIENT`): never the proxy's address in
  its place, so a client hidden by a trusted proxy is not loopback; the agent proxy then sends no
  `X-Forwarded-For`. Hops to the left of the client are the client's own text and are never read, so
  text that does not parse there refuses nothing: `X-Forwarded-For` is split on commas as bytes, and
  an element that is not text is one more hop that names no address. A `Forwarded` value is read
  with any byte that is not text replaced, so one inside a quoted string is a hop that names no
  address, and one anywhere else does not parse. Ports (`192.0.2.5:4711`, `[2001:db8::5]:4711`) are
  kept, an obfuscated port is 0. A `Forwarded` header whose syntax does not parse (an unclosed
  quote, a parameter twice in one element) is refused, since its elements cannot be told apart.
- **D50 — The scheme and the host are one value each.** `X-Forwarded-Proto` and `X-Forwarded-Host`
  may repeat one value (a chain that appended what it agrees on) but are refused when the values
  differ; the scheme is `http` or `https` (`ws` and `wss`, which some proxies write for an upgrade,
  read as those two), the host an authority without user information (`lmgw.example:8443`,
  `[2001:db8::5]:8443`). With `Forwarded` they are the `proto` and `host` of the element the walk
  stopped at, the one the proxy that received the client's request wrote. Absent, the listener's
  scheme and the `Host` the proxy sent stand. A request with more than one `Host` header is
  refused on the plain and the TLS listener, trusted peer or not (`400 host_invalid`, RFC 9112
  §3.2). `X-Forwarded-Port` and `X-Forwarded-Prefix` are not read yet: a proxy on a port other
  than its scheme's default puts the port in the host it forwards (nginx: `$http_host`, not
  `$host`), or the browser's `Origin` does not match.
- **D51 — Written in where every reader already looks.** One layer, `server::forwarded::edge`,
  outermost on the plain and the TLS listener's router (outside the trace, never on the device
  listener), reads a trusted peer's headers and writes the result into the request: `Conn.peer`
  becomes the client and `Conn.proxy` holds the TCP peer, `Conn.forwarded_scheme` the scheme
  (`Conn::scheme`, `scheme_of`, and `RequestCtx.scheme`, which the gate fills), and the `Host`
  header, and an HTTP/2 URI's authority, the forwarded host. Then it strips every forwarded header
  (`X-Forwarded-For`, `-Proto`, `-Host`, `-Port`, `-Prefix`, `Forwarded`) from every request,
  trusted or not, so nothing past it can read a client's own. So the cookie choice and
  `same_origin` (D43), `/mcp`'s guard (D38), the realtime handshake's foreign-origin test, the
  agent origins' dispatch and the agent proxy's `X-Forwarded-*` (D33), `/v1/responses`' own-server
  test and the request span (`client`, `scheme`, `proxy`) see the forwarded values without a
  check of their own. The realtime handshake's foreign-origin test compares the `Origin`'s scheme
  with the request's exactly, as `/mcp`'s guard does. A later reader of a client address reads
  `Conn.peer`: WP6's throttle reads the client, not the TCP peer. A request with `Conn.proxy` set
  is never loopback, for D26's exemption or any later loopback rule, whatever client address the
  proxy forwarded: it crossed a network hop. `Conn::is_direct_loopback` (a loopback client and no
  proxy) is the test such a rule reads. The edge stands outside the trace, so its own refusals
  (`host_invalid`, `forwarded_headers_invalid`) are written in a request span of their own, at
  the trace's level (debug).
- **D52 — URLs come from `public_url`, never from a forwarded host.** Every builder of §4.3 stays
  what it was (D45); in proxy mode the administrator sets `public_url` to the address the proxy
  serves, and while it is empty the TLS card and the start's log warn that links are built from
  lmgw's own listener. `/mcp`'s guard does not take the forwarded host as the gateway's own: a
  proxy that forwards any `Host` would otherwise carry a DNS-rebinding page's origin through. An
  `https` `public_url` without a TLS listener is not warned of while a trusted proxy is set (D37).
- **D53 — What proxy mode shows.** The start logs the trusted list. Per main listener and start,
  the first request's peer, after a loopback one (the health check, the CLI) the first from
  outside loopback, and the first from a trusted peer (so a scanner that reaches the port first
  cannot hide the proxy's line) are logged and listed on the TLS card, each saying whether it was
  trusted. They are noted while proxy mode is on, and in container mode always, where the line is
  how the administrator learns the address to trust; a desktop with proxy mode off notes none. A
  save that changes the list starts the record again, and its lines say "since the trusted
  proxies were last saved". A request whose lines are all taken reads one atomic per listener,
  not the record's lock. A
  trusted peer that is the connection's own local address while not loopback is warned of (§4.5,
  the outer pasta's case), and so is a list holding a `/0` and a stored entry that does not parse.
  The first refused request of a start is a warning in the log, later ones are counted on the card
  with the last one's time, peer and reason, so a client cannot fill the log with refusals.

- **D54 — Three schema changes to §5.1** (WP6). `sessions` has
  an `id` of its own beside `token_hash` (now `UNIQUE`): `session_end { id }` and the principal's
  `session` name a session by it, so neither ever holds the hash. `account_tokens.account_id` is
  `UNIQUE`, which is what "a second link for the same account replaces the first" (§5.5) is in
  the table: the insert is an upsert on it. `accounts.id` and `sessions.id` are `INTEGER PRIMARY
  KEY AUTOINCREMENT`: a plain rowid hands the newest deleted id out again, so a deleted account's
  door-key name `owner:account-<id>`, "made by" and a session id the card still shows would come
  back as someone else's.
- **D55 — The current password guards a session, not a key, and only against a walk-up.**
  `account_password_set` for one's own account asks for `current` only when the caller is a
  session; a bearer of one of the account's own owner keys sets it without, and the self-link
  refusal (§5.5) likewise applies to sessions only. Why: `lmgw-server account set-password owner`
  (§5.7) calls as account 1's door key, and the recovery of account 1's forgotten password is
  exactly that call; an owner key already holds more than the password does. What the check
  guards is narrow, and said so here: someone at a browser left signed in cannot change *that
  account's own* password without knowing it, nor try passwords faster than a sign-in can (the
  check counts on the account's name in the throttle, §5.4). It does not guard one account
  against another: every account is a full admin (rulings 5, 6, D68), and any of them can set
  another's password without a current one, mint it a set-password link, reveal the owner keys
  that are not door keys, or create an account, and then act as that account. A stolen session
  cookie is therefore as good as the gateway, current password or not.
- **D56 — The door-key refusals hold on both states of the desktop's switch**, not only while
  accounts are on (D25 lists them among the accounts-on surfaces). A paused door key revealed
  while accounts are off would be a working credential the moment they are on again, and one
  deleted while off would leave its account without a door. `owner:dashboard`'s delete keeps
  its own refusal (principals §3.12); every other door key's delete is `409 door_key`, naming
  the account.
- **D57 — The command line chooses its path by the lock** (D16, D28). It takes the data dir's
  lock: got it, the service is stopped and it writes the database itself under the lock; the
  lock held, it reads the door key (read-only, no migration) and calls the plain listener on the
  stored `bind_addr`'s port on loopback. A lock held while that listener does not answer (a
  one-off container on a volume a running gateway holds) is refused naming the holder and both
  ways out. A password is read twice without echo on a terminal, as two lines on a pipe.
- **D58 — A session's end closes what it opened through the existing revocation watch**
  (`devices::watch`, client-apps design §1.6). A watch knows its principal's session and whether
  its principal was an owner key in the cookie; it ends when the session is no longer on the
  desk (signed out, ended on the card, a password change, a disable or delete, the switch), when
  the owner row's account is disabled or paused, and, for an owner-key cookie, when accounts come
  on. Every end moves the revocation generation, so a watch wakes at once (an expiry by the
  clock is honoured within the watch's 30 s re-check). The long-lived connections that carry a
  cookie principal and so end with it: the dashboard's `/api/events` stream (it had no watch
  before WP6, so a rotated owner key's stream stayed open too; now it ends with a `revoked`
  frame), the Chat's turn streams and its change feed, the `/mcp` notification stream,
  `/v1/realtime` sockets and WebRTC calls, the Chat's media actions, and what a caller opens on an
  agent's MCP face (`/agents/{id}/mcp`: a proxied event stream or WebSocket tunnel ends without a
  frame of lmgw's, as `/mcp`'s does; the agent origin asks for no credential and has none to
  watch).
- **D59 — Sign out everywhere is `session_end { account }`**, no op of its own: `{ id }` ends one
  session, `{ account }` every session of that account, one's own included.
- **D60 — A sign-in name is 1 to 64 characters** of §5.2's alphabet, the 64 named in the refusal
  (`LOGIN_MAX_CHARS`); `accounts.session_days` is 1 to 3650, named in its refusal and on its row.
  Both are display bounds, not security ones.
- **D61 — `accounts.session_days` is an ordinary setting** of `settings_set` and the full save;
  `lmgw__settings_set` refuses the whole `accounts` section as an access setting (D17). A save
  that carries `accounts.enabled` at all, even unchanged, is `400 accounts_switch_only`.
- **D62 — The set-password page's sign-in name starts empty**, with `owner` as its placeholder,
  rather than "owner filled in" (§5.3 step 2): the page cannot tell whose link it holds, and a
  filled-in `owner` would be refused for every account but account 1. Empty keeps the login.
- **D63 — With accounts on, the key link and the key paste sign into a session** of the key's
  account (§5.2's "any owner key, which then stays out of the browser"), and so does a login-link
  nonce, which now names its account (`LoginNonces` carries it; the tray's is account 1's). With
  accounts off only account 1's nonce signs in (to today's door-key cookie); another account's,
  minted before the switch went off, is spent and lands on `/?login=invalid`.
- **D64 — Known browsers are minted per sign-in**: each good password sign-in writes a new
  token (expired rows pruned at that write) rather than renewing the cookie the browser sent.
  On `http` the cookie is `lmgw_known`, which D26 confines to loopback anyway. A known browser's
  pair in the throttle is (name, the token's hash), not (name, address), so a spray from the
  known browser's own address (one NAT, the same proxy) holds the sprayer and not it (§5.4).
- **D65 — Sign-in lines in the log**: a good password sign-in, a set-password submit and the
  switch are `info` lines; a failed sign-in is `debug` only, counted on the card
  (`failed_logins`) and later in the audit log, so a spray cannot fill the log (D53's reasoning).
- **D66 — The switch's confirmation and kept-line name the audit log**: the kept-line counts it
  among what is kept, and turning accounts off says the log is kept with the accounts (§5.11);
  both point at the Audit log row below them for its entries and size, which only `audit_list`
  reads (D88). The confirmation is a second click on the row, with the sentence and its counts
  shown first. (Until WP7 the kept-line said the log "arrives with a later version"; WP7's first
  pass put the log's entries and size in the sentences, read on every settings read.)
- **D67 — A sign-in flood costs CPU, not the owner's way in** (WP6; what more to do is not decided yet). One of the `available_parallelism()` verification
  permits is kept for known browsers (§5.4) whenever there are two or more, so an account's own
  browser still signs in while unknown browsers meet `503 login_busy`; a one-core machine has
  none to keep. The address key also counts an IPv6 client on its /56 beside its /64, so a host
  walking the /64s of its site counts as one. What stays exposed: anyone who can reach the
  sign-in page can keep N − 1 cores busy with argon2 (and N − 1 × 64 MiB of memory) for as long
  as they send from enough addresses, and keep every unknown browser at `503` meanwhile. A
  proof-of-work, a global sign-in rate or a smaller permit count are not decided yet.
- **D68 — Every account can act as every other** (WP6, ruling 6 read through). All accounts are full admins by ruling, and the paths that let one act as
  another stay open: setting another's password (ruling 21), its set-password link, revealing the
  owner keys that are not door keys, creating accounts. So the attribution between accounts (the
  sessions card, "made by", WP7's audit log) says who signed in, not proof of who acted: an
  account that set another's password can sign in as it. D55 says what the current-password check
  does guard.
- **D69 — An attempt is counted before it runs** (§5.4): the throttle's check and its count are one
  step under one lock, so the second of two concurrent attempts inside one window is refused, as
  it would be a moment later. A good sign-in takes its count back (the pair's and the name's
  entries go, the addresses lose the one it added); one that reached no verification (no permit
  free, the store failing) is undone. The cost: while a sign-in is checked (about 95 ms), another
  sign-in from the same address is held for 1 s.
- **D70 — A session start checks its account again** (§5.2): its row is inserted by one `INSERT …
  SELECT` that requires the account enabled and, for a password, still holding the hash the
  sign-in verified (after its own rehash). Every end moves its accounts' end generation before
  deleting anything; a start reads it before its insert and goes on the session desk only while
  it stands and accounts are still on, checked under the lock the bump takes. Otherwise it
  deletes its row and answers `409 sign_in_superseded`, "the account changed while this sign-in
  was checked …; sign in again". The desktop's switch saves `accounts.enabled = false` before it
  ends the sessions, and a disable saves before it ends them, so neither leaves a session that
  raced it.
- **D71 — `account_login_link` is the command line's** (§5.6): refused for a session with `403
  login_link_cli_only`, naming `lmgw-server login-link` and a set-password link instead. A
  signed-in browser could otherwise turn itself into a sign-in as any account without its
  password in one call; a bearer of an owner key (the door key the CLI reads) already holds the
  gateway.
- **D72 — A credential of no account acts as none.** An owner row the seed has not attached yet
  (`account_id` NULL) is no account's: every op that acts as the caller's own account
  (`account_password_set` without `id`, `accounts_switch`) answers `403 no_account`, nothing
  lists as "you", and a key it makes reads no "made by". It is never account 1 by default.
- **D73 — A browser holds one session.** A sign-in from a browser that already carries a session
  ends that one once the new one has started, whichever account either is. Expired session rows
  go at each sign-in (in the start's write transaction) and are never listed or counted on the
  card meanwhile; no timer prunes them yet.
- **D74 — A set-password link is spent in the transaction that uses it** (§5.5): the token's row is
  deleted by its hash while it is live, `RETURNING` its account, and only that statement goes on
  to set the password, so of two submits of one link exactly one sets a password and the other
  is `401 link_invalid`. A link of a disabled account is refused (`401 account_disabled`, naming
  it) without being spent, and works once the account is enabled again.
- **D75 — The account commands refuse where a start does** (WP6). `lmgw-server account …` and `login-link` run only with
  `LMGW_DEPLOYMENT=container` or `LMGW_DEV`, and a dev instance never on the installed app's data
  dir, both checked before the data dir is touched. Why: the tray app takes no data-dir lock (D28 is
  the windowless gateways'), so without the refusal a dev build on a desktop would take the free lock
  and migrate and write the tray's production database under it.
- **D76 — The command line never takes a proxy.** Its HTTP client is built with `no_proxy`: every
  call carries account 1's door key and `set-password` a password, and loopback is the only place
  they go, whatever `HTTP_PROXY`, `ALL_PROXY` or their lowercase forms name.
- **D77 — Offline never migrates.** The offline path opens the database without the migrator and
  refuses while this image holds a migration the database has not applied: "start the gateway once
  with this image first". Migrating is the start's step, with its repairs and checks around it.
- **D78 — `account set-password <login> --login <new>`**: account 1's sign-in name with its first
  password, on both paths, under `first_login`'s rule (§5.3). A password is read before the lock is
  tried (the names checked first), so a person at the prompt never holds the data dir against a
  gateway that starts meanwhile; online that means the password is typed before the name is found.
- **D79 — An offline link follows the stored TLS listener.** With the service stopped nothing
  serves, so the link's origin is `public_url`, else the TLS listener the stored settings describe
  (the last start wrote them, its pinned `LMGW_TLS_*` included), else the plain listener (D45 reads
  the serving listener; offline there is none).
- **D80 — `account_password_set`'s `429` and `503` carry `Retry-After`**, the throttle's wait and
  `1`, as the sign-in route's do: the op path answers a whole response, so it can.
- **D81 — A held lock is told apart by its host line.** Same host as the command (one container, or
  one machine outside one): the gateway may still be starting, run it again; another host: another
  container, with both ways out. The pid alone cannot tell, since it is another namespace's number.
- **D82 — "made by" is left out on a desktop with accounts off and only account 1**, which is
  today's desktop (§12, "nothing changed"); with accounts off and more accounts it stays shown
  (§5.11). In the dashboard: the sign-in card waits for `GET /api/session` (asked again, doubling
  from 1 s to 30 s, while it fails, and saying why) rather than guessing the desktop's sentence;
  `GET /api/session` says whether this is the server image, so only there does the card name
  `lmgw-server login-link`; `/api/events`' `revoked` frame asks `GET /api/session` again and either
  opens a fresh stream (the Keys card's rotate signs in again at once) or raises the card with the
  frame's sentence; an armed danger item of every row menu shows its consequence under it; ending
  a session asks first.
- **D83 — A failed audit write fails nothing** (\1). The gate writes
  after the handler answered, so the request already did what it did; failing it then would answer
  "failed" for a change that happened, and writing a row before every request would double the
  writes and still leave the outcome to a second write. So the answer stands, the failure is an
  `error` line naming the action and the caller, and the Accounts card's list counts the failed
  writes since the start (`audit_list.failed_writes`). Fail-closed is the alternative, not taken yet, if the log must never miss an admin write. An admin write's entry is written before the answer leaves (an
  awaited insert); every sign-in route's entry (a sign-in, a failed or throttled one, a sign-out,
  a password set through a link and a failed submit of one) is written after its answer, like a
  failure's count (D13), so no sign-in waits for the log and an unauthenticated request cannot make
  its answer wait on a write (D91).
- **D84 — The op plane's reading ops get no entry** (\1; §5.8 had
  "the POST routes that only read are logged too"). `/api/op/{name}` carries the dashboard's
  reads as well as its writes (`account_list`, `model_find`, `audit_list` itself, which would add
  an entry for every page read), and the op table already marks each op (`writes`,
  `reveals_secret`, the marks the API reference and the `lmgw__*` catalog publish), so an op is
  written when it writes or hands out a secret (`key_reveal`, a set-password or login link). The
  other `Admin` `POST` routes that only read or compute an answer are left out the same way:
  `audit::at_gate::READS` names each with why (the two searches, the decision lab's count and
  batch preview, the audio lab's speech, transcription, alignment and task runs, the voice lab's
  clone check), and `audit::route_walk` holds the list to the API document's
  `x-lmgw-writes: false` marks, so a route marked so later fails the walk until it is listed. A
  request that starts a model for its answer leaves a request log row, as `/v1`'s does. (An earlier version wrote these too.)
- **D85 — What an entry holds** (\1). `principal` is the login as it
  was then with the session or key (a deleted account's entries stay readable); `via` is
  `dashboard` for a session or an owner key's cookie, `api` for a bearer, `mcp` and `chat` for a
  `lmgw__*` call from `/mcp/admin` and from the dashboard's Chat, `cli` and `cli-offline`;
  `outcome` the HTTP status (a tool call 200 or 400 by whether it answered an error). `target`
  keeps an op's or tool's arguments that name its row (`audit::redact::IDENTITY`: `id`, `name`,
  `login`, `model`, `alias`, `enabled`, ..., plain values only), never the rest of a body (a prompt,
  a document, a file). A settings op (`settings_set`, `settings_set_full`, `lmgw__settings_set`,
  `audit::redact::PATCH_OPS`) names the fields its own patch asked for, by dotted path with the
  values as D92 has them (a secret's empty value, which keeps the stored one, left out; a `clear_*`
  flag as its field `"removed"`). Any other write that changed the stored settings as a side
  effect (`hold_set`, the switch) names every field that differs between the settings before and
  after it; there, a change another request made in between is in the entry too, the residual race
  of having no patch to read (an earlier version used the snapshots' difference for the settings ops too, which attributes a concurrent save to the wrong request). Request bodies are
  read by the gate only on the op plane, under the route's own body limit (its `413` is the
  answer), where the handler reads the whole body anyway. A sign-in's entry names the session it
  started ("sam, session #12"), as that session's later entries do.
- **D86 — Sign-in events name the account they concern, never an unknown name** (\1). A sign-in, a password set through a link and a sign-out are the account's own
  entries; a failed or throttled attempt is `anonymous`, with the account whose name was presented
  in its target (it did not act); a name no account has is not written at all, since it may be a
  password typed into the name field. A throttled attempt is written once per hold, at the first
  `429` a delay answers, not per `429`; a failed or throttled attempt's repeats collapse (D91). A
  sign-in refused as busy (`503 login_busy`) is not written: it says nothing about an account, and
  a flood would fill the log with it.
- **D87 — An agent's own MCP endpoint gets no entry; the Chat's tool calls do** (\1). `/agents/{id}/mcp` is `Admin`, but its calls act inside the agent's own tool
  server, as `/mcp`'s do, which is `Inference` and not written; `audit::at_gate::BY_OTHERS` names
  it with `/mcp/admin` and why, and the route walk holds every other `Admin` write route to the
  gate. A writing `lmgw__*` call a model makes in the dashboard's Chat is written as the signed-in
  account's, `via = chat`; one made through `/v1/responses` or a realtime session is the caller
  key's, `via = api`, and one an agent's run makes is `via = agent`. A writing call refused before
  it runs (the self-admin mode, the device's level, an access guard) is an entry too, outcome
  `403` (`400` for an argument the tool does not take), so a refused attempt is visible (D94).
- **D88 — Retention is days, 0 keeps every entry** (WP7; §5.8's draft default, "keeps everything"):
  `accounts.audit_days`, 0 to 3650 (the session lifetime's ceiling), a row of its own under
  Accounts. The prune runs after each entry is committed, in batches of 1000 rows, each its own
  short write transaction (a read first, so a write with nothing to remove costs nothing more), and
  once at a start, so a log nothing is written to keeps its old entries until the next write or
  start; a change from keeping everything to 30 days on a big log then never holds the writer for
  the whole delete (an earlier version pruned inside the entry's own transaction). The size the list
  shows is the table's and its indexes' pages in the database file (SQLite's `dbstat`); it and the
  entry count scan the log, so only `audit_list` reads them, and the settings read the kept-line
  rides on asks only whether the log holds an entry (an index step); a count that cannot be read
  is said on the card, not shown as 0.
- **D89 — The list pages, and stays readable while accounts are off** (\1). `audit_list` answers 50 entries a page by default, any `count` the caller asks for, with
  `before` for the next page and `account` for whose entries (ruling 18's follow-up); it answers on
  a desktop with accounts off too, since the log is kept there (§5.11) and the kept-line counts it.
  The card says "the newest 50" on the first page and "page N, 50 per page" after it, not a computed
  "N–M of T", which drifted as entries arrived.
- **D90 — The command line's offline writes are entries of no account** (\1): `principal` "lmgw-server (gateway stopped)", `peer` "local", the action named as the op the
  running gateway would run (`account_password_set`, `account_password_link`); root in the
  container is no account. Written in container mode and for a dev instance alike, the only places
  the commands run (D75); a failed write is said in the command's own output.
- **D91 — A sign-in event no credential stands behind collapses into one entry a minute** (\1). A failed or throttled sign-in and a failed set-password
  submit, repeated by the same client address with the same action, target and outcome within 60 s
  of the entry's time, add to that entry (`count`, `last_ts`, shown "×N" on the list) rather than
  writing a row each; a write for one is pending at most once at a time, and the repeats that
  arrive meanwhile join it, so a flood from one address costs one row a minute and about one write
  each time the last one finished. Before this, `GET /api/session/login` with a garbage token, a
  token sign-in, and each failed or throttled password sign-in wrote a row each, awaited before the
  answer (a write flood without a credential). An IPv6 client walking
  the addresses of its /64 gets a row per address and minute; the throttle already counts it on its
  /64 and /56.
- **D92 — Prose and command lines in settings are named, never quoted; a URL loses its sign-in**
  (\1). A settings field whose name ends in `prompt`,
  `instructions` or `speech_style` (the Chat's system prompt, realtime's instructions, the Chat's
  speaking style) and every class's `extra_run_args` read `"changed (N characters)"` or `"changed
  (N arguments)"` in an entry: someone's prose may hold anything, and a container's extra arguments
  may carry a token in a flag. Every URL in a settings field (`public_url`, an upstream's address in
  a setting) keeps its scheme, host and path and loses `user:password@`. A value that became an
  object or an array (a setting that was empty) is redacted inside, member by member.
- **D93 — The desktop's tray and window write entries too** (\1). With accounts on, the tray's Start all models and Stop all models (`container`) and its
  GPU hold (`hold_set`), the window's machine setup (`machine_apply`, with its plan's hash and how
  it ended) and a device's pairing link handed to the desktop (with the device's name, never the
  link or its token) are entries of account 1 (the shell's nonce is account 1's, D11), principal
  "owner, tray" or "owner, app", `via = tray` or `app`, peer "local". The window's audio output
  choice is the desktop's own and writes nothing.
- **D94 — A writing tool call the dispatcher refuses is an entry** (\1): see D87. Its outcome is `403`, or `400` for an unknown argument, or `503` when a
  device's level could not be read.

*\1's calls (2026-11-08):*

- **D95 — The image's wrapping key is 64 hex digits.** The Podman secret holds the 256-bit
  AES-256-GCM key as hex, as `openssl rand -hex 32` writes it; either case and whitespace around it
  (its newline) are accepted. Anything else (raw bytes, a passphrase, another length) is refused
  with what was found, the real length and the command that makes one ("… holds 63 hex digits; the
  wrapping key is exactly 64 (256 bits, for AES-256-GCM). Make one with `openssl rand -hex 32 |
  podman secret create lmgw-wrap-key -`"). A passphrase derived into a key is not offered yet.
- **D96 — Without the secret, the image's store answers as no Secret Service does, naming the
  secret** (§13 said `None`; this is the same behaviour with the image's own sentences). A new or
  plain key stays plain with the warn line, which names `/run/secrets/lmgw-wrap-key` and how to
  make it. The reverse case, a key already wrapped and the secret removed (or the unit's line):
  the start logs a warn line, the device listener stays off, and the Devices card shows
  `wrap_key_missing` with "… no wrapping key secret is mounted at /run/secrets/lmgw-wrap-key (the
  Podman secret lmgw-wrap-key was removed, or the unit lost its Secret=lmgw-wrap-key line); restore
  the secret it was wrapped under, or rotate the device CA, which makes a new key kept plain". lmgw
  cannot unwrap it without the key, and a rotation is the owner's step (paired devices stop
  verifying), so nothing replaces the CA on its own.
- **D97 — A secret file that holds no key fails every call.** Unreadable, not hex or not 64 digits:
  the start logs an error line, and nothing is made, wrapped or kept plain; even a plain key that
  served before stays closed until the secret holds a key or the unit's line goes. The owner meant
  the key to be wrapped, and a plain fallback would hide that it is not. *Changed 2026-11-08
  (WP8's review round):* the Devices card's error kind is `wrap_key_secret` (a `DeviceCaErrorKind`
  of its own, in the API types and the API document), with the file's problem as its message; it
  was `secret_service`, which named the desktop's wallet for a file in the image.
- **D98 — Read once per start; a different secret opens nothing.** Podman copies a secret in when
  the container is created, and Quadlet recreates it at each start, so a changed secret reaches
  lmgw at the next start. A key file sealed under another key (a replaced secret, or a data
  volume from another lmgw, a desktop's included) is `unreadable` with the secret named; moving the
  CA from one wrapping key to another (keeping the CA) is not offered yet, rotating the CA under
  the new secret is the way. *Changed 2026-11-08 (WP8's review round):* one key seals every write
  (D99), so unlike the Secret Service, whose prune deletes the old item, a rotation does not end
  an older copy of the key file: a volume backup taken before it still opens with the same
  secret. When such a backup leaked together with the secret, the full recovery is: replace the
  secret (stop lmgw, `podman secret rm lmgw-wrap-key`, make it anew), start lmgw (the key file is
  `unreadable` then, the new secret named), and rotate the device CA, which seals a new key under
  the new secret and re-pairs every device. Tested in that order
  (`a_leaked_backup_ends_with_a_new_secret_and_a_rotation_not_a_rotation_alone`): the old copy
  opens under the old secret after a rotation alone, and opens under neither once the steps ran.
- **D99 — The fixed key seals every write under a fresh nonce.** Under the Secret Service each
  wrapping key seals one file; the image's key seals the file at its creation, the re-wrap and each
  rotation, each time under a random 96-bit nonce (`wrap.rs`). NIST SP 800-38D bounds random nonces
  at 2^32 seals per key, which owner-made rotations do not approach, so the key has no use count.
  What this costs is D98's: an older copy of the key file opens for as long as the secret is the
  same.
- **D100 — Container mode reads the secret, dev instance or not.** The dev rule keeps dev
  instances off the desktop's wallet (device link D2); a file handed to a container is not that
  wallet. A desktop start never reads the file.
- **D101 — `Secret=lmgw-wrap-key,mode=0400`, active in the shipped unit.** A missing secret fails
  the start (D104 says how), so the unit's comment and `docs/server.md` (WP10) say to create it
  before the first start; a server that runs plain drops the line and D104's check. `mode=0400` keeps the
  file root's in the container (Podman's default is 0444). The unit says that the secret sits
  unencrypted in the account's Podman storage, not on the data volume, and how to print it for a
  backup of its own (`podman secret inspect --showsecret`), since a wrapped key opens only with it.
- **D102 — In the image a plain key is wrapped at the start** (\1). When a container-mode start finds the secret holding a key and the CA's key plain
  on disk, it seals the key under the secret there and then, the device listener on or off
  (`Trust::start_on_secret`, `devices/tls/trust/secret_start.rs`). The desktop waits for the
  device listener's next open because wrapping may raise the wallet's unlock prompt, which belongs
  to the moment the owner turns the listener on; the image has no prompt, and a plain key left on
  the volume until someone turns the listener on would be what the secret was made to prevent.
  The start leaves the key closed: the status reads `stored` and `wrapped`. A wrap that fails (a
  certificate that does not match the key) is a warn line, the file stays as it was, and the
  device listener's start tries again.
- **D103 — The start's line about the secret says what the key file is** (\1). One line, keyed on the key file on disk and what the secret held: no
  CA yet with the secret (info: the first CA is made wrapped) or without it (info: kept plain, the
  command that makes the secret); already wrapped with the secret (info); plain and wrapped now
  (info, the load's own "… is wrapped under the wrapping key secret … now" line); plain and no
  secret (warn, the command); wrapped and no secret (warn: the listener cannot open it, restore
  the secret or rotate); a key file that is not usable, a mode others can read included (warn,
  left as it is); a plain key that could not be wrapped (warn); a secret that holds no key
  (error). Tested with the tests' log capture
  (`devices_trust::wrap_secret_start`).
- **D104 — The unit checks the secret before Podman is asked** (\1). `ExecStartPre=/bin/sh -c 'podman secret exists lmgw-wrap-key || { echo "…" >&2;
  exit 1; }'` in `[Service]` prints to the journal that the secret does not exist, the command
  that makes it (`openssl rand -hex 32 | podman secret create lmgw-wrap-key -`, lmgw's own
  `WRAP_KEY_SECRET_HOW`) and that removing the `Secret=` line and this check runs with the key
  plain, then fails the start; without it the journal held only Podman's `no secret with name or
  id "lmgw-wrap-key": no such secret`. It runs on the host as the service account, so it asks the
  account's own secret store, the one `Secret=` reads. `quadlet -dryrun -user` keeps it as
  written and `systemd-analyze --user verify` accepts the generated unit (Podman 6.1.3, systemd
  of Fedora 45, 2026-11-08); the message has no `%`, `$` or backslash, which systemd would expand
  or unescape (the packaging test checks).

*\1's calls (2026-11-08):*

- **D105 — The copy is named by migration numbers, and a taken name moves to `-2`.** `<from>` is
  the newest migration the database records and `<to>` this build's newest, four digits as the
  files are numbered (`lmgw-0101-0102.sqlite`), not release versions: the database records
  migrations, not the release that ran them, and the build that refuses a newer database knows
  its own newest migration, so it finds the copy made from its schema by the name alone. The
  file is created empty first (`create_new`, which never opens a file that is there; `VACUUM
  INTO` fills an empty file and refuses one with content), so no copy is ever overwritten: when
  the name is taken, the copy is `lmgw-<from>-<to>-2.sqlite`, then `-3` …, without a bound.
  The case is real: a start that copied and was then refused (a guard ahead of the migrator,
  §6.4), the old image run on for a while, and this image again; the earlier copy is older, and
  still a restore point. *Changed 2026-11-08 (WP9's review round):* the copies of `knowledge.db` and `quickdoc.db` (D117)
  are named the same way under their own stems (`knowledge-0002-0003.db`), and `<from>` is the
  newest migration recorded below the first one the start applies (D116). The file is no longer
  created under its name first: the copy goes to a hidden partial and is published by `link()`,
  which refuses a name that is there just as `create_new` did (D114). A `-2` now comes only when
  the database changed since the newest copy of the same step (D113).
- **D106 — A copy that cannot be made refuses the start; nothing migrates without it.** The copy
  is the only way back to the schema the previous image opens, so a backups dir that cannot be
  made, a file that cannot be created (permissions, a taken name that is not a file) or a `VACUUM
  INTO` that fails (`SQLITE_FULL` on a full volume) ends the start with one sentence: the
  database, the step (`<from> → <to>`), the cause, that the start does not migrate without the
  copy, the free space at the backups dir (`statvfs`) and the database's size (`page_count ×
  page_size`), the two ways out (free space, fix the dir) and that nothing was written to the
  database. There is no size check before the copy: SQLite's own failure is the signal, not a
  guessed margin. The file this start created and could not fill is removed again (it is not a
  copy, and it would hold the name); a removal that fails is named in the sentence.
  *Changed 2026-11-08 (WP9's review round):* the free space is measured and named at the backups dir when it is there, and at
  the data dir it would be in when it is not (a backups path that cannot be made); the sentence
  ends "nothing was written to `<database file>`" for each of the three databases (the checkpoint
  of D113 folds the log into the file and changes no content).
- **D107 — A database this start creates has nothing pending in the copy's sense.** A database
  with no `_sqlx_migrations` table (a new data dir, an empty file) holds nothing to restore, so
  it gets no copy and no backups dir. "Pending" is an up migration of this build that a database
  with the table has not recorded, as `open_current` reads it.
- **D108 — A newer database is refused before anything writes, on every path.** `refuse_newer`
  reads `_sqlx_migrations` for a migration this build does not have, at the top of
  `run_migrations` (the desktop too: sqlx's `VersionMissing` came only after the repairs and
  notices had run), ahead of the copy in container mode, and in `open_current`, so the command
  line's offline path refuses such a database too (WP6's Not-yet line). In container mode and on
  the offline path the sentence names the newest copy this build opens (the highest `<from>` at
  or below its newest migration, then the highest number): its path, size and time, whose
  schema it is (this build's, or an older one this build migrates forward at its start), and
  that restoring it loses everything written since it was made, the newer image's migrations
  included, with the step (the service stopped, the copy in place of the database, its `-wal`
  and `-shm` removed) and that lmgw never restores one by itself. Without such a copy it says so
  and where the copies would be. The desktop's sentence names the migrations and "run the newer
  lmgw on it again". *Changed 2026-11-08 (WP9's review round):* among the copies of one `<from>`, the newest by the
  file's time wins, then the highest number (the code had sorted by `<to>` in between, against
  this text); a migration this build does not hold that lies below its newest is "a newer or
  different build of lmgw" (another line of builds, not a later one), and the ways back say "that
  image" and "that lmgw"; a backups dir or entry that cannot be read is said so ("no copy this
  build can open can be named, since …"), never "no copy"; the sentence names its database
  (`lmgw.sqlite`, `knowledge.db`, `quickdoc.db`), each refused by its own copies. The check runs
  once per start: in container mode ahead of the copy, on the desktop at the top of
  `run_migrations`, not both.
- **D109 — The copy is `0600` in a `0700` backups dir.** It holds what the database holds, the
  provider keys in cleartext included (ruling 12, §9). The file has the mode from its creation;
  the dir's mode is set when this start makes it, an existing dir is left as it is.
  *Changed 2026-11-08 (WP9's review round):* an existing backups dir whose mode lets others in (any of `077`) is set to `0700`
  with a warn line naming the old mode; the dir is lmgw's own, and what is in it holds the
  provider keys. A dir lmgw cannot change (another owner) is a warn line too, and the copy goes
  on (each copy is `0600` itself).
- **D110 — A container-mode dev instance copies as well.** Container mode is the gate (D25), not
  production: a dev instance in container mode on a scratch data dir copies as the image does,
  which is also how the copy is tried without an image.
- **D111 — Settings → About: "Copies before migrating", in container mode.** One note line (what
  the copies are, that restoring one loses everything written since, that lmgw never deletes or
  restores one), then one row per copy, newest first: the file name, its size in binary units,
  when it was made (the file's time, UTC) and the step it was made before ("before migration
  0101 → 0102"), the path in the row's tooltip; "None yet." without one. The list is read from
  the backups dir at each settings answer (`migration_copies` in `GET /api/settings-full`),
  files with the copy's name pattern only; anything else an administrator keeps there is not
  listed. *Changed 2026-11-08 (WP9's review round):* the list is newest first by the file's time (then the number), holds the
  three databases' copies with each row naming its database ("…, lmgw.sqlite before migration
  0101 → 0102"); the note says each copy is the database "as that start found it, before it
  changed anything", which holds for a `-2` too, and that a refused start finding the database
  unchanged makes no second copy; what of the dir cannot be read comes as
  `migration_copies_error` and shows as a warn line ("Not every copy can be listed: …").
- **D112 — The log names `TimeoutStartSec` and compares nothing.** One info line for the copy
  (path, size, time, what it restores and that lmgw never deletes or restores it) and one after
  the migrations (their count and time, and the copy's and theirs together, "which the server
  unit's `TimeoutStartSec` must cover"). The start is not told the unit's start timeout (the stop
  timeout comes as `LMGW_STOP_TIMEOUT`, D20; the start timeout has no variable yet), so the line
  gives the figure to compare rather than a verdict. *Changed 2026-11-08 (WP9's review round):* both lines name their
  database, the second counts "the copy step" (a copy, or the check that reused one, D113), and
  a copy whose size cannot be read says so instead of "0 B".
- **D113 — No copy for a start a read-only guard refuses, and one copy per unchanged database**
  (\1). The unit's `Restart=always` restarts a refused
  start, so a refusal after the copy used to add a full copy per try until the volume filled. Two
  rules stop that. (a) The guards that only read run ahead of the copy: 0070's price rows always,
  and 0023's aliases and 0095's global fallback once 0018 is recorded (`migration_guards/
  before_copy.rs`); on a database from before 0018 the repair ahead of the migrator renames what
  those two read, so they keep their place after it. (b) Beside each copy a `.source` record holds
  the database as the last start that made or reused the copy left it: the database file's inode,
  size and modification time (nanoseconds), read right after `PRAGMA wal_checkpoint(TRUNCATE)`
  folded the write-ahead log in and emptied it. A start with the same `<from> → <to>` reuses the
  newest copy of that step while the record matches (the copy's own size too), and logs "the copy
  … still matches the database …; not copied again". The record is taken at the copy, again just
  before the migrator (after the repairs, the notices and the foreign-key check's marker wrote) and again when the
  start fails. Why this test: in WAL mode a commit reaches the file only
  through a checkpoint, which writes it, so with the log empty any later write moves the time,
  and a file put in its place has another inode; a content hash would also catch a same-size
  write in the same nanosecond, but reads the whole database at every refused start, the time a
  large database is already short of. It errs one way only: what cannot be read, or a record
  that does not match, makes a new copy. The record matches only the state it holds, so one that
  could not be updated needs no removing.
  *Changed 2026-11-08 (WP9's second review round):* this call had said that a migration killed by
  `TimeoutStartSec` "leaves the database as recorded". That holds for a migration in a transaction
  only (one that commits moves `<from>` on, a new step). `lmgw.sqlite`'s eight migrations without
  one (0002, 0008, 0013, 0018, 0034, 0058, 0086, 0090; 0058 and 0090 open their own transaction
  inside, but sqlx records them only after it) commit statement by statement, so a start killed in
  there moved the file on from the record, and the next start made a `-2` copy of a part-applied
  database, the newest, offered as the restore point. D118 covers that case, D119 a database whose
  state cannot be read; "it errs one way only" no longer holds for either. The record's rename is
  followed by a sync of the backups dir, and a sync that fails is a warn line (the record may be
  the earlier one after a power loss); so is the copy's own dir sync, which had dropped its error.
- **D114 — A copy is published whole; a partial is lmgw's to remove** (\1). `VACUUM INTO` writes a hidden `.<name>.partial` (`0600`, made with
  `create_new`), which is synced, then published under the first free name by `link()` (it fails
  on a name that is there, so no copy is overwritten, D105), the partial name removed and the dir
  synced. A start killed while it copied leaves only the partial, which matches no copy's pattern,
  so nothing lists or offers it; the next start of the same database removes it with a warn line
  ("removed …, the unfinished copy of … an earlier start left"). Removing rather than keeping:
  it never held a whole database, and a start killed at every try would otherwise fill the volume
  with them, the failure D113 is about. *Changed 2026-11-08 (WP9's second review round):* the warn
  line goes on: when every start stops while it copies, the server unit's `TimeoutStartSec` is
  likely shorter than the copy of the database takes, with the database's path and size, and to
  raise it in the unit.
- **D115 — What cannot be read is said** (\1). A backups
  dir that cannot be read, an entry or a copy's metadata that cannot, are collected into one
  sentence: the settings answer's `migration_copies_error` (Settings → About shows it), and the
  newer database's refusal ("no copy … can be named, since …"); a dir that is not there lists none
  without an error.
- **D116 — `<from>` is the newest migration below the first gap** (\1). A database that recorded a later migration out of order (a different build) has
  `<from>` as the newest recorded migration below the first one this start applies, not its
  highest: `lmgw-0100-0102.sqlite` for a database holding 0102 but not 0101, where the highest
  would have read `lmgw-0102-0102` and been called this build's schema. In the ordinary case the
  two are the same.
- **D117 — `knowledge.db` and `quickdoc.db` are copied too** (\1). Both run their own migrations on the same start path (`knowledge::store::open`,
  `quickdoc_core::store::open`), so the start copies each under the same rules before its
  migrator (`migration_copy::around_open`): refused when newer, copied or reused when pending, the
  partial and the record as for `lmgw.sqlite`, no copy for one this start creates. The copies sit
  in the same backups dir and list, and a newer one's refusal names its own copies. Each database
  is copied at its own open (`lmgw.sqlite` first, then the corpus, then the knowledge bases), so a
  start refused at a later one has migrated the earlier ones already; restoring means each
  database the newer image migrated, by its own copy.

*Further WP9 calls (2026-11-08):*

- **D118 — A start stopped inside a migration without a transaction reuses the copy, and a copy
  of a part-applied database is never offered ahead of a whole one.** When the step's pending
  migrations include one without a transaction, the record taken just before the migrator (and
  the failure path's after it) carries `migrator_entered <from>-<to>`. A later start reuses the
  step's newest copy despite a changed size or time when the record names the same database path
  and inode, the copy is its recorded size, the mark names this very step, and the foreign-key
  check's marker the stopped start set before the migrator is still in the database; the info line
  says the last start went on into the step's migrations and stopped inside them, that it is the
  same file and that the database may hold a migration part applied, which the copy does not. Why
  the marker too: nothing of this build writes a database with a pending migration (the command
  line's offline open refuses one), but the previous image sees nothing pending in it and would
  run on it; a build with the foreign-key check clears the marker when it finishes its
  migrations, and then the start copies again (its own line says so). Such a new copy, and any
  copy made while a record beside a copy of the same file says a start entered the migrator
  towards the same `<to>` from this `<from>` or below, or while the marker is set (both only when
  the step holds a migration without a transaction), is marked `part_applied yes` in its record;
  the copy's line says it may hold one of the migrations part applied. The refusal of a newer
  database (D108) ranks a whole copy ahead of a part-applied one of the same `<from>` (then the
  file's time, then the number), and names a part-applied one as such when no whole copy of that
  schema is there. A copy whose records were removed and whose marker a start cleared is not
  marked. The marker can also be left by a migration that failed in its transaction or a
  foreign-key check that refused, so "part applied" is a "may".
- **D119 — A database whose state cannot be read reuses the copy that is there.** When the reuse
  check cannot read the database's state (its log cannot be folded in, its file's time cannot be
  read), a start whose step already has a published copy reuses the newest one, with a warn line
  naming the copy, the database and why, and that the copy does not hold what may have been
  written since; only a step without a copy is copied (also a warn line). Copying at every such
  start would bring back the restart loop D113 is about while the failure lasts.
- **D120 — A linked backups dir is used as it is; the dir's setgid and sticky bits stay.** A
  backups path that is a symbolic link (an administrator putting the copies on another volume) is
  copied into, but its target's mode is never changed, since `chmod` goes through the link onto a
  dir lmgw did not make; when the target lets others in, a warn line names the link, its target,
  the mode and that the administrator makes it `0700` (each copy is `0600` itself). A dir lmgw
  tightens (D109) keeps its setgid and sticky bits (`2775` becomes `2700`): `0700` is about who
  reads, those bits are about the group new files get and who may remove them, and both are an
  administrator's.

## 1. The production server start

`lmgw-server` with no arguments (or `serve`) does, in order:

1. `lmgw_core::machine::probe::run_if_asked()` first, as both entry points do (`headless.rs:44`,
   `main.rs:495`): the machine check starts the program itself for its NVML probe.
2. Logging from `RUST_LOG`, default `lmgw_server=info,lmgw_core=info`.
3. D1's refusal outside a container; the PID 1 warning (D5).
4. The data dir: `LMGW_DATA_DIR` (the image sets `/data`); required, no fallback. The data-dir lock
   (D28); a held lock ends the start naming the holder.
5. `AppState::init_with(data_dir, dev)`; the deployment flag (D2); the pinned settings (§2.2)
   applied to the snapshot before anything binds. In container mode the start asks no Secret
   Service (there is no session bus to ask): it reads the image's wrapping key from
   `/run/secrets/lmgw-wrap-key` instead, wraps a plain CA key under it there and then, and says
   in one line what it found (§9, WP8, D96, D102, D103).
6. The listeners (§4): plain, and TLS when a certificate is configured. A failed bind ends the
   start with the address and the error: there is no tray to keep alive, and a unit that fails is
   what the service manager reports.
7. `server::serve` until `quit_signal` (SIGTERM from `podman stop`, which the unit sends, §3.3); a
   second signal exits at once with `signal_exit_code`, as the example does (`headless.rs:113-125`).
8. `runtime::lifecycle::shutdown` once the server returned, so the inner model containers stop
   before the outer one does.

**Subcommands** (`lmgw-server account …`, `lmgw-server login-link`, `lmgw-server --version`) are
§5.7's; they never serve.

**What it does not do.** No updater and no machine-check fix (ruling 1), no Secret Service (§9),
no dev-instance prefix steering (D4). It has no in-process restart yet (§6.2).

## 2. Configuration without a dashboard

### 2.1 What a fresh server must know before it serves

| Need | Why | Env | Kind | Image default |
|---|---|---|---|---|
| Plain listener | Inside the container the published port reaches only a non-loopback bind (the report) | `LMGW_BIND_ADDR` | pinned | `0.0.0.0:8787`; `127.0.0.1:8787` when TLS is on (D6, D32) |
| TLS certificate and key | §4 | `LMGW_TLS_CERT`, `LMGW_TLS_KEY` (file paths) | pinned | unset: cleared (D32) |
| TLS listener | §4 | `LMGW_TLS_LISTEN` | pinned | `0.0.0.0:8443` |
| Public origin | Login and set-password links, snippets, pairing links, agent origins (§4.3) | `LMGW_PUBLIC_URL` | pinned | unset: cleared, derived from the listener (D32) |
| Trusted proxies | §4.5, proxy mode (ruling 8, WP5) | `LMGW_TRUSTED_PROXIES` (addresses and CIDRs, commas or spaces between) | pinned | unset: cleared, proxy mode off (D47) |
| WebRTC ports | Must match the unit's UDP `PublishPort` (D22) | `LMGW_WEBRTC_PORT` (the plain group), `LMGW_WEBRTC_TLS_PORT` (the TLS group) | pinned | unset: the HTTP port's number and the TLS port's number |
| Stop budget | D20 | `LMGW_STOP_TIMEOUT` (seconds, the unit's `StopTimeout`) | read, not a setting | unset: no comparison, a log line says so |
| Require API key | §6.1, ruling 9 | `LMGW_REQUIRE_API_KEY` | seeded | `1` |
| Device listener scope | It defaults to loopback (device link §4.1), unreachable from a published port inside a container | `LMGW_DEVICES_LISTEN` | seeded | `all` |
| Data dir | | `LMGW_DATA_DIR` (exists) | — | `/data` |
| Update feed | §6.3 | `LMGW_UPDATE_ENDPOINT` (exists, `update.rs:43-48`) | — | the build's feed |

`container_prefix` needs nothing (D4). Accounts need no variable: container mode has them on
(ruling 17). Everything else is set on the dashboard as on the desktop. The TLS, public-origin and
WebRTC keys of this table are ordinary dashboard settings on the desktop (§4.7); only the image pins
them.

### 2.2 Who wins, and what the dashboard shows

- **Pinned** settings are the ones the unit file and the outer container's port mapping depend on:
  a dashboard edit of the bind address would leave the published port pointing at nothing. Each
  start applies the environment over the stored value and writes it, so a later start without the
  variable keeps the last value rather than falling back to the desktop default. The settings
  answer carries a `pinned` list (setting, variable); the row shows the value read-only with "Set
  by this server's container (`LMGW_BIND_ADDR`). Change the unit file and restart the service
  (`systemctl --user restart lmgw`)."
- **Every save path refuses a pinned field** with that sentence: the full settings save
  (`settings_set_full`, `web/api_settings.rs:547`), the patch op (`ops::settings_set`,
  `ops/settings_patch.rs:198`, which `/api/op/settings_set` and `lmgw__settings_set` both reach),
  and any per-section save that writes one of the pinned keys. One check in the shared settings
  write, so a later save path cannot forget it. The same check refuses `accounts.enabled` on both
  deployments, naming `accounts_switch` (D29).
- **Seeded** settings are written only when the data dir is fresh (`fresh_install::open_db`'s
  `fresh`, `state/fresh_install.rs:104`), then the stored value rules and the dashboard edits it
  normally. The start logs each seed it applied.
- A variable that does not parse ends the start, naming the variable and the value: a server that
  comes up on a guessed address is worse than one that does not come up.

## 3. The image, the volumes, the unit, the host

### 3.1 The image

From the spike's `Containerfile`, with these changes:

| Spike | Image |
|---|---|
| `fedora:45`, debug `headless` example | `registry.fedoraproject.org/fedora:<release>` as the RPM's CI tracks, the release `lmgw-server` binary (§10); shipped as an OCI archive (ruling 11) |
| toolkit repo `gpgcheck=0` | D19: lmgw-machine's `REPO_TEXT` (all but `name`), the key imported and checked to be exactly the one added with the pinned fingerprint, then `dnf -y upgrade --refresh` and the installs in the same `RUN` |
| `EXPOSE 8897` | WP2's plain-HTTP image: `EXPOSE 8787 8787/udp 8788 8788/udp` (the plain listener and its WebRTC group, the device listener and its group); WP4 added `8443 8443/udp`. `EXPOSE` names every listener the image may serve; a test checks that the unit's `PublishPort=` lines are what its own start binds, each `EXPOSE`d (D39) |
| `VOLUME /var/lib/containers` | `VOLUME /data /var/lib/containers /var/tmp` |
| additional image store `/var/lib/hoststore` in `storage.conf`, made by the entrypoint | dropped: the shared variant is not the goal (ruling 3); the confined variant's store is the volume alone |
| `ENV LMGW_DATA_DIR, XDG_RUNTIME_DIR` | plus `LMGW_DEPLOYMENT=container` |

Kept as the spike found them necessary: catatonit as PID 1, the entrypoint's cgroup leaf
(`entrypoint.sh:7-18`), the drop-ins under `containers.conf.d/` and `storage.rootful.conf.d/`, the
inner `netns = "pasta"`, `default_sysctls = []`, `cgroup_manager = "cgroupfs"`,
`events_logger = "file"`, `XDG_RUNTIME_DIR=/run/user/0`. The image carries no CUDA: engines are
the inner Podman's images, pulled or built at run time as on the desktop.

The files move from `spike/headless-image/` to `packaging/server/`; the spike dir stays as the
record of what was measured.

### 3.2 Volumes

| Volume | Mount | Holds |
|---|---|---|
| `lmgw-data` | `/data` | the databases, `models/` (`fresh_install`, its `MODELS_SUBDIR`), `builds/`, `tls/` (the device CA), labs, transfers, knowledge files, `backups/` (§6.4), the data-dir lock |
| `lmgw-store` | `/var/lib/containers` | the inner image store and the inner containers. **Never** used by a `label=disable` run: files copied up there keep `container_ro_file_t`, and a confined Podman then fails every call (the report) |
| `lmgw-buildcache` | `/var/tmp` | buildah's scratch dirs and cache mounts (D21), and `TMPDIR`: lmgw's staging copies of user content (a document upload, a corpus export or import) and its tools' temporary files |
| host dir, read-only | `/etc/lmgw/tls` | the TLS pair (D8); only with TLS |

The `.volume` files set `VolumeName=` to these names; without it Quadlet would call them
`systemd-lmgw-data` and so on.

An administrator may bind-mount a host directory at `/data/models` instead (the weights are the
bulk); the model containers then mount it from the outer's path, as they mount the data dir now.
Host folders for agents' host mounts (principals §5) must be mounted into the outer container
first; the picker's text input (principals §5.4) reads "absolute path inside the lmgw container"
in container mode.

### 3.3 The unit

A rootless user Quadlet for a dedicated service account with linger (ruling 10; a rootful system
unit is not yet and not verified). Every flag is the report's minimal
set, in Quadlet's own keys where it has one (checked against `podman-systemd.unit(5)`, Podman
6.1.3). WP2 ships it without the lines marked `WP4`, as a plain-HTTP unit (`PublishPort=8787:8787`
and `8787:8787/udp` in their place); WP4 adds them.

The unit is `packaging/server/lmgw.container`; it is the one copy, so the
design does not repeat it. The lines the prose below explains:

```ini
Image=localhost/lmgw-server:current
Pull=never
AddDevice=nvidia.com/gpu=all
AddDevice=/dev/net/tun
AddCapability=SYS_ADMIN NET_ADMIN
SecurityLabelType=container_engine_t
PodmanArgs=--systemd=always
StopSignal=SIGTERM
StopTimeout=95
Environment=LMGW_STOP_TIMEOUT=95
Notify=healthy
```

Beside them it carries the three volumes, the TLS pair's directory and the
`LMGW_TLS_*` and `LMGW_PUBLIC_URL` lines (WP4), the published ports, the wrapping
key `Secret=` with its `ExecStartPre=` check (WP8), the health commands, and
`Restart=`, `TimeoutStartSec=` and `TimeoutStopSec=` in `[Service]`.

Plus three `.volume` files, each `[Volume]` with its `VolumeName=` (§3.2). No
`network-online.target` lines: Quadlet adds `Wants=` and `After=` on
`podman-user-wait-network-online.service` to every rootless unit itself (`quadlet -dryrun -user`,
2026-11-08).

- **`StopSignal=SIGTERM`**: `--systemd=always` switches the stop signal to SIGRTMIN+3, which lmgw
  does not handle (the report).
- **`StopTimeout`** must cover a whole quit (Today, "The quit's bounds"): `STOP_WITHIN` (10 s) +
  the benchmark abort (`STOP_GRACE_SECONDS` 10 s + `vram.unload_timeout_seconds`, 60 s by default)
  + `SHUTDOWN_TIMEOUT` (15 s) = 95 s. Three of the four are constants; the fourth is a setting, so
  the unit mirrors its value into `LMGW_STOP_TIMEOUT`, and when the stored setting makes the sum
  exceed it, the start logs and Settings → About shows "this server's unit gives lmgw 95 s to stop;
  with an unload timeout of 120 s a quit can take 155 s. Raise `StopTimeout`, `LMGW_STOP_TIMEOUT`
  and `TimeoutStopSec` in the unit, or lower the unload timeout." Podman's default of 10 s would
  kill lmgw in the middle of its stop, and so does the spike's `podman stop -t 20` (`run.sh:69`). A
  test parses the shipped unit and fails when `StopTimeout` is below the four bounds' sum with the
  setting's default, when `LMGW_STOP_TIMEOUT` differs from it, or when `TimeoutStopSec` is not above
  it (D20).
- **`TimeoutStopSec`** must exceed `StopTimeout`: the generated service stops the container with
  `podman rm -v -f -i` (Quadlet's own `ExecStop`), which waits `StopTimeout` before it kills, and
  systemd's default stop timeout (90 s) would kill that `podman rm` first; `podman-systemd.unit(5)`
  says the same under `StopTimeout=`. The margin above `StopTimeout` is the time the removal takes
  after the kill: 0.25 s with an inner container running (`podman rm -v -f -t 0`, Podman 6.1.3,
  2026-11-08), so `TimeoutStopSec=105` leaves forty times that. `StopTimeout` itself equals the
  quit's four bounds exactly, so a quit that uses all of them is killed at its last moment.
- **`Image=` names a local tag, `Pull=never`, no `AutoUpdate=`** (ruling 11). The image arrives as
  an archive: `podman load -i lmgw-server-<version>.oci.tar`, then `podman tag` of the loaded
  version to `localhost/lmgw-server:current`, then a restart (§6.3). `Pull=never` makes a missing
  image fail the start with Podman's own sentence instead of trying a registry that does not exist.
  `podman load` gives the archive the reference `buildah push` wrote into it,
  `localhost/lmgw-server:<version>` with the version's `+` written as `_`, because an image tag
  cannot hold a `+` (`0.6.0+231` → `localhost/lmgw-server:0.6.0_231`); seen 2026-11-08 in a scratch
  store, Podman 6.1.3. The docs' tag command names it that way. A missing `current` tag fails the
  start with Podman's "…: image not known" (`podman run --pull=never`, 2026-11-08). `AutoUpdate=registry` and
  `podman auto-update` wait for a registry (Not yet).
- **`Notify=healthy` with `HealthCmd`**: the unit counts as started only once lmgw answers, so
  `systemctl --user status lmgw` and a failed start say what they should. A startup probe
  (`HealthStartupCmd=`, the same request, every 2 s) makes ready follow the first answer instead of
  the regular check's 30 s interval, and `HealthStartPeriod=` equals `TimeoutStartSec`, so a
  migrating start is never counted unhealthy before systemd itself gives up; the startup probe
  never restarts the container (`HealthStartupRetries` is 0 by default). The probes' `8787` is the
  image's bind address: an administrator who pins `LMGW_BIND_ADDR` to another port changes both
  probes and the `PublishPort=` lines with it. Expected, not yet run under systemd: that
  `Notify=healthy` reaches ready when the startup probe passes. Without `podman auto-update` there
  is no automatic rollback (§6.4).
- **`TimeoutStartSec=900`**: nothing is pulled at a start (ruling 11), but a migrating release's
  first start copies the database and runs the migrations before lmgw answers (§6.4), and both
  grow with the database. 900 s is the draft's figure, written in the unit where the administrator
  sees it; WP9 logs the copy's and the migrations' time at each such start, so a database that
  outgrows it shows before it fails.
- **Quadlet recreates the container on every start**: the generated `ExecStart` is `podman run
  --name … --replace --rm --cgroups=split …` (`/usr/libexec/podman/quadlet -dryrun`). Everything
  that must survive lives on the volumes; D21 is why `/var/tmp` is one.
- **`CgroupsMode=split`** is Quadlet's default (above), where the spike used `podman run`'s
  `enabled`. Whether the entrypoint's cgroup leaf works under `split` is not yet run (Unverified);
  if not, the unit sets `CgroupsMode=enabled`. The entrypoint moves every process of the
  namespace's root into the leaf at every start (a container restarted in place keeps the old
  leaf) and retries the controllers' write against a racing `podman exec` (the health probe), at
  most 20 times 0.1 s apart, each try and the bound in its log line; after the last it exits
  non-zero before lmgw starts, so `Restart=` and the journal show it.
- **`--systemd=always` mounts tmpfs** on `/run`, `/run/lock`, `/tmp` and `/var/lib/journal`
  (`podman-run(1)`). Those are RAM on the host: `XDG_RUNTIME_DIR` (`/run/user/0`) belongs there;
  anything lmgw writes to `/tmp` counts against the host's memory. The image sets
  `TMPDIR=/var/tmp` (the `lmgw-buildcache` volume), which `std::env::temp_dir()` and the tools lmgw
  runs follow; which writers still reach `/tmp` under real work (a code path that names `/tmp`
  itself) is not yet inventoried on a running server (Unverified). The
  TLS pair sits under `/etc` (D8), outside these mounts. The wrapping key's `Secret=` target,
  `/run/secrets/lmgw-wrap-key`, is mounted over the `/run` tmpfs (seen 2026-11-08, Podman 6.1.3:
  `/run/secrets` and the secret's file are mounts below the tmpfs's in `mountinfo`, and the file
  reads in the container; §9, WP8).
- **`Secret=lmgw-wrap-key,mode=0400`** (§9, D101): Quadlet passes it to `podman run --secret`
  as written (`quadlet -dryrun`), Podman's default target is `/run/secrets/<name>`, and `mode=0400`
  makes the file readable by root only (the default is 0444). A missing secret would fail the
  start with Podman's bare `no secret with name or id "lmgw-wrap-key": no such secret` (exit 125,
  seen 2026-11-08), so the unit's `ExecStartPre=` checks `podman secret exists lmgw-wrap-key`
  first and, when it is missing, says in the journal how to make it (`openssl rand -hex 32 |
  podman secret create lmgw-wrap-key -`) and how to run plain, and fails (D104). The administrator
  creates it before the first start. Dropping the line and the check is how a server runs with
  the key plain.

### 3.4 What the host must have (the docs list it; the image cannot)

| Prerequisite | Note |
|---|---|
| Linux with cgroups v2 and systemd, Podman with Quadlet | Tested on Podman 6.1.3; older versions not yet verified |
| SELinux policy with `container_engine_t` | Tested with container-selinux 2.251.0. `label=nested` does not work there (the report) |
| NVIDIA driver and `nvidia-container-toolkit` on the host, and a current CDI spec in `/etc/cdi` or `/var/run/cdi` | The outer container gets the card from the host's system spec. It goes stale after a driver or `egl-wayland` update and then the container cannot start: refresh it (`nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`, or the toolkit's `nvidia-cdi-refresh` units) before restarting lmgw. An outer start on a user-generated spec (`GlobalArgs=--cdi-spec-dir=…`) is not yet verified (WP2) |
| `/dev/net/tun` | The inner pasta needs it |
| A service account with subuid and subgid ranges, and `loginctl enable-linger <account>` | So its user unit runs without a login session (ruling 10) |
| The image archive loaded into the service account's store and tagged `current` | Ruling 11, §3.3; the same two commands at each update |
| The data volume and the build-cache volume protected like a password file | `lmgw-data` holds the provider API keys and the Hugging Face token in cleartext (ruling 12, §9); `lmgw-buildcache` holds, through `TMPDIR`, staging copies of user content (uploads, corpus exports and imports) while they are made. Both, their backups and copies |
| The image archive handled like the deploy token it carries | A GitLab-built archive's binary has the update feed's `read_package_registry` deploy token compiled in, as the RPM does (§9) |
| The published ports open in the firewall | TCP 8443 (or 8787 without TLS) and 8788; UDP on the same two numbers if calls are used |
| With TLS: the certificate pair in the service account's `~/lmgw-tls/` | Written by the host's ACME client or its deploy hook, which copies the renewed pair in (the container sees no symlink target outside the directory). The key is owned by the service account (the account the rootless container runs as, not root) with mode 600; a root hook `chown`s it to that account after writing. A key left `root:<group>` mode 0640 still loads when the group is what lets lmgw read it, and the TLS card warns of the group bit (the group must then contain nothing else). Copied or written, never `mv`ed in from elsewhere: a file created in the `:z`-labelled directory takes its label, a moved one keeps its own and the confined container gets `Permission denied` (2026-11-08, container-selinux 2.251.0); lmgw then keeps the old pair and says so |
| With agent pages: a suffix, its wildcard DNS record, and the certificate covering it | D27, §4.3 |
| Disk for the three volumes | Sized by the models; the machine check's room item shows what is free (§7) |

## 4. TLS and the public origin

### 4.1 What plain HTTP costs on a server

A browser on another machine reaching `http://192.0.2.10:8787` is not in a secure context: no
dictation, no clipboard copy (Today), and the owner key in the cookie and every bearer travel in
cleartext. A reverse proxy in front does not fix it today: lmgw builds its login link, snippets
and agent origins as `http://` on its own address, its same-origin rule refuses an `https://`
`Origin`, and `/mcp` refuses any port but `bind_addr`'s.

### 4.2 The options

| Option | What the administrator does | Browser | Agent origins | Device listener | Cost in lmgw |
|---|---|---|---|---|---|
| **A. Native TLS, administrator's certificate** | Provides a PEM pair (internal CA, ACME from the host or a DNS provider, a bought one) in a host directory | Trusted, secure context | Need `*.<agent_origin_suffix>` in the certificate | Unchanged, published beside it | A TLS listener with a reloading resolver (D7, D8), the scheme threaded through (§4.3), a WebRTC group (D22). The accept loop and rustls config exist (`tls_listener.rs`, `server_config.rs`) |
| B. lmgw's own CA | Imports lmgw's CA into every browser | Trusted once imported | lmgw issues them | Unchanged | Small, but the CA has no name constraints (`ca.rs:169-170`), so a browser that trusts it trusts anything the gateway signs, for any site |
| C. Built-in ACME | Nothing, if the server is publicly reachable on port 80 (HTTP-01), or gives DNS API credentials (DNS-01) | Trusted | DNS-01 only | Unchanged | A client, its state, a provider list for DNS-01; most LAN servers have no public port 80 |
| **D. Trusted-proxy mode** | Runs Caddy, Traefik or nginx in front (Caddy does ACME itself) | Trusted | The proxy's certificate | Must be published directly: mTLS cannot pass a TLS-terminating proxy | `trusted_proxies`; `X-Forwarded-Proto`, `-Host`, `-For` honoured only from them; the same scheme threading as A |

**Ruling 8:** A first, D second (WP5), both on the same public-origin change (§4.3), so D is small
once A lands. B not for browsers. C not yet. A makes the image complete on its own and keeps the
device listener's story unchanged; D is what many servers already run, and its cost is mostly A's.
A and D apply to the desktop as well (§4.7).

### 4.3 The scheme and the public origin, threaded through

`Conn.via` (`server/conn.rs:33-42`) gains `Via::Tls`, the main TLS listener. "The request's scheme"
is `https` on `Tls` and `Devices`, `http` on `Plain`, and in proxy mode (D) the trusted proxy's
`X-Forwarded-Proto`. `net` gains `public_base_url(settings)`: `public_url` when set, else the
primary URL of the TLS listener that serves (D45), else today's `primary_base_url`. `primary_base_url` itself keeps its
meaning (the bind address, loopback for a wildcard), because lmgw's own dials depend on it. Then:

| Where | Change |
|---|---|
| `principal::same_origin` (`principal.rs:612-627`) | `Origin` must equal `<scheme>://<Host>` |
| `web/session.rs::set_cookie` / `clear_cookie` (`:161-172`) | On `https`: `Secure`, and the name `__Host-lmgw_session`, which a browser refuses to accept with a `Domain` attribute, so a page on an agent origin cannot overwrite it from a shared parent (the F8 direction in `agents/service.rs:346-363`). The suffix rules stay as they are |
| `mcp/ingress.rs::origin_allowed` (`:1246-1290`) | The port compared is the listener's own (or the public origin's); a portless `Origin` is 443 under `https`, 80 under `http` (`:1233-1235` reads it as 80 today); the public origin's host is the gateway's own besides the loopback names |
| The start's log line (`server.rs:731-736`) | `public_base_url`; in container mode ruling 15's lines (§5.3); on the desktop today's line, with `public_base_url` |
| `/api/connect`'s `bases` (`web/api.rs:162-170`) | `public_base_url` first, then today's `reachable_urls` of the plain listener, which in the server image is left out while it is loopback-only (D42) |
| The wiring view's `base_url` (`web/wiring.rs:98`) | `public_base_url` |
| A device `hello`'s `dashboard_url` (`net::dashboard_url`, `net.rs:94-118`, called at `web/chat_feed.rs:360-366`) | `public_url` when set, for every device that is not "this computer"; today's rule otherwise |
| `agents::service::mcp_url` / `is_own_mcp_url` (`agents/service.rs:1713-1760`) | **Unchanged**: plain `http` on the loopback listener. The row's dial carries the `owner:dashboard` bearer and must stay inside this process's own listener (D9); in container mode with TLS that is `127.0.0.1:8787` |
| `net::own_host_names` (`net.rs:200`) | Adds `public_url`'s host and the TLS listener's reachable addresses: the suffix shadowing and cookie-parent checks (principals §4.1, `agents/service.rs:330-380`) must see the names people use, not only the container's host name |
| Agent origins (`agents/service.rs:194-196`), `X-Forwarded-Proto` (`web/agent_proxy.rs:569-572`), `LMGW_APP_ORIGIN` | The public scheme and port |
| Pairing proposals (`devices::pairing::proposed_urls` / `proposed_for_device`, `devices/pairing.rs:76-127`) | With `public_url` set, `https://<public_url's host>:<devices port>` is proposed first, before the interface addresses (which inside a container are the container's view). The device listener's leaf certificates then carry that name, because every device URL's host goes into them (`devices.rs:211-216`) |
| Overview snippets | `public_base_url` |

**Agent origins under TLS** (D27) need a certificate that covers `*.<agent_origin_suffix>` and a
suffix outside the dashboard's registrable domain. Example: the dashboard at `lmgw.example`, agent
origins under `lmgw-apps.example`, a wildcard DNS record `*.lmgw-apps.example`, and a certificate
with both `lmgw.example` and `*.lmgw-apps.example`. (A suffix under the dashboard's own name, such
as `apps.lmgw.example`, is refused by the cookie-parent check once `lmgw.example` is one of the
gateway's own names.) At load the resolver reads the certificate's names; a suffix it does not
cover shows on Settings → Network & access ("this certificate does not cover the agents' addresses
(*.lmgw-apps.example): their pages will show a certificate error") and in the log, never silently.

**Ports.** The image listens on the numbers the unit publishes (8443, 8788). A different outer
number works when `public_url` names it, because every URL is built from it and every check
compares the request's own `Host`.

### 4.4 The listeners in container mode

| Listener | TLS off | TLS on |
|---|---|---|
| Plain (`bind_addr`) | `0.0.0.0:8787`, published | `127.0.0.1:8787`: agents (`pasta:-T`), health check, CLI |
| TLS (`tls.listen`) | — | `0.0.0.0:8443`, published |
| Devices (`devices.listen`, `devices.port`) | `all`, 8788 (`bind` port + 1, device link §4.1), published | the same |
| WebRTC (D22) | plain group on `0.0.0.0:8787/udp`; device group on 8788/udp | TLS group on `0.0.0.0:8443/udp` (`realtime.webrtc_tls_port`); no plain group; device group on 8788/udp |

The TLS listener serves the same router as the plain one (`build_router`) and stops with it,
within the same `STOP_WITHIN`, by the same `Stops` generation scheme the device listener uses
(`server/stop.rs`, device link §4.4). The desktop's listeners are §4.7's.

### 4.5 Proxy mode (ruling 8's second step, WP5)

`trusted_proxies` (addresses and CIDR networks; pinned in container mode, a dashboard setting on
the desktop that applies from the next request on, §4.7, D47). A request whose TCP peer is inside
one has its `X-Forwarded-For`, `-Proto` and `-Host`, or its `Forwarded` (RFC 7239), read for the
client address, the scheme and the host, one family per request (D48); from any other peer they
are ignored, and stripped (D51). The client address is the right-most hop that is not itself
inside `trusted_proxies` (each proxy appends the peer it saw, so the hops to the left are the
client's to write; D49). The scheme and the host are one value each (D50). The edge of the main
router writes them where every reader already looks: the connection's client and scheme, the
`Host` header (D51). URLs keep coming from `public_url`, which proxy mode needs set to the
proxy's address (D52). The agent proxy still drops the incoming forwarded headers from what it
forwards and sets its own (`agent_proxy.rs:73-90`), with the forwarded client and scheme. WebSockets
(`/v1/realtime`) pass through a proxy (nginx, 2026-11-08); WebRTC's UDP and the device listener do
not, and are published directly.

A proxy in front of lmgw must overwrite `X-Forwarded-Proto` and `X-Forwarded-Host` (or `Host`)
and append to `X-Forwarded-For`, and drop the other family; nginx passes any header it does not
set, so its server block sets all of them:

```nginx
proxy_http_version 1.1;
proxy_set_header Host              $http_host;
proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
proxy_set_header X-Forwarded-Proto $scheme;
proxy_set_header X-Forwarded-Host  $http_host;
proxy_set_header Forwarded         "";
proxy_set_header Upgrade           $http_upgrade;
proxy_set_header Connection        $connection_upgrade;  # map $http_upgrade → upgrade / close
```

The other proxies drop a client's own `Forwarded` with a line of their own (D48): Caddy
`header_up -Forwarded` in its `reverse_proxy` block, Traefik a `headers` middleware that sets
`customRequestHeaders` `Forwarded: ""` (which removes it), HAProxy
`http-request del-header Forwarded`. The docs (WP10) carry these lines; none is verified live yet
except nginx's.

**The outer pasta shows lmgw every connection from the host's loopback as one address**, the namespace's own (the host's address, which pasta copies by default, e.g. 192.0.2.10; seen 2026-11-08, Podman 6.1.3; from another machine not yet verified),
and a proxy in the same pod sees its host-loopback clients the same way. A trusted CIDR cannot tell
the proxy from anyone else who reaches the published port from the host. Proxy mode is then safe
only when nothing but the proxy can reach lmgw's port: the proxy in the same pod, trusting
`127.0.0.1` with lmgw's port unpublished (the proxy in the pod on `127.0.0.1` ran 2026-11-08), or
the port published on the host's loopback only (`PublishPort=127.0.0.1:8787:8787`), which still
trusts every process on the host. The start logs the peer of each main listener's first request, and
the TLS card shows it, so the administrator can tell; a trusted peer that is the connection's own
address (the pasta case) is warned of (D53). Inside the container the plain listener's loopback
peers include the agents (`gateway_access`) and the health check, so trusting `127.0.0.1` trusts
them too.

### 4.6 WebRTC calls

A call's sockets are chosen by the listener its `POST` came in on (`Group::of`), and the ICE-lite
candidate comes from the connection's local address (`Conn.local`, `server/conn.rs:8-11`). With TLS
that is the TLS group (D22). In a container the local address is the container's view; whether a
call works through the outer pasta forward with the published UDP port is not yet verified (WP4); a
candidate taken from `public_url`'s address is the fix if it does not.

### 4.7 TLS on the desktop

Ruling 17 lets a desktop turn accounts on, and D26 refuses a password over plain HTTP from another
machine. A desktop that turns accounts on for people at other machines therefore needs the same TLS
listener (or proxy mode) as the image, so everything in §4.2–§4.6 is a listener feature of both
deployments, not of the image. It is not tied to the accounts switch either: a remote browser in a
secure context gets dictation and clipboard copy with accounts off too. Until a pair and an address
are set no TLS listener exists, so a desktop with accounts off and no TLS configured behaves as
today; the new rows on Settings are the only difference it shows (ruling 17's "exactly as today"
read as behaviour, not as the absence of new settings).

- **The settings** are the image's: `tls.cert_path`, `tls.key_path`, `tls.listen`, `public_url`,
  `trusted_proxies`, `realtime.webrtc_tls_port`, on Settings → Network & access. On the desktop
  they are ordinary dashboard settings; the image pins them from its environment (§2.1). The TLS
  listener exists once the pair and `tls.listen` are set; the row proposes `0.0.0.0:8443` and the
  user confirms it, since a desktop's other listener defaults to loopback and a wildcard bind is
  the user's choice to make.
- **Applying**: `tls.listen`, the paths and `realtime.webrtc_tls_port` apply at the next start of
  the server, like `bind_addr`: their rows carry "restart to apply", and the tray's **Restart
  gateway** applies them (§6.2). A renewed pair at the same paths is picked up by the next accept
  (D8), with no restart. `public_url` (D41) and `trusted_proxies` (D47) apply at once.
- **The plain listener stays where `bind_addr` puts it** (D6), because the app's window loads it;
  its sign-in through the tray's nonce is a loopback request, which D26 allows. With a non-loopback
  `bind_addr` the plain listener stays reachable for key-bearing clients as today, and its row says
  "passwords from other machines are accepted on the TLS address only" while accounts are on.
- **WebRTC**: the plain and the TLS group can both be bound, on their own ports (D22).
- **The cookie** on a `https` request is `__Host-lmgw_session` with `Secure`, on the app window's
  loopback `http` the plain name, as §4.3 says; they are different cookies of different origins.
- **What the user does outside lmgw**, and the Settings rows name: open the TLS port in the
  firewall, and keep the pair renewed at its paths. lmgw does not touch the firewall.

## 5. Accounts and sign-in

Everything in this section applies wherever accounts are on (D25): always in container mode, on
the desktop once its accounts setting is on (ruling 17; §5.11 has what turning it on and off
does). With the setting off the desktop keeps its one owner, its cookie and its login line.

### 5.1 What an account is

An **account** is a person who signs in. It is not a new kind of principal: its credentials are
`owner`-kind keys, which already hold every capability but `Ledger` (`principal.rs:174-182`), and
ruling 6 gives every account exactly that.

```sql
-- migration 0101_accounts.sql: schema only (as built, with D54's three changes)
CREATE TABLE accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,  -- never handed out twice (D54)
    login               TEXT NOT NULL UNIQUE,  -- [a-z0-9._-], stored as typed in lowercase (§5.2)
    display_name        TEXT NOT NULL DEFAULT '',
    password_hash       TEXT,                  -- PHC string (argon2id); NULL = no password yet
    enabled             INTEGER NOT NULL DEFAULT 1,
    door_key_id         INTEGER NOT NULL,      -- api_keys.id of its door key
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    created_by          INTEGER,               -- accounts.id, soft link
    password_changed_at TEXT,
    last_login_at       TEXT,
    failed_logins       INTEGER NOT NULL DEFAULT 0   -- since the last good sign-in
);
CREATE TABLE account_tokens (            -- set-password links (§5.5)
    token_hash  TEXT PRIMARY KEY,         -- hex(sha256(token))
    account_id  INTEGER NOT NULL UNIQUE,  -- one link per account (D54)
    expires_at  TEXT NOT NULL
);
CREATE TABLE sessions (                  -- §5.2
    id           INTEGER PRIMARY KEY AUTOINCREMENT,  -- what session_end and the principal name (D54)
    token_hash   TEXT NOT NULL UNIQUE,    -- hex(sha256(token))
    account_id   INTEGER NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    peer         TEXT NOT NULL,           -- the client address at sign-in
    user_agent   TEXT NOT NULL DEFAULT ''
);
CREATE TABLE known_browsers (            -- §5.4
    token_hash  TEXT PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    expires_at  TEXT NOT NULL
);
ALTER TABLE api_keys ADD COLUMN account_id INTEGER;     -- soft link, as every key_id column is
ALTER TABLE api_keys ADD COLUMN made_by_login TEXT;     -- the creator's login, copied at creation
```

- **Account 1 is made by `seed_owner_keys`** (`agents/token.rs:443`), which runs after the
  migrations on every start, not by the migration: on a fresh database `api_keys` is still empty
  when 0101 runs, so a migration could not give the account its `door_key_id`. The seed, in one
  `BEGIN IMMEDIATE` transaction: inserts `owner:dashboard` and `owner:self-admin` as today; creates
  account 1 (login `owner`, no password, door key `owner:dashboard`) when it does not exist; and
  attaches every owner row whose `account_id` is NULL to account 1. That last step makes the seed
  idempotent and covers an existing database's owner rows on its first start after 0101, and the
  `owner:self-admin` row that a later start re-seeds after a delete. On the desktop the same seed
  runs whether accounts are on or off; while they are off the row exists unseen (D25), so turning
  them on finds account 1 already holding every owner key (§5.11).
- **Every owner-key writer takes an `account_id`**: `store::insert_owner_key` and
  `store::upsert_owner_key` (`store/keys.rs:143,169`), `token::set_owner_key` /
  `write_owner_key` (`agents/token.rs:400,414`), and `key_create kind=owner`, which attaches the new
  row to the calling account (`web/api_settings.rs:1488`). Rotation's upsert updates the
  credential columns only, so a rotated row keeps its account, as it keeps its id today.
- **Door keys are named by id**: account 1's is `owner:dashboard`; every other account's is
  `owner:account-<id>`, so no login can collide with `owner:dashboard`, `owner:self-admin` or an
  owner key someone named. `key_create` refuses an owner name that starts with `owner:account-`.
- **Each account** may create more owner keys (scripts, `/mcp/admin` clients) that belong to it.
- **Every key gets `account_id`**: for `owner` rows the account it acts as; for client, device and
  agent rows the account that created or paired it, with `made_by_login` copied beside it, shown on
  the Keys and Devices cards as "made by". A deleted account's keys then read "made by a deleted
  account (<login>)" without the account row. That is the "per-account API keys" this model gives
  for free: an account's own owner keys, and attribution of the rest.
- **Door keys belong to their account.** `key_reveal`, `key_rotate` and `key_set` refuse a door key
  that is not the caller's account's ("'owner:account-3' is the sign-in key of account 'sam'; change it
  through that account"); `key_delete` refuses every door key (an account goes with `account_delete`). Any
  other owner key stays revealable by every account, as today (ruling 6). The Keys card shows door
  keys without Copy for other accounts.
- **The principal** carries the account: `Principal::Key` gains `account: Option<i64>` and
  `session: Option<i64>`, filled for owner rows and for session cookies (`principal.rs:27-45`).
  Resolution refuses an owner row or a session whose account is disabled, naming the account, as a
  disabled row is named today (principals §3.3).
- **Every account sees and changes everything** (rulings 5, 6): all Chat threads, all usage, all
  keys, all settings. The Accounts card says so in one line ("everyone with an account here
  administers this gateway and sees its chats and usage").

### 5.2 Sign-in and sessions

- **The page.** With accounts on the SPA's login view gains a sign-in name and a password; the
  paste-a-key fallback (`POST /api/session { token }`, principals §3.4) stays under a fold.
- **`POST /api/session { login, password }`**, in order: the same-origin rule, unconditionally (as
  both existing login routes apply it, principals §3.4, which also covers login CSRF); the
  transport rule (D26); the throttle (§5.4); a verification permit (§5.10); argon2 verification
  (D12, D13); then a session (below), `204`, `last_login_at` and `failed_logins = 0` written.
  Wrong, unknown, disabled, without a password or no sign-in name at all: `401 login_invalid`,
  "the sign-in name or password is wrong", answered before its `failed_logins` is written (D13).
- **Sign-in names** are ASCII letters, digits, `.`, `_` and `-`, compared and stored in lowercase:
  SQLite's `NOCASE` folds ASCII only, so a wider alphabet would make two spellings of one name
  distinct accounts. The display name takes any text.
- **A session** is a `sessions` row: a 256-bit token, stored by its sha256, its account, its expiry
  `created + accounts.session_days`, the address and user agent it signed in from. The cookie
  carries `lmgw-session-<token>`, `Max-Age` from the same setting. Every sign-in path ends in a
  session while accounts are on (ruling 19): the password, the `?token=` link and the paste-a-key
  fallback (any owner key, which then stays out of the browser), `login-link`'s nonce (§5.7), and
  on the desktop the tray's nonce, which signs in account 1 (D11). Door keys never travel to a
  browser as a cookie. A sign-in from a browser that carries a session ends that one (D73), and a
  start checks its account again in its own insert (D70).
- **`from_cookie`** (`principal.rs:577`) resolves a value with the session prefix through an
  in-memory session desk (loaded from the table at start, written through), and answers the
  principal of the account's door-key row, with `account` and `session` set; an unknown or expired
  session is `Anonymous`, as an unknown key is today. While accounts are on a cookie that carries
  an owner key is not honoured either (it reads as `Anonymous`, and the sign-in card shows), so a
  browser signed in before the desktop's switch was turned on signs in again (§5.11). With accounts
  off the desktop's `from_cookie` is today's, owner keys only.
- **`accounts.session_days`** is a visible setting on the Accounts card (ruling 19), the draft's
  default 30 days (today's one year stays the desktop's with accounts off, where rotation ends a
  session). `last_seen_at` is written
  at most once an hour per session, so a busy tab does not write on each request; the card says
  "last seen" to the hour.
- **Sign out** deletes this browser's session row and clears the cookie. **Sign out everywhere**
  deletes all of the account's rows. A password change deletes the account's other rows. Disabling
  or deleting an account deletes all of them. A deleted session ends what it opened (open event
  streams and `/v1/realtime` sockets on the cookie), as a rotated key's do today (`key_written`,
  `devices::revoke`); D58 has the list of long-lived connections that carry a cookie principal.
- **Session list.** The Accounts card lists each account's sessions (created, last seen, address,
  user agent, "this browser"), each with **End**; an expired one is not listed, and its row goes
  at the next sign-in (D73).
- **Session fixation** has no foothold: the token is always the server's, minted after a successful
  sign-in, and nothing a request carries before it is ever promoted.
- **`GET /api/session`** adds `account { id, login, display_name, has_password }` and the session's
  expiry.
- **Changing one's password** (`account_password_set` for oneself) needs the current one when there
  is one, and ends the account's other sessions. A wrong current password counts on the account's
  name in the throttle (§5.4), so the check is no faster way to try passwords than a sign-in; its
  `429` carries `Retry-After`, as the sign-in's does (D80). What the check guards is D55's.
- **Passwords** (rulings 16, 20): at least 12 characters (`PASSWORD_MIN_CHARS`, named, stated on
  the form and in the refusal), counted as Unicode code points after NFKC normalization, which is
  also what is hashed; no composition rules; no blocklist. Against NIST SP 800-63B revision 4
  (§3.1.1.2, read 2026-11-08 at pages.nist.gov/800-63-4): 12 is the owner's choice and lies below
  the 15 characters revision 4 requires (SHALL) for a password that is the only factor, as it is
  here (revision 4 allows 8 only for a password used within multi-factor authentication); the
  blocklist check revision 4 requires (SHALL) is left out by ruling 20; no composition rules
  (revision 4: SHALL NOT impose them) and counting each code point as one character (SHALL) agree
  with it; a maximum of at least 64 characters (SHOULD) is met by the 2 MiB body limit below.
  Revision 4 names NFC (SHOULD), not NFKC: NFKC also folds compatibility characters (full-width
  letters, ligatures, superscripts) into their plain forms, so two passwords that look different
  can verify alike. NFKC is what ruling 20 keeps from the draft.
- **Body limit.** `/api/session` and `/api/account/password` declare an explicit
  `SESSION_BODY_LIMIT` of 2 MiB, which is axum's default today written down, so the only ceiling on
  a password's length is a named one: a `413` names it, and the form says "up to 2 MiB".

### 5.3 Bootstrap on a fresh server

In container mode the start never prints an owner key (ruling 15):

1. While account 1 has no password, each start prints a set-password link for it (ruling 15, single
   use):
   `set a password for account 'owner': https://lmgw.example:8443/set-password#token=…` (built
   from `public_base_url`, §4.3), with its expiry. A new start replaces the previous link with a
   fresh one. Whoever reads the container's log (`podman logs lmgw`, `journalctl --user -u lmgw`)
   can use it, and it stops working the moment someone has.
2. The link's page asks for the sign-in name (empty, `owner` as its placeholder, D62; the only time
   account 1's login can change, and the command line's `--login` is the other way, D78) and the
   password twice, then signs the browser in. Over plain HTTP from another
   machine it refuses (D26) and names `lmgw-server login-link` and TLS.
3. From then on each start prints `sign in at https://lmgw.example:8443/ — for a one-time sign-in
   link run: podman exec -it lmgw lmgw-server login-link`, and no credential.
4. More accounts: Settings → Network & access → Accounts → **Add account** (login, display name)
   answers a set-password link (§5.5) to hand over.

On the desktop none of these lines is printed: its log keeps today's line in both states of the
switch (ruling 15), and account 1 gets its password on the Accounts card, signed in through the
tray (§5.11).

### 5.4 Throttling

The keyspace argument that lets keys go unthrottled (principals §3.3) does not hold for passwords.

- **Keys, in memory**, each a doubling delay after each failure, starting at
  `LOGIN_BACKOFF_START` (1 s) up to `LOGIN_BACKOFF_MAX` (15 min): the pair, (name, address); the
  name across all addresses; the address across all names. Both numbers are the draft's choice,
  without an outside source; they are named constants, printed in every `429` and on the Accounts
  card. The name is keyed by the sha256 of the presented name, folded as logins are (D13). An
  attempt is counted before it runs, under the lock its check takes (D69), so concurrent attempts
  inside one window are refused; a success clears the pair's entry and the name's and takes back
  what it counted on the address.
- **Known browsers.** A successful sign-in also sets `__Host-lmgw_known` (on `https`; `lmgw_known`
  on loopback `http`), a token stored in `known_browsers` for that account, valid for
  `accounts.session_days` and minted anew at each sign-in (D64). An attempt that carries a
  known-browser token for the name it tries is held only by the delay of its own pair, which is
  (name, the token's hash) rather than (name, address), and it may take the verification permit
  kept for known browsers (D67). So someone spraying a predictable name (`owner`) from many
  addresses, or from the known browser's own address, slows down unknown browsers trying it, not
  the account's own browsers, and one office NAT address with a mistyping colleague behind it does
  not hold up the others' known browsers.
- **The answer** while waiting: `429 login_throttled`, `Retry-After`, "too many failed sign-ins for
  this name; try again in 32 s". Never a silent delay. It applies to every presented name,
  existing or not (D13).
- **The client address** is the TCP peer, with an IPv6 peer taken as its /64 (one host usually
  holds a whole /64) and, as a second address key, its /56 (one site usually holds a /56, D67); in
  proxy mode it is §4.5's right-most untrusted hop. Whether the outer pasta
  shows lmgw the real peer is WP4's verification; if all peers read as one address, the address key
  becomes global, the card says "every client reads as <address>: the per-address delay applies to
  everyone", and known browsers still sign in.
- **The map's size has a real bound.** An entry outlives its attempt only when the attempt reached
  a verification (unknown names verify too, D13; one that found no permit free is undone), and is
  dropped once `LOGIN_BACKOFF_MAX` has passed since its delay ran out (so a capped entry still
  holds the next failure at the cap rather than at 1 s). So it holds at most four entries per
  verification done in the last 2 × `LOGIN_BACKOFF_MAX`, which §5.10's permit count bounds;
  pruning runs on each attempt. The card shows the current count.
- **Visible**: `failed_logins` per account on the Accounts card, and every failure in the audit log
  (§5.8).
- **No hard lockout** (ruling 16): a lock after N failures lets anyone who knows a sign-in name
  lock its owner out.

### 5.5 Set-password links

`account_password_link { id }` (and `account_create`) mints a 256-bit token, stores its sha256 in
`account_tokens` with an expiry of `PASSWORD_LINK_TTL` (24 h, the draft's choice, named and printed
with the link: "valid until 2026-11-09 14:00 UTC"), and answers `<public origin>/set-password#token=
…`. The page reads the fragment, asks for the new password twice, and posts `POST
/api/account/password { token, password }`, which checks the token by hash, applies D26 and §5.2's
password rules, then spends the token and sets the password in one write transaction (D74: of two
submits of one link exactly one sets it), ends the account's sessions and signs this browser in. A
link of a disabled account is refused and kept (D74). A second link for the same account replaces
the first. **A link for one's own account
is refused** ("change your own password with your current one"), so the current-password check in
§5.2 cannot be stepped around.

### 5.6 Ops and the card

| Op | Does |
|---|---|
| `account_list` | Accounts with login, display name, enabled, has password, created, last sign-in, failed sign-ins, their keys' and sessions' count |
| `account_create { login, display_name }` | Creates the account and its door key; answers a set-password link |
| `account_set { id, display_name?, enabled? }` | Account 1 cannot be disabled (D11); an account cannot disable itself (another account can). Disabling ends its owner keys and sessions (D18) |
| `account_delete { id }` | Not account 1, not oneself. Deletes its owner keys and sessions; keys it made keep working with "made by a deleted account (<login>)" (D18). Confirm, then do |
| `account_password_link { id }` | §5.5, for any account but one's own |
| `account_password_set { id?, current?, password }` | Without `id` or with one's own: `current` required when there is a password (§5.2). With another account's `id`: sets it without `current`, ends that account's sessions, and the audit names who did it (ruling 21) |
| `account_sessions { id }`, `session_end { id }` | §5.2's list and **End**; ending one's own current session is a sign-out |
| `account_login_link { id }` | Mints a login nonce for that account (`LoginNonces`, `LOGIN_NONCE_TTL`) and answers `<public origin>/api/session/login?nonce=…` with its expiry; what `login-link` calls (§5.7). Refused for a session (`403 login_link_cli_only`, D71) |
| `accounts_switch { enabled }` | The desktop's switch (D29, §5.11); refused in container mode. Confirm, then do |

`/api` only, `Admin`, absent from the `lmgw__*` plane (D17). While accounts are off every op but
`accounts_switch` answers `409 accounts_off`, "accounts are off on this desktop; turn them on under
Settings → Network & access → Accounts". The card lives under Settings → Network & access →
Accounts wherever accounts are on (D25, ruling 17); on a desktop with the setting off that place
holds the switch and §5.11's line of what is kept.

### 5.7 The command line

Through the running container: `podman exec -it lmgw lmgw-server …`.

| Command | Does |
|---|---|
| `account list` | As `account_list` |
| `account set-password <login> [--login <new>]` | Reads the new password twice from the terminal (no echo; two lines from a pipe) before it takes or tries the lock, then `account_password_set { id, password, login? }`; the account's sessions end. `--login` gives account 1 another sign-in name with its first password (D78) |
| `account link <login>` | Prints a set-password link (`account_password_link`) |
| `login-link [<login>]` | Prints a one-time sign-in link for the account (account 1 by default) from `account_login_link`, valid `LOGIN_NONCE_TTL` (60 s) and once, with its expiry; run it again if it lapsed |

**How** (D16): the running gateway holds keys, sessions and nonces in memory, so a direct database
write would not reach it, and a nonce exists only in the process that will exchange it. The CLI
reads account 1's door key from the database (it runs as root in the same container as the
gateway, which is the trust it needs) and calls the plain loopback listener's `/api/op/…` with it,
sending `x-lmgw-client: cli`, which the audit log records as `via = cli` (a claim, recorded as one;
the header joins the documented `x-lmgw-*` headers). It calls without
a proxy, whatever `HTTP_PROXY` says (D76), on loopback at the stored port (D57). Account 1's door key stays the recovery path for
every other account.

**With the service stopped** the container is gone (lmgw-server is its main process), so there is
nothing to `exec` into. The same commands then run in a one-off container on the data volume:
`podman run --rm -it -v lmgw-data:/data <image> lmgw-server account set-password owner` (`-it`:
without a terminal or a pipe on stdin the password read finds stdin ended). That path
takes the data-dir lock (D28) and refuses while a gateway holds it; it writes the database itself
(`BEGIN IMMEDIATE`, as every write), writes the audit entry with `via = cli-offline` (D90),
and says that the change applies at the gateway's next start. It never migrates: a database this
image has not started on is refused with "start the gateway once with this image first" (D77),
and one a newer image migrated as a start refuses it, the newest copy named (D108).
Its set-password link is built on `public_url`, else the stored TLS listener, else the plain one
(D79). A lock held while nothing answers on loopback is refused naming the holder, a gateway on this
host as one that may still be starting and any other as another container (D57, D81). Like a start,
the commands run only in container mode or as a dev instance on a scratch data dir (D75). `login-link` has no offline form (a nonce needs the
running gateway); `account link` does, since a link lives in the table.

**On the desktop** there is no `lmgw-server` and no CLI. Its recovery path is the tray: the shell's
nonce always signs in account 1 (D11), whose card can set any account's password (ruling 21).

### 5.8 The audit log

Ruling 18: one table, `audit_log (id, ts, account_id, principal, via, peer, action, target,
outcome)` (migration 0102), written wherever accounts are on (the image always, the desktop while
its switch is on, §5.11). It is attribution among people who are all admins, not tamper evidence:
any account can change any setting, including the log's retention. Between accounts it says who
signed in, not proof of who acted (D68); the card says so above the list.

- **Columns.** `account_id` is the account that acted (a soft link: a deleted account's entries
  stay); `principal` its login as it was then with the session or key it came in with ("sam,
  session #12", "owner, key owner:dashboard"); `via` one of `dashboard` (a session or an owner
  key's cookie), `api` (a bearer), `mcp` (a `lmgw__*` call on `/mcp/admin`), `chat` (one a model
  made in the dashboard's Chat), `agent` (one an agent's run made), `cli` (the `x-lmgw-client: cli`
  header, a claim), `cli-offline`, and the desktop's `tray` and `app` (D93); `peer` the client
  address (a trusted proxy's forwarded one, §4.5); `outcome` the answer's HTTP status (D85);
  `count` and `last_ts` how many times a collapsing sign-in event happened and the last time
  (D91).
- **Written at the gate** (`audit/at_gate.rs`) for every non-`GET` request to a route
  `CAPABILITY_TABLE` marks `Admin`, once the gate let it in and the handler answered: the method
  and the route's template, or `/api/op/{name}`'s name. One place, so a route added later is
  covered by its table row; `audit::route_walk` sends a request to every `Admin` write row and
  finds its entry. The `POST` routes outside the op plane that only read or compute an answer
  (`audit::at_gate::READS`, held to the API document's `x-lmgw-writes: false`) are left out, as the
  reading ops are by the op table's own marks: an op is written when it writes or hands out a
  secret (`key_reveal`, the links), D84.
- **`/mcp/admin`** is one route for reads and writes, so the self-admin dispatcher
  (`mcp/selfadmin.rs`, `audit::ToolCall`) writes each call of a tool whose catalog entry writes,
  with the tool's name, from `/mcp/admin` (`via = mcp`), the Chat (`via = chat`), `/v1/responses`
  and realtime (`via = api`) and an agent's run (`via = agent`), and a refused call too (D94). An
  agent's own MCP endpoint (`/agents/{id}/mcp`) gets no entry (D87).
- **Plus** every sign-in (with what: password, key, login link, set-password link), failed sign-in,
  throttled sign-in, sign-out, a password set through a link and a failed submit of one (the
  session routes are `Public`, so the gate does not see them), the command line's offline writes
  (`via = cli-offline`, D90), and the desktop's `accounts_switch` in both directions (the gate
  writes it when accounts are on before or after the request). Session ends, password changes,
  set-password and login links, account and key changes are `Admin` ops, so the gate writes them.
  A failed or throttled sign-in names the account whose name was presented and no account as the
  actor; a name no account has is never written, since it may be a password typed into the name
  field (D86). Every sign-in route's entry is written after its answer; a throttled attempt once per
  hold; a failed or throttled one repeated by one client collapses into one entry a minute (D91).
  The desktop's tray and window actions are entries with accounts on (D93).
- **Not written for**: `/v1` (the request log has it), Chat messages, reads (the reading ops and
  `READS`), `GET`s, a sign-in refused as busy (`503 login_busy`), the desktop's audio output.
- **`target`** is a JSON object: an op's or tool's arguments that name its row (`id`, `name`,
  `login`, `model`, `alias`, `enabled`, ... — `audit::redact::IDENTITY`, plain values only, never a
  prompt or a file), another route's concrete path, a settings op's own patch by dotted path
  (`accounts.session_days = 7`), and for any other write that changed the stored settings the
  fields that differ before and after it (D85); prose, command lines and URLs' sign-ins as D92
  has them.
- **Redaction**: every property and parameter name the API document flags `x-lmgw-secret`
  (the flags `openapi_structure::secret_inputs_per_operation` pins: passwords, `current`, tokens,
  upstream `api_key` and `extra_headers`, MCP servers' `headers` and `env`, `hf_token`,
  `update_token`, `forge_tokens`, agent `values`) reads `"changed"` wherever it appears in a target,
  a settings field included, at any depth (inside a value that became an object or an array too);
  nothing else holds a list. `audit::redaction` fails for a request-body or settings field whose
  name reads as a credential (password, token, secret, key, nonce, pin, otp, pass, passcode, dsn,
  signature, hmac, salt and the rest of its list; integer fields included, since a PIN is one) and
  carries no flag; it walks the API document and the settings' JSON schema, so an optional struct,
  a list of structs or a map added later is walked whether or not the defaults hold one; and it
  checks end to end that no sentinel secret of an op or a settings save reaches an entry.
- **Retention** (`accounts.audit_days`, Settings → Network & access → Accounts → Keep audit
  entries): days, 0 to 3650; the default 0 keeps every entry. Each entry is committed and then the
  entries older than the retention removed, in short batches (D88), so a change of the retention is
  written to the log before it applies and a shorter one leaves its own entry behind (ruling 18); a
  start prunes too. The list shows the entries, the space they take on disk (SQLite's `dbstat`,
  binary units) and the oldest entry's time.
- **The Activity list** (ruling 18's follow-up, the owner's) is the Accounts card's Audit log row:
  `audit_list { account?, before?, count? }`, 50 per page, newest first, readable while accounts
  are off on the desktop (D89). Its account picker shows everyone's entries, one account's, or the
  entries of no account (the offline command line, failed and throttled sign-ins). Each entry shows
  its time, who with how and from where, the action in words (the op's or tool's title, the route's
  summary, the event's sentence: `audit_list` adds `action_label` from today's tables), what it
  names with each settings field by its row's label, the answer, and "×N" for a collapsed one.
- **A failed write** fails nothing (D83): the request already did what it did, so its answer stands;
  the failure is an `error` line naming the action and the caller, and the list says how many
  writes failed since the start.

### 5.9 With the rest of lmgw

| Surface | With accounts |
|---|---|
| `/v1`, `/mcp` (`Inference`) | Unchanged: client keys, Require API key (§6.1). An account's owner keys work there as owner keys do today |
| `/mcp/admin` | Any account's owner key (`Admin`); the self-admin level (`settings.self_admin`) unchanged |
| Device pairing, the device listener | Any account pairs and confirms; the device row records who (`account_id`, `made_by_login`). A device's admin level and its cap by the global one (`config/keys.rs:277-282`, `DeviceAdmin::capped`) unchanged |
| Kai clients | Unchanged: they are devices with their own keys and mTLS, never the cookie; their pairing and their Chat are the gateway's, as all Chat is |
| Agent origins | The session cookie is host-only (principals §3.3), so `<id>.<suffix>` never receives it; on `https` it is `__Host-` and `Secure`. An agent origin's page is still served to anyone who reaches the port (principals §3.2); on a server that is wider than on a desktop (Not yet) |
| Desktop, accounts off | Unchanged (D25, ruling 17): the shell's nonce sets the `owner:dashboard` cookie, the log prints the token link |
| Desktop, accounts on | As the image, but the shell's nonce signs in account 1 into a session, the log keeps today's line, and there is no CLI (§5.7, §5.11) |

### 5.10 Security review points

| Point | How it is met |
|---|---|
| CSRF | `SameSite=Strict` plus the same-origin rule on every cookie request and on every login route, now scheme-aware (§4.3) |
| Login CSRF (a foreign page signing a browser into another account) | The login routes' unconditional same-origin rule (principals §3.4) |
| Session fixation | §5.2: the session token is only ever the server's, minted after the check |
| Stolen cookie | A session, not a key: it expires (`accounts.session_days`), works only as a cookie, and ends from the card or with Sign out everywhere |
| Timing | argon2's own constant-time comparison; the dummy verification for unknown, disabled, password-less and malformed names, against a dummy hash made at the start; one store lookup for every name; a failure's count written after its answer, with no snapshot reload (D13); keys, sessions and set-password tokens looked up by the sha256 of a 256-bit secret, where an early-exit comparison leaks nothing usable |
| Enumeration | One answer and one duration for every failing name (D13); the throttle keyed on the presented name |
| Brute force and spraying | §5.4; attempts counted before they run, so concurrency buys no extra tries (D69); the current-password check counted like a sign-in |
| Races between a sign-in and an end | A start checks its account in its own insert and against the ends' generation (D70); a link is spent in the transaction that uses it (D74) |
| Cost driven by unauthenticated requests | Each verification takes 64 MiB and a blocking thread (D12). At most `available_parallelism()` run at once; a sign-in that finds no permit free answers `503 login_busy`, "N sign-ins are being checked right now; try again in a moment", `Retry-After: 1`. One of the N is kept for known browsers (D67), so a flood keeps unknown browsers out, not the account's own; the CPU a flood burns meanwhile is not decided yet (D67). N and N × 64 MiB are logged at start and on the Accounts card, so the memory a flood can take is a number the administrator sees. One verification takes 95 ms (min 94.6, max 99.5 of ten) on the RustCrypto crate 0.6.0 without its `parallel` feature (the four lanes one after another), in a release build on a 16-core desktop CPU, 2026-11-08; `accounts::password::tests::one_verification_s_time` (ignored, run with `--release -- --ignored --nocapture`) measures it again. A debug build took 1.75 s, so dev builds compile `argon2` and `blake2` at opt-level 3 |
| Cleartext | D26; `Secure` cookies on `https`; in container mode plain stays on loopback when TLS is on; on the desktop TLS exists for the same reason (§4.7) |
| Credentials in logs | Ruling 15: the container's log carries no durable key. The desktop's log keeps today's login line with `owner:dashboard` in it, in both states of the switch (ruling 15); it is the desktop user's own journal |
| Who did what | §5.8; between accounts it records who signed in, not proof of who acted: every account can set another's password and sign in as it (D68) |
| Key reuse | Each account its own door key and sessions; disabling an account ends them (D18) |
| The desktop's switch | `/api` only and confirmed (D29); turning it off ends every session and pauses the other accounts' owner keys, so it never leaves a credential working that no card shows (§5.11) |

### 5.11 Accounts on the desktop: the switch

Ruling 17 puts accounts behind `accounts.enabled` on the desktop, off by default, written only by
`accounts_switch` (D29). Off is today's desktop. On is §5 as the image has it, with three
differences: the tray's nonce signs in account 1 (D11), the log keeps today's line (ruling 15), and
there is no CLI (§5.7). Account 1 exists on every desktop already (§5.1), holding every owner key,
so turning accounts on moves no data.

**Turning it on** (from any owner principal; the confirmation: "Everyone who signs in to this
dashboard will need an account. Other browsers signed in now sign in again; this one stays signed
in."):
- The answer sets a session cookie for the caller's account in the same response, so the browser
  that flipped the switch is not signed out. Every other browser's owner-key cookie reads as
  `Anonymous` from then on (§5.2) and shows the sign-in card; the app's window gets a session
  through the tray's nonce, as it does whenever it is built.
- While account 1 has no password a banner says so ("account 'owner' has no password: set one
  before signing in from another browser"). The Accounts card sets it for oneself without a current
  password and, as the image's set-password page does, asks for the sign-in name in the same form,
  the only time account 1's login can change (§5.3).
- Passwords from other machines need the TLS listener or proxy mode (D26, §4.7); the switch's row
  says so while neither is set.
- The audit log starts with the switch's own row. Require API key is not touched (§6.1).

**Turning it off** (the confirmation names the numbers: "Turning accounts off signs every browser
out and pauses the keys of 2 other accounts. The 3 accounts, their passwords and the audit log
(its size is under Audit log, below) are kept and come back when you turn accounts on again."). This record
recommends keeping every piece of data and deleting nothing silently:
- **Sessions end**: every row is deleted and what it opened is closed, as a Sign out everywhere
  for every account. A session is a credential, not data, and one left in the table would come
  back to life, perhaps months later, when the switch goes on again. The caller's browser gets
  today's door-key cookie in the same answer when the caller is account 1; any other account's
  browser is signed out with the rest, since with accounts off only account 1 signs in.
- **Other accounts' owner keys are paused, not deleted**: their door keys (`owner:account-<id>`)
  and every owner key that belongs to them. Resolution refuses them with a sentence ("this key
  belongs to account 'sam'; accounts are off on this desktop"), the Keys card shows them as paused,
  and they work again when accounts are on. So turning accounts off never leaves a person's
  credential working without a card to see or end it. Account 1's keys, `owner:self-admin` among
  them, stay today's owner keys. Client, device and agent keys any account made keep working: they
  are the gateway's, as D18 holds for a disabled account.
- **Kept and unused**: the accounts with their password hashes, known browsers (they expire on
  their own), set-password links (refused with `409 accounts_off` while off, expiring on their
  own), "made by" on keys and devices (still shown). The throttle's map is in memory and goes.
- **The audit log** is kept and gets the switch as its last row; nothing is written while off,
  since the desktop with accounts off is today's, which has none. Where the Accounts card stands, a
  line says what is kept, with the counts; the Audit log row under it has the log's size. Deleting accounts or log rows stays
  what it is with accounts on: `account_delete` and the retention setting, both audited.

**Turning it on again** finds everything kept: the accounts and their passwords, the paused keys
working again, the log continuing with the switch's row. Sessions start fresh, since every one
ended at the switch-off.

**What stays container-mode only on a desktop with accounts on**: the migration copy (ruling 14),
because it belongs to the image's update path, not to accounts; the desktop's RPM update path is
unchanged (ruling 7). The container's log lines, the CLI and the data-dir lock are the image's too
(D25).

## 6. Require API key, restart, the update notice

### 6.1 Require API key

The image seeds `auth_enabled = true` (§2.1, ruling 9): a server reached beyond loopback otherwise
serves `/v1` and `/mcp` to anyone who reaches the port, at the owner's cost for cloud aliases. It is
seeded, not pinned: an ordinary setting the administrator can switch off on the dashboard. The
desktop default stays off, and the desktop's accounts switch does not change it. The Overview's
snippets already print the `Authorization` line from the setting (principals §3.8).

### 6.2 Restart

The tray's **Restart gateway** applies the settings that take effect at the next start:
`bind_addr` and the two WebRTC keys (Today). In container mode `bind_addr`, the TLS listener and
`realtime.webrtc_port` are pinned (§2.2), the certificate reloads by itself (D8), and the device
listener moves at once (device link §4.1). That leaves `realtime.webrtc_enabled`: in container mode
its save sentence reads "the WebRTC calls switch applies at the next start of lmgw (systemctl
--user restart lmgw on the host)", the panel's pending line says the same of the saved switch, and
the pinned rows name the same command. The sentences name the setting by its label on the page,
never by its key. The image has no in-process restart yet; the row's `restart to apply` unit is
shown on the desktop only. On the desktop the TLS listener's settings and
`realtime.webrtc_tls_port` join `bind_addr` among the "restart to apply" rows that **Restart
gateway** applies (§4.7); the accounts switch needs no restart (§5.11).

### 6.3 The update notice

No updater (ruling 1). Ruling 13: in container mode lmgw checks the same feed the desktop's updater
polls (`update::check`, `update.rs:89-99`), on the desktop poll's schedule (20 s after the start,
then every 6 hours, `src-tauri/src/updater.rs:26-28`), and shows one line on Settings → About and
one in the log: "lmgw 0.8.0 is out; this server runs 0.7.2. Updates come as a new image archive:
load it and restart the service." It downloads nothing. The switch is the existing
`update_check_enabled` (Settings → Tokens & updates, `config/settings.rs:206`, on by default), which
the desktop's poll already honours (`updater.rs:44`); off, the background check sends no request.
The save that switches it off clears the notice; an explicit `update_check` still asks the feed
and keeps its answer (D31), and in the image it answers with the notice's line. With no
registry (ruling 11) the update path the docs give is: download the new archive, `podman load -i …`,
`podman tag` it as `localhost/lmgw-server:current`, `systemctl --user restart lmgw`. The manifest
(`lmgw-update/src/manifest.rs:19-26`) gains an optional `image { reference, digest }` only once a
registry exists (Not yet); until then the notice compares versions alone.

### 6.4 Rollback and one-way migrations

With no registry there is no `podman auto-update` and no automatic rollback (ruling 11). Rolling
back is a manual step, never automatic (ruling 14): tag the previous image `current` again (it
stays in the store until the administrator removes it) and restart. That works for a release that
changes no schema. **A release that migrates the database cannot be rolled back that way**: once
the new image has migrated, the old image's migrator refuses the database (`VersionMissing`,
`store.rs:173`), so the old image does not start on it. The docs say this plainly with each release
that carries a migration. (The same holds for `podman auto-update`'s rollback once a registry
exists, Not yet.)

Ruling 14, container mode only (D25): before a start touches a database with pending
migrations, ahead of the repairs (`store.rs:159`, which write too), it copies the database with
`VACUUM INTO /data/backups/lmgw-<from>-<to>.sqlite`, logging its size and the time it took, and
the migrations' time after it (§3.3's `TimeoutStartSec`). `knowledge.db` and `quickdoc.db`, which
run migrations of their own at the start, are copied the same way before theirs (D117). That copy
is a **manual restore source**, not a rollback: restoring it on the old image loses everything
written since, and lmgw never restores it by itself. A start refused after its copy (the unit
restarts it) reuses that copy while the database is unchanged, and the guards that only read come
before the copy, so a refusal loop does not fill the volume (D113).
From the release that brings this on, a build that refuses a newer database names the newest such
copy and what restoring it loses. The copies are listed with their sizes on Settings → About and
are never deleted by lmgw. *Built in WP9:* the copy's name, a taken name and a copy that cannot be
made (D105, D106: the start then refuses to migrate), a new database (D107), the refusal before
anything writes and on the offline path too (D108), the copy's mode (D109), the About list (D111)
and the log lines (D112); in its review round the read-only guards ahead of the copy and the
reuse of an unchanged database's copy (D113), the partial and its publish (D114), what cannot be
read said (D115), `<from>` below the first gap (D116) and the two other databases (D117); in its
second review round a start stopped inside a migration without a transaction reusing the copy, a
part-applied copy never offered ahead of a whole one (D118), an unreadable database reusing the
copy that is there (D119) and a linked or setgid backups dir (D120).

## 7. The machine check in a container

The check runs unchanged; what changes is what it can know and what its steps say. Inside the
image the toolkit, the rpm database and the repositories are the image's, and the fix does not exist
(ruling 1).

| Item (machine check §1) | Desktop | Container mode |
|---|---|---|
| 1.1 Card and driver | The PCI card, the module, the libraries, NVML; steps to install or update the driver | The same reads (the host's module through `/proc/driver/nvidia`, the libraries the outer CDI spec mounted). A card on the PCI bus with no `/dev/nvidia*`: "The lmgw container was started without the graphics card. On the host: add the card to the unit (`AddDevice=nvidia.com/gpu=all`), check the host's CDI spec is current (`nvidia-ctk cdi list`), restart the service." Driver steps read "On the host: …" |
| 1.1 After a host driver update | Module and libraries differ: restart | The outer container keeps the libraries it started with until it restarts, and lmgw's own-spec stamp sees the change only then. A module newer than the libraries: "On the host: the driver changed (module 600.20, libraries 600.10). Refresh the host's CDI spec, then restart the lmgw service." Libraries newer than the module: "On the host: restart the machine to load the new driver (module 600.10, libraries 600.20)." NVML's mismatch without versions names both (D30) |
| 1.2 Toolkit, lmgw's own spec | Install the toolkit (the fix); own spec under `XDG_RUNTIME_DIR` | The image ships the toolkit; missing means a broken image: "update the image". The own spec is generated inside, as the spike found. In the confined variant it lacks `nvidia-smi` (SELinux denies reading it); CUDA works, and the check does not flag it |
| 1.3 Podman | Rootless; "as root" (`running.rs:55`); subuid hints (`:103-108`) | "Podman inside the lmgw container", "rootful on the host" or "its root is host uid N" as `/proc/self/uid_map` shows it. No subuid hints for the inner root; a uid map without a range gives "On the host: the service account needs subuid and subgid ranges" |
| 1.4 Settings fit | | Unchanged |
| 1.5 Room | Free space where models and images live | The same statvfs, each named as the volume it lies on (`/data`, `/var/lib/containers`, `/var/tmp`) by `/proc/self/mountinfo`; another mount, or none, is said as such (D30) |
| 1.6 Download | | Unchanged (the inner store) |
| §3 The fix, §4.3 the shell command, the plan block | Behind the window's button | Absent; the plan's text steps become host steps |
| §5 Pre-fetch, metered check | `/proc/net/route` + `nmcli` | The probe is skipped: "whether the connection is metered is not known (lmgw does not check it inside a container)", logged once; the pre-fetch goes ahead as it does today when the answer is unknown (`prefetch.rs:192-195`). No `nmcli` in the image |

## 8. The spike's other findings

| Report item | Here |
|---|---|
| 1. Read-only images (`backends/images.rs` ignores `ReadOnly`) | Only the shared variant has them (ruling 3): not yet |
| 2. Dev-instance assumptions (`state.rs:1051-1060`: a dev instance shares production's image store) | A production server start is not a dev instance (D1), and its store is private (D4); the refusals do not apply. A dev instance run inside the image (`LMGW_DEV`) keeps refusing image deletion although nothing is shared there; harmless, and not changed yet |
| 3. Machine check in a container | §7 |
| 4. Metered check | §7 |
| 5. PID 1 | D5 |
| 6. "Could not locate device 195:254 on host" on every inner GPU start | A failed start's text quotes Podman's stderr (built in `runtime/registry/acquire.rs`). The runner (`runtime/registry/command.rs`) drops a list of known-harmless lines from the stderr it hands back, one entry with the report as its evidence, and keeps them in the debug log. An entry matches the whole line as Podman prints it (bare, behind `WARN[0000] `, or as `level=warning msg="…"`), case-sensitive; anything else stays as Podman said it, and `podman logs`' stderr (a container's own) is never filtered |
| Named in the report's `Not tried yet` | Each is a verification step in WP2 or WP4 (§13) |

## 9. Secrets at rest

| Secret | Where | In the image |
|---|---|---|
| Device CA key | `/data/tls/` (`devices/tls/files.rs`), wrapped under a Secret Service item on the desktop, plain (0600) without one | **Built (WP8)**: sealed under a wrapping key from `/run/secrets/lmgw-wrap-key` (the Podman secret `lmgw-wrap-key`, `Secret=lmgw-wrap-key,mode=0400` in the unit), so a copy of the data volume alone does not carry the CA's key. `WrapKeyStore` has a `fixed_key()` (and a `secret_file()` its sentences name); the image's store, `WrapKeySecret` (`devices/tls/wrap_secret.rs`), answers it with the file's key, and `key_file_for` seals under it instead of generating and adding one, with nothing to prune (`add` and `prune` are no-ops for it). An existing plain key is wrapped by the next start with the secret, the device listener on or off (D102), and the start says in one line what it found (D103). Without the secret: plain, with the warn line, its sentences naming the secret (D96); a key already wrapped stays closed until the secret is back or the CA is rotated (D96); a file that holds no key fails every call, error kind `wrap_key_secret` (D97). An older copy of the key file opens for as long as the secret is the same; a leaked backup ends with a new secret and a rotation (D98). The unit checks the secret before Podman is asked (D104). The secret is 64 hex digits (D95) |
| TLS key | The host directory (D8); on the desktop the file `tls.key_path` names (§4.7) | Read from the file the variable names; never copied into the data dir |
| Upstream provider keys, the Hugging Face token | Plaintext in SQLite, on the desktop too | Not yet: a design of its own for the desktop and the server alike (ruling 12). This design only warns: `docs/server.md` says that the data volume holds them in cleartext and must be protected like a password file (who can read the volume, its backups and copies, the migration copies of §6.4 included), and §3.4 lists it among the host's duties |
| Owner and agent keys | Plaintext (they are handed out again, principals §3.12) | Unchanged; door keys are revealed to their own account only (§5.1) |
| The update feed's deploy token (GitLab builds) | Compiled into the RPM's binary (`LMGW_UPDATE_DEPLOY_TOKEN`, `ci/build.sh`) | The same in the server binary GitLab builds, so a GitLab image archive carries a `read_package_registry` token: handing an archive to someone else hands them that token. GitHub release builds carry none. `docs/server.md` says so (WP10) |
| Sessions, known browsers, set-password tokens | — | sha256 of a 256-bit token |
| Passwords | — | argon2id hashes (§5) |

A Podman secret with the default `file` driver sits unencrypted in the service account's storage
on the host, so what it buys is that a copy of the data volume alone does not carry the CA's key.
systemd credentials (`LoadCredentialEncrypted=`, sealed to the host's TPM) bind it to the machine;
handing one into the container is not yet verified for a rootless user unit, and the docs mention it
as an alternative once it is.

## 10. Building the image in CI

The RPM job is the desktop's path and stays as it is (ruling 7): the server binary and the image
are built in jobs of their own after it, never in it.

- **`LMGW_BUILD_SERVER=only bash ci/build.sh`**: the same version stamp, update feed, deploy token
  and UI build as the RPM gets, then `cargo build --release -p lmgw-server`, `--version` checked
  against the version, and `dist/lmgw-server` with `dist/build.env` (`VERSION`, and
  `SERVER_FEDORA_RELEASE`, the Fedora release it linked against). No RPM, no guard, no
  `latest.json`. `-p lmgw-server` resolves `lmgw-core`'s features for that binary alone, so
  `lmgw-core` and the dependencies it resolves differently compile with another feature set than
  the RPM's: correct (no desktop features in a server), and a cost. The build runs under
  `/usr/bin/time -v`, whose "Maximum resident set size" is the largest single process's peak (in
  practice the link), printed also when the build fails; the job's time is GitLab's own.
- **`.gitlab-ci.yml`**: `server-binary` (`needs: build` for the order only, without its
  artifacts, so the two release builds never compile side by side in one pipeline; the test
  builds' thin LTO; the RPM job's cache with `policy: pull`: warm registry, tools and most of
  `target/`, and the RPM job's cache never gets the server's feature set or a second multi-GiB
  `target/` archive; the price is the differing units compiling in every run), then
  `server-image` (`needs: server-binary`), which runs `ci/build-image.sh`; its artifact is the
  archive. Both `allow_failure`, so nothing on the server side can hold back the RPM or its
  publish, which needs only `build`. Not named `image`: at the top level that is GitLab's keyword
  for the default job image. Jobs of two pipelines can still overlap when the runner takes more
  than one job at a time; whether that matters is the measurement below.
- **`ci/build-image.sh`**: takes `dist/lmgw-server` (or a path), refuses a binary with a `.symtab`
  (the release profile strips; an unstripped or debug build is not the release), copies
  `packaging/server/`'s image files, NVIDIA's key, the licence notices and the binary into a build
  context under `target/` (a test keeps the list equal to the Containerfile's `COPY`s), builds with
  `buildah bud` (`--isolation chroot`, vfs, its own store), checks inside the built image that
  `lmgw-server --version` prints the version, catatonit is executable, the entrypoint parses
  (`bash -n`) and `podman --version` runs, and writes `dist/lmgw-server-<version>.oci.tar` with
  `buildah push … oci-archive:`, its `.sha256` before the build image is removed from the store.
  The labels name lmgw's public repository (vendor `lmgw-dev`, url and source
  `https://github.com/lmgw-dev/lmgw`) over the base's Fedora ones; the base's
  `org.opencontainers.image.licenses` is removed, not replaced: it is one SPDX expression for the
  whole image (Fedora's packages and lmgw), and OCI cannot scope it to one part, so lmgw's notices
  are the files in `/usr/share/licenses/lmgw-server/`.
- **Unverified**: that buildah builds in an unprivileged job on the `docker` runner at all. In a
  rootless Podman container with Podman's default capabilities (no `CAP_SYS_ADMIN`) the whole build,
  `RUN` included, worked on 2026-11-08; a Docker engine's default seccomp profile refuses the
  namespaces and mounts buildah needs without `CAP_SYS_ADMIN`, so on a Docker-backed runner the job
  is expected to fail until it runs privileged or on a dedicated runner (not decided yet). The archive (about 250 MiB) and the binary (about 180 MiB) are also above GitLab
  CE's default maximum artifact size of 100 MiB, which the administrator raises or the job fails at its
  upload.
- **The second binary's cost.** One more whole-program link with its own peak memory, in a job of
  its own on the GitLab runner (thin LTO there). No runner has a recorded out-of-memory on a
  release build: that run was GitHub's test job compiling two jobs at once. WP2 measures the job's
  peak and time on GitLab before the job is required.
- **GitHub release workflow**: not changed yet. The same mode and script would make the archive a
  release asset beside the RPM, attached by the release the owner runs; `latest.json` needs nothing
  new: the image carries the release's version, which the notice compares (§6.3).
- **No registry** (ruling 11): the archive is the image's only form. No job pushes an image
  anywhere, and no agent ever publishes; the administrator downloads the archive and loads it
  (`podman load`, §3.3). A registry (ghcr.io, the GitLab container registry) is not yet.

## 11. Open questions

None open. The owner answered every question of the earlier draft on 2026-11-08; the answers are
rulings 8–21 (Decisions), each naming its old question number.

## 12. Tests

**Unit and integration** (no GPU, no nested Podman):
- *Start:* `lmgw-server` refuses outside a container and without `LMGW_DEV`; starts with
  `LMGW_DEPLOYMENT=container`; a second SIGTERM exits 143; the teardown runs after the server
  returned; PID 1 warns; a second start on the same data dir refuses while the lock is held; in
  container mode no Secret Service is asked.
- *Settings:* a pinned variable overrides the stored value and is written; a later start without it
  keeps the value; every save path (`settings_set_full`, `ops::settings_set` through `/api/op` and
  `lmgw__settings_set`) refuses a pinned field with its sentence; a seeded variable applies only to
  a fresh data dir; an unparsable variable ends the start naming it; `webrtc_enabled`'s save
  sentence names the restart command in container mode; `accounts.enabled` refused by every save
  path on both deployments, naming `accounts_switch`.
- *Unit file:* the shipped `lmgw.container`'s `StopTimeout` ≥ `STOP_WITHIN` + `STOP_GRACE_SECONDS`
  + the default `vram.unload_timeout_seconds` + `SHUTDOWN_TIMEOUT`; `LMGW_STOP_TIMEOUT` equals it;
  `TimeoutStopSec` above it; `StopSignal=SIGTERM`; TCP and UDP published for each WebRTC group;
  `Image=` a `localhost/` tag with `Pull=never` and no `AutoUpdate=` (ruling 11); every flag of
  the report's table present. The stop-budget warning appears when the setting
  exceeds the budget.
- *TLS:* a TLS listener serves the router; TLS 1.2 and 1.3 accepted, no client certificate asked; a
  changed certificate file is served from the next accept; a broken or mismatched new pair keeps the
  old one and shows the error; a reload that failed on the key's mode is retried once only the mode
  is fixed; a leaf ending within 14 days and a group-readable key are warned of; `covers` keeps DNS
  and IP names apart and refuses `*.<tld>`; a certificate without `*.<suffix>` gives the warning;
  `same_origin`
  accepts `https://<Host>` on `Tls` and refuses it on `Plain`; `Secure` and `__Host-` on `https`
  only; `/mcp` `origin_allowed` with the public origin's host and port, a portless `https` origin as
  443; the login line, `/api/connect`, the wiring view, a device `hello`'s `dashboard_url`, the
  pairing proposal, snippets, agent origins and `X-Forwarded-Proto` follow `public_url`;
  `mcp_url` stays plain loopback; `own_host_names` contains `public_url`'s host, and a suffix under
  it is refused; the `localhost` suffix warning in container mode and on a desktop with a TLS
  listener or a non-loopback `bind_addr`, and not on a desktop on loopback only.
- *TLS on the desktop:* the TLS keys are editable dashboard settings there and pinned in container
  mode; they carry "restart to apply" and **Restart gateway** brings the TLS listener up; the plain
  listener stays on `bind_addr` with TLS on; the tray's nonce sign-in on loopback works with TLS on;
  a TLS save leaves every URL on the plain listener until the restart, and so does a pair that did
  not load at the start (D45); the login line names the bound address (port 0); a restart serves
  the same TLS port while a request on the old connection is still in flight; two listeners on one
  TCP port are refused by the save, the container start and the environment (D46).
- *WebRTC:* a call `POST`ed on `Tls` gets the TLS group's sockets on `realtime.webrtc_tls_port`
  (unset: the TLS port's number); no plain group in container mode while the plain listener is
  loopback-only behind TLS; on the desktop both groups bound on their own ports;
  `webrtc_port_collision` covers the TLS group against the plain and the device group.
- *Proxy mode:* forwarded headers read from a trusted peer only; ignored (and stripped) from any
  other; the client address is the right-most untrusted hop, the left-most when all are trusted,
  unknown at a hop without an address and for a chain with no hops beside the other forwarded
  headers, an `X-Forwarded-For` element that is not text one such hop; both families, differing
  values, a bad scheme or host and two `Host` headers refused; the client's own `Forwarded` element;
  a trusted proxy's `https` origin gets the `__Host-` cookie and passes `same_origin` and `/mcp`'s
  guard, `http://` on the same host does not; the edge writes the client, the proxy, the scheme and
  the host and strips the headers; the networks' parsing and IPv4-mapped matching;
  `LMGW_TRUSTED_PROXIES` read, pinned and cleared when unset; the save's spelling and the patch op's
  refusal (`op_failed`, the field unknown to it); the first peers (the first trusted one after a
  scanner's, none on a desktop with proxy mode off, a save starting the record again, a request
  whose lines are taken turned away by the atomic), the pasta warning and the refusal count; the
  edge's refusals in a request span; the agent proxy telling an app the forwarded client and scheme,
  and no `X-Forwarded-For` for an unknown client; the TLS listener behind a trusted peer (the
  forwarded host's `__Host-` cookie, the refusal and the first peer on the `tls` listener); the
  realtime handshake's origin under each scheme; `Conn::is_direct_loopback` false for every proxied
  request.
- *Accounts:* a fresh database gets account 1 with `owner:dashboard` as its door; an existing one
  gets account 1 and every owner row attached at its first start; a re-seeded `owner:self-admin` is
  attached; `key_create kind=owner` attaches to the caller and refuses `owner:account-…`; door keys
  named by id; another account's door key refused by reveal, rotate and set, every door key by
  delete; password set, verify, rehash on changed parameters, NFKC and code-point counting, 12 code
  points accepted and 11 refused with the minimum named, no blocklist (a commonly used password of
  12 characters is accepted, ruling 20); unknown, disabled and password-less names give the same
  code and run one verification each; the transport rule refuses plain non-loopback; logins outside
  the alphabet refused; account 1 undeletable and undisableable; self-disable refused; a self-link
  refused; a disabled account's owner keys and sessions refused naming it, its client keys still
  working; "made by" survives the creator's delete; the CLI through a running gateway, offline under
  the lock, and refused offline while the lock is held.
- *Sessions:* with accounts on every sign-in path (the tray's nonce on the desktop included) sets a
  session cookie and never a key, and an owner-key cookie reads as anonymous; an expired or deleted
  session is anonymous; sign-out deletes the row; Sign out everywhere, a password change, another
  account's password set by an admin (ruling 21) and a disable end the sessions they should and the
  open streams on them; the lifetime follows `accounts.session_days`.
- *Desktop, accounts off (nothing changed):* a desktop data dir with the setting at its default
  behaves as today: the login view has no name or password field, the tray's nonce sets the
  `owner:dashboard` cookie, an owner-key cookie is honoured, the log prints today's login line,
  `GET /api/session` has no `account`, no `sessions` or `audit_log` row is written by any request,
  every account op but `accounts_switch` answers `409 accounts_off`, Require API key stays off.
- *The switch (§5.11):* refused in container mode, from the `lmgw__*` plane and through every
  settings save path. On: the caller's answer carries a session cookie for its account, other
  browsers' owner-key cookies read as anonymous, the first audit row is the switch, the no-password
  banner shows for account 1 and its card form sets the password and the login without a current
  password. Off: every session row deleted and its streams closed; other accounts' door keys and
  owner keys refused naming the account, account 1's keys working, client, device and agent keys
  any account made working; accounts, password hashes, known browsers and audit rows kept (counts
  equal before and after); a set-password link refused with `409 accounts_off`; account 1's caller
  given the door-key cookie, another account's caller signed out; the switch the log's last row;
  the kept-line's counts and size. On again: the paused keys work, the old passwords verify, no
  session from before the switch-off works.
- *Throttle:* the doubling per key, its cap, `Retry-After`, the reset on success; unknown names
  throttled as known ones, keyed by their hash; a known browser held only by its (name, token)
  pair, also through a spray from its own address; IPv6 by /64 and /56; a capped entry kept past
  its delay; concurrent attempts inside one window refused (D69); a failure answered without a
  snapshot reload; a wrong current password counted; `503 login_busy` when no permit is free, and
  the permit kept for known browsers (D67).
- *Races* (D70, D74): a start whose account's password changed or that was disabled is refused by
  its own insert and leaves no row; an end between a start's read and its admission keeps it off
  the desk; two submits of one set-password link set one password; a disabled account's link
  refused and kept.
- *Login links:* in container mode the start prints a set-password link while account 1 has none,
  a fresh one each start, no key ever, and only the command afterwards; on the desktop with
  accounts on, today's line and no set-password link; `login-link` works once and expires.
- *Audit:* one row per non-`GET` `Admin` request, per writing `lmgw__*` call, per sign-in event, per
  offline CLI write and per switch; none on a desktop with accounts off; a retention change written
  before it removes a row; the route walk shows every `Admin` write route reaching the gate's
  writer; no value of a redacted field in any row.
- *Container mode words:* every §7 row's sentence from fake facts; the metered probe not run.
- *stderr filter:* the known line dropped, any other line kept.
- *Migrations copy* (ruling 14): a start with a pending migration writes the copy before the
  repairs and logs its size and time; none without one; none on the desktop, with accounts on or
  off; no copy ever deleted or restored by lmgw; a start a read-only guard refuses makes none, and
  starts refused after the copy make one until the database changes (D113); `knowledge.db` and
  `quickdoc.db` copied as well (D117); a start stopped inside a migration without a transaction
  reusing the copy while the marker stands, a part-applied copy behind the whole one (D118); an
  unreadable database reusing the copy (D119).
- *Update notice* (ruling 13): in container mode a newer feed version gives the About line and the
  log line naming the load-and-restart path, and nothing is downloaded; with `update_check_enabled`
  off no request is made.
- *Route walk* (principals §10): the new routes in `CAPABILITY_TABLE`; the op docs for the new ops;
  `x-lmgw-client` among the documented headers.

**Live, with the GPU**, each in its WP: §13's verification lists.

## 13. Work packages

In build order; each ends with the project's checks (`ci/check.sh`). **They run strictly in
sequence**, because several share files: WP1 and WP6 `crates/lmgw-server/src/cli.rs`; WP4 and WP6
`principal.rs` and `web/session.rs`; WP2, WP4 and WP8 `packaging/server/lmgw.container`; WP3, WP9
and WP1 Settings → About; WP1, WP4 and WP6 `config/settings.rs` and the shared settings write;
WP4 and WP6 the Settings → Network & access page.

- **WP1 — The server start.** New crate `crates/lmgw-server` (`main.rs`, `cli.rs`, `env.rs`), the
  workspace members, `lmgw-core/src/config/deployment.rs` (new: `Deployment`, pinned and seeded
  settings, §2), the `Deployment` field and `wrap_key_store` returning `None` in container mode in
  `state.rs`, the data-dir lock (D28), the pinned-field refusal in the shared settings write
  (`ops/settings_patch.rs`, `web/api_settings.rs::settings_set_full`, reached by
  `lmgw__settings_set`), the settings answer's `pinned` list in `lmgw-api-types` and the read-only
  rows in `crates/lmgw-ui/src/pages/settings/`, the stop-budget comparison (D20) on Settings →
  About. Tests of §12's start and settings groups.
- **WP2 — The image and the plain-HTTP unit.** `packaging/server/` (Containerfile, entrypoint,
  containers.conf, storage.conf, the toolkit repo file with D19, `lmgw.container` without the WP4
  lines, three `.volume` files), `ci/build-image.sh`, the `server-binary` and `server-image` jobs
  in `.gitlab-ci.yml`, `LMGW_BUILD_SERVER=only` in `ci/build.sh`, the unit test (D20). **Verification**: buildah in the CI job (§10) and
  the job's peak memory and time; the archive's update path (ruling 11: the name `podman load` gives
  it, `podman tag … current`, a restart picking the new image up, `Pull=never` failing clearly
  without one); the unit as written under Quadlet (CgroupsMode `split` against the entrypoint's
  cgroup leaf; `AddDevice=` with a CDI name, else `PodmanArgs=--device`; health, `Notify=healthy`;
  the removal time that sets `TimeoutStopSec`); the `/tmp` writers under the tmpfs; a model planned,
  created and tested; **an engine build inside the nested container** (`backends/run/podman.rs:436`,
  a CUDA `podman build`, its cache mounts in `/var/tmp` surviving a recreate); **a pull through the
  private pull service** (`backends/pull/service.rs`, its socket under `/run/user/0`); **an agent
  run** (script and service, `gateway_access`, a host mount through the outer container); **the
  audio and image classes**; **the outer container on a user-generated CDI spec**; the inner start's
  stderr noise; a WebRTC call on the plain group through the published UDP port.
- **WP3 — Container mode for the machine check and the report's items.** `lmgw-core/src/machine/`
  (`running.rs`, `prefetch.rs`, the words), `runtime/registry/command.rs` (the stderr filter), the
  host-mount placeholder in the UI, the update notice in `update.rs` and the server start (§6.3,
  ruling 13: the poll, `update_check_enabled`, the About and log lines), the restart unit shown on
  the desktop only and `webrtc_enabled`'s container-mode sentence. Tests of
  the words and the filter. Live: the check's sentences in a container with and without the card
  passed in. Built on its branch, with the review's fixes (D30, D31). The "Could not locate
  device" line did not reproduce on Podman 6.1.3 here; the filter's pattern is the podman binary's
  format string `Could not locate device %s on host`. A real model start inside the image is not
  yet verified.
- **WP4 — TLS, the public origin and everything that builds a URL.** `server/listeners.rs` (new,
  out of `server.rs`: `bind`, `bind_tls`, `serve`, `serve_with`), `server/conn.rs` (`Via::Tls`, `scheme_of`), `server/stop.rs` (the TLS
  listener's own generation), `server/main_tls.rs` with `main_tls/listener.rs` and `names.rs` (new:
  the reloading pair, D8, D34; the accept loop with per-connection handshakes, D36; the names check,
  D37), `config/settings/tls.rs` (new: `tls`, `public_url`), `config/deployment.rs` and
  `deployment/apply.rs` (the five TLS variables pinned, D32), `Cargo.toml` (rustls `tls12`),
  `principal.rs` (scheme-aware `same_origin` and cookie, D43), `web/session.rs` (`Secure`,
  `__Host-`), `mcp/ingress.rs` (D38), `net.rs` with `net/public.rs` (new: `public_base_url`,
  `device_dashboard_url`, `own_host_names_for`, `reached_from_elsewhere`), the callers in
  `web/api.rs` (`/api/connect`, D42), `web/wiring.rs`, `web/chat_feed.rs`, `devices/pairing.rs`,
  `agents/service.rs` (origins, D33; the D27 warning; `mcp_url` left as it is),
  `web/agent_proxy.rs`, `realtime/calls.rs` and `config/settings/webrtc.rs` (the TLS group and
  `realtime.webrtc_tls_port`, D22, D44), `web/api_settings/tls.rs` (new: the save, D41),
  `lmgw-server`'s `env.rs` and `start.rs`, the WP4 lines of `lmgw.container` and the
  Containerfile's `EXPOSE` (D39, D40), Settings → Network & access → TLS
  (`crates/lmgw-ui/src/pages/settings/tls.rs`, new) and the TLS UDP port row, the agent tab's D27
  line. The desktop shell's Restart gateway serves again through `server::serve`, which binds the
  TLS listener with the stored settings; the old one's port is released before `released` fires.
  Tests: `tests/it/main_tls` (listener, scheme, webrtc), the deployment, env, packaging, principal,
  session and names unit tests. Built on its branch (D32–D46), with the review's fixes: URLs from the
  serving listener (D45), the stamps' change time and the checks off the workers (D34), typed names
  (D37), the paired `/mcp` ports (D38), the TCP check (D46), the card's warnings, and the restart
  test holding a request in flight on the old connection while the next start serves the same
  port. Live on 2026-11-08 (Podman 6.1.3,
  container-selinux 2.251.0, curl and OpenSSL of Fedora 45), the debug `lmgw-server` in container
  mode with `LMGW_DEV` on loopback, then inside `wp2-test` plus that binary with the unit's flags
  (no card) and an outer port other than the inner one (`public_url` naming 18901, the listener on
  8443): `curl --cacert` against the TLS port; TLS 1.2 and 1.3 with ALPN `http/1.1` and "No client
  certificate CA names sent" (`openssl s_client`); the plain listener on the container's loopback
  and unpublished, its WebRTC group not bound, the TLS group on UDP 8443; the plain listener moved
  from a stored `0.0.0.0:18899` to `127.0.0.1:18899` when TLS came on (D32); `Set-Cookie:
  __Host-lmgw_session=…; Secure` on a TLS sign-in and a login link, the plain name without `Secure`
  on the plain one; with that cookie `/api/status` 200 from the `https` origin and 403 from
  `http://` on the same host and port; a plain cookie on TLS anonymous; `/mcp` refusing `http://`,
  a portless `https://localhost` and a foreign host, accepting the listener's and the public origin's;
  `/api/connect` and the wiring view on `public_url`, the login line on it; a pinned `public_url`
  save refused naming the TLS card's row; a renewed pair served from the next connection (serial
  1001 → 1002 → 1003, in the container 2001 → 2002) without a restart, a certificate written beside
  a key that is not its own kept the old pair with the reason on Settings until its key followed; a
  public name the leaf does not cover and a suffix without its `*.` entry warned in the log and on
  Settings; a suffix under the public host refused (`origin_suffix_shadows_gateway`). From the
  host's loopback through the outer pasta, lmgw sees the namespace's own address (192.0.2.10) as the
  peer. Not run yet: a browser on another machine in a secure context (dictation, copy),
  against the image and against a desktop with a TLS listener; an agent origin under a wildcard
  certificate and its own suffix; a device paired through the published 8788 with the public name
  in its leaf; **a WebRTC call through the container on the TLS group** (§4.6); the peer address
  lmgw sees for a connection from another machine; the unit under Quadlet with `%h`.
- **WP5 — Proxy mode** (ruling 8's second step). `config/settings/proxies.rs` (new: the networks,
  D47), `trusted_proxies` in `Settings`, `Pinned::TrustedProxies` in `config/deployment.rs` and
  `deployment/apply.rs`, `LMGW_TRUSTED_PROXIES` in `lmgw-server`'s `env.rs` (its WP1 refusal
  gone), `server/forwarded.rs` with `forwarded/headers.rs` and `forwarded/seen.rs` (new: the
  reading, D48–D50; the edge, D51; the first peers and refusals, D53), `server/conn.rs`
  (`Conn.proxy`, `Conn.forwarded_scheme`, `Conn::scheme`), `RequestCtx.scheme` and its readers
  (`server/gate.rs`, `web/session.rs`, `mcp/ingress.rs`), the request span in `server.rs`,
  `web/agent_proxy.rs` (the client, an unknown one left out), `server/conn/loopback.rs`
  (`Conn::is_direct_loopback`, D51, for WP6), the realtime handshake's origin test in
  `realtime/handshake.rs`, `Snapshot.trusted_nets` (the list parsed at each publish), the save in
  `web/api_settings/tls.rs`, `ProxyStatus` in `lmgw-api-types`, the TLS card's warning in
  `main_tls/slot.rs` (D52), and the Trusted proxies row with a proxy-mode panel on Settings →
  Network & access → TLS. Tests: `tests/it/server_forwarded` (with `agent_proxy` and `tls`), the
  unit tests of the networks, the headers, the walk, the first peers, the realtime origin and
  `Conn::is_direct_loopback`, the deployment and env tests. Built on its branch
  (D47–D53). Live on 2026-11-08 with nginx 1.29.5 (`docker.io/library/nginx:alpine`; Caddy was not
  in the local store), Podman 6.1.3, curl 8.21: the debug `lmgw-server` in container mode with
  `LMGW_DEV` on `127.0.0.1:18902`, `LMGW_TRUSTED_PROXIES=127.0.0.1`, `LMGW_PUBLIC_URL=https://
  lmgw.example:18904`, nginx terminating TLS with a self-signed pair on `127.0.0.1:18904`: a
  sign-in from `https://lmgw.example:18904` got `__Host-lmgw_session=…; Secure`, that cookie gave
  `/api/status` 200 from the `https` origin and 403 from `http://` on the same host and port,
  `/api/connect` answered the public URL, `/mcp` refused `http://` and a foreign origin and let
  the `https` one through; a client on `127.0.0.2` sending `X-Forwarded-For: 198.51.100.66,
  127.0.0.1` and `X-Forwarded-Proto: http` through nginx was logged as `client=127.0.0.2
  scheme=https proxy=127.0.0.1`, and the same headers sent to lmgw directly from `127.0.0.2` read
  `client=127.0.0.2 scheme=http`, and a direct sign-in with the `https` origin was refused (403);
  `/v1/realtime` answered `101 Switching Protocols` and `session.created` through nginx. Then the
  same in one pod (`localhost/lmgw-server:wp2-test` with the debug binary, the unit's flags
  without the card, nginx beside it on the pod's loopback): the `__Host-` cookie, `/api/status`
  200, `/api/connect` on the public URL and the `101` through nginx; the client through nginx and
  a direct request on lmgw's published port both showed as 192.0.2.10, the direct one with
  forwarded headers ignored and its sign-in refused, the first-peer line saying "this machine's own
  address"; trusting 192.0.2.10 as well gave the pasta warning in the log and on the card, and a
  host-loopback `curl` then wrote its own client address (198.51.100.77) and scheme. Not run yet: Caddy and Traefik in front (their header defaults, Traefik's `wss`), the peer lmgw and a
  proxy in its pod see for a connection from another machine through the outer pasta, a proxy on
  another host, a browser through the proxy (dictation, copy, the dashboard's own requests), a
  re-encrypting proxy to the TLS listener.
- **WP6 — Accounts and sessions, on both deployments.** Migration 0101 (with D54);
  `lmgw-core/src/accounts.rs` with `accounts/` (`password.rs`: argon2id, NFKC in one function,
  `PASSWORD_MIN_CHARS` = 12, no blocklist, the verification permits; `throttle.rs`; `links.rs`;
  `sessions.rs`: the session desk on every published snapshot; `ops.rs`; `switch.rs`), "accounts
  on" as `Snapshot::accounts_on`, set at each publish (D25); `config/settings/accounts.rs`
  (`accounts.enabled`, refused by every save path, and `accounts.session_days`, D61);
  `store/accounts.rs`; `agents/token.rs` (account 1 in `seed_owner_keys`, the account through
  `write_owner_key`); `store/keys.rs` (the account on owner-key inserts, `made_by_login`);
  `web/api_settings/doors.rs` (D56, `key_create`'s account and reserved prefix);
  `principal.rs` with `principal/cookie.rs` (the account and session on the principal, the
  session cookie, the owner-key cookie refused while accounts are on, `account_disabled`,
  `key_paused`); `devices/revoke.rs` (D58) and `/api/events`' watch; `web/session.rs` with
  `session/nonces.rs` and `session/password.rs` (the password route, sessions on every sign-in
  path, sign-out, `GET /api/session`'s account, `SESSION_BODY_LIMIT`); `web/accounts.rs`
  (`/api/account/password`, the account ops' dispatcher); the ten ops in `op_names` and their
  `OpDoc`s (`ops-accounts`), the route's `DocRoute`, `x-lmgw-client` among the headers;
  `lmgw-api-types` (`accounts.rs`, `SessionView`, `KeyRow`'s `made_by`, `paused`, `door_of`,
  the settings answer's `accounts_status`); in `crates/lmgw-ui` the sign-in card's name and
  password with the key under a fold, the set-password page, Settings → Network & access →
  Accounts (the accounts with a row menu, their sessions with End, Add account with its link,
  set password, the session lifetime, the no-password banner, the switch with its
  confirmations and the kept-line), "made by" and "paused" on Keys and Devices, the signed-in
  account and Sign out in the sidebar's foot; `lmgw-server`'s `account.rs` and `cli.rs` (D57);
  the container start's ruling-15 lines (`accounts::start_lines`); after the review, D67–D74
  (the throttle's reservations, the known browsers' pair and permit, the session start's re-check
  and end generation, the link spent in its transaction, the CLI-only login link, no default
  account, one session per browser) and the agent MCP face under the session watch; after the
  second round, D75–D82 (the commands' refusals, no proxy, no offline migration, `--login`, the
  offline link's origin, `Retry-After` on the password op, the lock's host, made-by on today's
  desktop and the dashboard's sign-in card, `revoked` frame and confirmations). Tests:
  `tests/it/accounts` (seed, desktop off, switch and container mode, sign-in, the throttle,
  sessions, links, keys, the agent MCP face's stream, the start's sign-in lines from a captured
  log), `lmgw-server`'s `tests/it/account` (the commands offline, through a served test gateway,
  refused on a held lock and an unmigrated database, and the binary's refusals and its calls past
  a proxy the environment names) and the unit tests of the password rules,
  the throttle, the session desk, the logins and the commands' parsing. One verification's time: 95 ms (§5.10). Live on 2026-11-08 (Podman 6.1.3, curl and
  OpenSSL of Fedora 45, the debug `lmgw-server`): a desktop dev instance with the switch at its
  default behaved as before (the token link's year-long owner-key cookie honoured, `account_list`
  `409 accounts_off`, no `sessions` row); with a self-signed TLS pair on its TLS listener, the
  switch on from the `https` origin answered `__Host-lmgw_session=lmgw-session-…; …;
  Max-Age=2592000; Secure` and the old owner-key cookie then read `401`; account 1's password set
  through a set-password link over TLS (the link spent after one use), a password sign-in over
  TLS set `__Host-lmgw_known` and `__Host-lmgw_session`, both `Secure`; sign-out closed that
  browser's `/api/events` stream within 27 ms with a `revoked` frame; a second wrong password
  answered `429 login_throttled` with `Retry-After: 1`, and after the wait a known browser signed
  in; an account `sam` added, then the switch off (account 1's browser handed the door key's
  cookie, three sessions ended, sam's door key `key_paused` naming sam, account 1's key working,
  accounts and known browsers kept) and on again (sam's key and password working, sam's old
  session not). `lmgw-server` in container mode on a fresh dir printed a set-password link and no
  key, then `account list`, `account set-password owner` (piped) and `login-link` through the
  running gateway, the link signing in once, and at the next start only "sign in at …"; with it
  stopped, `account list`, `account link owner` and `account set-password owner` on the data dir
  (the password working at the next start), `login-link` refused, and a lock held by another
  process refused naming it. Then in `localhost/lmgw-server:wp2-test` plus the debug binary with
  the unit's flags (no card, no TLS): the same first-start link and later line, `podman exec …
  lmgw-server account list`, `set-password` and `login-link`, a one-off `podman run --rm -v
  <volume>:/data … account list` and `account link owner` with the service stopped, and the same
  one-off refused naming the running container's pid while it ran. Without TLS, the link opened
  from the host's own browser through the published port is refused (`403 login_insecure`): the
  outer pasta shows lmgw the namespace's address (192.0.2.10), not loopback (§4.5), so the image
  without TLS takes no password from anywhere but inside the container; `login-link` (no
  password) is the way in until TLS is set. Not run yet: a password sign-in from another
  machine over the desktop's TLS listener, and the browser flows by a human (the sign-in card,
  the set-password page, the Accounts card, the switch's confirmations).
- **WP7 — The audit log.** Built (2026-11-08): `audit_log` (migration 0102) and
  `accounts.audit_days` (D88) first; the writer at the gate (`audit/at_gate.rs`) for non-`GET`
  `Admin` routes, reading ops left out by the op table's marks (D84); the writing `lmgw__*` calls
  in `mcp/selfadmin.rs` (`audit::ToolCall`, from `/mcp/admin` and the Chat); the session routes'
  events (D86); the command line, online through the gate with `via = cli` and offline with
  `via = cli-offline` (D90); the desktop's switch in both directions; all only while accounts are
  on. The redaction list is the API document's `x-lmgw-secret` names (`audit::redact`), held by
  `audit::redaction`; `audit_list` and the Accounts card's Audit log row, a page at a time with
  the log's size and oldest entry (D89); the kept-line and the switch-off sentence name the log
  (D66). The Activity list with its account filter (ruling 18's follow-up) came with the review
  round below. Tests: the `audit` group of
  `tests/it` (the route walk sends a request to every `Admin` write row) and `lmgw-server`'s
  `account` cases (the entries online and offline, no password or link token in them). Live, in a
  container-mode dev instance of the debug binary (no container, no card) and the headless desktop
  dev instance, on scratch data dirs, 2026-11-08: a set-password link, a password sign-in and a
  failed one, a settings save, a client key made and deleted, an owner key made and revealed, a
  `lmgw__hold_set` through `/mcp/admin` with that key, the offline `account set-password`, a sign-in
  with the new password and a retention of 30 days, each an entry on the card with its account,
  way in, time and target; the two entries from 2020 and 2021 seeded by hand gone and the
  retention's own entry kept; none of the passwords, the client key, the owner key or the link's
  token in `audit_log` (the owner key's plaintext is in `api_keys`, where `key_reveal` reads it,
  and in no other table); on the desktop with accounts off a key and a settings save wrote
  nothing, then the switch on, a key and the switch off wrote three entries and the key after
  nothing.
  **Review round** (2026-11-08): the sign-in routes' entries detached and collapsed (D91), a
  throttled attempt once per hold; the gate reading an op's body under its route's limit; leaf
  redaction inside new objects and arrays, the guard walking the settings' JSON schema and integer
  fields; a settings op's own patch (D85); prose, command lines and URLs' sign-ins (D92); the
  log's size read only by `audit_list` and a count error said on the card; the prune after the
  commit in batches; `before` and the account filter as their own statements over the new
  `audit_log_account` index; the desktop's tray and window (D93); refused tool calls (D94);
  `READS` (D84); `via = agent` and `api` for the tool plane's other callers; the session in a
  sign-in's principal; the Activity list with its account picker, actions in words and
  "×N". Live, in the headless dev instance of the debug build on a scratch data dir, 2026-11-08:
  two floods of 200 garbage login links from loopback within a minute were one entry, ×400; the
  list filtered by an account held only that account's entries (its link, its sign-in, its key,
  each named "sam, session #2" after the sign-in), "none" only the collapsed failure; none of the
  passwords, the link's token, the keys made, the garbage token or the system prompt in any table
  but where they belong (the dashboard key in `api_keys.key_plain`, the prompt in `settings`); the
  list at 125 % in headless Chrome fitting the pane once Who and What wrap.
- **WP8 — Secrets.** Built (2026-11-08): `WrapKeyStore::fixed_key` and `secret_file`
  (`devices/tls/secret.rs`, both `None` by default); the image's store `WrapKeySecret`
  (`devices/tls/wrap_secret.rs`: the key, no file, or a file that holds none, read once per start,
  D95, D97, D98); `devices/tls/trust.rs` sealing under the fixed key in `key_file_for` (creation,
  the re-wrap of a plain key, each rotation; nothing added or pruned, D99) and naming the secret in
  the sentences of a missing, replaced or unusable one; `state.rs::wrap_key_store` in container
  mode returning `WrapKeySecret` (also without the file, D96), dev instance or not (D100), and its
  start line (used, missing, a wrapped key whose secret is missing, unusable), with the secret's
  path a `Host` field (`wrap_key_secret`) so tests hand their own file; `Secret=lmgw-wrap-key,
  mode=0400` in the unit with how the secret is made and backed up (D101); the device CA's
  `key_mode` and error kinds documented for the image. Tests: `devices_trust::wrap_secret` in
  `tests/it` (a new CA wrapped and reopened, a rotation under the same secret, a plain key wrapped,
  the secret removed, restored and the CA rotated plain, another secret, every malformed file),
  `state`'s unit test, and `lmgw-server`'s `start_serve` (three container starts on one data dir:
  with, without and with the secret again) and `packaging` (the unit's `Secret=`). Live
  (2026-11-08, Podman 6.1.3): a Fedora 45 container with `--systemd=always --secret …` read the
  secret at `/run/secrets/<name>`, mounted below the `/run` tmpfs; `--secret` naming a missing
  secret ended `podman run` with exit 125; `quadlet -dryrun -user` passes `--secret
  lmgw-wrap-key,mode=0400`. Then four starts of the image (`localhost/lmgw-server:wp2-test` with
  this build's debug `lmgw-server` copied into each container, the unit's flags, no card, a scratch
  data volume with the device listener switched on): without the secret the CA was made plain with
  the warn line; with it the next start wrapped the same CA (`LMGW WRAPPED PRIVATE KEY`, 0600) and
  the device listener served; without it again the start's warn line and the listener's refusal
  named the secret; with it once more the CA opened and served. The secret's hex was in no file
  of the data volume. Not run: a start under Quadlet and systemd itself (WP2's open item).
  Review round (2026-11-08): `fixed_key()` answers `Some(Err)` for an image store without a usable
  key, so `key_file_for` asks it before any wrapping key is made; `TrustError::WrapKeySecret` and
  the error kind `wrap_key_secret` (D97); the rotation's comments (`trust.rs`'s `make` and
  `replace`, `device_keys.rs`'s `ca_rotate`) true for both stores, and the leak's recovery (D98);
  `state.rs::wrap_key_place` choosing the store and `Trust::start_on_secret`
  (`devices/tls/trust/secret_start.rs`) installing the image's, wrapping a plain key at the start
  (D102) and logging the one line keyed on the key file (D103); the unit's `ExecStartPre=` check
  (D104); the Devices card's key mode worded for the server image ("key wrapped under the
  server's wrapping key secret", "key plain (no wrapping key secret at the server's start)"), where
  it said the Secret Service. Tests: `devices_trust::wrap_secret`'s leak recovery in its order,
  `devices_trust::wrap_secret_start` (each start line's content and level through the tests' log
  capture, and a plain key wrapped at the start with nothing opening the CA: status `stored`,
  `wrapped`, same fingerprint), `lmgw-server`'s `start_serve` (a container start wrapping a plain
  key before the listener opens it), `packaging` (the `ExecStartPre=` script: one quoted word,
  the same command as lmgw's messages, the plain way out, no `%`, `$` or backslash), the UI's
  `ca_words` test. Ran: `quadlet -dryrun -user` and `systemd-analyze --user verify` on a scratch
  copy of the unit, and the check's script against a secret name that does not exist (the message
  and exit 1).
- **WP9 — The migration copy** (ruling 14). `store.rs` ahead of `repair_before_migrations`,
  container mode only, the copy's and the migrations' time in the log, the refusal's sentence, the
  list on Settings → About. Tests of the copy.
  **Built** (2026-11-08): `store/migration_copy.rs` (new: `before_migrating`, the copy by `VACUUM
  INTO` into a file made with `create_new` at `0600` under the first free name, the refusal when
  it cannot be made, `refuse_newer` and its sentence, `migration_copies`, `backups_dir`);
  `store.rs`'s `open_for_start(path, backups)` (`open` is it without a backups dir), `refuse_newer`
  at the top of `run_migrations` and in `open_current(path, backups)`; `fresh_install::open_db`
  taking the deployment and passing `<data dir>/backups` in container mode only; the offline
  command line passing its data dir's backups dir; `MigrationCopy` and the settings answer's
  `migration_copies` in `lmgw-api-types` (a field of `GET /api/settings-full`, no new route or op),
  filled in container mode only; Settings → About's "Copies before migrating" (D111) and its
  subtitle. Decisions D105–D112. Tests: `tests/it`'s `migration_copy` (the copy before the 0018
  repair with its mode, the dir's and the two log lines; a new database and nothing pending
  without a copy, an earlier copy kept byte for byte; a taken name moving to `-2`; a backups path
  that is a file refusing the start with the database left unmigrated; the desktop with accounts
  off and on making none; the settings answer's list in container mode and none on the desktop;
  a newer database refused at the start, on the desktop and on the offline open, the newest copy
  named, with this build's own schema and with no copy), the module's unit tests (the name read
  back, the desktop's sentence), the UI's `a_migration_copy_reads_as_its_size_time_and_step`, and
  `lmgw-server`'s `migration_copy` (a container start on a database one migration behind copies
  it and the next start does not; a container-mode dev instance copies too; the offline `account
  list` and the start refusing a newer database, the copy named). The old databases are this
  build's migrations run up to a version, as `migrations::db_at_version` does.
  Live (2026-11-08, the debug `lmgw-server` as a container-mode dev instance on a scratch data
  dir, no container, no card): the first start on a new data dir made no copy; with migration
  0102 rolled back by hand (its table dropped, its row deleted) the next start copied the
  database to `backups/lmgw-0101-0102.sqlite` (644 KiB, `0600` in a `0700` dir) and logged both
  lines (the copy and the migration each under 0.1 s at this size); `GET /api/settings-full`
  listed it; Settings → About listed it and a second copy at 125 % in headless Chrome (1536×864,
  scale 1.25), every row within the card; with a migration 9999 recorded by hand, the start and
  the offline `account list` refused with the sentence naming `lmgw-0101-0102-2.sqlite`, and no
  copy was made.
  **Review round** (2026-11-08): `store/migration_copy.rs` split into `names` (the names of all
  three databases' copies, the partial's, the list with what cannot be read: `CopyList`),
  `make` (the dir made or tightened, partials removed, `VACUUM INTO` the partial, synced,
  published by `link()`, the refusal), `source` (the `.source` record, D113) and `newer` (the
  refusal); `migration_guards/before_copy.rs` (the read-only guards ahead of the copy) with
  0095's refusal split from its writes (`refuse_unreadable_global`); `store.rs`'s `migrate` (the
  repairs on, the record just before the migrator, the guards that ran early not asked again,
  `refuse_newer` once); `foreign_keys::pending_versions` (the copy's step reads it);
  `migration_copy::around_open` around the corpus's and the knowledge bases' opens in
  `state.rs`, with `knowledge::store::migrator` and `quickdoc_core::store::migrator`;
  `MigrationCopy.database` and `SettingsFull.migration_copies_error`; Settings → About's note,
  rows and warn line. Decisions D113–D117; D105, D106, D108, D109, D111 and D112 changed.
  Tests added to `tests/it`'s `migration_copy`: a start the price-row guard refuses twice making
  no copy; a database from before 0018 refused by the alias guard after the copy and the repair,
  three starts making one copy (two "still matches" lines, the copy as before the repair), and a
  write between making `-2` holding it; a partial left by a killed start removed and logged,
  never listed, a `0755` backups dir set to `0700`; `knowledge.db` and `quickdoc.db` one
  migration behind copied under their stems and listed; in the newer test, two copies of one
  `<from>` won by the newer file's time, and a backups path that is a file said as "cannot be
  read" in the refusal and the list. The modules' unit tests: the three stems' names and the
  partial's, "a newer or different build" for a migration below the newest. The review round ran
  no live check (the earlier live check above is the first build's).
  **Second review round** (2026-11-08): `migration_copy/reuse.rs` (new: the reuse decision with
  its lines, D113, D118, D119, and whether a new copy is part applied); `source.rs`'s record with
  `migrator_entered` and `part_applied`, read back as a `Record` compared to `Same`, `Entered` or
  `Differs`, and the dir synced after its rename; `Copied::entering` ahead of the migrator in
  `store.rs`'s `migrate`; the step knowing whether a pending migration runs without a transaction;
  `foreign_keys::check_pending`; `newer.rs` ranking a whole copy ahead of a part-applied one of
  the same `<from>`; `make.rs`'s partial line naming `TimeoutStartSec` and the database's size,
  the copy's dir sync logged, a linked dir's mode left alone and the setgid and sticky bits kept
  (D120). Decisions D118–D120; D113 and D114 changed. Tests added (`tests/it`'s
  `migration_copy::stopped`): a start failing inside 0086 part applied (a view on the old table
  failing its rename after the drop) writing `migrator_entered`, the next start after a write reusing the copy, a
  cleared marker making `-2` marked part applied, and a newer build's refusal naming the whole
  copy over the newer part-applied one; a database whose file time cannot be read reusing the
  step's copy, and copied when the step has none; a linked backups dir keeping its target's
  `0755` with the warn line, a `2775` dir becoming `2700`; the partial test's line naming
  `TimeoutStartSec`. No live check.
- **WP10 — Documentation.** `docs/server.md` (new: §3.4's prerequisites, the rootless unit for a
  service account with linger, the volumes, the warning that the data volume holds provider keys in
  cleartext and must be protected like a password file (ruling 12), TLS and renewal, the device
  CA's wrapping key secret (made before the first start, backed up on its own, the unit's check and
its journal message, and the recovery when a volume backup leaked with the secret: a new secret, a
restart, a rotation; §9, D98, D101, D104), agent origins
  on a server, accounts and recovery, the update path by archive (load, tag, restart) and what a
  migrating release means for a manual rollback, that a GitLab-built archive carries the update
  feed's deploy token (§9), and that `lmgw-buildcache` holds staging copies of user content);
  `docs/accounts.md` (new: the desktop's accounts
  switch, what turning it on and off does, TLS on the desktop for sign-in from other machines); the
  README's pointers; this record's row in `docs/design/README.md` updated to "built".
  **Built** (2026-11-08): `docs/server.md` (the host's needs, the service account, the archive and
  its deploy token, the wrapping key secret with its backup, its absence and the leak's recovery,
  the unit and what its numbers are for, the volumes and a backup by `podman volume export`, the
  cleartext warning, TLS with a certbot deploy hook that copies and renames inside the directory,
  the plain-HTTP variant and what it loses, proxy mode with the four proxies' header lines, agent
  origins, accounts and the command line online and offline, updates, the migration copies and a
  manual rollback, the pinned and seeded variables, Not yet), `docs/accounts.md` (the switch on
  and off as `accounts/switch.rs` and the Accounts card do it, sign-in from other machines, the
  desktop's TLS rows), the README's Install, Remote use and Documentation pointers, and this
  record's row. Where this record and the shipped files differ, the docs follow the files: §3.3's
  unit block has no `Secret=` and `ExecStartPre=` lines, which `packaging/server/lmgw.container`
  has (§3.3's prose and WP8 describe them). The offline command line's form lacked `-it`, which
  `account set-password` needs to read the password from a terminal: added to `lmgw-server
  --help` (`cli.rs`'s `USAGE`) and to §5.7. Ran: `lmgw-server --help` of the debug binary, and
  `quadlet -dryrun -user` on a scratch copy of the unit and its volumes (the generated `podman
  run` as the docs describe it). Not run: the docs' deploy hook, the volume export, the rollback
  recipe in a one-off container, `machinectl shell` into the account, and the Caddy, Traefik and
  HAProxy lines (§4.5); a Quadlet pod for proxy mode is not shipped yet, so the docs describe the
  safe layout without a unit for it.

## Not yet

- The shared variant (the host's store read-only, `label=disable`), its read-only image handling
  and the store-volume separation it would need (ruling 3).
- A non-root user inside the outer container (the report: own store, file caps, `cap_setfcap`
  untried).
- Per-account data ownership (private chats, threads, keys, usage) and roles other than full admin
  (rulings 5, 6).
- A registry for the image (ghcr.io, the GitLab container registry), and with it
  `AutoUpdate=registry`, `podman auto-update` with its health-based rollback, and the manifest's
  `image { reference, digest }` (ruling 11; not yet). Quadlet's `AutoUpdate=local`,
  which follows a locally retagged image, would work without one; it is not tried yet.
- A rootful system unit (ruling 10).
- The migration copy on the desktop (ruling 14 keeps it in container mode).
- Copying all three databases before any of them migrates: each is copied at its own open
  (D117), so a start refused at the corpus or the knowledge bases has migrated `lmgw.sqlite`
  already.
- One copy per run of migrations: a start killed after some of its step's migrations committed
  leaves a database with a later `<from>`, so the next start makes one more copy, for that step
  (part applied when it was killed inside a migration without a transaction, D118, and then
  ranked ahead of the earlier whole copy, which has a lower `<from>`). Settings → About does not
  show a copy's part-applied mark yet.
- A command that restores a copy (the restore is a manual file step, §6.4, D108), and comparing
  the copy's and the migrations' time with the unit's `TimeoutStartSec`, which the start is not
  told (D112).
- A CLI on the desktop (the tray's nonce is its recovery path, §5.7).
- OIDC sign-in (for example against a self-hosted GitLab): it needs discovery, the redirect flow, a
  client secret at rest and account linking, so it is not nearly free on this model.
- Per-account quotas and per-account model access: with every account a full admin, a limit on one
  binds nothing another account could not lift. Per-account usage falls out wherever a request row
  carries the caller's key id (`request_logs.key_id`, migration 0027): an account's door key makes
  its own traffic attributable; whether the dashboard Chat's turns carry the caller's key is checked
  in WP6.
- An idle timeout for sessions (beside their fixed lifetime), and renaming an account after its
  first password.
- Making `LOGIN_BACKOFF_START`, `LOGIN_BACKOFF_MAX` and `PASSWORD_LINK_TTL` settings (they are
  shown, not editable, in this version).
- Gating agent origins behind a signed-in account on a server.
- The Activity list's account picker offering a deleted account (its entries show under
  everyone; the picker lists today's accounts).
- Fail-closed audit writes (D83), and an entry for a sign-in refused as busy (D86).
- Attributing the dashboard Chat's model calls to the signed-in account: its turns log under
  `internal:chat` (`telemetry::internal_identity`), not the caller's door key, so per-account
  usage covers `/v1` traffic with an account's own owner keys only (checked in WP6).
- A `503 login_busy` test against a real flood (the permit count and the known browsers' kept
  permit are unit-tested); anything against the CPU a flood burns (D67).
- A timer that prunes expired session rows between sign-ins (D73).
- `session_required`'s sentence still names the desktop's login link in the server image. (The
  command line refusing a database newer than its image offline came with WP9, D108.)
- The set-password page telling whose link it holds before the submit (D62).
- Built-in ACME; HSTS; OCSP stapling on the TLS listener.
- A WebRTC candidate from `public_url`'s address (§4.6): the TLS group's candidate is the
  connection's local address, the container's view, until a call through the outer pasta shows
  whether it needs one.
- `/api/connect` in container mode leaving out the container's own interface addresses (it lists
  `https://192.0.2.10:8443` after the public URL and loopback).
- Reading `X-Forwarded-Port` and `X-Forwarded-Prefix` in proxy mode (D50), and lmgw under a path
  prefix behind a proxy.
- A setting naming the forwarded header family a proxy writes, in place of refusing a request
  that carries both (D48).
- URLs built from a trusted proxy's forwarded host when `public_url` is empty (D52).
- An in-process restart in container mode.
- Encrypted provider keys, a design of its own for both deployments (ruling 12); systemd
  credentials into a rootless container.
- A wrapping key derived from a passphrase (the image's secret is 64 hex digits, D95), and moving
  the device CA to a new wrapping key while keeping the CA (D98).
- A pre-fetch switch in the image (the machine check's own Not yet).
- The `lmgw-server` binary outside a container (D1 refuses it).
- A dev instance inside the image skipping its image-store refusals.

## Unverified (2026-11-08)

What the design depends on that neither the spike nor this paper design has run, each with its WP:
- WP2, not run yet (the unit's and the scripts' comments say "expected" for each): Quadlet's
  `CgroupsMode=split` against the entrypoint's cgroup leaf, and the leaf's retry bound against a
  real racing `podman exec`; `AddDevice=` with a CDI name under Quadlet (the dry run passes it to
  `--device`; a start with the card is not run); `Notify=healthy` reaching ready through the
  startup probe under systemd; engine builds inside the nested container; pulls through the private
  pull service; agent runs and host mounts through the outer container; the audio and image
  classes; the outer container on a user-generated CDI spec; a WebRTC call on the plain group
  through the published UDP port; which writers still reach the tmpfs `/tmp` under real work;
  buildah in an unprivileged job on the `docker` runner, GitLab's artifact size limit for the
  archive, and the server binary's peak memory and time there.
- WP2, run on 2026-11-08 (Podman 6.1.3): the name `podman load` gives the OCI archive, in a
  scratch store; `--pull=never`'s "image not known" for a missing tag; the removal time after the
  kill (0.25 s); the image built with buildah in a rootless Fedora 45 Podman container without
  added capabilities, `--version`, catatonit, the entrypoint and `podman --version` inside it;
  `quadlet -dryrun -user` on the unit and volumes (`VolumeName=`, the health flags,
  `podman-user-wait-network-online.service`); one confined start without the card, with the unit's
  flags and `--cgroups=split` from a shell rather than a unit: the leaf's line on try 1 (also after
  a restart in place), the inner Podman seeing `cpu io memory pids`, the startup probe's second
  answer marking the container healthy 3 s after the start, `/api/version`, and a stop by SIGTERM
  in 0.2 s with exit 0.
- WP4, not run yet: WebRTC through the outer pasta (TCP and UDP published) on the TLS group; the
  peer address a connection from another machine shows lmgw through the outer pasta; Quadlet
  expanding `%h` in `Volume=`; a browser's secure context on another machine (dictation, copy); an
  agent origin under a wildcard certificate; a device paired through the published 8788 with the
  public name in its leaf.
- WP4, run on 2026-11-08 (Podman 6.1.3, container-selinux 2.251.0): a renewed pair in the
  `:z`-labelled host directory read by the confined container and picked up by the next accept, as
  long as the files are written or copied into the directory (a file `mv`ed in from elsewhere keeps
  its label and is refused, §3.4); a connection from the host's loopback through the outer pasta
  shows lmgw the namespace's own address (192.0.2.10) as its peer, so a peer CIDR cannot tell local
  callers apart there (§4.5).
- WP5, not run yet: Caddy and Traefik in front of lmgw; the client address a proxy in lmgw's pod
  sees for a connection from another machine through the outer pasta; a proxy on another host; a
  browser through the proxy; a re-encrypting proxy to the TLS listener.
- WP5, run on 2026-11-08 (nginx 1.29.5, Podman 6.1.3): proxy mode on loopback and in one pod (§13,
  WP5); in the pod, nginx sees a client from the host's loopback as 192.0.2.10, as lmgw does.
- WP6, not run yet: a password sign-in from another machine over the desktop's TLS listener;
  the browser flows by a human; known browsers through a NAT with a mistyping colleague (§5.4)
  beyond the unit test; after the second review round, the commands in the image again (their
  tests drive them in-process and through the debug binary, not in a container), the terminal's
  no-echo prompt typed by a person, and the dashboard's revoked card, waiting card and armed-item
  notes in a browser.
- WP6, run on 2026-11-08: one argon2id verification, 95 ms in release on a 16-core desktop CPU
  (§5.10), up to 32 at once there (2.0 GiB together); the image without TLS refusing a password
  from the host's own browser, since the outer pasta hides loopback (§13, WP6). (§5.2's NIST SP
  800-63B-4 figures were read against the published text on 2026-11-08.)
- WP7, not run yet: the audit log in the image under Quadlet (its entries through a real `podman
  exec … lmgw-server account set-password` and a one-off `podman run --rm`); an entry's `peer`
  for a client on another machine through the outer pasta (it is the namespace's address there,
  §4.5) and through a proxy; the card by a human at 125 %; the log's size on a busy server.
- WP7, run on 2026-11-08: §13's WP7 live check (two scratch dev instances, the debug binary),
  and the review round's (one headless dev instance: the flood, the account filter, the secret
  grep, the list at 125 % in headless Chrome).
- WP7's review round, not run yet: the tray's and the window's entries on a real desktop with
  accounts on (tested through `audit::desktop` only), a pairing link handed to a registered client,
  `machine_apply`'s entry, a flood from many addresses of one IPv6 /64, and the prune's batches on
  a log of millions of rows.
- WP8, not run yet: the secret under a real Quadlet unit and systemd (the starts above were
  `podman create`/`start` with the unit's flags), the `ExecStartPre=` check among them (D104:
  that its message reaches the journal and the unit ends failed); a data volume moved from a
  desktop, whose key the desktop's Secret Service wrapped (expected: `unreadable`, the secret
  named, until the CA is rotated, D98); the review round's start-time wrap (D102) in the image
  itself (tested through `start::prepare` only); the dashboard's server wording of the key mode
  in a browser.
- WP8, run on 2026-11-08 (Podman 6.1.3): the secret's mount over `--systemd=always`'s `/run`
  tmpfs, a missing secret ending the start, the unit's `Secret=` through `quadlet -dryrun`, and
  the four image starts of §13's WP8 entry.
- WP9, not run yet: a copy of a large database and its time against the unit's 900 s (the
  databases here were under 1 MiB); a copy failing for a full volume (`SQLITE_FULL`) or for
  permissions (the test makes the backups path a file, which fails for root too); a real
  restore of a copy with the previous image; the copy under Quadlet in the image itself. From
  the review round: a start killed by `TimeoutStartSec` inside a migration or inside the copy
  (D113's record before the migrator and D114's partial are tested only through refusals and a
  partial placed by hand); the reuse under `Restart=always` in a real unit; `link()` on the data
  volume's filesystem in the image (tested on the tests' temp dirs); the unreadable-dir warn
  line on Settings → About in a browser; a `knowledge.db` or `quickdoc.db` newer than the image.
  From the second review round: a start really killed inside a migration without a transaction
  (the test makes 0086 fail part way and stands in for the later commits with a write); the
  previous image clearing the foreign-key marker on a part-applied database (the test deletes it
  by hand); a linked backups dir on another volume in the image.
- WP9, run on 2026-11-08: §13's WP9 live check (one container-mode dev instance of the debug
  binary on a scratch data dir).
- Elsewhere: the host removing an image an inner container uses (shared variant only); systemd
  credentials in a rootless user unit (§9).
