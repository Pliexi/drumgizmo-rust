//! Deterministic xorshift128+ random engine.
//!
//! Mirrors the semantics of [`std::rand`] helpers used by original DrumGizmo
//! (`Random::floatInRange`, `Random::intInRange`, `Random::choose`) while
//! being a tiny pure-Rust implementation with a fixed, seedable output.
//!
//! The engine is realtime-safe: no allocation, no syscalls.

/// A small, fast, deterministic PRNG (xorshift128+ variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rand {
    state: [u64; 2],
}

impl Rand {
    /// Creates a new instance from a seed. Branchless promotion keeps zeros
    /// out of the internal state so that a seed of 0 still produces
    /// deterministic noise.
    pub fn new(seed: u64) -> Self {
        // splitmix64 promote to two words; guarantees nonzero state.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut w = [0u64; 2];
        for s in &mut w {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            *s = x;
        }
        Self { state: w }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let s = &mut self.state;
        let mut x = s[0];
        let y = s[1];
        s[0] = y;
        x ^= x << 23;
        x ^= x >> 17;
        x ^= y ^ (y >> 26);
        s[1] = x;
        x.wrapping_add(y)
    }

    /// Uniform float in `[0, 1)`.
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    /// Uniform float in `[low, high)` as used by `Random::floatInRange`.
    #[inline]
    pub fn float_in_range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.next_f32()
    }

    /// Uniform integer in `[low, high]` inclusive as `Random::intInRange`.
    #[inline]
    pub fn int_in_range(&mut self, low: i64, high: i64) -> i64 {
        if high <= low {
            return low;
        }
        let range = (high - low + 1) as u64;
        low + (self.next_u64() % range) as i64
    }

    /// Uniformly pick an element from a slice.
    #[inline]
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        let idx = (self.next_u64() % items.len() as u64) as usize;
        Some(&items[idx])
    }
}

impl Default for Rand {
    fn default() -> Self {
        Self::new(0x6D5C_F6F1_09D3_26FF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_sequences() {
        let mut a = Rand::new(42);
        let mut b = Rand::new(42);
        for i in 0..1000 {
            let _ = i;
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn floats_in_range() {
        let mut r = Rand::new(7);
        for _ in 0..1000 {
            let f = r.next_f32();
            assert!((0.0..1.0).contains(&f));
            assert!(r.float_in_range(10.0, 20.0) >= 10.0);
            assert!(r.float_in_range(10.0, 20.0) < 20.0);
        }
    }

    #[test]
    fn int_range_is_inclusive() {
        let mut r = Rand::new(3);
        for _ in 0..1000 {
            let v = r.int_in_range(5, 7);
            assert!((5..=7).contains(&v));
        }
    }

    #[test]
    fn choose_empty() {
        let mut r = Rand::new(1);
        assert!(r.choose::<u32>(&[]).is_none());
        assert_eq!(r.choose(&[42u8]), Some(&42));
    }
}
