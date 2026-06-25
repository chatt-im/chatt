use std::{
    cell::UnsafeCell,
    mem,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Fixed-size single-producer/single-consumer queue that transfers slot
/// ownership by swapping full and empty values.
///
/// This mirrors WebRTC's `rtc_base/swap_queue.h`: the producer and consumer
/// each own one index, and `num_elements` is the only cross-thread
/// synchronization point. The queue never allocates after construction and
/// never blocks either side.
pub(crate) struct SpscSwapQueue<T> {
    slots: Box<[UnsafeCell<T>]>,
    next_write_index: AtomicUsize,
    next_read_index: AtomicUsize,
    num_elements: AtomicUsize,
}

// SAFETY: This queue supports exactly one producer calling `insert` and one
// consumer calling `remove`. Slot access is mediated by `num_elements` with
// acquire/release ordering, matching the WebRTC SwapQueue ownership model.
unsafe impl<T: Send> Send for SpscSwapQueue<T> {}
unsafe impl<T: Send> Sync for SpscSwapQueue<T> {}

impl<T: Default> SpscSwapQueue<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "SPSC swap queue capacity must be non-zero");
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(UnsafeCell::new(T::default()));
        }
        Self {
            slots: slots.into_boxed_slice(),
            next_write_index: AtomicUsize::new(0),
            next_read_index: AtomicUsize::new(0),
            num_elements: AtomicUsize::new(0),
        }
    }
}

impl<T> SpscSwapQueue<T> {
    pub(crate) fn insert(&self, input: &mut T) -> bool {
        if self.num_elements.load(Ordering::Acquire) == self.slots.len() {
            return false;
        }

        let index = self.next_write_index.load(Ordering::Relaxed);
        // SAFETY: only the producer writes `next_write_index`, and the acquire
        // load above proved this slot is not currently owned by the consumer.
        unsafe {
            mem::swap(input, &mut *self.slots[index].get());
        }

        self.num_elements.fetch_add(1, Ordering::Release);
        self.next_write_index
            .store((index + 1) % self.slots.len(), Ordering::Relaxed);
        true
    }

    pub(crate) fn remove(&self, output: &mut T) -> bool {
        if self.num_elements.load(Ordering::Acquire) == 0 {
            return false;
        }

        let index = self.next_read_index.load(Ordering::Relaxed);
        // SAFETY: only the consumer writes `next_read_index`, and the acquire
        // load above proved this slot contains a producer-published value.
        unsafe {
            mem::swap(output, &mut *self.slots[index].get());
        }

        self.num_elements.fetch_sub(1, Ordering::Release);
        self.next_read_index
            .store((index + 1) % self.slots.len(), Ordering::Relaxed);
        true
    }

    #[cfg(test)]
    pub(crate) fn size_at_least(&self) -> usize {
        self.num_elements.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn swaps_items_in_fifo_order() {
        let queue = SpscSwapQueue::<Option<u32>>::with_capacity(2);
        let mut item = Some(1);
        assert!(queue.insert(&mut item));
        assert_eq!(item, None);
        item = Some(2);
        assert!(queue.insert(&mut item));
        item = Some(3);
        assert!(!queue.insert(&mut item));
        assert_eq!(item, Some(3));

        let mut out = None;
        assert!(queue.remove(&mut out));
        assert_eq!(out, Some(1));
        out = None;
        assert!(queue.remove(&mut out));
        assert_eq!(out, Some(2));
        out = None;
        assert!(!queue.remove(&mut out));
    }

    #[test]
    fn transfers_between_threads_without_copying_payloads() {
        let queue = std::sync::Arc::new(SpscSwapQueue::<Option<Vec<u32>>>::with_capacity(4));
        let producer = std::sync::Arc::clone(&queue);
        let handle = thread::spawn(move || {
            for value in 0..64u32 {
                let mut item = Some(vec![value; 3]);
                while !producer.insert(&mut item) {
                    thread::yield_now();
                }
                assert!(item.as_ref().is_none_or(Vec::is_empty));
            }
        });

        let mut seen = Vec::new();
        while seen.len() < 64 {
            let mut item = Some(Vec::new());
            if queue.remove(&mut item) {
                seen.push(item.unwrap()[0]);
            } else {
                thread::yield_now();
            }
        }
        handle.join().unwrap();

        assert_eq!(seen, (0..64).collect::<Vec<_>>());
        assert_eq!(queue.size_at_least(), 0);
    }
}
