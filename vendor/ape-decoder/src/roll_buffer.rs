/// Circular buffer with history region for the Monkey's Audio decoder.
///
/// Replaces the C++ `CRollBuffer<TYPE, WINDOW_ELEMENTS>` and
/// `CRollBufferFast<TYPE, WINDOW, HISTORY>` types.
///
/// Layout: `[history region | active window]`
/// - Total allocation: `window + history` elements.
/// - `current` index starts at `history` (the first active slot).
/// - Negative offsets from `current` access the history region.
/// - When `current` reaches the end, `roll()` copies the last `history`
///   elements back to the front and resets `current`.

#[derive(Clone)]
pub struct RollBuffer<T: Copy + Default> {
    data: Vec<T>,
    current: usize,
    history: usize,
    total: usize,
}

impl<T: Copy + Default> RollBuffer<T> {
    /// Create a new roll buffer.
    ///
    /// * `window` -- number of active slots (e.g. 512 for NNFilter, 256 for predictor).
    /// * `history` -- number of history elements (e.g. filter order, or 8 for predictor).
    pub fn new(window: usize, history: usize) -> Self {
        let total = window + history;
        Self {
            data: vec![T::default(); total],
            current: history,
            history,
            total,
        }
    }

    /// Read the element at a signed offset from the current position.
    /// Replaces C++ `operator[](int nIndex)` for read access.
    #[inline(always)]
    pub fn get(&self, offset: isize) -> T {
        self.data[(self.current as isize + offset) as usize]
    }

    /// Write an element at a signed offset from the current position.
    /// Replaces C++ `operator[](int nIndex)` for write access.
    #[inline(always)]
    pub fn set(&mut self, offset: isize, value: T) {
        let idx = (self.current as isize + offset) as usize;
        self.data[idx] = value;
    }

    /// Return a mutable reference to the element at a signed offset.
    #[inline(always)]
    pub fn get_mut(&mut self, offset: isize) -> &mut T {
        let idx = (self.current as isize + offset) as usize;
        &mut self.data[idx]
    }

    /// Advance the current pointer by one. If we have reached the end of the
    /// allocation, perform a roll to preserve history.
    /// Replaces C++ `IncrementSafe()`.
    #[inline(always)]
    pub fn increment_safe(&mut self) {
        self.current += 1;
        if self.current == self.total {
            self.roll();
        }
    }

    /// Advance the current pointer by one WITHOUT checking for end-of-buffer.
    /// The caller must ensure that a roll is performed before the pointer
    /// exceeds the allocation. Used by the predictor roll buffers which check
    /// `current_index == WINDOW_BLOCKS` explicitly.
    /// Replaces C++ `IncrementFast()`.
    #[inline(always)]
    pub fn increment_fast(&mut self) {
        self.current += 1;
    }

    /// Copy the last `history` elements (immediately before `current`) back to
    /// the front of the buffer, then reset `current` to `history`.
    /// Replaces C++ `Roll()` / memmove pattern.
    pub fn roll(&mut self) {
        let src_start = self.current - self.history;
        self.data.copy_within(src_start..self.current, 0);
        self.current = self.history;
    }

    /// Zero the first `history + 1` elements and reset `current` to `history`.
    /// Called at frame boundaries to clear state.
    /// Replaces C++ `Flush()`.
    pub fn flush(&mut self) {
        for i in 0..=self.history {
            self.data[i] = T::default();
        }
        self.current = self.history;
    }

    /// Return a slice of `count` elements starting at `current + offset`.
    /// Useful for passing contiguous history ranges to dot-product / adapt.
    #[inline(always)]
    pub fn slice(&self, offset: isize, count: usize) -> &[T] {
        let start = (self.current as isize + offset) as usize;
        &self.data[start..start + count]
    }

    /// Mutable slice variant.
    #[inline(always)]
    #[allow(dead_code)]
    pub fn slice_mut(&mut self, offset: isize, count: usize) -> &mut [T] {
        let start = (self.current as isize + offset) as usize;
        &mut self.data[start..start + count]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_write_read() {
        let mut rb = RollBuffer::<i32>::new(4, 2);
        // current starts at index 2 (history=2)
        rb.set(0, 10);
        assert_eq!(rb.get(0), 10);
        rb.increment_fast();
        assert_eq!(rb.get(-1), 10);
        rb.set(0, 20);
        rb.increment_fast();
        rb.set(0, 30);
        rb.increment_fast();
        rb.set(0, 40);
        // Now current == 6 == total, so increment_safe would roll.
        // History should contain the last 2 elements before current (30, 40).
        rb.increment_safe();
        assert_eq!(rb.get(-2), 30);
        assert_eq!(rb.get(-1), 40);
    }

    #[test]
    fn flush_zeroes_history() {
        let mut rb = RollBuffer::<i16>::new(4, 3);
        rb.set(0, 99);
        rb.increment_fast();
        rb.set(0, 88);
        rb.flush();
        // After flush, history region and first active slot are zeroed.
        for i in -3isize..=0 {
            assert_eq!(rb.get(i), 0);
        }
    }
}
