// The page-audio probe of chat-voice WP6 (§15), run inside the dashboard by
// scripts/webkit-check.py --media (WebKitGTK, mock capture devices) and by
// scripts/media-probe-chrome.sh (Chromium in a container, fake capture from a
// TTS-generated WAV). It defines `window.lmgwMediaProbe(opts)`, which answers
// a JSON string `{checks: [{name, ok, detail}], info}`.
//
// Two halves:
// - the worklets alone, loaded from the gateway's /voice/: the capture worklet
//   on the microphone at 24 and 16 kHz (40 ms chunks, real-time rate), and the
//   player worklet (played == pushed, the analyser hears it, a flush answers
//   what was played) — behind a zero gain after the analyser, so inaudible;
// - the page's own code through the Chat composer's audio devices popover:
//   Test microphone (the UI's capture at 24 kHz, the level meter, device names
//   after the grant, every track ended at Stop and when the popover closes; a
//   track the system mutes, a track that ends on its own and a pagehide into
//   the back/forward cache, each said and released), the test tone through
//   the page's player (played == pushed, heard <= played, the output
//   analyser), the echo chip's default, its warning on the composer's button,
//   the device that is the default not warning, the stored mode, and a choice
//   another tab stores (from a same-origin frame: the storage event) taken
//   over — in a browser that routes outputs also a chosen output that is gone,
//   said on the button.
//
// The main run ends by storing one of the inputs as the window's choice; the
// harness reloads the page and runs it again with {phase: "reopen"}: a fresh
// document (WebKitGTK hides ids again) must open that stored input, asking
// getUserMedia at most twice (`ideal`, then `exact` if another one came).
//
// window.lmgwGestureProbe(step) serves a harness without autoplay allowed
// (scripts/media-probe-chrome.mjs): "open" opens the popover and answers the
// tone button's rectangle, "script" clicks it from script (no user gesture),
// "state" reads the tone's state. window.lmgwRevokeProbe(step) opens the
// test microphone ("start") and reads how it ended ("check") around a
// permission the harness takes back.
//
// {phase: "dictation"} (chat-voice WP7) runs a dictation round through the
// composer's microphone against a mock ASR: the gateway's `voice/warm` and
// `transcribe` are answered in the page (window.fetch), so no model is asked
// and nothing leaves the page, and a fresh temporary chat is opened whose
// thread JSON resolves its speech-to-text to the mock — the round runs on any
// gateway, one with no speech model set up included. It checks the uploaded WAV (16 kHz mono
// PCM16, as long as the press, not silent), every track ended at the
// release, the text at the caret with the dictation mark, Esc discarding,
// Right Ctrl held, a Right Ctrl tap and Right Ctrl + another key opening no
// microphone at all (the key's arm time), a release before the microphone
// opened uploading nothing, the press-and-hold path by pointer events, Enter
// while recording finishing the dictation rather than sending, a GPU hold
// and a benchmark run said as a hold, a failure naming the fallback that had
// the audio, the mark going with an emptied composer, and a live region that
// is always mounted and never says the ticking clock (WP7 fix).
//
// {phase: "realtime"} (chat-voice WP9) runs voice mode against an in-page
// bound session (scripts/realtime-mock.js, which the harness loads beside
// this file): see `realtime` below.
//
// opts: {inputLabel, outputKind, outputs: [names], skipWorklets, phase}
// The probe opens a temporary chat when no thread is open (it is never saved).
(() => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  async function until(fn, ms = 10000, step = 100) {
    const end = performance.now() + ms;
    let v;
    while (performance.now() < end) {
      try {
        v = await fn();
      } catch (_) {
        v = undefined;
      }
      if (v) return v;
      await sleep(step);
    }
    return v;
  }
  const notes = (root) => [...root.querySelectorAll(".vd-note")].map((n) => n.textContent);
  // Where the probe is, for a harness that polls (window.__probeStage).
  const stage = (s) => { window.__probeStage = s; };

  async function worklets(check, info) {
    for (const f of ["capture-worklet.js", "player-worklet.js"]) {
      const r = await fetch("/voice/" + f, { cache: "no-store" });
      const ct = r.headers.get("content-type") || "";
      check(`/voice/${f} is served as JavaScript`, r.ok && /javascript/.test(ct), `${r.status} ${ct}`);
    }
    stage("worklets: getUserMedia");
    const stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: false } });
    const track = stream.getAudioTracks()[0];
    const settings = track.getSettings();
    info.worklet_track = { label: track.label, sampleRate: settings.sampleRate || null };
    for (const target of [24000, 16000]) {
      stage(`worklets: capture at ${target}`);
      const ctx = new AudioContext(settings.sampleRate ? { sampleRate: settings.sampleRate } : {});
      await ctx.audioWorklet.addModule("/voice/capture-worklet.js");
      const src = ctx.createMediaStreamSource(stream);
      const node = new AudioWorkletNode(ctx, "lmgw-capture", {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        outputChannelCount: [1],
        processorOptions: { targetRate: target, chunkMs: 40 },
      });
      const zero = ctx.createGain();
      zero.gain.value = 0;
      src.connect(node);
      node.connect(zero);
      zero.connect(ctx.destination);
      let n = 0;
      let samples = 0;
      let peak = 0;
      const lens = new Set();
      node.port.onmessage = (e) => {
        if (e.data.type !== "chunk") return;
        const a = new Int16Array(e.data.pcm);
        n++;
        samples += a.length;
        lens.add(a.length);
        for (let i = 0; i < a.length; i++) peak = Math.max(peak, Math.abs(a[i]));
      };
      await ctx.resume();
      await sleep(500);
      const s0 = samples;
      const n0 = n;
      const t0 = performance.now();
      await sleep(2000);
      const rate = (samples - s0) / ((performance.now() - t0) / 1000);
      node.port.postMessage({ type: "stop" });
      const ctxRate = ctx.sampleRate;
      await ctx.close();
      const want = (target * 40) / 1000;
      check(`capture worklet: ${target / 1000} kHz, 40 ms chunks`,
        n - n0 >= 40 && [...lens].every((l) => l === want),
        { chunks: n - n0, lengths: [...lens], context_rate: ctxRate });
      check(`capture worklet: ${target / 1000} kHz in real time`,
        Math.abs(rate - target) / target < 0.1, { measured: Math.round(rate) });
      check(`capture worklet: ${target / 1000} kHz carries the input`, peak > 300, { peak });
    }

    // Push-to-talk: gated, only the pre-roll is kept; opening the gate sends
    // it at once, then live chunks; a flush hands over the partial chunk.
    stage("worklets: gate and pre-roll");
    {
      const ctx = new AudioContext(settings.sampleRate ? { sampleRate: settings.sampleRate } : {});
      await ctx.audioWorklet.addModule("/voice/capture-worklet.js");
      const src = ctx.createMediaStreamSource(stream);
      const node = new AudioWorkletNode(ctx, "lmgw-capture", {
        numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1],
        processorOptions: { targetRate: 24000, chunkMs: 40, prerollMs: 200, gated: true },
      });
      const zero = ctx.createGain();
      zero.gain.value = 0;
      src.connect(node);
      node.connect(zero);
      zero.connect(ctx.destination);
      const got = [];
      node.port.onmessage = (e) => got.push({ type: e.data.type, n: e.data.pcm ? e.data.pcm.byteLength / 2 : 0, t: performance.now() });
      await ctx.resume();
      await sleep(1000);
      const whileGated = got.length;
      node.port.postMessage({ type: "gate", open: true });
      await sleep(100);
      const burst = got.filter((m) => m.type === "chunk").length;
      await sleep(400);
      node.port.postMessage({ type: "flush" });
      await until(() => got.some((m) => m.type === "flushed"), 2000, 20);
      node.port.postMessage({ type: "stop" });
      await ctx.close();
      const last = got.filter((m) => m.type === "chunk").slice(-1)[0];
      check("capture worklet: gated, it sends nothing", whileGated === 0, whileGated);
      check("capture worklet: opening the gate sends the 200 ms pre-roll at once", burst >= 5 && burst <= 8, burst);
      check("capture worklet: a flush hands over the partial chunk, then says flushed",
        got.length > 0 && got[got.length - 1].type === "flushed" && !!last && last.n > 0 && last.n <= 960,
        { last_chunk: last && last.n, messages: got.length });
    }
    stream.getTracks().forEach((t) => t.stop());

    stage("worklets: player");
    const ctx = new AudioContext({ sampleRate: 24000 });
    check("a 24 kHz playback context", ctx.sampleRate === 24000, ctx.sampleRate);
    stage("worklets: player addModule");
    await ctx.audioWorklet.addModule("/voice/player-worklet.js");
    const node = new AudioWorkletNode(ctx, "lmgw-player", {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [1],
    });
    const an = ctx.createAnalyser();
    an.fftSize = 2048;
    const mute = ctx.createGain();
    mute.gain.value = 0; // inaudible: after the analyser
    node.connect(an);
    an.connect(mute);
    mute.connect(ctx.destination);
    stage(`worklets: player resume (${ctx.state})`);
    await ctx.resume();
    stage("worklets: player push");
    const msgs = [];
    node.port.onmessage = (e) => msgs.push(e.data);
    let pushed = 0;
    for (let k = 0; k < 6; k++) {
      const a = new Int16Array(2400);
      for (let i = 0; i < a.length; i++) {
        a[i] = Math.round(8000 * Math.sin((2 * Math.PI * 440 * (k * 2400 + i)) / 24000));
      }
      pushed += a.length;
      node.port.postMessage({ type: "push", item: 1, pcm: a.buffer }, [a.buffer]);
    }
    node.port.postMessage({ type: "end", item: 1 });
    const buf = new Float32Array(an.fftSize);
    let peak = 0;
    const ended = await until(() => {
      an.getFloatTimeDomainData(buf);
      for (let i = 0; i < buf.length; i++) peak = Math.max(peak, Math.abs(buf[i]));
      return msgs.find((m) => m.type === "ended");
    }, 5000, 20);
    check("player worklet: played == pushed", !!ended && ended.pushed === pushed && ended.played === pushed,
      { pushed, ended: ended || null });
    check("player worklet: the output analyser hears it", peak > 0.1, { peak: +peak.toFixed(3) });
    check("player worklet: progress about every 50 ms",
      msgs.filter((m) => m.type === "progress").length >= 5,
      msgs.filter((m) => m.type === "progress").length);
    msgs.length = 0;
    const big = new Int16Array(24000).fill(1000);
    node.port.postMessage({ type: "push", item: 2, pcm: big.buffer }, [big.buffer]);
    await sleep(300);
    node.port.postMessage({ type: "flush", id: 9 });
    const fl = await until(() => msgs.find((m) => m.type === "flushed"), 2000, 20);
    const it = fl && fl.items.find((i) => i.item === 2);
    check("player worklet: a flush answers what was played",
      !!it && it.pushed === 24000 && it.played > 0 && it.played < 24000, it || null);
    await ctx.close();
  }

  const q = (s) => document.querySelector(s);

  // The composer's devices button (a temporary chat opened if no thread is),
  // and the popover's root once open.
  async function popover(check) {
    if (!q("[data-voice-devices-btn]")) {
      const b = await until(() => [...document.querySelectorAll("button")]
        .find((x) => (x.title || "").startsWith("New temporary chat")), 15000);
      if (!b) {
        check("a temporary chat to work in", false, "no 'New temporary chat' button");
        return {};
      }
      b.click();
    }
    stage("ui: the devices button");
    // The composer re-mounts while the page restores its thread: wait until
    // the same button has stood for a second.
    let btn = null;
    let since = performance.now();
    const settled = performance.now() + 15000;
    while (performance.now() < settled) {
      const b = q("[data-voice-devices-btn]");
      if (b !== btn) {
        btn = b;
        since = performance.now();
      } else if (b && performance.now() - since > 1000) {
        break;
      }
      await sleep(100);
    }
    check("the composer has the audio devices button", !!btn);
    if (!btn) return {};
    // Open, connected and shown; clicked again if a re-mount took it.
    const shown = () => {
      const r = q("[data-voice-devices]");
      return r && r.isConnected && r.closest(":popover-open") ? r : null;
    };
    let root = shown();
    for (let i = 0; i < 3 && !root; i++) {
      if (!q("[data-voice-devices]")) q("[data-voice-devices-btn]").click();
      root = await until(shown, 3000);
    }
    check("the devices popover opens", !!root);
    return { btn: q("[data-voice-devices-btn]"), root };
  }

  async function ui(check, info, tracks, opts) {
    const { btn, root } = await popover(check);
    if (!root) return;
    await until(() => root.dataset.labels && root.dataset.labels !== "reading", 5000);
    info.labels_before = root.dataset.labels;
    info.output_kind = root.dataset.outputKind;
    if (root.dataset.labels === "hidden") {
      const ns = [...root.querySelectorAll("[data-vd-note]")].map((n) => n.textContent);
      check("before a grant the list says the names come with first use",
        ns.some((t) => t.includes("first used")), ns);
    }
    if (opts.outputKind) {
      check(`the outputs come from: ${opts.outputKind}`, root.dataset.outputKind === opts.outputKind,
        root.dataset.outputKind);
    }
    const echo = root.querySelector("[data-echo]");
    check("the echo mode defaults to the input device", !!echo && echo.dataset.echo === "device",
      echo && echo.dataset.echo);

    const first = tracks.length;
    stage("ui: Test microphone");
    q("[data-vd-mic=start]").click();
    await until(() => root.dataset.mic === "open" || root.dataset.mic === "error", 15000);
    check("Test microphone opens the page's capture", root.dataset.mic === "open",
      { mic: root.dataset.mic, notes: notes(root) });
    if (root.dataset.mic === "open") {
      await sleep(600);
      const s0 = +root.dataset.samples;
      const c0 = +root.dataset.chunks;
      const t0 = performance.now();
      await sleep(2000);
      const s1 = +root.dataset.samples;
      const c1 = +root.dataset.chunks;
      const rate = (s1 - s0) / ((performance.now() - t0) / 1000);
      check("the page's capture delivers 40 ms chunks at 24 kHz in real time",
        c1 - c0 >= 40 && (s1 - s0) / (c1 - c0) === 960 && Math.abs(rate - 24000) / 24000 < 0.1,
        { chunks: c1 - c0, samples_per_chunk: (s1 - s0) / Math.max(1, c1 - c0),
          rate: Math.round(rate), native_rate: root.dataset.nativeRate });
      check("the microphone meter moves", +root.dataset.peak > 0.05, root.dataset.peak);
      await until(() => root.dataset.labels === "known", 5000);
      const rows = [...root.querySelectorAll("[data-vd=input] .vd-opt")];
      info.inputs = rows.map((b) => b.textContent.trim());
      check("after the grant the inputs are named",
        root.dataset.labels === "known" && rows.length >= 2
          && (!opts.inputLabel || info.inputs.some((t) => t.includes(opts.inputLabel))),
        info.inputs);
      if (rows.length >= 3) {
        const target = rows[rows.length - 1];
        const label = target.textContent.trim();
        const before = tracks.length;
        target.click();
        await until(() => tracks.length > before && root.dataset.mic === "open", 10000);
        const t = tracks[tracks.length - 1];
        const stored = localStorage.getItem("lmgw.voice.input") || "";
        check("choosing an input reopens the test on it, stored for the window",
          !!t && t.label === label && stored.includes(label),
          { track: t && t.label, label, stored });
        const prev = tracks.slice(first, tracks.length - 1);
        check("the previous capture's tracks ended", prev.every((x) => x.readyState === "ended"),
          prev.map((x) => x.readyState));
        rows[0].click(); // back to the system default
        await until(() => root.dataset.mic === "open" && !localStorage.getItem("lmgw.voice.input"), 10000);
      }
      q("[data-vd-mic=stop]").click();
      await sleep(300);
      const mine = tracks.slice(first);
      check("Stop releases the microphone: every track ended",
        mine.length > 0 && mine.every((t) => t.readyState === "ended") && root.dataset.mic === "closed",
        mine.map((t) => t.readyState));

      // Ends nobody asked for (review M3), dispatched as the browser would.
      stage("ui: a microphone that ends on its own");
      const opened = async () => {
        q("[data-vd-mic=start]").click();
        await until(() => root.dataset.mic === "open" || root.dataset.mic === "error", 10000);
        return tracks[tracks.length - 1];
      };
      let t = await opened();
      t.dispatchEvent(new Event("mute"));
      await until(() => root.dataset.micMuted === "true", 3000);
      check("a track the system mutes is said (it delivers silence)",
        root.dataset.micMuted === "true" && notes(root).some((n) => n.includes("muted")),
        { muted: root.dataset.micMuted, notes: notes(root) });
      t.dispatchEvent(new Event("unmute"));
      await until(() => root.dataset.micMuted === "false", 3000);
      check("... and its unmute", root.dataset.micMuted === "false" && root.dataset.mic === "open",
        root.dataset.micMuted);
      t.dispatchEvent(new Event("ended"));
      await until(() => root.dataset.mic === "ended", 3000);
      check("a track that ends on its own (unplugged) ends the capture: released and said",
        root.dataset.mic === "ended" && tracks.slice(first).every((x) => x.readyState === "ended")
          && notes(root).some((n) => n.includes("stopped delivering")),
        { mic: root.dataset.mic, notes: notes(root) });
      t = await opened();
      window.dispatchEvent(new PageTransitionEvent("pagehide", { persisted: true }));
      await until(() => root.dataset.mic === "ended", 3000);
      check("a pagehide into the back/forward cache ends the capture: released and said",
        root.dataset.mic === "ended" && t.readyState === "ended"
          && notes(root).some((n) => n.includes("page was hidden")),
        { mic: root.dataset.mic, track: t.readyState, notes: notes(root) });
    }

    info.outputs = [...root.querySelectorAll("[data-vd=output] .vd-opt")].map((b) => b.textContent.trim());
    if (opts.outputs) {
      check("the output list names the outputs", opts.outputs.every((o) => info.outputs.some((t) => t.includes(o))),
        info.outputs);
    }
    stage("ui: test tone");
    q("[data-vd-tone]").click();
    let outPeak = 0;
    await until(() => {
      outPeak = Math.max(outPeak, +root.dataset.outPeak);
      return root.dataset.tone === "done" || root.dataset.tone === "error";
    }, 15000, 25);
    check("the test tone plays through the page's player", root.dataset.tone === "done",
      { tone: root.dataset.tone, notes: notes(root) });
    check("the test tone: played == pushed",
      +root.dataset.tonePushed > 0 && root.dataset.tonePushed === root.dataset.tonePlayed,
      { pushed: +root.dataset.tonePushed, played: +root.dataset.tonePlayed });
    check("the output analyser hears the test tone", outPeak > 0.3, outPeak);
    check("the test tone: heard <= played (the reported output latency taken off)",
      +root.dataset.toneHeard <= +root.dataset.tonePlayed && +root.dataset.toneHeard > 0,
      { heard: +root.dataset.toneHeard, played: +root.dataset.tonePlayed, latency: +root.dataset.latency });
    info.latency_samples = +root.dataset.latency;
    info.route = { state: root.dataset.route, notes: notes(root) };
    if (!opts.skipIdle) {
      stage("ui: the player suspends when idle");
      const suspended = await until(() => root.dataset.player === "suspended", 14000, 250);
      check("the playback context suspends after 10 s with nothing to play", !!suspended, root.dataset.player);
      q("[data-vd-tone]").click();
      await until(() => root.dataset.tone === "playing" || root.dataset.tone === "error", 5000, 25);
      await until(() => root.dataset.tone === "done" || root.dataset.tone === "error", 15000, 25);
      check("the next tone resumes it, played == pushed",
        root.dataset.tone === "done" && root.dataset.tonePushed === root.dataset.tonePlayed
          && root.dataset.player === "running" && root.dataset.route === "ok",
        { tone: root.dataset.tone, pushed: +root.dataset.tonePushed, played: +root.dataset.tonePlayed,
          player: root.dataset.player, route: root.dataset.route });
    }

    stage("ui: echo");
    const outRows = [...root.querySelectorAll("[data-vd=output] .vd-opt")];
    const other = outRows.find((b, i) => i > 0 && !b.disabled && !b.querySelector(".vd-tag"));
    if (other) {
      const label = other.querySelector(".vd-opt-name").textContent.trim();
      other.click();
      const warned = await until(() => q("[data-echo-warning=yes]"), 3000);
      const text = warned ? q(".echo-warn").textContent : "";
      check("an output other than the default warns in device mode",
        !!warned && text.includes(label), text || null);
      check("... and the composer's devices button says so",
        btn.classList.contains("warn") && (btn.dataset.voiceAlert || "").includes(label),
        btn.dataset.voiceAlert || null);
      info.picked_output = label;
      outRows[0].click();
      check("the system default clears the warning", !!(await until(() => q("[data-echo-warning=no]"), 3000)));
    } else {
      info.echo_warning = "no second output listed here: the warning rule is covered by the unit tests";
    }
    const isDefault = outRows.find((b, i) => i > 0 && !b.disabled && b.querySelector(".vd-tag"));
    if (isDefault) {
      isDefault.click();
      await sleep(400);
      check("choosing the device that is the system default does not warn",
        !!q("[data-echo-warning=no]") && !btn.classList.contains("warn"),
        isDefault.textContent.trim());
      outRows[0].click();
      await sleep(200);
    } else if (opts.outputKind === "sinkid") {
      info.default_output = "no listed output is marked as the system default";
    }

    // A choice another tab of this profile stores (review n11), and in a
    // browser that routes outputs a chosen output that is gone (m5).
    stage("ui: another tab's choice");
    const frame = document.createElement("iframe");
    frame.style.display = "none";
    document.body.appendChild(frame);
    const other_tab = frame.contentWindow.localStorage;
    other_tab.setItem("lmgw.voice.echo", "not_needed");
    await until(() => (q("[data-echo]") || {}).dataset?.echo === "not_needed", 3000);
    check("a choice another tab stores is taken over (the storage event)",
      q("[data-echo]").dataset.echo === "not_needed", q("[data-echo]").dataset.echo);
    other_tab.removeItem("lmgw.voice.echo");
    await until(() => q("[data-echo]").dataset.echo === "device", 3000);
    if (root.dataset.outputKind === "sinkid") {
      other_tab.setItem("lmgw.voice.output", JSON.stringify({ id: "gone-output", label: "Gone headphones" }));
      await until(() => (btn.dataset.voiceAlert || "").includes("not present"), 3000);
      check("a chosen output that is gone warns on the composer's devices button",
        btn.classList.contains("warn") && (btn.dataset.voiceAlert || "").includes("Gone headphones"),
        btn.dataset.voiceAlert || null);
      other_tab.removeItem("lmgw.voice.output");
      await until(() => !btn.dataset.voiceAlert, 3000);
      check("... and going back to the system default clears it", !btn.dataset.voiceAlert && !btn.classList.contains("warn"),
        btn.dataset.voiceAlert || null);
    }
    frame.remove();
    q(".echo-chip").click();
    (await until(() => q('.echo-mode[data-mode="none"]'), 2000)).click();
    check("the echo mode is stored for the window", localStorage.getItem("lmgw.voice.echo") === "none",
      localStorage.getItem("lmgw.voice.echo"));
    q(".echo-chip").click();
    (await until(() => q('.echo-mode[data-mode="device"]'), 2000)).click();

    stage("ui: close with the test running");
    q("[data-vd-mic=start]").click();
    await until(() => root.dataset.mic === "open", 10000);
    btn.click(); // the popover closes with the test running
    await sleep(500);
    check("closing the popover releases the microphone",
      tracks.length > first && tracks.every((t) => t.readyState === "ended") && !q("[data-voice-devices]"),
      tracks.map((t) => t.readyState));
  }

  // The reload's half: the stored input, in a document with no grant yet.
  async function reopen(check, info, tracks, calls) {
    const stored = JSON.parse(localStorage.getItem("lmgw.voice.input") || "null");
    if (!stored) {
      check("an input stored by the first run", false, null);
      return;
    }
    const { root } = await popover(check);
    if (!root) return;
    await until(() => root.dataset.labels && root.dataset.labels !== "reading", 5000);
    info.reopen_labels_before = root.dataset.labels;
    if (root.dataset.labels === "hidden") {
      const rows = [...root.querySelectorAll("[data-vd=input] .vd-opt")].map((b) => b.textContent.trim());
      check("before a grant the stored input is listed as chosen here before",
        rows.some((r) => r.includes(stored.label) && r.includes("chosen here before")), rows);
    }
    stage("reopen: Test microphone");
    const before = calls.n;
    q("[data-vd-mic=start]").click();
    await until(() => root.dataset.mic === "open" || root.dataset.mic === "error", 15000);
    const t = tracks[tracks.length - 1];
    const asked = calls.n - before;
    check("a fresh document opens the window's stored input",
      root.dataset.mic === "open" && !!t && t.label === stored.label && t.readyState === "live",
      { track: t && t.label, stored: stored.label, mic: root.dataset.mic, notes: notes(root) });
    check("... asking getUserMedia once, or twice when the browser opened another first",
      asked >= 1 && asked <= 2 && tracks.slice(0, -1).every((x) => x.readyState === "ended"),
      { getUserMedia_calls: asked });
    info.reopen_getusermedia_calls = asked;
    q("[data-vd-mic=stop]").click();
    await sleep(300);
    check("Stop releases it: every track ended", tracks.every((x) => x.readyState === "ended"),
      tracks.map((x) => x.readyState));
    localStorage.removeItem("lmgw.voice.input");
  }

  // Store an input that is not the system default for the reopen phase.
  async function storeForReopen(info) {
    const devs = await navigator.mediaDevices.enumerateDevices();
    info.devices = devs.map((d) => ({ kind: d.kind, id: d.deviceId.slice(0, 8), label: d.label, group: d.groupId.slice(0, 8) }));
    const ins = devs.filter((d) => d.kind === "audioinput" && d.deviceId && d.label
      && d.deviceId !== "default" && d.deviceId !== "communications");
    const pick = ins[ins.length - 1];
    if (pick) {
      localStorage.setItem("lmgw.voice.input", JSON.stringify({ id: pick.deviceId, label: pick.label }));
      info.reopen = { label: pick.label };
    }
  }

  // ---- Dictation against a mock ASR (WP7) ----
  const MOCK_TEXT = "das ist ein Diktat";
  const sseBody = (frames) =>
    frames.map(([e, d]) => `event: ${e}\ndata: ${JSON.stringify(d)}\n\n`).join("");
  function parseWav(buf) {
    const v = new DataView(buf);
    const tag = (o) => String.fromCharCode(...new Uint8Array(buf, o, 4));
    if (buf.byteLength < 44 || tag(0) !== "RIFF" || tag(8) !== "WAVE") {
      return { ok: false, bytes: buf.byteLength };
    }
    const rate = v.getUint32(24, true);
    const data = v.getUint32(40, true);
    let peak = 0;
    for (let i = 44; i + 1 < buf.byteLength; i += 2) peak = Math.max(peak, Math.abs(v.getInt16(i, true)));
    return { ok: true, channels: v.getUint16(22, true), rate, bits: v.getUint16(34, true),
      samples: data / 2, ms: Math.round((data / 2 / rate) * 1000), peak, bytes: buf.byteLength };
  }
  // The gateway's two dictation routes, answered here. `mock.mode`: "ok",
  // "hold" (503 gpu_hold), "fallback" (502 naming the fallback that had it).
  // A thread the page reads resolves its speech-to-text to the mock.
  function mockAsr(mock) {
    const real = window.fetch;
    window.fetch = async function (input, init) {
      const url = typeof input === "string" ? input : input.url;
      const req = typeof input === "string" ? new Request(input, init) : input;
      if (req.method === "GET" && /\/chat\/api\/threads\/-?\d+$/.test(url)) {
        const resp = await real.call(window, req);
        const v = await resp.json();
        const r = v.thread && v.thread.voice_resolved;
        if (r) {
          r.asr = { alias: "mock/asr", source: "chat", inherited: "mock/asr", local: true, cpu: true,
            fallback: null, fallback_unusable: null };
          r.problems = (r.problems || []).filter((p) => p.stage !== "asr");
        }
        return new Response(JSON.stringify(v), { status: resp.status,
          headers: { "content-type": "application/json" } });
      }
      if (/\/voice\/warm$/.test(url)) {
        mock.warm++;
        mock.warmBody = await req.clone().text();
        return new Response(sseBody([
          ["state", { stage: "asr", alias: "mock/asr", state: "loading", ms: null }],
          ["state", { stage: "asr", alias: "mock/asr", state: "ready", ms: 1234 }],
          ["done", {}],
        ]), { headers: { "content-type": "text/event-stream" } });
      }
      if (/\/transcribe$/.test(url)) {
        const wav = parseWav(await req.clone().arrayBuffer());
        mock.uploads.push({ type: req.headers.get("content-type"), wav });
        const json = (status, body, headers = {}) => new Response(JSON.stringify(body),
          { status, headers: { "content-type": "application/json", ...headers } });
        if (mock.mode === "hold") {
          return json(503, { code: "gpu_hold", message: "the GPU hold is on and mock/asr has no usable fallback",
            asr_answered_by: null });
        }
        if (mock.mode === "bench") {
          return json(503, { code: "gpu_benchmark", message: "benchmark run 7 holds the GPU",
            asr_answered_by: null });
        }
        if (mock.mode === "fallback") {
          return json(502, { code: "upstream_error", message: "the provider failed",
            asr_answered_by: "cloud/whisper" }, { "x-lmgw-fallback": "cloud/whisper" });
        }
        return json(200, { text: MOCK_TEXT, alias: "mock/asr", asr_answered_by: null, asr_ms: 42,
          audio_ms: wav.ms, language: "de" });
      }
      return real.apply(this, arguments);
    };
    return () => { window.fetch = real; };
  }

  async function dictation(check, info, tracks, calls) {
    stage("dictation: a temporary chat with the mock ASR");
    const mock = { mode: "ok", warm: 0, uploads: [] };
    const restore = mockAsr(mock);
    try {
      const was = location.search;
      const b = await until(() => [...document.querySelectorAll("button")]
        .find((x) => (x.title || "").startsWith("New temporary chat")), 15000);
      if (b) b.click();
      await until(() => location.search !== was && location.search.startsWith("?t=-"), 15000);
      check("a fresh temporary chat to dictate into", location.search.startsWith("?t=-"), location.search);
      await dictationRound(check, info, tracks, mock, calls);
    } finally {
      restore();
    }
  }

  async function dictationRound(check, info, tracks, mock, calls) {
    stage("dictation: the composer");
    const mic = await until(() => q("[data-mic-btn]") && /mock\/asr/.test(q("[data-mic-btn]").title)
      && q("[data-mic-btn]"), 15000);
    check("the composer has the microphone, its tooltip naming the thread's ASR", !!mic,
      q("[data-mic-btn]")?.title);
    if (!mic) return;
    await sleep(1000);
    const ta = q("textarea.composer-input");
    const typed = (v) => {
      ta.value = v;
      ta.dispatchEvent(new Event("input", { bubbles: true }));
    };
    typed("Vorher");
    ta.setSelectionRange(6, 6);
    const status = () => q("[data-voice-status]");
    const state = () => q("[data-mic-btn]").dataset.dictation;
    const allEnded = () => tracks.length > 0 && tracks.every((t) => t.readyState === "ended");
    const key = (type, code, k, extra = {}) =>
      window.dispatchEvent(new KeyboardEvent(type, { key: k, code, bubbles: true, ...extra }));
    const live = () => q("[data-voice-live]");
    const liveSaid = [];
    const watchLive = new MutationObserver(() => liveSaid.push(live()?.textContent || ""));
    check("the voice live region is mounted before anything happens", !!live() && live().getAttribute("aria-live") === "polite");
    if (live()) watchLive.observe(live(), { childList: true, subtree: true, characterData: true });
    {
      stage("dictation: a click records");
      q("[data-mic-btn]").click();
      await until(() => state() === "recording", 8000);
      check("a click opens the microphone and records", state() === "recording", state());
      check("the press warms the thread's ASR (voice/warm, stage asr)",
        mock.warm === 1 && JSON.parse(mock.warmBody || "{}").stages?.join() === "asr", mock);
      const said = await until(() => /mock\/asr loaded in 1\.2 s/.test(status()?.textContent || ""), 3000);
      check("the warm's state frames are said on the status line", !!said, status()?.textContent);
      check("the status line shows the recording with its level meter",
        status()?.dataset.dictation === "recording" && !!status()?.querySelector(".vs-vu"));
      await sleep(1500);
      stage("dictation: a second click transcribes");
      q("[data-mic-btn]").click();
      await until(() => state() === "idle", 10000);
      const up = mock.uploads[0];
      info.dictation_upload = up || null;
      check("the release uploads one WAV as audio/wav: 16 kHz, mono, 16-bit",
        mock.uploads.length === 1 && up.type === "audio/wav" && up.wav.ok && up.wav.rate === 16000
          && up.wav.channels === 1 && up.wav.bits === 16, up);
      check("... as long as the press, and not silent", !!up && up.wav.ms >= 1200 && up.wav.ms < 5000
        && up.wav.peak > 300, up && up.wav);
      check("every track ended at the release", allEnded(), tracks.map((t) => t.readyState));
      check("the text lands at the caret, spaced", ta.value === "Vorher " + MOCK_TEXT, ta.value);
      check("the composer is marked as dictated, its tooltip says by which model and how fast",
        status()?.dataset.dictated === "true" && /Dictated · transcribed by mock\/asr · 42 ms/.test(ta.title),
        { dictated: status()?.dataset.dictated, title: ta.title });
      await until(() => document.activeElement === ta, 1000, 20);
      check("the composer has the focus, the caret after the text",
        document.activeElement === ta && ta.selectionStart === ta.value.length,
        { active: document.activeElement?.tagName, caret: ta.selectionStart });

      stage("dictation: Esc discards");
      q("[data-mic-btn]").click();
      await until(() => state() === "recording", 8000);
      key("keydown", "Escape", "Escape");
      await until(() => state() === "idle", 5000);
      check("Esc discards: nothing uploaded, the microphone released",
        state() === "idle" && mock.uploads.length === 1 && allEnded(), { uploads: mock.uploads.length });

      stage("dictation: a Right Ctrl tap");
      const gumTap = calls.n;
      key("keydown", "ControlRight", "Control", { ctrlKey: true });
      await sleep(80);
      key("keyup", "ControlRight", "Control");
      await until(() => state() === "idle", 3000);
      await sleep(500);
      check("a Right Ctrl tap opens no microphone, warms nothing, uploads nothing, and says so",
        calls.n === gumTap && mock.uploads.length === 1 && /nothing was recorded: hold Right Ctrl/.test(status()?.textContent || ""),
        { gum: calls.n - gumTap, uploads: mock.uploads.length, status: status()?.textContent });

      stage("dictation: a remapped Right Ctrl");
      key("keydown", "ControlRight", "Compose", {});
      await sleep(500);
      check("Right Ctrl reporting another key (Compose) is not dictation", state() === "idle" && calls.n === gumTap, state());
      key("keyup", "ControlRight", "Compose");

      stage("dictation: Right Ctrl held");
      const warmsBefore = mock.warm;
      key("keydown", "ControlRight", "Control", { ctrlKey: true });
      await until(() => state() === "recording", 8000);
      key("keydown", "ControlRight", "Control", { ctrlKey: true, repeat: true });
      await sleep(1300);
      check("Right Ctrl held records (repeats ignored), and warms once it hears speech or is held a further arm time",
        state() === "recording" && mock.warm === warmsBefore + 1, { state: state(), warm: mock.warm });
      check("the live region never says the ticking clock or the level",
        liveSaid.every((t) => !/\d:\d\d/.test(t)) && /recording/.test(live()?.textContent || ""),
        liveSaid.slice(-3));
      key("keyup", "ControlRight", "Control");
      await until(() => state() === "idle", 10000);
      check("letting go of Right Ctrl transcribes", mock.uploads.length === 2 && allEnded(),
        { uploads: mock.uploads.length });
      check("two dictations add up in the mark",
        ta.value === "Vorher " + MOCK_TEXT + " " + MOCK_TEXT && /\(2 dictations\)/.test(ta.title), ta.value);

      stage("dictation: Right Ctrl + C");
      const warms2 = mock.warm;
      const gum2 = calls.n;
      key("keydown", "ControlRight", "Control", { ctrlKey: true });
      await until(() => state() !== "idle", 3000);
      await sleep(100);
      key("keydown", "KeyC", "c", { ctrlKey: true });
      await until(() => state() === "idle", 5000);
      await sleep(400);
      key("keyup", "ControlRight", "Control");
      check("Right Ctrl + C within the arm time: no microphone opened, nothing warmed or uploaded, no note",
        state() === "idle" && mock.uploads.length === 2 && mock.warm === warms2 && calls.n === gum2
          && !/cancelled/.test(status()?.textContent || ""),
        { uploads: mock.uploads.length, warm: mock.warm - warms2, gum: calls.n - gum2,
          status: status()?.textContent });

      stage("dictation: a slow Right Ctrl combination");
      // Right Ctrl held past its arm time while reaching for the other key
      // (WP11 UI review m4), with a quiet room: the microphone opens, but the
      // Admit warm (which may evict) waits for speech or a further arm time.
      const warms3 = mock.warm;
      calls.silent = true;
      key("keydown", "ControlRight", "Control", { ctrlKey: true });
      await until(() => state() === "recording", 8000, 10);
      await sleep(120);
      const warmedBeforeKey = mock.warm - warms3;
      key("keydown", "KeyS", "s", { ctrlKey: true });
      await until(() => state() === "idle", 5000);
      await sleep(600);
      key("keyup", "ControlRight", "Control");
      calls.silent = false;
      check("a slow Right Ctrl combination: the other key ends it, nothing warmed or uploaded, the microphone released",
        warmedBeforeKey === 0 && mock.warm === warms3 && mock.uploads.length === 2 && allEnded()
          && /another key was pressed with Right Ctrl/.test(status()?.textContent || ""),
        { warm: mock.warm - warms3, uploads: mock.uploads.length, status: status()?.textContent });

      stage("dictation: Esc while Right Ctrl arms");
      const gum4 = calls.n;
      key("keydown", "ControlRight", "Control", { ctrlKey: true });
      await sleep(60);
      const esc = new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true, ctrlKey: true });
      window.dispatchEvent(esc);
      await sleep(500);
      key("keyup", "ControlRight", "Control");
      check("Esc while Right Ctrl arms: a silent cancel that leaves Esc its own effect (review NIT 10)",
        !esc.defaultPrevented && state() === "idle" && calls.n === gum4
          && !/dictation discarded/.test(status()?.textContent || ""),
        { prevented: esc.defaultPrevented, state: state(), gum: calls.n - gum4, status: status()?.textContent });

      stage("dictation: a release before the microphone opened");
      const gum3 = calls.n;
      q("[data-mic-btn]").click();
      q("[data-mic-btn]").click();
      await until(() => state() === "idle", 5000);
      await sleep(1200);
      check("a release while the microphone still opens uploads nothing, says so, and releases it",
        mock.uploads.length === 2 && /the microphone was not open yet/.test(status()?.textContent || "")
          && allEnded(), { uploads: mock.uploads.length, gum: calls.n - gum3, status: status()?.textContent });

      stage("dictation: press and hold, by pointer events");
      const mic2 = q("[data-mic-btn]");
      const r = mic2.getBoundingClientRect();
      const ptr = (type) => mic2.dispatchEvent(new PointerEvent(type, { bubbles: true, cancelable: true,
        pointerId: 1, pointerType: "mouse", button: 0, buttons: type === "pointerdown" ? 1 : 0, isPrimary: true,
        clientX: r.left + r.width / 2, clientY: r.top + r.height / 2 }));
      ptr("pointerdown");
      await until(() => state() === "recording", 8000);
      await sleep(900);
      check("held past 350 ms, the status line says release (not click) to transcribe",
        /release to transcribe/.test(status()?.textContent || ""), status()?.textContent);
      ptr("pointerup");
      await until(() => state() === "idle", 10000);
      check("letting go of a held press transcribes", mock.uploads.length === 3 && allEnded()
        && mock.uploads[2].wav.ms >= 700, { uploads: mock.uploads.length, ms: mock.uploads[2]?.wav.ms });

      stage("dictation: Enter while recording");
      const sent = document.querySelectorAll(".msg-item-user").length;
      q("[data-mic-btn]").click();
      await until(() => state() === "recording", 8000);
      await sleep(700);
      ta.focus();
      ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", code: "Enter", bubbles: true, cancelable: true }));
      await until(() => state() === "idle", 10000);
      await sleep(300);
      check("Enter while recording finishes the dictation and sends nothing yet",
        mock.uploads.length === 4 && document.querySelectorAll(".msg-item-user").length === sent
          && ta.value.endsWith(MOCK_TEXT), { uploads: mock.uploads.length, value: ta.value });

      stage("dictation: the GPU hold");
      mock.mode = "hold";
      q("[data-mic-btn]").click();
      await until(() => state() === "recording", 8000);
      await sleep(700);
      q("[data-mic-btn]").click();
      await until(() => state() === "idle", 10000);
      const hold = status()?.querySelector(".vs-note.hold");
      check("a GPU hold is said as a hold, not an error", !!hold && /GPU hold/.test(hold.textContent)
        && !status()?.querySelector(".vs-note.err"), status()?.textContent);

      stage("dictation: a benchmark run");
      mock.mode = "bench";
      q("[data-mic-btn]").click();
      await until(() => state() === "recording", 8000);
      await sleep(700);
      q("[data-mic-btn]").click();
      await until(() => state() === "idle", 10000);
      const bench = status()?.querySelector(".vs-note.hold");
      check("a benchmark run's refusal is the hold chip, not an error",
        !!bench && /a benchmark run holds the GPU/.test(bench.textContent)
          && !status()?.querySelector(".vs-note.err"), status()?.textContent);

      stage("dictation: a failed fallback");
      mock.mode = "fallback";
      q("[data-mic-btn]").click();
      await until(() => state() === "recording", 8000);
      await sleep(700);
      q("[data-mic-btn]").click();
      await until(() => state() === "idle", 10000);
      const err = status()?.querySelector(".vs-note.err");
      check("a failed transcription names the fallback that had the audio",
        !!err && /the audio went to cloud\/whisper/.test(err.textContent), err?.textContent);
      check("... and the composer keeps what it had", ta.value === "Vorher " + [1, 2, 3, 4].map(() => MOCK_TEXT).join(" "),
        ta.value);

      stage("dictation: emptying the composer");
      typed("");
      await until(() => status()?.dataset.dictated !== "true", 2000);
      check("emptying the composer drops the mark", !status() || status().dataset.dictated !== "true");
      check("the live region stays mounted", !!live());
    }
    watchLive.disconnect();
  }

  // {phase: "realtime"} (chat-voice WP9): voice mode against an in-page
  // bound session (scripts/realtime-mock.js, loaded beside this file), in a
  // fresh temporary chat whose thread JSON resolves its speech models to the
  // mock's — so it runs on a gateway with no speech model. The state walk
  // idle → listening → thinking → speaking → idle (with the response done
  // only after its playout, as pacing drains), the captions, both bubbles,
  // the microphone at 24 kHz streaming appends, a barge-in's truncate at what
  // was heard (≤ what was sent) and the reply it re-cuts, stop talking (the
  // truncate first, no cancel the truncate made needless), a stop while it
  // thinks (the cancel alone), push-to-talk by Space, M, a tool turn, a
  // takeover while it speaks with Re-enter, and every way out (WP9 review
  // m1): Esc, Leave while it speaks (the truncate before the close, the
  // thread read back once the gateway closed, Keep waiting for it), another
  // thread mid-reply, Esc while the microphone still opens, a mid-session
  // refusal, a refused handshake (deleted, Admin Chat), the microphone
  // ending, the page hidden, and the page's unmount — each releasing every
  // track and the socket.
  async function realtime(check, info, tracks, calls, only) {
    const mock = window.lmgwRealtimeMock;
    if (!mock) {
      check("realtime: the session mock is loaded (scripts/realtime-mock.js)", false);
      return;
    }
    const mode = { admin: false };
    mock.patchThread(mode);
    const ctx = countContexts();
    try {
      stage("realtime: a temporary chat");
      const tid = await newTemp();
      check("a fresh temporary chat for voice mode", tid < 0, location.search);
      mock.install({ thread: tid, title: "", warmMs: 300, thinkMs: 700, audioMs: 3000 });
      const t0 = performance.now();
      await realtimeRound(check, info, tracks, calls, mock, mode, ctx, only);
      info.realtime_ms = Math.round(performance.now() - t0);
    } finally {
      ctx.restore();
      mock.uninstall();
    }
  }

  // Every AudioContext and WebGL context the page makes from now on, so
  // enter/leave cycles can be counted (WP11 UI review m7).
  function countContexts() {
    // The page's glue holds the AudioContext constructor it loaded with, so
    // a context is counted at its first gain node: the player and every
    // capture make one.
    const Base = window.BaseAudioContext || window.AudioContext;
    const createGain = Base.prototype.createGain;
    const audio = [];
    Base.prototype.createGain = function () {
      if (!audio.includes(this)) audio.push(this);
      return createGain.apply(this, arguments);
    };
    const getContext = HTMLCanvasElement.prototype.getContext;
    const gl = [];
    HTMLCanvasElement.prototype.getContext = function (kind, o) {
      const c = getContext.call(this, kind, o);
      if (c && /webgl/.test(kind) && !gl.includes(c)) gl.push(c);
      return c;
    };
    return {
      audio,
      gl,
      openAudio: () => audio.filter((c) => c.state !== "closed").length,
      liveGl: () => gl.filter((c) => !c.isContextLost()).length,
      detail: () => ({
        audio: audio.map((c) => c.state),
        gl: gl.map((c) => ({ lost: c.isContextLost(), connected: c.canvas.isConnected, w: c.canvas.width })),
      }),
      restore() {
        Base.prototype.createGain = createGain;
        HTMLCanvasElement.prototype.getContext = getContext;
      },
    };
  }

  // A new temporary chat, opened; its id.
  async function newTemp() {
    const was = location.search;
    const b = await until(() => [...document.querySelectorAll("button")]
      .find((x) => (x.title || "").startsWith("New temporary chat")), 15000);
    if (b) b.click();
    await until(() => location.search !== was && location.search.startsWith("?t=-"), 15000);
    return Number(new URLSearchParams(location.search).get("t"));
  }

  async function realtimeRound(check, info, tracks, calls, mock, mode, ctx, only) {
    const vbtn = () => q("[data-voice-mode-btn]");
    const ready = () => until(() => vbtn() && vbtn().dataset.disabledReason === "" && vbtn(), 15000);
    const btn = await ready();
    check("the composer's voice group has the voice-mode button, enabled", !!btn,
      vbtn()?.dataset.disabledReason);
    if (!btn) return;
    const panel = () => q("[data-rt-panel]");
    const st = () => panel()?.dataset.voiceState;
    const ph = () => panel()?.dataset.voicePhase;
    const walk = [];
    let obs = null;
    const key = (type, code, k) => window.dispatchEvent(new KeyboardEvent(type, { key: k, code, bubbles: true }));
    const live = () => tracks.filter((t) => t.readyState === "live");
    const allEnded = () => tracks.every((t) => t.readyState === "ended");
    const composerNote = () => q(".composer-area [data-voice-status]")?.textContent
      || [...document.querySelectorAll("[data-note=realtime]")].map((n) => n.textContent).join(" ");
    const enter = async () => {
      const b = await ready();
      if (b) b.click();
      return until(() => ph() === "live", 15000);
    };
    // Watch the state from now on: every change, in order.
    const watch = () => {
      const seen = [st()];
      const o = new MutationObserver(() => { if (seen[seen.length - 1] !== st()) seen.push(st()); });
      o.observe(panel(), { attributes: true, attributeFilter: ["data-voice-state"] });
      return { seen, stop: () => o.disconnect() };
    };
    if (only === "integration") {
      return integration(check, info, tracks, calls, mock, {
        panel, st, ph, vbtn, ready, key, live, allEnded, composerNote, enter, ctx,
      });
    }
    const gum0 = calls.n;
    stage("realtime: enter");
    btn.click();
    await until(() => ph() === "live", 15000);
    check("the panel takes the composer's place and goes live", ph() === "live"
      && getComputedStyle(q(".composer-area")).display === "none", { phase: ph() });
    check("one microphone opened, at the window's device", calls.n === gum0 + 1, calls.n - gum0);
    await until(() => /voice mode on/.test(q("[data-rt-announce]")?.textContent || ""), 2000, 20);
    check("entering focuses the panel and says its keys through the live region (review m5)",
      document.activeElement === panel() && /voice mode on: M mutes, Esc leaves/.test(q("[data-rt-announce]")?.textContent || ""),
      { active: document.activeElement && `${document.activeElement.tagName}.${document.activeElement.className}`,
        said: q("[data-rt-announce]")?.textContent });
    const t0 = mock.appends;
    await sleep(600);
    check("the microphone streams 40 ms appends at 24 kHz",
      mock.appends - t0 >= 8 && Math.abs(mock.appendBytes / mock.appends - 1920) < 2,
      { appends: mock.appends - t0, avgBytes: mock.appendBytes / Math.max(1, mock.appends) });
    const upd = mock.got.find((e) => e.type === "session.update");
    check("session.update says the client's turn detection and half duplex (echo mode device: off)",
      !!upd && upd.session.lmgw.half_duplex === false && upd.session.audio.input.turn_detection?.type, upd);
    await until(() => !q("[data-rt-status] .vs-note.busy"), 3000);
    const viz = q("[data-rt-panel] .rt-viz");
    await until(() => viz && viz.dataset.vizKind, 5000);
    check("the visualisation is the ribbon in the chat panel, drawing", panel().dataset.viz === "ribbon"
      && /Canvas2D/.test(viz.dataset.vizKind || "") && !!viz.querySelector("canvas[data-viz=ribbon]"),
      { viz: panel().dataset.viz, kind: viz && viz.dataset.vizKind });
    check("idle, with the hint", st() === "idle" && /Listening for your voice/.test(q("[data-rt-caption]").textContent),
      { state: st(), caption: q("[data-rt-caption]").textContent });
    check("the panel's status line and end note are announced, the captions line is not (review m8)",
      q("[data-rt-status]")?.getAttribute("role") === "status"
        && q("[data-rt-caption]")?.getAttribute("aria-live") === "off" && !!q("[data-rt-announce][aria-live=polite]"),
      { status: q("[data-rt-status]")?.getAttribute("role"), caption: q("[data-rt-caption]")?.getAttribute("aria-live") });

    stage("realtime: a turn");
    obs = new MutationObserver(() => { if (walk[walk.length - 1] !== st()) walk.push(st()); });
    obs.observe(panel(), { attributes: true, attributeFilter: ["data-voice-state"] });
    walk.push(st());
    // Done only once its playout drained, as a bound response is.
    const turn = mock.speak("Wie spät ist es?", 900, { doneAfterPlayout: true });
    await until(() => st() === "speaking", 10000);
    const capt = q("[data-rt-caption]").textContent;
    await turn;
    await until(() => st() === "idle", 10000);
    obs.disconnect();
    info.realtime_walk = walk.slice();
    check("the state walks idle → listening → thinking → speaking → idle, no thinking after the voice ends",
      walk.join(">") === "idle>listening>thinking>speaking>idle", walk);
    check("the captions show the reply's spoken words while it speaks", /Assistant/i.test(capt) && /Es ist/.test(capt), capt);
    check("the user's final words are announced", /You said: Wie spät ist es\?/.test(q("[data-rt-announce]")?.textContent || ""),
      q("[data-rt-announce]")?.textContent);
    const users = [...document.querySelectorAll(".msg-item-user")];
    const replies = [...document.querySelectorAll(".msg-assistant")];
    const lastUser = users[users.length - 1];
    const lastReply = replies[replies.length - 1];
    check("the user bubble is written, with its mic badge", !!lastUser && /Wie spät ist es\?/.test(lastUser.textContent)
      && !!lastUser.querySelector('[data-mic-badge="realtime"]'), lastUser && lastUser.textContent);
    check("the reply bubble holds the stored reply, with its speaker badge and timing line",
      !!lastReply && /Es ist halb neun/.test(lastReply.textContent) && !!lastReply.querySelector("[data-speaker-badge]")
      && /ASR 31 ms/.test(lastReply.textContent), lastReply && lastReply.textContent.slice(0, 200));
    check("the panel's timing readout says the last turn", /last turn: ASR 31 ms · first token 208 ms · first audio 782 ms/
      .test(q("[data-rt-timing]")?.textContent || ""), q("[data-rt-timing]")?.textContent);
    check("the reply's audio played to its end (no truncate)", Number(panel().dataset.voicePlayed) > 24000
      && mock.truncates.length === 0, { played: panel().dataset.voicePlayed, truncates: mock.truncates });

    stage("realtime: a barge-in");
    const second = mock.speak("Und morgen?", 600);
    await until(() => st() === "speaking", 10000);
    await sleep(1200);
    const replies0 = mock.replies.length;
    mock.bargeIn();
    const sawInterrupted = await until(() => st() === "interrupted", 2000, 20);
    await until(() => mock.truncates.length === 1, 3000);
    const tr = mock.truncates[0];
    info.realtime_truncate = tr;
    check("a barge-in shows interrupted", !!sawInterrupted, st());
    check("... and truncates at what was heard: more than nothing, no more than was sent",
      !!tr && tr.audio_end_ms > 300 && tr.audio_end_ms <= tr.sent_ms && tr.item_id === "item_a2", tr);
    check("... with no response.cancel of its own (the server cancels a barge-in)", mock.cancels === 0, mock.cancels);
    await second;
    await until(() => mock.replies.slice(replies0).some((r) => r.unheard && mock.truncates.length === 1)
      && !!document.querySelector(".msg-assistant [data-unheard]"), 3000);
    const cuts = mock.replies.slice(replies0);
    const full = "Es ist halb neun. Der Termin beginnt gleich im großen Raum, bitte bring die Unterlagen mit.";
    const want = full.slice(0, mock.heardChars(full, 3000, tr ? tr.audio_end_ms : 0)).trim();
    const last = cuts[cuts.length - 1];
    info.realtime_barge_replies = cuts.map((r) => ({ content: r.content, unheard: r.unheard }));
    check("... the server cut at what was sent, and the truncate re-cut it at what was heard (two replies, the last one wins)",
      cuts.length >= 1 && !!last && last.content === want && full.startsWith(last.content + " "),
      { replies: info.realtime_barge_replies, want });
    const cutReply = [...document.querySelectorAll(".msg-assistant")].pop();
    const shown = cutReply.querySelector(".msg-body, .md")?.textContent || cutReply.textContent;
    check("the cut reply shows its unheard rest greyed, the heard part ending at a word",
      !!cutReply.querySelector("[data-unheard]") && shown.includes(want), shown.slice(0, 200));
    await until(() => st() === "listening", 2000);
    check("after interrupted: listening (the user still speaks)", st() === "listening", st());

    stage("realtime: stop talking");
    // The user's turn after the barge-in ends and is answered.
    const s3 = mock.current;
    mock.userTurn = false;
    s3.emit({ type: "input_audio_buffer.speech_stopped", audio_end_ms: 500, item_id: "item_open" });
    const third = mock.respond();
    await until(() => st() === "speaking", 10000);
    await sleep(800);
    q("[data-rt-stop]").click();
    await until(() => mock.truncates.length === 2, 3000);
    const r3 = await third;
    check("stop talking truncates at what was heard, which stops the response still being produced: no response.cancel",
      mock.cancels === 0 && mock.cancelRefused === 0 && mock.truncates.length === 2
        && mock.truncates[1].audio_end_ms > 0 && mock.truncates[1].audio_end_ms <= mock.truncates[1].sent_ms
        && r3.cancelled === "client_cancelled",
      { cancels: mock.cancels, refused: mock.cancelRefused, t: mock.truncates[1], cancelled: r3.cancelled });
    await until(() => st() === "idle", 3000);

    stage("realtime: stop while it thinks");
    mock.opts.thinkMs = 1500;
    let stopOffered = null;
    const responses0 = mock.responses;
    const fourth = mock.speak("Noch etwas?", 300);
    await until(() => {
      if (st() === "thinking" && mock.responses === responses0 && stopOffered === null) {
        stopOffered = !q("[data-rt-stop]").disabled;
      }
      return mock.responses > responses0;
    }, 5000, 10);
    await until(() => !q("[data-rt-stop]").disabled, 2000);
    const cancels0 = mock.cancels;
    const truncs0 = mock.truncates.length;
    q("[data-rt-stop]").click();
    const r4 = await fourth;
    mock.opts.thinkMs = 700;
    check("thinking before the response exists offers no stop (review NIT 8)", stopOffered === false, stopOffered);
    check("a stop before any audio sends the cancel alone", mock.cancels === cancels0 + 1
      && mock.truncates.length === truncs0 && mock.cancelRefused === 0 && r4 && r4.cancelled === "client_cancelled",
      { cancels: mock.cancels - cancels0, truncates: mock.truncates.length - truncs0, r4 });
    await until(() => st() === "idle", 3000);

    stage("realtime: mute");
    key("keydown", "KeyM", "m");
    await sleep(100);
    const lv = live();
    check("M mutes: the track is disabled (silence flows), the panel says it",
      panel().dataset.voiceMuted === "true" && lv.length === 1 && lv[0].enabled === false,
      { muted: panel().dataset.voiceMuted, live: lv.map((t) => t.enabled) });
    key("keydown", "KeyM", "m");
    await sleep(100);
    check("M again unmutes", panel().dataset.voiceMuted === "false" && lv[0].enabled === true);

    stage("realtime: switching to push-to-talk mid-utterance");
    mock.startSpeech();
    await until(() => st() === "listening", 2000);
    const clears0 = mock.clears;
    q('[data-rt-mode="ptt"]').click();
    await until(() => panel().dataset.voicePtt === "true" && mock.clears > clears0, 2000);
    await until(() => st() === "idle", 1500);
    check("the turn the switch ended is idle once the gateway says cleared, not thinking (review m4)",
      st() === "idle" && mock.clears > clears0, { state: st(), clears: mock.clears - clears0 });
    check("the session's echo keeps push-to-talk", panel().dataset.voicePtt === "true" && mock.current.detection === null,
      { ptt: panel().dataset.voicePtt, detection: mock.current.detection });

    stage("realtime: push-to-talk");
    const upd2 = mock.got.filter((e) => e.type === "session.update").pop();
    check("push-to-talk: session.update with turn_detection null", upd2 && upd2.session.audio.input.turn_detection === null, upd2);
    await sleep(400);
    const gatedFrom = mock.appends;
    await sleep(500);
    check("... the microphone is gated: no appends while Space is up", mock.appends === gatedFrom,
      mock.appends - gatedFrom);
    const commits0 = mock.commits;
    key("keydown", "Space", " ");
    await until(() => st() === "listening", 2000);
    check("Space held: listening, the buffer cleared first", st() === "listening"
      && mock.got.some((e) => e.type === "input_audio_buffer.clear"), st());
    await sleep(800);
    check("... the pre-roll and live chunks flow while held", mock.appends - gatedFrom >= 15, mock.appends - gatedFrom);
    key("keyup", "Space", " ");
    await until(() => mock.commits === commits0 + 1, 3000);
    const lastTwo = mock.got.slice(-2).map((e) => e.type);
    check("Space up: commit, then response.create", lastTwo.join() === "input_audio_buffer.commit,response.create", lastTwo);
    await until(() => st() === "speaking", 10000);
    // Space while it speaks in push-to-talk mode stops it before listening.
    const pttTruncs = mock.truncates.length;
    const pttCancels = mock.cancels;
    key("keydown", "Space", " ");
    await until(() => st() === "listening", 2000);
    await until(() => mock.truncates.length > pttTruncs, 2000);
    check("Space over the voice (push-to-talk) stops it first — the truncate, no cancel — then listens",
      mock.truncates.length === pttTruncs + 1 && mock.cancels === pttCancels && st() === "listening",
      { truncates: mock.truncates.length - pttTruncs, cancels: mock.cancels - pttCancels, state: st() });
    key("keyup", "Space", " ");
    await until(() => st() === "speaking", 10000);
    await until(() => st() === "idle", 10000);
    q('[data-rt-mode="auto"]').click();
    await until(() => panel().dataset.voicePtt === "false", 2000);

    stage("realtime: a tool turn");
    const tw = watch();
    let toolThinking = false;
    const tool = mock.speak("Wie warm ist es?", 300, { tool: true, toolMs: 1800 });
    await until(() => {
      if (q("[data-rt-tool]") && st() === "thinking") toolThinking = true;
      return toolThinking;
    }, 8000, 20);
    const toolNote = q("[data-rt-tool]")?.textContent || "";
    await tool;
    await until(() => st() === "idle", 10000);
    tw.stop();
    info.realtime_tool_walk = tw.seen;
    const tj = tw.seen.join(">");
    check("a tool turn: speaking (the preamble), thinking while the tool runs, speaking (the answer) (review m3)",
      toolThinking && /speaking>thinking>speaking/.test(tj) && /running stub__echo/.test(toolNote),
      { walk: tw.seen, note: toolNote });
    const toolReply = [...document.querySelectorAll(".msg-assistant")].pop();
    check("... and its bubble holds the tool card and the answer", !!toolReply?.querySelector(".tool-card")
      && /einundzwanzig Grad/.test(toolReply.textContent), toolReply && toolReply.textContent.slice(0, 160));

    stage("realtime: the focus view and the variants");
    q("[data-rt-focus]").click();
    await until(() => panel().dataset.viz === "orb", 3000);
    check("the focus view takes the column, with the orb", panel().dataset.viz === "orb"
      && getComputedStyle(q(".chat-scroll")).display === "none", panel().dataset.viz);
    await until(() => q(".rt-viz")?.dataset.vizKind, 5000);
    info.realtime_orb = q(".rt-viz")?.dataset.vizKind;
    q("[data-rt-focus]").click();
    await until(() => panel().dataset.viz === "ribbon", 3000);
    check("back in the chat panel: the ribbon again", panel().dataset.viz === "ribbon");

    stage("realtime: a takeover while it speaks");
    const fifth = mock.speak("Und dann?", 300);
    await until(() => st() === "speaking", 10000);
    await sleep(500);
    mock.takeOver();
    await until(() => ph() === "ended", 3000);
    const bytes0 = Number(panel().dataset.voiceAudioBytes);
    await sleep(400);
    await fifth;
    check("a takeover ends the session with its reason and offers Re-enter",
      ph() === "ended" && /moved to another window/.test(q("[data-rt-ended]")?.textContent || "") && !!q("[data-rt-reenter]")
        && q("[data-rt-ended]")?.getAttribute("role") === "alert",
      q("[data-rt-ended]")?.textContent);
    check("... and releases the microphone and the voice (nothing more is played)", allEnded()
      && st() === "idle" && Number(panel().dataset.voiceAudioBytes) === bytes0,
      { tracks: tracks.map((t) => t.readyState), state: st() });
    q("[data-rt-reenter]").click();
    await until(() => ph() === "live", 10000);
    check("Re-enter opens a new session", ph() === "live" && mock.sockets.length === 2, mock.sockets.length);

    stage("realtime: Esc");
    key("keydown", "Escape", "Escape");
    await until(() => !panel(), 3000);
    await sleep(200);
    const where = () => {
      const a = document.activeElement;
      let open = null;
      try {
        open = document.querySelector("dialog[open], [popover]:popover-open")?.outerHTML.slice(0, 160) || null;
      } catch (e) {
        open = "selector failed: " + e;
      }
      return { panel: !!panel(), active: a ? `${a.tagName}.${a.className}` : null, open };
    };
    check("Esc leaves: the panel goes, the composer comes back",
      !panel() && getComputedStyle(q(".composer-area")).display === "contents", where());
    check("... every track ended, the socket closed by the page (1000)",
      allEnded() && mock.closedByPage && mock.closedByPage.code === 1000,
      { tracks: tracks.map((t) => t.readyState), closed: mock.closedByPage });

    stage("realtime: Leave while it speaks");
    await enter();
    const sixth = mock.speak("Erzähl mehr.", 300);
    await until(() => st() === "speaking", 10000);
    await sleep(700);
    // What a re-mount would replace: an earlier bubble's node, and its
    // timing line opened (WP11 UI review m2).
    const earlier = [...document.querySelectorAll(".msg-assistant")].find((b) => b.querySelector("details.voice-timing"));
    const opened = earlier && earlier.querySelector("details.voice-timing");
    if (opened) opened.open = true;
    const leaveTr = mock.truncates.length;
    const leaveCancels = mock.cancels;
    const reads0 = mock.threadReads;
    mock.closedAt = null;
    mock.closedByPage = null;
    q("[data-rt-leave]").click();
    await sleep(0);
    const keep = q("[data-keep]");
    const keepWaits = !!keep && keep.disabled && /still closing/.test(keep.title || "");
    const r6 = await sixth;
    const lt = mock.truncates[leaveTr];
    const stored = mock.stored[mock.stored.length - 1] || {};
    const reply6 = "Es ist halb neun. Der Termin beginnt gleich im großen Raum, bitte bring die Unterlagen mit.";
    check("Leave: the panel goes, every track ended, the socket closed by the page",
      !panel() && allEnded() && mock.closedByPage?.code === 1000, { tracks: tracks.map((t) => t.readyState) });
    check("... the reply still playing was truncated at what was heard before the close, with no cancel (review m9)",
      !!lt && lt.audio_end_ms > 0 && lt.audio_end_ms <= lt.sent_ms && mock.cancels === leaveCancels
        && r6.cancelled === "client_cancelled"
        && stored.content === reply6.slice(0, mock.heardChars(reply6, 3000, lt.audio_end_ms)).trim(),
      { truncate: lt, cancels: mock.cancels - leaveCancels, stored });
    await until(() => mock.closedAt && mock.threadReads > mock.readsAtClose, 4000);
    await sleep(300);
    check("... the thread is read back once, when the gateway closed its side (review m5, WP11 m1)",
      !!mock.closedAt && mock.readsAtClose === reads0 && mock.threadReads === mock.readsAtClose + 1,
      { reads: mock.threadReads - reads0, atClose: mock.readsAtClose - reads0 });
    const heard6 = reply6.slice(0, mock.heardChars(reply6, 3000, lt ? lt.audio_end_ms : 0)).trim();
    const shown6 = [...document.querySelectorAll(`.msg-assistant[data-mid="${stored.message_id}"]`)];
    check("... the cut reply shows once, as stored, its unheard rest greyed (WP11 m2)",
      shown6.length === 1 && !!shown6[0].querySelector("[data-unheard]")
        && (shown6[0].querySelector(".md")?.textContent || "").trim().startsWith(heard6.slice(0, 30)),
      { count: shown6.length, text: shown6.map((b) => (b.querySelector(".md")?.textContent || "").slice(0, 80)) });
    check("... patched in place: an earlier bubble is the same node and its open timing line stays open (WP11 m2)",
      !!earlier && earlier.isConnected && !!opened && opened.open,
      { earlier: !!earlier, connected: earlier && earlier.isConnected, open: opened && opened.open });
    await until(() => q("[data-keep]") && !q("[data-keep]").disabled, 2000);
    check("... Keep waits until then, saying why (review NIT 9)", keepWaits && !!q("[data-keep]") && !q("[data-keep]").disabled,
      { waited: keepWaits, title: keep && keep.title });

    stage("realtime: another thread mid-reply");
    await enter();
    const seventh = mock.speak("Und weiter?", 300);
    await until(() => st() === "speaking", 10000);
    await sleep(500);
    const switchTr = mock.truncates.length;
    mock.closedByPage = null;
    const tid2 = await newTemp();
    mock.opts.thread = tid2;
    await seventh;
    await sleep(300);
    check("another thread ends voice mode: the panel goes, every track ended, the socket closed, the reply truncated",
      !panel() && allEnded() && mock.closedByPage?.code === 1000 && mock.truncates.length === switchTr + 1,
      { panel: !!panel(), tracks: tracks.map((t) => t.readyState), truncates: mock.truncates.length - switchTr });
    check("... and nothing of that session reaches the new thread",
      document.querySelectorAll(".msg-assistant, .msg-item-user").length === 0,
      document.querySelectorAll(".msg-assistant, .msg-item-user").length);

    stage("realtime: Esc while the microphone opens");
    calls.delayMs = 900;
    const gum1 = calls.n;
    const created0 = mock.created || 0;
    const b2 = await ready();
    if (b2) b2.click();
    await until(() => (mock.created || 0) > created0 && calls.n > gum1 && panel()?.dataset.voiceMic === "opening", 5000, 20);
    const opening = panel()?.dataset.voiceMic;
    key("keydown", "Escape", "Escape");
    await until(() => !panel(), 2000);
    await sleep(1300);
    calls.delayMs = 0;
    check("Esc while the microphone opens: the track that lands after is ended at once",
      opening === "opening" && !panel() && calls.n === gum1 + 1 && allEnded(),
      { opening, calls: calls.n - gum1, tracks: tracks.map((t) => t.readyState) });

    stage("realtime: push-to-talk chosen while connecting");
    mock.opts.createdMs = 600;
    const b3 = await ready();
    if (b3) b3.click();
    await until(() => ph() === "connecting" && mock.current && mock.current.readyState === 1, 3000, 20);
    q('[data-rt-mode="ptt"]').click();
    await until(() => ph() === "live", 5000);
    mock.opts.createdMs = 0;
    await sleep(300);
    const gated0 = mock.appends;
    await sleep(500);
    check("push-to-talk chosen while connecting holds after session.created, the microphone gated (review NIT 4)",
      panel().dataset.voicePtt === "true" && mock.appends === gated0 && mock.current.detection === null,
      { ptt: panel().dataset.voicePtt, appends: mock.appends - gated0, detection: mock.current.detection });
    q('[data-rt-mode="auto"]').click();
    await until(() => panel().dataset.voicePtt === "false", 2000);

    stage("realtime: a mid-session refusal");
    mock.closedByPage = null;
    mock.emitError("chat_thread_not_found", "this conversation no longer exists");
    await until(() => !panel(), 3000);
    await sleep(100);
    check("a thread deleted mid-session: voice mode ends, the composer says why, everything released",
      !panel() && /voice mode ended: this conversation no longer exists/.test(composerNote() || "") && allEnded()
        && mock.closedByPage?.code === 1000,
      { note: composerNote(), tracks: tracks.map((t) => t.readyState) });

    stage("realtime: a refused handshake");
    const gum2 = calls.n;
    // Press, see the session start, then see it end: a note left from
    // before must not pass for this one's.
    const refusedOnce = async () => {
      const b = await ready();
      if (b) b.click();
      await until(() => !!panel(), 2000, 10);
      await until(() => !panel(), 4000);
      return composerNote();
    };
    mock.opts.refuse = true;
    mode.gone = true;
    const goneNote = await refusedOnce();
    mode.gone = false;
    mode.kindAdmin = true;
    const adminNote = await refusedOnce();
    mode.kindAdmin = false;
    mock.opts.refuse = false;
    check("a refused handshake names why from the thread: deleted, then Admin Chat — and no microphone opened",
      /voice mode ended: this conversation no longer exists/.test(goneNote || "")
        && /voice mode ended: Voice mode is not available in Admin Chat/.test(adminNote || "") && calls.n === gum2,
      { gone: goneNote, admin: adminNote, calls: calls.n - gum2 });

    stage("realtime: the microphone ends");
    mock.closedByPage = null;
    await enter();
    const lt2 = live();
    for (const t of lt2) t.dispatchEvent(new Event("ended"));
    await until(() => ph() === "ended", 3000);
    check("the microphone ending ends the session, says why, offers Re-enter, releases everything",
      ph() === "ended" && /microphone/.test(q("[data-rt-ended]")?.textContent || "") && allEnded()
        && mock.closedByPage?.code === 1000,
      { ended: q("[data-rt-ended]")?.textContent, tracks: tracks.map((t) => t.readyState) });

    stage("realtime: the page hidden");
    mock.closedByPage = null;
    q("[data-rt-reenter]").click();
    await until(() => ph() === "live", 10000);
    window.dispatchEvent(new Event("pagehide"));
    await until(() => ph() === "ended", 3000);
    check("pagehide ends the session and releases everything",
      ph() === "ended" && /hidden/.test(q("[data-rt-ended]")?.textContent || "") && allEnded()
        && mock.closedByPage?.code === 1000,
      { ended: q("[data-rt-ended]")?.textContent, tracks: tracks.map((t) => t.readyState) });
    key("keydown", "Escape", "Escape");
    await until(() => !panel(), 3000);

    stage("realtime: the page unmounts");
    await enter();
    mock.closedByPage = null;
    const nav = (href) => {
      const a = [...document.querySelectorAll("a[href]")].find((x) => x.getAttribute("href") === href);
      if (a) a.click();
      return !!a;
    };
    const away = nav("/settings");
    await until(() => !q(".chat-shell"), 5000);
    await sleep(200);
    check("leaving the Chat page releases every track and closes the socket",
      away && !panel() && allEnded() && mock.closedByPage?.code === 1000,
      { away, tracks: tracks.map((t) => t.readyState), closed: mock.closedByPage });
    nav("/chat");
    await until(() => q(".chat-shell") && vbtn(), 10000);
    const back = await newTemp();
    mock.opts.thread = back;
    check("refused cancels and truncates: none", mock.cancelRefused === 0 && mock.truncateErrors.length === 0,
      { cancelRefused: mock.cancelRefused, truncateErrors: mock.truncateErrors });

    await integration(check, info, tracks, calls, mock, {
      panel, st, ph, vbtn, ready, key, live, allEnded, composerNote, enter, ctx,
    });

    stage("realtime: an Admin Chat thread");
    mode.admin = true;
    const was = location.search;
    const nb = [...document.querySelectorAll("button")].find((x) => (x.title || "").startsWith("New temporary chat"));
    if (nb) nb.click();
    await until(() => location.search !== was, 10000);
    const ab = await until(() => q("[data-voice-mode-btn]")?.dataset.disabledReason && q("[data-voice-mode-btn]"), 10000);
    check("on an Admin Chat thread the button is disabled with its one-line reason",
      !!ab && ab.getAttribute("aria-disabled") === "true" && /not available in Admin Chat/.test(ab.title),
      ab && ab.title);
    const s0 = mock.sockets.length;
    if (ab) ab.click();
    await sleep(300);
    check("... and a press opens nothing", mock.sockets.length === s0 && !panel());
    mode.admin = false;
  }

  // Where voice mode meets the page's dictation and read-aloud, and what the
  // WP11 server batch changed (WP11 UI review m1, m3, m6, m7): a dictation
  // and a read-aloud cut off by entering, Right Ctrl and a speaker button in
  // voice mode, an echo change mid-session, the pointer's Talk button, a
  // hold and a refusal said once, a failed transcription, a transcript that
  // never comes, a late turn the read-back adds, re-entering while the old
  // session drains, and the contexts left over after enter/leave cycles.
  async function integration(check, info, tracks, calls, mock, h) {
    const { panel, st, ph, ready, key, live, allEnded, enter, ctx } = h;
    // The Leave button: Esc's own rules (a popover open, the focus in a
    // field) are the main round's; here leaving must happen.
    const leave = async () => {
      mock.closedAt = null;
      q("[data-rt-leave]")?.click();
      await until(() => !panel(), 3000);
    };
    // The page's close was answered and the thread read after it.
    const closedAndRead = () => until(() => mock.closedAt && mock.threadReads > mock.readsAtClose, 4000);
    // An echo mode from the panel's chip (its popover opened if it is not).
    const pickEcho = async (m) => {
      let b = q(`.rt-pop .echo-mode[data-mode="${m}"]`);
      if (!b) {
        q('[data-rt-chip="echo"]').click();
        b = await until(() => q(`.rt-pop .echo-mode[data-mode="${m}"]`), 2000);
      }
      if (b) b.click();
      return !!b;
    };
    const notes = () => [...document.querySelectorAll("[data-rt-status] .vs-note")];
    const noteText = (k) => q(`[data-rt-status] [data-note="${k}"]`)?.textContent || "";
    // The page's dictation and read-aloud routes, answered here.
    const asr = { mode: "ok", warm: 0, uploads: [] };
    const restoreAsr = mockAsr(asr);
    const speak = { calls: 0, aborted: 0 };
    const realFetch = window.fetch;
    window.fetch = async function (input, init) {
      const url = typeof input === "string" ? input : input.url;
      if (!/\/messages\/\d+\/speak$/.test(url)) return realFetch.apply(this, arguments);
      speak.calls++;
      const signal = (init && init.signal) || (typeof input !== "string" && input.signal);
      // A stored reply read aloud: a second of a quiet tone every 400 ms,
      // until the page aborts the fetch (nothing is audible: the probe's
      // output is muted or a null sink).
      const pcm = new Int16Array(24000);
      for (let i = 0; i < pcm.length; i++) pcm[i] = Math.round(2000 * Math.sin((2 * Math.PI * 330 * i) / 24000));
      let b = "";
      const u8 = new Uint8Array(pcm.buffer);
      for (let i = 0; i < u8.length; i += 0x8000) b += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
      const chunk = btoa(b);
      const enc = new TextEncoder();
      const body = new ReadableStream({
        async start(c) {
          if (signal) signal.addEventListener("abort", () => { speak.aborted++; try { c.error(new DOMException("aborted", "AbortError")); } catch (_) { /* closed */ } });
          c.enqueue(enc.encode(sseBody([["voice", { tts: "mock/tts", voice: "F2", tts_answered_by: null }]])));
          for (let i = 0; i < 20 && !(signal && signal.aborted); i++) {
            c.enqueue(enc.encode(sseBody([["speech", { pcm: chunk }]])));
            await sleep(400);
          }
          if (!(signal && signal.aborted)) {
            c.enqueue(enc.encode(sseBody([["speech_done", { first_audio_ms: 40, audio_ms: 20000, tts: "mock/tts" }]])));
            c.close();
          }
        },
      });
      return new Response(body, { headers: { "content-type": "text/event-stream" } });
    };
    try {
      stage("realtime: voice mode during a dictation");
      {
        const mic = await until(() => q("[data-mic-btn]"), 5000);
        const gum = calls.n;
        mic.click();
        await until(() => mic.dataset.dictation === "recording", 8000);
        const btn = await ready();
        btn.click();
        await until(() => ph() === "live", 15000);
        await sleep(300);
        check("entering voice mode while a dictation records discards it: its track ended, nothing uploaded, said on the panel",
          q("[data-mic-btn]").dataset.dictation === "idle" && asr.uploads.length === 0 && calls.n === gum + 2
            && live().length === 1 && /dictation discarded: voice mode started/.test(noteText("dictation")),
          { dictation: q("[data-mic-btn]").dataset.dictation, uploads: asr.uploads.length, gum: calls.n - gum,
            live: live().length, note: noteText("dictation") });
        // A reply to read aloud later on.
        await mock.speak("Wie spät ist es?", 300);
        await until(() => st() === "idle", 8000);

        stage("realtime: Right Ctrl in voice mode");
        const gum2 = calls.n;
        key("keydown", "ControlRight", "Control");
        await sleep(800);
        key("keyup", "ControlRight", "Control");
        await sleep(200);
        check("Right Ctrl in voice mode opens no second microphone and warms nothing",
          calls.n === gum2 && live().length === 1 && asr.warm === 1 && q("[data-mic-btn]").dataset.dictation === "idle",
          { gum: calls.n - gum2, live: live().length, warm: asr.warm });

        stage("realtime: a speaker button in voice mode");
        const sp = [...document.querySelectorAll(".msg-assistant .msg-speak")].find((b) => !b.disabled);
        const speak0 = speak.calls;
        if (sp) sp.click();
        await sleep(400);
        check("a speaker button rests in voice mode: focusable, it says why, nothing plays into the microphone (review m3)",
          !!sp && sp.getAttribute("aria-disabled") === "true" && sp.dataset.speak === "idle" && speak.calls === speak0
            && /leave voice mode to read a reply aloud/.test(noteText("read-aloud"))
            && /leave voice mode/.test(document.getElementById(sp.getAttribute("aria-describedby") || "-")?.textContent || ""),
          { found: !!sp, aria: sp && sp.getAttribute("aria-disabled"), speak: sp && sp.dataset.speak,
            calls: speak.calls - speak0, note: noteText("read-aloud") });

        stage("realtime: the echo mode changes mid-session");
        const gum3 = calls.n;
        const upd0 = mock.updates;
        await pickEcho("none");
        await until(() => calls.n > gum3 && live().length === 1 && panel().dataset.voiceMic === "open", 5000);
        const upd = mock.got.filter((e) => e.type === "session.update").pop();
        check("an echo change mid-session reopens the microphone with it and says half duplex for none",
          calls.n === gum3 + 1 && live().length === 1 && mock.updates > upd0 && upd?.session?.lmgw?.half_duplex === true,
          { gum: calls.n - gum3, live: live().length, update: upd && upd.session.lmgw });
        await pickEcho("device");
        await until(() => mock.got.filter((e) => e.type === "session.update").pop()?.session?.lmgw?.half_duplex === false, 3000);
        // The echo popover, if it stayed open: its chip closes it.
        if (q('.rt-pop .echo-mode[data-mode="device"]')) q('[data-rt-chip="echo"]').click();
        await sleep(200);
        await leave();
        localStorage.removeItem("lmgw.voice.echo");
      }

      stage("realtime: voice mode during a read-aloud");
      {
        const sp = await until(() => [...document.querySelectorAll(".msg-assistant .msg-speak")].find((b) => !b.disabled), 5000);
        const speak0 = speak.calls;
        const aborted0 = speak.aborted;
        if (sp) sp.click();
        const playing = await until(() => sp && sp.dataset.speak === "playing", 8000);
        const btn = await ready();
        btn.click();
        await until(() => ph() === "live", 15000);
        await sleep(300);
        check("entering voice mode while a reply is read aloud stops it: its fetch aborted, the button idle",
          !!playing && speak.calls === speak0 + 1 && speak.aborted === aborted0 + 1 && sp.dataset.speak === "idle",
          { playing: !!playing, calls: speak.calls - speak0, aborted: speak.aborted - aborted0, speak: sp && sp.dataset.speak });
        const r = await mock.speak("Und jetzt?", 300);
        await until(() => st() === "idle", 8000);
        check("... and the session's reply plays after it", !!r && Number(panel()?.dataset.voicePlayed) > 0,
          { r, played: panel()?.dataset.voicePlayed });
      }

      stage("realtime: the Talk button");
      {
        q('[data-rt-mode="ptt"]').click();
        await until(() => panel().dataset.voicePtt === "true" && q("[data-rt-talk]"), 3000);
        const talk = q("[data-rt-talk]");
        const rect = talk.getBoundingClientRect();
        const ptr = (type, x) => talk.dispatchEvent(new PointerEvent(type, { bubbles: true, cancelable: true,
          pointerId: 7, pointerType: "mouse", button: 0, buttons: type === "pointerup" ? 0 : 1, isPrimary: true,
          clientX: x, clientY: rect.top + rect.height / 2 }));
        const commits0 = mock.commits;
        const clears0 = mock.clears;
        ptr("pointerdown", rect.left + rect.width / 2);
        await until(() => st() === "listening", 2000);
        await sleep(500);
        // Drifting off the button keeps talking (review NIT 6).
        ptr("pointerleave", rect.right + 40);
        await sleep(300);
        const stillListening = st() === "listening" && mock.commits === commits0;
        ptr("pointerup", rect.right + 40);
        await until(() => mock.commits === commits0 + 1, 3000);
        check("the Talk button: held listens (the buffer cleared), drifting off it keeps talking, letting go commits",
          mock.clears > clears0 && stillListening && mock.commits === commits0 + 1,
          { clears: mock.clears - clears0, stillListening, commits: mock.commits - commits0 });
        await until(() => st() === "speaking", 10000);
        await until(() => st() === "idle", 10000);
        q('[data-rt-mode="auto"]').click();
        await until(() => panel().dataset.voicePtt === "false", 2000);
      }

      stage("realtime: a refusal under the hold");
      {
        await leave();
        mock.opts.heldChat = "gpu_hold";
        await enter();
        await until(() => notes().some((n) => n.classList.contains("hold")), 3000);
        const held = "'mock/chat' is a local model and lmgw is holding the GPU — release the hold or configure a fallback alias";
        const r = await mock.speak("Hallo?", 300, { refuse: { code: "gpu_hold", held: "gpu_hold", message: held } });
        await until(() => st() === "idle", 5000);
        await sleep(200);
        const holds = notes().filter((n) => n.classList.contains("hold"));
        check("a hold says one amber chip on the panel, not two (the held state and the gpu_hold error, review m6)",
          !!r && r.refused === "gpu_hold" && holds.length === 1 && /^GPU hold: voice mode paused — 'mock\/chat' is a local model/.test(holds[0].textContent)
            && !notes().some((n) => n.classList.contains("err")),
          { holds: holds.map((n) => n.textContent), notes: notes().map((n) => `${n.className}: ${n.textContent}`) });

        stage("realtime: a context overflow is worded");
        const over = "'mock/chat': prompt 5000 tokens exceeds the per-request context of 4096 tokens — shorten the conversation";
        await mock.speak("Und?", 300, { refuse: { code: "context_length_exceeded", message: over } });
        await until(() => st() === "idle", 5000);
        await sleep(200);
        check("a context overflow says what to do, with the gateway's numbers, in place of the hold's chip",
          /no longer fits the model's context: start a new conversation/.test(noteText("chat"))
            && /4096 tokens/.test(noteText("chat")) && !notes().some((n) => n.classList.contains("hold"))
            && document.querySelectorAll('[data-rt-status] [data-note="chat"]').length === 1,
          notes().map((n) => `${n.className}: ${n.textContent}`));

        stage("realtime: a failed transcription");
        const why = "this turn cannot be transcribed: chat thread 1 names no transcription model (set one in the thread's voice settings, or under Settings → Chat → Voice)";
        await mock.speak("…", 300, { transcription: { code: "asr_not_configured", message: why } });
        await until(() => noteText("turn"), 3000);
        check("a transcription refused for no speech-to-text model names where to set one",
          /no speech-to-text model for this conversation/.test(noteText("turn")) && /Settings → Chat → Voice/.test(noteText("turn"))
            && q('[data-rt-status] [data-note="turn"]').classList.contains("err") && st() === "idle",
          { note: noteText("turn"), state: st() });
        mock.opts.heldChat = undefined;
      }

      stage("realtime: a transcript that never comes");
      {
        const users0 = document.querySelectorAll(".msg-item-user").length;
        mock.startSpeech();
        await until(() => st() === "listening", 2000);
        mock.commitOnly();
        await sleep(300);
        await leave();
        await closedAndRead();
        await sleep(300);
        check("a turn whose transcript never came leaves nothing broken: no bubble invented, no error said",
          document.querySelectorAll(".msg-item-user").length === users0 && !document.querySelector(".msg-assistant .md:empty")
            && !/error|failed/i.test(q(".composer-area [data-voice-status]")?.textContent || ""),
          { users: document.querySelectorAll(".msg-item-user").length - users0,
            status: q(".composer-area [data-voice-status]")?.textContent || "" });
      }

      stage("realtime: a turn written during the drain");
      {
        await enter();
        await mock.speak("Eine Frage noch.", 300);
        await until(() => st() === "idle", 8000);
        mock.opts.lateUser = "Und was ist mit morgen?";
        const keepNode = [...document.querySelectorAll(".msg-item-user")].pop();
        await leave();
        await until(() => [...document.querySelectorAll(".msg-item-user")].some((u) => /Und was ist mit morgen\?/.test(u.textContent)), 4000);
        mock.opts.lateUser = undefined;
        const users = [...document.querySelectorAll(".msg-item-user")];
        check("a turn the gateway wrote while it drained is added from the read-back, the rest left in place",
          /Und was ist mit morgen\?/.test(users[users.length - 1]?.textContent || "") && !!keepNode && keepNode.isConnected,
          { last: users[users.length - 1]?.textContent, kept: keepNode && keepNode.isConnected,
            rows: (mock.rows[Number(new URLSearchParams(location.search).get("t"))] || []).map((r) => `${r.id}:${r.role}`),
            page: [...document.querySelectorAll(".msg-item")].map((e) => `${e.dataset.mid || "-"}:${e.classList.contains("msg-item-user") ? "user" : "assistant"}`) });
      }

      stage("realtime: re-enter while the old session drains");
      {
        mock.opts.drainMs = 1500;
        await enter();
        const r1 = mock.speak("Erzähl.", 300);
        await until(() => st() === "speaking", 10000);
        await sleep(500);
        await leave();
        const closed0 = mock.closedAt;
        await enter();
        const r2 = mock.speak("Und dann?", 300);
        await until(() => st() === "speaking", 10000);
        const bubble = [...document.querySelectorAll(".msg-assistant")].pop();
        await until(() => mock.closedAt && mock.closedAt !== closed0, 5000);
        await sleep(300);
        await r1;
        await r2;
        await until(() => st() === "idle", 10000);
        await sleep(300);
        mock.opts.drainMs = undefined;
        check("re-entering while the old session drains: its close reads nothing over the new session's reply (review m1)",
          !!bubble && bubble.isConnected && /Es ist halb neun/.test(bubble.textContent) && !!bubble.dataset.mid,
          { connected: bubble && bubble.isConnected, mid: bubble && bubble.dataset.mid,
            text: bubble && bubble.textContent.slice(0, 80) });
        await leave();
      }

      stage("realtime: contexts over enter/leave cycles");
      {
        await closedAndRead();
        for (let i = 0; i < 5; i++) {
          await enter();
          q("[data-rt-focus]").click();
          await until(() => panel()?.dataset.viz === "orb" && q(".rt-viz")?.dataset.vizKind, 5000);
          await sleep(200);
          await leave();
        }
        await sleep(500);
        info.realtime_contexts = { audio: ctx.audio.length, openAudio: ctx.openAudio(), gl: ctx.gl.length,
          liveGl: ctx.liveGl(), detail: ctx.detail() };
        check("five enter/leave cycles leave at most the page's one playback context and no live WebGL context",
          ctx.openAudio() <= 1 && ctx.liveGl() === 0 && allEnded(), info.realtime_contexts);
      }
    } finally {
      window.fetch = realFetch;
      restoreAsr();
      mock.opts.heldChat = undefined;
      mock.opts.lateUser = undefined;
      mock.opts.drainMs = undefined;
    }
  }

  window.lmgwMediaProbe = async function (opts = {}) {
    const checks = [];
    const info = {};
    window.__probeChecks = checks;
    const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail: detail === undefined ? null : detail });
    // Every track getUserMedia hands out, so the probe can see each one end,
    // and how often it was asked.
    const tracks = [];
    const calls = { n: 0 };
    const md = navigator.mediaDevices;
    if (!md) {
      check("navigator.mediaDevices exists (secure context)", false, location.origin);
      return JSON.stringify({ checks, info });
    }
    const orig = md.getUserMedia.bind(md);
    md.getUserMedia = async (c) => {
      calls.n++;
      const s = await orig(c);
      for (const t of s.getTracks()) {
        tracks.push(t);
        // A quiet room: the track delivers silence (dictation's slow combo).
        if (calls.silent) t.enabled = false;
      }
      // A microphone slow to open (the realtime phase's Esc while opening).
      if (calls.delayMs) await sleep(calls.delayMs);
      return s;
    };
    try {
      if (opts.phase === "reopen") {
        await reopen(check, info, tracks, calls);
        return JSON.stringify({ checks, info });
      }
      if (opts.phase === "dictation") {
        await dictation(check, info, tracks, calls);
        return JSON.stringify({ checks, info });
      }
      if (opts.phase === "realtime") {
        // `only: "integration"`: that round alone (a quicker loop while
        // working on it).
        await realtime(check, info, tracks, calls, opts.only);
        return JSON.stringify({ checks, info });
      }
      // The page first, so its device list meets a document with no capture
      // granted yet (WebKitGTK then hides names and ids).
      localStorage.removeItem("lmgw.voice.input");
      localStorage.removeItem("lmgw.voice.output");
      localStorage.removeItem("lmgw.voice.echo");
      await ui(check, info, tracks, opts);
      if (!opts.skipWorklets) await worklets(check, info);
      await storeForReopen(info);
    } catch (e) {
      check("the probe ran to the end", false, String(e) + " at " + String(e && e.stack));
    } finally {
      md.getUserMedia = orig;
      for (const t of tracks) t.stop();
    }
    return JSON.stringify({ checks, info });
  };

  // For a harness without autoplay allowed: see the header.
  window.lmgwGestureProbe = async function (step) {
    const noop = () => {};
    if (step === "open") {
      const { root } = await popover(noop);
      const b = root && q("[data-vd-tone]");
      if (!b) return JSON.stringify({ error: "no tone button" });
      b.scrollIntoView({ block: "center" });
      await sleep(200);
      const r = b.getBoundingClientRect();
      return JSON.stringify({ x: r.left + r.width / 2, y: r.top + r.height / 2 });
    }
    if (step === "script") q("[data-vd-tone]").click();
    const root = q("[data-voice-devices]");
    return JSON.stringify(root ? {
      tone: root.dataset.tone, pushed: +root.dataset.tonePushed, played: +root.dataset.tonePlayed,
      player: root.dataset.player, notes: notes(root),
      alert: (q("[data-voice-devices-btn]") || {}).dataset?.voiceAlert || "",
    } : { error: "the popover is closed" });
  };

  window.lmgwRevokeProbe = async function (step) {
    const root = q("[data-voice-devices]");
    if (step === "start") {
      const { root } = await popover(() => {});
      if (!root) return JSON.stringify({ error: "no popover" });
      q("[data-vd-mic=start]").click();
      await until(() => root.dataset.mic === "open" || root.dataset.mic === "error", 10000);
      return JSON.stringify({ mic: root.dataset.mic });
    }
    if (!root) return JSON.stringify({ error: "no popover" });
    await until(() => root.dataset.mic !== "open", 5000);
    return JSON.stringify({ mic: root.dataset.mic, notes: notes(root) });
  };
})();
