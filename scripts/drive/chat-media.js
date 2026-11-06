// Helpers for scripts/drive/chat-media.json (run by scripts/chat-media-drive.sh, which puts this
// file into the drive's `"js": "__HELPERS__"` steps: webkit-check.py has no `script` step). Every
// helper answers at once (WebKit's drive does not await a promise): work that takes its time
// leaves its answer on window for an `until` step. A full reload drops them; the drive runs the
// step again after each `nav`.
(() => {
  const q = s => document.querySelector(s);
  const qa = s => [...document.querySelectorAll(s)];
  const last = s => qa(s).pop();
  window.__cm = {
    // The newest reply, and the newest image card and live pane on the page.
    reply: () => last('.msg-assistant'),
    card: () => last('.media-card'),
    live: () => last('.media-live'),
    facts: () => { const c = __cm.card(); return c ? c.querySelector('.media-facts').textContent : ''; },
    notApplied: () => { const c = __cm.card(); return c ? c.querySelectorAll('.media-facts')[1]?.textContent || '' : ''; },
    prompt: () => { const c = __cm.card(); return c?.querySelector('.media-prompt')?.textContent || ''; },
    thumbs: () => { const c = __cm.card(); return c ? c.querySelectorAll('.media-thumb').length : 0; },
    // The strip's selected picture is the newest one, and the big picture is it.
    newestShown: () => {
      const c = __cm.card(); if (!c) return false;
      const t = [...c.querySelectorAll('.media-thumb')];
      const sel = t.filter(x => x.getAttribute('aria-selected') === 'true');
      const ids = t.map(x => +x.dataset.attachment);
      const big = +c.querySelector('.media-pic')?.dataset.attachment;
      return sel.length === 1 && +sel[0].dataset.attachment === Math.max(...ids) && big === Math.max(...ids);
    },
    loaded: () => { const i = __cm.card()?.querySelector('.media-pic img'); return !!i && i.complete && i.naturalWidth > 0; },
    // Type into a field the way an input event says it, then press its button.
    set: (sel, v) => { const t = q(sel); t.value = v; t.dispatchEvent(new Event('input', { bubbles: true })); return v; },
    // Type the message, then press Send once the page has taken the text (the button is
    // disabled while the box is empty, and enabled a moment after the input event).
    send: s => {
      __cm.set('.composer-input', s);
      const press = (n) => {
        const b = q('button[aria-label="Send"]');
        if (b && !b.disabled) b.click();
        else if (n > 0) setTimeout(() => press(n - 1), 100);
      };
      setTimeout(() => press(50), 50);
      return 'typed';
    },
    canSend: () => {
      const b = q('button[aria-label="Send"]');
      return !!b && !q('.msg-assistant .caret');
    },
    // A user message with this text is on the page.
    sent: s => qa('.msg-item-user .msg-user-text').some(e => e.textContent.includes(s)),
    act: name => {
      const b = __cm.card()?.querySelector(`[data-media-action="${name}"]`);
      if (!b) return 'missing ' + name;
      b.click();
      return 'clicked ' + name;
    },
    form: kind => q(`[data-media-form="${kind}"]`),
    formButton: (kind, primary) => {
      const f = __cm.form(kind); if (!f) return 'no form ' + kind;
      const b = f.querySelector(primary ? '.btn.primary' : '.btn.ghost'); b.click(); return 'clicked';
    },
    notice: () => __cm.card()?.querySelector('.media-notice')?.textContent || '',
    // A Select (lmgw-ui-kit's): open the one inside `sel`, list the open one's options, pick
    // the option whose label starts with `prefix`, read the label it shows.
    openSelect: sel => { const b = q(sel + ' .select-btn'); if (!b) return 'no select in ' + sel; b.click(); return 'opened'; },
    options: () => qa('.select-opt').filter(o => o.getClientRects().length).map(o => o.textContent.trim()),
    pickOption: prefix => {
      const o = qa('.select-opt').filter(x => x.getClientRects().length).find(x => x.textContent.trim().startsWith(prefix));
      if (!o) return 'no option ' + prefix + ' among ' + __cm.options().join(' | ');
      o.click();
      return 'picked ' + o.textContent.trim();
    },
    // The label a Select shows, without its arrow.
    selected: sel => q(sel + ' .select-btn > span:first-child')?.textContent.trim() || '',
    // GET `url` (POST `body` as JSON when given); the answer lands in __cm.got(key) — WebKit's
    // drive does not await, so an `until` step asks again until it is there.
    fetchJson: (key, url, body) => {
      window.__cmGot = window.__cmGot || {};
      delete window.__cmGot[key];
      const init = body === undefined ? {} : { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) };
      fetch(url, init).then(r => r.json()).then(v => { window.__cmGot[key] = v; }, e => { window.__cmGot[key] = { error: String(e) }; });
      return 'asked ' + url;
    },
    got: key => (window.__cmGot || {})[key],
    // Ask again until `pred` holds for the answer to `url` (one request in flight at a time).
    poll: (key, url, pred) => {
      const v = __cm.got(key);
      if (v !== undefined && pred(v)) return true;
      if (!window.__cmAsking?.[key]) {
        window.__cmAsking = window.__cmAsking || {};
        window.__cmAsking[key] = true;
        fetch(url).then(r => r.json()).then(v => { window.__cmGot = window.__cmGot || {}; window.__cmGot[key] = v; })
          .finally(() => { window.__cmAsking[key] = false; });
      }
      return false;
    },
    cancelButton: () => __cm.live()?.querySelector('.media-live-actions button'),
    lastToolText: () => last('.tool-card')?.textContent || '',
    lastTool: () => last('.tool-card'),
    // Open a thread the way a person does: its row in the thread list.
    openThread: id => {
      const r = q(`.thread-row[data-thread="${id}"]`);
      if (!r) return 'no row for thread ' + id;
      r.click();
      return 'opened ' + id;
    },
    // Save: what a download link would fetch, recorded instead of leaving the page.
    hookSave: () => {
      if (window.__cmSaved) return 'hooked';
      window.__cmSaved = [];
      const click = HTMLAnchorElement.prototype.click;
      HTMLAnchorElement.prototype.click = function () {
        if (this.hasAttribute('download')) {
          __cmSaved.push({ href: this.getAttribute('href'), name: this.getAttribute('download') });
          return;
        }
        return click.call(this);
      };
      return 'hooked';
    },
    // Whether a URL's bytes are a PNG, on window.__cmPng once read.
    checkPng: href => {
      window.__cmPng = undefined;
      fetch(href).then(r => r.arrayBuffer()).then(b => {
        const u = new Uint8Array(b);
        window.__cmPng = u.length > 8 && u[0] === 0x89 && u[1] === 0x50 && u[2] === 0x4e && u[3] === 0x47;
      }).catch(() => { window.__cmPng = false; });
      return 'reading ' + href;
    },
    // Whether a URL's bytes start with `magic` (an ASCII signature), on window.__cmMagic once
    // read. The player loads nothing until played (preload none), and WebKit under Broadway has
    // no audio output to play to.
    checkMagic: (href, magic) => {
      window.__cmMagic = undefined;
      fetch(href).then(r => r.arrayBuffer()).then(b => {
        const u = new Uint8Array(b.slice(0, magic.length));
        window.__cmMagic = String.fromCharCode(...u) === magic;
      }).catch(() => { window.__cmMagic = false; });
      return 'reading ' + href;
    },
    drafts: () => qa('.draft-chips .chip').map(c => c.textContent.trim()).join(' | '),
    // The card, its strip and its forms inside the reply's column, and nothing in it scrolling
    // sideways (with html.tauri's classic bars, as the app window draws them).
    fits: () => {
      const c = __cm.card(); if (!c) return 'no card';
      const col = c.closest('.msg-assistant').getBoundingClientRect();
      const out = [...c.querySelectorAll('*')].filter(e => {
        const b = e.getBoundingClientRect();
        return b.width > 0 && (b.right > col.right + 1 || b.left < col.left - 1);
      }).map(e => e.className || e.tagName);
      const bars = [c, ...c.querySelectorAll('*')].filter(e => {
        const s = getComputedStyle(e);
        return (s.overflowX === 'auto' || s.overflowX === 'scroll') && e.scrollWidth > e.clientWidth + 1;
      }).map(e => e.className || e.tagName);
      return out.length === 0 && bars.length === 0 ? true : JSON.stringify({ out, bars });
    },
    tauri: () => {
      const h = document.documentElement;
      h.classList.add('tauri');
      h.style.display = 'none'; void h.offsetHeight; h.style.display = ''; void h.offsetHeight;
      return 'tauri';
    },
    // Bring the newest card's lowest part (an open form or notice, else its buttons) into view,
    // for a shot.
    view: () => {
      const c = __cm.card();
      const el = c && [...c.querySelectorAll('.media-notice, .media-form, .media-actions')].pop();
      (el || c)?.scrollIntoView({ block: 'end' });
      return 'in view';
    },
    player: () => __cm.reply()?.querySelector('.media-audio') || last('.media-audio'),
    // When each SSE frame of a Chat turn or a card action reached the page: the page's fetch
    // is wrapped once, its body teed, and per frame the time its first byte came and the time
    // it was whole (its blank line) are kept on window.__cmWire, one entry per stream. WebKitGTK
    // under Broadway once handed the page a frame's tail only with the next bytes (design §14
    // item 54); wireLag says the longest such wait of the streams so far.
    hookWire: () => {
      if (window.__cmWire) return 'hooked';
      window.__cmWire = [];
      const orig = window.fetch;
      window.fetch = function (input) {
        const url = typeof input === 'string' ? input : input.url;
        const p = orig.apply(this, arguments);
        if (!/\/chat\/api\/(threads\/\d+\/send|attachments\/\d+\/media)$/.test(url)) return p;
        return p.then(r => {
          if (!r.body) return r;
          const [a, b] = r.body.tee();
          const rec = { url: url.replace(/^https?:\/\/[^/]+/, ''), frames: [] };
          __cmWire.push(rec);
          const reader = b.getReader(), dec = new TextDecoder();
          let buf = '', first = null;
          const pump = () => reader.read().then(({ done, value }) => {
            if (done) return;
            const t = performance.now();
            buf += dec.decode(value, { stream: true });
            if (first === null && buf.length) first = t;
            for (let i; (i = buf.indexOf('\n\n')) >= 0;) {
              const f = buf.slice(0, i);
              buf = buf.slice(i + 2);
              const ev = f.startsWith(':') ? ':' : ((f.match(/^event: ?(.*)$/m) || [])[1] || 'message');
              rec.frames.push({ event: ev, first, whole: t, bytes: f.length });
              first = buf.length ? t : null;
            }
            return pump();
          });
          pump().catch(() => {});
          return new Response(a, { status: r.status, statusText: r.statusText, headers: r.headers });
        });
      };
      return 'hooked';
    },
    // The longest wait (ms, rounded) between a frame's first byte and its last, over every
    // data frame of the streams read so far; the frames that waited over 100 ms in `slow`.
    wire: () => {
      const all = (window.__cmWire || []).flatMap(r => r.frames.map(f => ({ ...f, url: r.url })));
      const data = all.filter(f => f.event !== ':');
      const lag = f => Math.round(f.whole - f.first);
      return {
        streams: (window.__cmWire || []).length,
        frames: data.length,
        comments: all.length - data.length,
        lag: Math.max(0, ...data.map(lag)),
        slow: data.filter(f => lag(f) > 100).map(f => `${f.event} ${f.bytes} B ${lag(f)} ms`),
      };
    },
  };
  return 'helpers';
})()
