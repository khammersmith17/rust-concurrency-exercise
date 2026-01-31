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

pub mod basic_arc {
    use std::ops::Deref;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicUsize, Ordering, fence};
    // Basic implementation of Arc<T>

    // internal data structure
    struct ArcData<T> {
        ref_count: AtomicUsize,
        data: T,
    }

    impl<T> ArcData<T> {
        fn new(data: T) -> ArcData<T> {
            ArcData {
                ref_count: AtomicUsize::new(1),
                data,
            }
        }

        fn increment_ref_count(&self) {
            // overflow when the ref count exceeds the bounds of usize::MAX / 2
            // likely will not happen but could. This leaves some space to not overflow in between
            // the time abort is called and the abort executes.
            if self.ref_count.fetch_add(1, Ordering::Relaxed) > usize::MAX / 2 {
                std::process::abort()
            };
        }

        fn decrement_ref_count(&self) -> usize {
            // need Release Ordering here to ensure nothing is accessing the data before it is
            // dropped.
            // Return the previous ref count, the value before the fetch sub operation.
            self.ref_count.fetch_sub(1, Ordering::Release)
        }
    }

    // Box does not work here, due to lifetime restrictions and the inability to drop only when the
    // strong count reaches 0. Box is owned by whoever originally allocated the data here.
    pub struct MyArc<T> {
        arc_ptr: NonNull<ArcData<T>>,
    }

    unsafe impl<T: Send + Sync> Send for MyArc<T> {}
    unsafe impl<T: Send + Sync> Sync for MyArc<T> {}

    impl<T> MyArc<T> {
        pub fn new(data: T) -> MyArc<T> {
            // Leaking here gives up the exclusive reference to the heap allocated data.
            let arc_ptr = NonNull::from(Box::leak(Box::new(ArcData::new(data))));
            MyArc { arc_ptr }
        }

        fn data(&self) -> &ArcData<T> {
            unsafe { self.arc_ptr.as_ref() }
        }

        pub fn get_mut(arc: &mut Self) -> Option<&mut T> {
            // "static" style function on the Arc. Needs to be called via MyArc::get_mut().
            // This creates a semantic difference for types the implement Deref but not DerefMut to
            // disambiguite the access style, and make sure conditions hold that make the exclusive access
            // valid.
            //
            // The lifetime of the exclusive reference is bound to the mutable reference on the
            // MyArc itself, statically defining the property that no other instance of a MyArc can
            // exist while this exclusive reference is held.
            //
            // This is only valid when we can ensure exclusive access to the data, the only case
            // where this property holds is when the ref count == 1.
            // We use an acquire fence here when this condition holds to create the happens before
            // relationship with the mutable access.
            if arc.data().ref_count.load(Ordering::Relaxed) == 1 {
                fence(Ordering::Acquire);

                unsafe { Some(&mut arc.arc_ptr.as_mut().data) }
            } else {
                None
            }
        }
    }

    impl<T> Clone for MyArc<T> {
        fn clone(&self) -> MyArc<T> {
            unsafe { self.arc_ptr.as_ref().increment_ref_count() };
            MyArc {
                arc_ptr: self.arc_ptr,
            }
        }
    }

    impl<T> Deref for MyArc<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.data().data
        }
    }

    impl<T> Drop for MyArc<T> {
        fn drop(&mut self) {
            if unsafe { self.arc_ptr.as_ref().decrement_ref_count() } == 1 {
                // Release ordering is used on the ref count deref.
                // We establish the happens before relationship here using a fence.
                // This ordering only needs to be established when dropping.
                fence(Ordering::Acquire);
                unsafe { drop(Box::from_raw(self.arc_ptr.as_ptr())) };
            }
        }
    }
}

