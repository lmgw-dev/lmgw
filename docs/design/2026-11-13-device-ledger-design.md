# The device ledger: VRAM admission per card, and lmgw placing models on cards

**Draft, 2026-11-13, revised the same day after an adversarial review** (§14 says what it found
and where each finding went). On a machine with several graphics cards lmgw keeps a ledger per card,
chooses the card or cards for every model that does not name its own, tells the engine exactly
which cards it has and how the model is split over them, and charges each card what it put there.
WP9 and WP1 to WP4 are built (2026-11-13), WP0 has measured (2026-11-16, §1.1), WP5 is
built (2026-11-16, in two rounds; its notes under §13 say what it leaves), and so are WP6
(2026-11-16, splits; its note under §13 says what it leaves and where it reads this record),
WP7 (2026-11-16, repacking; likewise), WP8 (2026-11-17, words; likewise) and WP12
(2026-11-17, fit marks for splits and Get it asking; likewise).
"Decisions" separates the owner's rulings from this record's own decisions; §11 has no open
question left (the review's three are rulings 14 to 16, WP0's two are 17 and 18); "Not yet" lists what this
version leaves out, and nothing there is ruled out.

The factual base is the first run of lmgw on a machine with more than one card, 2026-11-13 (§1):
four NVIDIA cards, shared with other programs, lmgw in its server image under Docker.

Companion records, cited by short name:
- "per-model containers §n": [per-model containers](2026-08-30-per-model-containers-design.md)
  (one container per model, the VRAM ledger, admission, headroom, partial fitting);
- "machine check §n": [the machine check](2026-11-03-machine-check-design.md) (lmgw's own CDI
  spec, the card list);
- "headless server §n": [lmgw on a server](2026-11-08-headless-server-image-design.md);
- "ladder §n": [ladder models](2026-09-27-ladder-models-design.md) (climbing);
- "Windows §n": [lmgw on Windows](2026-11-10-windows-native-design.md) (native engine starts).

| § | Content |
|---|---|
| 1 | What was measured |
| 2 | Cards: identity, and what the probe reports per card |
| 3 | The ledger per card |
| 4 | A placement, and what a model costs on it |
| 5 | Choosing the placement |
| 6 | Eviction and repacking |
| 7 | Telling the engine |
| 8 | The pin |
| 9 | Headroom and a declared budget |
| 10 | What shows it: API and UI |
| 11 | Open questions |
| 12 | Tests |
| 13 | Work packages |
| 14 | The review of 2026-11-13 |

**Today** (lines at `main` f5c15bd3; paths under `crates/lmgw-core/src/`; a `~` marks a line
read from a range):
- **One pooled ledger.** `pooled(budget, devices)` (`vram/ledger.rs:943`) sums every device's
  total and free; `Capacity{total, free, measured, kept_free}` (`ledger.rs:20`) is one figure for
  the machine. Its callers: `ledger.rs:177`, `:463`, `:503`, `:705`, `vram/partial.rs:555`.
  `known_total` (`ledger.rs:538`) and the `device_total` and `device_count` atomics
  (`vram/scheduler.rs:176`, `:180`) carry the pooled total and the number of cards, nothing more.
- **"Does it fit" is one comparison of two numbers**, in three places: `cap.free >= needs`
  (`vram/queue.rs:552`), the guest's (`vram/background.rs:467`) and the climb's
  (`vram/climb.rs:971-976`), plus the verdict's (`verdict_for`, `vram/verdict.rs:110`;
  `external_table`, `vram/admission.rs:109`). A reservation, a grant, a resident's charge and a
  waiter's need are each a single `u64` (`Reservation.bytes`, `scheduler.rs:119`;
  `Grant::Whole(u64)`, `partial.rs:93`; `ResidentView.estimated_bytes`, `vram/view.rs:15`).
- **Eviction is one flat list**: `evict_one` (`queue.rs:752`) orders the idle `Ready` entries by
  `last_used` and stops the oldest, whatever card it is on; `freeable` (`queue.rs:1033`) is a sum
  over all residents and is added to the pooled free in `fit_in` (`verdict.rs:320`).
- **A card has no identity.** `GpuMemory{index, name, total_bytes, used_bytes, free_bytes}`
  (`vram/nvml.rs:20`) is all the scheduler knows. The UUID is read once, in the machine check's
  child process (`machine/probe.rs:32`, `NvmlCard`), and never reaches it; the PCI address is
  read by the scheduler's probe for the Windows probe alone (`nvml.rs:182`, `pci_addresses`).
- **What a process holds is summed over the cards.** `ProcessMemory{pid, bytes}`
  (`nvml.rs:123`) has no device; NVML is read per device and then added up (`merge_device`
  `nvml.rs:335`, `sum_devices` `:377`). lmgw's share and the outside use (`measure`,
  `ledger.rs:572`; `members_share`, `vram/attribution.rs:640`) are pooled figures.
- **The cards reach an engine through run arguments.** Every class defaults to `--device
  nvidia.com/gpu=all` (`config/settings_classes.rs:~259`, `~491`, `~570`); a row's
  `extra_run_args` (`config/models.rs:37`, `:211`, `:312`, `:403`) replaces the class's, and go
  into the Podman line as written (`runtime/argv/podman.rs:129`). `cards_seen`
  (`vram/partial.rs:471`) counts how many cards a row names and never which.
- **llama-server's own split is unmanaged.** Nothing renders `--tensor-split`, `--split-mode`,
  `--main-gpu` or llama-server's `--device`; they are not in `MANAGED_FLAGS`
  (`llama_caps.rs:~80`). `LlamaPlacement{ctx, n_cpu_moe, n_gpu_layers}`
  (`runtime/argv/llama_fit.rs:12`) and the `Sizer` (`vram/plan/chat/fit.rs:52`) describe one
  card; `fit_to_card` returns the footprint unchanged unless the row sees exactly one card
  (`vram/partial/llama.rs:94`).
- **Headroom is one figure**, fixed or learned from the pooled outside use
  (`vram/headroom.rs:124`; `vram_outside_hours`, migration 0106, keyed by the hour alone).
- **Every test states one card.** Each fake `GpuProbe` returns a single `GpuMemory` with `index:
  0` (`tests/it/vram_admission/world.rs:269`, `FakeGpu`, and six more).

What that does on several cards (seen or read 2026-11-13, §1):
- A row pinned to one card is fitted against the machine: `fit_in` hands `fit_on` the pooled
  total and the pooled free. A row pinned to a card with 7.5 GiB free is told of 57 GiB.
- A model that no single card can hold is admitted when the pool can, and llama.cpp splits it as
  it sees fit; lmgw charges it "as written" (per-model containers §4).
- A model spread over four cards costs more than on one, and nothing counts that.

## Decisions

*The owner, 2026-11-13:*
1. **The ledger becomes per device.** No contained fix for a pinned row first.
2. **A row may name its cards, and need not.** Naming them is how a model is put on a specific
   card (the fastest one, say). Every other model is placed by lmgw.
3. **lmgw distributes the models over the cards**, the pinned ones excepted. The goal is the
   most efficient use of VRAM whatever the number of cards and models.
4. **A card is charged the more precise value**: what is put on it, not a share of a guess.
5. **Evict before splitting.** Where a model fits on one card once an idle model is evicted,
   and fits now only spread over two, lmgw evicts.
6. **Repacking is allowed when idle**: lmgw may move an idle model to another card, which
   reloads it.
7. **VRAM before speed.** The cards' speeds do not enter the choice; a model that should run on
   a particular card is pinned to it (ruling 2).
8. **A pin is a field on the row and holds the cards' UUIDs**, not run arguments and not
   indices.
9. **Headroom is for a desktop**, where apps change through the day; the owner did not see it
   applying on a server. Superseded by ruling 10.

*The owner, 2026-11-13, on this record's open questions:*

10. **[Q1] Headroom stays, per card and learned per card** (§9.1, this record's proposal against
    ruling 9: a four-card test server shares a card with programs that take memory on demand). **A
    bad learned value can be reset.**
11. **[Q2] A row with its own `--tensor-split` must be pinned** to as many cards as the split has
    entries. It is started with its split as written and charged by it. Unpinned, the editor says
    the split needs a pin, and lmgw places the row as if the split were not there (§7.2).
12. **[Q3] Aux, audio and image models go on one card** in this version (D10).
13. **"Get it" waits for its engine image** before it sets the row up. Found on the same run
    (§1), not part of the ledger, and built with it (WP9).

*The owner, 2026-11-13, on the questions the review opened:*

14. **[Q4] A busy model in the way: split now.** A model that fits over several cards now does
    not wait for a busy model to leave one. Real use will show what it costs (§5.2).
15. **[Q5] No context set means the model's own**, and stays that. On a machine with several
    cards such a row is kept on one card: where the model's own context does not fit one, the
    largest that fits one card is used, and it is never spread over cards for it. **The
    model's editor says so where the context is set** (§5.3, §8.2).
16. **[Q6] Ruling 11 is a chat row's.** An aux row's own `--tensor-split` is dropped with a
    warning and the row goes on one card (§7.2).

*The owner, 2026-11-16, on what WP0 found:*

17. **[Q7] The engine's cost per card is learned per card.** After a start, what the probe says
    the model's process holds on each card, less the buffers lmgw estimated there, is that card's
    engine cost. It is kept per card and shown in Settings → GPU; a card that has none yet is
    charged a stated default per backend, 640 MiB for CUDA (§4.2).
18. **A drafter without a speculative type gets `draft-simple`.** lmgw renders `--spec-type
    draft-simple` when a row sets a drafter and no type, and the rendered command line shows it
    (§1.1, WP10).

*Standing rules that bind this design:* no hidden or guessed limits (every bound is a real
quantity, shown); confirm, then do, never refuse; a configured fallback always applies, for every
reason; no config keys as labels or in messages; binary units; nothing changes the `/v1` wire.

*This record's own decisions:*

- **D1 — A card is known by a stable id.** `GpuMemory` gains `id: DeviceId`: the NVIDIA UUID
  (`GPU-…`, `nvmlDeviceGetUUID`), on AMD the PCI address the probe already reads
  (`vram/amdgpu.rs:402`). The index stays for display and for nothing else. Why: the index is a
  position in the driver's list and changes when a card is added or fails; a pin and a
  container's recorded placement outlive a restart.
- **D2 — No decision reads a pooled figure.** The ledger holds a `Capacity` per device. The
  pooled total and free stay in the API as sums, for a headline and for clients that read them
  today, and are named as sums.
- **D3 — What a process holds is known per card.** `GpuProbe::processes` answers per device;
  lmgw's share and the outside use are per device. NVML already reads per device.
- **D4 — A charge is a placement**: a list of (card, bytes). Reservations, grants, registry
  entries, residents and waiters carry one instead of a number. A model on the CPU has the empty
  placement.
- **D5 — What the engine itself costs per card is part of the charge**, not of the headroom:
  the CUDA context and whatever llama.cpp keeps on every card it touches. It is measured (WP0),
  and shown. Why: it is the model's cost, it scales with the number of cards, and the owner's
  ruling 4 asks for what lands. WP0 (§1.1) found it is a figure of the card, not of the engine:
  621 MiB on the Blackwell, 480 on the Ada, the same in every case. It is learned per card,
  with a stated default per backend until it is (ruling 17).
- **D6 — One card whenever one can hold it**, by the order of §5.2: on one card now, on one card
  after evicting, over several now, over several after evicting, after waiting for a busy model,
  then fitted smaller. Why one
  card first: the same model took 5.3 GiB on one card and 6.4 GiB over four (§1), and a split
  ties up two cards for one model.
- **D7 — Among the cards that can hold it, the one it leaves least free on.** Why: the large
  free blocks stay for the models that need them. Speed does not enter (ruling 7).
- **D8 — Repacking is an eviction whose victim is started again elsewhere.** It adds no wait to
  the request that needed the room, and needs no planner of its own (§6).
- **D9 — The engine gets exactly its cards.** The container is passed the placement's cards by
  UUID in place of the class's `nvidia.com/gpu=all`, and a split model gets its split as flags
  (§7). lmgw never leaves the split to the engine and then guesses at it.
- **D10 — Only a chat model is split in this version.** Aux, audio and image models go on one
  card (ruling 12).
- **D11 — One gate.** Decisions stay one at a time (per-model containers §4); a gate per card
  would let two decisions each take half of a split. A start that waits gives it back (D15).
- **D12 — A container records its placement** in a label, as a fitted start records its fit,
  so a restart of lmgw adopts it with the charge it was given (§3.3).
- **D13 — A declared budget stays a bound on the whole.** `budget_mb` with telemetry is a second
  condition beside the cards' own room; without telemetry it is one stated card (§9.2).

