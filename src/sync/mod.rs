//! GLM OS synchronization primitives.
//!
//! A minimal spinlock, written from scratch: no `spin` crate, no dark magic.
//! Single-core v0.1: the shell polls input, and IRQ handlers never take the
//! console lock, so plain spin semantics are enough for now.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T> Sync for Spinlock<T> where T: Send {}
unsafe impl<T> Send for Spinlock<T> where T: Send {}

pub struct Guard<'a, T> {
    lock: &'a Spinlock<T>,
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

impl<T> core::ops::Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> core::ops::DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Spinlock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> Guard<'_, T> {
        while self.locked.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        Guard { lock: self }
    }
}
