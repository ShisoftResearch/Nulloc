use crate::mmap_heap::*;
use crate::utils::*;
use crate::{bump_heap, generic_heap, Ptr, Size, NULL_PTR};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use lfmap::{Map, WordMap};
use libc::*;
use std::alloc::{Alloc, AllocErr};
use std::ptr::{null_mut, NonNull};

lazy_static! {
    static ref RUST_ADDR_MAPPING: lfmap::WordMap<MmapAllocator, AddressHasher> =
        lfmap::WordMap::with_capacity(256);
}

pub unsafe fn nu_malloc(size: Size) -> Ptr {
    if size == 0 {
        return null_mut();
    } // The C standard (C17 7.22.3/1)
    generic_heap::malloc(size)
}
pub unsafe fn nu_free(ptr: Ptr) {
    if ptr == null_mut() {
        return;
    }
    generic_heap::free(ptr);
}

pub unsafe fn nu_calloc(nmemb: Size, size: Size) -> Ptr {
    let total_size = nmemb * size;
    let ptr = nu_malloc(total_size);
    if ptr != NULL_PTR {
        // zero-initialize is required
        memset(ptr, 0, total_size);
    }
    ptr
}

pub unsafe fn nu_realloc(ptr: Ptr, size: Size) -> Ptr {
    generic_heap::realloc(ptr, size)
}

// Allocator for rust itself for internal heaps
pub struct NullocAllocator;

unsafe impl GlobalAlloc for NullocAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        let actual_size = size + align - 1;
        let base_addr = nu_malloc(actual_size) as usize;
        let align_padding = align_padding(base_addr, align);
        let rust_addr = base_addr + align_padding;
        RUST_ADDR_MAPPING.insert(rust_addr, base_addr);
        rust_addr as *mut u8
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let addr = ptr as usize;
        if let Some(base_addr) = RUST_ADDR_MAPPING.remove(addr) {
            nu_free(base_addr as Ptr)
        }
    }
}

impl Default for NullocAllocator {
    fn default() -> Self {
        NullocAllocator
    }
}

unsafe impl Alloc for NullocAllocator {
    unsafe fn alloc(&mut self, layout: Layout) -> Result<NonNull<u8>, AllocErr> {
        Ok(NonNull::new((self as &mut GlobalAlloc).alloc(layout)).unwrap())
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        (self as &mut GlobalAlloc).dealloc(ptr.as_ptr(), layout)
    }
}
