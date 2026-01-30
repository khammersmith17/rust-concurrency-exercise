use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

/// This is like a tradional Mutex, or a non rust Mutex.
/// Can lock around code blocks, not data like Rust Concurrency primitives typically do.
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
/// This basic channel uses a VecDeque internally to queue data, and a condition variable to notify
/// receiver when data is ready.
/// This channel is unbounded.
pub struct BasicChannel<T> {
    data: Mutex<VecDeque<T>>,
    ready: Condvar,
}

impl<T> BasicChannel<T> {
    pub fn new() -> BasicChannel<T> {
        BasicChannel {
            data: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        }
    }

    pub fn send(&self, data: T) {
        self.data.lock().unwrap().push_back(data);
        self.ready.notify_one();
    }

    /// This is a blocking implementation of recv.
    pub fn recv_blocking(&self) -> T {
        let mut guard = self.data.lock().unwrap();
        loop {
            if let Some(message) = guard.pop_front() {
                return message;
            } else {
                guard = self.ready.wait(guard).unwrap();
            }
        }
    }

    /// Non blocking recv returns an option if data is available.
    pub fn recv_non_blocking(&self) -> Option<T> {
        self.data.lock().unwrap().pop_front()
    }
}
pub mod basic_one_shot_channel {
    use std::cell::UnsafeCell;
    use std::mem::MaybeUninit;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    pub struct Channel<T> {
        data: UnsafeCell<MaybeUninit<T>>,
        // 2 booleans defines 4 possible states, it may also make sense to use an enum in this case
        used: AtomicBool,
        ready: AtomicBool,
    }

    unsafe impl<T> Sync for Channel<T> where T: Send {}

    impl<T: std::fmt::Debug> Channel<T> {
        pub fn new() -> Channel<T> {
            Channel {
                data: UnsafeCell::new(MaybeUninit::uninit()),
                used: AtomicBool::new(false),
                ready: AtomicBool::new(false),
            }
        }

        pub fn is_ready(&self) -> bool {
            // relaxed ordering here since we do not need a well defined happens before
            // because of the fact no data is read here, and a single atomic is updated which
            // preserves total modifcation order
            self.ready.load(Ordering::Relaxed)
        }

        pub fn in_use(&self) -> bool {
            // same considerations as is_ready
            self.used.load(Ordering::Relaxed)
        }

        /// SAFETY: assumes only a single caller calls once.
        /// Panics when the channel has been used, ie data has been written.
        pub fn send(&self, message: T) {
            // Ensure that the channel is not in use by swapping a true here and ensure the
            // previous value in this atomic was false, otherwise panic
            assert!(!self.used.swap(true, Ordering::Relaxed));

            unsafe { (*self.data.get()).write(message) };

            // update the ready flag using release store ordering
            self.ready.store(true, Ordering::Release);
        }

        /// SAFETY: It is up to the user here to ensure that the data is ready to be consumed.
        pub fn recv(&self) -> T {
            // swap in a false for ready flag and assert that the previous value was true.
            assert!(self.ready.swap(false, Ordering::Acquire));

            // Compiler suggests explicit unsafe block here
            // Even though we are in an unsafe function
            let message = unsafe { (*self.data.get()).assume_init_read() };
            message
        }
    }

    impl<T> Drop for Channel<T> {
        fn drop(&mut self) {
            // we only need to drop the data if has been written but not read
            if *self.ready.get_mut() {
                unsafe { self.data.get_mut().assume_init_drop() }
            }
        }
    }
}

pub mod arc_type_safe_oneshot_channel {
    use std::cell::UnsafeCell;
    use std::mem::MaybeUninit;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    pub fn channel<T>() -> (Sender<T>, Reciever<T>) {
        let sender_arc: Arc<Channel<T>> = Arc::new(Channel::new());
        let reciever_arc = Arc::clone(&sender_arc);

        (
            Sender {
                channel: sender_arc,
            },
            Reciever {
                channel: reciever_arc,
            },
        )
    }

    pub struct Reciever<T> {
        channel: Arc<Channel<T>>,
    }

    impl<T> Reciever<T> {
        pub fn is_ready(&self) -> bool {
            self.channel.is_ready()
        }

        // move self so it is dropped when a user calls recv().
        // this prevents any additional attempt to read the data at compile time
        pub fn recv(self) -> T {
            self.channel.read_message()
        }
    }

    pub struct Sender<T> {
        channel: Arc<Channel<T>>,
    }

