use bytemuck::Pod;

/// A cursor over a packet byte slice, tracking current offset and a logical
/// length cap (used to restrict parsing to the IP payload length).
pub struct PacketBuf<'a> {
    data: &'a [u8],
    offset: usize,
    len: usize,
}

impl<'a> PacketBuf<'a> {
    /// Create a new `PacketBuf` covering the entire slice.
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        let len = data.len();
        Self {
            data,
            offset: 0,
            len,
        }
    }

    /// Number of bytes remaining (from offset to logical len).
    #[inline]
    pub fn remaining(&self) -> usize {
        self.len.saturating_sub(self.offset)
    }

    /// Current byte offset from the start of the original slice.
    #[inline]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Peek at the next `size_of::<H>()` bytes as `&H` without advancing the
    /// cursor. Returns `None` if there are not enough bytes remaining.
    #[inline]
    pub fn peek<H: Pod>(&self) -> Option<&H> {
        let sz = std::mem::size_of::<H>();
        let end = self.offset.checked_add(sz)?;
        if end > self.len || end > self.data.len() {
            return None;
        }
        Some(bytemuck::from_bytes(&self.data[self.offset..end]))
    }

    /// Return a reference to the next `size_of::<H>()` bytes as `&H` without
    /// advancing the cursor. Same as `peek` but named `get` for API symmetry.
    #[inline]
    pub fn get<H: Pod>(&self) -> Option<&H> {
        self.peek::<H>()
    }

    /// Read the next `size_of::<H>()` bytes as `&H` and advance the cursor.
    /// Returns `None` if there are not enough bytes remaining.
    #[inline]
    pub fn consume<H: Pod>(&mut self) -> Option<&H> {
        let sz = std::mem::size_of::<H>();
        let end = self.offset.checked_add(sz)?;
        if end > self.len || end > self.data.len() {
            return None;
        }
        let hdr = bytemuck::from_bytes(&self.data[self.offset..end]);
        self.offset = end;
        Some(hdr)
    }

    /// Advance the cursor by `n` bytes. Returns `false` and leaves offset
    /// unchanged if fewer than `n` bytes remain.
    #[inline]
    pub fn advance(&mut self, n: usize) -> bool {
        let new = self.offset.checked_add(n);
        match new {
            Some(new) if new <= self.len && new <= self.data.len() => {
                self.offset = new;
                true
            }
            _ => false,
        }
    }

    /// The remaining bytes from current offset to the logical length.
    #[inline]
    pub fn remaining_slice(&self) -> &'a [u8] {
        let end = self.len.min(self.data.len());
        &self.data[self.offset.min(end)..end]
    }

    /// Override the logical length (must not exceed the underlying slice
    /// length). Useful to restrict parsing to the IP payload length.
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        self.len = len.min(self.data.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::{Pod, Zeroable};

    #[test]
    fn new_buf_has_correct_initial_state() {
        let data = [0u8; 40];
        let buf = PacketBuf::new(&data);
        assert_eq!(buf.remaining(), 40);
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn peek_returns_first_byte_without_advancing() {
        let data = [0xABu8, 0xCD];
        let buf = PacketBuf::new(&data);
        assert_eq!(buf.peek::<u8>(), Some(&0xAB));
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn peek_empty_returns_none() {
        let data: [u8; 0] = [];
        let buf = PacketBuf::new(&data);
        assert_eq!(buf.peek::<u8>(), None);
    }

    #[test]
    fn consume_advances_offset() {
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable)]
        struct TwoBytes {
            a: u8,
            b: u8,
        }

        let data = [0x11u8, 0x22, 0x33];
        let mut buf = PacketBuf::new(&data);
        let two = buf.consume::<TwoBytes>().unwrap();
        assert_eq!(two.a, 0x11);
        assert_eq!(two.b, 0x22);
        assert_eq!(buf.offset(), 2);
        assert_eq!(buf.remaining(), 1);
    }

    #[test]
    fn consume_insufficient_bytes_returns_none() {
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
        struct FourBytes {
            val: [u8; 4],
        }

        let data = [0x11u8, 0x22];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(buf.consume::<FourBytes>(), None);
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn advance_and_remaining_slice() {
        let data = [1u8, 2, 3, 4];
        let mut buf = PacketBuf::new(&data);
        assert!(buf.advance(2));
        let remaining = buf.remaining_slice();
        assert_eq!(remaining, &[3, 4]);
    }

    #[test]
    fn set_len_truncates() {
        let data = [1u8, 2, 3, 4, 5];
        let mut buf = PacketBuf::new(&data);
        assert!(buf.advance(1));
        buf.set_len(3);
        assert_eq!(buf.remaining(), 2);
        assert_eq!(buf.remaining_slice(), &[2, 3]);
    }

    #[test]
    fn set_len_cannot_grow() {
        let data = [1u8, 2];
        let mut buf = PacketBuf::new(&data);
        buf.set_len(100);
        assert_eq!(buf.remaining(), 2);
    }

    #[test]
    fn remaining_slice_empty_when_exhausted() {
        let data = [1u8, 2, 3];
        let mut buf = PacketBuf::new(&data);
        assert!(buf.advance(3));
        assert_eq!(buf.remaining_slice(), &[] as &[u8]);
    }
}
