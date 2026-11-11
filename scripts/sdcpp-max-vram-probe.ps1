# Usage (on a Windows host with an NVIDIA card, e.g. a Windows test VM, through ssh):
#   powershell -NoProfile -ExecutionPolicy Bypass -File sdcpp-max-vram-probe.ps1 `
#     -Name offload-max1 -Port 18912 -Extra "--offload-to-cpu --max-vram 1" `
#     -Exe C:\path\to\sd-vulkan\sd-server.exe -Models C:\path\to\models -Scratch D:\scratch
# Starts sd-server with Z-Image-Turbo (z_image_turbo-Q4_K.gguf, ae.safetensors,
# Qwen3-4B-Instruct-2507-Q4_K_M.gguf as the text encoder) and the case's flags on a
# loopback port, samples the card's memory.used every 100 ms, reads it at rest 5 s after
# /sdcpp/v1/capabilities answers, posts one 512x512 /v1/images/generations, and prints
# one line: the baseline, the reading at rest and the maximum during the generation (MiB),
# then the log lines that name the budget. Stops sd-server and the sampler at the end.
# Per-model containers design §4 records what it measured on 2026-11-11. Run one case at
# a time with nothing else on the card: the figures are the card's, not the process's
# (WDDM lists no per-process memory).
param(
  [Parameter(Mandatory)][string]$Name,
  [Parameter(Mandatory)][int]$Port,
  [string]$Extra = "",
  [Parameter(Mandatory)][string]$Exe,
  [Parameter(Mandatory)][string]$Models,
  [Parameter(Mandatory)][string]$Scratch
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force $Scratch | Out-Null
function used { [int]((nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits) | Select-Object -First 1).Trim() }
function most($lines) { ($lines | ForEach-Object { [int]($_.Split(',')[1].Trim()) } | Measure-Object -Maximum).Maximum }
$base = used
$a = "--listen-ip 127.0.0.1 --listen-port $Port --diffusion-model $Models\z_image_turbo-Q4_K.gguf " +
     "--vae $Models\ae.safetensors --llm $Models\Qwen3-4B-Instruct-2507-Q4_K_M.gguf " +
     "--eager-load --steps 8 --cfg-scale 1 $Extra"
$p = Start-Process -FilePath $Exe -ArgumentList $a -RedirectStandardOutput "$Scratch\$Name.out" `
  -RedirectStandardError "$Scratch\$Name.err" -PassThru -WindowStyle Hidden
$smi = Start-Process -FilePath nvidia-smi -ArgumentList "--query-gpu=timestamp,memory.used --format=csv,noheader,nounits -lms 100" `
  -RedirectStandardOutput "$Scratch\$Name.smi" -PassThru -WindowStyle Hidden
try {
  $ready = $false
  for ($i = 0; $i -lt 600 -and -not $p.HasExited; $i++) {
    try {
      Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$Port/sdcpp/v1/capabilities" | Out-Null
      $ready = $true; break
    } catch { Start-Sleep -Milliseconds 500 }
  }
  Start-Sleep -Seconds 5
  $idle = used
  $mark = (Get-Content "$Scratch\$Name.smi").Count
  $status = "not-ready"; $secs = 0
  if ($ready) {
    $body = '{"prompt":"a red bicycle leaning on a brick wall","size":"512x512","n":1}'
    $t = [Diagnostics.Stopwatch]::StartNew()
    try {
      $r = Invoke-WebRequest -UseBasicParsing -TimeoutSec 1200 -Method Post -ContentType "application/json" `
        -Body $body "http://127.0.0.1:$Port/v1/images/generations"
      $status = "http-$($r.StatusCode)"
    } catch { $status = "error: $($_.Exception.Message)" }
    $secs = [math]::Round($t.Elapsed.TotalSeconds, 1)
  }
  Start-Sleep -Milliseconds 500
  $peak = most (Get-Content "$Scratch\$Name.smi" | Select-Object -Skip $mark)
  "case=$Name base=$base idle=$idle peak=$peak at_rest=$($idle - $base) during=$($peak - $base) gen=$status secs=$secs"
  Get-Content "$Scratch\$Name.out", "$Scratch\$Name.err" -ErrorAction SilentlyContinue |
    Select-String -Pattern "auto-fit|budget|\[E\]" | Select-Object -First 8 | ForEach-Object { "  log: " + $_.Line }
} finally {
  Stop-Process -Id $smi.Id -Force -ErrorAction SilentlyContinue
  if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
}
