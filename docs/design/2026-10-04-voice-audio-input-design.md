# Voice turns the model hears: a local audio model gets the spoken turn as audio, ASR beside it

*Superseded 2026-10-06 (the owner's ruling):* not only a local model. Any chat model that takes
audio input hears the turn, wherever it runs; see [Changed 2026-10-06: capability, not
locality](#changed-2026-10-06-capability-not-locality).

Requested by the owner 2026-10-04; this **lean version** was approved the same day. In voice
mode, a thread whose chat model is a local model that takes audio input gets the user's spoken
turn as audio; the ASR transcribes the same audio beside it, for everything that needs text.
*Superseded 2026-10-06:* the model that answers the turn, local or not, hears it when it takes
audio input.
Companion to [2026-10-03-chat-voice-design.md](2026-10-03-chat-voice-design.md) ("chat-voice
§n") and [2026-10-01-realtime-voice-design.md](2026-10-01-realtime-voice-design.md) ("realtime
§n"); it lifts realtime §19's "audio-native chat models" for sessions bound to a chat thread.

**The live probe (2026-10-04)** ran Gemma 4 12B with its audio projector on llama.cpp 0c6a6a7,
with TTS-generated German speech and Parakeet on the CPU as the ASR. The 12B understood every turn
and answered as well as from the transcript. Audio costs 25.0 tokens per second of speech, and MTP
works with it. Time to first token was 93–104 ms from audio against 172–228 ms for ASR plus text
(2–5 s turns); on a 60 s turn the audio prefill took 391 ms and the ASR ~2.1 s. The model's own
transcripts are worse (WER 6.4 % against 4.0 %), and on noise it greets or hallucinates, so it
cannot veto. An earlier turn replayed as transcript cost no time and took 7–10× fewer tokens.

**What this buys, honestly.** The output is held until the turn's transcript is in, so the ASR
stays on the critical path. Short turns get the first token 80–125 ms sooner (probe). A long turn
the VAD commits in segments waits only for its last segment's transcript, so it gains about the
same. An unbroken long turn gains at most what the transcript path does after its transcript
(prefill, first clause, its synthesis), which here runs beside the ASR: under a second, never the
ASR's seconds. The owner chose the lean version (2026-10-04): not a clear win yet, but
expected to become one as omni models spread, locally too. §6 names the upgrade that turns the
ASR's time into gain.

## Decisions (owner, 2026-10-04)

1. **Answer from the audio; hold the output until the turn's transcript lands** (§3.2). The ASR
   transcript stays the text of record and the only veto against noise. *Why:* simple; the gain
   is the small one stated above, and `transcript_wait_ms` shows what the hold costs.
2. **Only turns still being transcribed at launch go as audio** (§3.1). Earlier spoken turns, a
   turn owed again after a cut, and a regenerate use the transcript (no time cost, 7–10× smaller).
3. **No audio is stored** (§4; chat-voice ruling 1). Dictation stays on ASR.
4. **`chat_voice_audio_input`: `off` | `local`, default `off`**, with a thread override (§2).
   *Why off:* llama.cpp marks audio input experimental, and real voices are untested.
   *Superseded 2026-10-06:* the values are `off` | `on`; a stored `local` reads as `on`.
5. **Local means managed by this lmgw**, `vram::classify(&route).is_some()`, for both the verdict
   and the route check (§2.2, §3.4). A llama-server elsewhere on the network counts as remote.
   *Superseded 2026-10-06:* an oversight. Locality plays no role; capability alone decides.
6. **Generic:** any local model whose `input_modalities` include `audio`, not only Gemma.
   *Superseded 2026-10-06:* any chat model, wherever it runs.
7. **A refused audio attempt is retried once with the transcript, with a visible note** (§3.5).
8. **Noise ends quietly; a failed ASR does not stop the reply** (§3.2): all-empty transcripts
   cancel with no error, note or row; after a failed ASR the reply plays and the row says so.
9. **The language sentence stays** (chat-voice §8.5); it fixes the language of the reply.
   *Changed 2026-10-05 (chat-voice §2.1, the split):* it names the reply language as the one to
   answer in and the spoken language as what the user speaks, so a model that hears German audio
   can still be asked to answer in English.
10. **Timing records the path and the hold** (§5); no `audio_tokens` (`prompt_tokens` and
    `audio_ms` are stored already).

## Changed 2026-10-06: capability, not locality

**The owner's ruling (2026-10-06).** A configured fallback is always used, with no exception by
content, and locality plays no role anywhere. The locality guard of this feature (decision 5: the
verdict's locality row, `fit_route`'s `local_only`, the forecast that a candidate alias's walk may
reach a model lmgw does not run) was an oversight. Whether a turn goes as audio is decided by
capability alone: the model that actually answers takes audio input, and lmgw's egress to it can
encode an audio part. Otherwise the turn goes as its transcript, by the path that existed. Keeping a
voice on the machine is the owner's choice of an alias without a fallback, not a rule by content.

What changed:

- **One predicate** (`capabilities/hears.rs`), asked of the model that answers and the route it
  answers on, by the verdict and at the send alike. (1) The egress: the OpenAI-compatible, llama.cpp
  and Gemini egresses encode an audio part; Anthropic's API has none. (2) The model, as `/v1/models`
  publishes it (the owner's override merged in): a chat model (task `chat`; a speech-to-text row
  takes audio too) whose `input_modalities` include `audio`; a candidate a walk picked that is not
  public is read by its row. (3) The server: what a llama-server said in `GET /props`
  (`LlamaFacts.audio`, llama egress design §4.1). No audio projector loaded vetoes; one loaded lifts
  an unknown (a projector lmgw could not read); unknown leaves step 2's answer (egress decision 14).
  At the send the facts are the claimed container's, in the verdict those of the container up now
  or an external row's cache.
- **Unknown is no** (decision D1), at the verdict and at the send: a server that drops a content
  part it does not know answers an empty turn. Where the page or the self-admin text explains why a
  model reads the transcript, it says how to make it hear: list `audio` in its capabilities
  override (Gemini's and OpenAI's own catalogs publish no modalities).
- **The verdict** (§2.2) has no locality row and no forecast of a candidate walk's fallback (with the
  Audio facet on, a fallback that does not take audio counts as none, so every model the walk can
  reach hears). *Changed 2026-10-06* (candidate-aliases §4.6): such a fallback is used; a turn the
  walk hands to it goes as its transcript, and under the hold the verdict says so. Every request
  whose audio went as its transcript because the model lacks it (the verdict at launch, a retry
  after a refusal of the audio, a tool-loop call) carries "'m' lacks audio: transcript sent" on
  its row (`request_logs.degraded`). Under a swap at resolve (the hold, a benchmark's lease) only the fallback is judged,
  without the candidate walk or its guards, and the swap's words come along (`lead`). With no block
  live, the verdict a block would bring is computed beside (`blocked`, decision D3): the fallback's
  own, or the refusal when there is none. The page shows it while it sees a block, and says when a
  fallback hears ("under the GPU hold your voice goes to … as audio").
- **The send** (§3.4): `fit_route`'s `local_only` is `spoken::may_hear`, the predicate on the model
  that answers (`answered_by`, else the thread's model) and the route about to be sent, with its
  claim's `/props`. A model that cannot take the audio, or that lmgw cannot judge, is refused with
  `audio_input_unsupported` (400) before anything is sent; a bound session's turn goes again as its
  transcript, with the predicate's sentence as the note, and it is not remembered (§3.5: the next
  verdict judges that route itself). `fit_route` and `PerRoute::request` are async.
- **The crash memory** (§3.5, decision D5): a dropped connection or a failed restart under the audio
  is remembered only on a route to a llama-server (`UpstreamKind::LlamaServer`,
  `SentAs::llama_server`). llama-server aborts on an audio part its projector cannot take, so a drop
  there is evidence about the audio. Through a cloud API, a proxy or any other server it is a network
  or provider fault that says nothing about the audio, and the model hears the next turn again. The
  rule is about the quality of the evidence, not about where the model runs.
- **The value** (decision D2): `local` is `on`. Strict on input (`local` is refused, naming `off,
  on`); a stored `local`, in the settings blob, a thread's voice or a folder's default, reads as
  `on`. No migration: the next settings save stores the new name.
- **Live checks** still call no cloud model: `scripts/voice-audio-in-check.py` refuses to run when
  the thread's verdict names another model than the one under test (a fallback would answer), and
  fails a run a fallback answered. That is the test harness's conduct, not product behaviour.

Everything not about locality stays: the transcript as the text of record, the hold until it has
words, the noise veto, nothing stored, tools waiting for the user row, the GPU claim let go while
waiting, the context guard, the refusal retry and its memory, and `off` as today's bytes. Other
guards that skip a configured fallback by content are outside this change.

*Changed 2026-10-06, after the review (V1–V16):*
- **A tool loop's later call** to a model that cannot take the audio — a candidate re-pick, a
  fallback — goes with the user row's words in place of the spoken parts once the journal settled
  the row (`agentchat::unheard`); before, it failed after its tool frames. An Anthropic alias
  fallback counts as lacking the Audio facet, so the walk and the predicate agree.
- **A claim let go while tools wait** and refused again by a GPU block (the hold switched on
  meanwhile) no longer fails the turn: that call goes where the gate sends any request under the
  block, its configured fallback (`agentchat::claim::under_block`).
- **A dropped connection on a route that is no llama-server**, under the audio and before an
  answer, retries that one turn with the transcript and is not remembered (`refusal::dropped`):
  a provider that closes the connection on large bodies no longer fails every turn. D5 stays for
  llama-server.
- **A candidate's pick** is judged by its row through the route (`hears::Model::Pick`), never by
  its public name, which an alias of the same name would shadow.
- **The verdict never waits on a catalog** (`catalog::cached_only`): a cold one is read in the
  background and the verdict says "lmgw is still reading the model list … is in" until it is in.
  The send reads it in full, raced against the turn's stop, and stays the authority.
- **The hint** for a model lmgw cannot judge says what the owner does where it lives: an alias's
  or a row's capabilities override with task `chat` and input modalities `text` and `audio`, and
  an alias for a passthrough model, which has no override of its own.
- **Attachments** decide native audio by the same predicate (`hears::hears_from`): an Anthropic
  alias or a server with no audio projector gets the transcript at upload. The Anthropic egress's
  own refusal of an audio part is classified as lmgw's (`Refusal::Unheard`).
- **Voice-library clips** name the model that wrote their transcript, a fallback as one
  (`transcript_source: asr:<fallback>`, `answered_by`, the op's and the toast's words).
- **The live check** runs only on a model with no fallback at all.
- **Known, accepted (V10):** under the GPU hold, a fallback that hears answers a turn whose own
  speech recognition is a GPU row with no fallback, which the same hold refuses. Every such turn
  then ends as a failed transcription the model heard: the reply plays, no tool runs, the row is
  "[spoken turn, not transcribed]", and noise is answered rather than vetoed. On the transcript
  path the same turn fails visibly with `transcription_failed`. A speech-to-text row on the CPU
  is not affected; give a GPU speech-to-text row a fallback (or move it to the
  CPU) to keep the veto under the hold.

## Ground rules, and what exists today

chat-voice's ground rules hold. **New child modules:** `realtime/lifecycle/held.rs` (the hold;
`realtime/audio_in.rs` is taken), `realtime/thread/turn/audio.rs` (attempt and retry; `turn.rs`
is 566 lines), `web/chat_voice/audio_input.rs` (the verdict), `web/chat_turn/spoken.rs` (spoken
parts, barrier, local-only predicate; *superseded 2026-10-06:* the capability check
`may_hear`); the deferred row is in `thread/journal/user.rs`. Other files
gain call sites only. **No hidden limits:** the audio goes as long as the turn was; a refused one is
retried as transcript, with the note. **`off` is today:** no event, no timing field, the same
bytes. **Privacy:** only TTS-generated speech and synthetic noise; live checks run on a dev copy
(`scripts/dev-copy.sh`), call no cloud model, and never flip the GPU hold.

**Today** (lines at `main` 5ad9ee5): every response waits for its transcripts
(`realtime/lifecycle.rs:301`, `lifecycle/pending.rs:203`). The journal writes the user row before
the turn begins, as a history write (`thread/journal/user.rs:20-62`). A reply is saved only while
the thread's generation is still its turn's (`web/chat_live.rs:281`). IR audio (`ir.rs:38`)
reaches llama-server (`egress/openai.rs:129`), and attachments already send WAV natively
(`web/chat_attach_gate.rs:32-40`): attaching is an explicit act, speaking is not.

## 1. Overview

At commit the WAV is built once (ASR and request) and the response launches; the chat model
hears the turn, its output **held**. When the transcript lands, the core releases or vetoes, and
the journal writes the user row, after which the reply is saved.

## 2. The setting and the verdict

### 2.1 The setting

`chat_voice_audio_input` (`off` | `local`, **default `off`**) follows `chat_voice_language`'s path
(which `chat_voice_reply_language` took too on 2026-10-05):
`lmgw-api-types/src/settings.rs:51`, `config/settings.rs:287`/`:459`,
`ops/chat_voice_settings.rs:22`/`:66`/`:106`, `ops/settings_patch.rs:64`/`:226`,
`ops/reads.rs:631`, `web/api_settings.rs:198`/`:424`/`:785`,
`mcp/selfadmin/catalog/runtime.rs:198`, UI `pages/settings/chat_voice.rs:78`. The save check
takes the two values. The MCP text says "experimental; local models only, a cloud chat model never
gets audio" (*changed, WP1 review m3:* it and the Settings hint add that the speech-to-text model
transcribes every turn either way, so a cloud speech-to-text alias still receives the audio).
*Superseded 2026-10-06:* the MCP text and the Settings hint say the model that answers hears the
turn when it takes audio, that a configured fallback is always used wherever it runs, and how to
make a model lmgw cannot judge hear.
`ThreadVoice.audio_input` sits next to `language` (`store/chat_voice.rs:84`): strict on
input, tolerant on read, carried into folder defaults by `ThreadDefaults.voice`.

### 2.2 The verdict: does this turn go as audio?

`web/chat_voice/audio_input.rs::verdict(state, thread) -> AudioInput {path, model, why}` is async.
Every row must pass; the first that fails gives the `why`, and the path `transcript`:

| Check | `why` when it fails |
|---|---|
| the setting resolves to `local` (thread, then Settings) | "audio input is off (this thread / Settings → Chat → Voice)" |
| the thread's knowledge bases are not in auto mode | "its knowledge bases search with your words (auto mode)" |
| (*added, WP3 review #2*) its speech-to-text alias is set and resolves | "no speech recognition is set up, and only a transcript tells your words from noise" |
| `gate::resolve(alias, RouteCheck::None)` (`gate/open.rs:477`) settles a route: hold and benchmark swaps, a candidate alias's fallback under the hold (`:488-511`, `:586`); it starts nothing | the resolve's own error |
| that route is **managed by this lmgw**: `vram::classify(route).is_some()` (`vram/routing_target.rs:26-38`). *Superseded 2026-10-06: row removed* | "openai/… is a model lmgw does not run: your voice stays on this machine"; with a fallback, "under the GPU hold this goes to <fallback>" |
| no context guard: not a ladder row (`LocalModel::is_ladder`, `ladder.rs:55`) or guarded pool (`pool_guarded`, `config/llama_params.rs:492`); for a candidate alias, none of its candidates | "<model> guards its context, which cannot bound audio" |
| `input_modalities` include `audio` (`model_caps`). *Changed 2026-10-06:* the model that answers hears (`capabilities::hears`): an egress with an audio part, a chat model whose `input_modalities` include `audio`, a server whose `/props` did not say it loaded no audio projector | "<model> does not take audio input" / "lmgw cannot tell whether <model> takes audio" (*2026-10-06:* ": if it does, list audio in its capabilities override"), "<model> is served over the Anthropic API, which has no audio input part", "<model> is not a chat model", "<model>'s server loaded no audio projector"; after a swap "under the GPU hold this goes to <fallback>, which …" |
| (session only) its server has not refused audio this session (§3.5) | "it refused the audio this session: …" |

Unknown is no. `Snapshot::is_local_upstream` (`config/snapshot.rs:171-184`) is not used: it is
true for any llama-server-kind upstream wherever it runs, which is right for pricing and wrong
here. Under the hold a candidate alias is judged by its fallback, so the chip never says "hears
you" while a cloud model would answer. *Superseded 2026-10-06:* nothing here asks where a model
runs. Under the hold the fallback is judged by capability alone, and alone: the chip says "hears
you" when it takes audio, wherever it runs. The verdict is computed for the thread JSON
(`speech::resolve_shown`, `web/chat_voice/speech/plan.rs:150`), at the bind, and at every
`speech_started` beside the warm's thread re-read (`realtime/input.rs:355-382`). That last one
arrives by a new session-loop arm (`realtime/session.rs:131-137`). The core keeps the result as
`Bound.audio_input` (`realtime/thread.rs:68`), with the refusal memory on top; a commit reads it.

### 2.3 Shown

`VoiceConfig` gains `audio_input {value, source, path, model, why}`. The thread's Voice section
(`pages/chat_voice/section.rs`) shows the select, its source, and "your voice goes to gemma4-12b
as audio" or "gemma4-12b gets the transcript: <why>". Settings → Chat → Voice says "experimental;
local models only" (*superseded 2026-10-06:* it says a configured fallback is always used, and
hears the turn when it takes audio). The chip (`pages/chat_voice/realtime/chips.rs`) says "hears you" or "reads the
transcript" (tooltip: `why`), from the thread JSON and then each `lmgw.chat.input`.

*Built (WP1):*
- **Types.** `store::AudioInputMode` (`off` | `local`, the dashboard's `AUDIO_INPUTS` kept in step by a
  test; *superseded 2026-10-06:* `off` | `on`) and `store::InputPath` (`audio` | `transcript`), which `VoiceTiming.input` and
  `MessageVoice.input` use too. Every new stored field is skipped when absent, so `off` stores
  today's bytes; a value a newer build wrote is dropped alone.
- **The verdict** is `web/chat_voice/audio_input.rs`: `setting`, `thread_rows` (the setting and the
  auto-mode row, which §3.5's responder re-reads), `verdict` and `shown`. `VoiceConfig.audio_input`
  is an `Option`, filled only by `resolve_shown`, since the sync `resolve` cannot judge a route.
  `model` is the thread's alias, or the fallback a block hands the turn to.
- **One addition to the locality row:** the route must be a *chat* model lmgw runs
  (`Class::Chat`); an aux, audio or image row says "<model> is not a chat model". *Superseded
  2026-10-06:* the predicate checks the task (`chat`) of whatever answers.
- **Wording.** A llama-server lmgw does not run: "<model> is served by a server this lmgw does not
  run: your voice goes only to models lmgw runs" (`is_local_upstream` picks only these words). With a
  fallback: "under the GPU hold this goes to openai/…, a model lmgw does not run: your voice stays on
  this machine" ("while a benchmark run holds the GPU …" under a lease). *Superseded 2026-10-06:* no
  locality wording; "under the GPU hold this goes to openai/…, which does not take audio input".
- **The page.** The drawer's "Audio input in voice mode" select (inherit / off / local, a folder's
  defaults too) shows "in effect: local · Settings → Chat" and the verdict line (*2026-10-06:*
  inherit / off / on). The chip is a chip
  of its own, `INPUT`, after the model chip.
- **Docs.** The `/v1/realtime` DocRoute lists the three timing fields; the event and
  `lmgw.chat.user`'s `response_id` are listed since WP3 sends them.

*Changed (WP1 review fixes):*
- **A candidate alias's walk to its fallback by design** (M1). `gate::resolve` foresees only the
  hold's and a benchmark's swaps. The verdict also reads the gate's own pick
  (`candidates::derive::cached_pick`, `audio_input/candidate.rs`): a background alias (its walk
  takes the alias fallback whenever its primary is not loaded) and an alias whose primary it cannot
  use (`primary_skipped`; the fallback answers whenever no other candidate is loaded) get the
  transcript when the walk has a usable fallback — "as background traffic this may go to openai/…,
  a model lmgw does not run: …", "its primary 'x' is disabled, so this may go to …". The module doc
  no longer claims more: admission's outside-VRAM swap, a climb, the walk over loaded candidates and
  a hold switched on after the verdict stay unforeseen, and `fit_route`'s `local_only` stays the only
  guard (§3.4). *Superseded 2026-10-06:* removed. With the Audio facet on, the walk's fallback takes
  audio or counts as none, so the alias hears either way; `fit_route` asks the predicate of
  whatever answers. (Later on 2026-10-06 the fallback stopped counting as none: one that cannot
  hear gets the transcript.)
- **Only routable candidates guard** (m1): a disabled or missing guarded row says nothing about the
  alias. The guard names the row by its public name (m6).
- **One snapshot for the capability** (m2): the lookup is `capabilities::exposed::exposed_entry` for
  the name the resolve settled on, not a second resolve.
- **Wording** (m3): a route that is not a llama-server-kind upstream is "a model lmgw does not run"
  (a LAN OpenAI-compatible server is no cloud model either). *Superseded 2026-10-06:* gone with the
  locality row.
- **Speech recognition is a row** (WP3 review #2): only the transcript can veto noise, so a thread
  whose speech-to-text alias is unset at every level, or does not resolve, reads the transcript, and
  its turns fail to transcribe as with the setting off (`asr_not_configured`). The row is the
  verdict's only, not one the responder re-reads (§3.5): a chip cleared after the commit fails that
  turn's transcription, which then is not heard (§3.2).
- **The page follows the live block** (M2): the INPUT chip and the drawer's verdict line read the
  titlebar's block (`pv.block`, as the STT and TTS chips do). Under the hold or a benchmark run an
  audio verdict shows "reads the transcript", amber: "the next voice turn: under the GPU hold it goes
  to <model>'s fallback, or is refused, as its transcript: your voice goes only to models lmgw
  runs". The drawer's "in effect" line shows the value's label (m6). *Superseded 2026-10-06
  (decision D3):* under a live block the page shows the gateway's forecast (`blocked`), the
  fallback's own verdict: amber "hears you" and "under the GPU hold your voice goes to <fallback> as
  audio", or "… this goes to <fallback>, which does not take audio input", or "… <model> has no
  fallback, so the turn is refused".

## 3. A turn the model hears, end to end

### 3.1 Commit and launch (core)

An **audio turn** is one committed while `Bound.audio_input.path` is `audio`. The core keeps it in
`Bound.hearing` until its transcript is in; that map (item id → WAV handle, and its response once
launched) is the one fact the core reads.
- **The WAV is built once.** At commit `wav_16k` runs on the blocking pool into a shared handle
  (`futures::future::Shared`). The ASR job carries it; `call` (`transcribe.rs:423`) and the word
  check's second call await it instead of building one (`:432`).
- **A turn has words until it is transcribed.** `input::has_words` takes the `hearing` set and
  counts a turn in it as having words. Its callers pass the set: `Core::has_words`
  (`pending.rs:273`), `bound_empty` (`lifecycle/bound.rs:143-153`), `transcripts_failed`
  (`input/transcript.rs:28-46`), and the post-transcript check (`:148`), which removes it first.
- **Decided at commit.** `pending_commit` (`pending.rs:86`) runs `pending_decide` (`:278`) at once
  for an audio turn. `pending_resolve` (`:203`) re-decides a `due` debt on an audio transcript, so
  a debt whose turns all came back empty is dropped as noise is today (`:284-303`).
- **A launch-only busy.** `Transcriber::busy_for_launch`, beside `busy` (`transcribe.rs:217`),
  ignores turns in `hearing`. `start_response` (`lifecycle.rs:301`), the launch after a transcript
  (`input/transcript.rs:152`) and `bound_refusal` (`lifecycle/bound.rs:167`) use it, and
  `awaited` (`lifecycle.rs:287-292`) leaves audio turns out. `end_bound_turns` (`bound.rs:100`)
  keeps `busy()`, so it still waits for every turn. Push-to-talk takes the same path.
- **Handed over at launch.** `launch_bound` (`lifecycle/bound.rs:173`) splits the new turns
  (`unwritten_turns`, `:33`) into audio turns still in `hearing` and the rest, which are already
  transcribed. Without an audio turn, the launch is today's. Otherwise the responder gets the
  spoken parts (§3.4) and the journal the turns marked deferred (§3.3). A turn transcribed before
  its response launched goes as its transcript, and its handle is dropped.
- **A pause in mid-sentence.** `pending_defer` (`pending.rs:99`) still defers a debt not started,
  but this response started at the commit. The resumed speech meets a response nobody heard, since
  nothing plays under the hold, so `interrupt` (`lifecycle/interrupt.rs:188-209`) cuts it and
  owes its turns again (`pending_reowe`, `:114`). The next response answers both, as a deferred
  one does today; the first turn replays as the cut response's row (§3.3). The cost is a prefill
  and a `canceled` row. A pause window before every launch was rejected: every turn would pay it.

*Built (WP3):*
- **The verdict in the core.** `Bound.audio_input` is the `Shown` (setting and verdict) judged at the
  bind (`thread/bind.rs`) and again off the core by `thread/verdict.rs`: at every `speech_started`
  and, added, after every bound response ends, so push-to-talk (no `speech_started`) follows a chip
  changed mid-session. The judgement returns by a session-loop arm; an older one is not taken. A
  commit reads it with `Bound.refused` on top (`after_refusal`).
- **The hearing** is `thread/hearing.rs` (`Hearing`: item id → WAV and its response; per launched
  response its new turns, answers and audio turns). The core's half is `lifecycle/hearing.rs`:
  `commit_upload` at the commit, `launch_heard` at the launch, `heard_settled` on the last audio
  transcript. The WAV is `transcribe::Wav`, a `Shared` future started on the blocking pool at the
  commit; the ASR job's `Upload` is the segment or that WAV.
- `has_words(item, hearing)` and `transcripts_failed(conv, awaited, hearing)` take the hearing;
  `pending_commit(.., heard)` decides at once and `pending_resolve(.., heard)` re-decides a due debt.
  A turn transcribed before its launch leaves the hearing with its transcript and goes as text.
- **A mid-utterance pause** cuts the held response before its request may even have left (its
  responder still waits at its barrier): then only the next response calls the model.

*Changed (WP3 review fixes):*
- **Heard means carried** (#3). Handed to a response is not heard: the responder says whether its
  attempt carried the audio to the model (`Msg::Carried`, and `In::Began` to the journal): `true` at
  the model's first frame (delta, reasoning, tool, usage, stop), `false` once its frames ended, it
  was refused, the response was stopped first, or the attempt was skipped. The core keeps it on the
  launched response (`Launched.carried`).
- **A pause in mid-sentence cuts the held response whatever `interrupt_response` says** (#6):
  nobody heard it, and with the client's `interrupt_response: false` it would answer half the
  sentence beside the next response (`input/judged.rs`, `Core::listen`'s `cuts`).

### 3.2 The hold, the veto and a failed ASR (`lifecycle/held.rs`)

**The hold.** A response with audio turns starts held. `on_responder` (`lifecycle/ending.rs:35`)
queues its output in arrival order: chat frames other than `state`, deltas, clauses, `Unspoken`
and `Finished`. Neither a reply nor its end can reach the client before the verdict. `Mark`,
`Planned`, `Tts`, `Voices` and `state` frames pass through, so timings and model states stay live,
and a held message records its arrival marks (§5). Generation, clause cutting and synthesis go on;
the writer's progress does not move, so synthesis stops at its lead. The hold is why an unbroken
long turn gains little (intro); `transcript_wait_ms` records it per response.

**The verdict** is settled in `on_transcript` (`input/transcript.rs:94`) once the response's last
audio turn is transcribed:
- **Released** when any turn it answers has words, or an audio turn's ASR failed. The queue is
  replayed in order through the normal path, then output flows live.
- **Vetoed**, quietly, when every turn it answers was transcribed with no words. The queue is
  dropped, the call stopped, and the response ends as a cancel does (`lifecycle/cancel.rs:70`,
  `response.done {cancelled}`), with no `error` and no note. The journal is told (§3.3), so no row
  and no reply are written. *Cost:* a VAD false trigger now calls the chat model until the veto: a
  prefill, a few tokens, a `canceled` request row.
- **ASR failed** (no words, at least one failure): released. The model heard the turn, and waiting
  on a failed ASR is exactly what this feature avoids. The failure is logged at INFO and shown on
  the user row. It is not sent as `transcription.failed` or `error`: today
  `input/transcript.rs:117-133` sends one of them, and the page's `heard_failed` would then say no
  reply comes. *Cost:* a noise turn whose ASR failed reaches the model unvetoed.
- **A barge-in during the hold** cuts a response nobody heard; the queue goes with it. The
  barge-in word check, backchannels and script checks stay on ASR.

*Built (WP3):*
- The veto's `response.done` reason is **`no_words`**. A response whose hold ended with nothing
  released (a veto, a cut, a `response.cancel`) is kept in `Bound.unreleased`, and its late chat
  frames are dropped rather than relayed, so the page never makes a bubble for a reply nobody heard.
  A vetoed slot sends no `lmgw.response.timing` either.
- **Changed: a heard reply's last clause is cut at the `stop` frame.** A heard turn says `done` only
  after its pre-save barrier, that is after the transcript; the clause in progress was flushed at
  `done`, so a one-sentence reply was synthesized only after the release. It is flushed at the end
  of generation instead, so it is synthesized during the hold (heard responses only; `off` is
  unchanged).
- **Review fix: the plain path lets go of its GPU claim before `persist`** (the tool loop already
  did). A heard reply waits there for its row, which waits for an ASR call that may need the VRAM
  the claim pinned: a circular wait. Nothing in `persist` needs the GPU. *Changed (verification
  review):* the tool loop's own wait for the row, before its first tool call, had the same circle;
  it now lets its claim go there too (§3.4).

*Changed (WP3 review fixes):*
- **A failed ASR stays quiet only for a turn a model heard** (#2, #3): its response is active and
  its attempt carried the audio. Any other failure is sent as `transcription.failed` or `error` as
  with the setting off — a turn whose response was cut, skipped or refused, or whose model had not
  answered yet (an ASR failing fast, a model still loading), and a failure that is no transcription
  at all (`input::attempted`: `asr_not_configured`, the key's policy, the session's stop). The
  response, with no other words to answer, is vetoed with reason **`transcription_failed`**
  (`no_words` stays the reason for noise). Only a heard failure keeps its error (`Bound.asr_errors`)
  and counts at the verdict: a response's own audio turn when its attempt carried it, and one owed
  again after a cut whose response's attempt did (its row says "not transcribed", and the next
  request carries the placeholder). A turn owed again whose cut response carried nothing was never
  heard: its failure was said, and it does not keep the next response from its veto.
- **The usage passes the hold** (#5): a veto's or a cut's `response.done` carries the usage known.
- **A response whose hold released nothing** (`Bound.unreleased`) drops everything it still says,
  its notes (`lmgw.chat.input`) and refusal memory too (#9); only the voice list, the marks and
  `Msg::Carried` pass. It is forgotten at its responder's last word (`Finished`), or at once when
  that was already held (#11). `asr_errors` are dropped at the verdict for turns nobody heard, and
  once a response that answers them has played.
- **Cost:** a CPU ASR that fails fast now ends the turn as with the setting off — the reply plays
  "not transcribed" only when the model answered first.

### 3.3 The user row (journal)

**The barrier stays, as an empty op.** `In::Response` (`journal.rs:89`) carries the turns, audio
turns marked. Its `Op::User` answers the responder's barrier with no id once every earlier entry
is done (`journal.rs:351-356`), so turn N+1 still begins after reply N is finalized. **Two new
inputs:** `In::Began {gen, generation}` from the responder once `start_turn` returned (the
generation from `TurnOpts::began`, `chat_turn.rs:424-425`). `In::Heard {gen, turns, veto}` from
the core when response `gen`'s last audio turn is transcribed (`on_transcript`, or, as a session
ends, `on_last_transcript`, `input/transcript.rs:163`), with texts, ASR facts and errors.

**The row is written when the slot is at the head, its `Heard` is in, and its `Began` (or a
`Saved` from a turn that never began)**, before the slot waits for its reply (`ready`,
`journal.rs:338`, gains this first phase). Content is the turns' words.
`voice` is as today (`journal/user.rs:90`), plus `input: "audio"` and, for a failed ASR,
`transcript_error`. The write is `append_spoken_user`, an insert that moves no generation, as
`set_message_voice` (`chat_repo.rs:435`) is an update that moves none. It runs under `write_if`
with the generation the turn began at, so the reply being generated can still be saved after it.
If the history moved meanwhile (another window's turn, an edit), or the turn never began, the row
is written as today (`bound::write_user`, a history write); a superseded reply is not saved
anyway. No new words and no failure: no row. The journal then answers the **pre-save barrier**
(§3.4): the row's id, `failed`, none, or `veto`.

`lmgw.chat.user` goes out once, as today, and gains `response_id`. The release and the row's
write race by one store write, so the page puts the user bubble before that response's reply. A
row that names an untitled thread (as `write_user` does, `web/chat_voice/bound.rs:177-180`) sends
`lmgw.chat.thread`. The session loop's journal arm (`session.rs:136`) runs it through
`bound_planned`'s compare-and-send (`thread/hooks.rs:105-116`), so `session.lmgw.resolved`
follows. **A veto** writes no row and saves no reply. The slot finalizes with nothing saved
(`journal/finalize.rs:30-32`): no `{removed}`, no delete. **The order is unchanged:** user N, row N
once heard, reply N saved, N finalized, user N+1. A turn owed again after a cut belongs to the cut
response's entry, and its row is written before the next barrier passes.

*Built (WP3):*
- The row phase is `thread/journal/row.rs`; `bound::append_spoken_user` (`ChatRepo::
  append_spoken_user`) is the insert under `write_if`.
- **Review fix: `In::Began {gen, generation: Option<u64>}`.** A skipped attempt (a thread row of
  the verdict fails by now, or the turn's WAV could not be built) sends `generation: None` before it
  waits for the row, which is then a history write; without it the responder and the journal waited
  on each other. The attempt sends `Some` only after `start_turn` returned, when the history is read,
  so the row never lands in its own request beside its audio.
- A row the store refused keeps its words for the next user entry (as today) and drops the barrier:
  the reply is not saved. The ASR's error is kept per turn (`Bound.asr_errors`) and becomes
  `transcript_error` only for a turn the model heard: a turn that went as its (empty) text is not
  "heard and not transcribed".
- `lmgw.chat.user.response_id` is sent on a heard response's row only, so `off` sends today's
  events byte for byte; the page orders by it only where the race exists. The title event goes from
  the journal through the session's compare (`Core::journal_event`) for heard rows, whichever way
  the row was written (the turn's own re-read came before the row).
- **Changed (WP3 review #3): `In::Began {gen, generation, carried}`** is the attempt's fate,
  sent when it is known (§3.1's note), so the row waits for it: a turn is written heard (`input:
  audio`, and `transcript_error` for a failed transcription) only when the attempt carried the
  audio. A skipped or refused attempt's row is a transcript turn's (no `input`), and a failed
  transcription nobody heard writes no row. A reply with nothing in it is saved by no one, so its
  turn never waits at the barrier for a row that waits for the attempt's fate.
- **Changed (WP3 review #8): a row the store refused** (or a thread gone) answers the barrier
  `unwritten`, not a dropped sender: the reply is not saved, `persist` says why (WARN: "the spoken
  turn's user message could not be written"), and a retry or skipped attempt waiting for the row
  ends with `chat_history_write_failed`. A journal that ended without answering says so as such.
- **Editing a user row clears `transcript_error`** when the text changes: the resend rewrite already
  drops the whole `voice` then (the text is no longer what was spoken, chat-voice §3), and the row is
  a typed turn from there on, with no placeholder and no "not transcribed" note. An edit that keeps
  the text keeps it.

### 3.4 The request

`TurnOpts` (`web/chat_turn/out.rs:71`) gains `spoken: Option<Vec<ContentPart>>`: the response's
new turns, not yet in the history, in order. An audio turn is `ContentPart::Audio {mime:
"audio/wav", data}`; the others are their transcript text. It also gains `user_row`, the pre-save
barrier, a `watch` the responder reads too. `build_messages` (`chat_turn.rs:891`) appends `spoken`
as a user message after the history. Merging (`chat_turn/merge.rs`) joins it with a trailing user
row, so an owed-again turn (as text) and the new audio go as one message. Other rows go as stored;
a row with `transcript_error` and no text goes as `[spoken turn, not transcribed]`. The audio has
no text part beside it: none exists yet, and the probe answered from audio alone. `Turn::persist`
(`chat_turn.rs:229`) awaits `user_row` after its nothing-to-save return (`:231`) and before
`save_lock`, so the reply row follows the user row; on `veto` or a lost sender nothing is saved.

**Local is enforced where the bytes are routed.** A turn whose `spoken` carries audio is
`local_only`. `fit_route` (`chat_turn.rs:794`) refuses a route `vram::classify` does not claim, with
`audio_not_local` (a 4xx; nothing sent). It already runs on every route a request goes to: the
admitted one, and every re-route before anything is sent. That covers the plain stream
(`web/chat.rs:1101`) and the tool loop (`web/agentchat.rs:191`, `:531`); a continue's prefill is
refused there for the same reason. *Superseded 2026-10-06:* **capability is enforced where the
bytes are routed.** `fit_route` asks `spoken::may_hear` of the model that answers on the route
(the predicate), and refuses one that cannot take the audio with `audio_input_unsupported`, on the
same routes.

*Why not a `RouteCheck`:* after `resolve`, the gate also swaps at admission's outside-VRAM verdict
(`gate/open.rs:690-700`), in a climb under the hold (`vram/climb.rs:705-750`) and in the candidate
walk (`gate/candidate/walk.rs:538-566`, `:606-634`). Each judges the site's check through
`fallback_serves` (`open.rs:835`), where a failing check means "no fallback". Admission would then
queue into `vram_queue_timeout`, and the climb and walk would refuse with `gpu_hold`, stranding the
audio turn on an error the retry does not take. With `fit_route` the gate decides as it would for
the transcript, the swapped route is refused before a byte leaves, and the retry takes the same
swap with text. An attachment's audio is not `local_only`. (*2026-10-06:* this holds for the
capability check too, with one more reason: a failing `RouteCheck` would keep the configured
fallback from being used at all, which the owner's ruling forbids. An attachment's audio has its
own capability check, `chat_attach_gate`.)

**Tool threads** send `spoken` with each model call. The stored record starts after the request's
messages (`agentchat.rs:604-613`), so audio never reaches `ir_messages`. **Regenerate** replays the
stored row. On a reply with `timing.input: audio` its tooltip says "answers from the transcript:
the audio is not kept".

*Built (WP2):*
- **`web/chat_turn/spoken.rs`** holds the turn's half: `hears` (the parts carry audio), `local_only`
  (*2026-10-06:* `may_hear`), the placeholder (`row_text`, user rows only) and the barrier. `user_row` is a `watch` of
  `Option<UserRow>`: `Written(id)`, `Failed` (the row has `transcript_error`), `NoRow`, `Veto`.
- **`fit_route(route, ir, (continuing, local_only))`**; a turn is `local_only` when its spoken parts
  carry audio. The refusal is a 400 `invalid_request_error`, code `audio_not_local`, naming the model
  and its upstream; the plain path writes its request row as for any refusal there. *Superseded
  2026-10-06:* `fit_route(state, (route, hold), answering, ir, (continuing, hears))`, async; code
  `audio_input_unsupported`, the predicate's sentence, the upstream, "so nothing was sent".
- **The barrier** waits after the nothing-to-save return. A turn superseded during the wait goes on
  to its own save check, which refuses it. A veto saves nothing quietly (`done {saved: false}`, no
  `error`); a journal gone saves nothing and says `not_saved`. The turn drops its spoken parts once
  the request is built.
- **Tests.** The `off` golden is `tests/fixtures/chat_requests/` (`seam_tests/spoken.rs`), captured
  at 1e1475f before the request changed. A route only lmgw runs can take the audio (*superseded
  2026-10-06:* any route whose model takes it), so the request
  shape, the tool loop, the swap at admission (§4.7's outside-VRAM verdict to a cloud fallback) and a
  server's refusal run in `tests/it/chat_voice_audio_in/turns.rs` on `gpu_world`'s local rows,
  through two doc-hidden seams like the other `*_for_tests`: `web::spoken_turn_for_tests` (the turn)
  and `realtime::heard_response_for_tests` (the bound responder). The hold's swap at resolve is a
  seam test. A climb's and the candidate walk's swaps pass the same `fit_route` call on every route
  the gate opens, and have no test of their own.

*Changed (WP3 review #1): a heard turn's tools wait for its user row.* On noise the model greets
or hallucinates before the transcript can veto, so the tool loop's executor
(`web/agentchat/heard.rs`) waits for the pre-save barrier before the loop's first tool call —
self-admin `lmgw__*`, MCP and knowledge-base tools alike (`spoken::tools_may_run`). A row written,
or none to write, runs them; a veto, a row not written or a journal gone runs none: the loop is
stopped, each call's result says "not run" and why, and nothing is saved. The response's stop (a
cut) ends the wait as it ends any tool call.

*Changed (verification review, the owner's decision): a failed transcription runs no tool.* A
reply that plays is heard by the user; a tool run is a side effect nobody confirmed had words. On
`failed` every call's result is "not run: the transcript failed, so lmgw could not confirm what
was said", and the model's reply goes on and is saved as before (`ToolsHeld::Failed`, the one
answer that does not stop the loop). A row the store refused although it had words says so in its
own words ("…could not be stored, so this reply is not kept"), a veto "…had no words", a journal
gone "…the voice session ended before it stored…".

*Known, accepted: a pause in mid-sentence may run a tool on the first half.* The first half's row is
written once its transcript has words, and the gate lets the call run then — about when `off` would
have started the model on that half anyway. The resumed speech still cuts the response, and the
next one answers both halves, but a call made before the cut has happened. No code guards it.

Tested with a stub MCP server and the self-admin plane (`tests/it/chat_voice_audio_in/tools.rs`).

*Changed (verification review): the loop lets its GPU claim go while the gate waits.* WP3 review
#1 kept the claim through the wait and called the cost "an ASR that needs that VRAM ends at
`vram_queue_timeout`, never a deadlock". That was wrong: with `vram.queue_timeout_seconds` 0 ("waits
indefinitely") a GPU ASR that is not loaded and does not fit beside the chat model waits for the
loop's claim, which waits for the row, which waits for the ASR — the turn hung for good, held and
silent; with a timeout it became a heard failure. Now the first tool call lets the claim go before
it waits (`web/agentchat/claim.rs`, `LocalHold::let_go`), as the plain path drops its claim before
its barrier, and the loop's next model call takes it again through admission on the same model
(`vram::LetGo::regain`: a model still up is joined at once; one the ASR evicted is started again;
the hold's policy, origin and restart rule are kept, so a ladder's climb and a candidate's walk go
on as before, and a guest stays a guest). A regain that admission refuses (the GPU hold came on, a queue
timeout) is the next model call's error and writes its request row. The tools themselves run
unclaimed until that call; a vetoed turn takes nothing again. `LocalHold`'s doc no longer says
"never a deadlock": a holder must never wait on work that needs the room its hold pins. Tested on
`gpu_world` (`tests/it/chat_voice_audio_in/tool_claim.rs`): a 6 GiB GPU ASR beside an 8 GiB chat
model on a 10 GiB card, queue timeout 0 — the ASR is admitted while the tool waits, the row lands,
the tool runs once and the reply is saved; before the fix the turn never ended.

*Changed (WP2 review fixes):*
- **The tool path's refusals.** A tool turn refused at admission, or on the route admission settled
  on (`fit_route`), writes its request row as the plain path does (`agentchat/refused.rs`). Inside
  the loop, a route the runner refuses gets no reasoning fit — no catalog read — before the refusal
  (`proxy/in_process.rs`).
- **What a refusal went out as.** A routed request's `error` frame carries, in-process only,
  `TurnFrame.sent`: who it went to in the thread model's place (`answered_by`) and whether it carried
  an image. The responder keys its memory and reads llama-server's media error by it (§3.5).
- **Tests.** The `off` golden on a managed server: `tests/fixtures/chat_requests/managed_off.json`
  (`tests/it/chat_voice_audio_in/managed_off.rs`): the llama-server body of a bound voice turn with
  the setting off, and the shape of the session's chat events (types, frame events, keys; the system
  prompt's date masked). Captured when the fixes landed, not before WP2: the seam goldens prove the
  request with no spoken parts unchanged through WP2, and this one guards a managed body and the
  events from here on. A guest's re-pick to its alias's cloud fallback (llama-server's context
  refusal held back) is refused the audio on the plain path and in the tool loop
  (`turns.rs`); a refused route gets no catalog read (`proxy/in_process/tests.rs`). *Superseded
  2026-10-06:* a cloud fallback that takes audio gets it there; one that reads text only is refused
  (`fallbacks.rs`). The refused route still gets no reasoning fit, but the predicate reads its
  catalog to judge it.

### 3.5 A refused audio attempt (`thread/turn/audio.rs`)

The responder re-reads the thread as today (`turn.rs:174`). If a thread-level row of the verdict
now fails (setting off, auto-mode knowledge), it skips the audio attempt and runs the transcript
turn below at once. Route and capability are left to `fit_route` and the model's own refusal.

**Retried once with the transcript:** a refusal of the audio attempt before its first `delta`,
`reasoning` or `tool` frame, of two kinds. One is lmgw's own about the audio: `audio_not_local`
(*2026-10-06:* `audio_input_unsupported`),
or the context guard's (`gate/count.rs:197-205`) when a candidate alias picked a guarded row. The
other is a 4xx from the managed server: an audio part it cannot take, a body too large, or
`context_length_exceeded` (the transcript is 7–10× smaller). Such a 4xx is not left over from the
reasoning fit's retries, because that fit never retries an off on a local route
(`Fitted::retryable`, `proxy/reasoning_fit. *Added 2026-10-06:* a cloud route now hears too, and there the reasoning fit does retry an off (model-capabilities §5.6): a 4xx about the reasoning parameter can be read as the server's refusal of the audio and cost one transcript call more, never a wrong answer.rs:155-160`). **Not retried:** a 5xx, a dropped
connection, a server crash or OOM, a timeout, `gpu_hold`, `gpu_benchmark`, `vram_queue_timeout`,
`vram_too_large`, `superseded`, `not_saved`, a stop, or a policy refusal.

**How:** the attempt's `error` and `done` frames are not relayed. Neither attempt sends a `turn`
frame: an audio response's barrier answers no id. The responder waits on `user_row`. A row starts
a second turn, `Fresh {user_message_id: None}` with no `spoken`, over the history now holding the
row; `failed` fails the response with `transcription_failed` (worded as at `lifecycle.rs:326-337`);
on `veto` the core has cancelled already. The journal gets the retry's generation in `In::Saved`
(`turn.rs:335`). A failed retry is the response's error; there is no third attempt.

**The note:** `Msg::Input {input: "transcript", why}` becomes `lmgw.chat.input` and the timing's
`input_why`. Only a refusal by the model's server is remembered (`Bound.refused`, the verdict's
last row), so its later turns go as transcript until voice mode is entered again: one refusal, one
note, no loop. `audio_not_local` and the context guard concern the route, not the model, and are
not remembered. (*2026-10-06:* `audio_input_unsupported` concerns the route the gate took this time,
which the next verdict judges itself: not remembered either. A cloud model's own refusal of the
audio is a server's refusal, retried and remembered like any other.)

*Built (WP2):*
- **Changed: llama-server refuses an audio part it cannot take with a 500**, not a 4xx:
  `oaicompat_chat_params_parse` throws "audio input is not supported" and `handle_media` "Failed to
  load image or audio file" as server errors (llama.cpp `tools/server/server-common.cpp`). These two
  500s are retried and remembered like a 4xx; every other 5xx is not. The retried 4xx are 400, 413,
  415 and 422, and `context_length_exceeded`. The guard's refusal is told apart by its text, now the
  constant `gate::count::AUDIO_UNBOUNDED`.
- **`Msg::Input {input, why, refused}`**: `refused` is the server's refusal, which the core keeps as
  `Bound.refused`. `AudioInput::after_refusal` is the verdict's session row ("it refused the audio
  this session: …"). In WP2 the core logs the note (INFO) and keeps the memory; `lmgw.chat.input`,
  the timing's `input_why` and reading `Bound.refused` at commit are WP3's.
- **A skipped attempt** (a thread row fails by now) notes its `why`, not remembered, and waits for
  the row as a retry does. A veto or the response's stop during that wait relays `done {aborted}`
  alone; a failed transcription relays `transcription_failed` (500, `server_error`) and `done
  {aborted}`.
- **Until WP3** every launch passed `Job.audio: None`. Since WP3 the launch hands `Job.audio` a
  `Launch` (its WAVs, maybe still being built, and its texts), which the responder makes into the
  request's parts; the dead-code allowances are gone.

*Changed (WP2 review fixes):*
- **The memory is kept by the model that refused**, as the turn names who answered (a candidate
  alias's pick by its public name, else the thread's model): `Bound.refused` is a map, and the
  verdict carries `via`, the models an audio turn may reach (the model, or the routable candidates).
  Its session row applies once every one of them refused: a thread switched to another model, or a
  candidate alias that may still pick one that did not, hears again.
- **Kept only when it is about the audio**: at once when the server's message names the audio, else
  once the transcript retry answered (a refusal the transcript meets as well says nothing about the
  audio). llama-server's "Failed to load image or audio file" counts as a refusal of the audio only
  when the request carried no image. `Msg::Input {input, why}` is the note; `Msg::Refused {refused,
  note}` the memory.
- **A server that went away under the audio** (a dropped connection after the dead-container
  restart, or a restart that failed) is still not retried — the response's error — but is kept for
  that model at once, with a note (`lmgw.chat.input`, about the later turns; the response's own
  timing keeps its path), so later turns do not pay two restarts each. *Changed 2026-10-06
  (decision D5):* kept only when the attempt went to a llama-server, where a drop under the audio is
  evidence about it; elsewhere it says nothing about the audio and is not kept.
- **Only a frame carrying a gateway error decides.** An `error` frame with none (an MCP server that
  did not answer, before any model call) no longer disarms the attempt: it and what follows it,
  `state` frames aside, are held back — dropped when the audio is refused (the retry says them
  again), relayed in order otherwise.
- **The skipped attempt relays how it ended** as the retry does: the frames above are now sent.
- **Wording.** lmgw's own refusals say nothing was sent ("its route is a model lmgw does not run, so
  nothing was sent: …" — *superseded 2026-10-06:* the predicate's sentence, e.g. "openai/gpt does
  not take audio input (upstream 'cloud'), so nothing was sent" —, "the model picked for it guards
  its context, …"); `transcription_failed` on
  the audio path no longer points at a `transcription.failed` that is not sent there.

*Changed (WP3 review fixes):* a refused or skipped attempt never makes its turn heard (§3.1's note),
so a failed transcription during its wait is said as with the setting off and the core vetoes; the
`failed` answer can no longer reach a waiting attempt, and stays only as a defence. A row the
journal could not write ends the wait with `chat_history_write_failed` (§3.3's note). The module's
`Launch`/`Spoken`, its refusal classification and its wait moved to child modules
(`audio/launch.rs`, `audio/refusal.rs`, `audio/settled.rs`), and the journal's inputs to
`journal/input.rs`.

## 4. Nothing is stored

The samples and the WAV live in memory for the turn's ASR and request only, never in
`chat_messages`, attachments, `ir_messages`, request rows or logs; llama-server's prompt cache holds
KV state, not audio. **Rejected:** the WAV as an attachment, so a regenerate could hear it. It
breaks chat-voice ruling 1, and a user's voice would be the most personal data the app holds,
copied into exports, Keep copies and backups; replaying as transcript costs nothing (probe).

## 5. Timing, events and the page

`VoiceTiming` (`store/chat_voice.rs:258`) gains, read tolerantly, `input` (`"audio"` |
`"transcript"`), `input_why` (why the transcript, or the refusal) and `transcript_wait_ms` (how long
the first output was held: 0 if nothing waited, absent on the transcript path). `Timing`
(`lifecycle/timing.rs:69`) gains `held_at` and `released`. Arrival marks are taken when a message
reaches the core, held or not (a held delta's first token, a held clause's synthesis). What
reached the client (`first`, `to_first_audio_ms`) counts at the later of arrival and release, so
`first_token_ms`, `first_clause_ms` and `first_audio_ms` are not inflated, and the wait counts
once. `asr_ms` stays, off the critical path; `first_token_ms` runs from the launch, now the
commit. `bound_ended` (`lifecycle/bound.rs:269-305`) fills them in, and the log line
(`timing.rs:189`) gains `input=audio held=N ms`. The user row's `MessageVoice`
(`store/chat_voice.rs:222`) gains `input` and `transcript_error`.

**Events:** `lmgw.chat.input {response_id, input, why}` is new, sent at each bound response's
launch and on a retry. `lmgw.chat.user` gains `response_id`, and `lmgw.chat.thread` may now come
from the journal. The `/v1/realtime` DocRoute (`openapi/planes/inference.rs:474-490`) gains the
event, the field, and the three timing fields in its list (`:486-488`). The settings op and MCP
gain the key. **The page:** a user bubble with `transcript_error` says "not transcribed — the
model heard it" (`pages/chat_voice/realtime/bubbles.rs:47`) and sits before its response's reply.
The mic badge's tooltip names the path, regenerate gets its tooltip (§3.4), and the timing readout
says "heard as audio, held N ms" or "transcript: <why>".

*Built (WP3):*
- `Timing` gains `held_at`, `released` and the first held output's arrival; `transcript_wait_ms` is
  absent for a response held and never released. The log line ends `; input=audio held=N ms`.
- `lmgw.chat.input` and the timing's `input`/`input_why` are sent while the thread's setting is
  `local` (*2026-10-06:* `on`) (whatever the path); a launch whose turns were all transcribed before it says why:
  "its turns were transcribed before it started".
- **The page:** the INPUT chip takes each `lmgw.chat.input` over the thread JSON's verdict
  (`Realtime.said_input`); the mic badge's title names the path and an error; the not-transcribed
  note sits beside the mic badge; the regenerate tooltip says it on a heard user row too.

*Changed (WP3 review #4):* the page says a `response.done {cancelled, no_words}` as it says
`empty_turn` ("nothing new was said: no reply"): a push-to-talk turn with no words no longer goes
idle without a note. *Changed (verification review):* only in push-to-talk (`rt.ptt`), the one
mode that gets `empty_turn` with the setting off. A hands-free veto stays quiet, as §3.2 and
`held.rs` say: server VAD keeps noise out with the setting off, and a note per false trigger would
be new noise of its own.

*Changed (WP3 review fixes):* `transcript_wait_ms` is absent on the transcript path, a refused
attempt's retry included (#7); a `transcription_failed` veto says nothing more than the failure
event before it.

## 6. Scope, and later

**In:** sessions bound to a chat thread, with audio or text output (text is held too). **Out:**
dictation stays on ASR: it puts editable text in the composer, and a recording can be attached
instead. Read-aloud is output only.

**Later, and why nothing here blocks it:**
- **Release on a words verdict from the turn's head.** The hold has one release (`held.rs`). A
  verdict from the turn's first seconds (an ASR call on its head, say) calls it early, while the
  row still waits for the full transcript. This is what turns the ASR's time into gain.
- **`any`, audio to cloud models:** a new setting value. The verdict's locality row and
  `fit_route`'s `local_only` take it, one predicate each. *Superseded 2026-10-06:* done without a
  new value: `on` sends audio to any model that takes it.
- **Stock `/v1/realtime` sessions** (realtime §19): `hearing`, launch at commit, the hold and the
  veto live in the core, not the binding. Such a session needs the renderer (`realtime/render.rs`)
  to carry a held turn's WAV as `ContentPart::Audio`, and no journal.
- **Omni models that also speak** (audio out instead of TTS): the hold queues output messages
  whatever made their PCM, and the verdict is a capability check. An output-modality row and a
  responder path that relays the model's audio can be added without changing either.

**Not planned:** dropping the ASR (worse transcript, no veto); storing turn audio (§4); a token
bound per audio part for guarded rows (the owner's v1 decision, `gate/count.rs:147`); Gemma 4 E4B
as listener beside a larger answerer (better listener, worse answerer).

## 7. Tests and live checks

**Unit tests:** the verdict table row by row (a llama-server on another host is remote —
*superseded 2026-10-06:* it hears like any model that takes audio; the hold
with a candidate fallback; a guarded row); `has_words` with `hearing`, `busy_for_launch`, a debt
decided at commit and dropped on an empty transcript; the hold (pass-through, replay order, veto,
failed ASR, marks); `build_messages` with `spoken` (merge, placeholder, `None` = today's bytes);
`local_only` in `fit_route` (*2026-10-06:* `may_hear`, and the predicate's own tests,
`capabilities/hears/tests.rs`); §3.5's classification; the journal (row before reply, the `write_if`
fallback, veto, failed marker, title).

**ITs** (`tests/it` module `chat_voice_audio_in.rs`, mock upstreams, TTS fixtures):
- **Request shape:** `input_audio` for the new turn only, earlier turns as text. **`off`:**
  requests and events byte-identical to today (golden).
- **Veto:** the ASR mock answers "" after the chat mock streamed. Nothing reaches the client, no
  `error` is sent, the response ends `cancelled`, and no row is written.
- **Slow ASR:** `transcript_wait_ms > 0`, and the user row's id is below the reply's. **Failed
  ASR:** the reply plays with no `transcription.failed` or `error`; the row has
  `transcript_error`, and the next request carries the placeholder.
- **Privacy:** a hold with a cloud fallback gives the transcript verdict. Swaps at admission, in a
  climb and to a candidate fallback each give `audio_not_local` and a retry. The cloud mock never
  sees `input_audio`. (*Built:* admission's swap and a guest's re-pick to its cloud fallback, plain
  and tool threads, in `turns.rs` and `session.rs`; the climb shares the same `fit_route` call.)
  *Superseded 2026-10-06: **Fallbacks.*** A hold with a fallback that takes audio gives the audio
  verdict naming it, and the fallback gets the audio; one that reads text only gets the transcript.
  Admission's swap and a guest's re-pick to a cloud fallback that takes audio send it the audio,
  plain and tool threads; to one that reads text only they give `audio_input_unsupported` and a
  retry; a fallback's own refusal is kept for it alone (`turns.rs`, `session.rs`, `fallbacks.rs`). A
  server's `/props` saying no audio projector refuses the audio; one saying it loaded one lifts an
  unknown (`props.rs`).
- **Refusal:** a 400 gives one retry and the note, no `turn` frame, and the next turn goes as
  transcript. A second 400 is the error, a 503 is not retried, and a refusal plus a failed ASR is
  `transcription_failed`.
- **Pause, tool threads, timing:** two commits get one reply; audio goes in each tool-loop call,
  never in `ir_messages`; §5's fields are stored and sent; the title reaches the page.

**The live check** runs on a dev copy (`scripts/dev-copy.sh`) with its Gemma 4 12B row and the ASR
on the CPU. It uses only TTS-generated German clips (the probe's corpus) and synthetic noise and
silence, and calls no cloud model. A committed driver, `scripts/voice-audio-in-check.py`, streams
each clip in real time into `/v1/realtime?chat_thread=` (server VAD), with one thread under
`local` (*2026-10-06:* `on`) and then `off`. *Added 2026-10-06:* it refuses to run when the
thread's verdict names another model than the one under test, and fails a run a fallback answered,
so it never calls a cloud model now that fallbacks hear. It reports median `to_first_audio_ms` per path for 2–5 s and 30–60 s
turns, `transcript_wait_ms`, prompt tokens per second of audio, both paths' replies side by side,
whether noise and silence end quietly, and whether a paused utterance is answered once.

*Added (WP3 review fixes):* the tools wait (`tools.rs`: a stub MCP server and the self-admin plane,
noise and words), a failed transcription before the model answered, speech recognition gone
mid-session and the session closing mid-hold (`transcripts.rs`), the carried rule, the cut's owed
turn and the never-attempted failures on a bound core (`lifecycle/tests/hearing.rs`), the usage and
the cut whatever `interrupt_response` says (`hold.rs`), a row nobody heard and a row the store
refused (`row/tests.rs`), the ASR row (`audio_input/tests.rs`).

*Built (WP3):* the driver times commit → first token and commit → first audio as the client sees
them (fresh thread per run, warm, ≥5 runs per path), and adds two sessions to the multi-turn one: a
300 ms silence window, so the paused clips commit twice, and push-to-talk for noise and silence,
since server VAD keeps the corpus's noise and silence out entirely. **Found live:** with audio input
on, a push-to-talk turn with no words is vetoed quietly where `off` answers `empty_turn` (decision
8); and the faster path meets a longer pause as a barge-in on an answer already playing, which is
then kept cut to what was heard, as on `off` after a longer pause. The numbers went to the owner.

## 8. Work packages (build order)

Each WP ends green on `cargo fmt --check --all`, `cargo test -p <crates touched>`, and a fresh
`trunk build` when the UI changed. Each is committed with explicit paths.

**WP1: the setting, the verdict, the storage fields** (§2, §5's fields); no turn changes.
*Delivers* the key, `ThreadVoice.audio_input`, `verdict`, `VoiceConfig.audio_input`, the new
`VoiceTiming`/`MessageVoice` fields, and the drawer line, Settings select and chip text.
*Verified by* the verdict tests, a settings IT (both save paths, the MCP schema, the thread
override) and `scripts/ui-matrix.py`. *Live:* on the dev copy, the 12B thread shows "audio input is
off" by default and "your voice goes to … as audio" under `local` (*2026-10-06:* `on`); a cloud-model thread shows its
transcript line. *Built:* the settings IT is `tests/it/chat_voice_audio_in.rs` (§7's module, which
WP2 and WP3 extend).

**WP2: the request** (§3.4, §3.5), testable through `TurnOpts` alone. *Delivers* `spoken`,
`user_row`, the append and placeholder, the barrier in `persist`, `local_only` in `fit_route`
(*2026-10-06:* `may_hear`),
`thread/turn/audio.rs` and the refusal memory. *Verified by* their unit tests and the
request-shape, privacy, refusal, `off`-golden and tool-thread ITs at the seam
(`chat_turn/seam_tests.rs`). *Live:* under the default `off`, three German clips in voice mode
give the rows, replies and timing they gave before.

**WP3: the core, the journal, the page** (§3.1–§3.3, §5). *Delivers* `hearing`, the WAV built
once, `has_words`/`busy_for_launch`, the decision at commit, the hold, `In::Began`/`In::Heard`,
`append_spoken_user`, the title event, and the timing, events, bubbles and docs. *Verified by* the
journal and hold tests, the remaining ITs, a `scripts/drive/` check of the bubbles, and §7's live
check, whose report goes to the owner with the review. *Built:* the hold's tests are
`realtime/lifecycle/tests/hold.rs`, the hearing's (a bound core) `lifecycle/tests/hearing.rs`, the
journal's `thread/journal/row/tests.rs`; the session ITs (hold, veto, failed ASR, refusal, pause,
the GPU claim, `off`) are `tests/it/chat_voice_audio_in/session.rs` on `gpu_world`'s local row with
the realtime fakes; the bubbles' drive is `scripts/drive/chat-voice-heard.json` (live, on a dev
copy); §7's driver is `scripts/voice-audio-in-check.py`.

## Appendix: review finding → resolution

| Finding | Resolution |
|---|---|
| B1 latency claim | Hold until the transcript; gain restated (intro, Decision 1): 80–125 ms short or segmented, under a second unbroken, never the ASR's seconds; `transcript_wait_ms`; head-verdict release as the later upgrade (§6) |
| B2 "local" | `vram::classify(&route).is_some()` for the verdict and `fit_route`; `is_local_upstream` named and not used (§2.2). *Superseded 2026-10-06:* capability, not locality |
| M3 launch gating | `has_words` counts a turn in `hearing`; `busy_for_launch` at the three launch sites; `end_bound_turns` keeps `busy()` (§3.1) |
| M4 swaps after resolve | `audio_not_local` from `fit_route`, which every route passes before bytes leave; a `RouteCheck` strands the turn via `fallback_serves`; retried with the transcript (§3.4, §3.5). *2026-10-06:* `audio_input_unsupported`, only where the answering model cannot take the audio |
| M5/M6, S7 pending rows | No pending row: it is written once heard, as an insert under `write_if`, with a pre-save barrier in `TurnOpts` and the barrier op kept empty. The veto is quiet, pauses go through the existing re-owe, and a failed ASR plays and marks the row. Dropped: fill, `{removed}`, title move, veto delete (§3.1–§3.3) |
| S8 `audio_tokens` | Dropped (Decision 10) |
| m9 timing marks | `Mark`, `Planned`, `Tts`, `state` pass through; arrival and release marks kept apart (§3.2, §5) |
| m10 verdict | Context-guard row; the hold's candidate fallback judged through `gate::resolve`; privacy refusals not remembered (§2.2, §3.5). *2026-10-06:* lmgw's capability refusals are not remembered either |
| m11 retry | Only 4xx refusals of the audio, with the reasoning-fit point; refusal plus failed ASR gives `transcription_failed`; no `turn` frame; a failed ASR sends no `error` (§3.2, §3.5) |
| m12 title | `lmgw.chat.thread` from the journal, through `bound_planned`'s compare (§3.3) |
| m13 drift | Re-cited at 5ad9ee5; `lifecycle/held.rs` and `thread/turn/audio.rs`; WAV built once; language comparison dropped; DocRoute fields (§5) |