pub mod arc_with_weak_pointers {
    use std::cell::UnsafeCell;
    use std::ops::Deref;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicUsize, Ordering, fence};

    struct ArcData<T> {
        // This is the strong count, ie the number of Arc that have been created through
        // Arc.clone().
        data_ref_count: AtomicUsize,
        // This is the total number of allocations, including both strong and weak count.
        alloc_count: AtomicUsize,
        // When the strong count is > 0, this will be Some.
        // When only weak references remain, this will be None.
        // This allows between seperation of lifetimes between the data and the container that
        // holds the data.
        data: UnsafeCell<Option<T>>,
    }

    impl<T> ArcData<T> {
        fn new(data: T) -> ArcData<T> {
            let data = UnsafeCell::new(Some(data));

            ArcData {
                data_ref_count: AtomicUsize::new(1),
                alloc_count: AtomicUsize::new(1),
                data,
            }
        }

        fn increment_alloc_ref_count(&self) {
            if self.alloc_count.fetch_add(1, Ordering::Relaxed) > usize::MAX / 2 {
                std::process::abort()
            }
        }

        fn decrement_alloc_count(&self) -> usize {
            self.alloc_count.fetch_sub(1, Ordering::Release)
        }

        fn decrement_data_ref_count(&self) -> usize {
            self.data_ref_count.fetch_sub(1, Ordering::Release)
        }
    }

    // We use a weak ref inside the actual Arc.
    // We can think of WeakRef of the mechanism to keep the data inside ArcData<T> alive.
    // An thus the Arc actually needs to hold a WeakRef T and add behavior on top of it.
    // There is a difference here between the "lifetime" of the container holding the data
    // and the data itself. The container may live longer than the actual data.
    pub struct MyArc<T> {
        weak: WeakRef<T>,
    }

    impl<T> MyArc<T> {
        pub fn new(data: T) -> MyArc<T> {
            MyArc {
                weak: WeakRef::new(data),
            }
        }

        pub fn get_mut(arc: &mut MyArc<T>) -> Option<&mut T> {
            // Here we want to ensure that the allocation count, ie the strong count, is 1 to hold
            // the property only a single owner of MyArc<T>.
            // Load the current allocation count and ensure that there is a single owner.
            // We can use Relaxed ordering herer given the total modification order.
            // We use an Acquire fence around this load to ensure the happens before relationship
            // with any subsequent load of the atomic.
            // Then we access the NonNull inside WeakRef. Then acquire the internal data
            // inside the UnsafeCell. The data inside the Option<T> should be Some if the
            // invariants are upheld here.
            // The check for all allocations protects the case where a WeakRef<T> is upgraded to a
            // strong Arc.

            if arc.weak.data().alloc_count.load(Ordering::Relaxed) == 1 {
                fence(Ordering::Acquire);
                let arc_data = unsafe { arc.weak.arc_ptr.as_mut() };
                let arc_data_option = arc_data.data.get_mut();
                let data = arc_data_option.as_mut().unwrap();

                Some(data)
            } else {
                None
            }
        }
        pub fn downgrade(arc: &Self) -> WeakRef<T> {
            arc.weak.clone()
        }
    }

    impl<T> Deref for MyArc<T> {
        type Target = T;
        fn deref(&self) -> &T {
            // deref the UnsafeCell pointer, and return a ref to the underlying data, which
            // should be Some in this case. The final unwrap is to handle the Option<T> inside the
            // UnsafeCell.
            unsafe { (*self.weak.data().data.get()).as_ref().unwrap() }
        }
    }

    impl<T> Clone for MyArc<T> {
        fn clone(&self) -> MyArc<T> {
            let weak = self.weak.clone();

            if weak.data().data_ref_count.fetch_add(1, Ordering::Relaxed) == 1 {
                std::process::abort()
            }

            MyArc { weak }
        }
    }

    impl<T> Drop for MyArc<T> {
        fn drop(&mut self) {
            // We need to decrement the global ref count here.
            // Decrement the global ref counter here, then when the weak gets dropped, the alloc
            // counter will get decremented.
            if self.weak.data().decrement_data_ref_count() == 1 {
                fence(Ordering::Acquire);
                let ptr = self.weak.data().data.get();

                // setting the weak ref data pointer to None, so nothing else will access it.
                unsafe { (*ptr) = None }
            }
        }
    }

    pub struct WeakRef<T> {
        arc_ptr: NonNull<ArcData<T>>,
    }

    unsafe impl<T: Send + Sync> Send for WeakRef<T> {}
    unsafe impl<T: Send + Sync> Sync for WeakRef<T> {}

    impl<T> WeakRef<T> {
        fn new(data: T) -> WeakRef<T> {
            WeakRef {
                arc_ptr: NonNull::from(Box::leak(Box::new(ArcData::new(data)))),
            }
        }

        fn data(&self) -> &ArcData<T> {
            unsafe { self.arc_ptr.as_ref() }
        }

        pub fn upgrade(&self) -> Option<MyArc<T>> {
            let mut n = self.data().data_ref_count.load(Ordering::Relaxed);

            loop {
                // if there are no more strong references left, then no upgrade is available.
                if n == 0 {
                    return None;
                }

                assert!(n < usize::MAX);

                // comosre exchange to ensure that the ref count has not changed since we read to
                // ensure the ref count has not been updated by another thread in the meantime.
                if let Err(e) = self.data().data_ref_count.compare_exchange_weak(
                    n,
                    n + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    n = e;
                    continue;
                }

                // when the compare and exchange suceeds
                return Some(MyArc { weak: self.clone() });
            }
        }
    }

    impl<T> Clone for WeakRef<T> {
        fn clone(&self) -> WeakRef<T> {
            // ensure there is a valid
            unsafe { self.arc_ptr.as_ref().increment_alloc_ref_count() };
            WeakRef {
                arc_ptr: self.arc_ptr,
            }
        }
    }

    impl<T> Drop for WeakRef<T> {
        fn drop(&mut self) {
            // when the allocation count goes to 0, then we drop the underlying data held in the
            // ArcData<T>, as there a no more strong references.
            if unsafe { self.arc_ptr.as_ref().decrement_alloc_count() } > usize::MAX {
                fence(Ordering::Acquire);
                unsafe { drop(Box::from_raw(self.arc_ptr.as_ptr())) }
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

    #[test]
    fn test_basic_arc() {
        use basic_arc::MyArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NUM_DROPS: AtomicUsize = AtomicUsize::new(0);

        struct DetectDrop;

        impl Drop for DetectDrop {
            fn drop(&mut self) {
                NUM_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let x = MyArc::new((1, DetectDrop));
        let y = x.clone();

        let t = std::thread::spawn(move || assert_eq!(x.0, 1));

        assert_eq!(y.0, 1);

        t.join().unwrap();

        // at this point, the ref count should be 1, thus the data inside the arc should have not
        // been dropped yet. Thus the drop count should be 0.
        assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 0);

        // drop remaining ref.
        drop(y);

        // once we drop the remaining ref, the ref count should be 0. Thus, the data inside the Arc
        // should be dropped.
        assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_arc_weak_ref() {
        use arc_with_weak_pointers::MyArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NUM_DROPS: AtomicUsize = AtomicUsize::new(0);

        struct DetectDrop;

        impl Drop for DetectDrop {
            fn drop(&mut self) {
                NUM_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let x = MyArc::new((1, DetectDrop));

        // create 2 weak refs
        let y = MyArc::downgrade(&x);
        let z = MyArc::downgrade(&x);

        let t = std::thread::spawn(move || {
            // upgrade one of the weak refs
            // this is valid given that the strong count is 1.
            let y = y.upgrade().unwrap();
            // the value loaded should be the same loaded into the initial Arc
            assert_eq!(y.0, 1);
        });

        assert_eq!(x.0, 1);
        t.join().unwrap();

        // data should not be dropped yet.
        assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 0);
        // upgrade should be valid given that at least one arc is still valid.
        assert!(z.upgrade().is_some());

        // dropping the last live Arc.
        drop(x);

        // data should be dropped at this point.
        assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 1);

        // upgrade should not work anymore given that the data has been dropped.
        assert!(z.upgrade().is_none());
    }
}
