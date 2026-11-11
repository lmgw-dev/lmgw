#!/usr/bin/env bash
# Usage: LMGW_WIN_HOST=user@host [LMGW_WIN_DIR=dir] scripts/windows-remote-check.sh [command...]
#
# Runs the Windows compile check (Windows design, WP1) on a Windows machine over ssh, from this
# Linux checkout: the files `git ls-files -co --exclude-standard` lists (so uncommitted and
# untracked edits count) go over as one tar, are mirrored into LMGW_WIN_DIR with robocopy (only
# changed files are written, so the others' mtimes and cargo's incremental state survive; files
# gone from the tree are removed; `target\` is kept), and the command runs there. A file robocopy
# writes gets the time of the copy as its mtime, not this checkout's: an edit made while an
# earlier remote build ran is older than that build's outputs, and cargo took it as built. Its output streams
# back and this script exits with its status, after a last line of its own:
# `windows-remote-check: exit status <n>`.
#
#   LMGW_WIN_HOST  required, the ssh destination (user@host or an ssh_config Host).
#   LMGW_WIN_DIR   the remote checkout, relative to the remote user's home or absolute
#                  (default: lmgw-win). A sibling `<dir>.sync` holds the transfer.
#   command        replaces the default `cargo clippy --workspace --all-targets -- -D warnings`,
#                  e.g. `cargo check --workspace`. Each argument is passed as one word.
#
# The remote side needs ssh (whatever its default shell: Windows OpenSSH's is cmd.exe; every remote
# command here goes through `powershell -EncodedCommand`, so it does not matter), Windows
# PowerShell 5.1, tar.exe and robocopy (both ship with Windows 10 and later), and Rust's MSVC toolchain on PATH.
# crates/lmgw-ui/dist/ is gitignored (Trunk's bundle) but rust-embed needs the folder to exist,
# so an index.html placeholder is seeded there when it is missing: a check never reads it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

host=${LMGW_WIN_HOST:-}
if [[ -z "$host" ]]; then
  echo "windows-remote-check: set LMGW_WIN_HOST to the Windows machine's ssh destination" >&2
  exit 2
fi
dir=${LMGW_WIN_DIR:-lmgw-win}
if [[ "$dir" == *"'"* ]]; then
  echo "windows-remote-check: LMGW_WIN_DIR must not contain a single quote" >&2
  exit 2
fi
if (($# == 0)); then
  set -- cargo clippy --workspace --all-targets -- -D warnings
fi

work=target/windows-remote-check
mkdir -p "$work"

# Tracked files deleted in the working tree are still listed by `ls-files -c`; tar only what exists.
git ls-files -z -co --exclude-standard |
  while IFS= read -r -d '' f; do if [[ -e "$f" ]]; then printf '%s\0' "$f"; fi; done |
  tar --null -T - -cf "$work/src.tar"

# PowerShell single-quoted literal: a quote is written twice.
psq() { printf "'%s'" "${1//\'/\'\'}"; }
cmd_words=()
for a in "$@"; do cmd_words+=("$(psq "$a")"); done
cmd_list=$(
  IFS=,
  echo "${cmd_words[*]}"
)

cat >"$work/run.ps1" <<EOF
# Written by scripts/windows-remote-check.sh; runs in Windows PowerShell 5.1.
\$ErrorActionPreference = 'Stop'
\$ProgressPreference = 'SilentlyContinue'
\$dir = $(psq "$dir")
\$sync = "\$dir.sync"
\$stage = Join-Path \$sync 'tree'
if (Test-Path \$stage) { Remove-Item -Recurse -Force \$stage }
New-Item -ItemType Directory -Force \$stage | Out-Null
New-Item -ItemType Directory -Force \$dir | Out-Null
tar.exe -xf (Join-Path \$sync 'src.tar') -C \$stage
if (\$LASTEXITCODE -ne 0) { Write-Error "tar.exe failed with \$LASTEXITCODE" }
# /MIR mirrors the tree; /XD keeps cargo's target dir and Trunk's dist dir out of the mirror, so
# neither is copied nor purged. Exit codes below 8 mean success (bit flags for copied/extra).
# /FP /NS /NC list each file written by its source path and each purged one by its destination's.
\$stageFull = (Resolve-Path \$stage).Path
\$dirFull = (Resolve-Path \$dir).Path
\$listed = robocopy \$stage \$dir /MIR /XD (Join-Path \$dirFull 'target') (Join-Path \$dirFull 'crates\lmgw-ui\dist') /NDL /NJH /NJS /NP /NS /NC /FP
if (\$LASTEXITCODE -ge 8) { Write-Error "robocopy failed with \$LASTEXITCODE" }
\$now = Get-Date
foreach (\$line in \$listed) {
  \$p = "\$line".Trim()
  if (-not \$p.StartsWith(\$stageFull, [StringComparison]::OrdinalIgnoreCase)) { continue }
  \$dest = \$dirFull + \$p.Substring(\$stageFull.Length)
  if (Test-Path -LiteralPath \$dest -PathType Leaf) { (Get-Item -LiteralPath \$dest).LastWriteTime = \$now }
}
Remove-Item -Recurse -Force \$stage
\$dist = Join-Path \$dir 'crates\lmgw-ui\dist'
if (-not (Test-Path (Join-Path \$dist 'index.html'))) {
  New-Item -ItemType Directory -Force \$dist | Out-Null
  Set-Content -Path (Join-Path \$dist 'index.html') -Value '<!-- placeholder: a check needs only the folder (scripts/windows-remote-check.sh) -->'
}
Set-Location \$dir
\$cmd = @($cmd_list)
Write-Output "==> \$(\$cmd -join ' ')  (in \$((Get-Location).Path))"
\$ErrorActionPreference = 'Continue'
if (\$cmd.Count -gt 1) { & \$cmd[0] \$cmd[1..(\$cmd.Count - 1)] } else { & \$cmd[0] }
exit \$LASTEXITCODE
EOF

# Runs a PowerShell snippet on the remote through -EncodedCommand (base64 of UTF-16LE): no quoting
# of the snippet survives or depends on the remote's default shell (cmd.exe, or PowerShell).
# Progress records off: over a redirected stdout PowerShell sends them as `#< CLIXML` blocks
# ("Preparing modules for first use"), which buried the output in XML.
remote_ps() {
  local enc
  enc=$(printf '%s' "\$ProgressPreference = 'SilentlyContinue'; $1" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)
  ssh -o BatchMode=yes "$host" "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $enc"
}

sync_dir="$dir.sync"
remote_ps "New-Item -ItemType Directory -Force $(psq "$sync_dir") | Out-Null"
scp -q -o BatchMode=yes "$work/src.tar" "$work/run.ps1" "$host:$sync_dir/"
# powershell ends with 1 for a failed native command unless told the code, so pass run.ps1's on.
rc=0
remote_ps "& $(psq "$sync_dir/run.ps1"); exit \$LASTEXITCODE" || rc=$?
# On a line of its own, whatever the remote output ended with.
printf '\nwindows-remote-check: exit status %s\n' "$rc"
exit "$rc"
