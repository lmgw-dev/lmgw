# Instructions for coding agents

## Long commands (builds, the gate, the Windows check)

Long commands are the full gate (`bash ci/check.sh`), release builds, the full
`cargo nextest run --workspace` and `scripts/windows-remote-check.sh`.

- Before landing, the check is `scripts/long-run.sh target/land.log scripts/land-check.sh`: it runs
  the full gate and the Windows compile check (needs `LMGW_WIN_HOST` in the environment; without
  it the check is reported SKIPPED and fails unless `--no-windows`) at the same time and ends with
  a gate line, a windows line and `land-check: exit status N`. Run the code review before it, not
  after (a review finding costs a full gate each when the gate ran first); iterate with
  `bash ci/check.sh --changed` (only what the diff touches).
- Run every cargo command through `scripts/build-slot.sh` (its header says why).
- Start a long command as ONE background command through `scripts/long-run.sh <log> …`, which keeps
  the output in `<log>`, the pid in `<log>.pid` and the exit status in `<log>.exit`:
  `scripts/long-run.sh target/ci.log scripts/build-slot.sh bash ci/check.sh`
- Then either end your turn and wait for the harness's notification that the command exited, or, if
  you poll, poll ONLY with `scripts/long-wait.sh <log>`, one call at a time. Each call blocks up to
  its window and reports measured progress. Never poll with sleep, tail, ps, pgrep or log reads of
  your own.
- Read its last line:
  - `running: making progress` or `running: waiting for a build slot, holder making progress`:
    call it again.
  - exit 0 with `finished: exit status N`: read the log.
  - `NO progress` on two calls in a row: look at the process tree
    (`ps -o pid,etime,time,cmd --ppid <pid from <log>.pid>`) and the log, and say what the command
    waits on.
- Never start a second copy of a command that is still running. Nothing of yours may be left
  running when you report.
