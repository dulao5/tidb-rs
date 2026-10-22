//! Byte ranges into the SQL text.

/// A half-open byte range `start..end` into the parsed SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    start: usize,
    end: usize,
}

impl Span {
    /// `start..end`; `start <= end`.
    #[inline]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "span start {start} > end {end}");
        Span { start, end }
    }
    /// First byte.
    #[inline]
    pub fn start(&self) -> usize {
        self.start
    }
    /// One past the last byte.
    #[inline]
    pub fn end(&self) -> usize {
        self.end
    }
    /// Length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }
    /// Whether the span covers no bytes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}
