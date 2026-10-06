//! Low-level podman verbs and container bookkeeping.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::watch;

use super::*;
use crate::runtime::Class;

impl Registry {
    /// Is a container for this model in the map (in any state)?
    pub fn contains(&self, class: Class, model_id: &str) -> bool {
        self.map().contains_key(&(class, model_id.to_string()))
    }

    /// Does an entry, in any state, name the container `name`?
    ///
    /// What a start or a climb whose entry was taken from it asks before it
    /// removes the container it started: when an entry holds that name again —
    /// a later start's (its `--replace` takes the name over, or already has),
    /// or the reconciliation pass's adoption (`unheld.rs`) — the container is
    /// that entry's, and removing it by name would strand it.
    pub(super) fn holds_name(&self, name: &str) -> bool {
        self.map().values().any(|e| e.container_name == name)
    }

    /// Where this model's running container computes ([`Entry::placement`]),
    /// `None` when none is in the map. The truth for that container: its row
    /// may have been switched since it started.
    pub fn placement_of(&self, class: Class, model_id: &str) -> Option<crate::runtime::Placement> {
        self.map()
            .get(&(class, model_id.to_string()))
            .map(|e| e.placement)
    }

    /// The host port of this model's container while it is `ready` — `None`
    /// when it is starting, stopping or not there at all. For a caller that
    /// polls the live container's own routes (the pool ledger's deferred
    /// release, `gate::pool`) and must treat "no longer up" as its answer.
    pub fn ready_port(&self, class: Class, model_id: &str) -> Option<u16> {
        self.map()
            .get(&(class, model_id.to_string()))
            .filter(|e| e.state == RuntimeState::Ready)
            .map(|e| e.host_port)
    }

