// A fake microphone that speaks on cue (chat-voice WP9 drives): the page's
// getUserMedia answers a stream from a Web Audio graph, and the drive says
// a TTS-generated clip into it when it wants — a question, then a second
// utterance over the reply for a barge-in. No real microphone is opened,
// nothing is audible (the stream feeds only the page's capture), and no
// recording of a person is used: the clips come from scripts/drive/
// make-voice-fixtures.py (a TTS model's renders of synthetic text), loaded
// before this file as window.lmgwVoiceFixtures = {name: base64 WAV}.
//
//   fakeVoice.install()   replace navigator.mediaDevices.getUserMedia; every
//                         call is recorded with its constraints (asked) and
//                         every track it handed out is kept (tracks).
//   fakeVoice.say(name)   play a clip into the stream; resolves with its
//                         length in ms once it has played.
(() => {
  const V = { asked: [], tracks: [], ctx: null, dest: null, buffers: {} };
  const md = navigator.mediaDevices;
  const real = md.getUserMedia.bind(md);

  function bytes(b64) {
    const s = atob(b64);
    const u = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) u[i] = s.charCodeAt(i);
    return u.buffer;
  }

  function graph() {
    if (!V.ctx) {
      V.ctx = new AudioContext({ sampleRate: 48000 });
      V.dest = V.ctx.createMediaStreamDestination();
      // A whisper of noise under the clips, as a real line has (-70 dBFS).
      const noise = V.ctx.createBufferSource();
      const buf = V.ctx.createBuffer(1, 48000, 48000);
      const d = buf.getChannelData(0);
      for (let i = 0; i < d.length; i++) d[i] = (Math.random() * 2 - 1) * 0.0003;
      noise.buffer = buf;
      noise.loop = true;
      noise.connect(V.dest);
      noise.start();
    }
    return V.ctx;
  }

  V.install = () => {
    md.getUserMedia = async (c) => {
      V.asked.push(JSON.parse(JSON.stringify(c || {})));
      if (!c || !c.audio || c.video) return real(c);
      const ctx = graph();
      await ctx.resume();
      // A fresh stream of a clone of the destination's track per call, as
      // a device gives each capture its own track.
      const t = V.dest.stream.getAudioTracks()[0].clone();
      V.tracks.push(t);
      return new MediaStream([t]);
    };
    return "fake voice installed";
  };

  V.say = async (name) => {
    const ctx = graph();
    await ctx.resume();
    if (!V.buffers[name]) {
      const b64 = (window.lmgwVoiceFixtures || {})[name];
      if (!b64) throw new Error(`no voice fixture "${name}"`);
      V.buffers[name] = await ctx.decodeAudioData(bytes(b64));
    }
    const src = ctx.createBufferSource();
    src.buffer = V.buffers[name];
    src.connect(V.dest);
    const done = new Promise((r) => (src.onended = r));
    src.start();
    await done;
    return Math.round(V.buffers[name].duration * 1000);
  };

  V.live = () => V.tracks.filter((t) => t.readyState === "live").length;

  window.fakeVoice = V;
  return "fakeVoice ready";
})();
