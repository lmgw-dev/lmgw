# lmgw on a server: the Podman image

lmgw also ships as a Podman image for a Linux server with no desktop. Inside
it, `lmgw-server` serves the same gateway and dashboard as the desktop app
and runs its model, agent and build containers with a nested Podman. A
rootless Quadlet unit for a dedicated service account starts it. The server
always has sign-in accounts: everyone who administers it signs in with a
password.

This page is for the administrator who installs and runs it. The design
record is
[docs/design/2026-11-08-headless-server-image-design.md](design/2026-11-08-headless-server-image-design.md);
where the two differ, this page follows what the shipped files do. The
desktop's own accounts switch is in [docs/accounts.md](accounts.md).

The files this page refers to are in `packaging/server/`: the unit
`lmgw.container`, the three `.volume` files, and the image's own files.

---

## 1. What the host needs

| Need | Notes |
| --- | --- |
| Linux with systemd and cgroups v2, Podman with Quadlet | Tested with Podman 6.1.3. Older versions are not verified yet. `podman info --format '{{.Host.CgroupsVersion}}'` must print `v2` |
| SELinux policy with the `container_engine_t` type | Tested with container-selinux 2.251.0. The container runs confined as `container_engine_t`, without `--privileged` |
| NVIDIA driver and `nvidia-container-toolkit` on the host, with a current CDI spec in `/etc/cdi` or `/var/run/cdi` | The container gets the card from the host's CDI spec (`nvidia-ctk cdi list` must show `nvidia.com/gpu=all`). The spec goes stale after a driver update: refresh it (`sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`, or the toolkit's `nvidia-cdi-refresh` units) and then restart lmgw |
| `/dev/net/tun` | The nested Podman's networking needs it |
| A service account with subuid and subgid ranges, and linger | Section 2 |
| The published ports open in the firewall | TCP 8443 and 8788 (8787 instead of 8443 without TLS); UDP on the same numbers if you use WebRTC calls |
| Disk for the volumes | Mostly the model weights. **Settings → GPU** (the "This server" card) shows the free space of each volume |

The image carries no CUDA itself. Engine images are pulled or built inside
it at run time, as on the desktop.

## 2. The service account

lmgw runs as an ordinary user with its own Podman store, not as root. The
examples use the account name `lmgw`:

```sh
sudo useradd --create-home lmgw
grep lmgw /etc/subuid /etc/subgid      # both need a line, such as lmgw:524288:65536
# if useradd made none, add a range no other account uses: sudo usermod --add-subuids 524288-589823 --add-subgids 524288-589823 lmgw
sudo loginctl enable-linger lmgw       # its user units run without anyone signed in
```

Every later command on this page runs **as that account**, in a session with
its own user manager. `sudo machinectl shell lmgw@` gives one (Fedora's
`systemd-container` package). With `sudo -iu lmgw`, set the runtime directory
first, or `systemctl --user` and `podman` cannot find the account's session:

```sh
sudo -iu lmgw
export XDG_RUNTIME_DIR=/run/user/$(id -u)
```

## 3. Getting the image

The image comes as an OCI archive, `lmgw-server-<version>.oci.tar`, with a
`.sha256` file beside it. lmgw has no image registry yet, so there is nothing
to pull.

- **From a GitLab pipeline**: the `server-image` job's artifact. The job is
  `allow_failure` and has not run on a runner yet. On a plain Docker-backed
  runner it is expected to fail (Docker's default seccomp profile refuses what
  buildah needs), until it runs privileged or on a dedicated runner. Build the
  archive yourself meanwhile.
- **Built yourself**: in a checkout with the build dependencies from the
  README's "Building from source", plus `buildah`, `jq` and `binutils`:

  ```sh
  LMGW_BUILD_SERVER=only bash ci/build.sh    # the release lmgw-server into dist/
  bash ci/build-image.sh                     # dist/lmgw-server-<version>.oci.tar
  ```

  The build uses its own buildah store under `target/image-build/` and never
  touches your Podman store.

Load and tag it as the service account:

```sh
sha256sum -c lmgw-server-0.7.0+240.oci.tar.sha256
podman load -i lmgw-server-0.7.0+240.oci.tar
#   Loaded image: localhost/lmgw-server:0.7.0_240
podman tag localhost/lmgw-server:0.7.0_240 localhost/lmgw-server:current
```

