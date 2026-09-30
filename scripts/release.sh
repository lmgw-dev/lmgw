#!/usr/bin/env bash
# Cut a release: set the version, commit it, and make the annotated tag v<X.Y.Z>. Pushes nothing.
#
# Usage: scripts/release.sh <X.Y.Z> [notes-file]
#
# Sets the version in Cargo.toml ([workspace.package]), src-tauri/tauri.conf.json and
# Cargo.lock, commits "release X.Y.Z" on main and tags that commit. The tag's message becomes
# the GitHub release notes: the notes file when given, else git opens your editor. When the tree
# already carries the version (the first release), it only tags. Then publish:
#   git push origin main vX.Y.Z    GitLab: a private build X.Y.Z+<pipeline> of main
#   git push github main vX.Y.Z    GitHub: the public release (.github/workflows/release.yml)
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: scripts/release.sh <X.Y.Z> [notes-file]}"
notes="${2:-}"
die() { echo "$*" >&2; exit 1; }

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not an X.Y.Z version: $version"
[[ "$(git branch --show-current)" == main ]] || die "releases are cut from main"
[[ -z "$(git status --porcelain --untracked-files=no)" ]] || die "commit or stash your changes first"
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then die "tag v$version already exists"; fi
[[ -z "$notes" || -f "$notes" ]] || die "no notes file $notes"

current="$(grep -m1 -oE '^version = "[0-9]+\.[0-9]+\.[0-9]+"' Cargo.toml | grep -oE '[0-9.]+')"
if [[ "$current" != "$version" ]]; then
    [[ "$(printf '%s\n' "$current" "$version" | sort -V | tail -n1)" == "$version" ]] \
        || die "$version is older than the current $current"
    cur="${current//./\\.}"
    sed -i -E "s/^version = \"$cur\"/version = \"$version\"/" Cargo.toml
    sed -i -E "s/(\"version\"[[:space:]]*:[[:space:]]*)\"$cur\"/\1\"$version\"/" src-tauri/tauri.conf.json
    cargo update --workspace --offline --quiet
    git commit -q -m "release $version" -- Cargo.toml Cargo.lock src-tauri/tauri.conf.json
fi

if [[ -n "$notes" ]]; then
    git tag -a "v$version" -F "$notes"
else
    git tag -a "v$version"
fi

echo "tagged v$version at $(git rev-parse --short HEAD). Publish with:"
echo "  git push origin main v$version"
echo "  git push github main v$version"
