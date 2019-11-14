// usize lock-free, wait free paged linked list stack

use crate::utils::*;
use core::alloc::Alloc;
use core::ptr;
use core::{intrinsics, mem};
use crossbeam::utils::Backoff;
use std::alloc::Global;
use std::intrinsics::size_of;
use std::ops::Deref;
use std::ptr::null_mut;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicPtr, AtomicUsize};

const EMPTY_SLOT: usize = 0;
const SENTINEL_SLOT: usize = 1;

struct BufferMeta<T: Default, A: Alloc + Default> {
    head: AtomicUsize,
    next: AtomicPtr<BufferMeta<T, A>>,
    refs: AtomicUsize,
    upper_bound: usize,
    lower_bound: usize,
}

pub struct List<T: Default, A: Alloc + Default = Global> {
    head: AtomicPtr<BufferMeta<T, A>>,
    count: AtomicUsize,
    buffer_cap: usize,
}

impl<T: Default, A: Alloc + Default> List<T, A> {
    pub fn new(buffer_cap: usize) -> Self {
        let first_buffer = BufferMeta::new(buffer_cap);
        Self {
            head: AtomicPtr::new(first_buffer),
            count: AtomicUsize::new(0),
            buffer_cap,
        }
    }

    pub fn push(&self, flag: usize, data: T) {
        let backoff = Backoff::new();
        let obj_size = mem::size_of::<T>();
        loop {
            let head_ptr = self.head.load(Relaxed);
            let page = BufferMeta::borrow(head_ptr);
            let slot_pos = page.head.load(Relaxed);
            let next_pos = slot_pos + 1;
            if next_pos > self.buffer_cap {
                // buffer overflow, make new and link to last buffer
                let new_head = BufferMeta::new(self.buffer_cap);
                unsafe {
                    (*new_head).next.store(head_ptr, Relaxed);
                }
                if self.head.compare_and_swap(head_ptr, new_head, Relaxed) != head_ptr {
                    BufferMeta::unref(new_head);
                }
            // either case, retry
            } else {
                // in this part, we will try to reason about the push on an buffer
                // It will first try to CAS the head then write the item, finally store a
                // non-zero flag (or value) to the slot.

                // Note that zero in the slot indicates not complete on pop, then pop
                // will back off and try again
                if page.head.compare_and_swap(slot_pos, next_pos, Relaxed) == slot_pos {
                    let slot_ptr =
                        (page.lower_bound + slot_pos * mem::size_of::<usize>()) as *mut usize;
                    let obj_ptr = (page.upper_bound + slot_pos * obj_size) as *mut T;
                    unsafe {
                        if obj_size != 0 {
                            ptr::write(obj_ptr, data);
                        }
                        assert_eq!(
                            intrinsics::atomic_cxchg_relaxed(slot_ptr, EMPTY_SLOT, flag).0,
                            EMPTY_SLOT
                        );
                    }
                    self.count.fetch_add(1, Relaxed);
                    break;
                }
            }
            backoff.spin();
        }
    }

    pub fn exclusive_push(&self, flag: usize, data: T) {
        // user ensure the push is exclusive, thus no CAS except for header
        let backoff = Backoff::new();
        let obj_size = mem::size_of::<T>();
        loop {
            let head_ptr = self.head.load(Relaxed);
            let page = BufferMeta::borrow(head_ptr);
            let slot = page.head.load(Relaxed);
            let next_slot = slot + 1;
            if next_slot > self.buffer_cap {
                // buffer overflow, make new and link to last buffer
                let new_head = BufferMeta::new(self.buffer_cap);
                unsafe {
                    (*new_head).next.store(head_ptr, Relaxed);
                }
                self.head.store(new_head, Relaxed);
                if self.head.compare_and_swap(head_ptr, new_head, Relaxed) != head_ptr {
                    BufferMeta::unref(new_head);
                }
            // either case, retry
            } else {
                let slot_ptr = (page.lower_bound + slot * mem::size_of::<usize>()) as *mut usize;
                let obj_ptr = (page.upper_bound + slot * mem::size_of::<T>()) as *mut T;
                page.head.store(next_slot, Relaxed);
                unsafe {
                    if obj_size != 0 {
                        ptr::write(obj_ptr, data);
                    }
                    intrinsics::atomic_store_relaxed(slot_ptr, flag);
                }
                self.count.fetch_add(1, Relaxed);
                break;
            }
            backoff.spin();
        }
    }

