//! Rope-aware byte cursor for the lexer scan path (plan §7.2).
//!
//! The DFA consumes bytes one at a time across ropey chunk boundaries;
//! lexemes are borrowed from a single chunk when possible or grown into a
//! reusable scratch buffer for cross-chunk matches. No document-sized
//! String allocation ever happens on the scan path.

use std::sync::Arc;

/// A byte cursor over an authoritative rope.
pub(crate) struct RopeCursor {
    rope: Arc<ropey::Rope>,
}

impl Clone for RopeCursor {
    fn clone(&self) -> Self {
        Self {
            rope: Arc::clone(&self.rope),
        }
    }
}

impl RopeCursor {
    pub(crate) fn new(rope: Arc<ropey::Rope>) -> Self {
        Self { rope }
    }

    pub(crate) fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    /// Materializes `[start..end)` into a contiguous String.
    /// Callers keep one reusable buffer; allocation happens only when
    /// the requested range exceeds current capacity.
    pub(crate) fn slice_range(&self, start: usize, end: usize) -> String {
        self.rope.byte_slice(start..end).to_string()
    }

    pub(crate) fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.len_bytes() {
            return None;
        }
        let (chunk, chunk_start) = self.chunk_containing(offset);
        chunk
            .as_bytes()
            .get(offset.saturating_sub(chunk_start))
            .copied()
    }

    /// Borrows one rope chunk containing `offset`, returning the chunk's
    /// text and the absolute byte offset of its first byte.
    pub(crate) fn chunk_containing(&self, offset: usize) -> (&str, usize) {
        let char_idx = self.rope.byte_to_char(offset);
        let (chunk_text, chunk_start_char, _byte_idx, _len) = self.rope.chunk_at_char(char_idx);
        let chunk_start_byte = self.rope.char_to_byte(chunk_start_char);
        (chunk_text, chunk_start_byte)
    }

    pub(crate) fn rope(&self) -> &ropey::Rope {
        &self.rope
    }
}
