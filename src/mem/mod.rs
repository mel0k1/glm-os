//! Memory subsystem: frame allocator, kernel heap, paging introspection,
//! own virtual memory manager (v0.2).

pub mod frames;
pub mod heap;
pub mod paging;
pub mod vmm;
