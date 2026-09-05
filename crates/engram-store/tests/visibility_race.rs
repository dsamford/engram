#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The visibility ring under real thread interleavings. The lost-wakeup this
//! pins: publisher A stores its ring slot, sees the gap below it, returns;
//! publisher B fills the gap, advances to its own stamp, and its load of A's
//! slot races A's store — misses it, returns. Both are done, the clock is
//! stuck one below A, and every later write on the store waits for ever (a
//! measured whole-server stall). The fix makes every WAITER re-drive
//! `advance_visible` itself, so no interleaving of publishers can strand
//! the clock. A wedge here shows as the watchdog panic, not a hung CI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(0),
    }
}

#[test]
fn concurrent_publishers_never_strand_the_visible_clock() {
    const THREADS: usize = 8;
    const PUTS: u64 = 5_000;
    let store = Store::new();
    let done = Arc::new(AtomicBool::new(false));

    // The watchdog: a wedge must FAIL the test loudly, not hang the suite.
    let watchdog = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            for _ in 0..600 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    return;
                }
            }
            panic!("visibility clock wedged: writers did not finish in 60s");
        })
    };

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let store = store.clone();
            s.spawn(move || {
                let p = prefix();
                for i in 0..PUTS {
                    let mut body = vec![t as u8];
                    body.extend_from_slice(&i.to_be_bytes());
                    store
                        .put(&p, &body, StoredValue::Plain(vec![1]))
                        .expect("put");
                }
            });
        }
    });
    done.store(true, Ordering::Relaxed);
    watchdog.join().expect("watchdog");
    // Every write acknowledged means every stamp published and visible.
    assert_eq!(
        store.count_at(&prefix(), &[], u64::MAX),
        THREADS as u64 * PUTS
    );
}
