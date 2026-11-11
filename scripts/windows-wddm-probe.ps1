# Measure D3DKMT's per-process GPU memory against NVML's totals for one engine process (Windows
# design §4.3, WP5): read the card's used memory, start the server on a model, wait for /health,
# read the used memory and the engine PID's D3DKMT figures (the wddm-probe example), send one chat
# request, read both again, stop the server and read the used memory a last time.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\windows-wddm-probe.ps1
#          -Server <dir>\llama-server.exe -Model <gguf> -Probe <target>\debug\examples\wddm-probe.exe
#          [-Port 18200] [-ServerArgs '-ngl','99','--ctx-size','4096'] [-TimeoutSeconds 600]
# The engine builds are the release zips scripts\windows-engine-probe.ps1 pins. Records:
# docs/design/2026-11-10-windows-native-design.md §4.3.
param(
  [Parameter(Mandatory)] [string]$Server,
  [Parameter(Mandatory)] [string]$Model,
  [Parameter(Mandatory)] [string]$Probe,
  [int]$Port = 18200,
  [string[]]$ServerArgs = @('-ngl', '99', '--ctx-size', '4096'),
  [int]$TimeoutSeconds = 600
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Used-MiB {
  $v = nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits
  if ($LASTEXITCODE) { throw "nvidia-smi failed with $LASTEXITCODE" }
  [int](($v | Select-Object -First 1).Trim())
}

$before = Used-MiB
Write-Output "==> before the start: nvidia-smi used $before MiB; the probe on this PowerShell, which has no GPU device"
& $Probe $PID
$log = Join-Path $env:TEMP "wddm-probe-server-$Port.log"
$argv = @('-m', $Model, '--host', '127.0.0.1', '--port', "$Port") + $ServerArgs
$started = Get-Date
$p = Start-Process -FilePath $Server -ArgumentList $argv -WorkingDirectory (Split-Path $Server) `
  -NoNewWindow -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err"
try {
  $ready = $false
  while (((Get-Date) - $started).TotalSeconds -lt $TimeoutSeconds) {
    if ($p.HasExited) { throw "the server exited with $($p.ExitCode); see $log.err" }
    try {
      $r = Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$Port/health"
      if ($r.StatusCode -eq 200) { $ready = $true; break }
    } catch { }
    Start-Sleep -Milliseconds 500
  }
  if (-not $ready) { throw "no /health within $TimeoutSeconds s" }
  Write-Output ("==> ready after {0:N1} s, PID {1}" -f ((Get-Date) - $started).TotalSeconds, $p.Id)
  Start-Sleep -Seconds 2
  $readyMiB = Used-MiB
  Write-Output "==> ready: nvidia-smi used $readyMiB MiB, delta $($readyMiB - $before) MiB"
  nvidia-smi --query-compute-apps=pid,used_memory --format=csv
  & $Probe $p.Id

  $body = '{"messages":[{"role":"user","content":"Name three colors."}],"max_tokens":32}'
  $null = Invoke-WebRequest -UseBasicParsing -Method Post -ContentType 'application/json' `
    -Body $body "http://127.0.0.1:$Port/v1/chat/completions"
  $chat = Used-MiB
  Write-Output "==> after one chat request: nvidia-smi used $chat MiB, delta $($chat - $before) MiB"
  & $Probe $p.Id
} finally {
  if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force; $p.WaitForExit() }
}
Start-Sleep -Seconds 2
$end = Used-MiB
Write-Output "==> after the stop: nvidia-smi used $end MiB"
