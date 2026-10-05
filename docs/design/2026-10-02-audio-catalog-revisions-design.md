# Audio catalog: spec-pinned Hugging Face revisions

Status: built 2026-10-02.

## 1. Facts

- audio.cpp's model specs may pin a package's Hugging Face repo to a revision
  (`download.revision`): a commit, or `main`. A package's own `download` may be
  null and inherit `package_defaults.download`. In the catalog of 2026-10-02, 15
  of 339 packages pin a commit, all in third-party repos (Confucius4,
  Echo-TTS, Fun-ASR-Nano, LFM2.5-Audio, the MMS aligner, SenseVoice).
  audio.cpp's own `audio-cpp/audio.cpp-gguf` says `main`.
- A pin is the spec author's compatibility promise. A third-party repo can
  later re-upload a file in a layout the engine cannot load, and the error that
  produces names a tensor, not the cause.
- Before this, lmgw ignored the revision: catalog downloads and the
  published-files listing always took `main`.
- The hub names the commit a revision resolves to in `X-Repo-Commit`, on its
  own `resolve` response. For a file in LFS (every GGUF) that response is a
  302 to a CDN whose 200 does not repeat the header; a small file's 307 to
  `/api/resolve-cache/…` carries it on both hops (checked 2026-10-02 against
  `openai-community/gpt2`). A 404 echoes the requested revision in the header,
  so it says nothing there. An unknown revision answers a tree listing with
  `404` and `x-error-code: RevisionNotFound`.

## 2. Decisions

1. **A setting**, `audio.catalog_revision`: `pinned` (the default, also for
   existing installs) or `latest`. Settings → Runtimes → Audio → *Catalog
   downloads*; `lmgw__settings_set audio_catalog_revision`; read back under
   `audio` in `lmgw__settings` and `/api/settings-full`. It is not part of the
   audio class's container definition, so saving it carries no restart note.
2. **Download at the pin.** Under `pinned`, a package whose spec revision is a
   full commit hash fetches every file at that commit
   (`resolve/<commit>/<file>`, the listing at `tree/<commit>`). A branch or tag
   name is not a pin; it and `latest` take `main`.
3. **The published-files listing reads the revision the download takes.** A
   refresh lists every repo at `main` and each pinned repo at its pin too, so
   "not published" is true in both modes without a refresh in between. The
   snapshot keeps a pin's listing under `repo@commit`; `main` stays under the
   repo, as before.
4. **Visible.** A pinned package shows "pinned to abc1234 by the spec", or
   "spec pins abc1234 · taking latest" under `latest`. A spec revision that
   is not a full commit (a tag, a short hash) shows "spec names v1.0 · lmgw
   takes main", so that fallback is not silent either. `lmgw__audio_catalog`
   `list` carries `pinned_commit` and `pin_followed`.
5. **Recorded.** Migration 0054 adds `hf_models.requested_revision` (what the
   download asks for) and `resolved_commit` (what the file on disk came from)
   for every download row, any target. The commit is the hub's
   `X-Repo-Commit`, read from its own hops: the transfer takes every redirect
   that stays on the hub's origin without following (a renamed repo's
   relative redirect, then the new name's), reading the header on each, and
   hands the first `Location` off the hub to the following client, sending
   the token only to the hub's own origin. A finished transfer also writes
   back the revision it fetched. Without the header it is the requested
   revision when that is a commit (the bytes are that commit's by definition),
   else unknown. A row from before the migration is unknown. Shown in the
   Downloads table, in the catalog package's chip title (`downloaded_from`)
   and in `lmgw__hf_downloads`.
6. **Update checks** (`lmgw__hf_set check_updates`) and re-downloads (update,
   retry) take the revision a row tracks now. Under `pinned` that is the
   revision the cached catalog's spec takes the row's file at **today**: a
   pin the spec moved is compared against and fetched, a row taken at `main`
   (before pins were followed, or under `latest`) joins the pin, and a spec
   that dropped its pin takes `main`. A file the catalog does not ship keeps
   its own pin, every other row `main`. Under `latest` everything is `main`.
   So no check offers an update the spec does not endorse. A catalog download
   under `pinned` (install, complete install, or a sibling package that
   shares files) takes along the package's installed files from another
   revision than the pin whose bytes the pin changed, and its message names
   them, so one package does not end up from mixed commits either. Each is
   asked first (a HEAD at the pin, all of them at once, so a hub that does
   not answer costs one timeout): one whose ETag there is the one its row
   recorded is recorded at the pin, as the update check records it (an
   `update_available` flag goes back to `done`), instead of fetched again,
   and the message names it too; one the hub names no ETag for is fetched,
   since "cannot tell" is not "the same". A row flagged under one setting whose
   file matches the remote at the revision it tracks under the other goes
   back to `done`. The catalog chip's title says when files on disk are from
   a revision other than the one the spec names now. A file byte-identical at
   the revision it tracks now (same ETag: a pin the spec moved without
   touching this file) is that revision's file as well, so the check records
   that revision on the row (and the commit, when it is one) and the chip
   calls it the pinned one, rather than promising an update that never
   comes.
7. **No silent fallback.** A pin the hub does not have fails the download, at
   queue time (the listing) or at transfer time (the file), with a sentence
   naming the pin and the setting that takes the latest instead. `main` is
   never fetched in its place. The sentence is for a missing pin only (404,
   410, or `x-error-code` `RevisionNotFound`/`EntryNotFound`): a rate limit
   or a server error fails with its status alone, since giving up the pin
   would not help.

## 3. Where it lives

- `crates/lmgw-core/src/audio/pins.rs`: what a pin is, the revision a download
  and an update check take, the sentences.
- `crates/lmgw-core/src/hf/resolve.rs`: the resolve URL at a revision, and the
  GET that reads `X-Repo-Commit` before following the redirect.
- `crates/lmgw-core/src/audio/published.rs`: listings keyed by repo and
  revision.
- `crates/lmgw-core/tests/it/audio_catalog_pins.rs`: the contract end to end
  against a mock hub and a mock CDN.

## 4. Left as they are

- The Hugging Face wizard (`lmgw__hf_add`) and image recipes download `main`,
  recorded as such.
