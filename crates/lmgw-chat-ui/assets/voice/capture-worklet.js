// lmgw's capture worklet (chat-voice design §11.2), registered as
// `lmgw-capture` in a capture context running at the microphone's own rate.
//
// It mixes the input to mono, resamples it to `targetRate` (24 kHz for
// realtime, 16 kHz for dictation) with a windowed-sinc low-pass, and posts
// fixed-length PCM16 chunks (`chunkMs`, 40 ms by default):
//
//   options: {targetRate, chunkMs, prerollMs, gated}
//   in:  {type: "gate", open}  while gated, chunks are held in a ring as long
//                              as `prerollMs` (push-to-talk's pre-roll); open
//                              sends the ring, then live chunks; closing
//                              sends the partial chunk first (the end of the
//                              utterance), then answers `gated`
//        {type: "flush"}       send the partial chunk now, then `flushed`
//                              (while gated the partial chunk is pre-roll and
//                              stays)
//        {type: "stop"}        stop processing for good
//   out: {type: "chunk", seq, pcm}  pcm: an ArrayBuffer of PCM16-LE samples
//        {type: "gated", seq}       the gate is closed; every chunk of the
//                                   utterance was posted before this
//        {type: "flushed", seq}
//
// The node must be pulled: the page connects it to the destination through a
// zero gain, because WebKit renders by pulling from the destination and a
// worklet that is not connected may never run.

// Kernel table resolution (steps per input sample) and the filter's length
// in zero crossings on each side of the centre. 24 keeps the band flat to
// 10 kHz (into 24 kHz) and every alias at least 50 dB down, for about 100
// taps per output sample at 48 → 24 kHz and 290 at 44.1 → 16 kHz, a delay of
// 1.1 and 3.3 ms (scripts/worklet-check.mjs measures it).
const STEPS = 64;
const CROSSINGS = 24;

class LmgwCapture extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = (options && options.processorOptions) || {};
    const chunkMs = o.chunkMs || 40;
    this.target = o.targetRate || 24000;
    this.chunk = Math.max(1, Math.round((this.target * chunkMs) / 1000));
    this.prerollMax = Math.ceil((o.prerollMs || 0) / chunkMs);
    this.gated = !!o.gated;
    this.ring = [];
    this.seq = 0;
    this.out = new Int16Array(this.chunk);
    this.fill = 0;
    this.stopped = false;
    // Input samples per output sample.
    this.ratio = sampleRate / this.target;
    this.direct = Math.abs(this.ratio - 1) < 1e-9;
    if (!this.direct) {
      // Cut-off a little below the lower of the two Nyquist frequencies, in
      // cycles per input sample; a Blackman-windowed sinc of CROSSINGS zero
      // crossings each side.
      const w = (0.5 * Math.min(sampleRate, this.target) * 0.92) / sampleRate;
      this.half = CROSSINGS / (2 * w);
      const n = Math.ceil(this.half * STEPS) + 2;
      this.table = new Float32Array(n);
      for (let i = 0; i < n; i++) {
        const d = i / STEPS;
        if (d >= this.half) continue;
        const x = 2 * w * d;
        const sinc = x === 0 ? 1 : Math.sin(Math.PI * x) / (Math.PI * x);
        const u = d / this.half;
        const win = 0.42 + 0.5 * Math.cos(Math.PI * u) + 0.08 * Math.cos(2 * Math.PI * u);
        this.table[i] = 2 * w * sinc * win;
      }
      const pad = Math.ceil(this.half) + 1;
      this.buf = new Float32Array(4096 + 2 * pad);
      // Zeros of history before the first sample, and the position (in input
      // samples) of the next output sample.
      this.len = pad;
      this.t = pad;
    }
    this.port.onmessage = (e) => this.onMessage(e.data);
  }

  onMessage(m) {
    if (m.type === "gate") {
      if (m.open && this.gated) {
        for (const c of this.ring) this.post(c);
        this.ring = [];
      } else if (!m.open && !this.gated) {
        // Push-to-talk's release: the utterance ends here, to the sample.
        this.postPartial();
      }
      this.gated = !m.open;
      if (!m.open) this.port.postMessage({ type: "gated", seq: this.seq });
    } else if (m.type === "flush") {
      if (!this.gated) this.postPartial();
      this.port.postMessage({ type: "flushed", seq: this.seq });
    } else if (m.type === "stop") {
      this.stopped = true;
    }
  }

  postPartial() {
    if (this.fill > 0) this.post(this.out.slice(0, this.fill));
    this.fill = 0;
  }

  post(pcm) {
    this.port.postMessage({ type: "chunk", seq: this.seq++, pcm: pcm.buffer }, [pcm.buffer]);
  }

  emit(sample) {
    const s = sample > 1 ? 1 : sample < -1 ? -1 : sample;
    this.out[this.fill++] = s < 0 ? Math.round(s * 32768) : Math.round(s * 32767);
    if (this.fill === this.chunk) {
      const c = this.out;
      this.out = new Int16Array(this.chunk);
      this.fill = 0;
      if (!this.gated) this.post(c);
      else if (this.prerollMax > 0) {
        this.ring.push(c);
        if (this.ring.length > this.prerollMax) this.ring.shift();
      }
    }
  }

  process(inputs) {
    if (this.stopped) return false;
    const input = inputs[0];
    if (!input || input.length === 0) return true;
    const frames = input[0].length;
    const chans = input.length;
    if (this.direct) {
      for (let i = 0; i < frames; i++) {
        let s = 0;
        for (let c = 0; c < chans; c++) s += input[c][i];
        this.emit(s / chans);
      }
      return true;
    }
    if (this.len + frames > this.buf.length) {
      const grown = new Float32Array((this.len + frames) * 2);
      grown.set(this.buf.subarray(0, this.len));
      this.buf = grown;
    }
    const buf = this.buf;
    for (let i = 0; i < frames; i++) {
      let s = 0;
      for (let c = 0; c < chans; c++) s += input[c][i];
      buf[this.len + i] = s / chans;
    }
    this.len += frames;
    const half = this.half;
    const tab = this.table;
    while (this.t + half < this.len) {
      const t = this.t;
      const k0 = Math.max(0, Math.ceil(t - half));
      const k1 = Math.floor(t + half);
      let acc = 0;
      let norm = 0;
      for (let k = k0; k <= k1; k++) {
        const p = Math.abs(t - k) * STEPS;
        const j = p | 0;
        const h = tab[j] + (tab[j + 1] - tab[j]) * (p - j);
        acc += buf[k] * h;
        norm += h;
      }
      this.emit(norm > 0 ? acc / norm : 0);
      this.t += this.ratio;
    }
    // Drop the input no later output sample reaches back to.
    const drop = Math.floor(this.t - half) - 1;
    if (drop > 0) {
      buf.copyWithin(0, drop, this.len);
      this.len -= drop;
      this.t -= drop;
    }
    return true;
  }
}

registerProcessor("lmgw-capture", LmgwCapture);
