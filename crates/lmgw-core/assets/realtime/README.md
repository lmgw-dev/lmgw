# Realtime voice models

Copied unmodified from upstream; embedded into the binary with `include_bytes!`
(realtime design §13).

| File | Model | Version | Licence | sha256 | Source |
|---|---|---|---|---|---|
| `silero_vad_16k_op15.onnx` | Silero VAD | v6.2.3, commit `1e261b0` | MIT, `LICENSE-silero` | `7ed98ddbad84ccac4cd0aeb3099049280713df825c610a8ed34543318f1b2c49` | https://github.com/snakers4/silero-vad (`src/silero_vad/data/silero_vad_16k_op15.onnx`) |
| `smart-turn-v3.2-cpu.onnx` | Smart Turn (Daily / Pipecat) | v3.2, int8 CPU build, 8.7 MB | BSD-2-Clause, `LICENSE-smart-turn` (the licence of https://github.com/pipecat-ai/smart-turn, copyright Daily); its encoder backbone is OpenAI's Whisper Tiny (the model card says so), MIT, `LICENSE-whisper` | `2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f` | https://huggingface.co/pipecat-ai/smart-turn-v3 (`smart-turn-v3.2-cpu.onnx`) |

The model runs on ONNX Runtime 1.28.0 (the `ort` crate, statically linked
from its prebuilt), which is MIT-licensed. Its licence and its
`ThirdPartyNotices` are here, copied unmodified from the release tag — the
prebuilt archive carries neither:

| File | From | sha256 |
|---|---|---|
| `LICENSE-onnxruntime` | https://github.com/microsoft/onnxruntime/blob/v1.28.0/LICENSE | `2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c` |
| `ThirdPartyNotices-onnxruntime.txt` | https://github.com/microsoft/onnxruntime/blob/v1.28.0/ThirdPartyNotices.txt | `0e07b95f3a8d6230037707c5c4a2b554d12c4cb67369669ac255635528ffcee2` |

`LICENSE-whisper` is the standard MIT licence text with Whisper's
copyright line ("Copyright (c) 2022 OpenAI", as in
https://github.com/openai/whisper), written here rather than fetched.

The RPM and the AppImage install these, `LICENSE-silero`,
`LICENSE-smart-turn` and `LICENSE-whisper` under `/usr/share/licenses/lmgw/`
(`src-tauri/tauri.conf.json`, `bundle.linux`; Whisper's next to Smart
Turn's, in `smart-turn/`), since the binary carries the code and the models
they cover. Update the ONNX Runtime ones with the `ort` pin.
