// Helpers for scripts/drive/chat-tasks.json (run by scripts/chat-tasks-drive.sh).
// A `script` step defines them on window.__tk; a full reload drops them, so the
// drive runs the step again after each `nav`.
window.__tk = {
  // A thread on the fake model with the `builder` server (mock-mcp.py --tasks)
  // attached, nothing gated (or the tool `gate` names); opened by the caller.
  mk: async (gate) => {
    const t = await (await fetch('/chat/api/threads', {
      method: 'POST', headers: {'content-type': 'application/json'},
      body: JSON.stringify({model_alias: 'fake-echo', kind: 'chat'})})).json();
    const require_approval = gate ? {always: {tool_names: [gate]}} : 'never';
    const body = {mcp_tools: [{server_label: 'builder', allowed_tools: null, require_approval}]};
    await fetch('/chat/api/threads/' + t.id + '/settings', {
      method: 'POST', headers: {'content-type': 'application/json'}, body: JSON.stringify(body)});
    localStorage.setItem('tkT', t.id);
    return t.id;
  },
  thread: async () => (await fetch('/chat/api/threads/' + localStorage.getItem('tkT'))).json(),
  // The thread's jobs as the gateway lists them.
  tasks: async () => (await window.__tk.thread()).tasks || [],
  // What a second window does between the page's last read and a click on
  // Answer: its own answer is under way when the click lands.
  answerElsewhere: async () => {
    const id = localStorage.getItem('tkT');
    const r = await fetch('/chat/api/threads/' + id + '/answer', {
      method: 'POST', headers: {'content-type': 'application/json'}, body: '{}'});
    window.__tkElsewhere = r.text();
    return r.status;
  },
  // The route itself, with nothing left to answer.
  answerStatus: async () => {
    const id = localStorage.getItem('tkT');
    const r = await fetch('/chat/api/threads/' + id + '/answer', {
      method: 'POST', headers: {'content-type': 'application/json'}, body: '{}'});
    return r.status + ' ' + (await r.json()).code;
  },
  // The transcript as shown, `role:id` per message, and as stored.
  shown: () => [...document.querySelectorAll('.msg-item')].map(e =>
    (e.classList.contains('msg-job') ? 'tool' : e.classList.contains('msg-item-user') ? 'user' : 'assistant')
    + ':' + (e.dataset.mid || '?')).join(','),
  stored: async () => (await window.__tk.thread()).messages.map(m => m.role + ':' + m.id).join(','),
  text: () => [...document.querySelectorAll('.msg-assistant')].map(e => e.innerText).join('\n---\n'),
  open: (sel) => { const d = document.querySelector(sel); if (d) { d.open = true; d.scrollIntoView({block: 'center'}); } return !!d; },
};
'set up'
