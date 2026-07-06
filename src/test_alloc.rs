//! Test-only counting global allocator for realtime-safety assertions.
//!
//! Wraps the system allocator and counts every allocator interaction made
//! while the current thread is armed. [`assert_no_alloc`] proves a region of
//! code neither allocates, reallocates, nor frees, which is the bar for code
//! running on the audio callback. The lib test target has a free
//! global-allocator slot (the mimalloc/dhat allocators are bin-only), so this
//! observes every Rust-side heap operation in tests.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub(crate) struct CountingAllocator;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static DEALLOCS: Cell<u64> = const { Cell::new(0) };
    static REALLOCS: Cell<u64> = const { Cell::new(0) };
    static LAST_DEALLOC_SIZE: Cell<usize> = const { Cell::new(0) };
}

/// Size of the most recent armed dealloc, a debugging breadcrumb for failures.
pub(crate) fn last_armed_dealloc_size() -> usize {
    LAST_DEALLOC_SIZE.with(Cell::get)
}

// The allocator must never panic or allocate itself; it only bumps
// thread-local counters while armed.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.with(Cell::get) {
            DEALLOCS.with(|count| count.set(count.get() + 1));
            LAST_DEALLOC_SIZE.with(|size| size.set(layout.size()));
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.with(Cell::get) {
            REALLOCS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Counts of allocator interactions observed while armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AllocCounts {
    pub(crate) allocs: u64,
    pub(crate) deallocs: u64,
    pub(crate) reallocs: u64,
}

fn counts() -> AllocCounts {
    AllocCounts {
        allocs: ALLOCS.with(Cell::get),
        deallocs: DEALLOCS.with(Cell::get),
        reallocs: REALLOCS.with(Cell::get),
    }
}

/// Runs `f` with the current thread armed and returns its result plus the
/// allocator interactions it performed.
pub(crate) fn count_alloc<R>(f: impl FnOnce() -> R) -> (R, AllocCounts) {
    let before = counts();
    ARMED.with(|armed| armed.set(true));
    let result = f();
    ARMED.with(|armed| armed.set(false));
    let after = counts();
    (
        result,
        AllocCounts {
            allocs: after.allocs - before.allocs,
            deallocs: after.deallocs - before.deallocs,
            reallocs: after.reallocs - before.reallocs,
        },
    )
}

/// Asserts `f` performs no allocator interaction at all: frees and reallocs
/// count as violations too, since allocator free paths are equally unsuitable
/// on a realtime thread. `label` names the failing region.
pub(crate) fn assert_no_alloc<R>(label: &str, f: impl FnOnce() -> R) -> R {
    let (result, delta) = count_alloc(f);
    assert_eq!(
        delta,
        AllocCounts {
            allocs: 0,
            deallocs: 0,
            reallocs: 0
        },
        "{label}: allocator touched on the armed (callback) path \
         (last armed dealloc size: {})",
        last_armed_dealloc_size()
    );
    result
}
