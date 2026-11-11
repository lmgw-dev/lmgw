# Probe the upstream Windows builds of lmgw's three engines (llama.cpp, stable-diffusion.cpp,
# audio.cpp) as native processes: unpack the pinned release zips, start each server, send one
# request, report readiness time, request time and the card's memory use.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\windows-engine-probe.ps1 -Chat <gguf>
#          [-ImageDiffusion <gguf> -ImageLlm <gguf> -ImageVae <safetensors>] [-Tts <dir> -Asr <dir>]
#          [-Variants cuda12,vulkan] [-Work <dir>]
# Records: docs/design/2026-11-10-windows-native-design.md §2.
param(
  [string]$Chat, [string]$ImageDiffusion, [string]$ImageLlm, [string]$ImageVae,
  [string]$Tts, [string]$Asr,
  [string[]]$Variants = @('cuda12', 'vulkan'),
  [string]$Work = (Join-Path $env:LOCALAPPDATA 'lmgw-engine-probe')
)
$ErrorActionPreference = 'Stop'

$gh = 'https://github.com'
$Builds = @{
  'llama-cuda12' = @("$gh/ggml-org/llama.cpp/releases/download/b12236/llama-b12236-bin-win-cuda-12.4-x64.zip",
                     "$gh/ggml-org/llama.cpp/releases/download/b12236/cudart-llama-bin-win-cuda-12.4-x64.zip")
  'llama-vulkan' = @("$gh/ggml-org/llama.cpp/releases/download/b12236/llama-b12236-bin-win-vulkan-x64.zip")
  'sd-cuda12'    = @("$gh/leejet/stable-diffusion.cpp/releases/download/master-1079-a3c49b1/sd-master-a3c49b1-bin-win-cuda12-x64.zip",
                     "$gh/leejet/stable-diffusion.cpp/releases/download/master-1079-a3c49b1/cudart-sd-bin-win-cu12-x64.zip")
  'sd-vulkan'    = @("$gh/leejet/stable-diffusion.cpp/releases/download/master-1079-a3c49b1/sd-master-a3c49b1-bin-win-vulkan-x64.zip")
  'audio-cuda12' = @("$gh/0xShug0/audio.cpp/releases/download/v0.9.4/audio-v0.9.4-bin-windows-x64-cuda12.4.zip",
                     "$gh/0xShug0/audio.cpp/releases/download/v0.9.4/audio-v0.9.4-cudart-windows-x64-cuda12.4.zip")
  'audio-vulkan' = @("$gh/0xShug0/audio.cpp/releases/download/v0.9.4/audio-v0.9.4-bin-windows-x64-vulkan.zip")
}

