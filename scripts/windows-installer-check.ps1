# Check the per-user NSIS installer and the update path on Windows end to end (Windows design
# §4.4, §7 WP7), against a feed served on loopback from this machine; nothing leaves it.
#
# Usage (Windows PowerShell 5.1, as a non-administrator, with no lmgw installed for the account):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows-installer-check.ps1 `
#     -Old <lmgw_A_x64-setup.exe> -New <lmgw_B_x64-setup.exe> -DataDir <dir> -ApiPort <port> -FeedPort <port>
#
# -Old and -New are two installers of ci\build-windows.ps1 with versions A < B. -DataDir is an
# lmgw data dir whose settings bind the API to 127.0.0.1:<ApiPort> (a fresh one seeded by the
# headless runner: `cargo run --example headless -- 127.0.0.1:<ApiPort>` with LMGW_DATA_DIR set,
# stopped once it serves), so the installed app runs beside nothing else's port and data.
#
# 1. installs A silently (`/S`), as the account, and checks the install dir
#    (%LOCALAPPDATA%\Programs\lmgw), the Start menu entry and the uninstall key;
# 2. serves latest.json (version B, its `nsis` entry pointing at B with B's SHA-256) and B from
#    127.0.0.1:<FeedPort>, starts the installed lmgw.exe with LMGW_DATA_DIR and
#    LMGW_UPDATE_ENDPOINT pointing at them, and waits for GET /v1/models on <ApiPort>;
# 3. waits for the "Update available" dialog (the updater's first check, 20 s after the start)
#    and clicks "Install & Restart" (TDM_CLICK_BUTTON with rfd's custom button id 1000), as the
#    owner would; then waits for the installed version to be B, the API to answer again from a
#    new process, and the update's note and download to be gone (the new start's after_start);
# 4. stops lmgw, uninstalls silently, and checks the install dir, the Start menu entry and the
#    key are gone while %APPDATA%\lmgw and %LOCALAPPDATA%\lmgw (a sentinel file in each) stay.
#
# Every wait has a bound and says it when it runs out. Prints the installer sizes and the
# install, update and uninstall times. The sentinel files (and a data dir the script made for
# them) are removed at the end; -DataDir is kept.
param(
    [Parameter(Mandatory = $true)][string]$Old,
    [Parameter(Mandatory = $true)][string]$New,
    [Parameter(Mandatory = $true)][string]$DataDir,
    [Parameter(Mandatory = $true)][int]$ApiPort,
    [Parameter(Mandatory = $true)][int]$FeedPort
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Fail([string]$Message) { Write-Host "FAIL: $Message"; exit 1 }
function Ok([string]$Message) { Write-Host "ok: $Message" }

# Wait up to $Seconds for $Test to be true; the seconds it took, or a failure naming $What.
function Wait-For([string]$What, [int]$Seconds, [scriptblock]$Test) {
    $t0 = Get-Date
    while (((Get-Date) - $t0).TotalSeconds -lt $Seconds) {
        if (& $Test) { return ((Get-Date) - $t0).TotalSeconds }
        Start-Sleep -Milliseconds 250
    }
    Fail "$What did not happen within $Seconds s"
}

$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\lmgw'
$StartMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\lmgw.lnk'
$ExpectedDir = Join-Path $env:LOCALAPPDATA 'Programs\lmgw'
$Private = Join-Path ([IO.Path]::GetTempPath()) 'lmgw'

function Version-Of([string]$Installer) {
    if ((Split-Path -Leaf $Installer) -notmatch '^lmgw_(.+)_x64-setup\.exe$') { Fail "$Installer is not named lmgw_<version>_x64-setup.exe" }
    $Matches[1]
}
$vOld = Version-Of $Old
$vNew = Version-Of $New
foreach ($f in @($Old, $New)) {
    Write-Host ("installer {0}: {1:N1} MiB" -f (Split-Path -Leaf $f), ((Get-Item -LiteralPath $f).Length / 1MB))
}
if (Test-Path $UninstallKey) { Fail "lmgw is installed for this account already ($UninstallKey)" }

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Win {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")]
    public static extern IntPtr SendMessage(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
}
'@

# --- 1. install A ---------------------------------------------------------------------------------
$t0 = Get-Date
$p = Start-Process -FilePath $Old -ArgumentList '/S' -Wait -PassThru
$installSeconds = ((Get-Date) - $t0).TotalSeconds
if ($p.ExitCode -ne 0) { Fail "the silent install of $vOld exited with $($p.ExitCode)" }
$key = Get-ItemProperty $UninstallKey
$dir = $key.InstallLocation.Trim('"')
if ($dir -ne $ExpectedDir) { Fail "installed to $dir, not $ExpectedDir" }
$exe = Join-Path $dir 'lmgw.exe'
if (-not (Test-Path -LiteralPath $exe)) { Fail "no $exe" }
if (-not (Test-Path -LiteralPath $StartMenu)) { Fail "no Start menu entry $StartMenu" }
if ($key.DisplayVersion -ne $vOld) { Fail "the uninstall key says $($key.DisplayVersion), not $vOld" }
Ok ("installed $vOld silently in {0:N1} s to $dir, Start menu entry $StartMenu" -f $installSeconds)
Get-ChildItem -LiteralPath $dir | ForEach-Object { Write-Host ("  {0} {1:N1} MiB" -f $_.Name, ($_.Length / 1MB)) }

# --- 2. the feed and the start --------------------------------------------------------------------
$feedDir = Join-Path ([IO.Path]::GetTempPath()) "lmgw-feed-$FeedPort"
if (Test-Path $feedDir) { Remove-Item -Recurse -Force $feedDir }
New-Item -ItemType Directory $feedDir | Out-Null
$newName = Split-Path -Leaf $New
Copy-Item -LiteralPath $New -Destination (Join-Path $feedDir $newName)
$sha = (Get-FileHash -Algorithm SHA256 -LiteralPath $New).Hash.ToLowerInvariant()
$base = "http://127.0.0.1:$FeedPort"
$manifest = [ordered]@{
    version = $vNew
    notes = 'Local feed of scripts/windows-installer-check.ps1.'
    rpm = [ordered]@{ file = 'lmgw.rpm'; url = "$base/lmgw.rpm"; sha256 = '' }
    nsis = [ordered]@{ file = $newName; url = "$base/$newName"; sha256 = $sha }
}
$manifest | ConvertTo-Json | Set-Content -Encoding Ascii (Join-Path $feedDir 'latest.json')
# A loopback HTTP/1.0 file server in a job of its own: GET of a file in the feed dir, 404 else.
$server = Start-Job -ArgumentList $feedDir, $FeedPort -ScriptBlock {
    param($root, $port)
    $l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $port)
    $l.Start()
    while ($true) {
        $c = $l.AcceptTcpClient()
        try {
            $s = $c.GetStream()
            $r = [IO.StreamReader]::new($s)
            $line = $r.ReadLine()
            while (($h = $r.ReadLine()) -ne '') { if ($null -eq $h) { break } }
            $name = ($line -split ' ')[1].TrimStart('/')
            $path = Join-Path $root $name
            if ($name -and $name -notmatch '[\\/]' -and (Test-Path -LiteralPath $path -PathType Leaf)) {
                $body = [IO.File]::ReadAllBytes($path)
                $head = "HTTP/1.0 200 OK`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n"
            } else {
                $body = [byte[]]@()
                $head = "HTTP/1.0 404 Not Found`r`nContent-Length: 0`r`nConnection: close`r`n`r`n"
            }
            $hb = [Text.Encoding]::ASCII.GetBytes($head)
            $s.Write($hb, 0, $hb.Length)
            $s.Write($body, 0, $body.Length)
            $s.Flush()
        } catch {} finally { $c.Close() }
    }
}
$logDir = Join-Path ([IO.Path]::GetTempPath()) "lmgw-installer-check-$ApiPort"
New-Item -ItemType Directory -Force $logDir | Out-Null
$env:LMGW_DATA_DIR = $DataDir
$env:LMGW_UPDATE_ENDPOINT = "$base/latest.json"
$env:RUST_LOG = 'lmgw=info,lmgw_core=info'
$models = "http://127.0.0.1:$ApiPort/v1/models"
function Api-Answers {
    try { (Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 -Uri $models).StatusCode -eq 200 } catch { $false }
}
function Lmgw-Processes { @(Get-Process -Name lmgw -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) }
try {
    $t0 = Get-Date
    $app = Start-Process -FilePath $exe -PassThru -RedirectStandardOutput (Join-Path $logDir 'old.out') -RedirectStandardError (Join-Path $logDir 'old.err')
    $s = Wait-For "GET $models answering $vOld" 120 { if ($app.HasExited) { Fail "lmgw.exe $vOld exited with $($app.ExitCode); its log: $logDir" }; Api-Answers }
    Ok ("lmgw $vOld answers GET /v1/models {0:N1} s after its start (pid $($app.Id))" -f $s)
    $body = (Invoke-WebRequest -UseBasicParsing -Uri $models).Content
    Write-Host "  /v1/models: $($body.Substring(0, [Math]::Min(200, $body.Length)))"

    # --- 3. the update ------------------------------------------------------------------------------
    $hwnd = [IntPtr]::Zero
    $s = Wait-For 'the "Update available" dialog' 90 { $script:hwnd = [Win]::FindWindow('#32770', 'Update available'); $script:hwnd -ne [IntPtr]::Zero }
    Ok ("the update to $vNew was offered {0:N1} s after the API answered" -f $s)
    $t0 = Get-Date
    # TDM_CLICK_BUTTON (WM_USER + 102) with the custom OK button's id (rfd: 1000).
    [Win]::SendMessage($hwnd, 0x466, [IntPtr]1000, [IntPtr]::Zero) | Out-Null
    $null = Wait-For "lmgw $vOld exiting for the installer" 120 { $app.HasExited }
    Ok ("lmgw $vOld exited {0:N1} s after the click (code $($app.ExitCode))" -f ((Get-Date) - $t0).TotalSeconds)
    $null = Wait-For "the installed version becoming $vNew" 180 { (Get-ItemProperty $UninstallKey -ErrorAction SilentlyContinue).DisplayVersion -eq $vNew }
    $null = Wait-For "lmgw $vNew answering GET /v1/models" 180 { ((Lmgw-Processes).Count -gt 0) -and (Api-Answers) }
    $updateSeconds = ((Get-Date) - $t0).TotalSeconds
    $newApp = (Lmgw-Processes)[0]
    if ($newApp.Id -eq $app.Id) { Fail 'the API answers from the old process' }
    Ok ("updated to $vNew and answering again {0:N1} s after the click (pid $($newApp.Id))" -f $updateSeconds)
    $s = Wait-For 'the update note being removed by the new start' 60 { -not (Test-Path (Join-Path $Private 'update-pending.json')) }
    Ok ("the new start read and removed $Private\update-pending.json ({0:N1} s)" -f $s)
    # The start cannot remove the installer that is still running; its first update check, 20 s
    # after the start, does.
    $s = Wait-For 'the downloaded installer being removed' 90 { -not (Test-Path (Join-Path $Private 'lmgw-update-setup.exe')) }
    Ok ("the downloaded installer was removed ({0:N1} s later)" -f $s)
} finally {
    Stop-Job $server -ErrorAction SilentlyContinue
    Remove-Job $server -Force -ErrorAction SilentlyContinue
    Lmgw-Processes | Stop-Process -Force -ErrorAction SilentlyContinue
}

# --- 4. uninstall ---------------------------------------------------------------------------------
$sentinels = @()
foreach ($d in @((Join-Path $env:APPDATA 'lmgw'), (Join-Path $env:LOCALAPPDATA 'lmgw'))) {
    $made = -not (Test-Path $d)
    if ($made) { New-Item -ItemType Directory $d | Out-Null }
    $f = Join-Path $d 'installer-check-sentinel'
    Set-Content -LiteralPath $f -Value 'kept?'
    $sentinels += [pscustomobject]@{ Dir = $d; File = $f; Made = $made }
}
$null = Wait-For 'lmgw stopping' 30 { (Lmgw-Processes).Count -eq 0 }
# The uninstaller copies itself to %TEMP% and runs from there, so the uninstall key's removal is
# what says it is done, not the first process's exit. Right after lmgw was stopped in a desktop
# session the silent uninstaller ends without uninstalling and without a word (seen 2026-11-10;
# 20 s later it works), so it is tried up to three times, 20 s apart, and each try is said.
$uninstallSeconds = $null
foreach ($try in 1..3) {
    $t0 = Get-Date
    Start-Process -FilePath (Join-Path $dir 'uninstall.exe') -ArgumentList '/S' -Wait | Out-Null
    while ((Test-Path $UninstallKey) -and ((Get-Date) - $t0).TotalSeconds -lt 30) { Start-Sleep -Milliseconds 250 }
    if (-not (Test-Path $UninstallKey) -and -not (Test-Path -LiteralPath $exe)) { $uninstallSeconds = ((Get-Date) - $t0).TotalSeconds; break }
    Write-Host "  uninstall try $try ended without uninstalling; the next try comes in 20 s"
    Start-Sleep -Seconds 20
}
if ($null -eq $uninstallSeconds) { Fail 'three silent uninstalls ended without uninstalling' }
if (Test-Path -LiteralPath $StartMenu) { Fail "the Start menu entry $StartMenu stayed" }
foreach ($s in $sentinels) {
    if (-not (Test-Path -LiteralPath $s.File)) { Fail "the uninstall removed $($s.File)" }
    Remove-Item -LiteralPath $s.File
    if ($s.Made) { Remove-Item -LiteralPath $s.Dir }
}
Ok ("uninstalled silently in {0:N1} s: $dir, the Start menu entry and the key are gone; %APPDATA%\lmgw and %LOCALAPPDATA%\lmgw stayed" -f $uninstallSeconds)
if (Test-Path -LiteralPath $dir) { Write-Host "  left in ${dir}: $((Get-ChildItem -LiteralPath $dir -Force | Select-Object -ExpandProperty Name) -join ', ')" }
Remove-Item -Recurse -Force $feedDir
Write-Host "logs of the first start: $logDir"
