/// Aligned storage for libopus state structures.
#[derive(Debug)]
pub struct AlignedBuffer {
    buf: Vec<usize>,
    bytes: usize,
}

impl AlignedBuffer {
    /// Allocate at least `bytes` of pointer-aligned storage.
    #[must_use]
    pub fn with_capacity_bytes(bytes: usize) -> Self {
        let word = std::mem::size_of::<usize>();
        let len = bytes.div_ceil(word);
        let buf = vec![0usize; len];
        let bytes = len * word;
        Self { buf, bytes }
    }

    /// Total available capacity in bytes.
    #[must_use]
    pub fn capacity_bytes(&self) -> usize {
        self.bytes
    }

    /// Borrow the underlying buffer as a mutable pointer.
    pub fn as_mut_ptr<T>(&mut self) -> *mut T {
        self.buf.as_mut_ptr().cast()
    }
}