*Decisions after the review of 2026-11-13 (this record's own; §14 maps the findings):*

- **D14 — How many cards a fit sees comes from the candidate, not from the run arguments.**
  `fit_to_card`, `Room.cards` and `cards_for` take the count from the placement `place` is
  trying; the run arguments only say whether the row uses a card at all. Why: with the default
  `=all` the count is the machine's, and a row is never fitted to one card (§5.2 step 6) nor an
  image model granted less (`partial.rs:444`).
- **D15 — A start that has to wait commits to its cards and lets the gate go.** `place` answers
  with the cards and what must leave them first; the waiter holds a claim on those cards, the
  gate is released while it waits, and it asks again for those cards only. A later start never
  takes room a waiter was promised. Why: choosing again after every eviction can evict on
  several cards for one start, and a waiter that keeps the one gate (`queue.rs:651`) would now
  block every start that fits another card (§5.5).
- **D16 — `place` has modes**, one per kind of question today's code asks (§5.6): an owner's
  start, a guest's, a climb, the outside verdict.
- **D17 — The cards are replaced only where the choice narrows them.** A placement on every card
  the row's arguments already pass leaves the arguments as written. A machine with one card is
  never touched: its containers start with the arguments they start with today.
- **D18 — Only NVIDIA cards are handed out, through CDI.** An engine that cannot be handed a
  card (AMD, a Vulkan or HIP build, a start without a decision) is placed on its cards as a
  group, as the pooled ledger does today (§5.7).
- **D19 — Every way a row or a class names a card today has a stated fate** (§8.1): read as a
  pin, rewritten, or dropped with a warning.
- **D20 — `--tensor-split`, `--split-mode`, `--main-gpu` and llama-server's `--device` are
  flags lmgw knows, not flags it manages.** A list of their own beside `MANAGED_FLAGS`
  (`llama_caps.rs:79`): not rejected in a row's arguments, not required of an image.
- **D21 — A moved model keeps its age**, moves to one card only, and is not moved again before
  it has served a request (§6.2).
- **D22 — `budget_mb` keeps its meaning**: lmgw and the other programs together stay within it.
  This replaces D13's second sentence (§9.2).
- **D23 — The new type is `CardCharge`**, not `Placement`: `runtime::Placement {Gpu, Cpu}`
  (`runtime/placement.rs`) exists, stays, and is what the GPU hold reads. A CPU row keeps
  `Placement::Cpu` and has an empty charge. This record's prose goes on saying "placement".

## 1. What was measured

2026-11-13 on a four-card test server shared with other programs: lmgw 0.7.0 at f5c15bd3 in the
server image under Docker (headless server; user docs `docs/server.md` §14), llama.cpp
`server-cuda` build 12352. Four cards in NVML order: three RTX 6000 Ada (49140 MiB) at 0, 1 and 3,
one RTX PRO 6000 Blackwell (97887 MiB) at 2. Other programs held 184 GiB of the 245, some of them
taking memory on demand. Free at the start: 7641, 12220, 22690 and 16143 MiB.

The model: Qwen3-4B-Instruct-2507, Q4_K_M, context 16384, slots on auto. lmgw's estimate
(weights, KV cache, compute): 3737 MiB. Headroom fixed at 1024 MiB.

| Start | Card 0 | Card 1 | Card 2 | Card 3 | Sum |
|---|---|---|---|---|---|
| every card passed (`nvidia.com/gpu=all`), llama.cpp's own split | 1166 | 1378 | 2306 | 1558 | 6408 MiB |
| `extra_run_args` `--device nvidia.com/gpu=1` | | 5256 | | | 5256 MiB |

- The pinned start landed on the card NVML and `nvidia-smi` call 1.
- One card holds 1519 MiB more than the estimate. Three more cards cost 1152 MiB more: 384 MiB
  a card.
- lmgw's measured share was 6408 MiB in the first row, the sum `nvidia-smi` showed for the
  process; the outside use 184104 MiB. The per-process attribution works from inside the
  container, with the other programs' processes in other containers.
- Getting the same model with "Get it" planned a context of 262144: 24.4 GiB, marked as fitting
  against a capacity of 59.8 GiB, the pool. The most free on one card was 22.2 GiB. It was not
  started at that size.
- A pinned row's fitting against the pooled figures was read in the code and not provoked: it
  would have run a shared card out of memory.

- The same "Get it" created its row while the engine image it had started to pull was still
  coming (4.4 GiB against a 2.5 GiB model file): the row's warning read "could not read
  llama-server --help from image …: image not known — the extra args and flag values were not
  validated" (ruling 13, WP9).

What these numbers do not say, and WP0 measures: how the 1519 MiB divides into a cost per card
and the estimate's own error; what lands on each card for a split lmgw sets itself; where
llama.cpp puts the compute buffers and the output layer of a split model.

### 1.1 WP0, 2026-11-16

`scripts/multi-card-measure.py` on the same server, against the deployed lmgw 0.7.0 (its cards
matched to `nvidia-smi`'s by index), llama.cpp `server-cuda` as pulled there, context 16384, four
slots, every case's cards passed by UUID with `NVIDIA_VISIBLE_DEVICES` and
`CUDA_DEVICE_ORDER=PCI_BUS_ID`. "Process" is what `nvidia-smi` lists for the model's
llama-server on that card, after load and again after one request (equal within 4 MiB in every
case); weights, KV and compute are llama.cpp's own buffer lines; "engine" is process less those.
The base model is §1's 4B (estimate 4779 MiB: weights 2382, KV 2304, compute 93).

| Case | Card | Layers | Weights | KV | Compute | Engine | Process |
|---|---|---|---|---|---|---|---|
| one card | 2 (Blackwell) | 36 + output | 2376 | 2304 | 93 | 621 | 5394 |
| `--tensor-split 15,22` | 2 (Blackwell) | 15 | 860 | 960 | 205 | 621 | 2646 |
| | 3 (Ada) | 21 + output | 1516 | 1344 | 160 | 482 | 3502 |
| `--tensor-split 9,13,15` | 1 (Ada) | 9 | 521 | 576 | 205 | 476 | 1778 |
| | 2 (Blackwell) | 13 | 738 | 832 | 160 | 622 | 2352 |
| | 3 (Ada) | 14 + output | 1117 | 896 | 160 | 481 | 2654 |

Sums: one card 5394, two cards 6148, three cards 6784 MiB, for the same model.

- **The engine's own cost per card is a figure of the card, not of the model or the number of
  cards**: 621–627 MiB on every start on the Blackwell, 476–486 MiB on every start on an Ada,
  in all ten cases (projector, drafter and MoE cases included, where the extra buffers are
  named). lmgw's estimate of the one-card start was short by 615 MiB, which is that cost: the
  estimate's own error on weights, KV and compute is nil here.
- **`--tensor-split` given layer counts places exactly those layers**, its entries summing to
  `n_layer + 1`: the last entry's extra layer is the output layer, which lands on the last card.
  The token embeddings stay in host memory (`CPU_Mapped`, 304 MiB). Each card's weights matched
  the sum of its layers' tensor bytes from the GGUF to the MiB, for the 4B, the vision model and
  the MoE model.
- **The KV cache per card follows its layers** (64 MiB a layer for the 4B at this context).
- **A split costs more compute per card than one card does.** llama.cpp turns on pipeline
  parallelism for a split (`pipeline parallelism enabled`, `sched copies = 4`): the first card's
  compute buffer was 205 MiB and every other card's 160, against 93 on one card. With
  `--n-cpu-moe` it does not (`sched copies = 1`): 185 and 92 MiB. lmgw's compute estimate does
  not model this yet.
- **The projector** (`mmproj`, Qwen3-VL-4B Q8 and its F16 projector, split 15/22): on the first
  card, about 800 MiB of weights plus a compute buffer of its own (322 MiB), and the first card's
  model compute buffer rose to 302 MiB.
- **A drafter** (Qwen3-0.6B) with llama.cpp's default placement is spread over every card the
  engine sees (model 191 / 413 MiB, KV 768 / 1024 MiB), not by `--tensor-split`.
  `--device-draft CUDA0` (`--spec-draft-device`) put it on the first card alone (model 604, KV
  1792, compute 42 MiB). A drafter is only kept when a speculative type is set: without
  `--spec-type` (default `none`) llama.cpp loads the drafter and frees it at once ("no
  implementations specified for speculative decoding").
- **`--n-cpu-moe 2`** (OLMoE-1B-7B Q4_K_M, split 7/10): the first card's weights were less by
  the two layers' expert tensors, as predicted. **`--moe-cache-mib 256`** went to the first card
  alone (`CUDA0 MoE cache size = 252.89 MiB`), not "split like the layers": the cache holds the
  host experts, which are the first layers'.
- **The device order.** With four cards passed by UUID in reverse PCI order, the engine without
  `CUDA_DEVICE_ORDER` numbered them 0, 1, 3, 2 (the Blackwell last); with
  `CUDA_DEVICE_ORDER=PCI_BUS_ID` in PCI order, 0, 1, 2, 3. NVML's, `nvidia-smi`'s and the CDI
  index named the same card throughout.
- **A start with `--device nvidia.com/gpu=GPU-<uuid>` runs**, and the engine saw that card
  alone, with and without `NVIDIA_VISIBLE_DEVICES` (this host has no legacy hook).

## 2. Cards

### 2.1 Identity (D1)

`DeviceId` is a string newtype in `lmgw-api-types`: `GPU-<uuid>` for an NVIDIA card, the PCI
address (`0000:17:00.0`) for an AMD one. `GpuMemory` and the API's `VramDevice`
(`lmgw-api-types/src/status.rs:538`) gain `id`. The NVML probe reads it beside the memory
(`dev.uuid()`, as `machine/probe.rs:147` does); the AMD probe has the address already; the
Windows probe passes NVML's through (`vram/wddm/probe.rs:23`).

A card is shown as its name and index ("RTX PRO 6000 Blackwell (card 2)"), never as its id; the id
is in the detail line, as the machine check's card list has it.

### 2.2 The order of the cards

Four numberings have to name the same card: NVML's index, `nvidia-smi`'s, the CDI names
`nvidia.com/gpu=<index>`, and CUDA's device numbers inside a container. The first three agreed on
the test server (§1). CUDA's default order is fastest first, which differs on a machine with unlike
cards; lmgw sets `CUDA_DEVICE_ORDER=PCI_BUS_ID` only for a native start today
(`runtime/launcher/native/gpu.rs:135`).

lmgw stops depending on the numbering: a container is passed its cards by UUID (§7), and is given
`CUDA_DEVICE_ORDER=PCI_BUS_ID` so that the order of a split's entries is the order of the cards'
PCI addresses, which lmgw knows. lmgw's own CDI spec (machine check §3.6) had the UUID names on
the measured machine: `0` to `3`, the four `GPU-<uuid>` names and `all`. WP0 measures the engine's
device order inside the container (`llama-server --list-devices`).

### 2.3 What the probe reports per card (D3)

`GpuProbe::processes` returns, per device, the processes on it and what each holds there:
`ProcessMemory` gains the device, or the answer becomes a list per `DeviceId`. `sum_devices`
(`nvml.rs:377`) goes; `members_share` (`attribution.rs:640`), `Measured` and `Shares`
(`ledger.rs:924`, `:933`) become per device. The AMD probe matches its DRM clients to a card by
PCI address already and the Windows probe matches D3DKMT adapters by it (`wddm.rs:130`); both sum
at the end today and stop doing so.

A process that the driver lists on a card without a figure makes that card's split unknown, as it
makes the pooled one unknown today; the other cards' stay known.

## 3. The ledger per card

### 3.1 What it holds

`Ledger.devices` becomes the ledger's spine. Per device: the driver's total, used and free; what
lmgw's containers hold there, measured; the outside use there; and what is kept free there that
the driver cannot show yet (`KeptFree`: starts in flight, image peaks, pending loads), each from
the placements of the entries concerned. `Capacity` is per device; the `Ledger.capacity` field
goes, and a caller that wants "the machine" says which sum it means.

The other pooled readers move with it:
- `beside.rs:204` (`outside_of`), `peak.rs:249` (the learned image peak, "one number for the
  box"), `climb.rs:364` and `:963`, `background.rs:915` (`free_for_climb`),
  `modelinfo/plan_fit.rs:48` (`Card::of`) and `modelinfo/fit.rs`;
- the preview fit inside `footprint()` (`verdict.rs:351-365`: the pooled `known_total` less
  `outside_recent`), which every surface that shows a footprint reads;
- **warm groups**: `group_capacity`, `crowded_beside` and `on_card_now` (`beside.rs:65-131`,
  used by `realtime/warm/admit.rs:272` and `warm/kept.rs:69`) check a group of stages against
  one pool. Per card a group is placed stage by stage with `place`, each stage's charge counted
  before the next is asked; a group that fits only spread over several cards is fine, its
  stages are separate containers;
- **the realtime budget** (`ops/realtime_budget.rs:367-454`): the cascade against
  `view.capacity_bytes` and one headroom. It asks `place` for its three models in turn;
- **the benchmark** renders its own container from the row's arguments (`bench/plan.rs:139`,
  `:503`) and sums VRAM over the cards (`bench/sampler.rs:128`, `:413`). A benchmark of a row
  runs on the placement the row would get, and its figures are per card;
- `WaitPhase::WaitingForRoom{needs_bytes, free_bytes}` (`lmgw-api-types/src/status.rs:628`)
  and the chat-media progress carry one number each: they gain the card or cards waited for.

### 3.2 The charge of an entry

An entry's charge is its placement (§4): the bytes on each card it was given. A start in flight
is charged its placement plus its margin on each card it touches. The estimated counterpart of
the measurement is the sum of the placements on a card.

### 3.3 Adoption (D12)

Every GPU container gets `lmgw.placement=<id>:<bytes>[,<id>:<bytes>…]` beside `lmgw.llama-fit`
and `lmgw.grant` (`runtime/argv/podman.rs:107-122`). Boot reconciliation
(`runtime/registry/reconcile.rs:506`, `adoptable`) reads it back into the entry. A container
without the label, started by an lmgw before this design, is charged by what its processes hold
on each card, which the probe now says; it is not restarted for it.

A recorded card that is gone (its id is in no device) leaves the entry adopted and charged
nowhere, with a warning on the entry; the engine is still running on whatever it has.

Two limits. Charging an unlabelled container by measurement needs its processes' figures, which
are read only while the outside trigger is on (`ledger.rs:742`, `cache_pids`) and where the
probe lists processes: without them it is charged its estimate on the cards it could be on as a
group (§5.7), and the entry says that this is a guess (the one place ruling 4 is not met, until
that container is next started). And "not restarted" holds for chat, aux and image containers; an
audio container whose class names a device is restarted once (§8.1). (WP4 charges every
unlabelled container so until WP5: see its note.)

An lmgw older than this design adopts a container that carries `lmgw.placement`: boot adoption
compares the container's command and its run arguments' digest and no other label
(`launcher/podman/listing.rs`, `same_container`), and charges it as it always did. It removes a
container whose `lmgw.llama-fit` records a split (WP6): `Fitted::parse` refuses unknown keys
(`llama_fit.rs:114`), the command is then rendered without the fit's values and differs. A
roll-back restarts the split models; nothing else is lost. (Corrected at WP4: this paragraph said
the label alone made an older lmgw remove the container.)

## 4. A placement, and what a model costs on it

### 4.1 The type

`CardCharge` (D23): an ordered list of `(DeviceId, bytes)`, in the cards' PCI order. It replaces the
`u64` in `Reservation`, `Grant`, `PartialGrant`, `Staged`, `Waiter.needs` and
`ResidentView`, and joins `RuntimeView` (`runtime/registry/state.rs:214`). `Target` stays the
model; a placement is a decision's answer, not part of the question.

### 4.2 The cost on one card

`weights + KV cache + compute`, as the estimate has it today, plus the engine's cost per card
(D5). The first three are per-model containers §4's and do not change: on one card WP0 found
them exact (§1.1). The fourth is a figure of the card (§1.1: 621 MiB on the Blackwell, 480 on an
Ada), learned per card after each start and, until a card has one, a stated default per
backend, 640 MiB for CUDA (ruling 17). It is kept beside the headroom readings and shown in
Settings → GPU. This design moves it from "what the headroom is for" to "what the model costs".

### 4.3 The cost of a split (chat only, D10)

llama-server splits by layers (`--split-mode layer`, its default: "split layers and KV across
GPUs"). A placement over several cards assigns each card a run of whole layers; a card's bytes
are those layers' weights and KV cache, its share of the compute buffers, and the engine's cost
per card.

- The weights per layer are derivable and not computed today: `TensorList.tensors`
  (`gguf/tensor_list.rs:15`) has every tensor's bytes, `layer_of` (`vram/plan/chat/offload.rs:365`)
  its layer. The KV cache per layer follows the layer's kind (`vram/compute/layers.rs`, `Layout`).
- The output layer is the last card's (WP0, §1.1); the token embeddings stay in host memory.
- The compute buffers of a split are larger than one card's, because llama.cpp runs a split as a
  pipeline (`sched copies = 4`): the first card's was 2.2 times the one-card figure and every
  other card's 1.7 times, for the 4B (§1.1). WP6 models them from the graph llama.cpp reserves,
  or, where it cannot, charges a split start by measurement after it loads (below).
- The split is chosen by filling: the cards are taken in the order of §5.4, each but the last
  filled to its room, the rest on the last. Why filling and not proportions: what is left over
  stays on one card, in one block.

What llama.cpp places by rules of its own, and what this version does about each:
- **The projector** (`--mmproj`) goes on the engine's main device: charged to the first card.
- **The projector** goes with a compute buffer of its own and raises the first card's model
  compute buffer (§1.1: about 800 MiB of weights, 322 MiB of its own compute, the model's
  compute from 205 to 302 MiB).
- **A draft or MTP model** is spread by llama.cpp's default over every card it sees, not by
  `--tensor-split` (§1.1). lmgw confines it to the first card with `--spec-draft-device`
  (`--device-draft`, `-devd`), which WP0 ran, and charges it there. A drafter is held only with
  a speculative type set (`--spec-type`, default `none`); without one it is loaded and freed.
- **`--n-cpu-moe`** keeps the first layers' expert tensors in host memory: those layers' weights
  on the first card are less by them, as `offload::place` already computes for one card.
- **`--moe-cache-mib`** goes to the first card alone (§1.1), not "split among them like the
  layers" as `--help` says: it caches the host experts, which are the first layers'. Charged to
  the first card.
- **The output layer** counts as a layer of its own on the last card: llama.cpp's layer count is
  `n_layer + 1`, and the split's entries sum to that (§1.1).
- The split has to survive `LlamaPlacement::only` (`llama_fit.rs:66-72`) and the `!is_whole()`
  gate (`podman.rs:116`, `reconcile.rs:569-573`), or adoption renders the command without it
  and removes the container.

WP0 measured every one of these (§1.1). The weights and the KV cache per card are predicted
exactly; the compute buffers of a split and the projector's are not modelled yet, and are WP6's.
Where WP6 cannot predict them, a split start is charged by measurement after it loads (the probe
says what the process holds per card), with its estimate reserved until then, and this section
says so.

## 5. Choosing the placement

One function, `place`, answers for a target, its footprint, a mode (§5.6) and one ledger read.
Every "does it fit" of today (`queue.rs:552`, `background.rs:467`, `climb.rs:971`, `verdict_for`,
the preview in `footprint()`, the warm groups, the realtime budget, the finder's fit marks) asks
it.

### 5.1 The candidates

The cards the model may use: its pin (§8), else every NVIDIA card (D18). A card whose telemetry
is missing is not a candidate. An AMD APU, whose total is host memory (`vram/amdgpu.rs:179-196`),
is a candidate only when the row is pinned to it.

### 5.2 The order (D6)

"Whole" is the row as written: its set context, or its trained one where the row sets none and
`--fit` is off. A row that adapts (no set context, `--fit` on) has no whole: §5.3.

1. **One card, now.** The candidates with room for the whole model and what it must leave free
   there (§5.6). Of those, the one with the least free left afterwards (D7); equal, the lower
   PCI address.
2. **One card, after evicting** (ruling 5). The candidates where stopping idle models of that
   card makes room (§6). Of those: the fewest victims; then the ones whose victims can all be
   started again on another card now (§6.2); then the victims used longest ago; then D7.
3. **Several cards, now** (chat only). The fewest cards that hold it (§5.4).
4. **Several cards, after evicting.** As 3, with idle models of those cards stopped.
5. **Waiting for a busy model** (ruling 14 puts it here). A card, or the fewest cards, that would hold it once
   models of lmgw's that are serving a request have finished and left: the start commits to
   them (D15) and waits, up to the queue timeout, as `decide` waits today (`queue.rs:643-651`).
6. **Smaller.** Nothing of lmgw's is left to wait for and no fallback applies (the owner's
   ruling of 2026-11-11, `vram/partial/llama.rs:20-23`): the candidate with the most room, and
   the row fitted to it (`fit_to_card`: a lowered context, layers off the card), with that
   card's own total and free and a count of one (D14). A row that cannot be fitted waits or
   takes its fallback, as today.

A step is taken only when every step before it has no answer. That the splits of steps 3 and 4
stand before the wait of step 5 is ruling 14.

### 5.3 A row that adapts (ruling 15)

Most rows set no context. That means the model's own, its trained context, and on one card it
is what the row gets where it fits; where it does not, llama.cpp's `--fit` and lmgw's `Sizer`
take the largest that does (`queue.rs:393`, `:548`; `check_background_start`, `queue.rs:319`).
Asked for the whole of such a row, steps 3 and 4 would spread a small model over two cards for
the sake of its trained context: the 4B model of §1 at 262144, 24.4 GiB, over cards 2 and 3.

So a row that sets no context is never split. It is placed by steps 1 and 2 at the model's own
context where a card holds that, and else fitted to the candidate with the most room, as step 6
does but without waiting for anything first: for it, smaller on one card outranks whole over
several. `modelinfo::refit::for_get` ("Get it", which writes the fitted context into the row)
and the finder's fit marks answer the same way: one card, the roomiest candidate.

The owner agreed to this on the condition that the editor says it, where the context is set
(§8.2). A row that should have its full context over several cards sets that context.

### 5.4 The fewest cards

The candidates by room, most first; cards are added until their room, less the engine's cost on
each, holds the model. That is the set with the fewest cards; among sets of equal size the one
chosen this way has the most room, and D7 does not apply (a split has no tight fit to prefer).

### 5.5 Waiting (D15)

A start that `place` answers with step 2, 4 or 5 does not start yet. It gets a claim: its cards,
its bytes on each, and the models that must leave. Under the gate it stops one idle victim at a
time, as `evict_one` does, and measures again; it does not ask `place` for other cards while it
holds the claim. While it waits for a busy model, or for a stopped one's memory to be released,
it gives the gate back.

Other decisions see a claimed card with the claim's bytes already taken once its victims are
gone: a start that fits beside the claim goes ahead, one that does not looks at the other cards.
Claims on a card are served in the order they were made. A claim ends when its start begins,
when the queue timeout runs out, or when its request goes away, and what it held is free again.

### 5.6 Modes (D16)

What counts as room, and who may be stopped, depends on who asks, as it does today:
- **An owner's start**: free memory, and idle models of any origin as victims.
- **A guest's** (background traffic): free memory, and only idle guests as victims
  (`queue.rs:791`, `Victims::IdleBackground`).
- **A climb** (ladder): the running rung's own charge counts as room on its cards
  (`climb.rs:971-976`, "free + running" today); the new rung is placed on the same card or cards
  where it fits there, and by §5.2 where it does not. "Larger than the card" (`climb.rs:365`)
  is §5.8's test.
- **The outside verdict** (`admission.rs:109`, `external_table`): all of lmgw's share on the
  candidate cards counts as room, busy models included, per card.

What a start leaves free on a card is today's rule per card: the learned headroom while anything
of lmgw's is left on that card to make room with, the margin alone once nothing is
(`queue.rs:1053-1068`, `room_now`, `others_hold`).

### 5.7 Engines that cannot be handed a card (D18)

An AMD card, a Vulkan or HIP build, a native start whose device order is not mapped (Windows
§4.1), and a start that no decision stands behind (`Decided::Go(None)`: admission inactive,
`queue.rs:388`; telemetry gone mid-wait, `:531`; a warm start, `Fit::Unchecked`, `queue.rs:298`):
the engine sees every card its arguments pass, and lmgw cannot say which it will use.

Such a model is placed on its cards as a group: it fits when their summed room holds it, it is
reserved against the sum, and once it runs each card is charged what its processes hold there.
That is today's pooled behaviour, kept for these cases and named as such on the entry. A machine
with AMD cards only therefore behaves as it does today.

A pin applies even without a decision: a pinned row starts on its pin's cards.

### 5.8 Too large for the machine

`VramTooLarge` is refused when the model could not run on the candidates even with nothing else
on them and the row does not adapt (`queue.rs`, `arbitrate`). For a chat model that is the sum
over the candidates' totals, per §5.4; for an aux, audio or image model, which goes on one card
(ruling 12), it is the largest single candidate.

## 6. Eviction and repacking

### 6.1 Per card

A victim is an idle `Ready` entry with bytes on the card that is short, within the asker's mode
(§5.6). The order among them is today's: least recently used first, asked for busy slots before
it is stopped (`queue.rs:752`). A model split over several cards is a victim on each of them,
and stopping it frees all of them. `freeable` becomes per card.

### 6.2 Repacking (ruling 6, D8, D21)

A victim that fits on another card now, without evicting anything there, is started again there
once it has stopped. The request that needed the room does not wait for it; it starts as soon as
the victim has stopped. A victim that fits nowhere else stays stopped, as an eviction leaves it
today.

- **The start** is the path a warm or boot start takes (`check_background_start`,
  `queue.rs:235`), which waits for the gate, and keeps the model's origin. It is not the guest
  path of `vram/background.rs`, which gives up when the gate is taken and would make the moved
  model a guest's.
- **Its age moves with it.** A start sets `last_used` to now (`registry/acquire.rs:217`); a
  moved model keeps the `last_used` it had, so it stays where it was in the eviction order and
  its idle timeout goes on running.
- **One card.** A moved model is placed by step 1 only: it goes where a card holds it now and never
  evicts anything for its move (owner, 2026-11-17). It is never split to be kept,
  and a split model that is stopped for room comes back on one card where one holds it.
- **Not again before it has served.** A moved model is not moved a second time until a request
  has used it; until then it is an ordinary victim, stopped and not started again. Why this and
  not a time: `idle_seconds = 0` means "never reap" and is the warm default, and an audio row
  always reads 0 (`runtime/lifecycle.rs:589-594`). Without the rule two models that each need
  the other's card would swap for ever.
- Only idle models move (a victim is idle), and a pinned one only among its pin's cards.
- The move is logged and shown on the entry ("moved from card 0 to card 3 to make room for …").
- A moved model's load reads the disk beside the requester's. Nothing orders the two.

There is no pass that repacks without a request waiting. Why: it would reload models nobody
asked for. "Not yet" has it.

## 7. Telling the engine (D9)

### 7.1 The container's cards

Where the placement is fewer cards than the row's arguments pass (D17), the NVIDIA device
arguments are replaced at render by one `--device nvidia.com/gpu=<uuid>` per card of the
placement; every other run argument stays as written. `strip_gpu_devices`
(`lmgw-api-types/src/audio_engine.rs:15-30`) knows what an NVIDIA argument is; `/dev/dri` and
`/dev/kfd` are not NVIDIA's and are never replaced (D18).

The container also gets:
- `-e NVIDIA_VISIBLE_DEVICES=<uuid,…>`. CUDA images set that variable to `all` themselves, and
  on a host with NVIDIA's legacy hook that alone gives a container every card
  (`audio_engine.rs:6-13`, the reason a CPU row adds `=void`). Without this the engine there
  sees four cards while the ledger charges one.
- `-e CUDA_DEVICE_ORDER=PCI_BUS_ID` (§2.2).

**The replacement is render-only.** `ModelRuntime.extra_run_args` stays the row's own: the
`lmgw.run_args` digest, the image model's `engine_key` (`descriptor.rs:155-158`,
`plan/image.rs:274-281`; it keys the learned peak, idle and work figures) and the benchmark's
settings hash are taken from the arguments as written, so a model that moves card is neither
"reconfigured" nor forgotten. Adoption compares the container's command and that digest
(`launcher/podman/listing.rs:132-161`), not Podman's device arguments; `lmgw.placement` (§3.3)
is what says which cards a running container has.

`passes_gpu` and the machine check keep reading the row's arguments for "does this row use a
card at all". A CPU row (`runtime::Placement::Cpu`, decided by the backend, `on_cpu.rs`) is
never given a card and its arguments are used as written.

**The copyable command line and the editor's "Rendered command line"** (`podman.rs:70-83`) have
no decision behind them. They show the arguments as written and, where lmgw would choose, a line
under them: "lmgw picks the card when it starts this model; now it would be card 2."

**The machine check** verifies today only that a spec defines `nvidia.com/gpu=all`
(`machine/toolkit.rs:188-199`). On a machine with several cards it also verifies that the spec
names every card by UUID, since every placed start depends on it.

A native start (Windows §4.1) gets `CUDA_VISIBLE_DEVICES` with the placement's cards. Whether
CUDA takes UUIDs there as it does on Linux is to verify; a row's own `-e` wins over lmgw's
variables natively today (`launcher/native/gpu.rs:25-27`), and §8.1 says what becomes of it.

### 7.2 The split

A chat model on several cards is started with `--split-mode layer`, `--tensor-split` with the
layers per card in the cards' order, `--n-gpu-layers` as the row or the fit has it, and `--fit
off`. `LlamaPlacement` gains the split; `lmgw.llama-fit` records it. On one card none of these is
passed.

`--tensor-split`, `--split-mode`, `--main-gpu` and llama-server's `--device` become flags lmgw
knows (D20): a list of their own, asked of the image's `--help`. They do not join
`MANAGED_FLAGS`, whose members are rendered from fields of the row, refused as extra arguments
("has a dedicated field … passed as an extra arg it is ignored",
`ops/local_model_checks.rs:84-93`) and required of every image (`llama_caps.rs:1290`). An engine
build without `--tensor-split` gets no split placement, and the start says why.

A row that sets `--tensor-split` itself (ruling 11) is held to it as written, as a row that sets
its context is: it must be pinned to as many cards as the split has entries, the entries are the
pin's cards in PCI order, and each card is charged by the row's split in place of §4.3's filling.
Without a pin, or with a pin of another size, the row's editor says that the split needs a pin
of that many cards; the row is then placed by §5 and started without its split, and the start's
warnings say so. `--main-gpu` and `--split-mode` in a row's arguments are kept as written beside
its own split and dropped with it. Ruling 11 is a chat row's: an aux row's own split is dropped
with a warning that says aux models run on one card, and the row is placed by §5 (ruling 16).

## 8. The pin (ruling 8)

A nullable column `gpu_pin` on `local_models`, `aux_models`, `audio_models` and `image_models`:
a JSON list of `DeviceId`. NULL: lmgw chooses. One card: the model runs there or not at all. Several:
the model may use those cards, by §5 among them.

- **In the editor** a pin is a choice of cards by name and index, with each card's room beside
  it. The field says what it is for: "Run this model on a card of your choosing. Empty: lmgw
  picks the card."
- **A pinned card that is not there** (removed, failed, its id unknown): the start does not go to
  another card. It says which card is missing, and the row's fallback applies.
- **Run arguments that name a card** are read as a pin and keep working (§8.1 has every form).
  The editor shows it as one and offers to move it into the field. Arguments that pass every
  card (`=all`, `--gpus all`) are no pin.
- **The op** (`local_model_set` and its siblings) takes `gpu_pin`; the API reference and the MCP
  tool descriptions follow.

The pin's migration is 0111: 0107 is main's (`chat_message_notices`), 0108 the branch's
`request_via_tls`, 0109 WP3's (`vram_outside_hours_per_card`, §9.1), 0110 the engine's cost per
card (§4.2), built before the pin in WP5. (Planned as 0109; renumbered at WP5 and again when
main was merged on 2026-11-16, see the WP5 note.)

### 8.1 What names a card today (D19)

"The only way to pin until now" was wrong. Each form and what becomes of it:

| Where | Form | Fate |
|---|---|---|
| a row's run arguments | `--device nvidia.com/gpu=<index or UUID>`, `--gpus device=<n>`, `--device /dev/nvidia<n>`, `-e NVIDIA_VISIBLE_DEVICES=<n or UUID>` | read as a pin of those cards; kept working; the editor offers the field |
| a class's run arguments | the same | a pin for every row of the class that has none; Settings shows it as one and offers to clear it |
| a row's run arguments | `-e CUDA_VISIBLE_DEVICES=<n>` | CUDA's own numbering, which differs from every other on unlike cards (§2.2). Dropped at render where lmgw chooses the cards, with a warning that names the pin field; kept where the arguments are left as written (D17) |
| a chat or aux row's arguments | llama-server's `-dev CUDA<n>`, `--main-gpu <n>`, `-sm none`, and the draft model's `-devd`/`--device-draft`/`--spec-draft-device` (owner, 2026-11-17) | numbers among the cards the engine sees, wrong once lmgw passes fewer. Dropped with the same warning, except beside a row's own split on a pin (ruling 11). They are not translated: the number a row wrote is CUDA's own, whose order differs from the driver's on unlike cards (§2.2), so lmgw cannot tell which card it meant; the card is named by UUID in the pin instead, and lmgw writes the engine's own numbers for the cards it hands (a split's `-devd CUDA0`) |
| the audio class | `AudioSettings.device` (`config/settings_classes.rs:355`), written into `server.json` (`runtime/audio.rs:149`) | with one card passed the engine's device is 0. A non-zero value is read once as a pin of that card for the audio rows that have none, the setting goes to 0, and a handed container's `server.json` says 0. The setting itself stays (owner, 2026-11-17): a Vulkan, HIP or Metal build sees every card and lmgw cannot hand it one ("Not yet"), so there the index is the only way to choose the card; it is read as a pin only where the class hands NVIDIA cards through CDI and every audio row can be handed one, and is left as it is otherwise. Settings says so beside it. Adoption compares `server.json` (`registry/reconcile.rs:516-524`), so an audio container started with another value is restarted once |
| any | `nvidia.com/gpu=all`, `--gpus all` | no pin |

Nothing is rewritten in the stored rows by a migration: the forms are read where they stand.

### 8.2 The context field's notice (ruling 15)

In a chat model's editor, at the context field, while the field is empty and the machine has
more than one card the row may use (its pin's, else all):

> No context set: the model's own is used (262,144 tokens). lmgw keeps this model on one card.
> Where that context does not fit one card, the largest that fits one card is used. Now that
> would be 180,000 tokens on card 2. To spread the model over several cards for more, set a
> context.

The figures are the row's: the model's own context from the file, and the context and card
`place` would answer now (§5.3), read when the editor opens and when the pin changes. Where the
model's own context fits a card the last two sentences give way to "It fits on card 2." On a
machine with one card the notice is not shown: nothing differs from today there. It is a notice
in the field's own help line, not a dialog, and the same sentence is the warning of a start
that lowered the context for this reason ("Context fitted to the GPU: …", `partial/llama.rs`,
`CONTEXT_FITTED`).

## 9. Headroom and a declared budget

### 9.1 Headroom (ruling 10)

Headroom today is one figure for the machine: fixed, or the largest rise of the outside use over
seven days (per-model containers §4, "Headroom").

It becomes per card. The outside use is known per card (§2.3); `vram_outside_hours` gains the
device (its key becomes the hour and the device); `headroom`, `margin`, `start_headroom` and
`card_headroom` (`vram/headroom.rs:124`, `:144`, `:154`, `:254`) answer for a card, and `place`
asks them for each candidate. A card nobody else uses learns none. In fixed mode `headroom_mb` is
kept free on every card.

Why, against ruling 9 as first stated: a four-card test server is not lmgw's alone. Other programs
take memory on card 2 when they are asked, which is the case headroom exists for, and a
card lmgw fills to the last MiB is one of theirs that fails. On a server that is lmgw's alone the
learned figure is zero.

With D5 the engine's own cost per card leaves the headroom, so headroom is only ever "room for
others". Two figures of today hold that cost and are corrected by what WP0 measured, or it is
kept free twice: `LLAMA_MARGIN` (807 MiB, which includes the CUDA context) and, in fixed mode,
`headroom_mb` itself, which also stands in for the start's slack (`headroom.rs:69-72`). WP0
(§1.1): the CUDA context and the rest of the engine's own memory is 621 MiB on the Blackwell and
480 on an Ada, so `LLAMA_MARGIN` keeps 186–327 MiB a card more than the engine takes, and nothing
more for each further card of a split. With D5 the engine's cost is charged per card and
`LLAMA_MARGIN` becomes the start's slack alone (WP5).

What the change costs, said here so nobody finds it later:
- `vram_outside_hours` has no device (0106). The readings of an install cannot be told apart
  by card, so they are dropped at the migration, and every install is on the fixed `headroom_mb`
  until it has watched its cards for `HOURS_NEEDED` (24) hours again.
- In fixed mode `headroom_mb` is kept free on every card: four times as much on four cards.
  Settings says the figure is per card.

**Resetting what was learned** (ruling 10). A learned value can be wrong for good reasons: a
one-off job of another program, a card that changed hands. `headroom_forget` exists today and
forgets everything (`web/api.rs:~990`; `openapi/ops/table/runtime.rs:48`). It gains an optional
card: with one, only that card's readings go, and that card is back on the fixed `headroom_mb`
until it has been watched for `HOURS_NEEDED` hours again (`headroom.rs:100`); without one, every
card's. Settings → GPU shows each card's learned figure with the hours it rests on and the rise
that set it, and "Forget" beside it; "Forget all" stays. The reset is in the audit log with the
card and the figure that was dropped.

**A reading every second** (the owner, 2026-11-19). Outside use was read every 30 s
(`headroom::SAMPLE_INTERVAL`). A 30 h log of a four-card test server at one reading a second
(nvidia-smi, 2026-11-17 to 2026-11-18) showed:
- **No llama.cpp process grew while it ran.** The other programs' three llama-servers did not move by
  one MiB in 30 h; lmgw's took their memory while loading and kept it flat until they stopped.
  So there is no growth to yield to, and moves still never take learned room (decision 7).
- **One program moved:** an embedding server on card 3. It
  sat at 15.0 GiB and rose 97 times, mostly for 1 to 10 s, up to +5.2 GiB (99 % of the rises
  under +2.1 GiB).

Replayed at other intervals, the largest rise a reading would have caught depends on where the
readings fall:

| Read every | Worst | Typical | Best |
|---|---|---|---|
| 2–3 s | 5.2 GiB | 5.2 GiB | 5.2 GiB |
| 5 s | 4.0 GiB | 5.2 GiB | 5.2 GiB |
| 10 s | 2.5 GiB | 4.0 GiB | 5.2 GiB |
| 30 s | 1.3 GiB | 3.3 GiB | 5.2 GiB |

The live suggestion for card 3 was "always keep 3.3 GiB free", the typical 30 s case. Too low a
learned room means a burst can run the other program out of memory, or fail an lmgw model
loading at that moment; lmgw's running models are safe, as llama.cpp allocates nothing after
loading.

Built: outside use is read every second. A reading is the driver's per-process and device
figures, the processes under lmgw's containers (their PIDs cached) and lmgw's ledger; no
process is started. The store is written only when a reading changes the card's hour (a new
hour, lowest, highest or rise), so it is not written every second. The 1 s log itself cannot
show a burst shorter than a second; the large ones held for several seconds.

### 9.2 A declared budget (D13, D22)

`budget_mb` is for a machine without telemetry, and for capping what the cards are used for
below what they have. Without telemetry there is one stated card of that size, with the id
`budget`, and everything above works on it. With telemetry it keeps today's meaning
(`ledger.rs:502-516`, `:949-951`): free is the budget less what is used on all cards, lmgw's and
the other programs' together. It is a second condition beside the cards' own room: a placement
must fit its cards, and the machine must stay within the budget. It is not divided among the
cards.

## 10. What shows it

- **`GET /api/vram` and the `vram` frame**: each device gains its id, lmgw's share, the outside
  use, what is kept free and its residents; each resident gains its placement. `capacity_bytes`,
  `free_bytes`, `lmgw_share_bytes` and `outside_share_bytes` stay, as sums.
- **The GPU pill and its panel** (`lmgw-ui/src/shell.rs:146`, `gpu_figures.rs`): one bar per
  card, each with lmgw's models on it, the outside use and the free part; a split model appears
  on each of its cards with its bytes there.
- **Settings → GPU**: the cards, the engine's cost per card, and each card's headroom with what
  it was learned from and its reset (§9.1).
- **A model's editor**: the pin (§8), and where the model is now.
- **Fit marks** (find a model, the model list): "fits on card 2", "fits over cards 0 and 1",
  "after unloading …", in place of a mark against the pool.
- **Messages** name cards by name and index and say which card was short.

## 11. Open questions

None open. Q1 to Q3 are rulings 10 to 12; the three the review opened (Q4, a busy model in the
way; Q5, a row with no set context; Q6, an aux row's own split) are rulings 14 to 16, all of
2026-11-13. Q7, which WP0 opened, is ruling 17, and the drafter finding below is ruling 18, both
of 2026-11-16. As they were put:

- **Q7 — The engine's cost on a card lmgw has not measured.** WP0 found it a figure of the card
  (621 MiB on the Blackwell, 480 on an Ada), not of the engine as D5 assumed. This record
  proposes: **learned per card**. After a start, the probe's figure for the model's process on
  each card less the buffers lmgw estimated there is that card's engine cost; it is kept per card
  (beside the headroom readings) and shown in Settings → GPU. Until a card has one, it is charged
  a stated default per backend, 640 MiB for CUDA, the largest WP0 saw rounded up. The other
  ways: one stated figure per backend for every card (simple; on the test server it charges an Ada
  160 MiB a card too much), or a table by card model (needs a row for every card that exists).

WP0 also found, outside the ledger: a row with a drafter and no speculative type set starts a
llama-server that loads the drafter and frees it again (`--spec-type` defaults to `none`), while
lmgw's estimate charges the drafter (§1.1). Whether the editor should require the type, or lmgw
set `draft-simple` for a drafter given without one, was open until ruling 18.

## 12. Tests

- **A shared fake machine with several cards**: cards with a total and an id, programs outside
  lmgw per card, and lmgw's containers holding what their placement says, on the cards the
  rendered UUIDs name. It is a builder the seven existing fakes are built on, not a replacement
  for them: each keeps what it does today (`vram_admission/world.rs:296-301` grows a process on
  every read and keeps lingering ones, `support/gpu_world.rs` records the run arguments, the
  two `ScriptedGpu`s play a script, `bench_fake.rs` and `machine_check/fixed.rs` have roles of
  their own). With D17 a one-card world's containers start with today's arguments, so the 57
  places under `lmgw-core/tests` and `lmgw-server/tests` that expect `nvidia.com/gpu=all` in a
  command line keep passing; the several-card tests state the UUIDs they expect.
- **Placement** as a pure function over a stated ledger: each step of §5.2 and each tie; a pin;
  a missing pinned card; the measured machine of §1 as a fixture (a 4 GiB model goes to card 0,
  not over four cards; a 24 GiB model with a set context goes over cards 2 and 3, nothing of
  lmgw's being in the way on card 2; the same model with no set context goes on card 2 alone,
  fitted to it).
- **Waiting**: a claim keeps its cards while the gate is free; a start that fits another card
  is not held up by it; one that fits beside the claim goes ahead and one that does not is sent
  elsewhere; a claim that times out frees what it held; the eviction for one start stays on the
  claimed card or cards.
- **Modes**: a guest stops only guests; a climb counts its own rung; the outside verdict counts
  busy models.
- **Forms that name a card** (§8.1), one test each, and a class-level pin.
- **The context field's notice** (§8.2): shown with several cards and no context, with the
  row's own figures; the "fits on card …" form; not shown with a set context or on one card;
  the start's warning uses the same sentence.
- **No decision**: admission off, a pinned row still starts on its pin.
- **Eviction per card**: the victim is on the short card; an idle model on another card is left
  alone; a split victim frees all its cards.
- **Repacking**: the victim starts again where it fits; not twice; a pinned one only within its
  pin.
- **The engine's arguments**: the placement's UUIDs in place of `=all`, the other run arguments
  untouched, the split flags only for several cards, the labels, and adoption from them.
- **Headroom per card**: a rise of the outside use on one card raises that card's headroom and
  no other's; forgetting one card leaves the others' readings; a card nobody else uses has none.
- **A row's own split**: pinned to as many cards, charged by it; unpinned, placed without it and
  the warning says so.
- **"Get it" and its engine image** (WP9): the image arrives after the files, and the row is
  created after the image, validated, without the warning; the pull fails, and the row is created
  with the pull's own sentence.
- **The pinned-row bug** as a regression test: a row pinned to a card with 7.5 GiB free on a
  machine with 57 GiB free in all is fitted to 7.5.
- **Live**, on a four-card test server: §13's verifications.

## 13. Work packages

In build order, strictly in sequence: they share `vram/ledger.rs`, `queue.rs` and the test world.
Each ends with the project's checks (`ci/check.sh`). The order was changed by the review: the
headroom per card now stands before the placing, and the one-card placing keeps today's
behaviour for a model no card holds until splits are built.

- **WP0 — Measure.** A script in `scripts/` against a running lmgw on a machine with several
  cards, in the manner of `scripts/llama-fit-check.py`. It measures: the engine's cost per card
  (one model on one, two and three cards with a split it sets through the row's arguments); what
  lands on each card for a stated `--tensor-split`, against §4.3's formula, and whether its
  entries are layer counts that sum to `n_layer + 1`; where the compute buffers, the output
  layer, a projector, a drafter and a MoE cache go; the engine's device order with cards passed
  by UUID, with and without `CUDA_DEVICE_ORDER`; that a start with a UUID device runs, with and
  without `NVIDIA_VISIBLE_DEVICES`. It writes the numbers into §1 and corrects §4.3, D5 and the
  margins of §9.1. On a shared test server it stays within what is free and never starts a model
  that could run a card out. Depends on nothing; changes no behaviour.
  *Prepared 2026-11-14, not yet run.* `scripts/multi-card-measure.py` (its header has the
  details; offline test `scripts/multi_card_measure_test.py`, in `ci/check.sh`). On the test server,
  as a user who may run `docker`, with an owner key in `LMGW_TOKEN`: `python3 -I
  multi-card-measure.py --base https://lmgw.example:8443` prints the plan and starts
  nothing; `--run` measures. The cases: `order` (`--list-devices` through the inner Podman with
  the cards by UUID in reverse PCI order, without and with `CUDA_DEVICE_ORDER`), `uuid` and
  `uuid-nvd` (one card by UUID, without and with `NVIDIA_VISIBLE_DEVICES`), `split2` and
  `split3` (layer counts summing to `n_layer + 1`, cards by UUID, §7.1's environment). Each
  case's rows are `wp0-<case>`, removed at the end and on Ctrl-C; a case starts only when every
  card it uses has its generous plan plus 4 GiB free now, and a watchdog stops it when a card
  falls below 1.5 GiB. Per card it reports the process's memory (`nvidia-smi` on the host),
  lmgw's estimate and charge, the log's model, KV and compute buffers and layer assignment
  (rows run with `-v`), the overhead (process less buffers: D5's cost per card) and §4.3's
  prediction from the GGUF's tensor sizes. **Not measured without files the owner provides
  under the chat models dir:** the projector's card (`mmproj`: a vision model and its projector,
  `--vl-file`, `--mmproj`), the drafter's placement with and without its device flag (`draft`,
  `draft-dev`: `--draft`, a drafter of the 4B such as Qwen3-0.6B), and `--n-cpu-moe` and the MoE
  cache (`moe-cpu`, `moe-cache`: `--moe-file`, a small MoE GGUF; the cache needs an image that
  knows `--moe-cache-mib`). The log lines it parses are llama.cpp's as remembered, not read
  from build 12352: a format that moved shows as dashes in the table, and the raw lines are in
  the report.
  *Run 2026-11-16* on the test server, all ten cases, nothing skipped, every `wp0-*` row removed:
  §1.1 has the numbers, and §4.2, §4.3, D5 and §9.1 are corrected by them. The script was
  changed twice for the run: cards of an lmgw without card ids are matched to `nvidia-smi` by
  index, and the drafter cases set `spec_type` (without it the drafter is freed at load, and
  the first run measured nothing of it). The deployed lmgw was 0.7.0, so the "lmgw's reading"
  and "charged" columns are empty.
- **WP1 — Cards have an id, and the probe reports per card.** D1, D3, §2. `GpuMemory.id`,
  per-device process memory in the three probes, the shared builder of §12 under the existing
  fakes. The pooled figures are still what decides; they are computed from the per-device data.
  No behaviour changes. *Built 2026-11-13.* `DeviceId` is in `lmgw-api-types` (`status.rs`,
  beside `VramDevice`, which gains `id`); `GpuMemory.id` is NVML's UUID, the AMD card's PCI
  address, and the Windows probe's NVML one. Of §2.3's two forms the answer is a list per card:
  `GpuProbe::processes` returns `DeviceProcesses{device, processes}` and `ProcessMemory` stays
  `{pid, bytes}`. `sum_devices` is gone; `members_share` and `PidCache::settle` answer a
  `CardHeld` per card, whose figure is unknown on its own; `Measured` holds `CardShares{id,
  used, lmgw}` per card and `Measured::shares()` is the sum the decisions read. The shared
  builder is `tests/it/support/fake_machine.rs` (`Machine`); the two `FakeGpu`s, both
  `ScriptedGpu`s, `plan_hub`'s `Card` and `machine_check/fixed.rs`'s `Probe` answer through it,
  `bench_fake.rs` wraps as before. Readings of this bullet against the code: a measurement with
  one card unknown is still unavailable as a whole, as the pooled figure was (what a view shows
  for that card is WP2's); the two process readings around the device read are merged per card
  and PID, so a process whose memory moved between cards between the two readings counts at the
  larger figure on each card, which can be more than the larger of its two sums was (the
  direction that never reads lmgw's memory as outside use); an NVML device whose UUID cannot be
  read fails the device read, as one whose memory cannot be read does (NVML answers it for every
  product); an AMD card whose PCI address cannot be read is known by the kernel's name for it
  (`card1`), and no process is attributed to it, as before; the Windows probe reads each process
  once per card, on that card's adapter (one read on one card, as before), since the D3DKMT
  `Kernel` was left as it is; `NvmlProbe::pci_addresses` became `device_addresses` (id and
  address).
- **WP2 — The ledger per card, shown.** §3.1, §10's API fields and the per-card bars. Decisions
  still read the sums. **Verification:** the test server's four cards with their own outside use in
  the panel. *Built 2026-11-13.* The ledger's card is `CardCapacity` (`vram/ledger.rs`):
  the driver's figures, lmgw's share there and the outside use there from a measurement, and
  what is kept free there; `Ledger::cards` builds them from the device read and a measurement.
  Readings of this bullet and of §3.1 and §10 against the code:
  - **Kept free per card is shown only on a machine of one card**, where all of it is on that
    card. A start in flight, an image peak and a pending load are one figure each until WP4
    charges them per card, and dividing them among the cards would be a guessed limit. On
    several cards a device's `kept_free` is absent, the top-level `kept_free` is all cards'
    together, and the GPU panel says "(all cards together)" after its kept-free line. So no
    per-card free figure net of what is kept free exists yet; a card shows the driver's free.
  - **The pooled `Ledger.capacity` stays** as the sum the decisions read (§3.1's "the field
    goes" is WP4/WP5's, with its readers); `CardCapacity` is shown and decides nothing.
  - **lmgw's share per card stays in the measurement** (`Measured.cards`), taken only while
    the outside trigger is armed and boot has settled, as the pooled share was: measuring on
    every ledger read would add a `podman inspect` and a process walk to every decision. With
    the trigger off a card shows its used memory and no share, as the machine does today.
  - **WP1's deviation 1 is resolved for the view**: a card whose share is unknown is unknown
    alone (`CardShares.lmgw` is a `Result`, the device's `share_unknown` says why), the other
    cards keep their shares and models, and `Measured::shares` — the sums the decisions read —
    is unknown with it, with the first such card's reason, as before. One consequence:
    `PidCache::settle` now also runs on a pass where a member's card is unknown (it ran only
    once every member card was known); it drops only tombstones the pass found gone.
  - **A resident's cards** are what the measurement read of its container's processes per
    card (`attribution::members_on_cards`), since charges per card are WP4: `cards` on the
    resident, `resident` on the device, each with its bytes there (`null` where the driver
    lists a process there without a figure). Empty where no measurement was taken, and for a
    reservation. §10's "each resident gains its placement" is this until WP4.
  - **The API** (`lmgw-api-types/src/status.rs`): `VramDevice` gains `lmgw_share_bytes`,
    `outside_share_bytes`, `share_unknown`, `kept_free` and `resident`
    (`VramDeviceResident`); `VramResident` gains `cards` (`VramResidentCard`). Additive only.
    A card's outside use is its used memory less lmgw's share there, never below zero; the
    top-level outside use stays the sums' difference, so the cards' figures can add up to a
    little more where a process reading saw more of lmgw's than the device reading did.
  - **The GPU pill's panel** (`lmgw-ui/src/shell.rs`, `GpuCards`; `gpu_figures.rs`) shows the
    cards one by one only on a machine with several: each card's name and index ("RTX PRO 6000
    Blackwell (card 2)", the id in its tooltip), the driver's free of its total, a bar of
    lmgw's models in their hue with their bytes there, the rest of lmgw's share, the outside
    use and the driver's reserve (the track is the free part), and the same in words. The
    header names "4 graphics cards" and keeps the machine's sums. On one card the panel is as
    it was. The Traffic page's GPU panel keeps its one bar of the sums.
- **WP3 — Headroom per card.** §9.1: the outside use and the learned rise per card, the
  migration (which drops the pooled readings), the reset per card (op, Settings, audit line).
  Decisions still read sums: the sum of the cards' headrooms where they read one headroom today.
  This changes a figure decisions read, on machines with several cards only: said in the
  release notes. **Verification** on the test server: card 2's learned figure follows the other programs on it, the other cards' stay near zero, and "Forget" on one card leaves the rest.
  *Built 2026-11-13.* Migration 0109 (`vram_outside_hours_per_card`; 0108 when built) drops 0106's table and
  makes it anew keyed by the hour and the card (`device`, the card's `DeviceId`); the store
  (`store/vram_outside.rs`) reads, writes and deletes per card. The record is
  `headroom::growth::Record`: each card's `Growth` (0106's hours, unchanged) and the cards the
  last device read listed, which the ledger sets beside `device_count`. The sampler reads each
  card's outside use from the measurement's `CardShares`. `VramScheduler::cards_headroom` is
  each card's part; `headroom`, `margin`, `start_headroom`, `start_margin`, `card_headroom`,
  `outside_at_rest` and `outside_recent` answer with the cards' sums, which is what the
  decisions read, unchanged in their callers. `headroom_forget` takes `card`
  (`HeadroomForgetArgs`); the audit line gets the card and what was dropped through a new
  response extension, `audit::Detail`, which the gate's writer merges into the entry, and
  `card` is an identity argument (a refused forget names the card asked for). Settings → GPU:
  "Fixed headroom per card"; on several cards each card by name and index with its sentence
  (what is kept free, the rise, its hours and hour) and "Forget…" beside it, and "Forget all
  learned growth…"; on one card the panel is as it was. Readings of this bullet and of §9.1
  against the code:
  - **The margin rides once, and only while every card is on the learned value.** It is a
    model's, not a card's; a card still on the fixed value keeps `headroom_mb`, which stands in
    for the start's slack as in `fixed` mode (`headroom.rs`, module doc "Per card"). So
    `start_margin` is the fixed value of each card still on it plus the margin once all are
    learned, and `card_headroom` is that plus, on each learned card, what other programs hold
    there at rest. On one card every figure is what it was.
  - **Which cards.** The cards the last device read listed; before any read, the cards with
    readings; with neither (no telemetry, a declared budget without it), one figure, the fixed
    one, as before. A card with readings that is no longer listed is not counted; its hours age
    out of the window, "Forget all" and a forget by its id remove them.
  - **A card whose share is unknown** this pass (§2.3) is not read; the others are. The pooled
    reading was skipped whole.
  - **`HeadroomNow`'s top-level figures are the sums** (`bytes`, `learned_bytes`,
    `at_rest_bytes`), `learned_in_use` is "every card", `hours_seen` is the card watched least
    and `learned_at` the hour of the largest rise of one card; `cards` lists each card
    (`HeadroomCard`). The growth-let-go warning on several cards says the growth is the cards'
    together and names the card that rose most, by name and index.
  - **D5 is not done here.** `LLAMA_MARGIN` (807 MiB, which includes the CUDA context) and the
    fixed `headroom_mb` (which also stands in for the start's slack) are unchanged: what the
    engine costs per card is measured in WP0, and the correction of both waits for it. Until
    then, in `fixed` mode on several cards that cost is kept once per card inside
    `headroom_mb`.
  - **Release notes** are the release tag's message (`scripts/release.sh`); the tree keeps no
    file of them. The line for the next tag: "**Headroom per card.** Auto headroom is learned
    per graphics card, and the fixed headroom is kept free on each card. On a machine with
    several cards admission now keeps the sum of the cards' headrooms free where it kept one
    figure for the machine: in fixed mode the fixed value times the number of cards. The
    readings learned so far could not be told apart by card and are dropped at the update, so
    every install uses the fixed headroom until it has watched its cards for a day again.
    **Settings → GPU** shows each card's learned figure with a Forget beside it. A machine with
    one card keeps what it had, apart from the day of learning again."
- **WP4 — Charges per card.** D4, D23, §3.2, §3.3, §4.1: `CardCharge` through reservations,
  grants, entries, waiters and labels. Every model is charged as a group on the cards it sees
  (§5.7), which is today's behaviour written in the new type: decisions compare the same sums.
  The engine's cost per card (§4.2) is not charged yet. No decision changes.
  *Built 2026-11-13.* `CardCharge` is `vram/charge.rs`. Readings of this bullet and of D4, §3.2,
  §3.3, §4.1 and §5.7 against the code:
  - **A charge is a list of parts, each the bytes on a list of cards** (`ChargePart{cards,
    bytes}`). A part of one card is the design's `(DeviceId, bytes)`. A part of several cards is
    a **group**: the bytes are on those cards together, and no card is charged a share of them,
    since a share would be a guessed limit (ruling 4). A part with no card is a charge whose
    cards no device read has named (a declared budget without telemetry). The CPU, and a
    container charged nowhere, have the empty charge. `total()` is what the decisions read; it
    is the figure they read before, so they compare the same sums.
  - **The cards a model sees** (`charge::seen`) are read from the run arguments it starts with,
    through the native start's `selection`: an NVIDIA name (`nvidia.com/gpu=<index or UUID>`,
    `--gpus device=<n>`) picks that card of the last device read; every card where the
    arguments pass every card or a GPU node, where a name is no card of the read (a MIG device),
    for a native Vulkan build, and where the arguments name no card at all (an image's own
    `NVIDIA_VISIBLE_DEVICES=all` gives such a container every card on a host with NVIDIA's
    legacy hook). The order is the device read's: NVML's index, which enumerates by PCI bus.
    One card seen is a part of one card, so on a machine of one card every charge is per card.
  - **Where it went.** `Reservation.charge`, `Waiter.needs` (the view's `needs_bytes` is its
    total), `Staged.charge` → the registry entry's `card_charge` (on `RuntimeView`, not
    published; a climb's new container keeps it), and `ResidentView.charge`, published as
    `VramResident.charge` (additive; `estimated_bytes` stays, as its total). **`Grant` and
    `PartialGrant` keep their byte figures**: they are what the engine is told and what
    `lmgw.grant` and `lmgw.llama-fit` record (`--max-vram`, the fitted values), and exist only
    where the engine sees one card. `reserve_as` takes the cards beside the grant and makes the
    charge from the grant's bytes; each decision passes the cards the target sees in its own
    ledger read (`seen_cards`), where WP5's `place` will pass the cards it chose.
  - **An entry's charge in the ledger** is its recorded cards (`card_charge`, else the cards its
    row's arguments pass now) with the bytes the ledger computed before (footprint, grant,
    rung), so a learned figure still moves it. **A group that runs is still charged its estimate
    on the group**, not what its processes hold on each card: that is shown (`cards` on the
    resident, WP2), and putting it in the charge would change what an eviction is counted to
    free. Charged by what it holds per card is WP5's, where the decisions read cards.
  - **Kept free per card** (WP2's deviation): a card's `kept_free` is the starts in flight,
    image peaks and pending loads of the models charged on it alone; absent where a group (or
    a part of unnamed cards) keeps something free on it. Nothing kept is now zero on every
    card, where WP2 showed nothing on a machine of several. The top-level figure stays the sum.
  - **The label** is written for every start admission decided (`Staged.charge` →
    `RenderSpec.charge` → `podman_run_argv`), after `lmgw.llama-fit`. A start no decision
    stood behind (admission off, no capacity, telemetry gone mid-wait, `Fit::Unchecked`) has no
    charge and no label, and is adopted as a container without one, which charges it the same
    group; so does a charge whose cards no device read named. Nothing else in a start changes:
    on one card the label is the only difference (D17, `runtime_argv.rs`).
  - **Adoption** (`reconcile.rs`, `adopted_charge`) reads the label into the entry; one that
    cannot be read is logged and the container is treated as one without. **A container without
    the label is charged its estimate on the cards its arguments pass**, as a group, whether or
    not its processes' figures are read (the reason above), and where that is several cards the
    resident says it is not what it holds on each, until it is started again. Not restarted.
  - **A recorded card that is gone** (in no device of this read; with no device read nothing is
    gone) leaves the entry adopted: it is charged on its other cards, or nowhere where none is
    left, the resident's note names the card, and it is logged once per container. A container
    charged nowhere keeps nothing free either. This is the one decision that differs from
    before, and only on a machine that lost a card.
  - **§3.3's roll-back sentence was wrong** and is corrected there: an older lmgw adopts a
    container with `lmgw.placement`; only a split in `lmgw.llama-fit` (WP6) makes it remove one.
- **WP5 — lmgw places on one card.** §4.2, §5 (steps 1, 2, 5 and 6, the claim, the modes),
  §6.1, §7.1, §8, §8.1 and §8.2: `place`, eviction per card, the pin with its migration, ops and
  editor, the forms that name a card, the context field's notice, the cards by UUID, the budget
  rule of §9.2. A chat model
  with a set context that no single card holds is still started on every card as written, as a
  group (§5.7), until WP6; nothing that runs today is fitted smaller or refused by this package.
  This fixes the pinned row. **Verification** on the test server: an unpinned 4 GiB model lands on
  the tightest card and on that card only; a row pinned to the Blackwell lands there; a pinned
  row is fitted to its card's free memory; a start that fits another card is not held up by one
  that waits.
  *Partly built 2026-11-16.* `place` is `vram/place.rs`, a pure function over
  stated cards (`Card`: total, free, what the start keeps free and costs there,
  lmgw's models there as `Holder`s) for an `Ask` (the whole, whether the row
  adapts, whether it may go over its candidates as a group) and a `Mode`; it
  answers `Placed::{Now, Evict, Wait, Roomiest, Group, Nowhere}`. The
  scheduler's side (`vram/place/choose.rs`) builds the cards from one ledger
  read and gives a decision a `Placement`: the cards, the answer, whether lmgw
  hands the engine exactly those cards, and the headroom, margin, structural
  headroom and engine cost summed over them. Readings of this bullet and of §5,
  §6.1 and §7.1 against the code:
  - **`place` chooses the cards; the decision then does on them what it did on
    the pooled card.** `Ledger::capacity_on(cards, claim)` is the capacity of
    those cards alone (their totals; the driver's free there less what is kept
    free there and what other starts' claims take there); the queue's
    `decide`, `check_background_start`, the guest's start and the climb read it
    where they read the pooled `capacity`, evict only models on those cards
    (`evict_one(…, on)`), fit to them (`fit_in_on`, the count from the
    placement, D14) and reserve on them. On a machine of one card every figure
    is what it was.
  - **Step 1** takes a card that holds the start with its headroom first, then
    a card nothing of lmgw's is on with its margin alone (§5.6's per-card rule);
    among each, D7 and then the lower index.
  - **Steps 3 and 4 until WP6**: a chat model with its whole set (it does not
    adapt) that no card holds now or after evicting is placed on its candidates
    as a group, today's pooled behaviour (the engine sees them all), before step
    5 — ruling 14's place for splits. So such a model is spread as llama.cpp
    sees fit rather than waiting for a busy model on one card.
  - **A row that adapts** (§5.3) and a guest's start (which never waits) go on
    the roomiest candidate where steps 1 and 2 have no card; the decision then
    fits it there and waits, evicts or defers as before.
  - **The claim** (§5.5, D15) is made at the first reading that cannot start
    the model at once, only for a one-card placement on a machine of several:
    there the gate is given back while it waits and taken again after each
    wait. On one card a waiting start keeps the gate as it did, so nothing on a
    machine of one card changes. What a claim takes on its card for others is
    what it needs less what the models it waits for (`place`'s victims or busy
    models, else all of lmgw's there) still hold. Claims on a card are served
    in the order they were made.
  - **What is kept free on a set of cards** counts a model charged as a group
    once, and whole, if the group names any of them: how it divides is not
    known (`KeptOn::upper_on`). A group's bytes on one card, for choosing a
    card and a victim, are likewise its whole; the start itself goes on what
    the driver then shows free.
  - **The climb** keeps the running rung's cards: its room is theirs, its
    victims theirs. Moving a climbing model to another card where the new rung
    does not fit its own was not done in this round (the second round below
    built it); the climb's plan still keeps the cards' summed headroom free
    (`start_headroom`), more than one card's.
  - **The verdict** is per card: it fits where one card the model may go on
    has the room in its free memory and lmgw's share there, busy models
    included, or — for a model placed as a group — the candidates together.
    The shortfall it reports is that of the card with the most room.
    `verdict_for` (the climb's) is on the running rung's cards.
  - **Too large** (§5.8): a chat model that may go over its candidates is
    judged against their sum, any other model against each candidate (the
    largest is named), one lmgw cannot hand a card against the cards it sees.
  - **The finder's fit marks, "Get it"'s refit and a plan's card check**
    (`on_card_now`, `plan_fit::Card::of`) read, on a machine of several cards,
    the card with the most room for lmgw: its total less what is not lmgw's
    there, where a model charged as a group counts as not lmgw's (the larger
    figure). The preview fit in `footprint()` fits a row to the candidate with
    the most room as last read. The image editor's partial-fit note is judged
    against the largest candidate.
  - **Not moved to cards yet** in this round (the second round below moved
    them): warm groups (`group_capacity`, `crowded_beside`) and the realtime
    budget still read the pooled capacity; a warm group's stages are each
    placed by their own admission.
  - **The engine's cards** (§7.1): where the placement is fewer cards than the
    run arguments pass and lmgw can hand a card (a Podman start, NVIDIA cards,
    arguments that select them through CDI or `--gpus`), the charge is marked
    `handed` and `podman_run_argv` replaces the NVIDIA devices, `--gpus`, and
    the row's `NVIDIA_VISIBLE_DEVICES`, `CUDA_VISIBLE_DEVICES` and
    `CUDA_DEVICE_ORDER` with one `--device nvidia.com/gpu=<uuid>` per card,
    `-e NVIDIA_VISIBLE_DEVICES=<uuid,…>` and `-e CUDA_DEVICE_ORDER=PCI_BUS_ID`
    (`runtime::argv::hand_cards`). The run-args digest, the engine key and the
    CDI globals are the arguments as written. A native start, an AMD card, a
    GPU node (`/dev/dri`, `/dev/nvidia0`) and a start no decision stood behind
    are placed as a group and started as written.
  - **The machine check** says in its detail lines whether lmgw's own spec
    names each of several cards by UUID, and that a placed start fails when it
    does not. It is a line, not a reason to write the spec again: writing it
    again with the same toolkit would not add names.
  - **A running group is charged by what its processes hold on each card**
    (WP4's deferral), where the driver gives per-process figures and the
    container's PID is known: the ledger reads its processes per card beside
    its device read (`residency::now`, `DeviceRead::cards`) and charges a part
    per card (`CardCharge::per_card`), saying so on the resident. A container
    without the label is charged so too; without figures both stay a group
    with the note of WP4.
  - **The engine's cost per card** (§4.2, ruling 17) is `vram/place/engine.rs`.
    After a llama.cpp start (chat or aux) on one card, once its container is
    ready and idle, its processes on that card less lmgw's estimate (its grant,
    else its footprint) is that card's cost; read once per container from
    `cache_pids`, beside the readings at rest; the latest reading is kept in
    memory and stored per card in its own table, `vram_engine_cost`
    (migration **0110**; 0109 when built: one row per card, not beside
    the hourly headroom readings, which age out of their window). Until a card has one it is
    charged 640 MiB on an NVIDIA card and 480 MiB on a card of another backend
    (the owner, 2026-11-16: what left `LLAMA_MARGIN`, so such a card keeps as much free as
    before; first built as nothing) (an AMD card is placed as a group and its
    engine cost is learned the same way once a model ran on it alone). It is
    charged on a llama.cpp model's reservation and on its running entry, on
    each card it is on (a group's on each of its cards). Settings → GPU shows
    "Engine cost per card", each card by name and index with its figure and
    whether it was learned or is the default (`HeadroomNow.engine_sentence`,
    `HeadroomCard.engine_bytes`/`engine_learned_at`). A test state charges
    nothing until a test states it (`set_engine_default`), as every test before
    it expects.
  - **`LLAMA_MARGIN` is 327 MiB**: 807 less 480, the smallest engine cost WP0
    measured, so what is left is the most the start's slack can be (the 807
    MiB gap was read on an unknown card). The fixed `headroom_mb` still stands
    in for the slack in fixed mode, unchanged.
  - **The pin** (§8) is migration **0111** (0110 when built; `gpu_pin` on the four tables, a
    JSON list of ids), not §8's 0109: the engine cost took the next free
    number first, and the migrations must stay contiguous (`migration_copy`
    names its copies by them). The rows' own types do not carry it: it is read
    into `Snapshot::gpu_pins` and written by `store::set_gpu_pin` once a
    row's own save went through. `local_model_set`, `aux_model_set`,
    `image_model_set` and `audio_model_set` take `gpu_pin` (a list of ids;
    `clear: "gpu_pin"` or an empty list clears it); the API reference and the
    MCP tool schemas carry the field's doc. `local_model_get` reads it back.
    A pin is the model's candidates (§5.1), of the cards the machine lists; a
    start whose pinned cards are all gone does not go to another card and is
    `gpu_setup` (503), which its fallback answers where it has one (as a start
    the machine check blames). A pin that differs from the cards the run
    arguments name is started on the pin's cards (the NVIDIA arguments
    replaced).
  - **§8.1's forms**: `nvidia.com/gpu=<n|UUID>` and `--gpus device=<n>` were
    read as the cards a row sees (WP4) and are its candidates;
    `-e NVIDIA_VISIBLE_DEVICES=<n|UUID>` and `--device /dev/nvidia<n>` are read
    as a pin where the field is empty (a node keeps the arguments as written,
    since lmgw cannot replace a node with a CDI name); a class's arguments are
    a row's where it has none. A row's own `CUDA_VISIBLE_DEVICES` is not passed
    where lmgw hands the engine its cards, and the start's warnings say so and
    name the GPU pin. **Not done** in this round, built in the second round
    below: llama-server's own `-dev CUDA<n>`, `--main-gpu <n>` and `-sm none`
    in a chat row's arguments (dropping them at render would have made boot
    adoption find the command changed, since adoption renders without the
    placement); the audio class's `device` setting. **Still not done**: the
    editor's offer to move a form into the pin field and Settings' class-level
    pin.
  - **The context field's notice** (§8.2) is `local_model_get`'s
    `context_notice`, shown under the context field while it is empty; the
    copyable command line gets `command_line_note` ("lmgw picks the card when
    it starts this model … now it would be <card>"). The start's own warning
    in the same words (§8.2's last sentence) was not done in this round (the
    second round below built it).
  - **Not done in this round**, beside what is said above, and built in the
    second round below: the warm groups and the realtime budget stage by
    stage with `place`; a climbing model moved to another card; the editor's
    pin field and its list of cards with each card's room; a pin honoured by
    a start no decision stands behind. Left for later: the wait phase naming
    its card (WP8).
  - **Points to note**: (1) a set-context chat model that no card holds
    now or after evicting is spread over its candidates by llama.cpp (WP6's
    place, ruling 14) rather than waiting on one card for a busy model;
    (2) an aux, audio or image model that only several cards together could
    hold is waited for on one card or refused (§5.8, ruling 12), where before
    its engine spread it; (3) the claim gives the gate back only on a machine
    of several cards; (4) `LLAMA_MARGIN` dropped to 327 MiB everywhere while a
    card of a backend with no stated default (AMD) was charged no engine cost
    until one is learned there — the owner ruled (2026-11-16) that such a card is
    charged 480 MiB until then, so it keeps as much free as before; (5) the test state charges no engine cost
    (`set_engine_default(0)`), so the suite's figures are those of before.
  - **Live verification** (on the test server, as the bullet says):
    an unpinned 4 GiB model lands on the tightest card and on that card only
    (its container's `--device nvidia.com/gpu=GPU-…` and
    `NVIDIA_VISIBLE_DEVICES`, `nvidia-smi` showing it on that card alone); a
    row pinned to the Blackwell (`gpu_pin` through `local_model_set`) lands
    there; a pinned row is fitted to its card's free memory; a start that fits
    another card is not held up by one that waits; Settings → GPU shows each
    card's engine cost becoming "learned" after a model started on it alone
    (expect about 621 MiB on the Blackwell and 480 on an Ada); the machine
    check's detail names every card by UUID.
  - **Migrations renumbered when main was merged (2026-11-16).** Main took 0107
    (`chat_message_notices`), so the branch's move up by one: `request_via_tls` 0107 → 0108,
    `vram_outside_hours_per_card` 0108 → 0109, `vram_engine_cost` 0109 → 0110, `gpu_pin`
    0110 → 0111. A database that applied the branch's 0107 has its row for
    version 107 deleted before the new build starts; the build then applies 0107 to 0111, and
    0108 runs a second time on a table it already changed.
  - *Run 2026-11-16* on the test server (3c4a788d deployed; the data migrated 0106 → 0111 after
    version 107's row was deleted, tried on a copy first). Verified: the unpinned 4 GiB row
    (`gpu-test-4b`) went to card 0, the tightest Ada (7.5 GiB free), and its process held
    4208 MiB there and nothing on another card, charged on card 0 only; a row naming card 1
    in its run arguments (`--device nvidia.com/gpu=1`, §8.1) went to card 1 alone (5256 MiB);
    the same row pinned through `gpu_pin` to the Blackwell went there alone (4344 MiB); the
    engine cost was learned per card after each start: 470 and 477 MiB on two Adas, 606 MiB on
    the Blackwell (WP0 measured 476 to 486 and 621 to 627: lmgw's figure is what the process
    holds less its own estimate of the buffers, which runs a little high); the machine check
    says the spec names each of the 4 cards by UUID. The wait could not be provoked for a chat
    row: with `gpu-test-4b` pinned to the Blackwell and busy (a long stream), a copy of it at
    180,224 tokens of context, which only the Blackwell holds and only without the busy model,
    did not wait — it was started at once as a group over all four cards (the order of ruling
    14 until WP6: a set-context chat row no card holds now or after evicting idle models is
    split before anything waits), and `gpu-test-pin1` on card 1 answered in 1.9 s beside it.
    On the test server a chat row waits only when even the group cannot hold it, so the wait is
    left to the tests (and to an aux, audio or image row, which never splits). A pinned row
    fitted to its card's free memory is likewise left to the tests. Found: the self-admin MCP tools
    (`lmgw__local_model_set` and its siblings) do not offer `gpu_pin`, so the pin was set
    over `/api/op/local_model_set`; a pinned row's `command_line_note` is empty where it could
    name the pinned card. Both join WP5's remainder. The test pin was cleared afterwards.
  *Built 2026-11-16, second round*; run on the test server the same day (6e99ad01 deployed, no
  data step): the self-admin MCP server's `tools/list` offers `gpu_pin` on all four setters;
  `gpu-test-4b` pinned to card 3 through `lmgw__local_model_set` read "Pinned to NVIDIA RTX 6000
  Ada Generation (card 3): lmgw starts it on … (card 3) alone, in place of the graphics cards
  these arguments name. Where that card is gone, it does not start on another card, and its
  fallback answers where it has one."; it started on card 3 alone (+4.2 GiB there, the other
  cards unchanged), its container labelled `lmgw.handed` and `lmgw.placement` with card 3's
  UUID; `clear: 'gpu_pin'` removed the pin and the note named card 0 again. Adoption of a handed
  container could not be tried there: restarting the Docker container `lmgw` ends the Podman
  containers inside it, so boot found the model's container exited and removed it (so it was
  before this package); the tests cover adoption. What the first round left:
  - **The self-admin tools take the pin.** `lmgw__local_model_set`, `lmgw__aux_model_set`,
    `lmgw__image_model_set` and `lmgw__audio_model_set` list `gpu_pin` (`selfadmin.rs`,
    `gpu_pin_p`): a list of card ids as `lmgw__status` lists them (`vram.devices[].id`) and
    Settings → GPU names them, what one card and several mean, that `clear: 'gpu_pin'` or an
    empty list removes it, and that a running model moves at its next start; each tool's
    `clear` names it. The API reference takes the tool's words for the op's field (the merge
    of api-docs §4.7). The aux, image and audio reads gain `gpu_pin` and `command_line_note`
    beside the chat read's.
  - **A pinned row's line under the command line** names its cards by name and index and
    where the pin is written (the field, or run arguments that pin it, §8.1): "Pinned to
    <card>: lmgw starts it on <card> alone, in place of the graphics cards these arguments
    name. Where that card is gone, it does not start on another card, and its fallback
    answers where it has one." A pin of several says that lmgw picks among them and which it
    would pick now; a pinned card the machine does not list is named by its id. A running
    model's line begins with where it runs now (§10, "where the model is now").
  - **The editors' pin field** (§8): the chat editor's GPU group, and the aux, image and audio
    editors' container section, list the machine's cards from the GPU panel's frame, each by
    name and index with its free memory of its total, as checkboxes, under "Run this model on
    a card of your choosing. Empty: lmgw picks the card."; a pinned card the machine does not
    list stays checked so it can be taken out (`lmgw-ui/src/pages/gpu_pin.rs`). The chat
    editor counts it as a change like any field; the other three send it once the row's read
    gave it, so a save never clears a pin it did not show. Under the field, the read's
    `command_line_note`.
  - **A pin applies without a decision** (§5.7): a start no decision stood behind (admission
    off, no telemetry, `Fit::Unchecked`) of a row whose pin field names NVIDIA cards, in a
    container whose run arguments pass NVIDIA cards through CDI or `--gpus`, is handed the
    pin's cards as a decided start is (`Registry::pinned_cards`, from the descriptor's new
    `gpu_pin`); its entry is charged on them. A pin written as run arguments
    (`NVIDIA_VISIBLE_DEVICES`, a node) still needs a device read to be one, and without a
    decision those arguments are used as written.
  - **llama-server's own flags that number a card** (§8.1: `-dev`/`--device` with any value
    but `none`, `--main-gpu`/`-mg`, `-sm`/`--split-mode none`, and the drafter's
    `-devd`/`--device-draft`/`--spec-draft-device`, which numbers a card the same way) are not
    passed where lmgw hands the engine its cards, chat and aux alike, and the start's
    warnings name them beside the GPU pin (`argv::split_card_flags`). Beside a row's own
    `--tensor-split` they are kept as written (ruling 11, §7.2). **Adoption**: a container
    whose cards were handed carries `lmgw.handed=<id,…>`; boot adoption renders the command it
    compares with as that start rendered it, without those flags, so it is adopted and not
    restarted. The other ways were each worse: comparing on the arguments as written would
    need a digest of the engine arguments in place of the command, which would stop
    adoption from seeing a row's fitted values or a changed image vocabulary; tolerating
    either command would adopt a container whose row gained such a flag since. The one
    cost: an lmgw older than this round finds such a container's command changed and
    starts it again once, as a roll-back does for a split (§3.3).
  - **The audio class's device index** (§8.1). An audio container handed its cards is started
    with device 0 in its `server.json` (the engine numbers its one card 0), and boot adoption
    compares the `server.json` of a container that carries `lmgw.handed` with that.
    `vram/place/audio_device.rs` reads a non-zero index once as a pin: on the first device
    read that lists the NVIDIA card the index names, where the audio class's backend is
    `cuda` and its run arguments pass NVIDIA cards, every audio row without a pin whose
    engine lmgw can hand that card is pinned to it and the index is set to 0; the learned
    residency, whose key holds the index, is kept under the new key. **Deviation from §8.1's
    "the setting goes"**: the setting stays, since a Vulkan, HIP or Metal build sees every
    card and its index still chooses there; it is left as it is, and said in the log, where
    the backend is not `cuda`, where a row's own run arguments cannot be handed a card, and
    where it names no card the machine lists. A setting saved later is read again the same
    way.
  - **The start's warning in the context field's words** (§8.2): a chat row that sets no
    context, started on one card of several it may use with its context lowered to fit,
    warns "Context fitted to the GPU: No context set: the model's own is used (…). lmgw keeps
    this model on one card. … Now that would be … tokens on <card>. To spread the model over
    several cards for more, set a context." followed by the figures it always gave
    (`VramScheduler::one_card_grant`; the editor's notice and the warning share the words).
  - **Warm groups and the realtime budget per card** (§3.1, `vram/place/in_turn.rs`). On a
    machine of several cards whose models lmgw can all hand a card, and no declared budget,
    the models are placed in turn with `place`, each one's footprint and engine cost on its
    card counted before the next is asked, against each card's total less what is not
    lmgw's there now (lmgw's other models counted as room, since each is admitted as a
    request is); what a model keeps free beside it is admission's rule per card, its
    headroom beside another of the group, its margin alone on a card it has to itself. A
    warm group fits where every model finds a card, spread over several or not; its message
    names the model that finds none and where the ones before it went. A stage still waiting
    is crowded out only where no card it may go on would hold it once every model of lmgw's
    there but the kept ones had left. The realtime budget places its cascade twice — beside
    what other programs hold now, and on the cards' totals alone — for `fits`, `tight` and
    `too_large`, and names each stage's card (`RealtimeBudgetStage.card`); a machine of one
    card, a budget and a model lmgw cannot hand a card keep the pooled sums. Its figures now
    count the engine's own cost on each model's card (`engine_bytes`, per stage and in all),
    on one card too, and its sentence says the CUDA context is that cost rather than the
    headroom's. Settings → Realtime shows the card per stage and the engine's part of the
    bar.
  - **A climbing model moves card** (§5.6): where the new rung does not fit its running rung's
    card now, with the running rung counted as freed, and another card it may go on holds it
    now, the climb starts the new rung there, handed that card (`ClimbTicket::start_on`, the
    entry's charge on the cards set with the claim), before anything is evicted or waited
    for on its own card. Whether a rung is too large is judged against the largest card it
    may go on. A climb does not evict on another card for its rung (§5.2 step 2 over the
    other cards): where no other card holds it now it evicts and waits on its own, as
    before.
  - **Tests**: `vram_admission/placing_pins.rs` (the pinned line, a pin with admission off,
    the engine's own flags dropped and the container adopted, one card keeping them, the
    audio index read as a pin, the start's warning in the notice's words, a climb that goes
    on another card, the realtime budget per card), `selfadmin.rs`
    (`the_model_setters_offer_the_gpu_pin`), `argv/engine_cards.rs`,
    `lmgw-ui/src/pages/gpu_pin.rs`; `placing.rs`'s notice test reads where the model runs now.
  - **What WP5 still leaves** (beside WP6 to WP8's): the editor's offer to move a run-argument
    pin into the field, and Settings showing a class's run arguments as a pin with an offer to
    clear it (§8, §8.1); step 2's tie-break by victims that could be started elsewhere
    (§5.2, which needs WP7's repacking); the benchmark on the placement the row would get,
    with its figures per card (§3.1); a native start's cards (`CUDA_VISIBLE_DEVICES`, §7.1);
    the wait phase naming its card (WP8).
- **WP6 — Splits.** §4.3, §5.2 steps 3 and 4, §7.2. Needs WP0's numbers. **Verification:** a
  model with a set context larger than any card's free memory starts over the fewest cards, and
  each card holds what the placement says within the margin.
  *Confirmed by the owner 2026-11-17*: a set-context row with automatic `--n-gpu-layers` is split
  before it is fitted smaller on one card (D6), and a set-context row lmgw cannot split waits or
  is refused instead of being spread by llama.cpp over every card (D9).
  *Run on the test server 2026-11-17* (b3a9a81e7315): a copy of `gpu-test-4b` at
  262,144 tokens, which no card's free memory held (the Blackwell had 21.7 GiB), was planned in the
  row's note and started over cards 2 and 3 only, handed by UUID with `CUDA_DEVICE_ORDER=PCI_BUS_ID`,
  `--split-mode layer --tensor-split 31,6 --fit off` (37 = n_layer + 1), label `split=31/6`; the
  driver showed its process on those two cards only. Planned with the engine's cost 21,175 and
  about 5,781 MiB, held 21,414 and 5,918 MiB: +239 and +137 MiB, inside the 327 MiB margin. Once
  ready the ledger charged what the process holds on each card. Stopped, the cards went back to
  what they held before. Found: filling the roomiest card first left it 274 MiB free; the owner
  keeps that order (spreading would move layers to slower cards and leave smaller holes) and the
  slack for other programs is a headroom question (§9.1), put to a review.
  *Built 2026-11-16*. It replaces WP5's "steps 3 and 4 until WP6":
  a set-context chat row no card holds now or after evicting is no longer started over every
  candidate as a group with llama.cpp's own split; it is split over the fewest cards that hold
  it, with explicit layers per card, or it goes on to step 5. Readings of §4.3, §5.2, §5.4,
  §7.2 and §3.3 against the code:
  - **What a split puts where** is `vram/plan/chat/by_layer.rs` (`ByLayer`, on the footprint
    as `Footprint::by_layer`): the one-card footprint taken apart, so its parts add up to it.
    Each repeating layer: its tensors' bytes from the header's tensor list less what the row's
    tensor overrides keep in host memory (`offload::layer_bytes`, matched as the one-card
    figure matches them, so `--n-cpu-moe`'s experts come off the first layers), and its KV
    cache (the main context's, layer by layer from `Layout::of_from`). The last card: the
    output layer (every weight outside the repeating layers on a card) and an MTP context's
    cache and compute buffer. The first card: the projector, a draft model's weights, cache and
    compute buffer, and the expert cache. Every card: the main context's compute buffer as one
    card reserves it. A row whose header has no layer count or a tensor list not read whole has
    no `ByLayer` and is not split, nor has one whose `block_count` is more than its tensor
    list can back (every layer has a tensor): the header's count is never a size to allocate
    by (a review of the first commit found a crafted `block_count` of 2^32 made lmgw allocate
    tens of GiB; fixed before the check). A `--n-gpu-layers` short of all splits the layers on the
    cards only; the entries then sum to the row's `--n-gpu-layers`.
  - **The compute buffers of a split and the projector's own are not predicted** (§4.3's
    "where WP6 cannot predict them"). A split start reserves the one-card compute estimate on
    each of its cards (WP0: the first card's real buffer was 2.2 times that, every other card's
    1.7 times, for the 4B; with a projector the first card's grows further) and, once it runs,
    is charged what its process holds on each card, where the driver lists per-process figures
    (the measurement WP5 built for a group, `CardCharge::on_several`). Until it is measured it
    is charged the parts admission reserved, not a total that moved since. No factor from
    WP0's one model is used. The start's warning says so.
  - **Who may be split** (`VramScheduler::splits`): a chat row that sets its context (the
    field, `-c`, or a pool per slot; ruling 15 keeps one that sets none on one card, also where
    the image has no `--fit`), without a `--tensor-split` of its own (ruling 11: lmgw's never
    replaces a row's), whose image's `--help` lists `--split-mode` and `--tensor-split` (D20;
    read with `--fit`, `fit_vocab.rs`, for such a row once a device read has shown several
    cards) and, with a draft model, `-devd`. A set-context row whose layers `--fit` could move
    (auto `--n-gpu-layers`) is split before it would be fitted smaller on one card (D6): WP5
    asked `place` such a row as one that adapts, which skipped the split; the split is now
    asked whatever `adapts` says, and a row that sets no context has none to ask. The split
    is of the row as written, also where the footprint was fitted to one card for a preview
    (`VramScheduler::footprint`). Any
    such row that cannot be split says why at the end of its refusal or its wait's log line.
  - **The order** (`place`): steps 3 and 4 stand after step 2 and before step 5, as written.
    Step 3: the cards by room — free less what the start keeps there (its headroom while
    something of lmgw's is on that card, its margin once nothing is) less the engine's cost —
    most first; two, then three, … until a split over them holds it (`place/split.rs`,
    `fewest`, at most 16 cards). Step 4: the same with each card's idle victims (within the
    mode) counted as room; then, card by card in the bus's order, the fewest victims (least
    recently used first) each card's part needs. The verdict splits too, lmgw's share counted
    as room, by its own per-card checks in `verdict.rs` and not through `place`: `split_fits`
    asks `split::fewest` with each card's measured free memory and lmgw's share there, less
    its floor and the engine's cost there, and `own_verdict` checks a row's own split kept
    on its pin card by card; under a declared budget it does not split. (Corrected
    2026-11-17: this note named a `Mode::Verdict` of `place`, which no decision passed; it
    is removed.)
  - **Filling** (§4.3): the chosen cards in the order of room, each but the last filled to its
    room and the rest on the last. Which layers a card gets follows from the bus order (the
    first card the first layers, the last card the output layer), which can differ from the
    order of room, so a card is filled by the widest run of that many layers it could get
    (`ByLayer::widest`), and the split is then checked card by card with the layers it does
    get. A card left with none takes one from the card with the most. Not filling by
    proportions, as §4.3 says.
  - **Telling llama.cpp** (§7.2): `LlamaPlacement` gains `split: Option<TensorSplit>`
    (`runtime/argv/llama_fit.rs`), rendered beside the params as `--split-mode layer
    --tensor-split <n,…>`, `-devd CUDA0` with a draft model, and `--fit off` where the image
    has `--fit`. `--n-gpu-layers` is not rendered (the row's, if it sets one). A split's
    engine is always handed its cards (`Placement::handed`), even where they are every card
    the arguments pass, since `CUDA_DEVICE_ORDER=PCI_BUS_ID` is what makes the entries' order
    the cards'. The row's own `-sm`, `-ts`, `-mg`, `-devd` spellings are dropped beside lmgw's
    split (they join the render's alias table). The grant is a `PartialGrant` of the split's
    bytes with no warning of its own; the placement's note is the start's warning ("No card
    holds it whole: lmgw splits it over 2 cards, the fewest that hold it — … llama-server is
    told …"). The command line's note says "now it would be split over … and …".
  - **The label** (§3.3): `lmgw.llama-fit` adds `split=<n>/<n>…` and `devd=1` before `fit`,
    and `fit=none` for an image without `--fit`; a one-card label is unchanged. `only()` keeps
    the split, so adoption renders it; the placement label holds a part per card, which
    adoption charges (`a_split_container_is_adopted_with_its_parts`). An lmgw older than WP6
    reads such a label as nothing, renders the command without the split, finds it changed and
    starts the model again, as §3.3 says of a roll-back.
  - **Deciding on a split** (`place/split_start.rs`): a split fits where each of its cards has
    its part, the engine's cost and what the start keeps there free now — never the cards'
    sum. Where one is short, the eviction is on that card, and the start claims its part on
    each card (§5.5, a claim per card under one id: `claim_parts`), so another start sees on
    each card only what the split needs there. The owner's start, the warm and boot start
    (`check_background_start`, which never waits: a short card is "not now") and the guest's
    start (idle guests on the short card) all decide so.
  - **The ledger**: a split's reservation and its starting entry keep free its part on each
    card with the margin beside each card's part (each card's process starts on its own); the
    margin of a split's first prompt is likewise kept on each card.
  - **A climb** of a split model does not split its new rung: its layer counts are the running
    rung's, so the new rung goes on the same cards as a group and its grant loses the split.
  - **Tests**: `place/tests.rs` (the measured machine of §1 splits a 24 GiB set-context model
    over cards 2 and 3 and fits an adapting one to card 2; three cards where two do not hold
    it; evict before splitting and a split after evicting with the victims each card needs; a
    split now before waiting for a busy model; the verdict's split), `plan/chat/by_layer.rs`,
    `llama_fit.rs` (the label and the flags), and
    `vram_admission/placing_splits.rs` (the verification: a set-context model no card holds
    starts over the fewest cards by UUID, with `--tensor-split` entries that sum to `n_layer +
    1`, `--fit off`, each card's part within what was free there less the 4 MiB kept free, the
    driver and the ledger showing each part on its card, and the warning; adoption; a row with
    no context stays on one card; a row's own split is not replaced). The fake machine holds a
    split container's placement parts on the cards it was handed.
  - **What WP6 leaves** (ruling 11 and 16 built in WP11): ruling 11 beyond "a row's own split wins" — a row with its own
    `--tensor-split` is still placed as WP5 placed it (on one card, its split passed as
    written to an engine that sees that card), not yet on its pin's cards and charged by its
    split, and one unpinned is not yet started without it with the warning and the editor's
    note §7.2 asks for; ruling 16 (an aux row's own split dropped with a warning); warm groups and the
    realtime budget place each model on one card (`in_turn` never splits), so a warm group
    whose chat model only a split holds is refused as before; a split of a ladder rung;
    fitting a model smaller over several cards ("Not yet"); the compute buffers of a split
    as a formula; the fit marks and messages naming splits (WP8). A chat row that sets its
    context but cannot be split (no tensor list, no `--tensor-split` in its image) is no
    longer spread by llama.cpp over every card: it waits or is refused like an aux row (§5.8
    against each card), and says why.
  - **Live verification** (the test server's four cards): a copy of
    `gpu-test-4b` with a set context that no card's free memory holds (as in WP5's run,
    180,224 tokens) starts over two cards, not four: its container passes those two by UUID
    with `CUDA_DEVICE_ORDER=PCI_BUS_ID`, its command has `--split-mode layer --tensor-split
    a,b --fit off` with a+b = 37, `nvidia-smi` shows its process on those two cards only, and
    each card's figure is within the margin of its part in `lmgw.placement` (expect the first
    card's compute buffer above the estimate, WP0's 205 against 93 MiB, until the measurement
    charges it); `/api/vram` shows a part per card, and after it is ready and the outside
    trigger is armed, what its process holds on each; with the busy pinned copy of WP5's run on
    the Blackwell, the same start does not wait but splits over the other cards that hold it;
    a restart of lmgw adopts the split container without starting it again.
- **WP7 — Repacking.** §6.2. **Verification:** with an idle model on the card a larger one
  needs, the idle one is running on another card afterwards, as old as it was.
  *Confirmed by the owner 2026-11-17*: a move uses step 1 only and never evicts (§6.2 corrected);
  no moves under a declared budget; a moved model may move again once a request has claimed it
  (a claim on the ready entry, not `Entry::served`).
  *Built 2026-11-16* (not run on the test server yet). It also builds what WP5 left of §5.2 step 2,
  the tie-break by victims that could be started elsewhere. Readings of §6.2, §5.2 and D21
  against the code:
  - **Where a victim could go** (`VramScheduler::elsewhere`, `vram/place/repack.rs`): of its
    candidates (its pin, else the cards its arguments pass, §5.1) other than the card it is
    stopped on, each card whose free memory — less what other starts' claims take there, plus
    what the victim itself holds there (a split victim's other card) — holds its whole
    footprint, the engine's cost there and what it keeps free there (its headroom where
    something else of lmgw's is on that card, else its margin): §5.2 step 1 for it. The
    footprint is the one the ledger read sized it by for its charge; the ledger now keeps it
    per generation (`Ledger::footprints`). None on a machine of one card, under a declared
    budget (a bound on the whole, §9.2, which a second start is not judged against; as the
    climb's `climb_elsewhere`), for a model lmgw cannot hand a card (§5.7: placed as a group,
    it cannot be moved), for one that is not ready or not idle, and for a moved one no claim
    has been taken on since.
  - **Step 2's tie** (`place`, `Holder::elsewhere`, `all_move`): after the fewest victims, the
    card whose victims could all be started again on another card now comes first, then the
    victims used longest ago, then D7. "All" is checked in turn: each victim on the card it
    leaves least spare on, what the ones before it take there counted, and never the card it
    makes room on. The verdict does not ask it.
  - **The move** (`repack`, `start_moved`): `evict_one` has stopped a victim on the short
    cards; where another card holds it now, as the reading before the stop found the cards,
    it is started again by a task of its own and the request goes on without waiting for it
    (D8). The task takes the gate as a warm start does and is then decided as one
    (`background_on`, `check_background_start`'s decision factored out: never evicts, never
    waits, a reservation or "not now"), on its candidates other than the short cards, by step
    1 alone (`place` answering `Now`): whole, on one card, never split, never fitted smaller.
    It is handed its card by UUID as a fresh start is (§7.1), and keeps its origin (a guest's
    model stays a guest's, `acquire_as`). **Reading**: §6.2 says both "fits on another card
    now, without evicting anything there" and "placed by steps 1 and 2 only"; it is built as
    the first, step 1 alone, since the start §6.2 names (the warm path) never evicts, and a
    move that evicted would stop a third model for a second one that nobody asked for.
  - **Its age moves with it** (`registry::Moved`, `Registry::stage_moved`): the reservation's
    staged grant carries the victim's `last_used` (now less its idle age in whole seconds, as
    the registry's view gives it, so it comes back at most a second younger), the new entry
    is given it, and neither its `ready` nor the release of its own start's claim stamps it;
    the idle timeout goes on running and the eviction order is as it was.
  - **Not again before it has served** (`Entry::moved`): a moved model is an ordinary victim
    until a claim is taken on it once it is ready — stopped for room, not started again. The
    first such claim (a request) ends the move and stamps `last_used` as any claim does.
    **Deviation**: "a request has used it" is read as a claim taken on the ready entry, not as
    `Entry::served`: that flag is the ledger's first-prompt margin and is set only by the
    forwards that run a prompt through `send_local_marked`, so a model asked in another way
    would never be movable again. A climb's new container ends the move too.
  - **Shown**: the entry's warnings carry "Moved from card 0 (…) to card 1 (…) to make room
    for <model>" for the container's life, and the log says it; a victim that stays stopped
    after all says why in the log ("no other card holds it now", the hold, its row gone).
  - **A request mid-move, a move that fails** (§6.2 says nothing of either): a request for the
    model before the move's reservation starts it as any request does, wherever it then goes,
    evicting if it has to, and the move finds it up and does nothing; one after it joins the
    move's start (and its claim ends the move). A move whose start fails leaves the model
    stopped, as an eviction did, and its next request starts it. So a move never leaves a
    model that cannot be served.
  - **A restart of lmgw**: a moved container that boot adopts is adopted as any is, `last_used`
    now and not marked moved: its age and the "not again" mark do not survive a restart.
  - **Tests**: `place/tests.rs` (`evicting_prefers_victims_that_can_move`: the movable victim
    beats an older one; neither, the older; fewer victims first; two that fit one card only
    one at a time do not all move; never onto the card it makes room on) and
    `vram_admission/placing_moves.rs` (the verification on two cards: the idle 2 GiB model on
    card 0 is stopped for an 8 GiB model, is running on card 1 afterwards, handed card 1 by
    UUID, with its warning, its idle age at least what it was and older than the model it made
    room for, the driver showing each on its card, and a request then makes it young; on
    three cards a moved model stopped for room again stays stopped though card 2 holds it, and
    once a request has used it the same eviction moves it to card 2; a model pinned to card 0
    stays stopped; one card moves nothing). `placing.rs`'s eviction test (a victim that fits
    nowhere else stays stopped) is unchanged.
  - **What WP7 leaves**: repacking without a request waiting ("Not yet"); a move that evicts
    (step 2 for the moved model); a model that adapts and ran fitted smaller (§5.3) is moved
    only where its whole fits, not at the size it ran (its fitted size is not asked); a ladder
    model moved comes back at its base rung, as any cold start does; the tie's check of
    several victims does not count that the first one landing on an empty card makes the
    headroom apply there for the next; the wait phases and the fit marks do not name a move
    (WP8); the move's age and mark do not survive a restart of lmgw.
  - **Live verification** (the test server's four cards): start
    `gpu-test-4b` unpinned (it goes on the tightest card, card 0 in WP5's run), let it idle a
    minute and read its idle age (`lmgw__status` → runtime `last_used_age_seconds`); then ask
    for a row with a set context that card 0 holds only without `gpu-test-4b` and that no
    other card holds now (a copy of `gpu-test-4b` at a larger context, pinned to card 0 so it
    is not split). Expect: the request is answered without waiting for the move; `gpu-test-4b`
    is running again on another card that had room (its container's `--device
    nvidia.com/gpu=GPU-…` and `NVIDIA_VISIBLE_DEVICES` name that card, `nvidia-smi` shows its
    process there and nothing of it on card 0); its entry's warning "Moved from … (card 0) to
    … to make room for …"; its idle age at least what it was, not reset; the log line
    "chat/gpu-test-4b: Moved from …". Then the same with `gpu-test-4b` pinned to card 0: it
    is stopped and stays stopped. And once more without a request between: a second eviction
    of the moved model leaves it stopped.
- **WP8 — Words.** The fit marks and messages of §10, the wait phases' cards; `README.md`,
  `docs/server.md` and `docs/backends.md`; per-model containers' "not yet" lines for several
  cards (`:312`, `:326`, `:480`, `:533`, `:585`, `:669`, `:990`) and
  `image_partial_bounds.rs:136`'s sentence.
  *Built 2026-11-17* (not run on the test server yet). Readings of §10 and §3.1 against the code:
  - **The wait phases name their cards** (`WaitCard` in `lmgw-api-types`, `status.rs`: id,
    index and name; `WaitPhase::WaitingForRoom.cards`, `WaitPhase::Evicting.cards` and
    `.moves`; additive, absent when empty or false). On a machine of several cards a wait
    carries the card or cards it is on: the placement's, or for a split the card that is
    short, and then `needs_bytes` and `free_bytes` are that card's (they were the parts' sum
    and the placement's free). The queue's stage, which the Traffic page shows, says the same
    (`stall.rs`, `wait_cards`, `waiting_stage`, `evicting_stage`): "needs 8.0 GiB on
    <card>, 2.0 GiB free there — evicting aux/embed-model, which starts again on another card
    that has room", "… waiting for a busy model there to finish", "… nothing of lmgw's is on
    that card to evict; waiting for memory held outside lmgw". Which of these it says counts
    lmgw's models on those cards only, where it counted the machine's. The eviction's free
    figure is the cards' (it was the pooled free, beside a need on one card). On one card
    every phase, stage and figure is as it was.
  - **A move is named** (WP7's leftover): before it stops a victim, `evict_one` asks
    `elsewhere` on the same reading `repack` asks after the stop, and the phase's `moves` says
    the victim starts again on another card. Which card is not named: the move is placed by
    its own task after the stop, and its warning and log line name it.
  - **The Chat's image and speech calls** (`ToolProgressFrame.on`, `.moves`;
    `MediaProgress::Stage`): "Waiting for room on <card> (needs …, … free)", "Unloading
    chat/q from <card> (it moves to another card)"; the MCP progress notifications say the
    same.
  - **Messages name the card that was short.** A queue timeout's `holding`
    (`describe_holders`) names the cards waited for and lists lmgw's models there only
    ("<card> is held by …"; "nothing lmgw put on <card> is resident — another process is
    using that card"); so do the refusals of a start that never waits (a split short on a card,
    too little free). "Too large" says what it was judged against (`TooLarge.holder`, and
    `VramTooLarge.holds`, which takes the place of its `capacity` so the error stays within
    its 104 bytes): "<card>, the largest card it may use, holds …", "<card> holds …"
    where it may use one, "the 4 cards it may use hold …" for a chat model judged as a split,
    "the 4 cards its engine sees hold …" for a group, and for a climbing rung its largest
    card or the cards it runs on; "the GPU holds" on one card and in a guest's check before
    the gate, which reads the pooled total. The log says "evicted … from <card> to make room
    for …" and "… waits for room on <card>: …".
  - **Fit marks** (§10): `FitCard` (`index`, `name`, `of`: how many cards) on `FileFit.card`
    and `CardMeasure.card`, set on a machine of several cards without a declared budget: the
    card the fit was measured against, WP5's card with the most room for lmgw
    (`OnCardNow.card`). The finder's chip says "fits on card 2", "fits by size on card 2",
    "doesn't fit on card 2", and its uses line names the card by name and index; the file
    list's chip says "fits on card 2 · 32 768 tokens"; who has to make room says "lmgw's other
    models on card 2 unload or move", "other programs on card 2 close". Every fit carries a
    note naming the card and the number of cards, and that a chat model is split only where it
    sets its context and no card holds it; the file list says it once, as it does any note
    every file shares. A plan's card check (`plan_fit::over_card`) names the card, and for a
    chat plan, which sets the trained context, says that no single card holds it and that lmgw
    splits it where its engine image can, in place of "admission will refuse".
  - **Not built: a mark for a split** ("fits over cards 0 and 1"). The marks measure one
    card, as "Get it" fits one (§5.3, WP9). A mark over several cards needs the finder to ask
    `place`'s split of the file at the context it names, and it would change which file the
    finder recommends and what "Get it" creates (a row whose set context only a split holds).
    That is a change of behaviour, not made yet.
  - **The docs.** `README.md`: the features list, the NVIDIA requirement, and a section
    "Several graphics cards" (where a model goes, splits, making room and moves, the pin,
    the fit marks, waiting, headroom and the engine's cost, and what is not built).
    `docs/server.md`: what `AddDevice` passes on several cards and the machine check's UUID
    line; what ran on the Docker host with the ledger (WP5's runs) and what has not.
    `docs/backends.md`: what lmgw reads from an image's `--help` for a split, the environment
    a container handed its cards gets, and AMD.
  - **Per-model containers' lines.** Each of the seven (the one gate, a partial image start
    on several cards twice, the row's own `--fit-target`, a llama row on several cards,
    headroom per device, out of scope) keeps its text and gains a dated note saying what this
    record does now. `image_partial_bounds.rs:136`'s sentence was replaced at WP5 by the test
    that places such a row on one card (`on_several_cards_it_is_placed_on_one_and_fits_there`);
    the same words were still in `vram/partial.rs`'s module doc and are corrected there.
  - **Tests**: `stall.rs` (`on_several_cards_the_words_name_the_card_and_a_move`), the Chat's
    stage words, the finder's chip and uses line, the file list's marks
    (`who_makes_room_is_what_the_card_says`), and `vram_admission/placing_moves.rs`
    (`a_followed_wait_names_the_card_it_makes_room_on_and_the_move`,
    `too_large_names_the_largest_card_it_may_use`).
  - **What WP8 leaves**: the mark for a split (above; built in WP12); naming the card a move goes to in the
    wait; the Traffic page's queue shows the stage's words, not the phase's cards as a field
    of its own.
- **WP9 — "Get it" waits for its engine image (ruling 13; independent of WP0 to WP8, may be
  built first).** The `model_get` op starts the engine image's pull beside the downloads
  (`ops/model_get.rs:454-476`, `engine::fetch`), but the `model_get` job waits for its downloads
  only (`jobs/model_get.rs:295`, `wait_for_downloads`) and then sets the row up (`:476`,
  `set_up`), whose create asks the image for `llama-server --help`. The job waits for the image
  to be there: it asks, as the op does, and starts or joins a pull, shown as a stage of its own
  ("waiting for the engine"). It does not wait on a stored job id: `ModelGetRow` has no field
  for one (`store/model_gets.rs:10-35`), image pulls are not resumed at boot as downloads are,
  and a get reattached after a restart would wait on a job that is gone. So WP9 needs no
  migration and the pin keeps its number (now 0111, §8). A pull that fails, or an image lmgw does not know how to
  fetch (`EngineFetch::NotKnown`, `Refused`), does not fail the get: the files are here, so the
  row is created as today, with the pull's sentence in place of "image not known". *Built
  2026-11-13* (`jobs/model_get.rs`, `wait_for_engine`; the stage is `waiting_for_engine`). Two
  readings of this paragraph against the code: `NotKnown` is podman not saying whether the image
  is here, not an image lmgw cannot fetch; the job does not wait for such an image, as a start
  does not, and the create's warning keeps podman's own reason, since there was no pull to have
  a sentence. An image no registry gives is `Refused` (`PullFailed::NotPullable`), and its
  sentence replaces podman's. An engine that runs no container (the native launcher) is not
  waited for either. With WP5 the
  fit of step 4 (`modelinfo::refit::for_get`) asks `place`, one card (§5.3), instead of the
  pool, which is what planned a 24 GiB context on a machine whose roomiest card had 22 free
  (§1).

- **WP10 — A drafter without a speculative type gets `draft-simple` (ruling 18; independent of
  the ledger).** Where a chat row sets a drafter (`draft_gguf_path`, or `--model-draft`/`-md` in
  its arguments) and no speculative type (`spec_type`, or `--spec-type` in its arguments), the
  rendered command line carries `--spec-type draft-simple`, where the image's `--help` lists
  that value. The editor's rendered command line shows it, so it is not hidden.
  *Built 2026-11-16.* `LlamaArgs::with_default_spec_type` (`runtime/argv.rs`) sets `spec_type` to
  `draft-simple` on a chat row whose drafter (`draft_gguf_path`, or `--model-draft`/`-md`/
  `--spec-draft-model` in its arguments) has no type (`spec_type` or `--spec-type`), reusing
  `ladder_checks`' `effective_spec_type`/`effective_draft_path_is_set` (the latter now also reads
  `--spec-draft-model`, which rule 6 gains too). `Registry::with_draft_type` applies it only where
  the image's `--help` lists `draft-simple` among `--spec-type`'s values, at the three places a
  command is rendered from a descriptor: a start, boot adoption's comparison and a bench run; the
  editor's command line (`command_line_preview`) cannot wait on podman and uses what the last
  `--help` read of the image said (`with_draft_type_known`), so it shows the type once a start, an
  adoption or a save's check has read the image. Where the help is unread, or the engine is native
  (no image to read), nothing is rendered, as before. Every rung inherits it, since it works on the
  rung's own descriptor. A running drafter container started before this build has no
  `--spec-type`, so adoption finds its command changed and restarts it once (it was running with
  no drafter anyway).
- **WP11 — A row's own split (rulings 11 and 16; added to this design
  2026-11-17).** §7.2's last paragraph: a chat row with its own `--tensor-split` pinned to as
  many cards as the split has entries is placed on those cards in PCI order and each card charged
  by the row's split in place of §4.3's filling; without a pin or with a pin of another size the
  editor says the split needs a pin of that many cards, and the row is placed by §5 and started
  without its split (`--main-gpu`/`--split-mode` dropped with it), the start's warnings saying so.
  An aux row's own split is dropped with a warning that aux models run on one card, and the row
  is placed by §5. **Verification:** a chat row with `--tensor-split 1,1` pinned to two cards
  starts on those two with its split and each card holds its share within the margin; the same
  row unpinned starts on one card without the split and warns; an aux row with a split starts on
  one card and warns.
  *Confirmed by the owner 2026-11-17*: a row whose own split is dropped for want of a pin is
  placed by §5 like any other row, so lmgw may split it itself (WP6) where no single card holds
  it; a row's own split in a mode other than `layer` is kept as a group, as built.
  *Built 2026-11-17* (not run on the test server yet). Readings of §7.2, §8.1 and rulings 11 and 16
  against the code:
  - **What becomes of a row's own split** (`vram/place/own_split.rs`, `OwnFate`, on the
    candidates as `Candidates.own`): asked only where lmgw hands the engine its cards (§7.1, D18)
    on a machine of several cards; on one card, and for an engine lmgw cannot hand a card, the
    arguments are used as written, the split with them (D17). A chat row's split is **kept**
    where its pin has as many cards as the split has entries and this computer lists every one;
    the pin is the GPU pin field, else the run arguments' pin as WP5 reads it
    (`NVIDIA_VISIBLE_DEVICES`, an NVIDIA device node), else the CDI names of its run arguments
    where they name fewer cards than the machine has (§8.1: also a pin). It is **dropped** with
    no pin, a pin of another size, a pinned card this computer does not list, a value lmgw
    cannot read (llama-server's own rule: numbers of 0 or more separated by commas or slashes,
    at most 16 — read by `argv::own_split_of`, which never takes more than 17 tokens of the
    value), and always for an aux row (ruling 16).
  - **Kept** (ruling 11): the row is not asked of `place`; it goes on its pin's cards in the
    bus's order (`own_split::fixed`), each card charged what the row's split puts there
    (`ByLayer::by_entries`, from WP6's per-layer breakdown of the row as written). The entries
    divide as llama.cpp divides them, not as layer counts: their running sums normalised to 1,
    and layer `i` of the `n` on the cards (the output layer the last) on the first card whose
    share is more than `i / n`, in `f32` — so `1,1` over 8 layers and the output layer is 5 on
    the first card and 3 with the output layer on the second. A card given a layer is charged a
    compute buffer; the first card the first card's parts; the card the output layer lands on
    (the last, unless the last entries are 0) the last card's. Where the idle models a card's
    part needs gone are enough, they are the victims; else none are named and the start evicts
    or waits on the card that is short, as any split's (`split_short`). It never goes
    elsewhere, is never split by lmgw, never fitted smaller. Its grant is the split's whole
    (`Grant::Whole`), nothing rendered over the row: its command is the row's arguments as
    written, `--tensor-split`, `--split-mode`, `--main-gpu`, `-dev` and `-devd` included, and
    no `--fit off`. It is handed its cards by UUID like any handed start (always, as lmgw's split
    is, since `CUDA_DEVICE_ORDER=PCI_BUS_ID` is what makes the entries' order the cards'), and
    its charge says so (`CardCharge::keeping_own_split`). Its warning: "Its own --tensor-split
    1,1 is kept: lmgw starts it on the 2 cards of its pin with its arguments as written — 5
    layers on …, 3 layers and the output layer on … . What a split adds to each card's compute
    buffers is not predicted: …", and once loaded it is charged what its process holds on each
    card, as WP6's splits are.
  - **What lmgw cannot divide per card** — `--split-mode row` or `tensor`, entries all 0
    (llama.cpp then splits by free memory), a header without a tensor list — is kept all the
    same, placed on its pin's cards as a group (§5.7) and charged its estimate on them together
    until it has loaded, then what its process holds on each. Its warning says so.
  - **Too large** (§5.8): a kept split is too large where one card of its pin cannot hold what
    the split puts there beside what it keeps free with nothing else of lmgw's on it; the
    refusal says "needs <part> … and its own --tensor-split puts that on <card>, which holds …"
    (`TooLarge.bytes`, the part judged in place of the footprint). The outside verdict judges
    each card's part (`own_verdict`).
  - **Dropped** (§7.2, ruling 16): the row is placed by §5 as any row — and so, where it sets
    its context and no card holds it, lmgw may split it with a split of its own (WP6's "without
    a `--tensor-split` of its own" no longer stands for such a row) — and it is handed its cards
    even where they are every card its arguments pass, so the split is never passed to an engine
    that sees fewer cards than it names. Its `--tensor-split`, and `--split-mode` and
    `--main-gpu` whatever their values, are not passed (`argv::card_flags`, its `split` part),
    beside WP5's flags that name a card. The start's warning: "Its llama-server arguments'
    --tensor-split 1,1 --main-gpu 0 were not passed: a split of its own needs a GPU pin of 2
    cards, and it has none, so lmgw placed it as it places any model; to keep the split, pin it
    to 2 cards" (or "it is pinned to 1", "this computer does not list …", "lmgw cannot read it
    as a split …"); an aux row's: "… were not passed: aux models run on one card".
  - **The editor** (§7.2): the line under the command line (`command_line_note`, which the
    chat, aux and image editors and `lmgw__local_model_get` show) ends with what becomes of the
    split: "Its own --tensor-split 1,1 is kept: lmgw starts it on the 2 cards of its pin, …";
    "Its own --tensor-split 1,1 needs a GPU pin of 2 cards, and it has none: lmgw starts it
    without --tensor-split 1,1 --main-gpu 0 and places it as it places any model. To keep the
    split, pin it to 2 cards."; "Aux models run on one card: lmgw starts it without …". A
    pinned row whose split is kept reads "lmgw starts it on all of them" in place of "picks
    among them", and the context field's notice (§8.2, one card) is not shown for it. Not in
    the static checks (`problems`): they read no pin and no device.
  - **A pin without a decision** (§5.7, `Registry::pinned_cards`): admission off or no
    telemetry, a chat row whose pin field has as many cards as its split has entries is handed
    them and keeps its split; any other row with its own split is handed its pin and started
    without it, and the entry's warning says so.
  - **Adoption** (§3.3): a container handed its cards for a kept split carries
    `lmgw.own-split=kept` beside `lmgw.handed`, and boot renders the command it compares with
    as the arguments as written; one without it, without the split. An lmgw older than this
    round renders a dropped split's container with the split and starts it once again; a
    container an older lmgw started with a row's split on one card (WP5 to WP8 passed it to an
    engine that saw that card) is found changed by this one and started once again, on its pin
    or without its split.
  - **Moves** (§6.2; the record is silent): a kept split is never moved (`elsewhere` and a
    climb's other card answer nothing for it): its pin's cards are all its own, so its pin has
    no spare card, and one card would not hold its split.
  - **Tests**: `argv/engine_cards.rs` (the split and what goes with it taken apart, the value
    read as llama-server reads it and bounded), `plan/chat/by_layer.rs` (`by_entries` divides
    as llama.cpp does), `place/own_split.rs` (each fate and its words; the split over its pin,
    the victims, the verdict), and `vram_admission/placing_own_splits.rs` (the verification: a
    chat row with `--tensor-split 1,1` pinned to two cards starts on those two by UUID with its
    split and `--main-gpu` as written, each card charged its share within what was free there
    less the 4 MiB kept free, the driver showing each share on its card, the warning and the
    editor's line, adoption; the same row unpinned starts on one card without the split and
    warns, and is adopted; a pin of one card; a run-argument pin keeps it; an aux row's split is
    dropped with its warning; an unpinned row no card holds is split by lmgw). `placing_splits`'
    "a row's own split is not replaced" is replaced by the last of these.
  - **What WP11 leaves**: warm groups and the realtime budget still place each model on one
    card (`in_turn`), a kept split too; a kept split is charged at its whole and never fitted
    smaller (a row that sets no context and keeps `--fit` may be lowered by llama.cpp's own fit
    at load, which the measurement then charges); a drafter beside a kept split without `-devd`
    is spread by llama.cpp over every card while lmgw's estimate puts it on the first card,
    until the measurement; a projector is charged to the first card whatever `--main-gpu` says;
    the static checks do not name the pin a split needs.
  - **Live verification** (the test server's four cards): a copy of
    `gpu-test-4b` with a set context and `--tensor-split 1,1`, its GPU pin two cards (say cards 1
    and 3): its container passes those two by UUID with `CUDA_DEVICE_ORDER=PCI_BUS_ID` and
    `lmgw.own-split=kept`, its command has `--tensor-split 1,1` as written and no `--split-mode
    layer` or `--fit off` of lmgw's, `nvidia-smi` shows its process on those two cards only, and
    each card's figure is within the margin of its part in `lmgw.placement` (expect the first
    card's compute buffer above the estimate, as for WP6); a restart of lmgw adopts it. The same
    copy with its pin cleared: it starts on one card without `--tensor-split` and its warning
    says it needs a pin of 2 cards; the editor's line says the same. An embedding row with `-ts
    1,1` in its arguments starts on one card without it and warns that aux models run on one
    card.

- **WP12 — Fit marks for splits, and Get it asks (owner, 2026-11-17).** The finder
  (`lmgw__model_find`, "Find a model") and the Hugging Face file list show two signals per file,
  judged at the context Get it would create (shown beside them): **Min. cards** — the fewest cards
  whose total memory holds it, "2 of 4"; **Free VRAM** — on the cards it would use: ✓ fits now,
  ✓ after unloading lmgw's idle models (named), ✗ with need against free ("needs 21 GiB, card 2
  has 18"). Shown as badges with icons and card chips, per-card figures on expand, in the existing
  style, not as plain strings. Cases: a one-card machine hides Min. cards; larger than all cards
  together; aux, audio and image models never split ("fits only split; this kind runs on one
  card"); an engine that cannot split (an image without `--tensor-split`/`-devd`, a card not
  NVIDIA's) says why; a mixture-of-experts model shows a third signal "with experts in RAM: N
  cards" (also beside a split, and as a Get it choice: a common setup on a one-card desktop);
  an unknown size shows "?" with the reason; a declared budget shows "budget ✓/✗" in place of
  Min. cards. The finder's pick stays the best one-card file. Get it on a file that fits only
  split asks, with numbers: split over cards X and Y at the full context with the GiB per card,
  one card at a smaller context with the tokens it would get, and with experts in RAM where that
  applies; the MCP tools carry the same as fields, and `model_get` takes the choice and answers
  with the choices when none is given. Where lmgw cannot decide, it shows what is needed and the
  user decides. **Verification:** tests per case; the finder and file list looked at in a browser.
  *Built 2026-11-17* (not run on the test server yet). Readings of the bullet against the code:
  - **Where it lives.** `FileFit.cards` (`CardsFit`, `lmgw-api-types` `model_find/cards.rs`):
    every fit by either measure carries its fit over the cards, judged at the context Get it
    would create with that measure, so `hf_repo`'s marks, `local_model_plan`'s fit, the
    finder's card (`model_find`, by size) and `model_get` read one thing. The cards come from
    one more ledger read beside the one-card one (`CardNow.cards`, `VramScheduler::cards_now`,
    `vram/place/marks.rs`): every card the computer lists, as a new chat model of the class
    finds them — lmgw's models on each as holders, its free memory less other starts' claims,
    headroom, margin and engine cost. None under a declared budget or without a device
    reading. The words are `modelinfo/cards_fit.rs`.
  - **Min. cards** is the file at its full context (the plan's): one card where admission's
    "too large" rule says a card holds it (`Card::could_hold`: its footprint, the engine's
    cost and what it keeps free with nothing else of lmgw's there, the largest such card),
    else the fewest cards WP6's `split::fewest` fills with rooms of each card's total less
    the same; "2 of 4". Not as a count of totals, but by the split lmgw would make, so the
    count is what a start would need. A model whose header gives no layers (an aux model)
    is counted by its size over the roomiest cards ("about"). Larger than every card (up to
    16): "too large", a cross; a "!" where one card holds a shorter context of it, and the
    sentence says how many tokens. One card: hidden (`one_card`, no label). A budget:
    "budget" with a tick or a cross (its footprint and the headroom against the budget), and
    no split is judged (lmgw does not split under one). Where a split holds it and lmgw
    cannot split it here, "!" with "Fits only split at N tokens; …": "this kind runs on one
    card" (aux), "lmgw cannot split it here: …" — the chat class's run arguments not passing
    the cards through CDI, a card not NVIDIA's, the native launcher, the class image's
    llama-server without `--tensor-split` or not read (it is read once for the marks where
    the image is here and several cards are: `read_class_vocab`), a drafter without `-devd`.
  - **Free VRAM** asks `place` (Mode::Owner) for the footprint at the judged context, with
    the split's cost where lmgw may split it: now ("fits now", a tick, the card or the
    split's cards with their parts), `Evict` or a split's victims ("after unloading
    chat/q", or "after unloading 3 models", a "!", every name in the sentence and in
    `unload`), `Wait` ("after busy models finish", a "!"), and otherwise short: a cross,
    "needs 21 GiB, card 2 has 18", needs being its footprint with the engine's cost and
    margin, has the roomiest card's free memory with every model of lmgw's there gone (the
    sentence says how much of that is lmgw's); for a split the roomiest as many cards as
    Min. cards counts, together. Under a budget it is the pooled room (now, once lmgw's
    models unload, or short).
  - **The context.** The full context where Get it asks; else the one-card fit's own: a
    `fits_shorter` fit's shorter context (what Get it creates today), the full one otherwise.
    Shown beside the badges ("at 262 144 tokens"). Where the context is shorter because no
    card holds the full one, a note says so and why.
  - **Experts in RAM** (`experts_in_ram`): a chat model with routed experts (the header's
    `_exps` tensors or `expert_used_count`) that no single card holds at its full context is
    sized again with `n_cpu_moe` over every layer; its Min. cards ("with experts in RAM: 1
    card", or 2 where the rest still needs a split lmgw can make), the bytes in RAM, its parts
    and its own Free VRAM. Not shown for a model one card holds whole.
  - **Get it asks** (`choices`) where no single card holds the full context and a split lmgw
    can make, or the experts in RAM, does: `split` (the cards it would use now where they
    have room, else Min. cards' cards, with GiB and layers on each), `experts_in_ram`, and
    `one_card` at the one-card fit's shorter context where one exists; each with its own
    Free VRAM and a sentence ("Split over cards 2 and 3 at 262 144 tokens: 21.0 GiB on card
    2, 5.6 GiB on card 3. There is room now."). None is chosen. A file one card holds asks
    nothing and is set up as before.
  - **`model_get`** takes `layout` (`one_card | split | experts_in_ram`). Without it, where
    the fit by the get's measure asks, it answers `choices` (and the model id it would use),
    queues nothing and starts no job; the op now plans the file from the hub to tell (a
    header read, 1 to 16 MiB, nothing where the finder or `hf_repo` read it already). A
    layout of `experts_in_ram` for a header without experts, and `split` or `experts_in_ram`
    for an aux get, are refused before anything is downloaded. The layout is kept on the
    get (migration 0112, `model_gets.layout`) and its result (`ModelGetDone.layout`); the
    job's `refit::for_get` then keeps the plan's full context with no fit note for `split`
    (the row sets its context, so WP6 splits it where no card holds it), and for
    `experts_in_ram` also sets `n_cpu_moe` to the layer count, with the fit and the plan's
    warning of that. `one_card` is the get as before.
  - **The UI.** `widgets/fit_signals.rs`: chips in the existing `.chip` tones (ok, warn,
    err, off) with the kit's tick and cross icons (the dashboard font lacks "✓"), "!" and
    "?" as glyphs; the finder's card shows them under the uses line with the context, and a
    **Per card** fold with each signal's sentence and each card's figures (bytes of total,
    free, layers); the Hugging Face file list shows them compact beside each file's mark
    ("2 of 4 cards", "fits now"), the sentences and figures in the tooltip. Get it on a
    file whose shown fit asks opens the choices in the card (`.mf-ask` with the `.mf-choices`
    rows, each with its Free VRAM chip), and "Not now"; a choice starts the get with its
    layout. A server answer with choices opens the same. The pick is unchanged: the best
    one-card file. A row with badges wraps them under the file's name rather than cut the
    name. *Looked at in a browser* (headless Chromium against a dev lmgw in the build
    container, `model_find` and `hf_repo` answered by hand-written answers for the four
    cards of §1 — the badges, the fold and the ask as drawn, not the test server's own
    figures, which the tests hold).
  - **The MCP side**: `lmgw__model_get`'s description and its `layout` argument,
    `lmgw__hf_repo`'s and `lmgw__model_find`'s descriptions name `cards`, and the admin
    instructions' step 3 says to show the person the choices and call again; the API
    reference takes `layout` and the new fields from the DTOs and the tool's prose (the
    existing merge).
  - **Tests**: `vram/place/marks/tests.rs` (one card holds it, the fewest cards of a split,
    too large, by size, Free VRAM now, after unloading, waiting, a split now, short on one
    card and over a split's cards), `modelinfo/cards_fit/tests.rs` (one card of four and no
    ask, a split asks with GiB per card, an engine that cannot split says why, an aux model
    runs on one card, too large, one card hides Min. cards, a mixture of experts with its
    experts in RAM on two cards and on one, the models named that unload, a budget, an
    unknown size), `widgets/fit_signals.rs` (a card's line, the row's tooltip), and
    `it/model_get/layout.rs` (on two cards a file only a split holds is answered with
    choices and nothing is downloaded, the plan's fit says "2 of 2", and `layout: split`
    creates the row at the full context; `experts_in_ram` for a model without experts is
    refused).
  - *Confirmed by the owner 2026-11-17*: `model_get` reads a file's header before it answers,
    so it knows whether to ask, also where the header was not read yet (a wait of 1–16 MiB on a
    slow line), on every machine, provided the UI shows what is going on.
    The finder shows a travelling bar and "Reading the start of the file (a few MiB) to see
    which graphics cards hold it, then queueing the download…" while it does (5d67fa11).
  - **What WP12 leaves**: a split smaller than the full context ("Not yet"), so a file that
    a split holds only at a shorter context is offered one card; the experts in RAM in part
    (some layers' experts): only every layer's is judged and offered; Free VRAM's "has" for a
    split adds the roomiest cards' room, not the split lmgw would make of it; the cards for
    the marks are read on a second ledger read beside the one-card one, which can differ by
    what changed between the two; the file list's tooltips are the figures per card (no fold
    in a row); the Models page's own fit marks (§10's "the model list") and "Fit again" are
    unchanged.
  - **Live verification** (the test server's four cards): Find a model with
    a model that only a split holds at its full context (`gpu-test-4b`'s repo at 262 144
    tokens did on 2026-11-17) shows "Min. cards 2 of 4" and Free VRAM with the cards it
    would use; the file list's row the same; Get it asks with the split's GiB per card and
    one card's tokens; choosing the split creates a row at the full context that starts
    over those cards (WP6). A mixture-of-experts file shows "with experts in RAM: 1 card"
    and offers it; `lmgw__model_get` without `layout` answers the same choices.

- **WP13 — Headroom as a floor and learned room, and an amount per card (owner, 2026-11-17,
  after a review).** Why: WP6's live split left 274 MiB free on one card of the test
  server, and the learned figure of §9.1 rests on a quiet window (5–7 MiB of outside growth per
  card over a quiet stretch of hours) that would have taken over at its 24th hour. The test server runs on fixed mode,
  1 GiB per card, until this is built. Read in the code before WP13 (to be checked by it): the
  learned figure is only kept while another lmgw model is on the card, a lone row fitted to its
  card keeps only the start margin (`rules_on` in `place/choose.rs`, `split_now` in
  `place.rs`); nothing reacts to outside growth between requests; the whole largest rise is kept
  free on top of the current reading, so a program that restarts once teaches its whole size.
  - **A — automatic headroom is a floor plus learned room**, per card:
    - **Floor:** `headroom_mb` (default 1 GiB) is always kept free, as fixed mode keeps it
      today, capped at the most the other programs ever held on that card. A card nobody else
      uses keeps nothing; a display server's few MiB do not cost 1 GiB.
    - **Learned room**, on top: the highest level the other programs reached on that card in
      the last 7 days, less what they hold now (not the largest rise on top of the current
      reading). It applies from the first reading, since it only adds to the floor: the 24-hour
      switch-over (`HOURS_NEEDED`) goes.
    - **Learned room is given up last.** A card's room is counted net of its whole reserve
      first, and net of the floor alone only when nothing else fits: a split keeps it on a
      filled card while its other cards have room, and a single model evicts an idle model on
      another card before taking it. Today both take it first. A learned figure never refuses
      a model that fits beside the floor (ruling of 2026-11-11).
    - **The start margin goes on top in every mode** (WP6's split took 239 MiB more than
      planned, out of the room meant for others).
    - **Shown** per card in Settings → GPU as a sentence, e.g. "1.0 GiB is always kept free for
      other programs. They have used up to 41.2 GiB here (2026-11-16) and use 21.9 GiB
      now: lmgw keeps the other 19.3 GiB free while it can make room another way." The
      "Learned growth not kept free" warning stays. Fill-the-roomiest-card-first and the
      tightest fit (D7) are unchanged.
  - **C — an amount declared per card:** Settings → GPU, per card, "Keep free for other
    programs: Automatic | Always N GiB | Nothing, this card is lmgw's". "Always" is kept
    without exception (lmgw waits, places elsewhere or falls back); "Nothing" keeps no floor and
    no learned room. Settings suggests an amount from what was seen ("other programs used up to
    4.1 GiB more than now, 3 times in 7 days: always keep it free?").
  - **Not in WP13:** the yield (idle models stopped when other programs grow, between
    requests) is its own package, sized from a four-card test server's per-second log of 2026-11-17/18; not designed around games, where the GPU hold is
    the answer (owner). Percentiles, time-of-day profiles, rules per program and extra decay
    are not built. With the yield, revisit whether a model moved to make room (§6.2) may move
    into another card's learned room (owner, 2026-11-18: today it asks `place` as an owner's
    start and is stopped instead). That becomes the better choice only when all of these
    hold: a watcher reads the cards between requests; lmgw stops idle models sitting in
    learned room when other programs grow; the per-second log of 2026-11-17/18 shows the
    outside growth per (watch interval + stop time) below the floor; and a rule exists for
    such a model that is busy when the room is needed (the lean: abort the request, the
    fallback answers). If the log shows big single jumps, it stays off on shared cards.
  - **Verification:** a card nobody else uses keeps nothing; a shared card keeps the floor from
    its first reading; with outside use below its 7-day high, a split puts its layers on another
    card before it takes the learned room, and takes it when nothing else fits; "Always N"
    makes a model that would need that room wait or go elsewhere; "Nothing" fills the card; the
    sentence per card and the suggestion read as above.
  *Built 2026-11-17* (not run on the test server yet). Readings of the bullet against the code before
  building, all confirmed:
  - **The learned figure only while another lmgw model is on the card.** `rules_on`
    (`place/choose.rs`) gave a learned card (headroom = rise + margin, margin alone); `Card::keep`
    (`place.rs`) kept the headroom only while holders were left; step 1's `now(false)` took the
    margin alone on a card without holders before step 2 evicted anywhere, and `split_now` kept
    the margin alone on every card without holders. So a lone row, a single model on an empty
    card and a split's empty cards all took the learned figure first.
  - **Nothing reacts to outside growth between requests**: only `growth::sample` records it.
  - **The whole largest rise on top of the current reading**: `Growth::largest` (the most a
    reading exceeded any earlier one), kept free beside the driver's free, which already has the
    current reading out. A program that restarted once taught its whole size.

  What was built:
  - **A, per card** (`vram/headroom.rs`, `CardHeadroom`): `floor` (auto: `headroom_mb`, capped at
    the window's highest reading, `Growth::most`; the whole `headroom_mb` without readings),
    `room` (auto: that highest less the newest reading, `recent`; 0 elsewhere), `bytes` (both),
    `structural` (auto: at rest and the floor; else the floor alone). `HOURS_NEEDED` and
    `Growth::learned`/`rise`/`largest` are gone; the stored hours are unchanged (no migration),
    `rise_bytes` is still written and no longer read. `rules_on` answers (floor + room + margin,
    floor + margin, structural + margin) on every card in every mode, so `Card.margin` is now
    "what is never given up". `margin()` (the ledger's charge of a starting or unmeasured entry)
    rides in every mode; `start_headroom`, `start_margin` and `card_headroom` are the cards' sums
    of the same with the margin once.
  - **Given up last** (`place.rs`, `Keep`): steps 1 and 2 are asked with every card keeping its
    whole headroom (`Keep::Whole`), then again with §5.6's rule (`Keep::Left`: the whole while
    something of lmgw's is left on the card, the floor and margin once nothing is); steps 3 and
    4 likewise. A row's own split (`own_split::fixed`), the fit marks and the wait (step 5) keep
    §5.6's rule. The queue, the climb and the background start were unchanged: their
    "headroom while others hold, margin once none do" (`room_on`) already gives the learned
    room up only once nothing of lmgw's is left, now beside the floor. The warning
    (`GROWTH_LET_GO`, unchanged lead) says the room taken and on which card other programs held
    their most, and when.
  - **C** (`vram.keep_free`, `config::VramSettings`): a map in the settings blob by card id,
    `KeepFree { mode: automatic | always | nothing, mb }` (`lmgw-api-types/settings.rs`); a card
    not listed is automatic. Always: floor = structural = N, no room; nothing: 0, 0, 0 (the
    margin still rides). Automatic follows `headroom_mode` (fixed keeps `headroom_mb`, now with
    the margin on top). `VramScheduler::merge_keep_free` merges a patch (only the cards given;
    automatic takes the entry out; always needs ≥ 1 MiB; a card set to always or nothing must
    be listed or have readings, `known_cards`), from the dashboard's save
    (`VramSettingsPatch.keep_free`) and from `settings_set` / `lmgw__settings_set` (`keep_free`,
    a JSON-encoded object for the tool, described in the catalog); `lmgw__settings` reads it
    back as `vram.keep_free` and per card in `vram.headroom.cards[].keep`.
  - **Shown** (`HeadroomNow`, `HeadroomCard`): `floor_bytes`, `room_bytes`, `most_bytes`,
    `most_at`, `now_bytes`, `keep`, `suggest`; `learned_in_use`, `learned_bytes`, `learned_at`
    and `hours_needed` are gone. The card's sentence reads as the bullet's example; the machine's
    says how auto works, which cards keep an amount of their own, and the margin. The suggestion
    (`KeepFreeSuggestion`): on an automatic card in auto mode where other programs held more than
    now and the floor together at least once (`Growth::times_above`: runs of hours in a row above
    that level), the most less now in whole MiB, "Other programs used up to 4.1 GiB more than
    now, 3 times in 7 days: always keep it free?". Settings → GPU: "Floor per card" (was "Fixed
    headroom per card"), and "Each card" (`settings/headroom.rs`, `Ctl::HeadroomCards`, the
    draft's `vram.keep_free` as one `Raw` value): per card its sentence, "Keep free for other
    programs" (Automatic | Always [N GiB] | Nothing, this card is lmgw's), the suggestion with
    "Always keep N GiB free", and Forget…; then "Forget all readings…". A forget leaves the card
    on the whole `headroom_mb` and no room until its next reading (op description, confirmation,
    audit words).
  - **An image's partial grant keeps no margin for its context** (`partial::Room::
    beyond_headroom_for`): the image class's margin is the stand-in for a context not measured
    yet, and a partial grant sets that context aside itself, as the headroom module says ("none
    for a partial start"); the grant's room was taken net of both, which only showed once auto
    had learned and would have shown in every mode with the margin always on top (a 2.5 GiB
    budget became 1.5 GiB in `image_partial_fit`). Now a partial grant's room is net of the
    floor and the context once, as fixed mode had it.
  - **Tests**: `headroom/tests.rs` (a card nobody else uses keeps nothing, a display server's
    MiB, the floor from the first reading, no readings; the room as most less now with the
    design's sentence and the warning; fixed with the margin on top; always and nothing; the
    suggestion; the merge and its refusals; no per-process figures; the audit words),
    `growth.rs` (the most, runs above a level), `place/tests.rs` (a model evicts on another card
    before taking an empty card's room, and takes it with nothing to evict; a split takes a
    third card before the room, takes it with two; one card holding it beside its floor is not
    split instead; three split tests now keep their cards' 1 GiB of room); `it`:
    `headroom_cards` (floor from the first reading, room once the outside use drops, forget),
    `llama_fit_headroom`, `llama_fit_review`, `audit/writes` adapted; settings UI's draft
    helpers. The margin on top in every mode moved figures the `vram_admission` suite had sized
    for `headroom_mb` standing in for it: its MiB-scale cards (`llama_fit`, `llama_fit_review`,
    `placing`, `placing_own_splits`, `placing_pins`, `placing_splits`) are `LLAMA_MARGIN` larger,
    which leaves each card's room for the model as it was; `admission`, `outside_vram_fallback`
    and `card_charges` expect the margin in what a start needs and in what is kept free until a
    first request; the audio tests expect an unlearned row's margin (as much as its estimate)
    in what is pending; the realtime budget's speech model is 3 GiB instead of 5 on its 8 GiB
    cards; the image tests run a chat request on the resident chat model (a model in use keeps
    no start margin) and read "1.5 GiB headroom" (the floor and the image margin) in a whole
    start's message. Outside it: `background_admission` reads the margin in what a guest
    start needs, `chat_voice_warm_group`'s three stages and their margins get an 11 GiB card
    instead of 10, `model_get::layout`'s two cards are each a margin larger, `self_admin_wire`
    and `lmgw-api-types` read `vram.keep_free`.

  Judgement calls:
  - **A model one card holds by giving up its learned room is not split instead.** The bullet's
    "net of the floor alone only when nothing else fits" could be read to prefer a split that
    keeps the room; that trades a model's speed (and an engine's cost per extra card) for the
    room, which neither the bullet nor D6 names. Built: one card (whole, after evicting, beside
    the floor) before any split; the split itself keeps the room first.
  - **The amount lives in the settings blob, not a table**: it is a setting the owner declares,
    saved with the page and patched like `headroom_mb`; no migration (0113 is not taken).
  - **Too large on an "always" or "nothing" card**, and in fixed mode, does not read what other
    programs hold at rest, as fixed mode did not before.
  - **The suggestion's amount** is the room alone (the bullet's figure), not the room and the
    floor; choosing it replaces the floor too.
  - **The margin in every mode is every class's margin**, as auto had it once learned: a chat or
    embedding model 327 MiB, an image model whose context is not measured 1.0 GiB, a voice
    model whose residency is not learned as much as its estimate (at least 0.78 GiB). In fixed
    mode, where `headroom_mb` stood in for all of them, a voice model is now judged against
    twice its files until its first request has taught its residency (a 5 GiB one no longer
    fits an 8 GiB card until then), and an image model that starts whole keeps 1.0 GiB more.
    The bullet's reason (WP6's split) is a llama start; whether the image and voice margins
    should ride in fixed mode too was settled by the change of 2026-11-17 below.

  *Changed 2026-11-17 (owner)*, after the judgement calls above:
  - **The start margin is given up last, after the learned room.** Before, every class's margin
    (327 MiB for chat and aux, the 1.0 GiB context stand-in for an image model, a voice model's
    whole estimate until its residency is learned) rode on top in every pass and in the
    too-large test, so a voice model whose files fit a card but whose files twice over do not
    was refused, never got its first request and never had its residency learned (auto mode
    had this before WP13). Now the margin is kept while lmgw can make room another way and
    given up only where nothing else fits; the floor, or a card's "Always N", never is; the
    too-large test counts the floor (or N, or nothing) and what other programs hold at rest,
    without the margin: a model that fits beside the floor always gets to start. The cost the
    owner accepted: in that tight case an estimate that runs low comes out of the floor.
    Built: `rules_on` answers `Rules { headroom, margin, floor, structural }` (floor + room +
    margin, floor + margin, floor alone, at rest + floor without margin); `Placement.floor`,
    `start_floor`; `card_headroom` without the margin. **Order: after the room.** `place`
    gained a third pass, `Keep::Last` (steps 1 and 2: Whole, then Left — the room given up on
    a card nothing of lmgw's is on — then Last — the margin given up too, the card's new
    `last`; steps 3 and 4 likewise; the wait, step 5, Left then Last). After rather than
    together because the two are not alike: the room is a guess at what other programs may
    take again, the margin what the start itself is likely to take on top of its estimate
    (WP6's split took 239 MiB of its 327); with one pass, D7's tightest fit would put a model
    on a card where it gives its margin up beside another empty card where only learned room
    goes. The queue, the climb and the background start were already "the headroom while
    others hold, the floor and margin once none do"; the margin is given up in their last
    step only. A row that adapts (a llama-server row on `--fit`, an image pipeline that can run
    with less) keeps its margin in `place` (`last` = floor + margin) and is fitted smaller
    first: `partial::decide` gained a last step (`Room.floor`): with nothing of lmgw's left to
    wait for, after the whole and anything smaller beside the margin, the whole (a fitted row's
    target, its least included) goes ahead beside the floor alone — a fallback or not, as the
    room is given up. Every path: `place` steps 1–4 and the wait (step 5, the Last pass); the
    queue and the background start (`room_on`, `partial::decide`); a split (`split_needs`, by
    `Split.last`); the climb (`Plan.real` is the rung and the floor, `Plan.kept` the rung, the
    floor and its margin for the warning; another card for the climb keeps the margin); the
    verdict (§4.7: the least, the engine and the floor) and the fit marks (`marks.rs`: Min.
    cards by `structural`, Free VRAM by `place`); the warm group and realtime budget
    (`in_turn`, as admission); a model moved to make room keeps its margin on its new card
    (`repack`). The image partial grant still takes its room net of the floor and the context
    once (`beyond_headroom_for`, be3cdc45); the warning (`note_growth`) now reads such a grant
    against what it keeps beside it — the headroom less the context it sets aside — so it no
    longer says "Learned growth not kept free" where the room was kept. The ledger's charge of
    a starting entry is unchanged: its estimate and its margin until it is measured.
    **Warnings:** "Learned growth not kept free" says when the margin went too ("…not with
    that room or that margin…, so it starts without either: should other programs use that
    much again, or it take more than its estimate, the GPU can run out of memory"); where
    only the margin went (no room on the card), "Start margin not kept free: VRAM admission
    keeps a margin of 327 MiB beside chat/m while it starts and is not measured yet, … so it
    starts without it: should it take more than its estimate, that comes out of the 1.0 GiB
    kept free for other programs".
  - **The suggested "Always N" includes the floor.** "Always" replaces the floor, so the room
    alone would have lowered what the card keeps. The suggestion is the floor and the room in
    whole MiB, "Other programs used up to 4.1 GiB more than now, 3 times in 7 days: always keep
    5.1 GiB free (the 1.0 GiB floor and that 4.1 GiB)?"; Settings' "Always keep N GiB free"
    takes the same figure (`KeepFreeSuggestion.mb`). Per card, once, never per model.
  - **Tests:** `headroom/tests.rs` (`start_floor`; too large without the margin; the warning
    with the margin given up, alone and with the room, on a card that is lmgw's; `rules_on`
    for automatic, always and nothing; the suggestion's figure is floor + room),
    `partial.rs` (a voice model gives its margin up last, never while something could be
    waited for, never into the floor or an "always" amount; an image pipeline's partial grant
    before the margin is given up), `partial/llama/tests.rs` (a least that fits beside the
    floor alone is not too large and starts last), `place/tests.rs` (the margin is kept while
    another card, an eviction or another empty card's learned room holds the model, given up
    where nothing else does, the floor never; `an_empty_card_keeps_its_floor_alone`, was
    `…_the_margin_alone`); `it`:
    `margin_last` (a voice model whose files fit an 8 GiB card but not twice over starts and
    is measured; a chat model that fits only without its margin starts, warned; the margin
    kept while an idle model can be evicted). Adapted where the figures moved: the refusals
    sized for the margin in the too-large test take cards that much smaller again
    (`llama_fit`'s least too large and explicit row, `llama_fit_review`'s image without
    `--fit`, at 5, 40 and 96 MiB as before WP13); `image_partial_fit`'s pinned pipeline reads
    "plus 512 MiB headroom" (the floor, no stand-in); `outside_vram_fallback`'s verdict needs
    6.5 GiB (the footprint and the floor); `llama_fit_headroom`'s real shortfall has other
    programs hold all but the floor; `headroom/tests.rs`' `card_headroom` and `rules_on`
    figures; the cards of `place/tests.rs`, `own_split.rs` and `marks/tests.rs` gain `last`.
  - **Judgement calls:** (1) after, not together (above); still, as for the
    room, a card that holds a model beside the floor alone is taken before a split that would
    keep the margin (D6: a split only where no card holds it), and a pipeline's partial grant
    or a fitted row's smaller placement comes before the whole beside the floor. (2) A fallback
    does not keep the margin: like the room, the margin is given up whether or not a fallback
    would answer (a voice model behind a remote fallback would otherwise never be measured); a
    row that adapts still never starts with less than its target while a fallback
    would answer. (3) A climb moving to another card keeps its margin there; it gives the
    margin up only on its own cards, at the end. (4) A model moved to make room (§6.2) keeps its
    margin on its new card. (5) The ledger keeps charging a starting entry its margin until it
    is measured: right after a start that gave its margin up, the card can read as over-full
    and other starts wait until it is measured.
  - **Live checks** (test server, fixed mode or auto): a voice model whose files fit a card
    but not twice over starts on its first request (Models page warning "Start margin not kept
    free…", then its residency learned on the next refresh); a chat model that fits a card only
    without its 327 MiB margin starts with the same warning, while an idle model of lmgw's on
    that card or room on another card makes it unload or move instead (no warning); a card set
    to Always N never gets less than N free; Settings → GPU → Each card: card 2's suggestion
    reads "always keep X free (the 1.0 GiB floor and that Y)" and its button sets X.

  What WP13 leaves: the yield (its own package, and with it a move into learned room, "Not in
  WP13"); a card's floor capped by readings of the window
  only (nothing older is kept); the room read against the newest reading, so after a restart,
  before the first reading, it is the most less what they hold at rest.

  **Fixed after the final review** (2026-11-17): a model moved to make room asked `place`
  with each card's `last` the floor alone, so it could land with its margin given up; it now
  asks with `last` the margin (`repack::move_to`), as judgement call 4 says. A warm, boot or
  operator start (`check_background_start`), which never evicts or waits, asked `place` as an
  owner's start, so an idle model to evict on one card won over an empty card that holds the
  model with its learned room or margin given up, and the start was refused on the first;
  it now asks in `Mode::Still`, where nothing of lmgw's is a victim or waited for (steps 2, 4
  and 5 find nothing; the roomiest candidate is the one with the most free memory). `place`'s `Mode::Verdict`, which no decision passed, is removed
  (WP6's note says what the verdict does).

  **Open after the final review** (2026-11-17; recorded, not built):
  - **A split-only row commits to one card that can never hold it.** The queue claims a
    `Placed::Roomiest` answer like any one-card answer and does not ask `place` again
    (`queue.rs`, the claim after "Not now: it commits to these cards"). A row that sets its
    context, does not adapt and is larger than every card, asked for while no split fits now,
    so waits on one card until `queue_timeout_seconds`, although a split would fit once the
    busy model finishes. The fix (no claim where `place` answered `Roomiest` and the row has a
    split) gives up the claim's promise of room for asking again each turn.
  - **A ladder climb's plan sums the cards' rules** (`climb.rs`, `Plan::of`: headroom, margin
    and floor summed over all cards) and `fits_the_card` compares them with one card's
    capacity. WP13's learned room makes the sum larger; it matters on several cards only.
  - **The learned engine cost is one unbounded sample** (`place/engine.rs`): what the last
    model started alone on a card holds there less its estimate. An estimate that is wrong by
    some amount makes that card's engine cost wrong by the same amount, until the next sample.
  - **A split gives up its margin in the last pass** (`Keep::Last` in steps 3 and 4), although
    what a split adds to the compute buffers is knowingly not in its estimate (WP6 measured
    239 MiB more). On a card whose floor is about 0, or set to Nothing, such a split can fail
    at load.
  - **Whether a model moved to make room may take another card's learned room.** A move asks
    `place` as an owner's start and takes only a `Now` answer, so where evicting an idle model
    on a third card keeps the room, the move is dropped and the model is stopped instead. Asking
    in `Mode::Still` would move it into a card's learned room. That keeps the model loaded but
    is the opposite of the order the owner set (evict an idle model before taking learned
    room), so it was built during the review and reverted.

  *Decided by the owner 2026-11-18*, on the list above:
  1. **A split-only row** (option A): where `place` answers the roomiest card for a row that
     lmgw may split, the queue takes no claim, asks `place` again on every pass, evicts and
     drains nothing on that card (evicting there cannot help) and gives the gate back while
     it sleeps. Accepted: nothing is held meanwhile, so a later, smaller start may take the
     room first; a machine that busy is short of resources anyway. Out of time, the refusal
     names what it waited for, the busy or starting models of lmgw's on the cards the split
     needs, not "another process is using that card". Test: two cards, a busy model on one,
     a set-context row that does not adapt and is larger than either card, requested while
     the busy model runs: it starts split once that model finishes.
  2. **A climb's rules per card**: a left-over, to be fixed. A climb reads the rules of the
     card or cards its new rung goes on, as admission reads them (`rules_on`), the
     too-large figure and the verdict's needed figure included; WP13's order and judgement
     call 3 stay (a climb gives the room up, then the margin, on its own cards only, at the
     end; moving to another card keeps the margin). Test: two cards, card 1 with large
     learned room; a climb on card 0 that fits by card 0's own rules goes ahead without
     evicting or waiting.
  3. **The engine cost per card** is the largest of the card's last 5 readings. A reading
     above 2 GiB is not kept: it is logged at info level as that model's estimate looking
     off, naming the model, the card, what it held, the estimate and the difference (no llama.cpp engine
     measured so far took more than 1 GiB). A new migration 0113
     keeps the readings and carries 0110's figure over as each card's first. Settings → GPU
     and the API show one figure per card, the largest kept, with when the newest was taken.
  4. **A split's compute buffers** (option A): modelled in its per-card estimate from WP0's
     measurements (§1.1, the 4B): on a pipeline the first card ×2.2 and every other card
     ×1.7; with `--n-cpu-moe` (no pipeline) ×2.0 and ×1.0; a projector on the first card
     adds what the one-card estimate leaves out, its own buffer and the rise of the model's.
     Factors rounded up. They come from one model and are to be checked on a second in the
     next live split check.
  5. **A card that reads over-full while a model starts**: display only. The ledger keeps
     charging a starting, unmeasured entry its estimate and its margin; the GPU surfaces
     never draw past 100% or a negative free figure, and say where a start not measured yet
     is in the bar.
  6. **Voice models keep giving up their margin** like every class (kept). Later, as a small
     task: measure each audio engine lmgw ships once and use a real residency ratio per
     engine instead of "files + a margin of the files' size" ("Not yet").
  7. **A model moved to make room keeps asking `place` as an owner's start**, so it never
     moves into another card's learned room; it is stopped instead (kept). Revisited with the
     yield package, under the conditions "Not in WP13" names.

  *Built 2026-11-18* (items 1–5, and 8, a bug found the same day; 6 and 7 are recorded above,
  not built):
  - **1. A split-only row** (`queue.rs` `decide`, `VramScheduler::split_wait` in
    `place/choose.rs`, `place::split_once_left`): where `place` answers `Placed::Roomiest`
    for an owner's start of a row that does not adapt and has a split (`Ask.split`, i.e.
    `splits` is ok; a row that adapts keeps its roomiest answer, which is where it is fitted
    smaller), on a machine of several cards, the queue takes no claim, asks `place` again on
    every pass, does not drain or evict, and gives the gate back while it sleeps, as a claimed
    wait does (an unclaimed wait keeps the gate). The cards it waits on are the fewest a split
    would go on once every model of lmgw's on them, busy ones too, has left (with the learned
    room given up, then the margin), else every candidate; its phase and stage name them. Out
    of time it says "no card holds it whole, and the split it needs over … waited for
    chat/m (…, 1 in flight) to leave room there" (`describe_split_holders`), or that nothing
    of lmgw's is there. Tests: `place/tests.rs`
    (`a_model_only_a_split_holds_waits_for_the_split_not_the_roomiest_card`); `it`
    `placing_splits::a_model_only_a_split_holds_starts_split_once_the_busy_model_finishes`
    (two 48 MiB cards, a request held open on the chat model on card 0, a set-context row at
    `--n-gpu-layers 999` larger than either card: out of time it names `chat/chat-model` and
    both cards, never "another process"; asked again it starts over both cards once the hold
    ends).
  - **2. A climb's rules per card** (`climb.rs` `Plan::of`, `structural_on`, `fits_the_card`,
    `climb_total`; `VramScheduler::rules_over` in `place/choose.rs`, which `placement_on` now
    uses too): the plan's `needs`, `kept`, `real` and `structural` are each card's
    `rules_on` summed over the running rung's cards, where the new rung goes (the pooled sums
    only where no device read names a card), so the wait, the warning, `fits_the_card` and
    §4.7's verdict (`plan.real`) read that card's rules; the too-large test reads the card
    `climb_total` names (the largest card the rung may go on). The order is WP13's, and moving
    to another card (`climb_elsewhere`) keeps its margin as before. Fixed on the way, a path
    that lagged the same rule: a guest's climb (`climb_denied`, `admit_climb_background`)
    compared its needs with the free memory of every card together; it now reads the running
    rung's cards, as the owner's climb does. Test: `it`
    `headroom_cards::a_climb_reads_its_own_cards_rules_not_every_cards` (card 1 with 8 GiB of
    learned room, card 0 none; the ladder's 8 GiB rung on card 0 starts without evicting the
    idle embedding model there; before, card 1's room was summed in and it was evicted).
  - **3. The engine cost per card** (`place/engine.rs`, `store/vram_engine.rs`, migration
    `0113_vram_engine_readings.sql`): `Readings`, the last `KEPT` = 5 readings per card in
    memory and in `vram_engine_readings` (0110's `vram_engine_cost` row carried over as each
    card's first reading, then that table dropped); a card is charged the largest, and
    Settings → GPU and `lmgw__settings` show it with the newest reading's time
    (`engine_learned_at`, the sentence "learned per card after a start on one card as the
    largest of its last 5 readings: … (learned, newest reading …)"). A reading above `MOST` =
    2 GiB is not kept and is logged at info: "chat/m: its estimate looks off on <card>: its
    processes hold X, lmgw's estimate of its buffers there is Y, Z apart — …". Tests:
    `engine.rs` (a low reading does not lower the figure, a sixth drops the oldest, over 2 GiB
    is the estimate being off, with the log's words), `it` `migrations::vram_engine_readings`
    (0110's figure carried over; a lower reading keeps the figure; five more drop it; 0110's
    table gone); `placing::the_engine_cost_is_charged_and_learned_per_card` unchanged.
  - **4. A split's compute buffers** (`plan/chat/by_layer.rs` `ByLayer::compute_on`,
    `computes`; used by `on_cards`, `by_entries`, `split::over` and the fit marks, so `place`,
    the reservation, the warnings and Min. cards see the same figures): on two or more cards
    the main context's buffer is the one-card figure times 2.21 on the first card and 1.73 on
    every other (WP0: 205 and 160 against 93 MiB), and for a row with tensor overrides, which
    llama.cpp runs without a pipeline (`--n-cpu-moe` measured), 2.02 and 1.00 (185 and 92
    against 92); each the measured ratio rounded up to the hundredth. A projector
    (`ChatFiles.projector`, one of the companions not the draft model): the one-card estimate
    charges its weights only and leaves its own compute buffer to the margin, so a split's
    first card gets its model buffer times 1.48 more (302 against 205 MiB) and 322 MiB for
    the projector's own. One card is unchanged. A row's own split (`by_entries`) gets the same
    on the cards that reserve a buffer. The start's note now says each card's part counts the
    larger compute buffers "by factors measured on one model". **The factors come from one
    model each (the 4B, OLMoE for `--n-cpu-moe`, Qwen3-VL-4B for the projector) and are to be
    checked on a second model in the next live split check.** For the live split of
    2026-11-17 (+239 and +137 MiB over plan), the 4B-shaped header at 262,144 tokens and four
    slots now plans +170 and +102 MiB on the first and second card (a one-card buffer of
    141 MiB). Tests: `by_layer.rs` (the factors against WP0's figures, the projector, one card
    unchanged; the existing tests at the larger first card), `fit/tests.rs`
    (`a_split_of_the_4b_plans_most_of_what_the_live_split_held_over`), `chat/tests.rs` (the
    parts add up to the footprint less one buffer plus the split's), and the split tests of
    `place/tests.rs` and `own_split.rs` at the new figures (one split fills card 0 with 24
    layers instead of 25).
  - **5. A card that reads over-full** (`lmgw-ui` `gpu_figures.rs`, `traffic/gpu.rs`): the
    surfaces already clamp: a card's bar cuts every part at the card's total, the machine's
    bar cuts the kept-free part at what is not free and both at 100%, and every free figure
    is the driver's or the ledger's, never below zero. What a start not measured yet is
    charged is in the machine bar's kept-free part, now said as "… kept free for models
    starting, not measured yet" (was "models starting"), and that line is the part's tooltip
    on the Traffic page (was "kept free by admission"). The ledger's charge is unchanged.
    Test: `gpu_figures.rs` (`a_start_not_measured_yet_is_said_and_never_overfills_the_bar`).
  - **8. A bug found on a one-card machine** (2026-11-18, a GTX 970 of 4096 MiB, "Always
    512 MiB" on it, 640 MiB of engine learned; `qwen3-0.6b` at 40,960 tokens): Fit again
    answered "already takes the largest conversation that fits" while its own cards said the
    card was short, and `local_model_test` then refused the row as too large ("needs 3.1 GiB
    … plus 1.1 GiB headroom … holds 4.0 GiB"). The fit searched the context against the
    card less the learned room and floor (`headroom(snap).bytes`), without the engine's cost
    and without the card's own rules. Now `OnCardNow` (`vram/beside.rs`) carries what
    admission keeps beside a llama.cpp start, from `VramScheduler::llama_rules` (`rules_on`'s
    rules with the llama.cpp margin, `engine_on`; the pooled sums where no device read names a
    card): on the whole card the engine's cost and `Rules::structural` (the too-large test,
    no margin); beside what runs now the engine's cost and the floor alone where nothing of
    lmgw's is on the card (what admission starts it with at the last), the engine's cost and
    the whole headroom with the margin beside lmgw's models, which stay (`CardNow::keep`).
    Fit again, Get it's fit, the plan's warning (`plan_fit::Card`, the whole card) and the
    finder's order (`hub_fit` bounds) search against it, so the context they land on is one
    admission starts; `headroom_bytes` on a fit, and the "headroom" the plan's warning names,
    now include the engine's cost, as admission's own refusal counts it ("plus 1.1 GiB
    headroom"). Min. cards' too-large sentence named the model's bytes against
    the card's total ("it needs 3.1 GiB, and the card hold 4.0 GiB"): it now says "it needs
    3.1 GiB plus 1.1 GiB kept free beside it (the engine's own cost and what the card keeps
    free for other programs), and the card holds 4.0 GiB" (`Min::TooLarge.kept`). The KV
    term: Fit again sized a chat row from its fields alone, cache types included, but not
    from its own llama-server arguments, which admission reads too (`--cache-type-k` and
    `-ctk`, `--n-cpu-moe`, `--parallel` …); it now sizes it with them (`Sizer::chat_row`).
    The figure reported, 2,495,610,880 bytes, is the q8_0 cache of Qwen3-0.6B at 40,960
    tokens (28 layers × 8 KV heads × 128 × 2 × 8.5 bits), not f16's 4,697,620,480, so the
    row had q8_0 in its fields there already. Tests: `it`
    `vram_admission::fit_again_card` (one 64 MiB card, Always 8 MiB, 6 MiB of engine learned:
    the fit keeps 14 MiB, its sentence and the cards at that context agree, and saved, the row
    starts at that context; q8_0 sizes the KV cache at about half and fits a longer context),
    `cards_fit/tests.rs` (the too-large sentence). Trade-off: beside lmgw's models the fit
    now keeps the learned room and the margin too, as admission does before it evicts them;
    a "beside what runs now" figure is accordingly smaller on a card with lmgw's models on it.
  - **Follow-ups of 2026-11-18 (owner):**
    - **Item 8's budget path.** Under a declared budget over several cards the fit still read
      the pooled sums of every card's rules, without the engine's cost, against the whole
      budget, while admission places such a model on one card by that card's rules and engine
      cost. `on_card_now` now takes the roomiest card there too (`roomiest_card`): its total,
      no more than the budget; what is not lmgw's to free there counts what the budget leaves
      beside everything in use on every card, as `Ledger::capacity_on` reads a card; and that
      card's `llama_rules`. One card under a budget holds no more than the budget either. Only
      a budget without a device read keeps the pooled sums (admission has no card there).
      Test: `it` `fit_again_card::under_a_budget_over_several_cards_fit_again_reads_one_card`.
      The fit over the cards (WP12's marks) followed (the owner's ruling, 2026-11-18: the marks
      must say what admission does, since any other reading would confuse): `cards_now` reads the cards under a budget too, each as admission
      reads it (`cards_for_place`, `Ledger::capacity_on`: its total no more than the budget,
      its free memory no more than the budget leaves beside everything in use, its own rules
      and engine cost), so Min. cards, Free VRAM, Get it's split-or-one-card question and
      `model_get`'s choices name what admission does on that machine; only a budget without a
      device read says "budget" on the pooled room, and there lmgw cannot split. Tests: `it`
      `fit_again_card::under_a_budget_the_marks_read_the_cards_admission_reads` (two 100 MiB
      cards under 150 MiB: "2 of 2", fits now over both, Get it offers the split, and
      admission splits it over the same two cards) and
      `one_card_under_a_larger_budget_holds_its_own_total` (64 MiB under 200 MiB). Found
      while doing so, not changed: under a budget over several cards admission judges a split
      card by card, each card's free memory capped at what the budget leaves (`capacity_on`),
      never their sum against it, so a split's parts together can take more than the budget
      leaves; the marks now say the same as admission there.
    - **When a split runs as a pipeline**, read in llama.cpp's source
      (`src/llama-context.cpp` at ggml-org/llama.cpp master 97069ff6, 2026-11-18, lines
      584–590): more than one device, `n_gpu_layers > n_layer`, split mode `layer`,
      `offload_kqv`, no tensor overrides and no MoE cache, and (lines 593–608) every device
      with async compute and events. Item 4 had turned the pipeline's factors off for any
      tensor override only. Now `Parts::pipeline` (`plan/chat.rs`) follows what the row
      decides: every layer on the cards (`--n-gpu-layers` unset or above the layer count),
      no `--no-kv-offload`, no tensor override and no `--moe-cache-mib`; anything else (a
      device without async compute or events, llama.cpp retrying without the pipeline when a
      buffer fails to allocate, line 866) is charged the pipeline's factors, the larger.
      Test: `plan/chat/tests.rs` (`a_split_runs_as_a_pipeline_where_llama_cpp_would`).
  - **After the live checks of 2026-11-18 (owner):**
    - **The split's compute buffers recalibrated** (`by_layer.rs`; the owner's ruling, 2026-11-18: wrong
      factors get corrected numbers, the rule stays). Three live splits at 262,144 tokens
      (Qwen3-VL-4B Q8_0 with its F16 projector, the 4B Q4_K_M, Qwen3.6-35B-A3B Q8_0 with
      `--n-cpu-moe 10` and its projector) and an own split of the 4B at 65,536 showed item 4's
      factors over-planning the first card by 0.8–2.2 GiB. lmgw's own one-card figure C for
      each, read from the published headers at those settings: 1332, 1332, 1088 (of it 272 the
      op-offload copy of a host-kept expert tensor) and 372 MiB. The model is now `a·C + b`:
      on a pipeline 1.58·C + 64 MiB on the first card and 1.58·C + 16 on the others, a
      projector 100 MiB more on the first and its own buffer; without a pipeline C + 32 on the
      first card and C less the op-offload copy (which lives on the first card alone) + 32 on
      the others. The projector's own buffer stays 322 MiB, the larger measured (322.49 for
      Qwen3-VL-4B's, 248.10 for Qwen3.6's): no header carries the image limits llama.cpp sizes
      it from. Planned against measured (MiB, the projector's own buffer apart; compute only):
      | case | C | first: planned / real | others: planned / real |
      |---|---|---|---|
      | WP0 4B, 16,384 | 93 | 211 / 205 | 163 / 160 |
      | WP0 VL, 16,384 | 93 | 311 / 302 | 163 / 160 |
      | A, VL-4B, 262,144 | 1332 | 2269 / 2252.08 | 2121 / 2110.06 |
      | B, 4B, 262,144 | 1332 | 2169 / 2155.05 | 2121 / 2110.04 |
      | C, 35B-A3B, `--n-cpu-moe` | 1088 | 1120 / 1076.00 | 848 / 827.55 |
      | D, own split, 65,536 | 372 | 652 / 607.05 | 604 / 562.04 |
      Per card in all (the plan's weights, KV cache and recurrent state matched llama.cpp's
      buffer lines to the MiB in every case; engine apart), planned against measured: A
      17,606 / 17,592 on its first card (was 19,694) and 11,568 / 11,553 on the other (was
      11,752); B 18,393 / 18,382 (was 19,168) and 7,861 / 7,846 (was 8,045); C 3,292 / 3,174
      on its first card (was 5,425: the 2.2 GiB was the compute factors — ×2.02 and ×1.48 on
      a C that held the op-offload copy — not the weights, cache, recurrent state or
      projector weights, which all matched; what is left is the 322 MiB projector buffer
      against 248 measured), 19,410 / 19,389 and 10,783 / 10,752 on the others (were 19,650
      and 11,023). Test:
      `by_layer.rs` (`a_split_s_compute_buffers_cover_every_split_measured`: every case above,
      never below llama.cpp's figure, never more than 64 MiB or 5 % above it). The synthetic
      split tests use a larger model now (`placing_splits::add_big`), since a split's fixed
      tens of MiB per card outweighed the old 58 MiB one. Found while doing so, not changed:
      `place` counts an idle model's bytes as the room evicting it frees, not the 327 MiB
      first-prompt margin the ledger keeps beside a model that has served nothing yet, which
      leaves with it too; a start can then wait or be refused where evicting that model
      would make room.
    - **A split start logs each card's parts by kind** at info, beside its note, in MiB as
      llama.cpp's buffer lines give them: layers, weights, KV cache, recurrent state, compute,
      the projector's own compute, the projector weights and other first-card parts, the
      output layer, the engine (`ByLayer::parts_on`, `Placement.kinds`, `reserve_as`). Test:
      `by_layer.rs` (`a_split_s_parts_by_kind_add_up_to_each_card`).
    - **The engine cost is read when due**: also on the headroom's periodic reading (every
      30 s, `growth::sample`), once per container as before, so a start whose own first
      request was in flight when it settled is read once idle. Test: `it`
      `placing::the_engine_cost_is_read_once_the_model_is_idle`.
    - **A tiny learned room** (below 64 MiB) is left out of a card's sentence, still kept.
      Test: `headroom/tests.rs`.
    - **Fit again reads the row's pin** (a left-over): `on_card_now` and `cards_now` take the
      row and read only its candidates — its pin's cards, the roomiest of them as admission
      picks — and a row's own split kept on its pin is searched by that split, each card its
      part (`CardsNow::own_fits`, `plan_fit::largest_fitting_by`). Test: `it`
      `fit_again_card::fit_again_sizes_a_pinned_row_for_its_pin`.
    - **The finder's one-card choice** (found in a browser check): "beside what runs now" is
      what admission gives a start on that card (`Ledger::capacity_on`'s free, the driver's
      reserve no longer counted as room), so the header's context and the one-card choice's
      "room now" come from the same figures; the one-card choice shows what the model takes
      on that card (it read "0 B" where it did not fit); the Free VRAM labels read one decimal
      ("needs 21.8 GiB, card 2 has 21.2"); the finder's line says what is kept free beside
      the model ("Uses 21.8 GiB (21.1 GiB and 0.7 GiB kept free beside it) of the 21.8 GiB
      free on …"), "free" being the card's free memory, not net of it. Tests: `it`
      `fit_again_card::the_one_card_choice_shows_its_bytes_and_agrees_with_the_header`,
      `cards_fit/tests.rs`, the finder's words.
  - **The whole card, after the live check of e66899a6** (a left-over of 16cc5b7f): the
    finder's `whole_card` header still read the card's total less what other programs hold
    at rest and the engine-less headroom, so on one card of a four-card test server (75,536 MiB held by another program
    and an idle model of lmgw's there) Qwen3.8-27B-UD-Q4_K_M was marked
    "fits shorter" at 51,712 tokens (room 22,437,395,552 B less nothing for the engine)
    while its own one-card choice at that context said "needs 22.5 GiB, card 2 has 21.2 …
    not room now". The whole card is now the card with lmgw's models unloaded, as the
    one-card choice and an owner's start read it: what is free there for a start
    (`capacity_on`) and what lmgw's models there hold, beside the engine's cost and the floor
    (`CardNow::room`/`keep` for `WholeCard`; a row's own split kept on its pin likewise,
    `CardsNow::own_fits`). Every path that fits against it follows: the finder's card and
    its "the file the address names" note (`find/card.rs`), hf_repo's per-file marks and
    picks (`hub_fit`), Get it's fit (`refit::for_get`), Fit again's whole-card measure, and
    a budget's pooled Free VRAM (`cards_fit::pooled_free`, which took what other programs
    hold off twice). The plan's warning (`plan_fit::over_card`) and Min. cards keep
    admission's too-large rule, the card's total beside what it needs at rest. For that file
    on that card, from the answer's figures: room 22,743,351,296 B (102,641,958,912 less
    79,898,607,616 not lmgw's to free), kept beside it 1,709,605,792 (the engine's
    635,863,968 and the 1 GiB floor), so a footprint of at most 21,033,745,504; the KV
    cache and compute terms the answer gives for its other files (38,912 B a token of
    cache and about 3 KiB of compute over a 3.15 GB recurrent state) put that at about
    19,600 tokens, where the one-card choice says it fits once lmgw unloads its idle
    model. Tests: `modelinfo::fit::tests::the_whole_card_is_the_card_with_lmgw_s_models_unloaded`,
    `find::card::tests::a_named_file_on_the_whole_card_isnt_swapped` (now lmgw's own models
    on the card, not other programs').
  - **Decided by the owner 2026-11-18, and built:**
    - **9. The engine on a split card** (`split::SPLIT_ENGINE_EXTRA`, 64 MiB): measured on
      splits of 2026-11-18 on a four-card test server as what the processes hold on a card less llama.cpp's
      own buffers, an RTX PRO 6000 Blackwell (606 MiB learned from one-card starts) held 623, 625
      and 667 MiB (the 667 the middle card of a three-card split without a pipeline), an RTX 6000 Ada
      (477 learned) 497, an Ada never learned (640 charged) 479–491: up to 61 MiB more
      than one card's figure. Every card of a split, first or not, pipeline or not, is
      charged its engine cost and 64 MiB; a one-card start is unchanged; engine readings
      still come from one-card starts only. In `split::over` (so `place`, admission's claims,
      the too-large test, the verdict, Min. cards and Get it's split choice), a row's own
      split (`own_split`), Free VRAM's short figure and the split note; the per-card log
      line says "split engine extra 64.00". Tests: `place::tests::a_split_card_s_engine_holds_64_mib_more`,
      and the split figures of `place/tests.rs`, `own_split.rs`, `marks/tests.rs` and
      `cards_fit/tests.rs` (the measured machine's split now 31 and 5 layers).
    - **10. What evicting an idle model frees** (the finding above): the ledger's
      first-prompt margin for a model that has served nothing (`ResidentView::kept_bytes`,
      327 MiB) is counted in what stopping it frees, on the card it is charged on alone
      (`Ledger::held_on`, so `place`'s evictions and waits, a move's room, a claim's kept
      share, the warm group's crowding and the fit marks' "after unloading"; `freeable_on`
      over several cards; the fit's whole card through `on_card_now`). Nothing more than the
      ledger keeps for it. Test: `it` `fit_again_card::evicting_an_unserved_model_frees_its_first_prompt_margin`
      (a chat model that fits card 0 only once the idle, unserved embedding model there and
      its margin go evicts it and starts there; before, it was sent to card 1 and timed out).
    - **11. A split's parts together under a declared budget** (option b of the finding):
      under a budget over several cards a split's parts together, the engine's cost included,
      fit within what the budget leaves after everything in use on every card and other
      starts' claims (`Ledger::budget_left`, `place::Card::pool`, checked in `split::over`),
      as each card's part fits its card; with nothing else of lmgw's there (the too-large
      test, Min. cards) within the budget itself (`Ledger::budget_total`). `place` then looks
      for another set of cards or a shorter context as the row's policy allows; a split the
      budget cannot hold is refused as too large, naming the budget ("the GPU memory budget,
      which a split's parts on its cards share together with their engine's cost, holds
      …"), and a split's wait says the budget too. A budget without a device reading keeps
      its pooled mark; one card is as before. A row's own split kept on its pin likewise
      (item 12, a left-over): `own_too_large` checks its parts together against the budget
      (and each card's part with its split engine), and `split_short` at the start against
      what the budget leaves, so it is refused or waits naming the budget (`it`
      `placing_own_splits::under_a_budget_an_own_split_s_parts_together_fit_it`: parts of
      about 258 MiB together refused under 200 MiB, started on its pin under 400). Test: `it` `fit_again_card::under_a_budget_a_split_s_parts_together_fit_what_it_leaves`
      (two 320 MiB cards: a split of about 560 MiB together starts over both under 600 MiB,
      is refused naming the budget and marked too large under 500); the marks' test of
      d5926d38 keeps its verdict (its split fits the 150 MiB budget's room).
  - **Live checks that remain** (the test server): a split-only row asked while a busy model
    holds one of its cards starts split once that model finishes, and out of time names it;
    a climb on a card without learned room beside one with it starts without evicting; after
    a few starts Settings → GPU shows each card's engine cost as the largest of its readings,
    and a start whose estimate is off by more than 2 GiB logs "its estimate looks off" and
    leaves the figure; **a split of a second model (and one with `--n-cpu-moe`, one with a
    projector) holds within its plan on each card, which checks the factors**; the GPU panel
    reads "models starting, not measured yet" while a model starts.

  **Live verification** on the test server (four cards; `headroom_mode` back to `auto` after
  the update, the floor 1 GiB): Settings → GPU → Each card: cards 0, 1 and 3 say what the
  display or nothing holds and keep at most that; card 2 keeps 1.0 GiB and, with the other programs on that card below their 7-day most, says the room it keeps; `lmgw__settings` `vram.headroom`
  shows the same figures. Start a model that fits card 2 only beside the floor while another
  card has an idle model to unload: it goes to that card, unloading it, not to card 2. A split
  that fits three cards with the room kept and two without it takes three (the start's warnings
  name the cards); with a third card filled it takes two, warned "Learned growth not kept free".
  Set card 2 to Always 4 GiB: a model needing that room waits or goes elsewhere; Nothing on an
  empty card: a model fills it to its margin. Forget… on card 2: it keeps 1.0 GiB and no room
  until the next reading (30 s).

## Not yet

- **Splitting an aux, audio or image model** (D10, ruling 12).
- **Fitting a model smaller over several cards.** §5.2 step 5 fits it to one card; a lowered
  context spread over two is not done.
- **Repacking without a request waiting** (§6.2), and choosing a placement by what is likely to
  be asked for next.
- **A gate per card** (D11).
- **Handing a card to an engine that is not NVIDIA's through CDI** (D18, §5.7): AMD cards,
  Vulkan and HIP builds. They are placed as a group, as today.
- **Keeping a card for a pinned model that is not running.** The tightest fit (D7) can fill the
  card a pinned model will ask for, and that model then evicts or moves what is there.
- **MIG devices.**
- **Speed.** Nothing prefers a faster card or a card on the CPU socket the engine's threads run
  on (ruling 7).
- **`split-mode row` and `tensor`.**
- **A real residency ratio per audio engine** (owner, 2026-11-18, a small task): measure each
  audio engine lmgw ships once, and charge an unlearned voice model by its engine's ratio
  instead of its files and a margin of the files' size. Until then voice models give up that
  margin last, like every class (kept).

## Unverified (2026-11-13)

- WP0 (§1.1) verified, 2026-11-16: the engine's cost per card, the per-card memory of a stated
  split, the place of the output layer, projector, drafter and MoE cache, the device order with
  cards passed by UUID, and a start by UUID name. Still open from it: the compute buffers of a
  split as a formula (WP6), and the engine's cost on a card model WP0 did not run on (Q7).
- That lmgw's own CDI spec lists `nvidia.com/gpu=GPU-<uuid>` names with every toolkit version.
  It did on the measured machine (the image's `nvidia-container-toolkit-base`, and the host's
  system spec from toolkit 1.20.1), and a start with a UUID name ran there (§1.1).
- That `CUDA_VISIBLE_DEVICES` takes UUIDs on Windows.
- The AMD and Windows probes' per-device process memory: read in the code, run on neither.
- That `-e NVIDIA_VISIBLE_DEVICES=<uuid>` confines a container on a host with NVIDIA's legacy
  hook. That it does no harm beside a CDI device on a host without it was run (§1.1).
- `vram/residency.rs` beyond what the ledger uses of it, `vram/stall.rs`, `local_hold`'s restart
  path (which probably needs a placement too), `machine/fit/switch.rs`'s rewriting of rows that
  name a card, and the Windows launcher's `same_start` were not read by anyone for this record.
- The line numbers marked `~`, and the test count of §12, come from one read of the tree, not
  from a tool. The review checked the other citations of "Today" against 75cfe6c8.

## 14. The review of 2026-11-13

One adversarial review of this record against the code at 75cfe6c8, before
anything was built. Its findings, by its own numbers, and where each went:

| # | Finding | Went to |
|---|---|---|
| 1 | No step for waiting on a busy model: a split or a smaller fit where today waits | §5.2 step 5, ruling 14 |
| 2 | `cards_seen` on the run arguments: with `=all` nothing is ever fitted to one card | D14 |
| 3 | A row with no set context has no "whole"; the order would split it for its trained context | §5.3, ruling 15 |
| 4 | A CDI device does not confine a container where the legacy hook is installed | §7.1 (`NVIDIA_VISIBLE_DEVICES`) |
| 5 | Other forms name a card today (audio's `device`, `CUDA_VISIBLE_DEVICES`, `-dev`, class arguments) | D19, §8.1 |
| 6 | AMD: the replacement would break starts; no rule for engines lmgw cannot hand a card | D18, §5.7 |
| 7 | Ruling 11 against `MANAGED_FLAGS`, and against ruling 12 for an aux row | D20, ruling 16 |
| 8 | Readers of the pooled figures missing: warm groups, realtime budget, the preview fit, the benchmark, the wait phase | §3.1 |
| 9 | `place` needs modes: climb, guest, outside verdict, headroom against margin | D16, §5.6 |
| 10 | Choosing again after each eviction can evict on several cards | D15, §5.5 |
| 11 | A waiter keeps the one gate and now blocks starts that fit other cards | D15, §5.5 |
| 12 | Repacking through the guest path fails and inverts the eviction order | D21, §6.2 |
| 13 | Starts without a decision have no placement | §5.7 |
| 14 | D13 changed what `budget_mb` means | D22, §9.2 |
| 15 | "Changes representation, not decisions" could not hold; a transitional guess against ruling 4 | WP4 as rewritten, §3.3 |
| 16 | The package order left regressions between one-card placing, splits and headroom | §13's new order |
| 17 | A non-chat model too large for any card would wait for ever | §5.8 |
| 18 | The split's cost leaves out the projector, the drafter, MoE and adoption | §4.3 |
| 19 | WP9 had no place to keep the engine's job, and pulls do not resume | WP9 as rewritten |
| 20 | The shared fake and 57 argv expectations | D17, §12 |
| 21–28 | The image key and digests, the machine check, the type's name, what headroom per card costs, pins of rows not running, the ranking of victims, labels on a roll-back, the copyable command line | §7.1, D23, §9.1, "Not yet", §5.2 step 2, §3.3 |
| 29 | `--tensor-split` and the output layer | WP0, "Unverified" |
| 30 | Citations | corrected in place |

What the review tried to break and could not: the other citations of "Today"; that a pinned row
is fitted against the pool; §1's arithmetic; that swapping device arguments at render does not
make a container look reconfigured, given the digest is taken from the arguments as written;
that CPU rows are decided by the backend and never by the run arguments; that unified KV pools
do not depend on cards; that a large model is not starved by small ones.
