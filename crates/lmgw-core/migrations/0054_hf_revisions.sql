-- What a tracked Hugging Face download asked the hub for, and what it got.
--
-- An audio.cpp spec may pin a package's repo to a commit, and an audio
-- catalog download follows that pin unless `audio.catalog_revision` says
-- `latest`. So a download no longer always means `main`, and the row has to
-- say which revision it fetches (the transfer, a retry and a resume read it
-- from here) and which commit the file on disk came from.
--
-- `requested_revision` is the revision the download asks for: `main`, or the
-- commit a spec pins. Written when the row is queued; NULL on a row from
-- before this column, which was a `main` download.
--
-- `resolved_commit` is the commit the file on disk came from: the hub's
-- `X-Repo-Commit` on the resolve response, else the requested revision when
-- that is itself a commit (the bytes are that commit's by definition), else
-- NULL. Never guessed: NULL means unknown, which is what every row from
-- before this column is. Written when a transfer finishes, so a re-download
-- that fails leaves the commit of the file still on disk.
ALTER TABLE hf_models ADD COLUMN requested_revision TEXT;
ALTER TABLE hf_models ADD COLUMN resolved_commit    TEXT;
