#![allow(dead_code)]

pub mod audio;
pub mod mdns;
pub mod network;
pub mod packet_log;

#[cfg(test)]
pub(crate) mod test_alloc;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: test_alloc::CountingAllocator = test_alloc::CountingAllocator;
