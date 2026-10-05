// An in-page stand-in for a bound `/v1/realtime?chat_thread=<id>` session
// (chat-voice §8, WP9), for the probes of the realtime panel: the WebKit
// probe (scripts/media-probe.js, phase "realtime", also run in the app by
// scripts/shell-check.py) and the Chrome panel drive (scripts/drive/
// chat-voice-panel.json, loaded with a {"script": …} step).
//
// window.lmgwRealtimeMock.install(opts) replaces window.WebSocket for URLs
// with /v1/realtime (every other socket is the browser's own) by a socket
// that answers as a bound session does — session.created with the thread,
// the connect warm's lmgw.model.state frames — and that is driven from the
// probe, not by a voice detector:
//
//   mock.speak(text, ms, o) the server heard speech for `ms`: speech_started,
//                          then speech_stopped, committed, the transcript, and
//                          a response (respond(o)).
//   mock.respond(o)        a response as a bound turn makes one: response.
//                          created, lmgw.chat.user, the chat frames of the reply
//                          (text faster than speech), audio deltas paced at
//                          real time (a synthetic voice: harmonics under a
//                          syllable envelope, PCM16 24 kHz) with the spoken
//                          transcript paced beside them, output_audio.done,
//                          response.done, lmgw.chat.reply, lmgw.response.timing.
//                          o.doneAfterPlayout: response.done only after the
//                          lead has played out, as a bound response's pacing
//                          drains (realtime §8.2); o.tool: a tool turn — a
//                          spoken preamble, the tool's start, o.toolMs (or
//                          opts.toolMs) of silence with the item still open,
//                          its result, then the answer, all one audio item.
//                          o.reasoning (or opts.reasoning): the model reasons
//                          first, as the server relays it (chat-voice §8.5):
//                          `reasoning` frames before the reply's deltas,
//                          never in the audio or the spoken transcript, kept
//                          with the stored reply, timed as reasoning_ms; with
//                          o.reasoningNote (or opts.reasoningNote) the done
//                          frame's reasoning_note and reasoning_ignored
//                          ["enabled"], as for a model that reasoned although
//                          the voice turn asked for reasoning off.
//   mock.startSpeech()     speech_started, and the turn stays open.
//   mock.bargeIn()         speech_started while the reply plays: the response
//                          is cancelled (turn_detected), as a server barge-in.
//   mock.emitError(code, message)  an `error` event.
//   mock.takeOver()        error chat_thread_taken_over, then close 4000.
//   mock.respond({refuse: {code, message, held}})  a bound turn the chat
//                          model refuses, as the WP11 server batch sends it
//                          (web/chat.rs, chat_turn/out.rs, realtime/thread/
//                          turn.rs, lifecycle.rs Failure::of_call): response.
//                          created, lmgw.chat.user (the user turn is written),
//                          for the hold and a benchmark run lmgw.model.state
//                          {stage chat, state held, cause} (`held`: gpu_hold
//                          or benchmark), for a VRAM wait loading then failed,
//                          the chat frames' error {message, code} and done
//                          {aborted}, then `error` with the code, its type and
//                          the message, and response.done failed with it.
//   mock.speak(text, ms, {transcription: {code, message}})  the turn's
//                          transcription fails instead (….transcription.failed
//                          with its error's code, as a bound turn whose thread
//                          names no ASR says asr_not_configured); no response.
//   mock.commitOnly()      speech_stopped and committed, and the transcript
//                          never comes (the gateway's end dropped it after
//                          ping_interval_s).
//
// The journal: what the session stored, per thread, in mock.rows[tid] — the
// user turns (lmgw.chat.user) and the replies as cut (lmgw.chat.reply; a
// removed one goes) — is what patchThread answers for the thread's
// messages, so a read-back after the session shows the stored rows.
// opts.lateUser: a user turn the journal writes during the drain of the
// page's close (its transcript came late), which the page never heard of.
//
// What the server does around a cut, from the WP8 code (realtime/lifecycle,
// realtime/thread/journal), not the spec:
// - a barge-in cancels at once and the journal cuts the reply at what was
//   *sent* (cancel.rs → bound_ended); the page's truncate, a flush later, is
//   then a re-cut with a second lmgw.chat.reply (finalize.rs recut). A
//   truncate that comes before the turn's save (opts.saveMs, default 0: the
//   save is quick) gives one reply, cut at the truncate;
// - a truncate of an item still being produced cancels the rest of its
//   response (truncate.rs → client_cancelled), and the reply is cut there;
//   one past the audio produced is refused (invalid_value), as is a
//   response.cancel with nothing active (response_cancel_not_active);
// - the cut: whole clauses, then the clause the cut falls in by character
//   share, back to its last whole word (heard/words.rs); unheard is the rest;
//   a reply heard whole is only annotated (unheard null), one heard not at
//   all is removed;
// - session.update is answered with session.updated, and an open turn ends
//   with speech_stopped (no commit) when turn detection goes off;
//   input_audio_buffer.clear with input_audio_buffer.cleared;
// - the page's close is answered only once the session's journal drained
//   (what a reply was stored as is in mock.stored), so the browser's close
//   event comes then (IT the_server_s_close_comes_once_the_journal_drained).
//
// opts: thread, title, adminTools, ptt (the session's own turn detection),
// warmMs, thinkMs, audioMs, reply, question, models {asr, chat, tts, voice},
// saveMs, toolMs, createdMs (session.created that long after the open),
// refuse (the handshake is refused: a close 1006 before it opened), drainMs
// (how long the page's close waits after the drain), heldChat (the connect
// warm's chat stage is held: gpu_hold or benchmark) and lateUser.
//
// What the page sends is in mock.got (appends counted, not kept: mock.appends,
// mock.appendBytes); a truncate in mock.truncates [{item_id, audio_end_ms,
// sent_ms}] (one refused in mock.truncateErrors), cancels in mock.cancels
// (refused ones in mock.cancelRefused). A push-to-talk commit +
// response.create is answered with respond(). Nothing here plays: the page's
// player plays what it is sent (the probes mute it). The models it names are
// mock/asr, mock/chat, mock/tts and the voice F2; opts.models names others
// (scripts/readme-shots.sh shows the dev copy's own aliases), as
// patchThread's mode.models does.
(() => {
  const RATE = 24000;
  const LEAD_MS = 300;
  const Real = window.WebSocket;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function b64(bytes) {
    let s = "";
    for (let i = 0; i < bytes.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    return btoa(s);
  }
  function b64len(s) {
    const pad = s.endsWith("==") ? 2 : s.endsWith("=") ? 1 : 0;
    return (s.length * 3) / 4 - pad;
  }

  // A speech-like voice: a 150 Hz source's harmonics weighted by two
  // formants, under a ~4 Hz syllable envelope; `from` samples in.
  function voice(from, n) {
    const out = new Int16Array(n);
    for (let i = 0; i < n; i++) {
      const t = (from + i) / RATE;
      const syl = Math.pow(Math.abs(Math.sin(Math.PI * 4.2 * t)), 0.9);
      let v = 0;
      for (let h = 1; h <= 24; h++) {
        const f = 150 * h;
        const w = Math.exp(-Math.pow((f - 650) / 220, 2)) + 0.7 * Math.exp(-Math.pow((f - 1800) / 300, 2)) + 0.08;
        v += (w / h) * Math.sin(2 * Math.PI * f * t + h);
      }
      out[i] = Math.max(-32767, Math.min(32767, v * syl * 9000));
    }
    return new Uint8Array(out.buffer);
  }

  // The reply's clauses as [start, end) character spans: a clause ends at
  // . ! ? , ; or : before a space (the server's splitter, roughly).
  function clauses(text) {
    const out = [];
    const re = /[.!?,;:](?=\s|$)/g;
    let start = 0;
    let m;
    while ((m = re.exec(text))) {
      out.push([start, m.index + 1]);
      start = m.index + 1;
      while (text[start] === " ") start++;
    }
    if (start < text.length) out.push([start, text.length]);
    return out;
  }

  // How many of `clause`'s characters are its words heard whole, when its
  // first `keep` characters were heard (heard/words.rs): back to the end of
  // the last whole word, its punctuation with it.
  function wordsHeard(clause, keep) {
    let end = 0;
    const re = /\S+/g;
    let m;
    while ((m = re.exec(clause))) {
      const word = m[0].replace(/[^\p{L}\p{N}]+$/u, "");
      if (!/[\p{L}\p{N}]/u.test(word)) continue;
      if (m.index + word.length > keep) break;
      end = m.index + m[0].length;
    }
    return end;
  }

  // The characters of `text` heard at `ms` of its `totalMs` of audio: whole
  // clauses, then the words of the one the cut falls in (module doc).
  function heardChars(text, totalMs, ms) {
    const spans = clauses(text);
    const chars = spans.reduce((n, [a, b]) => n + (b - a), 0) || 1;
    let t = 0;
    let end = 0;
    for (const [a, b] of spans) {
      const dur = (totalMs * (b - a)) / chars;
      if (t + dur <= ms + 1e-6) {
        end = b;
        t += dur;
        continue;
      }
      if (ms > t) {
        const n = wordsHeard(text.slice(a, b), Math.floor(((b - a) * (ms - t)) / dur));
        if (n > 0) end = a + n;
      }
      break;
    }
    return end;
  }

  const MOCK_MODELS = { asr: "mock/asr", chat: "mock/chat", tts: "mock/tts", voice: "F2" };
  const names = (m) => Object.assign({}, MOCK_MODELS, m || {});

  const mock = {
    sockets: [],
    got: [],
    appends: 0,
    appendBytes: 0,
    truncates: [],
    truncateErrors: [],
    cancels: 0,
    cancelRefused: 0,
    commits: 0,
    clears: 0,
    updates: 0,
    responses: 0,
    sentMs: 0,
    playingResponse: null,
    byItem: {},
    stored: [],
    replies: [],
    userTurn: false,
    opts: {},
    rows: {},
    // Set as the probe goes; declared here so the editor's checker knows
    // them before their first assignment (WP11 UI review NIT 1).
    current: /** @type {any} */ (null),
    created: 0,
    closedByPage: /** @type {any} */ (null),
    closedAt: /** @type {any} */ (null),
    readsAtClose: 0,
    threadReads: 0,
    lastThreadRead: 0,
    stopAudio: false,
    unpatch: /** @type {any} */ (null),
    // Assigned below (they use helpers defined further down).
    cancel: /** @type {any} */ (null),
    truncate: /** @type {any} */ (null),
    respond: /** @type {any} */ (null),
    writeCut: /** @type {any} */ (null),
  };

  // The journal's rows of thread `tid` (module doc).
  const rowsOf = (tid) => (mock.rows[tid] = mock.rows[tid] || []);

  class FakeSocket extends EventTarget {
    constructor(url) {
      super();
      this.url = url;
      this.tid = Number(new URL(String(url), location.href).searchParams.get("chat_thread"));
      this.readyState = 0;
      this.binaryType = "blob";
      this.protocol = "realtime";
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;
      this.n = 0;
      this.detection = mock.opts.ptt ? null : { type: "server_vad", prefix_padding_ms: 300 };
      this.halfDuplex = false;
      // What waits for session.created: the server sends it before it reads
      // any client event.
      this.created = false;
      this.afterCreated = [];
      mock.sockets.push(this);
      mock.current = this;
      if (mock.opts.refuse) {
        // A refused handshake: the WebSocket API hides the status and
        // closes with 1006 before it ever opened.
        setTimeout(() => { this.refusedAt = performance.now(); this.serverClose(1006, ""); }, 30);
      } else {
        setTimeout(() => this.open(), 30);
      }
    }
    session() {
      const o = mock.opts;
      const m = names(o.models);
      return {
        type: "realtime",
        object: "realtime.session",
        audio: { input: { turn_detection: this.detection } },
        lmgw: {
          half_duplex: this.halfDuplex,
          resolved: {
            chat: m.chat, asr: m.asr, tts: m.tts, voice: m.voice,
            chat_thread: { id: o.thread || 0, title: o.title || "", temporary: (o.thread || 0) < 0, admin_tools: !!o.adminTools },
          },
        },
      };
    }
    async open() {
      this.readyState = 1;
      // The session as it was before the page's first update.
      this.createdSession = this.session();
      if (this.onopen) this.onopen(new Event("open"));
      const o = mock.opts;
      const m = names(o.models);
      if (o.createdMs) await sleep(o.createdMs);
      mock.created = (mock.created || 0) + 1;
      this.emit({ type: "session.created", session: this.createdSession || this.session() });
      this.created = true;
      for (const f of this.afterCreated.splice(0)) f();
      for (const stage of ["asr", "chat", "tts"]) {
        if (stage === "chat" && o.heldChat) continue;
        this.emit({ type: "lmgw.model.state", stage, alias: m[stage], state: "loading", ms: null });
      }
      // The hold refuses a GPU chat model in the connect warm: held, at once.
      if (o.heldChat) this.emit({ type: "lmgw.model.state", stage: "chat", alias: m.chat, state: "held", cause: o.heldChat, ms: null });
      setTimeout(() => {
        for (const stage of ["asr", "chat", "tts"]) {
          if (stage === "chat" && o.heldChat) continue;
          this.emit({ type: "lmgw.model.state", stage, alias: m[stage], state: "ready", ms: stage === "tts" ? 1840 : null });
        }
      }, o.warmMs === undefined ? 400 : o.warmMs);
    }
    emit(ev) {
      if (this.readyState !== 1) return;
      ev.event_id = `event_mock_${++this.n}`;
      const m = new MessageEvent("message", { data: JSON.stringify(ev) });
      if (this.onmessage) this.onmessage(m);
    }
    error(code, message, param) {
      this.emit({ type: "error", error: { type: "invalid_request_error", code, message, param: param || null, event_id: null } });
    }
    send(text) {
      if (this.readyState !== 1) throw new Error("InvalidStateError: not open");
      const ev = JSON.parse(text);
      if (ev.type === "input_audio_buffer.append") {
        mock.appends++;
        mock.appendBytes += b64len(ev.audio);
        return;
      }
      mock.got.push(ev);
      if (ev.type === "session.update") {
        mock.updates++;
        const td = ev.session && ev.session.audio && ev.session.audio.input
          ? ev.session.audio.input.turn_detection : undefined;
        if (td !== undefined) this.detection = td === null ? null : { type: td.type, prefix_padding_ms: 300 };
        if (ev.session && ev.session.lmgw && "half_duplex" in ev.session.lmgw) this.halfDuplex = !!ev.session.lmgw.half_duplex;
        const answer = { type: "session.updated", session: this.session() };
        const ends = !this.detection && mock.userTurn;
        if (ends) mock.userTurn = false;
        const say = () => {
          this.emit(answer);
          // Turn detection off: the open turn ends, uncommitted.
          if (ends) this.emit({ type: "input_audio_buffer.speech_stopped", audio_end_ms: 0, item_id: "item_open" });
        };
        if (this.created) say();
        else this.afterCreated.push(say);
      } else if (ev.type === "input_audio_buffer.clear") {
        mock.clears++;
        mock.userTurn = false;
        this.emit({ type: "input_audio_buffer.cleared" });
      } else if (ev.type === "conversation.item.truncate") {
        mock.truncate(ev);
      } else if (ev.type === "response.cancel") {
        mock.cancels++;
        const r = mock.playingResponse;
        if (!r || !r.open || (ev.response_id && ev.response_id !== r.id)) {
          mock.cancelRefused++;
          this.error("response_cancel_not_active", "there is no active response to cancel");
        } else if (!r.audioDone) {
          // After the end of generation a cancel changes nothing.
          mock.cancel("client_cancelled");
        }
      } else if (ev.type === "input_audio_buffer.commit") {
        mock.commits++;
        this.emit({ type: "input_audio_buffer.committed", previous_item_id: null, item_id: `item_u${mock.commits}` });
        this.emit({ type: "conversation.item.input_audio_transcription.completed", item_id: `item_u${mock.commits}`,
          content_index: 0, transcript: mock.opts.question || "Wie spät ist es?", usage: { type: "duration", seconds: 1.2 } });
      } else if (ev.type === "response.create") {
        mock.respond();
      }
    }
    // The page's close: the session ends (a response still running is
    // stopped and cut at what was sent, or at a truncate that came first),
    // the journal drains, and only then the close is answered (module doc).
    close(code, reason) {
      if (this.readyState >= 2) return;
      this.readyState = 2;
      mock.closedByPage = { code, reason };
      mock.stopAudio = true;
      mock.userTurn = false;
      const r = mock.playingResponse;
      if (r && r.open && !r.audioDone) mock.cancel("client_cancelled");
      (async () => {
        if (r) {
          while (!r.finalized) await sleep(20);
        }
        // A turn whose transcript came during the drain is written now; the
        // page hears of it only from the read-back.
        if (mock.opts.lateUser) {
          rowsOf(this.tid).push({ id: 8000 + rowsOf(this.tid).length, role: "user", content: mock.opts.lateUser,
            voice: { via: "realtime", asr: names(mock.opts.models).asr } });
        }
        await sleep(mock.opts.drainMs === undefined ? 80 : mock.opts.drainMs);
        this.readyState = 3;
        mock.closedAt = performance.now();
        mock.readsAtClose = mock.threadReads;
        if (this.onclose) this.onclose(new CloseEvent("close", { code: 1000, reason: "", wasClean: true }));
      })();
    }
    serverClose(code, reason) {
      if (this.readyState >= 2) return;
      this.readyState = 3;
      mock.stopAudio = true;
      if (this.onclose) this.onclose(new CloseEvent("close", { code, reason, wasClean: code !== 1006 }));
    }
  }
  FakeSocket.CONNECTING = 0;
  FakeSocket.OPEN = 1;
  FakeSocket.CLOSING = 2;
  FakeSocket.CLOSED = 3;

  mock.install = (opts = {}) => {
    mock.opts = opts;
    window.WebSocket = function (url, protocols) {
      if (String(url).includes("/v1/realtime")) return new FakeSocket(url);
      return protocols === undefined ? new Real(url) : new Real(url, protocols);
    };
    Object.assign(window.WebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });
    window.WebSocket.prototype = Real.prototype;
    return "installed";
  };
  mock.uninstall = () => {
    window.WebSocket = Real;
    if (mock.unpatch) mock.unpatch();
    return "uninstalled";
  };

  // The thread JSON the page reads, with its speech models resolved to the
  // mock's (so the voice button is enabled on a gateway with none) — or,
  // with mode.admin, refused as on an Admin Chat thread. mode.gone answers
  // 404 as for a deleted thread, mode.kindAdmin says the thread is an Admin
  // Chat one (with its voice still resolved: what a refused handshake's
  // re-read sees); the turn detection resolves to mode.turnDetection, else
  // server_vad. `mode` is read at each fetch; mock.threadReads counts the
  // reads and mock.lastThreadRead says when the last one came.
  mock.patchThread = (mode = {}) => {
    if (mock.unpatch) return "patched";
    const real = window.fetch;
    mock.threadReads = 0;
    window.fetch = async function (input, init) {
      const url = typeof input === "string" ? input : input.url;
      const req = typeof input === "string" ? new Request(input, init) : input;
      if (req.method === "GET" && /\/chat\/api\/threads\/-?\d+$/.test(url)) {
        mock.threadReads++;
        mock.lastThreadRead = performance.now();
        if (mode.gone) {
          return new Response(JSON.stringify({ code: "not_found", message: "thread not found" }),
            { status: 404, headers: { "content-type": "application/json" } });
        }
        const resp = await real.call(window, req);
        const v = await resp.json();
        const r = v.thread && v.thread.voice_resolved;
        if (r) {
          const st = (alias) => ({ alias, source: "chat", inherited: alias, local: true, managed: true, cpu: true,
            fallback: null, fallback_unusable: null });
          const m = names(mode.models);
          r.asr = st(m.asr);
          r.tts = st(m.tts);
          r.problems = [];
          // The automatic mode the probes expect, whatever the gateway's
          // own default is (a dev copy may say push_to_talk).
          r.turn_detection = Object.assign({}, r.turn_detection || {}, { value: mode.turnDetection || "server_vad" });
          r.realtime = mode.admin
            ? { ok: false, code: "chat_thread_admin", reason: "Voice mode is not available in Admin Chat", admin_tools: false }
            : { ok: true, code: null, reason: null, admin_tools: !!mode.adminTools };
        }
        if (mode.kindAdmin && v.thread) v.thread.kind = "admin";
        // The journal's rows, when the session stored any for this thread.
        const tid = Number(url.split("/").pop());
        if (mock.rows[tid]) v.messages = mock.rows[tid].map((r) => Object.assign({}, r));
        return new Response(JSON.stringify(v), { status: resp.status, headers: { "content-type": "application/json" } });
      }
      return real.apply(this, arguments);
    };
    mock.unpatch = () => {
      window.fetch = real;
      mock.unpatch = null;
    };
    return "patched";
  };

  mock.startSpeech = () => {
    mock.userTurn = true;
    mock.current.emit({ type: "input_audio_buffer.speech_started", audio_start_ms: 0, item_id: "item_open" });
    return "speaking";
  };

  mock.speak = async (text, ms = 1200, o = {}) => {
    const s = mock.current;
    mock.opts.question = text || mock.opts.question;
    mock.startSpeech();
    await sleep(ms);
    if (!mock.userTurn) return null;
    mock.userTurn = false;
    s.emit({ type: "input_audio_buffer.speech_stopped", audio_end_ms: ms, item_id: "item_open" });
    mock.commits++;
    s.emit({ type: "input_audio_buffer.committed", previous_item_id: null, item_id: `item_u${mock.commits}` });
    await sleep(120);
    if (o.transcription) {
      // §5.2's shape: the error's code and param are left out when empty.
      const error = { type: "invalid_request_error", message: o.transcription.message || o.transcription.code };
      if (o.transcription.code) error.code = o.transcription.code;
      s.emit({ type: "conversation.item.input_audio_transcription.failed", item_id: `item_u${mock.commits}`,
        content_index: 0, error });
      return null;
    }
    s.emit({ type: "conversation.item.input_audio_transcription.completed", item_id: `item_u${mock.commits}`,
      content_index: 0, transcript: mock.opts.question || "Wie spät ist es?", usage: { type: "duration", seconds: ms / 1000 } });
    return mock.respond(o);
  };

  mock.commitOnly = () => {
    const s = mock.current;
    mock.userTurn = false;
    s.emit({ type: "input_audio_buffer.speech_stopped", audio_end_ms: 900, item_id: "item_open" });
    mock.commits++;
    s.emit({ type: "input_audio_buffer.committed", previous_item_id: null, item_id: `item_u${mock.commits}` });
    return "committed, no transcript";
  };

  // Cancel the response that plays (a barge-in's turn_detected, a client's
  // cancel): its cut, unless a truncate came first, is what was sent.
  mock.cancel = (reason) => {
    const r = mock.playingResponse;
    if (!r || !r.open || r.cancelled) return;
    r.cancelled = reason;
    r.cutMs = r.truncMs === undefined ? r.sentMs : r.truncMs;
  };

  mock.bargeIn = () => {
    mock.startSpeech();
    mock.cancel("turn_detected");
    return "barged";
  };

  mock.emitError = (code, message) => {
    mock.current.error(code, message || code);
    return code;
  };

  mock.takeOver = () => {
    const s = mock.current;
    s.error("chat_thread_taken_over", "voice mode moved to another window");
    s.serverClose(4000, "voice mode moved to another window");
    return "taken over";
  };

  // A truncate (module doc).
  mock.truncate = (ev) => {
    const s = mock.current;
    const r = mock.byItem[ev.item_id];
    if (!r) {
      mock.truncateErrors.push({ item_id: ev.item_id, why: "no such item" });
      s.error("invalid_value", `item '${ev.item_id}' does not exist`, "item_id");
      return;
    }
    if (ev.audio_end_ms > r.sentMs) {
      mock.truncateErrors.push({ item_id: ev.item_id, audio_end_ms: ev.audio_end_ms, sent_ms: r.sentMs });
      s.error("invalid_value", `audio_end_ms ${ev.audio_end_ms} is beyond the audio of item '${ev.item_id}' (${r.sentMs} ms)`,
        "audio_end_ms");
      return;
    }
    mock.truncates.push({ item_id: ev.item_id, audio_end_ms: ev.audio_end_ms, sent_ms: r.sentMs });
    s.emit({ type: "conversation.item.truncated", item_id: ev.item_id, content_index: 0, audio_end_ms: ev.audio_end_ms });
    if (!r.finalized) {
      // The table is cut; an item still being produced stops its response.
      r.truncMs = ev.audio_end_ms;
      if (r.open && !r.audioDone && !r.cancelled) mock.cancel("client_cancelled");
      else if (r.cancelled) r.cutMs = ev.audio_end_ms;
      return;
    }
    // After the finalize: a re-cut, written only when it cuts.
    mock.writeCut(r, ev.audio_end_ms, true);
  };

  // What the journal writes for `r` heard up to `ms` (`null`: whole), and
  // the lmgw.chat.reply it sends; `recut`: after the finalize, when only a
  // cut is written.
  mock.writeCut = (r, ms, recut) => {
    const s = mock.current;
    const m = names(mock.opts.models);
    const n = ms === null || ms === undefined ? r.reply.length : heardChars(r.reply, r.totalMs, ms);
    const heard = r.reply.slice(0, n).trim();
    const unheard = r.reply.slice(n).trim();
    const base = { via: "realtime", tts: m.tts, voice: m.voice, timing: r.timing };
    // Reasoning is never cut: it was not spoken (chat-voice §8.5).
    const reasoning = r.reasoning || "";
    let body;
    if (!unheard) {
      if (recut) return;
      body = { content: r.reply, unheard: null, voice: base };
    } else if (!heard) {
      body = { removed: true };
    } else {
      if (recut && r.stored && r.stored.content === heard) return;
      body = { content: heard, unheard, voice: Object.assign({}, base, { unheard }) };
    }
    r.stored = body;
    mock.stored.push(Object.assign({ message_id: r.replyId }, body));
    const rows = rowsOf(r.tid);
    const at = rows.findIndex((x) => x.id === r.replyId);
    if (body.removed) {
      if (at >= 0) rows.splice(at, 1);
    } else {
      const row = { id: r.replyId, role: "assistant", content: body.content, reasoning, voice: body.voice,
        model: m.chat };
      if (at >= 0) rows[at] = row;
      else rows.push(row);
    }
    const ev = Object.assign({ type: "lmgw.chat.reply", message_id: r.replyId }, body);
    mock.replies.push(ev);
    s.emit(ev);
  };

  // One response, as a bound turn makes it (module doc).
  mock.respond = async (o = {}) => {
    const s = mock.current;
    const n = ++mock.responses;
    const rid = `resp_mock_${n}`;
    const item = `item_a${n}`;
    const userId = 9000 + n * 2;
    const replyId = userId + 1;
    const tool = !!o.tool;
    const preamble = "Moment. ";
    const reply = tool
      ? preamble + (o.reply || "Es sind einundzwanzig Grad, schön warm.")
      : o.reply || mock.opts.reply || "Es ist halb neun. Der Termin beginnt gleich im großen Raum, bitte bring die Unterlagen mit.";
    const m = names(mock.opts.models);
    const totalMs = o.audioMs || mock.opts.audioMs || 3600;
    const reasoning = o.reasoning || mock.opts.reasoning || "";
    const reasoningNote = o.reasoningNote || mock.opts.reasoningNote || null;
    const r = { id: rid, item, replyId, reply, reasoning, totalMs, sentMs: 0, open: true, audioDone: false,
      cancelled: null, finalized: false, tid: s.tid };
    mock.byItem[item] = r;
    mock.playingResponse = r;
    mock.stopAudio = false;
    mock.sentMs = 0;
    r.timing = { response_id: rid, message_id: replyId, end_of_turn_ms: 412, asr_ms: 31,
      first_token_ms: reasoning ? 1408 : 208, first_clause_ms: 95, first_audio_ms: 36, total_ms: totalMs,
      to_first_audio_ms: reasoning ? 1982 : 782, cold: [],
      models: { asr: { alias: m.asr, answered_by: null }, chat: { alias: m.chat, answered_by: null },
        tts: { alias: m.tts, answered_by: null, voice: m.voice } } };
    if (reasoning) r.timing.reasoning_ms = 1200;
    const part = { response_id: rid, item_id: item, output_index: 0, content_index: 0 };
    const frame = (event, data) => s.emit({ type: "lmgw.chat.frame", response_id: rid, event, data });
    // The done frame's facts, as the server's chat turn says them.
    const doneData = () => Object.assign({ message_id: replyId, model: m.chat, saved: true },
      reasoningNote ? { reasoning_note: reasoningNote, reasoning_ignored: ["enabled"] }
        : { reasoning_note: null, reasoning_ignored: [] });
    s.emit({ type: "response.created", response: { id: rid, object: "realtime.response", status: "in_progress", output: [] } });
    const userVoice = { via: "realtime", asr: m.asr, asr_answered_by: null, asr_ms: 31, audio_ms: 1200 };
    const question = mock.opts.question || "Wie spät ist es?";
    s.emit({ type: "lmgw.chat.user", message_id: userId, content: question, voice: userVoice });
    rowsOf(s.tid).push({ id: userId, role: "user", content: question, voice: userVoice });
    frame("turn", { user_message_id: userId });
    if (o.refuse) {
      // The chat turn's refusal (module doc): nothing is generated, saved
      // or spoken.
      const f = o.refuse;
      const states = f.held ? [{ state: "held", cause: f.held }]
        : f.code === "vram_queue_timeout" ? [{ state: "loading" }, { state: "failed", message: f.message }] : [];
      for (const st of states) {
        const data = Object.assign({ stage: "chat", alias: m.chat, ms: null }, st);
        s.emit(Object.assign({ type: "lmgw.model.state" }, data));
        frame("state", data);
      }
      frame("error", { message: f.message, code: f.code });
      frame("done", { aborted: true });
      const kind = { context_length_exceeded: "invalid_request_error", unknown_alias: "invalid_request_error",
        key_budget: "permission_error" }[f.code] || "api_error";
      const error = { type: kind, code: f.code, message: f.message, param: null };
      s.emit({ type: "error", error: Object.assign({ event_id: null }, error) });
      s.emit({ type: "response.done", response: { id: rid, object: "realtime.response", status: "failed",
        status_details: { type: "failed", error }, output: [] } });
      r.open = false;
      r.finalized = true;
      if (mock.playingResponse === r) mock.playingResponse = null;
      return { rid, sent: 0, refused: f.code };
    }
    const words = reply.split(/(?<= )/);
    const answerAt = tool ? 1 : 0;
    // The reasoning comes first, and only as frames: nothing of it is
    // spoken or captioned.
    for (const t of reasoning.split(/(?<= )/).filter(Boolean)) frame("reasoning", { text: t });
    for (const w of words.slice(0, tool ? 1 : words.length)) frame("delta", { text: w });
    if (!tool) frame("done", doneData());
    await sleep(mock.opts.thinkMs === undefined ? 900 : mock.opts.thinkMs);
    // Audio paced at real time, 100 ms chunks, the spoken words beside it;
    // a tool turn's audio waits for its tool after the preamble.
    const preMs = tool ? Math.round((totalMs * preamble.trim().length) / reply.length) : 0;
    let wi = 0;
    let t0 = performance.now();
    let base = 0;
    const playTo = async (endMs) => {
      while (r.sentMs < endMs && !r.cancelled && !mock.stopAudio) {
        const pcm = voice((r.sentMs * RATE) / 1000, (100 * RATE) / 1000);
        s.emit(Object.assign({ type: "response.output_audio.delta", delta: b64(pcm) }, part));
        r.sentMs += 100;
        mock.sentMs = r.sentMs;
        while (wi < words.length && (wi / words.length) * totalMs <= r.sentMs) {
          s.emit(Object.assign({ type: "response.output_audio_transcript.delta", delta: words[wi++] }, part));
        }
        // A lead of LEAD_MS, then real time.
        const ahead = r.sentMs - base - LEAD_MS - (performance.now() - t0);
        if (ahead > 0) await sleep(ahead);
      }
    };
    if (tool) {
      await playTo(preMs);
      if (!r.cancelled && !mock.stopAudio) {
        frame("tool", { event: "start", index: 0, name: "stub__echo" });
        frame("tool", { event: "ready", index: 0, name: "stub__echo", arguments: { text: "21" } });
        await sleep(o.toolMs || mock.opts.toolMs || 1500);
        frame("tool", { event: "result", index: 0, output: "21", is_error: false, ms: 12 });
        for (const w of words.slice(answerAt)) frame("delta", { text: w });
        frame("done", doneData());
        // Paced afresh from here: the silence was the tool's.
        t0 = performance.now();
        base = r.sentMs;
      }
    }
    await playTo(totalMs);
    // The socket went (a takeover, the page's close): stopped at what was
    // sent, or at a truncate that came first.
    if (!r.cancelled && mock.stopAudio && r.open && !r.audioDone) {
      r.cancelled = "client_cancelled";
      r.cutMs = r.truncMs === undefined ? r.sentMs : r.truncMs;
    }
    // The audio part closes first, cancelled or not (WP3 review m4).
    const said = r.cancelled ? words.slice(0, wi).join("") : reply;
    s.emit(Object.assign({ type: "response.output_audio_transcript.done", transcript: said }, part));
    s.emit(Object.assign({ type: "response.output_audio.done" }, part));
    r.audioDone = true;
    if (!r.cancelled && o.doneAfterPlayout) {
      // A bound response is done once pacing's modelled playback drained.
      await sleep(LEAD_MS + 60);
    }
    r.open = false;
    if (r.cancelled) {
      s.emit({ type: "response.done", response: { id: rid, object: "realtime.response", status: "cancelled",
        status_details: { type: "cancelled", reason: r.cancelled }, output: [] } });
    } else {
      s.emit({ type: "response.done", response: { id: rid, object: "realtime.response", status: "completed", output: [] } });
    }
    // The turn's save; a truncate that comes meanwhile is the cut.
    await sleep(mock.opts.saveMs || 0);
    const cut = r.cancelled ? (r.truncMs === undefined ? r.cutMs : r.truncMs) : r.truncMs;
    mock.writeCut(r, cut === undefined ? null : cut, false);
    r.finalized = true;
    s.emit(Object.assign({ type: "lmgw.response.timing" }, r.timing));
    if (mock.playingResponse === r) mock.playingResponse = null;
    return { rid, sent: r.sentMs, cancelled: r.cancelled };
  };

  mock.heardChars = heardChars;
  window.lmgwRealtimeMock = mock;
  return "lmgwRealtimeMock ready";
})();
