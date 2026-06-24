//! Single-writer, multi-reader lock-free cell for `Copy` POD data.
//!
//! Classic seqlock: the writer bumps a sequence counter to an odd value, writes
//! the payload, then bumps to the next even value. Readers snapshot the seq,
//! read the payload, then re-read the seq — a mismatch (or odd starting value)
//! means a write overlapped and the read is retried.
//!
//! Why not a `Mutex` or `RwLock`?
//! - Writer never waits on readers, even under heavy read load.
//! - No syscall on the read path; in the steady state it's two atomic loads
//!   and a memcpy.
//!
//! # Constraints
//!
//! - **Exactly one writer.** Concurrent writers corrupt the sequence; debug
//!   builds assert this.
//! - **`T` must be POD-shaped.** All bit patterns of `T` must be valid: the
//!   reader can observe a torn payload mid-write before retrying, and using
//!   that value transiently must not be UB. Plain numeric structs (no enums
//!   with niche optimizations, no references, no `bool` validity invariants)
//!   are safe. The seqlock retry guarantees the *returned* value is consistent.
//!
//! # A note on the model-level data race
//!
//! The payload is read/written with non-atomic `read_volatile`/`write_volatile`.
//! Under the C++/Rust abstract memory model a concurrent non-atomic read and
//! write of the same location is a data race (UB) even though the seqlock retry
//! discards any torn value, so Miri will flag this. It is the same pragmatic
//! compromise the `seqlock` crate and the Linux kernel seqlocks make: on the
//! AArch64/x86 targets we run on, these lower to plain word loads/stores with
//! the fences below providing the real ordering, and the construction is sound
//! in practice.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering, fence};

pub struct SeqLock<T: Copy> {
    seq: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: synchronization is via the seq counter + fences; T must be Send.
unsafe impl<T: Copy + Send> Send for SeqLock<T> {}
unsafe impl<T: Copy + Send> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            seq: AtomicU32::new(0),
            data: UnsafeCell::new(value),
        }
    }

    /// Publish a new value. Single-writer contract — concurrent calls are UB.
    pub fn store(&self, value: T) {
        let s = self.seq.load(Ordering::Relaxed);
        debug_assert!(s & 1 == 0, "concurrent SeqLock writers");
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        // Release fence: makes the odd seq bump and all subsequent payload
        // writes visible before any later seq store can be observed.
        fence(Ordering::Release);
        // SAFETY: single-writer contract; readers detect overlap via the seq.
        unsafe { core::ptr::write_volatile(self.data.get(), value) };
        // Pair this Release with the reader's Acquire on seq.
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Read a consistent snapshot. Spins while a write overlaps; on a 1 Hz
    /// writer with a sub-microsecond write window, the retry path is rarely hit.
    pub fn load(&self) -> T {
        let mut spins = 0u32;
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 0 {
                // Even seq → no write was in flight when we sampled `s1`.
                // SAFETY: single-writer contract; a racing write is detected by
                // the `s1 == s2` recheck below and retried, so a torn value is
                // never returned.
                let value = unsafe { core::ptr::read_volatile(self.data.get()) };
                // The Acquire fence (not the volatility) is what orders the
                // payload read *before* the `s2` load: an Acquire fence forbids
                // any load preceding it from being reordered after a load that
                // follows it. So the relaxed `s2` is sufficient here.
                fence(Ordering::Acquire);
                let s2 = self.seq.load(Ordering::Relaxed);
                if s1 == s2 {
                    return value;
                }
            }
            // A write overlapped (odd `s1`, or `s2 != s1`). Back off and retry.
            // Spin briefly, then yield: the production 1 Hz writer essentially
            // never lands here, but a future high-rate writer must not be able
            // to starve a reader pinning a single core.
            spins = spins.wrapping_add(1);
            if spins < 64 {
                core::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Pod {
        a: u64,
        b: u64,
        c: u64,
        d: u64,
    }

    #[test]
    fn round_trip_uncontended() {
        let lock = SeqLock::new(Pod {
            a: 1,
            b: 2,
            c: 3,
            d: 4,
        });
        assert_eq!(
            lock.load(),
            Pod {
                a: 1,
                b: 2,
                c: 3,
                d: 4
            }
        );
        lock.store(Pod {
            a: 10,
            b: 20,
            c: 30,
            d: 40,
        });
        assert_eq!(
            lock.load(),
            Pod {
                a: 10,
                b: 20,
                c: 30,
                d: 40
            }
        );
    }

    /// Hammer the seqlock with one writer + many readers: every observed
    /// snapshot must be self-consistent (a == b == c == d), proving the
    /// reader never returns a torn value.
    #[test]
    fn no_torn_reads_under_contention() {
        let lock = Arc::new(SeqLock::new(Pod {
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let lock = Arc::clone(&lock);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    lock.store(Pod {
                        a: n,
                        b: n,
                        c: n,
                        d: n,
                    });
                    n = n.wrapping_add(1);
                }
            })
        };

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let mut iters = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let v = lock.load();
                        assert!(v.a == v.b && v.b == v.c && v.c == v.d, "torn: {v:?}");
                        iters += 1;
                    }
                    iters
                })
            })
            .collect();

        thread::sleep(std::time::Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
    }
}
