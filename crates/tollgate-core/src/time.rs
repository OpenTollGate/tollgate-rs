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