An image tag cannot hold a `+`, so the version's `+` becomes `_` in the tag
(`0.7.0+240` loads as `localhost/lmgw-server:0.7.0_240`). The unit always runs
`localhost/lmgw-server:current` and never pulls (`Pull=never`): without that
tag the start fails with Podman's "image not known".

**A GitLab-built archive carries a token.** The binary in it has the update
feed's deploy token compiled in (a GitLab `read_package_registry` token), as
the GitLab-built RPM does. Handing the archive to someone else hands them that
token. A local build carries one only if `LMGW_UPDATE_DEPLOY_TOKEN` was set
in your environment.

## 4. The wrapping key secret

The device link's CA key (the CA that signs paired devices' certificates)
lives on the data volume. In the image it is sealed under a wrapping key that
Podman hands the container as a secret, so a copy of the data volume alone
does not carry the CA's key. Make the secret once, before the first start:

```sh
openssl rand -hex 32 | podman secret create lmgw-wrap-key -
```

The secret is 64 hex digits. Podman mounts it at `/run/secrets/lmgw-wrap-key`
inside the container (the unit's `Secret=lmgw-wrap-key,mode=0400`).

**Back it up on its own**, apart from the data volume. With Podman's default
secret driver it sits unencrypted in the service account's Podman storage, and
a wrapped CA key opens only with it:

```sh
(umask 077; podman secret inspect --showsecret --format '{{.SecretData}}' lmgw-wrap-key > lmgw-wrap-key.hex)
# restoring it on a new host:
podman secret create lmgw-wrap-key lmgw-wrap-key.hex
```

**Without the secret.** The unit's `ExecStartPre=` checks for it before
Podman runs. A missing secret fails the start, and the journal says how to
make it. To run without one on purpose, remove both the `Secret=` line and
the `ExecStartPre=` line from the unit. lmgw then keeps the CA key plain
(mode 0600) and says so in its log at each start. A start with the secret
again wraps a plain key by itself.

**When the secret goes missing later.** A CA key that was wrapped cannot be
opened until the same secret is back. With the shipped unit lmgw does not
start at all then, because `ExecStartPre=` fails on the missing secret. Your
options:

- Restore the secret from your backup (`podman secret create`, above).
- Make a new secret, start, and rotate the device CA with the recipe below
  (pair every device again).
- Drop both the `Secret=` and the `ExecStartPre=` line. The wrapped key stays
  unreadable, so rotate the device CA; the rotation then makes a plain key.

**When a backup of the data volume leaked together with the secret**, the
copy's CA key opens with that secret for as long as you keep it. A rotation
alone does not end that. In this order:

```sh
systemctl --user stop lmgw
podman secret rm lmgw-wrap-key
openssl rand -hex 32 | podman secret create lmgw-wrap-key -
systemctl --user start lmgw          # the device CA is unreadable now, which is expected
```

Then rotate the device CA on the dashboard: **Usage → Keys & budgets**, the
Devices card, **Rotate the device CA…**. It seals a new key under the new
secret. Every paired device has to be paired again (**New token…** on its
row). Back up the new secret. The leaked volume also held everything in
section 6's warning, so revoke those keys too.

## 5. Installing the unit

As the service account, put the unit and the three volume files where
Quadlet reads them, and make the TLS directory:

```sh
mkdir -p ~/.config/containers/systemd ~/lmgw-tls
cp packaging/server/lmgw.container packaging/server/lmgw-{data,store,buildcache}.volume \
   ~/.config/containers/systemd/
```

Edit `~/.config/containers/systemd/lmgw.container`:

- **`Environment=LMGW_PUBLIC_URL=https://lmgw.example:8443`**: the address
  people open. Login and set-password links, the Overview's snippets, pairing
  proposals and agent origins are all built from it. The certificate must
  cover its host name. A different outer port works as long as the URL names
  it.
- **`AddDevice=nvidia.com/gpu=all`**: the card by its CDI name. On a server
  without an NVIDIA card, remove the line; lmgw then has no local models, and
  the machine check says what is missing.

Put the certificate pair into `~/lmgw-tls/` (section 7), then start it:

```sh
systemctl --user daemon-reload
systemctl --user start lmgw
systemctl --user status lmgw
journalctl --user -u lmgw -f         # or: podman logs -f lmgw
```

There is no `systemctl enable`: Quadlet reads the unit's `[Install]` section
(`WantedBy=default.target`) at each `daemon-reload`, and linger starts the
account's user manager at boot.

`quadlet -dryrun -user` shows the `podman run` command the unit turns into:

```sh
/usr/libexec/podman/quadlet -dryrun -user
```

### What the unit does

- **The container is recreated at every start** (`podman run --replace
  --rm`). Everything that lasts lives on the volumes.
- **It counts as started once lmgw answers.** A startup health probe asks
  `http://127.0.0.1:8787/api/version` inside the container every 2 s, and
  `Notify=healthy` marks the unit started at the first answer.
- **`TimeoutStartSec=900`** leaves room for a release's first start, which
  copies and migrates the databases before lmgw answers (section 11). Each
  such start logs how long that took, so a database that outgrows 900 s shows
  in the log first. `HealthStartPeriod=` has the same number.
- **The stop budget.** `StopTimeout=95` covers a whole stop: 10 s for open
  streams, 10 s plus the unload timeout (60 s by default, **Settings → GPU →
  Admission → Unload timeout**) for a running benchmark, and 15 s for the
  model containers. `LMGW_STOP_TIMEOUT` repeats the number for lmgw, which
  warns on **Settings → About** and in the log when the stored unload timeout
  makes a stop longer. Then raise `StopTimeout=`, `LMGW_STOP_TIMEOUT` and
  `TimeoutStopSec=` together (`TimeoutStopSec=` stays above `StopTimeout=`).
- **`StopSignal=SIGTERM`**: `--systemd=always` would otherwise send a signal
  lmgw does not handle.
- **`/tmp` is RAM.** `--systemd=always` mounts tmpfs on `/run` and `/tmp`, so
  the image points `TMPDIR` at `/var/tmp`, which is a volume.

### The volumes

| Volume | Mounted at | Holds |
| --- | --- | --- |
| `lmgw-data` | `/data` | The databases, model weights (`models/`), engine builds, the device CA (`tls/`), labs, transfers, knowledge files, the migration copies (`backups/`) |
| `lmgw-store` | `/var/lib/containers` | The nested Podman's images and containers: the engine images lmgw pulled or built |
| `lmgw-buildcache` | `/var/tmp` | Image builds' scratch and cache, and `TMPDIR`: lmgw's staging copies of user content (document uploads, corpus exports and imports) while they are made |
| `~/lmgw-tls` (host directory, read-only) | `/etc/lmgw/tls` | The TLS pair |

The `.volume` files give the volumes these names (`VolumeName=`); without it
Quadlet would call them `systemd-lmgw-data` and so on.

`lmgw-store` can be rebuilt (lmgw pulls or builds its engine images again),
so it rarely needs a backup. A backup of `lmgw-data`, with the service
stopped, includes `/data/models`, the model weights, which can be hundreds of
GiB and can be downloaded again:

```sh
systemctl --user stop lmgw
(umask 077; podman volume export lmgw-data > lmgw-data-backup.tar)
# or without the weights, tarred by a one-off container (not tried yet):
(umask 077; podman run --rm -v lmgw-data:/data:ro localhost/lmgw-server:current \
  tar -C /data --exclude=./models -cf - . > lmgw-data-nomodels.tar)
systemctl --user start lmgw
```

## 6. Protect the data volume like a password file

**`lmgw-data` holds the cloud providers' API keys and the Hugging Face token
in cleartext**, in its SQLite database. lmgw does not encrypt them yet, on the
server or on the desktop. Anyone who can read the volume, a backup of it, an
export, or one of the migration copies in `/data/backups/` can use those keys
at your cost. Owner and agent keys are stored readable too.

`lmgw-buildcache` holds, while they are made, staging copies of what people
upload or export: documents, corpus exports and imports.

So treat both volumes, every backup and copy of them, and the host's
`~lmgw/.local/share/containers/` (where rootless Podman keeps volumes and the
wrapping key secret) as secrets. Passwords and session tokens are stored only
as hashes.

## 7. TLS and renewal

The unit serves TLS on 8443 with your certificate, from any source: an
internal CA, an ACME client on the host, a bought one. lmgw has no ACME client
of its own yet.

- `~/lmgw-tls/tls.crt`: the PEM chain, leaf first.
- `~/lmgw-tls/tls.key`: its private key, **owned by the service account,
  mode 600**. Inside the rootless container that account is root, which is
  who reads it.

The unit passes them as `LMGW_TLS_CERT=/etc/lmgw/tls/tls.crt` and
`LMGW_TLS_KEY=/etc/lmgw/tls/tls.key`. Behind TLS the plain listener moves to
the container's loopback (`127.0.0.1:8787`): the health probe, agents and the
command line use it there, and it is not published.

**Renewal needs no restart.** lmgw looks at the two files when a connection
arrives and loads a changed pair from that connection on. A pair that does not
load (a certificate written before its key, a key that does not match) keeps
the old one serving, with the reason on **Settings → Network & access → TLS**
and in the log, and is tried again when either file changes. The TLS card also
warns of a certificate that expires within 14 days, a key that other accounts
or the file's group can read, and a public URL or agent suffix the certificate
does not cover.

**The deploy hook must copy or write the files into the directory, never
`mv` them in from elsewhere.** The unit mounts the directory with `:z`, which
labels it for the confined container. A file created in the directory takes
that label; a file moved in keeps its old one, the container gets "Permission
denied", and lmgw goes on serving the old pair. Renaming inside the directory
is fine. Symlinks to files outside the directory do not resolve inside the
container. A certbot deploy hook, run as root:

```sh
#!/bin/sh
# /etc/letsencrypt/renewal-hooks/deploy/lmgw.sh
set -eu
# hooks in renewal-hooks/deploy/ run for every certificate: only this one
case "$RENEWED_LINEAGE" in */lmgw.example) ;; *) exit 0 ;; esac
dir=/home/lmgw/lmgw-tls
cp "$RENEWED_LINEAGE/fullchain.pem" "$dir/tls.crt.new"
cp "$RENEWED_LINEAGE/privkey.pem" "$dir/tls.key.new"
chown lmgw:lmgw "$dir/tls.crt.new" "$dir/tls.key.new"
chmod 644 "$dir/tls.crt.new"
chmod 600 "$dir/tls.key.new"
# the certificate first, then the key: lmgw reloads at the key's rename and a
# renamed key beside the old certificate would log a mismatch
mv "$dir/tls.crt.new" "$dir/tls.crt"
mv "$dir/tls.key.new" "$dir/tls.key"
```

The hook needs `chmod +x`. certbot runs deploy hooks on renewals, not at the
first issuance: run it once by hand, as root, after the first certificate
(`RENEWED_LINEAGE=/etc/letsencrypt/live/lmgw.example
/etc/letsencrypt/renewal-hooks/deploy/lmgw.sh`).

**Without TLS.** Remove the two lines `LMGW_TLS_CERT` and `LMGW_TLS_KEY`, set
`LMGW_PUBLIC_URL` to `http://lmgw.example:8787` (keep the line: without it
lmgw builds login and set-password links on the loopback address, useless from
another machine), remove the `Volume=%h/lmgw-tls:…` line, and the two
`PublishPort=8443…` lines, and publish 8787 instead:

```ini
PublishPort=8787:8787
PublishPort=8787:8787/udp
```

The plain listener then binds every address of the container. Two things are
lost. Browsers on other machines are not in a secure context (no dictation,
no clipboard copy), and every key and cookie travels in cleartext. And **no
password is accepted over plain HTTP from another machine**. The host's own
browser counts as another machine here, since the container sees host
connections from its own network address. People then sign in with a
one-time link (`login-link`, section 10) each time.

## 8. Behind a reverse proxy

A proxy that terminates TLS in front of lmgw (Caddy, Traefik, nginx,
HAProxy) works in proxy mode. lmgw reads `X-Forwarded-For`, `-Proto` and
`-Host`, or `Forwarded` (RFC 7239), only from the addresses you trust, and
ignores and strips them from everyone else:

```ini
Environment=LMGW_TRUSTED_PROXIES=127.0.0.1
Environment=LMGW_PUBLIC_URL=https://lmgw.example
```

`LMGW_TRUSTED_PROXIES` takes addresses and CIDR networks, with commas or
spaces between them. `LMGW_PUBLIC_URL` must be the address the proxy serves.

**Which unit variant.** With the TLS lines kept, the plain listener the proxy
would use is the container's own loopback (it follows the TLS setup), so only
a proxy in the same pod reaches it. A proxy on the host needs the no-TLS
variant above, with the caveat below: lmgw cannot tell it from other host
processes.

**Only the proxy may reach lmgw's port.** Rootless Podman's network shows lmgw
every connection from the host's own loopback with one and the same address,
the container's own, so a trusted address cannot tell the proxy from any
other process that reaches the published port from the host. The safe layout
is the proxy in the same pod as lmgw, trusting `127.0.0.1`, with lmgw's port
not published (only the proxy's is). Inside the container, trusting
`127.0.0.1` also trusts lmgw's agents and the health probe, which talk to the
same listener. The log names the client address of each listener's first
request, and the TLS card shows it, so you can see what lmgw sees. A Quadlet
pod for this layout is not shipped yet.

The proxy must overwrite `X-Forwarded-Proto` and `X-Forwarded-Host` (or
`Host`), append to `X-Forwarded-For`, and drop `Forwarded`, because lmgw
refuses a request that carries both families. nginx passes on any header it
does not set, so set all of them:

```nginx
map $http_upgrade $connection_upgrade { default upgrade; '' close; }

location / {
    proxy_pass http://127.0.0.1:8787;
    proxy_http_version 1.1;
    proxy_set_header Host              $http_host;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-Host  $http_host;
    proxy_set_header Forwarded         "";
    proxy_set_header Upgrade           $http_upgrade;
    proxy_set_header Connection        $connection_upgrade;
}
```

The other proxies need one line each to drop a client's own `Forwarded`
header: Caddy `header_up -Forwarded` in its `reverse_proxy` block, Traefik a
`headers` middleware with `customRequestHeaders` setting `Forwarded` to `""`
(which removes it), HAProxy `http-request del-header Forwarded`. Only the
nginx setup has been run so far.

WebSockets (`/v1/realtime`) pass through the proxy. The device listener
(8788) does not: paired devices use mutual TLS, which a TLS-terminating proxy
cannot pass, so keep 8788 published directly. WebRTC's UDP ports are published
directly too.

## 9. Agent origins on a server

An agent with a web page is served on its own origin,
`<agent id>.<suffix>`. The default suffix, `localhost`, only works in a
browser on the gateway's own machine, so on a server the dashboard says
"agent pages are unreachable from other machines" until it changes. A working
setup has three parts:

1. A suffix outside the dashboard's own domain, under **Settings → Agents &
   tools → Agent containers → Agent origin suffix**. With the dashboard at
   `lmgw.example`, use `lmgw-apps.example`, not `apps.lmgw.example`; lmgw
   refuses a suffix under its own host names, since a page there could set
   cookies for the dashboard.
2. A wildcard DNS record, `*.lmgw-apps.example`, pointing at the server.
3. A certificate that covers both `lmgw.example` and `*.lmgw-apps.example`
   (with ACME, a wildcard needs the DNS-01 challenge). The TLS card warns
   when the certificate does not cover the suffix.

Agent pages then open at `https://<agent id>.lmgw-apps.example:8443/`, on the
public URL's scheme and port. An agent page is served to anyone who reaches
the port; putting it behind a signed-in account is not built yet.

Host folders for agents' host mounts have to be mounted into the lmgw
container first (a `Volume=` line of your own in the unit). The path you then
give an agent is the path inside the container. This is not tried in the
image yet.

## 10. Accounts, the first sign-in and recovery

Every account is a full administrator of the same gateway: the same models,
keys, chats and usage. Private data per account and other roles are not built
yet.

**The first sign-in.** Account 1 is called `owner` and starts without a
password. While it has none, each start logs a fresh single-use link for it,
valid 24 hours:

```sh
journalctl --user -u lmgw | grep 'set a password'
#   set a password for account 'owner': https://lmgw.example:8443/set-password#token=… (valid until …, single use)
```

Whoever reads the log can use the link, and it stops working once someone
has. The page asks for a sign-in name (leave it empty to keep `owner`; this is
the only time account 1's name can change) and the password twice, and signs
that browser in. Passwords need at least 12 characters. From then on each
start logs only where to sign in and how to get a one-time link, never a
credential.

**More accounts**: **Settings → Network & access → Accounts → Add account**
(a sign-in name and a display name) answers a set-password link to hand over,
valid 24 hours. The same card disables or deletes an account, ends its
sessions, sets another account's password, and holds the audit log of who
signed in and what changed.

A failed sign-in makes the next one for that name wait 1 s, doubling up to
15 minutes, and every refusal says how long. There is no lockout after a
number of failures. Browsers that signed in to an account before are held only
by their own failures.

**The command line.** With the service running, the commands go through the
gateway inside its container:

```sh
podman exec -it lmgw lmgw-server account list
podman exec -it lmgw lmgw-server account set-password owner     # asks twice, without echo
podman exec -it lmgw lmgw-server account link ops                # a set-password link for 'ops'
podman exec -it lmgw lmgw-server login-link                      # a one-time sign-in link for 'owner'
podman exec -it lmgw lmgw-server login-link ops
```

- `account set-password` ends that account's sessions. With
  `--login <new name>` it also renames account 1, but only with its first
  password.
- `login-link` prints a link that signs a browser in once, within 60 seconds,
  without a password. It works over plain HTTP too.
- `podman exec -it lmgw lmgw-server --help` lists them.

**With the service stopped** there is no container to `exec` into. `account
list`, `set-password` and `link` then run in a one-off container on the data
volume and apply at the next start; `login-link` needs the running gateway:

```sh
systemctl --user stop lmgw
podman run --rm -it -v lmgw-data:/data localhost/lmgw-server:current \
  lmgw-server account set-password owner
systemctl --user start lmgw
```

The one-off command refuses while a running gateway holds the data volume,
and refuses a database that this image has not started on yet (start the
service once with it first) or that a newer image migrated (section 11).

**Locked out**: take account 1's sign-in name from `account list` (the row
with id 1; `owner` only until a rename), then `account set-password <that
name>` from the command line, running or stopped, always gets account 1 back, and account 1 can set every other
account's password.

## 11. Updates and migrations

lmgw checks the update feed its binary was built for (20 s after the start,
then every 6 hours) and shows on **Settings → About** and in the log: "lmgw
0.8.0 is out; this server runs 0.7.2. Updates come as a new image archive:
load it and restart the service." It downloads nothing. **Settings → Tokens &
updates → Check for new versions in the background** switches it off.

**An update**, as the service account:

```sh
sha256sum -c lmgw-server-0.8.0.oci.tar.sha256
podman load -i lmgw-server-0.8.0.oci.tar
podman tag localhost/lmgw-server:0.8.0 localhost/lmgw-server:current
systemctl --user restart lmgw
```

An archive built by GitLab has a build number in its version, and the image
tag writes the `+` as `_` (section 3): `lmgw-server-0.8.0+12.oci.tar` loads as
`localhost/lmgw-server:0.8.0_12`, so tag that form.

The previous image stays in the store under its version tag until you remove
it (`podman images localhost/lmgw-server`, then `podman rmi` of the old tag).
There is no automatic update or rollback.

**Rolling back a release that changed no database schema** is the same in
reverse: tag the previous version `current` again and restart.

**A release that migrates the database cannot be rolled back that way.**
Once the new image has migrated a database, an older image refuses to open
it. You cannot yet tell from an archive whether it migrates; its first start's
log says so. Before such a start touches a database, it copies it into
`/data/backups/`:

```text
before migrating lmgw.sqlite from 0101 to 0102: copied it to /data/backups/lmgw-0101-0102.sqlite (644 KiB) in 0.1 s; …
migrated lmgw.sqlite from 0101 to 0102 (1 migration) in 0.1 s; …
```

- Each of the three databases is copied before its own migrations:
  `lmgw.sqlite` as `lmgw-<from>-<to>.sqlite`, and `knowledge.db` and
  `quickdoc.db` as `knowledge-<from>-<to>.db` and `quickdoc-<from>-<to>.db`.
  `<from>` is the schema the previous image opens. A name that is taken gets
  `-2`, `-3`.
- The copies are mode 0600 in a 0700 directory, listed with their sizes under
  **Settings → About → Copies before migrating**, and never deleted by lmgw.
  They hold the same cleartext keys as the database (section 6). Delete old
  ones yourself once you no longer need them.
- A copy that cannot be made (a full volume, for example) stops the start
  before anything is migrated, naming the cause, the path and the free space.
- A start that fails after its copy is restarted by the unit; while the
  database is unchanged it reuses that copy rather than filling the volume.

**The copy is a manual restore source, not a rollback.** Restoring it on the
previous image loses everything written to the database since the copy was
made: new keys, accounts, chats, settings and usage. lmgw never restores a
copy by itself. When an older image starts on a migrated database it refuses,
names the newest copy it can open and says what restoring it loses. The copy's name
is not fixed: a second copy of the same step is `-2`, `-3` and so on, and a
copy of a part-applied database is marked and ranked behind a whole one, so
take the name the old image's refusal gives (Settings → About lists the copies
too). A manual rollback, as the service account:

```sh
systemctl --user stop lmgw
podman tag localhost/lmgw-server:0.7.2 localhost/lmgw-server:current
systemctl --user start lmgw       # refuses, naming the copy; read it from the journal
systemctl --user stop lmgw
podman run --rm -it -v lmgw-data:/data localhost/lmgw-server:current bash
# inside the one-off container:
cd /data
ls -l backups/
mkdir replaced-0.8.0
mv lmgw.sqlite* replaced-0.8.0/                   # the migrated database with its -wal and -shm files
cp -p backups/<the copy named in the refusal> lmgw.sqlite
exit
systemctl --user start lmgw
```

Do the same for `knowledge.db` and `quickdoc.db` if the start copied them
too. Keep the replaced files until the old image runs as it should.

## 12. Settings the unit owns

Some settings are set by the unit's environment, because the published ports
and the unit depend on them. The dashboard shows them read-only, with the
variable that sets them; change the unit and restart
(`systemctl --user restart lmgw`). A variable that does not parse stops the
start, naming the variable and the value.

| Variable | Setting | Image default |
| --- | --- | --- |
| `LMGW_PUBLIC_URL` | Public URL | the TLS listener's address, else the plain one |
| `LMGW_TLS_CERT`, `LMGW_TLS_KEY` | Certificate, Private key | no TLS |
| `LMGW_TLS_LISTEN` | TLS address | `0.0.0.0:8443` |
| `LMGW_BIND_ADDR` | Bind address (the plain listener) | `0.0.0.0:8787`, or `127.0.0.1:8787` behind TLS |
| `LMGW_TRUSTED_PROXIES` | Trusted proxies | none, proxy mode off |
| `LMGW_WEBRTC_PORT`, `LMGW_WEBRTC_TLS_PORT` | UDP port, TLS UDP port (WebRTC calls) | the plain and the TLS listener's port numbers |

A changed `LMGW_BIND_ADDR` port also changes the health probes' URLs in the
unit and, without TLS, the `PublishPort=` lines.

Two more are only seeded: the first start on an empty data volume writes them,
and from then on they are ordinary dashboard settings.

| Variable | Setting | Seeded as |
| --- | --- | --- |
| `LMGW_REQUIRE_API_KEY` | Require gateway API keys on /v1 | on: `/v1` and `/mcp` need a key, which you make under **Usage → Keys & budgets** |
| `LMGW_DEVICES_LISTEN` | Device listener (`loopback`, `all` or an address) | `all`, so 8788 is reachable through the published port |

Settings that apply at a start on the desktop (the WebRTC calls switch, for
example) say "applies at the next start of lmgw" here; there is no in-process
restart in the image yet.

## 13. Not yet

- An image registry, and with it `podman auto-update` and automatic rollback.
  GitHub releases carry no image archive yet.
- A command that restores a migration copy, or tells whether an archive
  migrates.
- Encrypted provider keys (section 6).
- Built-in ACME, HSTS and OCSP stapling.
- A Quadlet pod for proxy mode (section 8), and reading `X-Forwarded-Port` or
  serving under a path prefix behind a proxy.
- Private data per account, roles other than full administrator, per-account
  quotas, OIDC sign-in, an idle timeout for sessions.
- Agent pages behind a signed-in account (section 9).
- A rootful system unit, and a non-root user inside the container.
- Running `lmgw-server` outside a container.

Not yet verified in the image under a real Quadlet unit: WebRTC calls through
the published UDP ports, a model start with the card, engine builds and
agent runs inside the container, host mounts for agents, and a browser on
another machine.