    impl<T> Sender<T> {
        // move self to guarante that only one message is passed. Sender is dropped after the
        // message is consumed and written to the channel
        pub fn send(self, message: T) {
            self.channel.write_message(message);
        }
    }

    // The channel is not private, and all actions are done via the publicly exposed.
    struct Channel<T> {
        message: UnsafeCell<MaybeUninit<T>>,
        ready: AtomicBool,
    }

    unsafe impl<T> Sync for Channel<T> where T: Send {}

    impl<T> Channel<T> {
        fn new() -> Channel<T> {
            Channel {
                message: UnsafeCell::new(MaybeUninit::uninit()),
                ready: AtomicBool::new(false),
            }
        }

        fn is_ready(&self) -> bool {
            self.ready.load(Ordering::Relaxed)
        }

        fn write_message(&self, message: T) {
            debug_assert!(!self.ready.load(Ordering::Acquire));

            unsafe { (*self.message.get()).write(message) };
            self.ready.store(true, Ordering::Release);
        }

        fn read_message(&self) -> T {
            // need to set ready back to false so no attempt is made to drop the data inside the
            // MaybeUninit. Using an atomic swap here.
            debug_assert!(self.ready.swap(false, Ordering::Acquire));

            unsafe { (*self.message.get()).assume_init_read() }
        }
    }

    impl<T> Drop for Channel<T> {
        fn drop(&mut self) {
            // we only need to drop the data if has been written but not read
            if *self.ready.get_mut() {
                unsafe { self.message.get_mut().assume_init_drop() }
            }
        }
    }
}

pub mod type_safe_oneshot_channel_ref {
    /// The idea here is to avoid the overhead of Arc
    /// But this requires some additional restrictions, ie the channel must have a longer lifetime
    /// than the sender and receiver.
    use std::cell::UnsafeCell;
    use std::mem::MaybeUninit;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub struct Sender<'a, T> {
        channel: &'a Channel<T>,
    }

    impl<T> Sender<'_, T> {
        pub fn send(&self, message: T) {
            self.channel.write_message(message);
        }
    }

    pub struct Receiver<'a, T> {
        channel: &'a Channel<T>,
    }

    impl<T> Receiver<'_, T> {
        pub fn is_ready(&self) -> bool {
            self.channel.is_ready()
        }

        // move self so it is dropped when a user calls recv().
        // this prevents any additional attempt to read the data at compile time
        pub fn recv(self) -> T {
            self.channel.read_message()
        }
    }

    pub struct Channel<T> {
        message: UnsafeCell<MaybeUninit<T>>,
        ready: AtomicBool,
    }

    unsafe impl<T> Sync for Channel<T> where T: Send {}

    impl<T> Channel<T> {
        pub fn new() -> Channel<T> {
            Channel {
                message: UnsafeCell::new(MaybeUninit::uninit()),
                ready: AtomicBool::new(false),
            }
        }

        pub fn split(&mut self) -> (Sender<T>, Receiver<T>) {
            // here we are using a trick to coerce an exclusive referrence into mutliple shared
            // references here.
            //
            // This lets us do 2 things
            //      1. Pass a reference to both the sender and receiver
            //      2. statically prevent any other construction of the sender and receiver,
            //         preserving the property on a oneshot channel
            //
            // This is called reborrowing
            // The lifetime of the shared references are bound to the lifetime of the exclusive
            // reference
            *self = Self::new();

            (Sender { channel: self }, Receiver { channel: self })
        }

        fn is_ready(&self) -> bool {
            self.ready.load(Ordering::Relaxed)
        }

        fn write_message(&self, message: T) {
            debug_assert!(!self.ready.load(Ordering::Acquire));

            unsafe { (*self.message.get()).write(message) };
            self.ready.store(true, Ordering::Release);
        }

        fn read_message(&self) -> T {
            // need to set ready back to false so no attempt is made to drop the data inside the
            // MaybeUninit. Using an atomic swap here.
            debug_assert!(self.ready.swap(false, Ordering::Acquire));

            unsafe { (*self.message.get()).assume_init_read() }
        }
    }

    impl<T> Drop for Channel<T> {
        fn drop(&mut self) {
            // we only need to drop the data if has been written but not read
            if *self.ready.get_mut() {
                unsafe { self.message.get_mut().assume_init_drop() }
            }
        }
    }
}

