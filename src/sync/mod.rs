//! GLM OS synchronization primitives.
//!
//! v0.4: the spinlock became IRQ-safe — `lock()` saves RFLAGS and clears
//! IF *before* acquiring, `Drop` releases and restores IF. On an SMP
//! kernel this is mandatory: without it, an LAPIC timer interrupt landing
//! on a CPU that already holds the scheduler lock would try to take the
//! same lock again and spin forever.
//!
//! Lock ordering (leaves never acquire other locks):
//!   SCHED -> { FRAMES, KEYBOARD, SERIAL }   (scheduler is the trunk)
//!   CONSOLE, HEAP, VMM are standalone leaves.

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
    /// RFLAGS at lock() time (IF bit matters); restored on Drop.
    rflags: u64,
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        // restore IF (and the rest of flags, harmlessly)
        unsafe {
            core::arch::asm!(
                "push {flags}",
                "popfq",
                flags = in(reg) self.rflags,
                options(nomem)
            );
        }
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

    /// Acquire with interrupts disabled (restored on release).
    pub fn lock(&self) -> Guard<'_, T> {
        let rflags: u64;
        unsafe {
            core::arch::asm!(
                "pushfq",
                "cli",
                "pop {out}",
                out = out(reg) rflags,
                options(nomem)
            );
        }
        while self.locked.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        Guard { lock: self, rflags }
    }
}
