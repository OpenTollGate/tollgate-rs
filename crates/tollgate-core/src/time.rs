//! Time. Core never reads a clock itself — that would pull in `std` and defeat
//! determinism. The host stamps every [`crate::Event`] with a millisecond
//! timestamp from its own monotonic source.

/// A monotonic timestamp in milliseconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct Millis(pub u64);

impl Millis {
    /// Elapsed time since `earlier`, saturating at zero.
    pub const fn since(self, earlier: Millis) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_a_forward_delta_that_saturates_backward() {
        assert_eq!(Millis(5000).since(Millis(2000)), 3000);
        assert_eq!(Millis(100).since(Millis(100)), 0);
        // earlier > self (e.g. clock skew) must clamp to zero, never wrap.
        assert_eq!(Millis(100).since(Millis(500)), 0);
    }
}
