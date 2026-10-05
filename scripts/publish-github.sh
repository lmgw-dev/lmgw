#!/usr/bin/env bash
# Publish to the public GitHub repo as one squashed commit: a local ref's exact tree on top of the public tip.
#
# Usage: scripts/publish-github.sh [--ref REF] -F MSGFILE [--tag vX.Y.Z] [--dry-run]
#        scripts/publish-github.sh --approve TREE_SHA --note "WHO AUDITED" [--ack-scan]
#        scripts/publish-github.sh --install-guard
#
#   --ref REF         what to publish, default main: its tree, never its history
#   -F MSGFILE        the public commit's message (required unless only a tag is new); scanned
#   --tag vX.Y.Z      also tag the public commit (GitHub's release workflow builds from it). The
#                     local annotated tag vX.Y.Z (scripts/release.sh) must be on a commit with the
#                     published tree; its message becomes the public tag's, i.e. the release notes.
#                     A tree public already (the tip, or an earlier publish in publish-map) gets
#                     only the tag, on the public commit that carries it
#   --dry-run         everything except the push: audit files, scan, gate, the commit (and tag)
#                     object created locally and printed; exits 3 when the gate would refuse
#   --approve SHA     record that the diff for this tree (its full sha, as a run prints it) was
#                     audited; --ack-scan acknowledges every hit of that tree's scan
#   --install-guard   for another clone: add the github remote if it is missing, disable its push
#                     URL and its tag auto-follow, install scripts/pre-push-github.sh as pre-push
#
# The commit is `git commit-tree <ref>^{tree} -p github/main`: a tree copy, never a merge, so
# the private history never reaches GitHub. A run fetches github's main (read-only, no tags),
# writes target/publish/<tree12>.diff (the whole diff from the public tip), .binaries (the added
# or changed binary files) and .scan (scripts/publish_scan.py: secret shapes, every non-GitHub
# remote host, and the private denylist), then gates: the diff and scan must be the ones
# approved in target/publish/<tree sha>.approved, the messages must scan clean, and the author
# and committer e-mail must be GitHub no-reply addresses. Then it shows the commit, asks for
# the word "publish" on the terminal and pushes with LMGW_PUBLISH=1, which the pre-push hook
# requires. Each publish is recorded in .git/info/publish-map as "<ref sha> <public sha> <date>",
# and a ref older than one recorded there is refused: copying it would revert the public repo.
#
# The denylist is private and lives outside the repo: LMGW_PUBLISH_DENYLIST, else
# ${XDG_CONFIG_HOME:-~/.config}/lmgw/publish-denylist, one case-insensitive regular expression
# per line, '#' comments (format and examples: scripts/publish_scan.py). LMGW_PUBLISH_URL
# overrides the push URL (tests point it at a local bare repo).
set -euo pipefail

REMOTE=github
PUBLIC_URL=git@github.com:lmgw-dev/lmgw.git
URL="${LMGW_PUBLISH_URL:-$PUBLIC_URL}"
OUT=target/publish
DENYLIST="${LMGW_PUBLISH_DENYLIST:-${XDG_CONFIG_HOME:-$HOME/.config}/lmgw/publish-denylist}"

die() { echo "publish-github: $*" >&2; exit 1; }
sha256() { sha256sum < "$1" | cut -d' ' -f1; }

ref=main msg="" tag="" dry=false approve="" note="" ack=false guard=false
while (($#)); do
    case "$1" in
        --ref) ref="${2:?--ref needs a ref}"; shift ;;
        -F) msg="${2:?-F needs a file}"; shift ;;
        --tag) tag="${2:?--tag needs vX.Y.Z}"; shift ;;
        --dry-run) dry=true ;;
        --approve) approve="${2:?--approve needs the tree sha}"; shift ;;
        --note) note="${2:?--note needs text}"; shift ;;
        --ack-scan) ack=true ;;
        --install-guard) guard=true ;;
        -h | --help) sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done
# Paths given relative to where the owner stands, before moving to the repo root.
if [[ -n "$msg" ]]; then msg="$(realpath -e "$msg")" || die "no message file $msg"; fi
DENYLIST="$(realpath -m "$DENYLIST")"
cd "$(dirname "$0")/.."

# --- --install-guard -----------------------------------------------------------------------------
if $guard; then
    git remote get-url "$REMOTE" >/dev/null 2>&1 || git remote add "$REMOTE" "$PUBLIC_URL"
    git config "remote.$REMOTE.pushurl" DISABLED-publish-with-scripts/publish-github.sh
    # A public tag sits on the public commit, a local one on the private commit of the same
    # name: auto-following github's tags would clash with the local ones.
    git config "remote.$REMOTE.tagOpt" --no-tags
    hooks="$(git rev-parse --git-path hooks)"
    mkdir -p "$hooks"
    if [[ -e "$hooks/pre-push" ]] && ! cmp -s scripts/pre-push-github.sh "$hooks/pre-push"; then
        die "$hooks/pre-push exists and differs from scripts/pre-push-github.sh: compare them and remove yours first"
    fi
    install -m 755 scripts/pre-push-github.sh "$hooks/pre-push"
    echo "guard installed: $REMOTE's push URL disabled, its tags not auto-followed, $hooks/pre-push in place"
    exit 0
