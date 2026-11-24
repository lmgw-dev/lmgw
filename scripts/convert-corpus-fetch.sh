#!/usr/bin/env bash
# Usage: scripts/convert-corpus-fetch.sh [dest]   fetch docling's tests/data at the pinned commit (default dest: ~/.local/share/lmgw-corpora/docling-tests/<commit>)
#
# The document-conversion measurement corpus (design 2026-11-18 §10.1): a sparse checkout of
# `tests/data` from docling-project/docling at DOCLING_COMMIT. The repository is MIT, but the
# folder carries no licence note of its own and holds third-party documents, so the files are an
# input kept outside the repo and outside target/; they are never committed. Re-running the script
# restores the same tree. It only reads from GitHub; it sends nothing.
#
# The pin is docling release v2.150.1. Raising it changes the corpus, so measurements taken on
# different pins are not comparable.
set -euo pipefail

DOCLING_REPO="https://github.com/docling-project/docling.git"
DOCLING_COMMIT="5e600e8d37a11f70b62f795ea1e3ce3a4052e551"

if [ "$#" -gt 1 ]; then sed -n '2p' "$0" >&2; exit 2; fi
DEST="${1:-$HOME/.local/share/lmgw-corpora/docling-tests/$DOCLING_COMMIT}"

if [ -d "$DEST/.git" ]; then
    have="$(git -C "$DEST" rev-parse HEAD)"
    if [ "$have" = "$DOCLING_COMMIT" ] && [ -d "$DEST/tests/data" ]; then
        echo "corpus already at $DOCLING_COMMIT: $DEST/tests/data"
        exit 0
    fi
    echo "error: $DEST holds another checkout ($have); pick another dest or move it away" >&2
    exit 1
fi
if [ -e "$DEST" ] && [ -n "$(ls -A "$DEST" 2>/dev/null)" ]; then
    echo "error: $DEST exists and is not empty" >&2
    exit 1
fi

mkdir -p "$DEST"
git -C "$DEST" init -q
git -C "$DEST" remote add origin "$DOCLING_REPO"
git -C "$DEST" config core.sparseCheckout true
git -C "$DEST" sparse-checkout set --no-cone /tests/data/
# Blobless fetch of exactly the pinned commit; the sparse checkout pulls only tests/data's blobs.
git -C "$DEST" fetch -q --depth 1 --filter=blob:none origin "$DOCLING_COMMIT"
git -C "$DEST" checkout -q --detach FETCH_HEAD

files="$(find "$DEST/tests/data" -type f | wc -l)"
size="$(du -sh "$DEST/tests/data" | cut -f1)"
echo "corpus at $DOCLING_COMMIT: $DEST/tests/data ($files files, $size)"
