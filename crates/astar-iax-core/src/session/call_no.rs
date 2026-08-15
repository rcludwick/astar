// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! 15-bit IAX call number newtype.
//!
//! Call numbers occupy the low 15 bits of the IAX2 `scallno` / `dcallno`
//! header fields; 0 is reserved per RFC 5456 §6.

/// A valid IAX call number in `1..=0x7fff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallNo(u16);

impl CallNo {
    /// Construct from a raw value; returns `None` if the value is 0 or has
    /// any bit above the 15-bit field set.
    #[must_use]
    pub const fn new(v: u16) -> Option<Self> {
        if v == 0 || v & 0x8000 != 0 {
            None
        } else {
            Some(Self(v))
        }
    }

    #[must_use]
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for CallNo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Pool of available call numbers. Linear-scan allocator: 32k slots is small
/// enough that a Vec<bool> bitset is the simplest correct thing.
pub struct CallNoAllocator {
    next: u16,
    in_use: Box<[bool]>,
}

impl CallNoAllocator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: 1,
            in_use: vec![false; 0x8000].into_boxed_slice(),
        }
    }

    /// Allocate the next free call number. Returns `None` when the entire
    /// 15-bit space is in use.
    pub fn alloc(&mut self) -> Option<CallNo> {
        for _ in 0..0x7fff {
            let candidate = self.next;
            self.next = if self.next >= 0x7fff {
                1
            } else {
                self.next + 1
            };
            if !self.in_use[candidate as usize] {
                self.in_use[candidate as usize] = true;
                return Some(CallNo(candidate));
            }
        }
        None
    }

    pub fn free(&mut self, c: CallNo) {
        self.in_use[c.value() as usize] = false;
        // Bias the next scan to re-pick freed slots quickly.
        if c.value() < self.next {
            self.next = c.value();
        }
    }
}

impl Default for CallNoAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_skips_zero_and_wraps() {
        let mut a = CallNoAllocator::new();
        let first = a.alloc().expect("first alloc");
        assert!(first.value() >= 1);
        let second = a.alloc().expect("second alloc");
        assert_ne!(first, second);
        a.free(first);
        // Freed slot becomes available again after wrap or on demand.
        let third = a.alloc().expect("third alloc");
        assert_eq!(third, first, "alloc reuses the freed slot");
    }

    #[test]
    fn allocator_exhausts_at_15_bit_max() {
        let mut a = CallNoAllocator::new();
        // Fill the pool: 0x7fff slots (1..=0x7fff).
        let mut allocated = Vec::with_capacity(0x7fff);
        for _ in 0..0x7fff {
            allocated.push(a.alloc().expect("slot available"));
        }
        assert!(a.alloc().is_none(), "pool exhausted");
        // Free one, then alloc — should succeed.
        let freed = allocated.pop().unwrap();
        a.free(freed);
        let reclaimed = a.alloc().expect("freed slot reclaimed");
        assert_eq!(reclaimed, freed);
    }

    #[test]
    fn allocator_wraps_past_top() {
        let mut a = CallNoAllocator::new();
        // Fill the pool entirely so `next` walks all the way to 0x7fff.
        for _ in 0..0x7fff {
            let _ = a.alloc().expect("slot");
        }
        // Free a low slot — next allocation should find it after wrapping.
        let low = CallNo::new(3).unwrap();
        a.free(low);
        let reclaimed = a.alloc().expect("low slot reclaimed after wrap");
        assert_eq!(reclaimed, low);
    }

    #[test]
    fn callno_value_fits_15_bits() {
        let n = CallNo::new(0x1234).expect("in range");
        assert_eq!(n.value(), 0x1234);
        assert!(CallNo::new(0x8000).is_none(), "16-bit values rejected");
    }
}