    pub fn pop(&self) -> Option<(usize, T)> {
        if self.count.load(Relaxed) == 0 {
            return None;
        }
        let backoff = Backoff::new();
        let obj_size = mem::size_of::<T>();
        loop {
            let head_ptr = self.head.load(Relaxed);
            let page = BufferMeta::borrow(head_ptr);
            let slot = page.head.load(Relaxed);
            let obj_size = mem::size_of::<T>();
            let next_buffer_ptr = page.next.load(Relaxed);
            if slot == 0 && next_buffer_ptr == null_mut() {
                // empty buffer chain
                return None;
            }
            if slot == 0 && next_buffer_ptr != null_mut() {
                // last item, need to remove this head and swap to the next one
                // CAS page head to four times of the upper bound indicates this buffer is obsolete
                if self
                    .head
                    .compare_and_swap(head_ptr, next_buffer_ptr, Relaxed)
                    == head_ptr
                {
                    BufferMeta::unref(head_ptr);
                }
                continue;
            }
            let mut res = None;
            if slot > 0 {
                unsafe {
                    let new_slot = slot - 1;
                    let slot_ptr =
                        (page.lower_bound + slot * mem::size_of::<usize>()) as *mut usize;
                    let obj_ptr = (page.upper_bound + slot * mem::size_of::<T>()) as *mut T;
                    let slot_flag = intrinsics::atomic_load_relaxed(slot_ptr);
                    if slot_flag != 0
                        // first things first, swap the slot to zero if it is not zero
                        && intrinsics::atomic_cxchg_relaxed(slot_ptr, slot_flag, EMPTY_SLOT).1
                    {
                        res = Some((slot_flag, T::default()));
                        if obj_size != 0 && slot_flag != SENTINEL_SLOT {
                            res.as_mut()
                                .map(|(_, obj)| *obj = unsafe { ptr::read(obj_ptr as *mut T) });
                        }
                        if page.head.compare_and_swap(slot, new_slot, Relaxed) != slot {
                            // Swap page head failed
                            // The only possible scenario is that there was a push for
                            // pop will back off if flag is detected as zero
                            // In this case, we have a hole in the list, should indicate pop that
                            // this slot does not have any useful information, should pop again
                            intrinsics::atomic_store_relaxed(slot_ptr, SENTINEL_SLOT);
                        } else if slot_flag != SENTINEL_SLOT {
                            return res;
                        }
                    }
                }
            } else {
                return res;
            }
            backoff.spin();
        }
    }
    pub fn drop_out_all(&self) -> Option<Vec<(usize, T)>> {
        if self.count.load(Relaxed) == 0 {
            return None;
        }
        let backoff = Backoff::new();
        let mut res = Vec::new();
        let new_head_buffer = BufferMeta::new(self.buffer_cap);
        let mut buffer_ptr = self.head.swap(new_head_buffer, Relaxed);
        let word_size = mem::size_of::<usize>();
        'main: while buffer_ptr != null_mut() {
            let buffer = BufferMeta::borrow(buffer_ptr);
            let next_ptr = buffer.next.load(Relaxed);
            loop {
                //wait until reference counter reach 2 one for not garbage one for current reference)
                let flag = 1 << word_size;
                let ref_num = buffer.refs.compare_and_swap(2, flag, Relaxed);
                if ref_num >= (flag << (word_size >> 1)) {
                    // dropping out by another thread, break
                    break 'main;
                } else if ref_num <= 1 {
                    // this buffer is marked to be gc, untouched
                    break 'main;
                } else if ref_num == 2 {
                    // no other reference, flush and break out waiting
                    BufferMeta::flush_buffer(&*buffer, Some(&mut res));
                    BufferMeta::unref(buffer_ptr);
                    buffer_ptr = next_ptr;
                    break;
                }
                backoff.spin();
            }
            backoff.spin();
        }
        self.count.fetch_sub(res.len(), Relaxed);
        return Some(res);
    }

    pub fn prepend_with(&self, other: &Self) {
        if other.count.load(Relaxed) == 0 {
            return;
        }
        let other_head = other.head.swap(BufferMeta::new(self.buffer_cap), Relaxed);
        let other_count = other.count.swap(0, Relaxed);
        let mut other_tail = BufferMeta::borrow(other_head);
        // probe the last buffer in other link
        loop {
            while other_tail.refs.load(Relaxed) > 2 {}
            let next_ptr = other_tail.next.load(Relaxed);
            if next_ptr == null_mut() {
                break;
            }
            other_tail = BufferMeta::borrow(next_ptr);
        }

        // CAS this head to other head then reset other tail next buffer to this head
        loop {
            let this_head = self.head.load(Relaxed);
            if self.head.compare_and_swap(this_head, other_head, Relaxed) != this_head {
                continue;
            } else {
                other_tail.next.store(this_head, Relaxed);
                break;
            }
        }
        self.count.fetch_add(other_count, Relaxed);
    }

    pub fn count(&self) -> usize {
        self.count.load(Relaxed)
    }
}

