# Captured Realtime clients

What stock OpenAI Realtime clients send to a server. Each capture was
recorded on 2026-10-01 against a stub WebSocket server with a dummy key; no
real service was contacted. Each file holds one connection:

- `request_line`, `path`, `query`, `headers`, `offered_subprotocols`
- `frames`: every client frame in arrival order, with base64 audio truncated
  to its length

The files:

| File | Client | Scenario |
|---|---|---|
| `openai_python.json` | `openai` 3.22.1, `client.realtime.connect(model="gpt-realtime")` | text session: `session.update`, user text item, `response.create` |
| `agents_js_normal.json` | `@openai/agents` 0.18.0 (Node), `OpenAIRealtimeWebSocket({url})` | default session config, `sendMessage`, `sendAudio` |
| `agents_js_fc.json` | same | function call; client sends `function_call_output` + follow-up `response.create` |
| `agents_js_barge.json` | same | `speech_started` during playback with `interrupt_response: true` echoed: truncate + retrieve, no cancel |
| `agents_js_barge_interrupt_false.json` | same | same with `interrupt_response: false`: `response.cancel` first |

They back the protocol tests (realtime design §2, §16).
