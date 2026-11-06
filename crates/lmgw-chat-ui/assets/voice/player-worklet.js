// lmgw's playback worklet (chat-voice design §11.1), registered as
// `lmgw-player` in the page's one 24 kHz playback context.
//
// It plays queued PCM16 mono chunks back to back, so consecutive chunks are
// gapless, and reports what it actually rendered, per item (one read-aloud,
// one realtime response, the test tone):
//
//   in:  {type: "push", item, pcm}   pcm: an ArrayBuffer of PCM16-LE samples
//        {type: "end", item}         no more audio for the item
//        {type: "flush", id, item?}  drop what is queued (of the item, or all)
//   out: {type: "progress", item, played, queued}   about every 50 ms of audio
//        {type: "underrun", items: [{item, pushed, played}]}  the queue ran dry
//                                    while these items wait for more audio
//        {type: "drained"}           the queue ran dry and no item waits for
//                                    more: everything ended or was flushed
//        {type: "ended", item, pushed, played}  an ended item played to its end
//        {type: "flushed", id, items: [{item, pushed, played}]}
//
// `played` counts samples rendered into the graph. An item is forgotten here
// once it ended or was flushed: the page keeps its final count and drops any
// later push for it (the page is this worklet's only feeder), so a flushed
// item never plays again. A push for an item marked ended is ignored too.
// The queue holds the PCM16 samples as they came (half the memory of floats)
// and converts as it renders; there is no cap on what it holds.
class LmgwPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queue = []; // {item, pcm: Int16Array, off}
    this.items = new Map(); // item -> {pushed, played, ended}
    this.queued = 0;
    this.current = null;
    this.sinceProgress = 0;
    this.progressEvery = Math.round(sampleRate * 0.05);
    this.playing = false;
    // An underrun was reported and no `drained` has followed yet.
    this.dry = false;
    this.port.onmessage = (e) => this.onMessage(e.data);
  }

  stat(item) {
    let s = this.items.get(item);
    if (!s) {
      s = { pushed: 0, played: 0, ended: false };
      this.items.set(item, s);
    }
    return s;
  }

  onMessage(m) {
    if (m.type === "push") {
      const pcm = new Int16Array(m.pcm);
      const s = this.stat(m.item);
      if (s.ended) return;
      s.pushed += pcm.length;
      if (pcm.length) {
        this.queue.push({ item: m.item, pcm, off: 0 });
        this.queued += pcm.length;
      }
    } else if (m.type === "end") {
      this.stat(m.item).ended = true;
      this.settle(m.item);
    } else if (m.type === "flush") {
      const all = m.item === undefined || m.item === null;
      const keep = [];
      for (const c of this.queue) {
        if (all || c.item === m.item) this.queued -= c.pcm.length - c.off;
        else keep.push(c);
      }
      this.queue = keep;
      const items = [];
      for (const [item, s] of this.items) {
        if (all || item === m.item) items.push({ item, pushed: s.pushed, played: s.played });
      }
      for (const r of items) this.items.delete(r.item);
      this.port.postMessage({ type: "flushed", id: m.id, items });
      this.maybeDrained();
    }
  }

  // An ended item with nothing left in the queue has played to its end.
  settle(item) {
    const s = this.items.get(item);
    if (!s || !s.ended || this.queue.some((c) => c.item === item)) return;
    this.items.delete(item);
    this.port.postMessage({ type: "ended", item, pushed: s.pushed, played: s.played });
    this.maybeDrained();
  }

  // Items still waiting for audio (not ended).
  waiting() {
    const w = [];
    for (const [item, s] of this.items) {
      if (!s.ended) w.push({ item, pushed: s.pushed, played: s.played });
    }
    return w;
  }

  // After an underrun, the last waiting item ending (or being flushed) with
  // the queue empty is the drain.
  maybeDrained() {
    if (!this.dry || this.queue.length || this.waiting().length) return;
    this.dry = false;
    this.port.postMessage({ type: "drained" });
  }

  process(_inputs, outputs) {
    const channels = outputs[0];
    const out = channels[0];
    let i = 0;
    while (i < out.length && this.queue.length) {
      const c = this.queue[0];
      const n = Math.min(out.length - i, c.pcm.length - c.off);
      for (let k = 0; k < n; k++) out[i + k] = c.pcm[c.off + k] / 32768;
      c.off += n;
      i += n;
      this.queued -= n;
      this.stat(c.item).played += n;
      this.current = c.item;
      if (c.off >= c.pcm.length) {
        this.queue.shift();
        this.settle(c.item);
      }
    }
    if (i < out.length) out.fill(0, i);
    for (let ch = 1; ch < channels.length; ch++) channels[ch].set(out);
    if (i > 0) {
      this.playing = true;
      this.dry = false;
      this.sinceProgress += i;
      if (this.sinceProgress >= this.progressEvery) {
        this.sinceProgress = 0;
        const s = this.items.get(this.current);
        this.port.postMessage({
          type: "progress",
          item: this.current,
          played: s ? s.played : null,
          queued: this.queued,
        });
      }
    }
    if (this.playing && this.queue.length === 0) {
      this.playing = false;
      this.sinceProgress = 0;
      const waiting = this.waiting();
      if (waiting.length) {
        // A clause synthesised slower than real time: not the end.
        this.dry = true;
        this.port.postMessage({ type: "underrun", items: waiting });
      } else {
        this.port.postMessage({ type: "drained" });
      }
    }
    return true;
  }
}

registerProcessor("lmgw-player", LmgwPlayer);
