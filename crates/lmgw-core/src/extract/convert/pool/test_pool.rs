//! A pool of a test's own in place of [`super::global`]: the steps of a
//! future run inside [`within`] take their slots from it. [`small`] makes
//! the normal case since the performance round, a pool whose memory admits
//! fewer heavy steps (a PDF's, `charge::PDF_FILE`) than it has slots, so a
//! test can drive the side-by-side paths (crops, geometry, text
//! recognition) through it and see that none waits for a slot while it
//! holds one.

use std::future::Future;

use lmgw_api_types::documents::MemorySource;

use super::pool_size::{Machine, Memory};
use super::{charge, ToolPool};

tokio::task_local! {
    pub(super) static POOL: &'static ToolPool;
}

/// A pool of `slots` slots whose memory admits `pdf_steps` PDF steps at
/// once (and a little more, less than one more), kept for the test's life.
pub fn small(slots: u32, pdf_steps: u64) -> &'static ToolPool {
    let bytes = charge::PDF_FILE * pdf_steps + charge::PDF_FILE / 2;
    let pool = ToolPool::new(Machine {
        cores: slots,
        memory: Some(Memory {
            bytes,
            source: MemorySource::Physical,
        }),
    });
    Box::leak(Box::new(pool))
}

/// `fut` with `pool` as every step's pool.
pub async fn within<F: Future>(pool: &'static ToolPool, fut: F) -> F::Output {
    POOL.scope(pool, fut).await
}

#[tokio::test]
async fn a_small_pool_admits_fewer_pdf_steps_than_its_slots() {
    let p = small(8, 2);
    let f = within(p, async { super::global().facts() }).await;
    assert_eq!((f.slots, f.derived_from.charges.pdf_file.fit), (8, 2));
    // Outside it, the process's pool.
    assert!(!std::ptr::eq(super::global(), p));
}