impl<T: Default, A: Alloc + Default> Drop for List<T, A> {
    fn drop(&mut self) {
        unsafe {
            let mut node_ptr = self.head.load(Relaxed);
            while node_ptr as usize != 0 {
                let next_ptr = (&*node_ptr).next.load(Relaxed);
                BufferMeta::unref(node_ptr);
                node_ptr = next_ptr;
            }
        }
    }
}

impl<T: Default, A: Alloc + Default> BufferMeta<T, A> {
    pub fn new(buffer_cap: usize) -> *mut BufferMeta<T, A> {
        let meta_size = mem::size_of::<Self>();
        let slots_size = buffer_cap * mem::size_of::<usize>();
        let data_size = buffer_cap * mem::size_of::<T>();
        let page_size = meta_size + slots_size + data_size;
        let head_page = alloc_mem::<T, A>(page_size) as *mut Self;
        let head_page_address = head_page as usize;
        let slots_start = head_page_address + meta_size;
        *(unsafe { &mut *head_page }) = Self {
            head: AtomicUsize::new(0),
            next: AtomicPtr::new(null_mut()),
            refs: AtomicUsize::new(1),
            upper_bound: slots_start + slots_size,
            lower_bound: slots_start,
        };
        head_page
    }

    pub fn unref(buffer: *mut Self) {
        let rc = {
            let buffer = unsafe { &*buffer };
            buffer.refs.fetch_sub(1, Relaxed)
        };
        if rc == 1 {
            Self::gc(buffer);
        }
    }

    fn gc(buffer: *mut Self) {
        let page_size = *SYS_PAGE_SIZE;
        let mut objs = Vec::with_capacity(page_size);
        Self::flush_buffer(unsafe { &*buffer }, Some(&mut objs));
        for obj in objs {
            drop(obj)
        }
        dealloc_mem::<T, A>(buffer as usize, page_size)
    }

    // only use when the buffer is about to be be dead
    // this require reference checking
    fn flush_buffer(buffer: &Self, mut retain: Option<&mut Vec<(usize, T)>>) {
        let size_of_obj = mem::size_of::<T>();
        let data_bound = buffer.head.load(Relaxed);
        let mut slot_addr = buffer.lower_bound;
        let mut obj_addr = buffer.upper_bound;
        debug_assert!(
            buffer.refs.load(Relaxed) <= 2,
            "Reference counting check failed"
        );
        for _ in 0..data_bound {
            unsafe {
                let slot = intrinsics::atomic_load_relaxed(slot_addr as *const usize);
                if slot != EMPTY_SLOT && slot != SENTINEL_SLOT {
                    let mut rest = (slot, T::default());
                    if size_of_obj > 0 {
                        rest.1 = ptr::read(obj_addr as *const T);
                    }
                    if let Some(ref mut res) = retain {
                        res.push(rest);
                    }
                }
            }
            slot_addr += mem::size_of::<usize>();
            obj_addr += mem::size_of::<T>();
        }
        buffer.head.store(0, Relaxed);
    }

    fn borrow(buffer: *mut Self) -> BufferRef<T, A> {
        {
            let buffer = unsafe { &*buffer };
            buffer.refs.fetch_add(1, Relaxed);
        }
        BufferRef { ptr: buffer }
    }
}

struct BufferRef<T: Default, A: Alloc + Default> {
    ptr: *mut BufferMeta<T, A>,
}

impl<T: Default, A: Alloc + Default> Drop for BufferRef<T, A> {
    fn drop(&mut self) {
        BufferMeta::unref(self.ptr);
    }
}

impl<T: Default, A: Alloc + Default> Deref for BufferRef<T, A> {
    type Target = BufferMeta<T, A>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr }
    }
}

