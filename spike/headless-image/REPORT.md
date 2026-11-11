# Spike: headless lmgw in a container that runs its own Podman

Date 2026-11-08, lmgw at 4dd3d26c (the headless example, debug build). Host: Fedora 45,
rootless Podman 6.1.3 (cgroups v2, systemd manager, overlay), SELinux enforcing with
container-selinux 2.251.0, a 24 GiB test GPU with driver 600.20.03, nvidia-container-toolkit 1.20.1.
Reproduce with `run.sh` in this directory (`build`, then `start`; `SPIKE_MODE=shared|confined`).

**Result: it works, without `--privileged`.** lmgw runs as root inside a rootless outer
container and starts its llama.cpp model containers with the inner Podman. The GPU reaches
them through lmgw's own CDI spec, NVML reports correct VRAM, and the host reaches the gateway
on a published port. A Qwen3.5-0.8B row planned, created, passed `local_model_test` and
answered chat completions from the host in both variants:

- **shared:** the host's image store is mounted read-only as an additional image store, so
  nothing is pulled again. Needs `--security-opt label=disable`.
- **confined:** runs as SELinux type `container_engine_t`, but the engine image has to be in
  the inner store (one load of about 3.2 GiB per instance).

## Outer container flags

| Flag | Why (what fails without it) |
|---|---|
| `--device nvidia.com/gpu=all` | GPU and `libnvidia-ml` for lmgw, from the host's CDI spec |
| `--device /dev/net/tun`, `--cap-add NET_ADMIN` | the inner pasta cannot open or configure its tun device |
| `--cap-add SYS_ADMIN` | crun cannot mount proc and devpts (`unmask=ALL` plus `seccomp=unconfined` did not replace it) |
| `label=disable` or `label=type:container_engine_t` | the default `container_t` may not mount proc, devpts, sysfs or cgroup |
| `--systemd=always` | the only way found to get a writable `/sys/fs/cgroup`; without it the inner Podman needs `cgroups=disabled` and loses `--memory` and other limits |
| `--stop-signal SIGTERM` | `--systemd=always` switches the stop signal to SIGRTMIN+3, which lmgw does not handle |

`label=nested` does not work with container-selinux 2.251.0: under `container_t`
`/sys/fs/selinux` is unreadable, under `container_engine_t` Podman's write to
`/sys/fs/selinux/context` is denied. With the host store under `container_engine_t`, crun is
denied `mounton` on its `container_ro_file_t` layers; fuse-overlayfs failed as well.

The image itself needs:

- **catatonit as PID 1.** With lmgw as PID 1, conmon and pasta processes stayed as zombies.
- **A cgroup leaf:** the entrypoint moves every process into `/sys/fs/cgroup/init` and
  enables the controllers; then `--memory` works for inner containers.
- **Drop-in config.** A plain `/etc/containers/storage.conf` is overridden by the vendor file
  `/usr/share/containers/storage.rootful.conf.d/00-vendor-rootful.conf`, so the storage config
  goes into `storage.rootful.conf.d/`. Inner containers.conf: `netns="pasta"`,
  `default_sysctls=[]`, `cgroup_manager=cgroupfs`, `events_logger=file`.
- **`XDG_RUNTIME_DIR=/run/user/0`**, which root in a container does not have; lmgw's CDI spec
  and pull service sockets live under it.

Root inside the outer container is the right user: it has exactly the host user's rootless
uid mapping, which is what makes the shared store readable. A non-root inner user works only
with its own store and more setup (file caps on `newuidmap`/`newgidmap`, `/dev/fuse`,
`unmask=ALL`, `seccomp=unconfined` or inner `--uts=host`), and cannot read the host store;
`cap_setfcap` for the keep-id mapping has not been tried yet.

## Findings per area

- **GPU/CDI:** `nvidia-ctk cdi generate` inside writes a valid spec, and lmgw's own-spec path
  works unchanged. The NVIDIA driver translates PIDs into the container's namespace, so lmgw's
  VRAM split (its own share vs. outside use) is correct. In confined mode the generated spec
  lacks `nvidia-smi` (SELinux denies reading `xserver_exec_t`), CUDA still works. Every inner
  GPU container start prints a harmless "Could not locate device 195:254 on host" warning.
  The outer container depends on the host's system CDI spec, the one that goes stale after an
  egl-wayland update; a user-generated spec with `--cdi-spec-dir` for the outer has not been
  tried yet.
- **Networking:** model containers publish `127.0.0.1:<port>` through pasta onto the outer's
  loopback, where lmgw reaches them; one inner container cannot reach another's port, and the
  host cannot either. lmgw has to bind `0.0.0.0` inside for the outer's published port to reach
  it. `gateway_access` then gives agents `http://host.containers.internal:<port>`, which
  worked from an inner container. pasta was chosen over netavark, whose loopback publishing
  needs `route_localnet` sysctls that are read-only here.
- **Image store:** only `overlay`, `overlay-images` and `overlay-layers` of the host store are
  mounted (read-only, no relabel), so the host's database, volumes and secrets stay outside.
  The inner Podman uses those images as lower layers without copying. **A store volume must
  never be reused across the two modes:** files copied up under `label=disable` keep
  `container_ro_file_t`, and a confined Podman then fails every call.
- **Audit:** with auditd running, AVC denials are not in `journalctl -k`; look in the full
  journal (`journalctl --since … | grep "AVC avc"`).

## What lmgw would have to change for a production image

Besides the items known before the spike (a production start instead of the dev example,
TLS on the main listener, the first-start bind address, Require API key on by default):

1. **Read-only images.** `backends/images.rs` ignores Podman's `ReadOnly` field. Delete, tag
   and pull should refuse read-only (host store) images with a reason, and the machine check's
   download and room items should say where an image lives.
2. **Dev-instance assumptions** (`state.rs`, `examples/headless.rs`) take the image store to be
   shared with production; in the container only the additional store is.
3. **Machine check in a container.** It reports the image, not the host: the toolkit, rpm
   database, repos and helper path (`lmgw-machine/src/rpm.rs`, `repos.rs`,
   `plan/embedded.rs`) are the image's, and setup steps would land in a throwaway layer.
   `machine/running.rs` calls Podman "as root" and shows subuid hints for a root Podman. Its
   steps there should read "update the image" or "on the host: …". The own-spec stamp only
   sees a host driver update after the outer container restarts.
4. **Metered-network check** (`machine/prefetch.rs`) reads the host's interface name through
   pasta and calls nmcli, which is not in the image: it needs a setting or a quiet "not checked
   in a container".
5. **PID 1:** lmgw does not reap orphans; keep an init in the image or make lmgw a subreaper.
6. **Podman stderr noise:** the "Could not locate device" warnings would end up in the failure
   text lmgw quotes from Podman (`runtime/registry/command.rs`).

Not tried yet: agent runs, pulls through the private pull service, the audio and image model
classes, the host removing an image while an inner container uses it, a non-root inner user
with `cap_setfcap`, and the outer container on a user-generated CDI spec.