pub mod basic_blocking_channel {
    use std::cell::UnsafeCell;
    use std::marker::PhantomData;
    use std::mem::MaybeUninit;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, Thread};

    // The contraint here is that the receiver cannot be moved to another thread.
    // The sender holds a thread handle for the consumer to wake the consumer when data becomes
    // available in the channel. Using the same borrowing approach to hold a reference to the
    // channel.

    pub struct Sender<'a, T> {
        channel: &'a Channel<T>,
        receiver_handle: Thread,
    }

    impl<T> Sender<'_, T> {
        pub fn send(&self, message: T) {
            self.channel.write_message(message);
            self.receiver_handle.unpark();
        }
    }

    pub struct Receiver<'a, T> {
        channel: &'a Channel<T>,
        // const pointers, or more broadly raw pointers in general, are not Send, thus the
        // receiver cannot be moved to a new thread moving the receiver to a new thread would
        // invalidate the thread handle held by the sender.
        _no_send: PhantomData<*const ()>,
    }

    /// Only providing a block recv method here
    impl<T> Receiver<'_, T> {
        pub fn recv(&self) -> T {
            // thread parking is done internally in the channel, thus this stays a simple wrapper
            // method.
            self.channel.read_message()
        }
    }

    pub struct Channel<T> {
        message: UnsafeCell<MaybeUninit<T>>,
        ready: AtomicBool,
    }

    unsafe impl<T> Sync for Channel<T> where T: Send {}

    impl<T> Channel<T> {
        pub fn new() -> Channel<T> {
            Channel {
                message: UnsafeCell::new(MaybeUninit::uninit()),
                ready: AtomicBool::new(false),
            }
        }

        pub fn split(&mut self) -> (Sender<T>, Receiver<T>) {
            // here we are using a trick to coerce an exclusive referrence into mutliple shared
            // references here.
            //
            // This lets us do 2 things
            //      1. Pass a reference to both the sender and receiver
            //      2. statically prevent any other construction of the sender and receiver,
            //         preserving the property on a oneshot channel
            //
            // This is called reborrowing
            // The lifetime of the shared references are bound to the lifetime of the exclusive
            // reference
            *self = Self::new();

            (
                Sender {
                    channel: self,
                    receiver_handle: thread::current(),
                },
                Receiver {
                    channel: self,
                    _no_send: PhantomData,
                },
            )
        }

        fn write_message(&self, message: T) {
            debug_assert!(!self.ready.load(Ordering::Acquire));

            unsafe { (*self.message.get()).write(message) };
            self.ready.store(true, Ordering::Release);
        }

        fn read_message(&self) -> T {
            // need to set ready back to false so no attempt is made to drop the data inside the
            // MaybeUninit. Using an atomic swap here.
            while !self.ready.swap(false, Ordering::Acquire) {
                thread::park()
            }

            unsafe { (*self.message.get()).assume_init_read() }
        }
    }

    impl<T> Drop for Channel<T> {
        fn drop(&mut self) {
            // we only need to drop the data if has been written but not read
            if *self.ready.get_mut() {
                unsafe { self.message.get_mut().assume_init_drop() }
            }
        }
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

    #[test]
    fn basic_channel_test() {
        use basic_one_shot_channel::Channel;
        let channel: Channel<String> = Channel::new();
        let t = thread::current();

        thread::scope(|s| {
            s.spawn(|| {
                channel.send(String::from("hello world"));
                t.unpark()
            });
            while !channel.is_ready() {
                thread::park()
            }
        });

        assert_eq!(channel.recv(), String::from("hello world"))
    }

    #[test]
    fn arc_oneshot_channel() {
        let (sender, receiver) = arc_type_safe_oneshot_channel::channel();

        thread::scope(|s| {
            let t = thread::current();
            s.spawn(move || {
                sender.send(1);
                t.unpark();
            });
            while !receiver.is_ready() {
                thread::park();
            }

            assert_eq!(receiver.recv(), 1);
        })
    }

    #[test]
    fn test_ref_oneshot_channel() {
        let mut channel = type_safe_oneshot_channel_ref::Channel::new();

        thread::scope(|s| {
            let (sender, receiver) = channel.split();
            let t = thread::current();
            s.spawn(move || {
                sender.send(1);
                t.unpark();
            });
            while !receiver.is_ready() {
                thread::park();
            }

            assert_eq!(receiver.recv(), 1);
        })
    }

    #[test]
    fn blocking_oneshot_sender() {
        let mut channel = basic_blocking_channel::Channel::new();

        thread::scope(|s| {
            let (sender, receiver) = channel.split();
            s.spawn(move || sender.send(1));

            assert_eq!(receiver.recv(), 1);
        })
    }
}