function Get-Build([string]$name) {
  $dir = Join-Path $Work "engines\$name"
  if (Test-Path (Join-Path $dir '.unpacked')) { return $dir }
  New-Item -ItemType Directory -Force $dir, (Join-Path $Work 'zips') | Out-Null
  foreach ($u in $Builds[$name]) {
    $zip = Join-Path $Work ('zips\' + [IO.Path]::GetFileName($u))
    if (-not (Test-Path $zip)) {
      curl.exe -sSL --fail -o "$zip.part" $u
      if ($LASTEXITCODE) { throw "download failed: $u" }
      Move-Item -Force "$zip.part" $zip
    }
    tar.exe -xf $zip -C $dir
    if ($LASTEXITCODE) { throw "unpack failed: $zip" }
  }
  New-Item -ItemType File (Join-Path $dir '.unpacked') | Out-Null
  $dir
}

function VramUsed { [int](nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits).Trim() }

# Start $exe, wait for $ready to answer 200, run $request (a scriptblock given the port), stop it.
function Probe([string]$label, [string]$exe, [string]$argLine, [int]$port, [string]$ready, [scriptblock]$request) {
  $logs = Join-Path $Work 'logs'; New-Item -ItemType Directory -Force $logs | Out-Null
  $log = Join-Path $logs $label
  $before = VramUsed
  $t0 = Get-Date
  $p = Start-Process -FilePath $exe -ArgumentList $argLine -WorkingDirectory (Split-Path $exe) `
         -RedirectStandardOutput "$log.out" -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
  $up = $false
  while (((Get-Date) - $t0).TotalSeconds -lt 900 -and -not $p.HasExited) {
    Start-Sleep -Milliseconds 500
    if ((curl.exe -s -o NUL -w '%{http_code}' "http://127.0.0.1:$port$ready") -eq '200') { $up = $true; break }
  }
  $readyS = ((Get-Date) - $t0).TotalSeconds
  $result = 'not ready'; $reqS = 0; $during = $before
  if ($up) {
    $t1 = Get-Date
    $result = & $request $port
    $reqS = ((Get-Date) - $t1).TotalSeconds
    $during = VramUsed
  }
  if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
  $p.WaitForExit()
  $err = (Get-Content "$log.err", "$log.out" -ErrorAction SilentlyContinue) -match 'no kernel image|error|failed' | Select-Object -First 2
  [pscustomobject]@{ probe = $label; ready_s = [math]::Round($readyS, 1); request_s = [math]::Round($reqS, 1)
                     result = $result; vram_mib = $during - $before; note = ($err -join ' | ') }
}

function Post([int]$port, [string]$path, [string]$json, [string]$out = 'NUL') {
  $body = Join-Path $Work 'body.json'; Set-Content -Encoding ascii $body $json
  curl.exe -s -o $out -w '%{http_code}' -H 'Content-Type: application/json' --data-binary "@$body" "http://127.0.0.1:$port$path"
}

New-Item -ItemType Directory -Force $Work | Out-Null
$port = 18180
$rows = foreach ($v in $Variants) {
  if ($Chat) {
    $port++
    $exe = Join-Path (Get-Build "llama-$v") 'llama-server.exe'
    Probe "llama-$v" $exe "-m `"$Chat`" --host 127.0.0.1 --port $port -ngl 99 -c 4096" $port '/health' {
      param($p) Post $p '/v1/chat/completions' '{"messages":[{"role":"user","content":"Name three colors."}],"max_tokens":32,"chat_template_kwargs":{"enable_thinking":false}}'
    }
  }
  if ($ImageDiffusion) {
    $port++
    $exe = Join-Path (Get-Build "sd-$v") 'sd-server.exe'
    Probe "sd-$v" $exe "--diffusion-model `"$ImageDiffusion`" --llm `"$ImageLlm`" --vae `"$ImageVae`" --offload-to-cpu --steps 4 --cfg-scale 1.0 --listen-port $port" $port '/' {
      param($p) Post $p '/v1/images/generations' '{"prompt":"a red apple on a wooden table","size":"512x512","n":1}' (Join-Path $Work "image-$v.json")
    }
  }
  if ($Tts) {
    $port++
    $exe = Join-Path (Get-Build "audio-$v") 'audiocpp_server.exe'
    $backend = $v -replace '\d+$', ''
    $models = @(@{ id = 'tts'; family = 'supertonic'; mode = 'offline'; task = 'tts'; path = $Tts })
    if ($Asr) { $models += @{ id = 'asr'; family = 'parakeet_tdt'; mode = 'offline'; task = 'asr'; path = $Asr } }
    $cfg = Join-Path $Work "audio-$v.json"
    @{ backend = $backend; device = 0; host = '127.0.0.1'; port = $port; threads = 4; lazy_load = $true; models = $models } |
      ConvertTo-Json -Depth 5 | Set-Content -Encoding ascii $cfg
    Probe "audio-$v" $exe "--config `"$cfg`" --no-ui" $port '/health' {
      param($p)
      $wav = Join-Path $Work "tts-$v.wav"
      $code = Post $p '/v1/audio/speech' '{"model":"tts","input":"Hello from Windows. This is a native test of the audio engine.","voice":"M5","response_format":"wav"}' $wav
      if ($Asr -and $code -eq '200') {
        $text = curl.exe -s -F 'model=asr' -F "file=@$wav" "http://127.0.0.1:$p/v1/audio/transcriptions" | ConvertFrom-Json
        "$code asr: $($text.text)"
      } else { $code }
    }
  }
}
$rows | Format-Table -AutoSize -Wrap | Out-String -Width 220
