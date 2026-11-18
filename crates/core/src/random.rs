//! Small, fast, non-cryptographic random numbers for load balancing.

use std::cell::Cell;
use std::hash::{BuildHasher, RandomState};

thread_local! {
    static STATE: Cell<u64> = Cell::new(RandomState::new().hash_one(0_u64) | 1);
}

/// Returns a random number in `0..bound`. `bound` must not be zero.
pub(crate) fn below(bound: usize) -> usize {
    STATE.with(|state| {
        // xorshift64*
        let mut x = state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) % bound as u64) as usize
    })
}
