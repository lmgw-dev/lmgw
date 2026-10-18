// The three pages of the WebRTC interop rig (realtime calls design §13.2),
// evaluated by scripts/webrtc-interop.mjs in headless Chromium on a document
// of the dev instance's own origin, so the POST to /v1/realtime/calls is
// same-origin. It defines `window.lmgwInterop.{stock, agents, bound}(cfg)`;
// each answers `{checks: [{name, ok, detail}], info, events, recording}`.
//
// - stock: OpenAI's documented browser example pointed at lmgw — an
//   RTCPeerConnection, createDataChannel("oai-events"), the microphone track,
//   the offer POSTed as application/sdp with the key, the answer applied —
//   then a turn that plays out (output_audio_buffer.started, response.done,
//   .stopped), a barge-in (speech_started, the cancelled response.done,
//   .cleared) and the HTTP hangup.
// - agents: the Agents SDK's WebRTC transport (`OpenAIRealtimeWebRTC`,
//   `useInsecureApiKey`; the UMD bundle the runner injected), a turn that
//   plays out, then the SDK's own interrupt() (response.cancel, then
//   output_audio_buffer.clear) and its close.
// - bound: the bound contract as the Android client speaks it, against an
//   ongoing folder that a WebSocket session holds: chat_folder with
//   takeover=ask (lmgw.session.bind_pending naming the holder, then
//   lmgw.session.take_over, the holder closed with 4000), a small
//   max-message-size in the offer so lmgw's larger events arrive as
//   lmgw.event.part and are joined, one client event sent as parts,
//   lmgw.output_audio.playout from getStats every second while audio plays,
//   lmgw.response.stop with estimatedPlayoutTimestamp mid-answer, when there
//   is one (Q5: Chromium leaves it absent on an audio-only call; the remote
//   track muted at the tap), a push-to-talk turn (clear at the press, commit
//   and response.create at the release, the microphone's tail inside the
//   commit window), and the End (lmgw.response.stop, then closing
//   oai-events). The holder uses a key of its own: a session bound with the
//   call's own key does not hold the thread against it.
//
// The microphone is Chromium's fake device looping the rig's speech WAV: a
// turn starts with the track enabled and the track is disabled (silence
// frames, the RTP stream goes on) once lmgw commits it, so the answer plays
// out unless a page re-enables it to barge in. Every page records the
// remote track with MediaRecorder (for listening and the level check) and
// its level through an AudioWorklet (the stop's check).
(() => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const now = () => performance.now();

  // A check passes or fails; `known` tags a known finding (the calls
  // design's question that recorded it), which passes while the fact holds
  // and fails the day it changes.
  function checks() {
    const list = [];
    return {
      list,
      add(name, ok, detail, known) {
        const c = { name, ok: !!ok, detail: detail === undefined ? null : detail };
        if (known) c.known = known;
        list.push(c);
        return !!ok;
      },
    };
  }

  // Until pred() is truthy, polling every 20 ms, at most `ms`: its value, or null.
  async function until(pred, ms) {
    const end = now() + ms;
    for (;;) {
      const v = pred();
      if (v) return v;
      if (now() >= end) return null;
      await sleep(20);
    }
  }

  // The fields of an event worth keeping in the result.
  function brief(e, t) {
    const b = { t: Math.round(t), type: e.type };
    for (const k of ["response_id", "item_id", "held_by", "kind", "by", "turn_mode"]) {
      if (e[k] !== undefined) b[k] = e[k];
    }
    if (e.type === "error") b.code = e.error && e.error.code, b.message = e.error && e.error.message;
    if (e.type === "response.done") {
      b.status = e.response && e.response.status;
      b.reason = e.response && e.response.status_details && e.response.status_details.reason;
      b.response_id = e.response && e.response.id;
    }
    if (e.type === "response.created") b.response_id = e.response && e.response.id;
    return b;
  }

  // The events of one data channel (or SDK session), in order, with waits.
  function eventLog(t0) {
    const raw = [];
    const log = {
      raw,
      brief: [],
      push(e) {
        raw.push({ e, t: now() - t0 });
        log.brief.push(brief(e, now() - t0));
      },
      // The first event from index `from` on that `pred` takes: [index, event], or null.
      async next(pred, ms, from = 0) {
        const hit = await until(() => {
          for (let i = from; i < raw.length; i++) if (pred(raw[i].e)) return [i, raw[i].e];
          return null;
        }, ms);
        return hit;
      },
      count: (type) => raw.filter((r) => r.e.type === type).length,
      types: () => raw.map((r) => r.e.type),
    };
    return log;
  }
  const isType = (t) => (e) => e.type === t;

  // lmgw.event.part's joiner (calls design §6.3), as lmgw-client's: parts
  // back to back, in index order, one count; the joined text is one event.
  function joiner() {
    let set = null;
    const j = {
      parts: 0,
      joined: 0,
      largest: 0,
      errors: [],
      push(text) {
        const v = JSON.parse(text);
        if (v.type !== "lmgw.event.part") {
          if (set) j.errors.push(`set ${set.id} broken by ${v.type}`), (set = null);
          return [v];
        }
        j.parts++;
        if (v.index === 0) {
          set = { id: v.id, count: v.count, data: [v.data] };
        } else if (!set || set.id !== v.id || v.index !== set.data.length || v.count !== set.count) {
          j.errors.push(`part ${v.index} of ${v.id} out of order`);
          set = null;
          return [];
        } else {
          set.data.push(v.data);
        }
        if (set.data.length < set.count) return [];
        const whole = set.data.join("");
        set = null;
        j.joined++;
        j.largest = Math.max(j.largest, whole.length);
        return [JSON.parse(whole)];
      },
    };
    return j;
  }

  // A client event as `n` parts of the same shape (calls design §6.3).
  function asParts(event, id, n) {
    const text = JSON.stringify(event);
    const size = Math.ceil(text.length / n);
    const out = [];
    for (let i = 0; i < n; i++) {
      out.push(JSON.stringify({
        type: "lmgw.event.part", id, index: i, count: n, data: text.slice(i * size, (i + 1) * size),
      }));
    }
    return out;
  }

  // One getStats() reading of the audio both ways.
  async function audioStats(pc) {
    const out = { inbound: null, outbound: null, codecs: {}, pair: null, remote_inbound: null, remote_outbound: null };
    (await pc.getStats()).forEach((s) => {
      if (s.type === "inbound-rtp" && s.kind === "audio") out.inbound = s;
      else if (s.type === "remote-outbound-rtp" && s.kind === "audio") out.remote_outbound = s;
      else if (s.type === "outbound-rtp" && s.kind === "audio") out.outbound = s;
      else if (s.type === "codec") out.codecs[s.id] = s;
      else if (s.type === "candidate-pair" && s.nominated) out.pair = s;
      else if (s.type === "remote-inbound-rtp" && s.kind === "audio") out.remote_inbound = s;
    });
    const i = out.inbound || {};
    const o = out.outbound || {};
    const codec = (id) => out.codecs[id] || {};
    return {
      at: now(),
      inbound: out.inbound && {
        packetsReceived: i.packetsReceived,
        packetsLost: i.packetsLost,
        jitter: i.jitter,
        jitterBufferDelay: i.jitterBufferDelay,
        jitterBufferEmittedCount: i.jitterBufferEmittedCount,
        jitterBufferTargetDelay: i.jitterBufferTargetDelay,
        jitterBufferMinimumDelay: i.jitterBufferMinimumDelay,
        concealedSamples: i.concealedSamples,
        totalSamplesReceived: i.totalSamplesReceived,
        estimatedPlayoutTimestamp: i.estimatedPlayoutTimestamp,
        fecPacketsReceived: i.fecPacketsReceived,
        codec: codec(i.codecId).mimeType,
        fmtp: codec(i.codecId).sdpFmtpLine,
      },
      outbound: out.outbound && {
        packetsSent: o.packetsSent,
        bytesSent: o.bytesSent,
        codec: codec(o.codecId).mimeType,
        fmtp: codec(o.codecId).sdpFmtpLine,
        targetBitrate: o.targetBitrate,
      },
      rtt_ms: out.pair && out.pair.currentRoundTripTime !== undefined
        ? Math.round(out.pair.currentRoundTripTime * 1000) : null,
      remote_inbound: out.remote_inbound && {
        fractionLost: out.remote_inbound.fractionLost,
        jitter: out.remote_inbound.jitter,
        roundTripTime: out.remote_inbound.roundTripTime,
      },
      // lmgw's sender reports, as libwebrtc received them.
      remote_outbound: out.remote_outbound && {
        reportsSent: out.remote_outbound.reportsSent,
        remoteTimestamp: out.remote_outbound.remoteTimestamp,
      },
    };
  }

  // NetEQ's delay since `prev` (a reading), in ms: the change in
  // jitterBufferDelay over the change in jitterBufferEmittedCount.
  function jitterBufferMs(prev, cur) {
    if (!prev || !prev.inbound || !cur.inbound) return null;
    const de = cur.inbound.jitterBufferEmittedCount - prev.inbound.jitterBufferEmittedCount;
    const dd = cur.inbound.jitterBufferDelay - prev.inbound.jitterBufferDelay;
    return de > 0 ? (dd / de) * 1000 : null;
  }

  // The remote stream's level every 10 ms through an AudioWorklet, on the
  // page's clock, and a MediaRecorder capture of it.
  async function capture(stream) {
    const ctx = new AudioContext();
    const code = "class Tap extends AudioWorkletProcessor{constructor(){super();this.a=0;this.n=0}"
      + "process(i){const c=i[0]&&i[0][0];if(c){for(let k=0;k<c.length;k++)this.a+=c[k]*c[k];"
      + "this.n+=c.length;if(this.n>=sampleRate/100){this.port.postMessage({t:currentTime,"
      + "rms:Math.sqrt(this.a/this.n)});this.a=0;this.n=0}}return true}}registerProcessor('tap',Tap)";
    await ctx.audioWorklet.addModule(URL.createObjectURL(new Blob([code], { type: "text/javascript" })));
    await ctx.resume();
    const src = ctx.createMediaStreamSource(stream);
    const tap = new AudioWorkletNode(ctx, "tap");
    const mute = ctx.createGain();
    mute.gain.value = 0;
    src.connect(tap).connect(mute).connect(ctx.destination);
    // The context's clock against the page's.
    const ctx0 = ctx.currentTime;
    const perf0 = now();
    const env = [];
    tap.port.onmessage = (m) => env.push([perf0 + (m.data.t - ctx0) * 1000, m.data.rms]);
    const chunks = [];
    let rec = null;
    try {
      rec = new MediaRecorder(stream, { mimeType: "audio/webm;codecs=opus" });
      rec.ondataavailable = (e) => e.data.size && chunks.push(e.data);
      rec.start(500);
    } catch (e) {
      rec = null;
    }
    return {
      env,
      // The first 10 ms block from `from` (page time) louder than `rms`.
      onset(from, rms = 0.01) {
        const hit = env.find(([t, r]) => t >= from && r > rms);
        return hit ? hit[0] : null;
      },
      peak(from, to) {
        return env.filter(([t]) => t >= from && t <= to).reduce((m, [, r]) => Math.max(m, r), 0);
      },
      async stop() {
        let b64 = null;
        if (rec && rec.state !== "inactive") {
          await new Promise((res) => {
            rec.onstop = res;
            rec.stop();
          });
          const blob = new Blob(chunks, { type: "audio/webm" });
          const buf = new Uint8Array(await blob.arrayBuffer());
          let s = "";
          for (let i = 0; i < buf.length; i += 0x8000) s += String.fromCharCode.apply(null, buf.subarray(i, i + 0x8000));
          b64 = btoa(s);
        }
        await ctx.close();
        return b64;
      },
    };
  }

  // What every answer of lmgw's SDP must be (calls design §2.4).
  function answerShape(c, sdp) {
    const lines = sdp.split(/\r?\n/);
    const cands = lines.filter((l) => l.startsWith("a=candidate"));
    const audio = lines.find((l) => l.startsWith("m=audio")) || "";
    const pts = audio.split(" ").slice(3);
    const names = pts.map((pt) => {
      const r = lines.find((l) => l.startsWith(`a=rtpmap:${pt} `));
      return r ? r.split(" ")[1].split("/")[0].toLowerCase() : "?";
    });
    c.add("the answer is ICE-lite with one host candidate", lines.includes("a=ice-lite") && cands.length === 1, cands);
    c.add("the answer's audio is Opus only (RED beside it when offered)",
      names.length > 0 && names.every((n) => n === "opus" || n === "red"), names);
    c.add("the answer carries the data channel", lines.some((l) => l.startsWith("m=application")));
    return { candidates: cands, codecs: names, max_message: (lines.find((l) => l.startsWith("a=max-message-size")) || "") };
  }

  // A peer connection with the microphone and oai-events, the way OpenAI's
  // browser example builds it; `onEvent` hears each event's text.
  async function connect(cfg, url, opts = {}) {
    const t0 = now();
    const marks = {};
    const pc = new RTCPeerConnection();
    const audio = new Audio();
    audio.autoplay = true;
    let remote = null;
    pc.ontrack = (e) => {
      remote = e.streams[0];
      audio.srcObject = remote;
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "connected" && !marks.connected) marks.connected = now() - t0;
    };
    const mic = await navigator.mediaDevices.getUserMedia({ audio: true });
    const track = mic.getAudioTracks()[0];
    pc.addTrack(track, mic);
    const dc = pc.createDataChannel("oai-events");
    dc.onopen = async () => {
      marks.open = now() - t0;
      const s = await audioStats(pc);
      marks.rtp_before_open = s.outbound ? s.outbound.packetsSent : 0;
    };
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    let sdp = offer.sdp;
    if (opts.maxMessage) sdp = sdp.replace(/a=max-message-size:\d+/, `a=max-message-size:${opts.maxMessage}`);
    const r = await fetch(url, {
      method: "POST",
      body: sdp,
      headers: { Authorization: `Bearer ${cfg.key}`, "Content-Type": "application/sdp" },
    });
    const answer = await r.text();
    marks.answered = now() - t0;
    const res = { pc, dc, audio, track, t0, marks, status: r.status, location: r.headers.get("location"), answer, offer: offer.sdp };
    res.remote = () => remote;
    if (r.status === 201) await pc.setRemoteDescription({ type: "answer", sdp: answer });
    return res;
  }

  // Say `turn` once: the microphone on until lmgw commits a turn, then off.
  async function speakOnce(log, track, from, ms = 30000) {
    track.enabled = true;
    const hit = await log.next(isType("input_audio_buffer.committed"), ms, from);
    track.enabled = false;
    return hit;
  }

  // The response the next `response.created` from `from` names, and its end.
  async function response(log, from, ms = 40000) {
    const created = await log.next(isType("response.created"), ms, from);
    if (!created) return null;
    const id = created[1].response.id;
    const started = await log.next((e) => e.type === "output_audio_buffer.started" && e.response_id === id, ms, created[0]);
    return { id, created, started };
  }

  async function finish(id, log, ms = 40000, from = 0) {
    const done = await log.next((e) => e.type === "response.done" && e.response.id === id, ms, from);
    const after = done && await log.next(
      (e) => (e.type === "output_audio_buffer.stopped" || e.type === "output_audio_buffer.cleared") && e.response_id === id,
      5000, done[0]);
    return { done, after };
  }

  // --- 1. OpenAI's documented browser example --------------------------------------------
  async function stock(cfg) {
    const c = checks();
    const info = {};
    const call = await connect(cfg, `${cfg.base}/v1/realtime/calls`);
    const log = eventLog(call.t0);
    call.dc.onmessage = (m) => log.push(JSON.parse(m.data));
    c.add("POST /v1/realtime/calls answers 201 with a Location", call.status === 201
      && /^\/v1\/realtime\/calls\/rtc_[0-9a-f]+$/.test(call.location || ""), [call.status, call.location]);
    if (call.status !== 201) return { checks: c.list, info: { answer: call.answer } };
    info.call_id = call.location.split("/").pop();
    info.answer = answerShape(c, call.answer);
    const first = await log.next(() => true, 15000);
    c.add("ICE and DTLS connect through pasta's forwarding", !!call.marks.connected, call.marks);
    c.add("the first event is session.created", first && first[1].type === "session.created", first && first[1].type);
    const cap = await capture(call.remote());
    // A turn that plays out.
    const said = await speakOnce(log, call.track, 0);
    c.add("the fake microphone's speech is a committed turn", !!said);
    const r1 = said && await response(log, said[0]);
    c.add("the answer starts: output_audio_buffer.started", r1 && r1.started, r1 && r1.id);
    if (r1 && r1.started) {
      const f = await finish(r1.id, log);
      c.add("response.done, then output_audio_buffer.stopped", f.done && f.after && f.after[1].type === "output_audio_buffer.stopped",
        f.after && f.after[1].type);
      const peak = cap.peak(r1.started[0] && log.raw[r1.started[0]].t + call.t0, now());
      info.peak_rms = peak;
      c.add("the answer is heard: the remote track's level rises", peak > 0.01, peak.toFixed(4));
    }
    // A barge-in: the microphone comes back while the next answer plays.
    const from = log.raw.length;
    const said2 = await speakOnce(log, call.track, from);
    const r2 = said2 && await response(log, said2[0]);
    if (r2 && r2.started) {
      await sleep(800);
      const at = log.raw.length;
      call.track.enabled = true;
      const f = await finish(r2.id, log, 30000, at);
      call.track.enabled = false;
      const reason = f.done && f.done[1].response.status_details && f.done[1].response.status_details.reason;
      c.add("a barge-in: the cancelled response.done (turn_detected), then output_audio_buffer.cleared",
        f.done && f.done[1].response.status === "cancelled" && reason === "turn_detected"
          && f.after && f.after[1].type === "output_audio_buffer.cleared",
        [f.done && f.done[1].response.status, reason, f.after && f.after[1].type]);
      c.add("input_audio_buffer.speech_started came with the cut",
        log.raw.slice(at).some((r) => r.e.type === "input_audio_buffer.speech_started"));
    } else {
      c.add("a second answer to barge into", false);
    }
    info.stats = await audioStats(call.pc);
    // OpenAI's route ends it.
    const h = await fetch(`${cfg.base}/v1/realtime/calls/${info.call_id}/hangup`, {
      method: "POST",
      headers: { Authorization: `Bearer ${cfg.key}` },
    });
    c.add("POST …/hangup answers 200", h.status === 200, h.status);
    const closed = await until(() => call.dc.readyState === "closed" || call.pc.connectionState === "closed"
      || call.pc.connectionState === "failed" || call.pc.connectionState === "disconnected", 10000);
    c.add("the hangup closes the peer connection", !!closed, [call.dc.readyState, call.pc.connectionState]);
    info.marks = call.marks;
    const recording = await cap.stop();
    call.pc.close();
    return { checks: c.list, info, events: log.brief, recording };
  }

  // --- 2. The Agents SDK's WebRTC transport ----------------------------------------------
  async function agents(cfg) {
    const c = checks();
    const info = {};
    const sdk = window.OpenAIAgentsRealtime;
    if (!c.add("the Agents SDK's UMD bundle is loaded", !!sdk)) return { checks: c.list, info };
    const t0 = now();
    const log = eventLog(t0);
    const audio = new Audio();
    audio.autoplay = true;
    const transport = new sdk.OpenAIRealtimeWebRTC({
      baseUrl: `${cfg.base}/v1/realtime/calls`, useInsecureApiKey: true, audioElement: audio,
    });
    const agent = new sdk.RealtimeAgent({ name: "interop", instructions: "Answer in one sentence." });
    const session = new sdk.RealtimeSession(agent, { transport, model: "gpt-realtime" });
    const errors = [];
    session.on("transport_event", (e) => e && e.type && log.push(e));
    session.on("error", (e) => errors.push(String((e && e.error && (e.error.message || e.error.type)) || e)));
    try {
      await session.connect({ apiKey: cfg.key });
      c.add("RealtimeSession.connect resolves (session.updated acknowledged)", true);
    } catch (e) {
      c.add("RealtimeSession.connect resolves (session.updated acknowledged)", false, String(e));
      return { checks: c.list, info: { errors }, events: log.brief };
    }
    info.call_id = transport.callId;
    c.add("the transport read the call's id from Location", /^rtc_[0-9a-f]+$/.test(info.call_id || ""), info.call_id);
    const pc = transport.connectionState.peerConnection;
    const stream = await until(() => audio.srcObject, 5000);
    const cap = stream ? await capture(stream) : null;
    // A turn that plays out.
    session.mute(false);
    const said = await log.next(isType("input_audio_buffer.committed"), 30000);
    session.mute(true);
    c.add("the SDK's microphone gives a committed turn", !!said);
    const r1 = said && await response(log, said[0]);
    if (r1 && r1.started) {
      const f = await finish(r1.id, log);
      c.add("an answer plays out: response.done, then output_audio_buffer.stopped",
        f.after && f.after[1].type === "output_audio_buffer.stopped", f.after && f.after[1].type);
    } else {
      c.add("an answer plays out: response.done, then output_audio_buffer.stopped", false, r1 && r1.id);
    }
    // The SDK's interrupt(): response.cancel, then output_audio_buffer.clear.
    session.mute(false);
    const said2 = await log.next(isType("input_audio_buffer.committed"), 30000, log.raw.length);
    session.mute(true);
    const r2 = said2 && await response(log, said2[0]);
    if (r2 && r2.started) {
      await sleep(800);
      const at = log.raw.length;
      session.interrupt();
      const f = await finish(r2.id, log, 15000, at);
      c.add("interrupt(): the cancelled response.done (client_cancelled), then output_audio_buffer.cleared",
        f.done && f.done[1].response.status === "cancelled" && f.after && f.after[1].type === "output_audio_buffer.cleared",
        [f.done && f.done[1].response.status, f.after && f.after[1].type]);
      await sleep(500);
      const unexpected = log.raw.slice(at).filter((r) => r.e.type === "error"
        && !(r.e.error && r.e.error.code === "response_cancel_not_active")).map((r) => r.e.error);
      c.add("no error but OpenAI's response_cancel_not_active", unexpected.length === 0, unexpected);
    } else {
      c.add("a second answer to interrupt", false);
    }
    info.stats = pc ? await audioStats(pc) : null;
    info.errors = errors;
    const recording = cap ? await cap.stop() : null;
    session.close();
    const closed = await until(() => !pc || pc.connectionState === "closed", 5000);
    c.add("session.close() closes the peer connection", !!closed, pc && pc.connectionState);
    return { checks: c.list, info, events: log.brief, recording };
  }

  // --- 3. The bound contract, as the Android client speaks it ----------------------------
  async function bound(cfg) {
    const c = checks();
    const info = { reports: [] };
    // The holder: a WebSocket session bound to the folder's current thread.
    // Another key: a holder bound with the call's own key does not hold the
    // thread against it (the same client binding again).
    const holder = new WebSocket(`${cfg.base.replace(/^http/, "ws")}/v1/realtime?chat_folder=${cfg.folder}`,
      ["realtime", `openai-insecure-api-key.${cfg.holderKey}`]);
    const held = { closed: null, created: null };
    holder.onmessage = (m) => {
      const e = JSON.parse(m.data);
      if (e.type === "session.created") held.created = e;
    };
    holder.onclose = (e) => (held.closed = { code: e.code, reason: e.reason });
    await until(() => held.created || held.closed, 15000);
    const thread = held.created && held.created.session.lmgw && held.created.session.lmgw.resolved
      && held.created.session.lmgw.resolved.chat_thread && held.created.session.lmgw.resolved.chat_thread.id;
    c.add("a WebSocket session holds the folder's current thread", !!thread, held.closed || thread);
    // The call: takeover=ask, and a small max-message-size so lmgw splits.
    const call = await connect(cfg, `${cfg.base}/v1/realtime/calls?chat_folder=${cfg.folder}&takeover=ask`,
      { maxMessage: cfg.maxMessage });
    const join = joiner();
    const log = eventLog(call.t0);
    call.dc.onmessage = (m) => {
      for (const e of join.push(m.data)) log.push(e);
    };
    c.add("POST /v1/realtime/calls?chat_folder=…&takeover=ask answers 201", call.status === 201, [call.status, call.answer.slice(0, 200)]);
    if (call.status !== 201) return { checks: c.list, info };
    info.call_id = call.location.split("/").pop();
    const pending = await log.next(() => true, 15000);
    c.add("lmgw.session.bind_pending first, naming the holder", pending && pending[1].type === "lmgw.session.bind_pending"
      && !!pending[1].held_by, pending && pending[1]);
    call.dc.send(JSON.stringify({ type: "lmgw.session.take_over" }));
    const created = await log.next(isType("session.created"), 15000);
    const bound = created && created[1].session.lmgw && created[1].session.lmgw.resolved
      && created[1].session.lmgw.resolved.chat_thread && created[1].session.lmgw.resolved.chat_thread.id;
    c.add("take_over: session.created bound to the holder's thread", !!created && bound === thread, [bound, thread]);
    info.turn_mode = created && created[1].session.lmgw && created[1].session.lmgw.turn_mode;
    c.add("session.lmgw.turn_mode is said", info.turn_mode === "automatic" || info.turn_mode === "push_to_talk", info.turn_mode);
    await until(() => held.closed, 5000);
    c.add("the holder's WebSocket closes with 4000 (taken over)", held.closed && held.closed.code === 4000, held.closed);
    info.marks = call.marks;
    const cap = await capture(call.remote());
    // Playout reports every second while a response's audio plays.
    let playing = null;
    let prev = null;
    let firstPacket = null;
    let firstPosition = null;
    const reporter = setInterval(async () => {
      const s = await audioStats(call.pc);
      if (s.inbound && s.inbound.packetsReceived > 0 && firstPacket === null) firstPacket = now();
      if (s.inbound && s.inbound.estimatedPlayoutTimestamp && firstPosition === null) firstPosition = now();
      const jb = jitterBufferMs(prev, s);
      // NetEQ's target delay over the same stretch, as jitterBufferDelay's.
      const de = prev && prev.inbound && s.inbound
        ? s.inbound.jitterBufferEmittedCount - prev.inbound.jitterBufferEmittedCount : 0;
      const target = de > 0 ? Math.round(((s.inbound.jitterBufferTargetDelay - prev.inbound.jitterBufferTargetDelay) / de) * 1000)
        : null;
      prev = s;
      if (!playing || call.dc.readyState !== "open") return;
      const ev = { type: "lmgw.output_audio.playout" };
      if (s.inbound && s.inbound.estimatedPlayoutTimestamp) ev.playout_ntp_ms = s.inbound.estimatedPlayoutTimestamp;
      if (jb !== null) ev.jitter_buffer_ms = jb;
      if (ev.playout_ntp_ms === undefined && ev.jitter_buffer_ms === undefined) return;
      call.dc.send(JSON.stringify(ev));
      // What else names the sample playing: the receiver's synchronization
      // source, its RTP time and when it was played (§16's question).
      const src = (call.pc.getReceivers()[0].getSynchronizationSources() || [])[0];
      info.reports.push({
        t: Math.round(now() - call.t0), response_id: playing, playout_ntp_ms: ev.playout_ntp_ms ?? null,
        jitter_buffer_ms: ev.jitter_buffer_ms ?? null, target_ms: target, rtt_ms: s.rtt_ms,
        source: src ? { rtp: src.rtpTimestamp, age_ms: Math.round(Date.now() - src.timestamp) } : null,
        sender_reports: s.remote_outbound && s.remote_outbound.reportsSent,
      });
    }, 1000);
    const watch = (r) => {
      playing = r.id;
    };
    // 1. An automatic turn whose answer plays out, reported.
    const said = await speakOnce(log, call.track, created ? created[0] : 0);
    c.add("an automatic turn is committed", !!said);
    const r1 = said && await response(log, said[0]);
    if (r1 && r1.started) {
      watch(r1);
      const f = await finish(r1.id, log, 60000);
      playing = null;
      c.add("its answer plays out: response.done, then output_audio_buffer.stopped",
        f.after && f.after[1].type === "output_audio_buffer.stopped", f.after && f.after[1].type);
      info.first_response = r1.id;
    } else {
      c.add("its answer plays out: response.done, then output_audio_buffer.stopped", false);
    }
    c.add("playout reports were sent while it played", info.reports.length >= 2, info.reports.length);
    info.first_packet_to_position_ms = firstPacket !== null && firstPosition !== null
      ? Math.round(firstPosition - firstPacket) : null;
    // Q5 (calls design §15): libwebrtc does not fill estimatedPlayoutTimestamp
    // on an audio-only call. A known finding, checked as one: it passes while
    // the field stays absent and fails the day Chromium fills it.
    c.add("estimatedPlayoutTimestamp stays absent on an audio-only call, as found",
      firstPosition === null,
      firstPosition === null
        ? "absent, as found"
        : `filled ${info.first_packet_to_position_ms} ms after the first packet: Chromium fills it now, `
          + "so Q5's premise changed and the reports' position and a stop's heard at it can be checked again",
      "Q5");
    // 2. lmgw.response.stop at the tap, with the playout position.
    const said2 = await speakOnce(log, call.track, log.raw.length);
    const r2 = said2 && await response(log, said2[0]);
    if (r2 && r2.started) {
      watch(r2);
      const startedAt = log.raw[r2.started[0]].t + call.t0;
      await sleep(cfg.stopAfterMs || 1800);
      const s = await audioStats(call.pc);
      const tap = now();
      call.audio.muted = true;
      const position = s.inbound && s.inbound.estimatedPlayoutTimestamp;
      const at = log.raw.length;
      call.dc.send(JSON.stringify({ type: "lmgw.response.stop", playout_ntp_ms: position }));
      playing = null;
      const f = await finish(r2.id, log, 15000, at);
      const reason = f.done && f.done[1].response.status_details && f.done[1].response.status_details.reason;
      c.add("lmgw.response.stop: the cancelled response.done (client_cancelled), then output_audio_buffer.cleared",
        f.done && f.done[1].response.status === "cancelled" && reason === "client_cancelled"
          && f.after && f.after[1].type === "output_audio_buffer.cleared",
        [f.done && f.done[1].response.status, reason, f.after && f.after[1].type]);
      info.stop = {
        response_id: r2.id,
        playout_ntp_ms: position,
        // When its audio started playing at the tap's track, and the tap.
        onset_ms: (() => { const o = cap.onset(startedAt); return o === null ? null : Math.round(o - call.t0); })(),
        started_ms: Math.round(startedAt - call.t0),
        tap_ms: Math.round(tap - call.t0),
      };
      await sleep(300);
      call.audio.muted = false;
    } else {
      c.add("a second answer to stop", false);
    }
    // 3. One client event as parts: a session.update in three.
    const before = log.raw.length;
    for (const p of asParts({ type: "session.update", session: { type: "realtime", lmgw: { half_duplex: false } } }, "part_1", 3)) {
      call.dc.send(p);
    }
    const upd = await log.next((e) => e.type === "session.updated" || e.type === "error", 10000, before);
    c.add("a client event sent as three lmgw.event.part messages is applied", upd && upd[1].type === "session.updated",
      upd && (upd[1].error || upd[1].type));
    // 4. A push-to-talk turn: clear at the press, commit at the release, the
    //    microphone's tail on for 100 ms after it.
    const sw = log.raw.length;
    call.dc.send(JSON.stringify({ type: "session.update", session: { type: "realtime", lmgw: { turn_mode: "push_to_talk" } } }));
    const ptt = await log.next((e) => e.type === "session.updated" || e.type === "error", 10000, sw);
    const mode = ptt && ptt[1].session && ptt[1].session.lmgw && ptt[1].session.lmgw.turn_mode;
    c.add("session.update switches turn_mode to push_to_talk", mode === "push_to_talk", ptt && (ptt[1].error || mode));
    if (mode === "push_to_talk") {
      const press = log.raw.length;
      call.dc.send(JSON.stringify({ type: "input_audio_buffer.clear" }));
      call.track.enabled = true;
      await sleep(cfg.pttMs || 2500);
      // OpenAI's manual turn: commit at the release, then ask for the answer.
      call.dc.send(JSON.stringify({ type: "input_audio_buffer.commit" }));
      call.dc.send(JSON.stringify({ type: "response.create" }));
      info.ptt = { release_ms: Math.round(now() - call.t0) };
      await sleep(100);
      call.track.enabled = false;
      const committed = await log.next(isType("input_audio_buffer.committed"), 10000, press);
      c.add("the push-to-talk commit is committed", !!committed);
      info.ptt.committed_ms = committed ? Math.round(log.raw[committed[0]].t) : null;
      const r3 = committed && await response(log, committed[0]);
      if (r3 && r3.started) {
        watch(r3);
        const f = await finish(r3.id, log, 60000);
        playing = null;
        c.add("its answer plays out", f.after && f.after[1].type === "output_audio_buffer.stopped", f.after && f.after[1].type);
      } else {
        c.add("its answer plays out", false);
      }
    }
    clearInterval(reporter);
    info.parts = { received: join.parts, joined: join.joined, largest: join.largest, errors: join.errors };
    c.add("lmgw's events over the offered max-message-size arrive as parts and join", join.joined > 0 && join.errors.length === 0,
      info.parts);
    info.stats = await audioStats(call.pc);
    // The End: lmgw.response.stop, then closing oai-events.
    call.dc.send(JSON.stringify({ type: "lmgw.response.stop" }));
    call.dc.close();
    const ended = await until(() => call.pc.connectionState === "closed" || call.pc.connectionState === "failed"
      || call.pc.connectionState === "disconnected", 10000);
    info.end_state = call.pc.connectionState;
    c.add("the End: closing oai-events ends the call (lmgw closes the peer connection)", !!ended, info.end_state);
    const recording = await cap.stop();
    call.pc.close();
    try { holder.close(); } catch (_) { /* closed */ }
    return { checks: c.list, info, events: log.brief, recording };
  }

  window.lmgwInterop = { stock, agents, bound };
})();
