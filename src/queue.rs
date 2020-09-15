use crate::{Atomic, Shared, Shield};
use std::{
    cell::UnsafeCell,
    mem::{self, MaybeUninit},
    ptr,
    sync::atomic::{AtomicIsize, AtomicUsize, Ordering},
};

const BUFFER_SIZE: usize = 32;

pub struct Queue<T>
where
    T: Send + Sync,
{
    head: Atomic<Node<T>>,
    tail: Atomic<Node<T>>,
}

impl<T> Queue<T>
where
    T: Send + Sync,
{
    pub fn new() -> Self {
        let sentinel = Node::new(None, 0);

        Self {
            head: Atomic::new(sentinel),
            tail: Atomic::new(sentinel),
        }
    }

    fn cas_head(
        &self,
        current: Shared<'_, Node<T>>,
        next: Shared<'_, Node<T>>,
        shield: &Shield,
    ) -> bool {
        self.head
            .compare_and_swap(current, next, Ordering::SeqCst, shield)
            == current
    }

    fn cas_tail(&self, current: Shared<'_, Node<T>>, next: Shared<'_, Node<T>>, shield: &Shield) {
        self.tail
            .compare_and_swap(current, next, Ordering::SeqCst, shield);
    }

    pub fn push(&self, value: T, shield: &Shield) {
        loop {
            let ltail = self.tail.load(Ordering::SeqCst, shield);
            let ltail_ref = unsafe { ltail.as_ref_unchecked() };
            let idx = ltail_ref.enq_allocated.fetch_add(1, Ordering::SeqCst);

            if idx > BUFFER_SIZE - 1 {
                if ltail != self.tail.load(Ordering::SeqCst, shield) {
                    continue;
                }

                let lnext = ltail_ref.next.load(Ordering::SeqCst, shield);

                if lnext.is_null() {
                    let new_node = Node::new(Some(unsafe { ptr::read(&value) }), 1);

                    if ltail_ref.cas_next(Shared::null(), new_node, shield) {
                        self.cas_tail(ltail, new_node, shield);
                        mem::forget(value);
                        return;
                    } else {
                        unsafe {
                            Box::from_raw(new_node.as_ptr());
                        }
                    }
                } else {
                    self.cas_tail(ltail, lnext, shield);
                }

                continue;
            }

            unsafe {
                ltail_ref.items[idx].write(value);
                let idx = idx as isize;
                while ltail_ref
                    .enq_committed
                    .compare_and_swap(idx - 1, idx, Ordering::SeqCst)
                    != idx - 1
                {}
                return;
            }
        }
    }

    pub fn pop_if<'shield, F>(&self, f: F, shield: &'shield Shield) -> Option<Shared<'shield, T>>
    where
        F: Fn(&T) -> bool,
    {
        loop {
            let lhead = self.head.load(Ordering::SeqCst, shield);
            let lhead_ref = unsafe { lhead.as_ref_unchecked() };
            let idx = lhead_ref.deqidx.load(Ordering::SeqCst);

            if idx > BUFFER_SIZE - 1 {
                let lnext = lhead_ref.next.load(Ordering::SeqCst, shield);

                if lnext.is_null() {
                    break;
                }

                if self.cas_head(lhead, lnext, shield) {
                    shield.retire(move || unsafe {
                        Box::from_raw(lhead.as_ptr());
                    })
                }

                continue;
            }

            if idx as isize > lhead_ref.enq_committed.load(Ordering::SeqCst) {
                break;
            }

            let entry = &lhead_ref.items[idx];
            let item = unsafe { entry.shared() };

            if !f(unsafe { item.as_ref_unchecked() }) {
                return None;
            } else if lhead_ref
                .deqidx
                .compare_and_swap(idx, idx + 1, Ordering::SeqCst)
                != idx
            {
                continue;
            }

            return Some(item);
        }

        None
    }
}

struct Node<T> {
    enq_allocated: AtomicUsize,
    enq_committed: AtomicIsize,
    deqidx: AtomicUsize,
    next: Atomic<Self>,
    items: [Entry<T>; BUFFER_SIZE],
}

impl<T> Node<T> {
    fn new<'a>(maybe_item: Option<T>, enqidx: usize) -> Shared<'a, Self> {
        let first_entry = Entry::new();

        if let Some(item) = maybe_item {
            unsafe {
                first_entry.write(item);
            }
        }

        let node = Self {
            enq_allocated: AtomicUsize::new(enqidx),
            enq_committed: AtomicIsize::new(enqidx as isize - 1),
            deqidx: AtomicUsize::new(0),
            next: Atomic::null(),
            items: [
                first_entry,
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
                Entry::new(),
            ],
        };

        unsafe { Shared::from_ptr(Box::into_raw(Box::new(node))) }
    }

    fn cas_next(&self, current: Shared<'_, Self>, next: Shared<'_, Self>, shield: &Shield) -> bool {
        self.next
            .compare_and_swap(current, next, Ordering::SeqCst, shield)
            == current
    }
}

struct Entry<T> {
    data: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Entry<T> {
    fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    unsafe fn write(&self, item: T) {
        let data_ptr = self.data.get() as *mut T;
        ptr::write(data_ptr, item);
    }

    unsafe fn shared<'a>(&self) -> Shared<'a, T> {
        let data_ptr = self.data.get() as *mut T;
        Shared::from_ptr(data_ptr)
    }
}

#[cfg(test)]
mod tests {
    use super::Queue;
    use crate::Collector;

    #[test]
    fn push_pop_check() {
        let collector = Collector::new();
        let shield = collector.shield();
        let queue = Queue::new();
        queue.push(5, &shield);
        queue.push(10, &shield);
        assert!(matches!(queue.pop_if(|x| *x == 10, &shield), None));
        assert!(matches!(queue.pop_if(|x| *x == 5, &shield), Some(_)));
        assert!(matches!(queue.pop_if(|x| *x == 5, &shield), None));
        assert!(matches!(queue.pop_if(|x| *x == 10, &shield), Some(_)));
    }
}