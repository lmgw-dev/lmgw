#!/usr/bin/env bash
# pre-push guard for the public GitHub repo; scripts/publish-github.sh --install-guard installs it
# as .git/hooks/pre-push. Two rules for any push to a github.com URL:
# 1. Nothing goes there by accident: only scripts/publish-github.sh pushes, with LMGW_PUBLISH=1
#    set. GitHub gets one squashed commit per publish; the granular history stays private. The
#    `github` remote's pushurl is disabled as well, so `git push github` fails before this runs.
# 2. Nothing that carries the archived pre-public history may go there. Its root commit is below.
#    Checked without replace refs: a push sends the real ancestry, and a local graft (git replace)
#    must neither hide it nor make every push look tainted.
set -euo pipefail
remote="$1" url="$2"
case "$url" in *github.com*) ;; *) exit 0 ;; esac
if [[ "${LMGW_PUBLISH:-}" != "1" ]]; then
    echo "pre-push: refusing to push to GitHub ($url). Publish with scripts/publish-github.sh." >&2
    exit 1
fi
old_root=27871c7e621eec8795e34f811d16e7812d68a019
git --no-replace-objects cat-file -e "$old_root^{commit}" 2>/dev/null || exit 0   # not here: nothing to leak
zero=0000000000000000000000000000000000000000
while read -r local_ref local_sha _ _; do
    [[ "$local_sha" == "$zero" ]] && continue
    if git --no-replace-objects merge-base --is-ancestor "$old_root" "$local_sha"; then
        echo "pre-push: $local_ref carries the pre-public history; refusing to push it to $remote" >&2
        exit 1
    fi
done
