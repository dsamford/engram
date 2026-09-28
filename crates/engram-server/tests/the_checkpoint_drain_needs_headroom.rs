//! `CALL engram.checkpoint()` runs its derived drain (refresh, warm, persist)
//! only below half the memory ceiling.
//!
//! On 2026-09-27 at SF10 (a 140 GiB ceiling, 143,360 MiB) the four drains that
//! completed started at 53-64 GB of resident set; the one that followed a
//! ~160k-message delete started at ~98 GB and took the process to the OOM
//! killer. The predicate is pinned on those figures, and on the case with no
//! ceiling configured, where nothing changes.

use engram_server::derived_drain_has_headroom;

const CEILING_MB: u64 = 143_360;

#[test]
fn the_drains_that_completed_still_run() {
    for rss_mb in [53_000, 60_000, 64_000] {
        assert!(
            derived_drain_has_headroom(rss_mb, CEILING_MB),
            "{rss_mb} MiB of {CEILING_MB} completed on 2026-09-27 and must still drain"
        );
    }
}

#[test]
fn the_drain_that_killed_the_process_is_skipped() {
    assert!(
        !derived_drain_has_headroom(98_000, CEILING_MB),
        "98 GB of 140 GiB is where the fatal drain started"
    );
}

#[test]
fn half_the_ceiling_is_the_line() {
    assert!(derived_drain_has_headroom(CEILING_MB / 2, CEILING_MB));
    assert!(!derived_drain_has_headroom(CEILING_MB / 2 + 1, CEILING_MB));
}

#[test]
fn no_ceiling_means_no_guard() {
    assert!(
        derived_drain_has_headroom(u64::MAX / 4, 0),
        "a server with no configured ceiling drains as before"
    );
}
