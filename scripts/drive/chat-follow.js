// Helpers for scripts/drive/chat-follow.json, on top of scripts/drive/chat-media.js (window.__cm):
// the transcript pane's distance from its end, and a wait counted in frames. Every helper answers
// at once (WebKit's drive does not await a promise).
(() => {
  const pane = () => document.getElementById('chat-scroll');
  window.__cf = {
    // How far the end of the transcript is below the pane's view, in CSS px (0 at the end).
    gap: () => { const p = pane(); return p ? Math.round(p.scrollHeight - p.scrollTop - p.clientHeight) : -1; },
    top: () => Math.round(pane()?.scrollTop ?? -1),
    // [gap, scrollTop, scrollHeight, clientHeight], for the log.
    m: () => { const p = pane(); return JSON.stringify(p ? [__cf.gap(), __cf.top(), p.scrollHeight, p.clientHeight] : null); },
    // The reader goes to the end, or turns the wheel `px` up from where they are: the wheel event
    // the page hears, then the scroll it makes (a synthetic wheel event scrolls nothing itself).
    bottom: () => { const p = pane(); p.scrollTop = p.scrollHeight; return 'at the end'; },
    up: px => {
      const p = pane();
      p.dispatchEvent(new WheelEvent('wheel', { deltaY: -px, bubbles: true, cancelable: true }));
      p.scrollTop = Math.max(0, p.scrollTop - px);
      window.__cfTop = __cf.top();
      return 'up to ' + window.__cfTop;
    },
    // What WebKitGTK did as a picture's live pane mounted, made on purpose: the transcript grows
    // (a block of `grow` px at its end) and in the same task the pane drops `px` up, with no wheel,
    // key or press. unjolt takes the block out again.
    jolt: (grow, px) => {
      const p = pane();
      const d = document.createElement('div');
      d.className = 'cf-jolt';
      d.style.height = grow + 'px';
      p.querySelector('.chat-col').appendChild(d);
      p.scrollTop = Math.max(0, p.scrollTop - px);
      return 'grew ' + grow + ', moved to ' + __cf.top();
    },
    unjolt: () => { document.querySelectorAll('.cf-jolt').forEach(d => d.remove()); return 'removed'; },
    // True once the pane's size, scroll position and content height have read the same over `n`
    // animation frames in a row (a fixed time says nothing about how many frames the page got:
    // under Broadway WebKit can leave an idle page undrawn). A call that finds no run of frames
    // starts one; the first call after it ends reads true and clears it.
    settled: n => {
      const s = window.__cfSettle;
      if (s && s.done) { window.__cfSettle = null; return true; }
      if (!s) {
        const st = window.__cfSettle = { last: null, same: 0, done: false };
        const frame = () => {
          const p = pane();
          const k = p && [p.scrollHeight, p.scrollTop, p.clientHeight].join();
          st.same = k && k === st.last ? st.same + 1 : 0;
          st.last = k;
          if (st.same >= n) st.done = true; else requestAnimationFrame(frame);
        };
        requestAnimationFrame(frame);
      }
      return false;
    },
    // Every scroll event of the pane since the helpers loaded, as [ms, scrollTop, scrollHeight]:
    // which way the pane moved and why, when a check fails.
    hookScroll: () => {
      if (window.__cfLog) return 'hooked';
      window.__cfLog = [];
      const p = pane();
      p.addEventListener('scroll', () => __cfLog.push([Math.round(performance.now()), Math.round(p.scrollTop), p.scrollHeight]), { passive: true });
      return 'hooked';
    },
    log: () => JSON.stringify((window.__cfLog || []).slice(-12)),
    // Every picture in the newest reply is decoded.
    pics: () => [...(__cm.reply()?.querySelectorAll('.media-frame img') || [])].every(i => i.complete && i.naturalWidth > 0),
  };
  return 'follow helpers';
})()
