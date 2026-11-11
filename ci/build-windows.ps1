# Build lmgw's per-user NSIS installer on Windows (Windows design §4.4, §5, WP7) into
# dist-windows\: the installer and nsis.json (its version, file name, size and SHA-256), which the
# publishers put into the release feed beside the RPM (`bash ci/publish.sh windows` on GitLab,
# .github/workflows/release.yml's publish-windows job on GitHub).
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File ci\build-windows.ps1 -Version <X.Y.Z[+N]> [-Jobs <n>]
#
# Runs in Windows PowerShell 5.1, from any directory (it works in the checkout it is in).
#
# - The UI, crates\lmgw-ui\dist, is Trunk's bundle from the Linux build (GitLab: the `build`
#   job's artifact; GitHub: the build job's `ui` artifact): this runner builds no wasm. A missing
#   index.html, or the placeholder a check puts there, fails the build, as ci/build.sh fails
#   rather than make a UI-less RPM.
# - The version is the Linux build's (dist\build.env's VERSION on GitLab, the tag's on GitHub),
#   stamped into Cargo.toml and src-tauri\tauri.conf.json as ci/build.sh stamps it, so the
#   binary, the installer's name and the manifest agree.
# - The update feed is baked in as ci/build.sh bakes it: on GitLab (GITLAB_CI=true) this
#   project's generic package registry and the masked LMGW_UPDATE_DEPLOY_TOKEN, on GitHub
#   (GITHUB_ACTIONS=true) the public default with both variables removed, locally whatever
#   LMGW_UPDATE_* are set.
# - tauri-cli is the prebuilt release asset of the version ci/install-build-deps.sh pins for
#   Linux, downloaded once into LMGW_TOOLS_DIR (default %LOCALAPPDATA%\lmgw-build-tools) and its
#   SHA-256 checked; nothing is installed system-wide. On its first bundle the CLI downloads NSIS
#   3.11 and its Tauri plugin into %LOCALAPPDATA%\tauri itself (it checks their SHA-1).
# - -Jobs sets CARGO_BUILD_JOBS (a runner shared with other builds); without it cargo takes every
#   core.
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [int]$Jobs = 0,
    [string]$OutDir = 'dist-windows'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Set-Location (Split-Path -Parent $PSScriptRoot)

function Fail([string]$Message) {
    Write-Host "ERROR: $Message"
    exit 1
}

# A native command, its output (stderr included) passed through as text, its exit code checked.
# Windows PowerShell 5.1 turns a native command's stderr into terminating errors under 'Stop'
# when the output is redirected, as it is in CI.
function Invoke-Native([string]$Exe, [string[]]$Arguments) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        # An stderr line arrives as an error record; its text is the exception's message.
        & $Exe @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [Management.Automation.ErrorRecord]) { $_.Exception.Message } else { "$_" }
        }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    if ($code -ne 0) { Fail "$Exe $($Arguments -join ' ') exited with $code" }
}

if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(\+[0-9]+)?$') {
    Fail "-Version '$Version' is not X.Y.Z or X.Y.Z+N"
}
if ($Jobs -gt 0) { $env:CARGO_BUILD_JOBS = "$Jobs" }
Write-Host "Building the lmgw $Version installer for Windows"

# --- tauri-cli, the version the Linux build pins --------------------------------------------
# The Windows asset's SHA-256 per version (GitHub's `digest` of
# cargo-tauri-x86_64-pc-windows-msvc.zip). Bump it with ci/install-build-deps.sh's version.
$TauriCliWindowsSha256 = @{
    '2.12.0' = '78785c9b574bbe4bbfe6cc3008422df068a0f661931a9fa9d13d08407604b6a9'
}
$pin = Select-String -Path 'ci\install-build-deps.sh' -Pattern '^TAURI_CLI_VERSION="([^"]+)"'
if (-not $pin) { Fail 'ci/install-build-deps.sh names no TAURI_CLI_VERSION' }
$tauriVersion = $pin.Matches[0].Groups[1].Value
if (-not $TauriCliWindowsSha256.ContainsKey($tauriVersion)) {
    Fail "ci/install-build-deps.sh pins tauri-cli $tauriVersion; add the SHA-256 of its Windows asset to `$TauriCliWindowsSha256 here"
}
$toolsRoot = if ($env:LMGW_TOOLS_DIR) { $env:LMGW_TOOLS_DIR } else { Join-Path $env:LOCALAPPDATA 'lmgw-build-tools' }
$tools = Join-Path $toolsRoot "tauri-cli-$tauriVersion"
$cargoTauri = Join-Path $tools 'cargo-tauri.exe'
if (-not (Test-Path -LiteralPath $cargoTauri -PathType Leaf)) {
    New-Item -ItemType Directory -Force $tools | Out-Null
    $zip = Join-Path $tools 'cargo-tauri.zip'
    $url = "https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v$tauriVersion/cargo-tauri-x86_64-pc-windows-msvc.zip"
    Write-Host "Downloading tauri-cli $tauriVersion from $url"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
    $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
    $want = $TauriCliWindowsSha256[$tauriVersion]
    if ($got -ne $want) {
        Remove-Item -LiteralPath $zip -Force
        Fail "the tauri-cli $tauriVersion download's SHA-256 is $got, not the pinned $want; it was removed"
    }
    Expand-Archive -LiteralPath $zip -DestinationPath $tools -Force
    Remove-Item -LiteralPath $zip -Force
    if (-not (Test-Path -LiteralPath $cargoTauri -PathType Leaf)) { Fail "the tauri-cli zip holds no cargo-tauri.exe" }
}
$env:PATH = "$tools;$env:PATH"
$line = (& $cargoTauri --version) -join ' '
if ($line -ne "tauri-cli $tauriVersion") { Fail "$cargoTauri says '$line', not 'tauri-cli $tauriVersion'" }
Write-Host $line

