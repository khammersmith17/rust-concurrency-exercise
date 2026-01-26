use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

/// This is like a tradional Mutex, or a non rust Mutex.
pub struct MinimalSpinLock {
    locked: AtomicBool,
}

impl MinimalSpinLock {
    pub const fn new() -> MinimalSpinLock {
        MinimalSpinLock {
            locked: AtomicBool::new(false),
        }
    }

    /// Swap true with Acquire load ordering.
    /// This creates the happens before relationship with the release store.
    /// The true will only be loaded after a false release store occurs.
    /// This can also be a compare exchange, with parameters to only swap when the value in the
    /// atomic is false.
    pub fn lock(&self) {
        while self.locked.swap(true, Ordering::Acquire) {
            std::hint::spin_loop()
        }
    }

    pub fn unlock(&self) {
        self.locked.store(false, Ordering::Release)
    }
}

pub struct DataSpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

/// Guard cannot outlive the lock itself.
/// There is not constructor and all data is private, thus not external users can mutate the state
/// of the lock other than through the guard.
pub struct SpinLockGuard<'a, T> {
    lock: &'a DataSpinLock<T>,
}

impl<'a, T> Deref for SpinLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release)
    }
}

/// UnsafeCell is not Sync, thus we need to tell the compiler we are taking the responsbility for
/// ensuring safety. T still needs to be Send.
unsafe impl<T> Sync for DataSpinLock<T> where T: Send {}

impl<T> DataSpinLock<T> {
    pub fn new(value: T) -> DataSpinLock<T> {
        DataSpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Same procedure to acquire the lock as above, but now we access the data in the cell and
    /// coerce it into an exclusive reference.
    pub fn lock(&self) -> SpinLockGuard<T> {
        while self.locked.swap(true, Ordering::Acquire) {
            std::hint::spin_loop()
        }

        SpinLockGuard { lock: self }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::thread;

    #[test]
    fn test_guard() {
        let data = DataSpinLock::new(Vec::new());

        thread::scope(|s| {
            s.spawn(|| data.lock().push(1));
            s.spawn(|| {
                let mut g = data.lock();
                g.push(2);
                g.push(2);
            });
        });

        let g = data.lock();
        assert!(g.as_slice() == &[1, 2, 2] || g.as_slice() == &[2, 2, 1]);
    }
}
