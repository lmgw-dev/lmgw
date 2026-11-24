//! The C allocator's two thresholds, fixed at start (document-conversion
//! design §14, performance round).
//!
//! glibc's `malloc` raises its mmap and trim thresholds each time a large
//! block is freed (up to 32 MiB and 64 MiB on 64-bit), so after a few large
//! conversions every later block of up to 32 MiB comes from a thread's
//! arena, and memory freed there stays with the process: measured
//! 2026-11-19 (glibc 2.44, release build) after six large conversions, the
//! gateway idled at 511 to 632 MiB resident with 7 to 8 MiB of it in use
//! (malloc's own count; the rest free inside its 32 arenas). Fixed at 1 MiB,
//! a block of 1 MiB or more is mapped on its own and given back when it is
//! freed, and an arena gives back free memory at its end past 1 MiB: the
//! same sequence idled at 330 to 526 MiB, no conversion measurably slower.
//!
//! What stays is free memory between live blocks of the arenas, which only
//! `malloc_trim` returns (to 85 MiB); a trim after a large request is not
//! done yet.
//!
//! **Process-wide** (performance round review S6): the thresholds are the
//! whole gateway's, not the conversions' alone, so every block of 1 MiB or
//! more anywhere (a proxied chat body or stream, image and audio buffers,
//! the layout model's tensors, the UI's assets) is mapped and unmapped on
//! its own. Measured 2026-11-19 on the release build with and without them
//! (`scripts/convert-perf/no-mallopt.c` keeps glibc's own): the layout
//! model's stage and a 64 MiB streamed chat answer through a loopback
//! upstream; the figures are in design §14's performance round review
//! block, and `docs/documents.md` says it for owners.

/// The threshold for both: 1 MiB.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const THRESHOLD_BYTES: libc::c_int = 1 << 20;

/// Fix the thresholds (module doc), at the gateway's start
/// (`AppState::init_started_with_host`, where the desktop app's,
/// the server image's and the headless example's starts meet): they are the process's, so the arenas made before
/// follow them too. An explicit `MALLOC_MMAP_THRESHOLD_` or
/// `MALLOC_TRIM_THRESHOLD_` in the environment wins.
pub fn tune() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let set = |var: &str, param: libc::c_int| {
            if std::env::var_os(var).is_none() {
                // SAFETY: `mallopt` with a documented parameter and value.
                unsafe { libc::mallopt(param, THRESHOLD_BYTES) };
            }
        };
        set("MALLOC_MMAP_THRESHOLD_", libc::M_MMAP_THRESHOLD);
        set("MALLOC_TRIM_THRESHOLD_", libc::M_TRIM_THRESHOLD);
    }
}
