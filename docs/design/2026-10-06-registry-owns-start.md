# The registry owns the start: requests that go away mid-load

A fix record. Companion to [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
("§n"): §3.2 (`acquire`), §3.4 (reconciliation), §3.6 (start and stop), §4 (admission).

## The incident (2026-10-06)

A client called `POST /v1/count_tokens` for a local model that was not running, with a 30 s
client timeout. Counting on a local model starts its container, and the load took longer than
30 s, so the client gave up mid-load. From then on the model's container was running and lmgw
did not know it:

- every request for that model queued for the GPU for the whole `vram.queue_timeout_seconds`
  (300 s) and was refused with `vram_queue_timeout`, while the queue said "waiting for a busy
  model to finish" with no busy model anywhere;
- the container's memory counted as used by something outside lmgw, so nothing reaped or evicted
  it, and `container stop` answered "stopped" without stopping anything;
- no request-log row showed any of it: counts were never logged, and a request whose client went
  away left no row.

## Root cause

hyper drops a handler's future when the client's connection closes. The start sequence
(`podman run`, the readiness poll, flipping the entry to `ready`) ran inside the requesting
future. Dropped between `podman run` and `ready`, the start's claim removed the `starting` entry
on its way out and left the container running. Reconciliation with podman ran only at boot, so
nothing found it again until the next restart. A stop had the same flaw: dropped between marking
the entry `stopping` and removing it, the entry stayed `stopping`, and every later request for
the model waited on it forever.

## What changed

1. **The registry owns starts and stops** (`runtime/registry/owned.rs`). The registry runs the
   start sequence and the stop as tasks it spawns. A request only waits for the result, so a
   request that goes away loses its wait, never the start. A model started this way ends `ready`
   and idle, and the reaper and eviction treat it like any other. The entry is `starting` before
   `podman run` runs.
   *First claim:* the start task takes the requesting request's in-flight claim under the same
   lock that flips the entry to `ready` and hands it to the request. So nothing can reap or evict
   the model between "it is up" and "this request holds it" (§3.2). If the request is already
   gone at that moment, the task takes no claim.
2. **Reconciliation after boot** (`runtime/registry/unheld.rs`, `runtime/lifecycle/readopt.rs`).
   lmgw lists the running containers that carry this instance's prefix label. For each one the
   registry has no entry for, it runs boot's own adoption. A container that runs what its model's
   row renders now is adopted as `ready` and idle. A container that does not is stopped and
   removed. Each adoption and removal is logged at WARN with the container's name and the reason.
   The pass skips names the registry holds in any state, benchmark and agent containers, and
   containers that are not running. A container it removes is taken down under a `stopping` entry,
   so a start of the same model waits for the name instead of racing `podman run --replace`.
   - *When, and bounded.* The reaper tick (15 s) spawns the pass as its own task and skips while
     one is still running, so the hold sweep and the reaping never wait for podman. An admission
     wait with nothing of lmgw's to evict runs one pass (or waits for the running one) before it
     settles into waiting, with the admission gate let go, within what is left of its queue
     budget and at most one tick (`vram/stall.rs`). `apply`, `restart` and the group `stop` wait
     at most one tick too. One pass runs at a time in total. A pass cut short loses nothing: an
     adoption inserts last, under the map lock, and a removal is the registry's own task, bounded
     by the stop grace plus `vram.unload_timeout_seconds`. An adoption probe is bounded like a
     start's readiness probe (2 s for llama and audio).
   - *Only this lmgw's own.* Every start stamps `lmgw.owner=<instance id>`, the data dir's id the
     build machinery already keeps. Two instances can share a prefix — every dev copy is
     `lmgw-dev` — and a pass acting on the other's containers every tick would make them fight.
     So the pass acts only on containers carrying its own owner; another owner's, or one with no
     owner label, is logged once and left alone. Boot reconciliation stays prefix-based on
     purpose: a container an older build left behind has no owner label, and boot is what takes
     it back after an upgrade.
   - *Still loading.* A container that does not answer its probe and is younger than
     `vram.load_timeout_seconds` may be loading, and is left alone; older, it is removed. A
     start whose container disappears while it loads ("no such container") now fails at once
     instead of waiting out its load budget.
   - *Backoff.* A container the pass cannot remove, or one it adopted that loses its entry again
     (its stop keeps failing), is remembered by name and creation time and retried after two
     ticks, doubling up to 16 ticks (four minutes). The first failure is logged at WARN, the
     retries at DEBUG.
3. **`container stop` without an entry** stops a running container with the model's name by name
   and says so, if it is this lmgw's; another owner's is named and left running. With nothing
   running, it says "not running". `apply`, `restart` and the group `stop` run a reconciliation
   pass first.
4. **Rows** (`proxy/unanswered.rs`). Chat, legacy completions, embeddings, rerank and the three
   token counters open a guard where they open the in-flight gauge. If the request is dropped
   before its row is written, the guard writes a `499` `client_disconnected` row that names where
   the request was, and it closes the gauge. The row itself is written by a task of its own, so a
   handler dropped during the insert neither loses it nor leaves the gauge open. A counter writes
   a row when it fails, or when it had to start its model; a count against a model that was
   already running still writes none. Count rows are class `tool`, the class of a row that is no
   model call, so they stay out of the chat class's token averages, the unpriced counts and an
   agent run's model calls; they and the `499` rows still count as requests and errors.
5. **Smaller fixes.** The queue now says what it waits for when nothing of lmgw's is resident:
   memory held outside lmgw. One llama or audio readiness probe takes at most the 2 s
   container-state interval instead of the whole load budget. A start whose task ends without a
   result logs at WARN. A start or a climb whose entry was taken no longer removes its container
   when another entry holds that name again. A start learns its container's PID and refreshes the
   dashboard's VRAM frame whether or not its requester is still there.

## Recovering an instance hit by the old bug

Symptom: requests for one local model end in `vram_queue_timeout`, and `podman ps` shows that
model's container running while the dashboard does not list it.

- **With this fix**, the restart into the new build is the recovery: boot reconciliation adopts
  the container (or removes it, if its row changed since). The pass after boot leaves it alone —
  it carries no owner label — and logs that once. A container this build starts and loses track
  of is adopted by the next reaper tick, with a WARN line naming it.
- **On a build without it**, stop the orphaned container: `podman stop <name>`. Use stop, not
  `rm`: the logs stay, and the next start's `--replace` collects the container. lmgw starts the
  model again on the next request. A client that talks to the container's port directly should
  be stopped first.