const SLOT_DATA_OFFSET: usize = 5;

pub struct WordList<A: Alloc + Default = Global> {
    inner: List<(), A>,
}

impl<A: Alloc + Default> WordList<A> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: List::new(cap),
        }
    }
    pub fn new() -> Self {
        Self::with_capacity(256)
    }
    pub fn push(&self, data: usize) {
        self.inner.push(data + SLOT_DATA_OFFSET, ())
    }
    pub fn exclusive_push(&self, data: usize) {
        self.inner.exclusive_push(data + SLOT_DATA_OFFSET, ())
    }
    pub fn pop(&self) -> Option<usize> {
        self.inner.pop().map(|(data, _)| data - SLOT_DATA_OFFSET)
    }

    pub fn drop_out_all(&self) -> Option<Vec<usize>> {
        self.inner
            .drop_out_all()
            .map(|vec| vec.into_iter().map(|(v, _)| v - SLOT_DATA_OFFSET).collect())
    }
    pub fn prepend_with(&self, other: &Self) {
        self.inner.prepend_with(&other.inner)
    }
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

pub struct ObjectList<T: Default, A: Alloc + Default = Global> {
    inner: List<T, A>,
}

impl<T: Default, A: Alloc + Default> ObjectList<T, A> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: List::new(cap),
        }
    }
    pub fn new() -> Self {
        Self::with_capacity(256)
    }
    pub fn push(&self, data: T) {
        self.inner.push(!0, data)
    }
    pub fn exclusive_push(&self, data: T) {
        self.inner.exclusive_push(!0, data)
    }
    pub fn pop(&self, data: usize) -> Option<T> {
        self.inner.pop().map(|(_, obj)| obj)
    }

    pub fn drop_out_all(&self) -> Option<Vec<T>> {
        self.inner
            .drop_out_all()
            .map(|vec| vec.into_iter().map(|(_, obj)| obj).collect())
    }

    pub fn prepend_with(&self, other: &Self) {
        self.inner.prepend_with(&other.inner)
    }
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

#[cfg(test)]
mod test {
    use crate::collections::lflist::*;
    use crate::utils::SYS_PAGE_SIZE;
    use std::alloc::Global;
    use std::sync::Arc;
    use std::thread;

    #[test]
    pub fn general() {
        let list = WordList::<_, Global>::new(128);
        let page_size = *SYS_PAGE_SIZE;
        for i in 2..page_size {
            list.push(i);
        }
        for i in (2..page_size).rev() {
            assert_eq!(list.pop(), Some(i));
        }
        for i in 2..page_size {
            assert_eq!(list.pop(), None);
        }
        list.push(32);
        list.push(25);
        assert_eq!(list.count(), 2);
        assert_eq!(list.drop_out_all(), Some(vec![(32, ()), (25, ())]));
        assert_eq!(list.count(), 0);
    }

    #[test]
    pub fn parallel() {
        let list = Arc::new(WordList::<_, Global>::new(128));
        let page_size = *SYS_PAGE_SIZE;
        let mut threads = (1..page_size)
            .map(|i| {
                let list = list.clone();
                thread::spawn(move || {
                    list.push(i);
                })
            })
            .collect::<Vec<_>>();
        for t in threads {
            t.join();
        }

        let mut counter = 0;
        while list.pop().is_some() {
            counter += 1;
        }
        assert_eq!(counter, page_size - 1);

        for i in 1..page_size {
            list.push(i);
        }
        let recev_list = Arc::new(WordList::<_, Global>::new());
        threads = (page_size..(page_size * 2))
            .map(|i| {
                let list = list.clone();
                let recev_list = recev_list.clone();
                thread::spawn(move || {
                    if i % 2 == 0 {
                        list.push(i);
                    } else {
                        let pop_val = list.pop().unwrap();
                        recev_list.push(pop_val);
                    }
                })
            })
            .collect::<Vec<_>>();
        for t in threads {
            t.join();
        }

        let mut agg = vec![];
        while let Some(v) = list.pop() {
            agg.push(v);
        }
        while let Some(v) = recev_list.pop() {
            agg.push(v);
        }
        agg.sort();
        agg.dedup_by_key(|k| *k);
        let total_insertion = page_size + page_size / 2 - 1;
        assert_eq!(agg.len(), total_insertion);
    }
}
