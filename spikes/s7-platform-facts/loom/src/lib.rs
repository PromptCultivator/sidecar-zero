//! S7 probe: can loom check the store-then-load pairs the wake protocols rely on?
//!
//! Every wake protocol in the design has this shape: each side stores, then loads what
//! the other side stored, and the protocol is correct only if at least one of them sees
//! the other's store. (Producer: publish, then read the consumer's wait word. Consumer:
//! announce it is about to sleep, then re-read the ring.)
//!
//! loom's README says it treats sequentially consistent loads and stores as
//! acquire/release, which would make it reject a correct protocol, and that an explicit
//! `fence(SeqCst)` is modelled. The design therefore writes the pair as
//! store(Release), fence(SeqCst), load(Acquire). These tests find out whether that is
//! right, and whether loom still catches the bug when the fence is missing.

#[cfg(loom)]
mod tests {
    use loom::sync::atomic::{fence, AtomicUsize, Ordering};
    use loom::sync::Arc;
    use loom::thread;

    /// Runs the two-sided store-then-load under every interleaving loom explores and
    /// fails if both sides can read the old value.
    fn store_then_load(store: Ordering, load: Ordering, with_fence: bool) {
        loom::model(move || {
            let published = Arc::new(AtomicUsize::new(0)); // "a request is in the ring"
            let parked = Arc::new(AtomicUsize::new(0)); // "the consumer is about to sleep"

            let (p, k) = (published.clone(), parked.clone());
            let producer = thread::spawn(move || {
                p.store(1, store);
                if with_fence {
                    fence(Ordering::SeqCst);
                }
                k.load(load) // did the consumer announce? if so, wake it
            });

            parked.store(1, store);
            if with_fence {
                fence(Ordering::SeqCst);
            }
            let consumer_saw_request = published.load(load);
            let producer_saw_parked = producer.join().unwrap();

            assert!(
                consumer_saw_request == 1 || producer_saw_parked == 1,
                "lost wake-up: the consumer saw no request and the producer saw nobody parked"
            );
        });
    }

    /// The form the ABI doc gives for Java, Go and C++: sequentially consistent accesses.
    #[test]
    fn seqcst_store_seqcst_load() {
        store_then_load(Ordering::SeqCst, Ordering::SeqCst, false);
    }

    /// The form `scz-shm` is to use: release store, explicit full fence, acquire load.
    #[test]
    fn release_store_fence_acquire_load() {
        store_then_load(Ordering::Release, Ordering::Acquire, true);
    }

    /// The bug: no fence at all. loom must reject this, or it is not checking anything.
    #[test]
    fn release_store_acquire_load_no_fence() {
        store_then_load(Ordering::Release, Ordering::Acquire, false);
    }

    /// Weakest accesses, with the fence. Shows the fence alone carries the guarantee.
    #[test]
    fn relaxed_store_fence_relaxed_load() {
        store_then_load(Ordering::Relaxed, Ordering::Relaxed, true);
    }
}
