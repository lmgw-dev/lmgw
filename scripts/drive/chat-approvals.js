// Helpers for scripts/drive/chat-approvals.json (run by scripts/chat-approvals-drive.sh).
// A `script` step defines them on window.__ap; a full reload drops them, so the drive runs the
// step again after each `nav`.
window.__ap = {
  // A thread on the fake model that attaches `notes` with `rule` as its require_approval.
  mk: async (rule) => {
    const t = await (await fetch('/chat/api/threads', {
      method: 'POST', headers: {'content-type': 'application/json'},
      body: JSON.stringify({model_alias: 'fake-echo', kind: 'chat'})})).json();
    if (rule !== undefined) {
      const body = {mcp_tools: [{server_label: 'notes', allowed_tools: null, require_approval: rule}]};
      await fetch('/chat/api/threads/' + t.id + '/settings', {
        method: 'POST', headers: {'content-type': 'application/json'}, body: JSON.stringify(body)});
    }
    localStorage.setItem('apT', t.id);
    return t.id;
  },
  thread: async () => (await fetch('/chat/api/threads/' + localStorage.getItem('apT'))).json(),
  pending: async () => ((await window.__ap.thread()).messages.at(-1) || {}).pending_approvals || [],
  // What a second client does between the page's last read and a click on its button.
  decideElsewhere: async (approve) => {
    const id = localStorage.getItem('apT');
    const ids = (await window.__ap.pending()).map(p => ({approval_request_id: p.approval_request_id, approve}));
    const r = await fetch('/chat/api/threads/' + id + '/approvals', {
      method: 'POST', headers: {'content-type': 'application/json'}, body: JSON.stringify({decisions: ids})});
    await r.text();
    return r.status;
  },
  moveOnElsewhere: async () => {
    const id = localStorage.getItem('apT');
    const r = await fetch('/chat/api/threads/' + id + '/send', {
      method: 'POST', headers: {'content-type': 'application/json'},
      body: JSON.stringify({content: 'never mind'})});
    await r.text();
    return r.status;
  },
  text: () => [...document.querySelectorAll('.msg-assistant')].map(e => e.innerText).join('\n---\n'),
};
'set up'