# --- the UI from the Linux build ---------------------------------------------------------------
$index = 'crates\lmgw-ui\dist\index.html'
if (-not (Test-Path -LiteralPath $index -PathType Leaf)) {
    Fail "$index is missing: the UI is Trunk's bundle from the Linux build's artifact"
}
if ((Get-Content -Raw -LiteralPath $index) -match 'placeholder') {
    Fail "$index is a check's placeholder, not Trunk's bundle: take crates\lmgw-ui\dist from the Linux build's artifact"
}

# --- the update feed ---------------------------------------------------------------------------
if ($env:GITLAB_CI -eq 'true') {
    # The numeric project id, as ci/build.sh: a moved project keeps its feed URL.
    $env:LMGW_UPDATE_MANIFEST_URL = "$($env:CI_API_V4_URL)/projects/$($env:CI_PROJECT_ID)/packages/generic/lmgw/latest/latest.json"
    if (-not $env:LMGW_UPDATE_DEPLOY_TOKEN) {
        Write-Host 'WARNING: LMGW_UPDATE_DEPLOY_TOKEN is not set: this build cannot read its own update feed (see .gitlab-ci.yml)'
    }
} elseif ($env:GITHUB_ACTIONS -eq 'true') {
    Remove-Item Env:LMGW_UPDATE_MANIFEST_URL, Env:LMGW_UPDATE_DEPLOY_TOKEN -ErrorAction SilentlyContinue
}
$feed = if ($env:LMGW_UPDATE_MANIFEST_URL) { $env:LMGW_UPDATE_MANIFEST_URL } else { 'public default' }
$token = if ($env:LMGW_UPDATE_DEPLOY_TOKEN) { 'yes' } else { 'no' }
Write-Host "Update feed baked in: $feed (deploy token: $token)"

# --- the version ------------------------------------------------------------------------------
function Set-FirstMatch([string]$Path, [string]$Pattern, [string]$Replacement) {
    $text = [IO.File]::ReadAllText((Resolve-Path -LiteralPath $Path))
    $re = [regex]::new($Pattern, [Text.RegularExpressions.RegexOptions]::Multiline)
    if (-not $re.IsMatch($text)) { Fail "no version line in $Path" }
    [IO.File]::WriteAllText((Resolve-Path -LiteralPath $Path), $re.Replace($text, $Replacement, 1))
}
Set-FirstMatch 'Cargo.toml' '^version = "[0-9]+\.[0-9]+\.[0-9]+(\+[0-9]+)?"' "version = `"$Version`""
Set-FirstMatch 'src-tauri\tauri.conf.json' '("version"\s*:\s*)"[0-9]+\.[0-9]+\.[0-9]+(\+[0-9]+)?"' "`${1}`"$Version`""

# --- the build ----------------------------------------------------------------------------------
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$metadata = (& cargo metadata --format-version 1 --no-deps 2>$null) -join "`n"
$ErrorActionPreference = $prev
if ($LASTEXITCODE -ne 0) { Fail "cargo metadata exited with $LASTEXITCODE" }
$target = ($metadata | ConvertFrom-Json).target_directory
$bundle = Join-Path $target 'release\bundle\nsis'
# Never an installer of an earlier build: the bundle dir and dist-windows start empty.
if (Test-Path -LiteralPath $bundle) { Remove-Item -LiteralPath $bundle -Recurse -Force }
if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force }
$started = Get-Date
Invoke-Native 'cargo' @('tauri', 'build', '--ci', '--bundles', 'nsis')
Write-Host ("cargo tauri build took {0:N0} s" -f ((Get-Date) - $started).TotalSeconds)

# The binary carries the stamped version (tauri-build's version resource, the same string the
# updater compares), never a stale build's.
$binary = Join-Path $target 'release\lmgw.exe'
$built = (Get-Item -LiteralPath $binary).VersionInfo.ProductVersion
if ($built -ne $Version) { Fail "$binary says version $built, not $Version" }

$name = "lmgw_${Version}_x64-setup.exe"
$installer = Join-Path $bundle $name
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    Get-ChildItem -LiteralPath $bundle -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $($_.Name)" }
    Fail "no $name was produced in $bundle"
}
New-Item -ItemType Directory -Force $OutDir | Out-Null
Copy-Item -LiteralPath $installer -Destination $OutDir
$sha = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer).Hash.ToLowerInvariant()
$size = (Get-Item -LiteralPath $installer).Length
$manifest = [ordered]@{ version = $Version; file = $name; size = $size; sha256 = $sha }
# ASCII: the name is lmgw_<version>_x64-setup.exe, and Windows PowerShell's UTF-8 writes a BOM.
$manifest | ConvertTo-Json | Set-Content -Encoding Ascii -LiteralPath (Join-Path $OutDir 'nsis.json')
Write-Host ("Built {0} ({1:N1} MiB, sha256 {2})" -f $name, ($size / 1MB), $sha)
