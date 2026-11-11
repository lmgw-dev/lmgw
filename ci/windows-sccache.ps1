# sccache for the GitLab Windows runner's jobs (`windows-check`, `windows-installer`): fetches the
# pinned release into the per-user tool dir, starts the server, or prints its statistics.
#
# Usage, from the checkout, dot-sourced so the script's environment stays in the job's shell:
#   . ci\windows-sccache.ps1            fetch if missing or another version, start the server
#   . ci\windows-sccache.ps1 -Stats     print `sccache --show-stats` (after_script; also on failure)
#
# Runs in Windows PowerShell 5.1. The job sets RUSTC_WRAPPER (= <LMGW_TOOLS_DIR>\sccache\sccache.exe),
# SCCACHE_DIR, SCCACHE_CACHE_SIZE and CARGO_INCREMENTAL (.windows-runner in .gitlab-ci.yml).
# Nothing is installed system-wide. GitHub's hosted runner does not use this (not yet: its cache
# would not outlive the job).
param([switch]$Stats)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# The pinned release and the SHA-256 of its x86_64-pc-windows-msvc .zip (GitHub's `digest` of
# the asset on mozilla/sccache's release page). Bump both together.
$SccacheVersion = '0.19.0'
$SccacheWindowsSha256 = @{
    '0.19.0' = '28be8c61f6b2c89b9eb2869f6af7f78e6f992e78a8abae0ce855fcc129cbeac1'
}

$toolsRoot = if ($env:LMGW_TOOLS_DIR) { $env:LMGW_TOOLS_DIR } else { Join-Path $env:LOCALAPPDATA 'lmgw-build-tools' }
$dir = Join-Path $toolsRoot 'sccache'
$exe = Join-Path $dir 'sccache.exe'
$marker = Join-Path $dir 'version.txt'

# A native command's output (stderr included) as text; its failure is the caller's to judge.
function Invoke-Sccache([string[]]$Arguments) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $exe @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [Management.Automation.ErrorRecord]) { $_.Exception.Message } else { "$_" }
        } | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
}

if ($Stats) {
    if (Test-Path -LiteralPath $exe -PathType Leaf) { [void](Invoke-Sccache @('--show-stats')) }
    else { Write-Host "sccache: $exe is missing, no statistics" }
    return
}

$have = if (Test-Path -LiteralPath $marker) { (Get-Content -LiteralPath $marker -Raw).Trim() } else { '' }
if (-not ((Test-Path -LiteralPath $exe -PathType Leaf) -and $have -eq $SccacheVersion)) {
    $want = $SccacheWindowsSha256[$SccacheVersion]
    $work = Join-Path $toolsRoot "sccache-fetch-$PID"
    New-Item -ItemType Directory -Force $work | Out-Null
    try {
        $zip = Join-Path $work 'sccache.zip'
        $url = "https://github.com/mozilla/sccache/releases/download/v$SccacheVersion/sccache-v$SccacheVersion-x86_64-pc-windows-msvc.zip"
        Write-Host "Downloading sccache $SccacheVersion (installed: '$have') from $url"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
        if ($got -ne $want) { throw "the sccache $SccacheVersion download's SHA-256 is $got, not the pinned $want" }
        Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $work 'x') -Force
        $found = Get-ChildItem -LiteralPath (Join-Path $work 'x') -Recurse -Filter sccache.exe | Select-Object -First 1
        if (-not $found) { throw 'the sccache zip holds no sccache.exe' }
        # A running server of the old version holds the exe; stop it before the replacement.
        if (Test-Path -LiteralPath $exe) { [void](Invoke-Sccache @('--stop-server')) }
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item -LiteralPath $found.FullName -Destination $exe -Force
        Set-Content -Encoding Ascii -LiteralPath $marker -Value $SccacheVersion
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (-not $env:SCCACHE_DIR) { throw 'SCCACHE_DIR is not set' }
New-Item -ItemType Directory -Force $env:SCCACHE_DIR | Out-Null
[void](Invoke-Sccache @('--version'))
# A server left by an earlier job (it idles out after 10 minutes) has that job's environment;
# stop it (an error when none runs is no error) and start this job's own.
[void](Invoke-Sccache @('--stop-server'))
$code = Invoke-Sccache @('--start-server')
if ($code -ne 0) { throw "sccache --start-server exited with $code" }