    /// Does a container with exactly this name exist (in any state)?
    ///
    /// For the legacy sweep (§3.4), which matches the router-mode containers by
    /// name because they carry no labels to filter on.
    ///
    /// Three answers, not two: "podman says it is not there" and "podman could
    /// not answer" look identical to a `bool` and mean opposite things to the
    /// caller. The first is a name that has been dealt with and can be dropped
    /// from the sweep list forever; the second is a name lmgw has learned
    /// *nothing* about, and forgetting it would leave a router-mode container
    /// running with nothing left in the system that could ever rediscover it.
    pub async fn container_exists(&self, name: &str) -> Presence {
        match self.podman(&["inspect", "--format", "json", name]).await {
            Ok(out) if out.ok() => Presence::Present,
            Ok(out) if is_no_such_container(&out.stderr) => Presence::Absent,
            Ok(out) => Presence::Unknown(format!(
                "podman inspect {name} failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            )),
            Err(e) => Presence::Unknown(format!("podman inspect {name} could not be run: {e}")),
        }
    }

    /// Stop and remove a container this registry has no entry for, by name.
    ///
    /// The legacy sweep's verb (§3.4/§6). Graceful first — the router-mode
    /// containers hold a GPU and llama-server exits cleanly on SIGTERM (§10.1)
    /// — then `rm -f`, because unlike a managed container nothing will ever
    /// `--replace` this one.
    pub async fn remove_unmanaged(&self, name: &str) -> Result<(), String> {
        self.stop_quietly(name).await;
        self.rm_force(name).await
    }

    /// Drop *our* entry (identified by its phase channel, never by key
    /// alone). `only_starting` protects a claim's cleanup from removing an
    /// entry a `stop` has already taken responsibility for draining.
    pub(super) fn forget(&self, key: &Key, phase: &Arc<watch::Sender<Phase>>, only_starting: bool) {
        self.forget_if(key, |e| {
            Arc::ptr_eq(&e.phase, phase) && (!only_starting || e.state == RuntimeState::Starting)
        });
    }

    /// Drop the entry under `key` when `ours` says it is the one the caller
    /// means; `true` when it was removed.
    pub(super) fn forget_if(&self, key: &Key, ours: impl FnOnce(&Entry) -> bool) -> bool {
        let mut map = self.map();
        let matches = map.get(key).is_some_and(ours);
        if matches {
            map.remove(key);
        }
        matches
    }

    /// Give back one in-flight claim.
    ///
    /// `phase` is the entry the claim was taken on, never just the key: see
    /// [`AcquireGuard::phase`]. A guard whose entry has been replaced
    /// decrements nothing — its claim died with the entry it was counted on.
    ///
    /// `stamp` says whether this release is evidence the model was *used*.
    /// True is the normal case, and re-stamping on release (not only on
    /// acquire) is deliberate: a 40-minute generation that just finished is
    /// the most recently used model on the box, and an LRU that stamped only
    /// on entry would rank it as the coldest victim available. False is the
    /// dead-endpoint case (§3.2): a request that could not reach the
    /// container did not use it, and stamping it would hide a corpse from the
    /// idle reaper and the LRU for a full idle period every time a client
    /// retried — the reaper starvation this parameter exists to prevent.
    pub(super) fn release(&self, key: &Key, phase: &Arc<watch::Sender<Phase>>, stamp: bool) {
        let mut map = self.map();
        if let Some(e) = map.get_mut(key) {
            if !Arc::ptr_eq(&e.phase, phase) {
                return;
            }
            e.in_flight = e.in_flight.saturating_sub(1);
            if stamp {
                e.last_used = Instant::now();
            }
        }
    }

    pub(super) async fn podman(&self, args: &[&str]) -> std::io::Result<CmdOutput> {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.runner.run("podman", &argv).await
    }

    pub(super) async fn podman_argv(&self, argv: &[String]) -> std::io::Result<CmdOutput> {
        self.runner.run("podman", argv).await
    }

    /// `llama-server --help` for one image, cached (§3.6, "probe / help").
    ///
    /// Read from a throwaway container rather than compiled in, because the
    /// flag vocabulary is a property of the image: a build that gained
    /// `--spec-type draft-dflash` can say so without an lmgw release. It no
    /// longer requires anything to be *running* — which is the point, since
    /// with per-model containers the normal state is that nothing is.
    ///
    /// The binary is tried where [`llama_server_candidates`] says it may be:
    /// the image's own entrypoint first when that *is* the server (ik_llama.cpp
    /// installs `/llama-server`, which is not on `PATH`), then `PATH` and the
    /// known install paths.
    ///
    /// `extra_run_args` are the caller's resolved `podman run` args — the
    /// row's own, else the class's, exactly as a start would use them. They
    /// are not optional decoration: a build without `GGML_BACKEND_DL` (ik's,
    /// any hand-built one) links `libcuda.so.1` directly and exits 127 on
    /// `--help` unless the GPU device is attached, which is what those args
    /// do. The official images load the CUDA backend lazily and answer either
    /// way. As with [`Self::sdcpp_caps`], whichever caller probes an image
    /// first supplies them; a vocabulary is the image's either way.
    ///
    /// The probe runs the resolved image **ID**, not the reference, so a tag
    /// moved between the resolve and the run cannot file one image's flags
    /// under another's ID. An image that is not on the box has no ID yet;
    /// the probe then runs the reference as before (and, `--pull=never`,
    /// fails fast), and anything it did read is filed under the ID resolved
    /// afterwards.
    pub async fn help_text(
        &self,
        container_prefix: &str,
        image: &str,
        extra_run_args: &[String],
    ) -> Result<String, String> {
        let facts = self.image_facts(image).await;
        if let Ok(f) = &facts {
            if let Some(cached) = self
                .help
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&f.id)
                .cloned()
            {
                return Ok(cached);
            }
        }
        let target = facts.as_ref().map_or(image, |f| f.id.as_str());
        let entrypoint = facts.as_ref().ok().and_then(|f| f.entrypoint.clone());
        let name = format!("{container_prefix}-help-{}", crate::runtime::hash6(image));
        let args = vec!["--help".to_string()];
        // `--help` needs the GPU libraries, never a device ([`HIDE_GPUS`]).
        let extra_run_args = &without_gpus(extra_run_args);
        let fail =
            |why: String| format!("could not read llama-server --help from image '{image}': {why}");
        let candidates = llama_server_candidates(entrypoint.as_deref());
        // Kept apart, because they mean different things: a candidate that is
        // not in the image says nothing, one that is there and failed (a
        // missing `libcuda.so.1`, say) is the diagnosis — and must not be
        // buried under the "not found" of the paths tried after it.
        let mut absent: Vec<&str> = Vec::new();
        let mut broken: Option<String> = None;
        for bin in &candidates {
            let out = self
                .run_throwaway(&name, target, bin, extra_run_args, &args)
                .await
                // podman itself could not be executed: no other path can do
                // better, so say that instead of trying them all.
                .map_err(fail)?;
            // `--help` exits non-zero on some builds (ik's exits 1); trust the
            // output.
            if out.stdout.len() > 500 {
                if let Some(id) = self.id_after_probe(image, facts).await {
                    self.help
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id, out.stdout.clone());
                }
                return Ok(out.stdout);
            }
            let text = format!("{}\n{}", out.stdout, out.stderr);
            if text.contains("executable file") && text.contains("not found") {
                absent.push(bin.as_str());
                continue;
            }
            let said = if out.stderr.trim().is_empty() {
                "no usable output".to_string()
            } else {
                out.stderr.lines().take(3).collect::<Vec<_>>().join("; ")
            };
            let why = format!("`{bin} --help` exited {}: {said}", out.status);
            // 125 is podman's own failure (image not known, a bad run arg),
            // not the binary's: it would be the same for every path.
            if out.status == 125 {
                return Err(fail(why));
            }
            broken.get_or_insert(why);
        }
        Err(fail(match broken {
            Some(why) => why,
            None => format!(
                "none of {} exists in it (its entrypoint is {})",
                absent
                    .iter()
                    .map(|b| format!("`{b}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                entrypoint.map_or("unset".to_string(), |e| format!("`{e}`"))
            ),
        }))
    }

    /// What `podman image inspect` says about `image` right now: its ID and
    /// the executable its entrypoint names.
    ///
    /// Asked on every vocabulary lookup rather than remembered, because the
    /// answer is exactly what changes under a running lmgw: a rebuild retags
    /// the reference to a new ID, with no event lmgw would see. It is one
    /// local metadata read, tens of milliseconds; remembering it is what let
    /// a rebuilt image be validated against its predecessor's flags.
    ///
    /// `Err` when podman cannot be run, or the image is not on this box (the
    /// read never pulls, like [`Self::run_throwaway`]).
    pub async fn image_facts(&self, image: &str) -> Result<ImageFacts, String> {
        let out = self
            .podman(&[
                "image",
                "inspect",
                "--format",
                "{{.Id}} {{json .Config.Entrypoint}}",
                image,
            ])
            .await
            .map_err(|e| format!("podman image inspect could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman image inspect '{image}' failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        ImageFacts::parse(&out.stdout)
            .ok_or_else(|| format!("podman image inspect '{image}' named no image ID"))
    }

    /// The ID a probe's result is filed under: the one resolved before it,
    /// else — the image was not on the box then — whatever the reference
    /// resolves to now. `None` means there is still nothing to key by, and
    /// the result is returned uncached rather than filed under the
    /// reference, which is the key that went stale in the first place.
    pub(super) async fn id_after_probe(
        &self,
        image: &str,
        before: Result<ImageFacts, String>,
    ) -> Option<String> {
        match before {
            Ok(f) => Some(f.id),
            Err(_) => self.image_facts(image).await.ok().map(|f| f.id),
        }
    }

    /// Drop every cached `--help` vocabulary (llama and sd-server) filed
    /// under `image_id`, and say how many went.
    ///
    /// Takes an **ID**, full or the 12-character short form, with or without
    /// `sha256:`. Not a reference, deliberately: the caller that needs this is
    /// the one that just moved a tag off an image (a promoted build), and by
    /// then the reference resolves to the *new* ID, which has nothing to
    /// drop. Correctness never depends on calling it — a lookup re-resolves
    /// the reference every time, so a moved tag is a miss by construction —
    /// it only stops an image that is gone from keeping its entry.
    pub fn forget_help(&self, image_id: &str) -> usize {
        let id = image_id.trim().trim_start_matches("sha256:");
        // Shorter than podman's own short form is not an ID but a prefix of
        // arbitrarily many, and "drop whatever starts with `a`" is a bug in
        // the caller, not a request.
        if id.len() < 12 {
            return 0;
        }
        let mut dropped = 0;
        {
            let mut help = self.help.lock().unwrap_or_else(|e| e.into_inner());
            let before = help.len();
            help.retain(|k, _| !k.starts_with(id));
            dropped += before - help.len();
        }
        let mut sd = self.sdcpp_help.lock().unwrap_or_else(|e| e.into_inner());
        let before = sd.len();
        sd.retain(|k, _| !k.starts_with(id));
        dropped + (before - sd.len())
    }

    /// Run a **throwaway** container and return what it printed (§3.6,
    /// "probe / help"): `podman run --rm --replace --name <name> [mounts]
    /// --entrypoint <bin> <image> <args…>`.
    ///
    /// This is how the two read-only interrogations of an image work now that
    /// there is no always-running shared container to `podman exec` into: the
    /// `llama-server --help` vocabulary read (per *image*, hence the image
    /// argument) and `modelinfo`'s architecture probe. Both are questions
    /// about an image, not about a model that happens to be up — asking them
    /// of a throwaway container is what makes them answerable while every
    /// model is stopped, which is the normal state of a per-model install.
    ///
    /// `--entrypoint` is load-bearing: these images front llama-server with
    /// nginx (§10), so the default entrypoint starts a server rather than
    /// running the binary the caller named.
    ///
    /// Foreground (no `-d`), so the caller gets the output; named and
    /// `--replace`d so a run abandoned by a timeout can be collected by name
    /// with [`Self::rm_force`] instead of leaking a container the caller
    /// cannot identify. `--rm` covers every path that does exit. The class's
    /// run args in `mounts` pass through [`throwaway_args`], so a `-d`,
    /// `--restart`, `--publish` or `--name` there cannot contradict that.
    ///
    /// `--pull=never` is deliberate: these are *read* paths (a settings form
    /// validating flags, a metadata probe), and an image that is not on the
    /// box yet must not turn one into a multi-gigabyte download nobody asked
    /// for. It fails fast instead, the caller degrades to "no vocabulary to
    /// check against", and the pull happens where it belongs — the first real
    /// start of a model on that image.
    pub async fn run_throwaway(
        &self,
        name: &str,
        image: &str,
        entrypoint: &str,
        mounts: &[String],
        args: &[String],
    ) -> Result<CmdOutput, String> {
        let mut argv: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--replace".into(),
            "--pull".into(),
            "never".into(),
            "--name".into(),
            name.into(),
        ];
        argv.extend(throwaway_args(mounts));
        argv.push("--entrypoint".into());
        argv.push(entrypoint.into());
        argv.push(image.into());
        argv.extend(args.iter().cloned());
        self.podman_argv(&argv)
            .await
            .map_err(|e| format!("podman run could not be run: {e}"))
    }

    /// `podman logs --tail <n>` for one container, verbatim (§3.6/§8's `logs`
    /// verb — the `lmgw__container action=logs` surface).
    ///
    /// Unlike [`Self::log_excerpt`] this is not a failure post-mortem: it is a
    /// caller-driven read of whatever the container has printed, so it
    /// returns the raw tail rather than filtering for "interesting" lines.
    /// With one container per model this is the only place a caller without a
    /// shell on the box can see why a model is misbehaving after it started
    /// cleanly.
    pub async fn logs_tail(&self, container: &str, tail: usize) -> Result<String, String> {
        let out = self
            .podman(&["logs", "--tail", &tail.to_string(), container])
            .await
            .map_err(|e| format!("podman logs could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman logs failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(format!("{}{}", out.stdout, out.stderr))
    }

    /// Best-effort `podman logs --tail` for a failed start. Prefers the lines
    /// that look like a diagnosis and falls back to the plain tail when none
    /// match — an empty excerpt would leave the error saying nothing at all,
    /// which is the whole failure mode this exists to prevent.
    pub(super) async fn log_excerpt(&self, container: &str) -> Vec<String> {
        let Ok(out) = self
            .podman(&["logs", "--tail", &LOG_TAIL.to_string(), container])
            .await
        else {
            return Vec::new();
        };
        log_excerpt_of(&format!("{}\n{}", out.stdout, out.stderr))
    }
}

/// [`Registry::log_excerpt`]'s reading of a log tail, for a caller that
/// fetched the tail itself (the benchmark's own container, which is not a
/// registry entry): the last [`LOG_EXCERPT_LINES`] lines that look like a
/// diagnosis, else the last lines as they are.
pub fn log_excerpt_of(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = |it: Vec<&str>| -> Vec<String> {
        it.into_iter()
            .rev()
            .take(LOG_EXCERPT_LINES)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let interesting: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            ["error", "failed", "unknown model", "exiting", "cannot"]
                .iter()
                .any(|needle| low.contains(needle))
        })
        .collect();
    if interesting.is_empty() {
        tail(lines)
    } else {
        tail(interesting)
    }
}