fi

# --- --approve -----------------------------------------------------------------------------------
if [[ -n "$approve" ]]; then
    [[ "$approve" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] \
        || die "--approve takes the full tree sha a run printed, not an abbreviation"
    [[ "$(git cat-file -t "$approve" 2>/dev/null)" == tree ]] || die "$approve is no tree in this repo"
    [[ -n "${note// /}" ]] || die "--approve needs --note \"who audited the diff\""
    t12="${approve:0:12}"
    [[ -f "$OUT/$t12.diff" && -f "$OUT/$t12.scan" && -f "$OUT/$t12.tree" ]] \
        || die "no audit files for this tree in $OUT: run a --dry-run first"
    [[ "$(cat "$OUT/$t12.tree")" == "$approve" ]] || die "$OUT/$t12.* belong to another tree"
    hits="$(grep -c . "$OUT/$t12.scan" || true)"
    if ((hits > 0)) && ! $ack; then
        die "the scan has $hits hit(s): read $OUT/$t12.scan, and approve with --ack-scan once every one is fine"
    fi
    {
        echo "tree $approve"
        echo "diff-sha256 $(sha256 "$OUT/$t12.diff")"
        echo "scan-sha256 $(sha256 "$OUT/$t12.scan")"
        echo "scan-hits $hits"
        echo "note $note"
        echo "date $(date -Iseconds)"
    } > "$OUT/$approve.approved"
    echo "approved: $OUT/$approve.approved (scan hits acknowledged: $hits)"
    exit 0
fi

# --- a publish, or its dry run -------------------------------------------------------------------
if [[ -n "$tag" ]]; then
    [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--tag takes vX.Y.Z, not $tag"
fi
[[ -f "$DENYLIST" ]] \
    || die "no private denylist at $DENYLIST (one case-insensitive regex per line; see scripts/publish_scan.py)"
case "$(realpath "$DENYLIST")" in
    "$(pwd -P)"/*) die "the denylist $DENYLIST is inside the repo: it holds private words, keep it outside" ;;
esac

git fetch --quiet --no-tags "$REMOTE" "+refs/heads/main:refs/remotes/$REMOTE/main"
public="$(git rev-parse --verify -q "refs/remotes/$REMOTE/main^{commit}")" \
    || die "$REMOTE has no main: this script only ever extends the public history"
src="$(git rev-parse --verify -q "$ref^{commit}")" || die "no commit $ref"
tree="$(git rev-parse "$src^{tree}")"
t12="${tree:0:12}"
map="$(git rev-parse --git-common-dir)/info/publish-map"
# A public commit that carries this tree already: the tip, or (for a tag) an earlier publish.
already=""
if [[ "$tree" == "$(git rev-parse "$public^{tree}")" ]]; then
    already="$public"
elif [[ -n "$tag" && -f "$map" ]]; then
    while read -r _ pub _; do
        if [[ "$(git rev-parse -q --verify "$pub^{tree}" 2>/dev/null)" == "$tree" ]] \
            && git merge-base --is-ancestor "$pub" "$public" 2>/dev/null; then
            already="$pub"
            break
        fi
    done < "$map"
fi
if [[ -z "$already" && -f "$map" ]]; then
    # Copying an older state onto the public tip would revert everything published since.
    while read -r done_src _; do
        if [[ "$done_src" != "$src" ]] && git merge-base --is-ancestor "$src" "$done_src" 2>/dev/null; then
            die "$ref is older than $done_src, which is public already: publishing it would revert the public repo"
        fi
    done < "$map"
fi
fresh=true
[[ -n "$already" ]] && fresh=false
if ! $fresh && [[ -z "$tag" ]]; then
    echo "nothing to publish: $ref's tree is the public tip's already ($public)"
    exit 0
fi
if $fresh; then
    [[ -n "$msg" ]] || die "-F <message file> is required: the public commit's message is part of what is audited"
    [[ -s "$msg" ]] || die "$msg is empty"
fi
mkdir -p "$OUT"
reasons=()

# The tag: a local annotated tag on the very tree being published, whose message is the notes.
tag_msg=""
if [[ -n "$tag" ]]; then
    [[ "$(git cat-file -t "refs/tags/$tag" 2>/dev/null)" == tag ]] \
        || die "$tag must be a local annotated tag (scripts/release.sh makes it): its message is the release notes"
    [[ "$(git rev-parse "refs/tags/$tag^{tree}")" == "$tree" ]] \
        || die "$tag's tree is not $ref's: publish the tagged commit itself (--ref $tag)"
    tag_msg="$OUT/$t12.tag-message"
    git cat-file tag "refs/tags/$tag" | sed '1,/^$/d' | sed '/^-----BEGIN [A-Z]* SIGNATURE-----$/,$d' > "$tag_msg"
    [[ -s "$tag_msg" ]] || die "$tag has an empty message: the release would have no notes"
fi

# Who the commit and the tag name. Only GitHub's no-reply addresses are published.
for who in AUTHOR COMMITTER; do
    ident="$(git var "GIT_${who}_IDENT")"
    email="${ident#*<}" email="${email%%>*}"
    if [[ "$email" != *@users.noreply.github.com ]]; then
        reasons+=("the ${who,,} e-mail <$email> would be published: set a GitHub no-reply address (git config user.email <id>+<name>@users.noreply.github.com)")
    fi
done

# The messages: no hit is acknowledgeable, they are the publisher's own words.
msg_scan="$OUT/$t12.message-scan"
: > "$msg_scan"
if [[ -n "$msg" ]]; then
    python3 scripts/publish_scan.py text --label message --denylist "$DENYLIST" "$msg" > "$msg_scan"
fi
if [[ -n "$tag_msg" ]]; then
    python3 scripts/publish_scan.py text --label tag-message --denylist "$DENYLIST" "$tag_msg" >> "$msg_scan"
fi
if [[ -s "$msg_scan" ]]; then
    reasons+=("the message scan has $(grep -c . "$msg_scan") hit(s), see $msg_scan: reword the message")
fi

if ! $fresh; then
    # Published already; only the tag is new, on the public commit that carries the tree.
    target="$already"
    echo "the tree is public already ($already): tagging that commit"
else
    diff="$OUT/$t12.diff" scan="$OUT/$t12.scan"
    echo "$tree" > "$OUT/$t12.tree"
    git -c core.quotePath=false diff --no-color --no-ext-diff --no-textconv --find-renames \
        "$public^{tree}" "$tree" > "$diff"
    python3 scripts/publish_scan.py tree --base "$public^{tree}" --tree "$tree" --diff "$diff" \
        --binaries-out "$OUT/$t12.binaries" --denylist "$DENYLIST" > "$scan"
    echo "audit: $diff ($(git diff --shortstat "$public^{tree}" "$tree" | sed 's/^ //'))"
    echo "       $OUT/$t12.binaries ($(grep -c . "$OUT/$t12.binaries" || true) binary file(s))"
    echo "       $scan ($(grep -c . "$scan" || true) hit(s))"
    echo "tree:  $tree"
    approval="$OUT/$tree.approved"
    if [[ ! -f "$approval" ]]; then
        reasons+=("no approval for tree $tree: audit $diff, then: scripts/publish-github.sh --approve $tree --note \"who audited\" [--ack-scan]")
    elif [[ "$(sed -n 's/^tree //p' "$approval")" != "$tree" ]]; then
        reasons+=("$approval names another tree")
    elif [[ "$(sed -n 's/^diff-sha256 //p' "$approval")" != "$(sha256 "$diff")" ]]; then
        reasons+=("the diff is not the one approved (the public tip moved?): audit $diff again and re-approve")
    elif [[ "$(sed -n 's/^scan-sha256 //p' "$approval")" != "$(sha256 "$scan")" ]]; then
        reasons+=("the scan is not the one approved (new hits, or a changed denylist): read $scan and re-approve")
    fi
    target="$(git commit-tree "$tree" -p "$public" -F "$msg")"
fi

tag_obj=""
if [[ -n "$tag" ]]; then
    tag_obj="$({
        printf 'object %s\ntype commit\ntag %s\ntagger %s\n\n' "$target" "$tag" "$(git var GIT_COMMITTER_IDENT)"
        cat "$tag_msg"
    } | git mktag)"
fi

refs=()
$fresh && refs+=("$target:refs/heads/main")
[[ -n "$tag_obj" ]] && refs+=("$tag_obj:refs/tags/$tag")
echo
git --no-pager show --stat --format=fuller "$target"
[[ -n "$tag_obj" ]] && echo && git --no-pager cat-file tag "$tag_obj"
echo
echo "push target: $URL"
for r in "${refs[@]}"; do echo "  $r"; done

if ((${#reasons[@]})); then
    echo
    echo "gate: $($dry && echo "would refuse" || echo "refused"):"
    for r in "${reasons[@]}"; do echo "  - $r"; done
    exit 3
fi
if $dry; then
    echo
    echo "gate: open. Dry run: nothing pushed (the objects above stay unreferenced)."
    exit 0
fi

[[ -t 0 ]] || die "the confirmation is read from a terminal: run this in one"
read -r -p "type 'publish' to push the above to $URL: " answer
[[ "$answer" == publish ]] || { echo "not published"; exit 1; }
LMGW_PUBLISH=1 git push --atomic "$URL" "${refs[@]}"
mkdir -p "$(dirname "$map")"
echo "$src $target $(date -Iseconds)${tag:+ $tag}" >> "$map"
echo "published: $target${tag:+ (tag $tag)}; recorded in $map"
