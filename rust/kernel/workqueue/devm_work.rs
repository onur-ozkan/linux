// SPDX-License-Identifier: GPL-2.0

//! Device-bound work items.

use super::{DelayedWork, Queue, RawDelayedWorkItem, RawWorkItem, Work, WorkItem, WorkItemPointer};
use crate::{
    bindings,
    device::{Bound, Device},
    devres,
    prelude::*,
    sync::{Arc, LockClassKey},
    time::Jiffies,
};
use core::convert::Infallible;

/// Work item tied to a device's bound lifetime.
pub trait ScopedWorkItem: Send + Sync {
    /// Runs the work item while the device is still guaranteed to be bound.
    fn run(&self, dev: &Device<Bound>);
}

/// Pointer to a scoped work item.
pub struct ScopedWorkPointer<'bound, T: ScopedWorkItem, const ID: u64 = 0>(
    &'bound ScopedWork<'bound, T, ID>,
);

// SAFETY: `__enqueue` passes the embedded `work_struct` in `ScopedWork` to the queueing closure.
// `ScopedWork` cancels and flushes the work item from its pinned destructor, so the callback cannot
// run after the scoped object is dropped.
unsafe impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> RawWorkItem<ID>
    for ScopedWorkPointer<'bound, T, ID>
{
    type EnqueueOutput = bool;

    unsafe fn __enqueue<F>(self, queue_work_on: F) -> Self::EnqueueOutput
    where
        F: FnOnce(*mut bindings::work_struct) -> bool,
    {
        // SAFETY: The pointer is derived from a live `ScopedWork` reference.
        let work = unsafe { Work::raw_get(core::ptr::addr_of!(self.0.work)) };

        queue_work_on(work)
    }
}

// SAFETY: `run` reconstructs the same `ScopedWork` that provided the queued `work_struct`.
unsafe impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> WorkItemPointer<ID>
    for ScopedWorkPointer<'bound, T, ID>
{
    unsafe extern "C" fn run(ptr: *mut bindings::work_struct) {
        let ptr = ptr.cast::<Work<ScopedWork<'bound, T, ID>, ID>>();
        // SAFETY: The pointer came from the `work` field of a live `ScopedWork`.
        let ptr = unsafe {
            <ScopedWork<'bound, T, ID> as super::HasWork<ScopedWork<'bound, T, ID>, ID>>::work_container_of(ptr)
        };
        // SAFETY: By the safety contract of `RawWorkItem`, the scoped lifetime has not expired.
        let this: &'bound ScopedWork<'bound, T, ID> = unsafe { &*ptr };

        <ScopedWork<'bound, T, ID> as WorkItem<ID>>::run(ScopedWorkPointer(this));
    }
}

/// Work item tied to a device binding scope.
///
/// `ScopedWork` is intended to be embedded in driver data whose lifetime is
/// tied to `dev`. Dropping it cancels pending work and waits for a running
/// callback to finish.
#[pin_data(PinnedDrop)]
pub struct ScopedWork<'bound, T: ScopedWorkItem + 'bound, const ID: u64 = 0> {
    dev: &'bound Device<Bound>,
    #[pin]
    handler: T,
    #[pin]
    work: Work<Self, ID>,
}

crate::impl_has_work! {
    impl{'bound, T: ScopedWorkItem + 'bound, const ID: u64} HasWork<Self, ID>
        for ScopedWork<'bound, T, ID> { self.work }
}

impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> WorkItem<ID> for ScopedWork<'bound, T, ID> {
    type Pointer = ScopedWorkPointer<'bound, T, ID>;

    #[inline]
    fn run(this: Self::Pointer) {
        this.0.handler.run(this.0.dev);
    }
}

#[pinned_drop]
impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> PinnedDrop for ScopedWork<'bound, T, ID> {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: We do not move out of any pinned fields.
        let this = unsafe { self.get_unchecked_mut() };

        // SAFETY: `this.work` is a valid embedded `work_struct`.
        unsafe {
            bindings::cancel_work_sync(Work::raw_get(core::ptr::addr_of!(this.work)));
        }
    }
}

impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> ScopedWork<'bound, T, ID> {
    /// Creates a new scoped work item.
    pub fn new<E>(
        name: &'static CStr,
        key: Pin<&'static LockClassKey>,
        dev: &'bound Device<Bound>,
        handler: impl PinInit<T, E>,
    ) -> impl PinInit<Self, E>
    where
        E: From<Infallible>,
    {
        try_pin_init!(Self {
            dev,
            handler <- handler,
            work <- Work::new(name, key),
        }? E)
    }

    /// Enqueues the work item on the given queue.
    ///
    /// Returns `false` if the work item is already pending.
    pub fn enqueue(&'bound self, queue: &Queue) -> bool {
        let queue_ptr = queue.0.get();

        // SAFETY: `ScopedWorkPointer` guarantees that the work item is valid until either the
        // callback runs or the scoped work item is dropped and cancellation completes.
        unsafe {
            ScopedWorkPointer(self).__enqueue(|work| {
                bindings::queue_work_on(
                    bindings::wq_misc_consts_WORK_CPU_UNBOUND as ffi::c_int,
                    queue_ptr,
                    work,
                )
            })
        }
    }

    /// Converts an allocated scoped work item into a device-managed work item.
    ///
    /// The returned work item is cancelled during driver detach.
    pub fn into_managed(self: Arc<Self>) -> Result<ManagedWork<T, ID>>
    where
        T: 'static,
    {
        let dev = self.dev;
        let ptr = Arc::into_raw(self);
        // SAFETY: `T: 'static`, and the devres registration below cancels the work before the
        // device-bound lifetime ends.
        let inner = unsafe { Arc::from_raw(ptr.cast::<ScopedWork<'static, T, ID>>()) };

        ManagedWorkRegistration::register(dev, inner.clone())?;

        Ok(ManagedWork { inner })
    }
}

/// Pointer to a scoped delayed work item.
pub struct ScopedDelayedWorkPointer<'bound, T: ScopedWorkItem, const ID: u64 = 0>(
    &'bound ScopedDelayedWork<'bound, T, ID>,
);

// SAFETY: `__enqueue` passes the embedded `delayed_work.work` in `ScopedDelayedWork` to the
// queueing closure. `ScopedDelayedWork` cancels and flushes the work item from its pinned
// destructor, so the callback cannot run after the scoped object is dropped.
unsafe impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> RawWorkItem<ID>
    for ScopedDelayedWorkPointer<'bound, T, ID>
{
    type EnqueueOutput = bool;

    unsafe fn __enqueue<F>(self, queue_work_on: F) -> Self::EnqueueOutput
    where
        F: FnOnce(*mut bindings::work_struct) -> bool,
    {
        // SAFETY: The pointer is derived from a live `ScopedDelayedWork` reference.
        let work = unsafe { DelayedWork::raw_as_work(core::ptr::addr_of!(self.0.work)) };

        // SAFETY: The pointer came from `DelayedWork::raw_as_work` above.
        queue_work_on(unsafe { Work::raw_get(work) })
    }
}

// SAFETY: By the `RawWorkItem` implementation above, the provided `work_struct` belongs to the
// embedded `delayed_work` in `ScopedDelayedWork`.
unsafe impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> RawDelayedWorkItem<ID>
    for ScopedDelayedWorkPointer<'bound, T, ID>
{
}

// SAFETY: `run` reconstructs the same `ScopedDelayedWork` that provided the queued
// `work_struct`.
unsafe impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> WorkItemPointer<ID>
    for ScopedDelayedWorkPointer<'bound, T, ID>
{
    unsafe extern "C" fn run(ptr: *mut bindings::work_struct) {
        let ptr = ptr.cast::<Work<ScopedDelayedWork<'bound, T, ID>, ID>>();
        // SAFETY: The pointer came from the `work` field of a live `ScopedDelayedWork`.
        let ptr = unsafe {
            <ScopedDelayedWork<'bound, T, ID> as super::HasWork<
                ScopedDelayedWork<'bound, T, ID>,
                ID,
            >>::work_container_of(ptr)
        };
        // SAFETY: By the safety contract of `RawWorkItem`, the scoped lifetime has not expired.
        let this: &'bound ScopedDelayedWork<'bound, T, ID> = unsafe { &*ptr };

        <ScopedDelayedWork<'bound, T, ID> as WorkItem<ID>>::run(ScopedDelayedWorkPointer(this));
    }
}

/// Delayed work item tied to a device binding scope.
///
/// `ScopedDelayedWork` is intended to be embedded in driver data whose lifetime
/// is tied to `dev`. Dropping it cancels pending work and waits for a running
/// callback to finish.
#[pin_data(PinnedDrop)]
pub struct ScopedDelayedWork<'bound, T: ScopedWorkItem + 'bound, const ID: u64 = 0> {
    dev: &'bound Device<Bound>,
    #[pin]
    handler: T,
    #[pin]
    work: DelayedWork<Self, ID>,
}

crate::impl_has_delayed_work! {
    impl{'bound, T: ScopedWorkItem + 'bound, const ID: u64} HasDelayedWork<Self, ID>
        for ScopedDelayedWork<'bound, T, ID> { self.work }
}

impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> WorkItem<ID>
    for ScopedDelayedWork<'bound, T, ID>
{
    type Pointer = ScopedDelayedWorkPointer<'bound, T, ID>;

    #[inline]
    fn run(this: Self::Pointer) {
        this.0.handler.run(this.0.dev);
    }
}

#[pinned_drop]
impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> PinnedDrop
    for ScopedDelayedWork<'bound, T, ID>
{
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: We do not move out of any pinned fields.
        let this = unsafe { self.get_unchecked_mut() };

        // SAFETY: `this.work` is a valid embedded `delayed_work`.
        unsafe {
            let work = Work::raw_get(DelayedWork::raw_as_work(core::ptr::addr_of!(this.work)));
            bindings::cancel_delayed_work_sync(crate::container_of!(
                work,
                bindings::delayed_work,
                work
            ));
        }
    }
}

impl<'bound, T: ScopedWorkItem + 'bound, const ID: u64> ScopedDelayedWork<'bound, T, ID> {
    /// Creates a new scoped delayed work item.
    pub fn new<E>(
        work_name: &'static CStr,
        work_key: Pin<&'static LockClassKey>,
        timer_name: &'static CStr,
        timer_key: Pin<&'static LockClassKey>,
        dev: &'bound Device<Bound>,
        handler: impl PinInit<T, E>,
    ) -> impl PinInit<Self, E>
    where
        E: From<Infallible>,
    {
        try_pin_init!(Self {
            dev,
            handler <- handler,
            work <- DelayedWork::new(work_name, work_key, timer_name, timer_key),
        }? E)
    }

    /// Enqueues the delayed work item immediately on the given queue.
    ///
    /// Returns `false` if the work item is already pending.
    pub fn enqueue(&'bound self, queue: &Queue) -> bool {
        let queue_ptr = queue.0.get();

        // SAFETY: `ScopedDelayedWorkPointer` guarantees that the work item is valid until either
        // the callback runs or the scoped work item is dropped and cancellation completes.
        unsafe {
            ScopedDelayedWorkPointer(self).__enqueue(|work| {
                bindings::queue_work_on(
                    bindings::wq_misc_consts_WORK_CPU_UNBOUND as ffi::c_int,
                    queue_ptr,
                    work,
                )
            })
        }
    }

    /// Enqueues the delayed work item on the given queue after `delay`.
    ///
    /// Returns `false` if the work item is already pending.
    pub fn enqueue_delayed(&'bound self, queue: &Queue, delay: Jiffies) -> bool {
        let queue_ptr = queue.0.get();

        // SAFETY: `ScopedDelayedWorkPointer` guarantees that the work item is valid until either
        // the callback runs or the scoped work item is dropped and cancellation completes.
        unsafe {
            ScopedDelayedWorkPointer(self).__enqueue(|work| {
                bindings::queue_delayed_work_on(
                    bindings::wq_misc_consts_WORK_CPU_UNBOUND as ffi::c_int,
                    queue_ptr,
                    crate::container_of!(work, bindings::delayed_work, work),
                    delay,
                )
            })
        }
    }

    /// Converts an allocated scoped delayed work item into a device-managed delayed work item.
    ///
    /// The returned work item is cancelled during driver detach.
    pub fn into_managed(self: Arc<Self>) -> Result<ManagedDelayedWork<T, ID>>
    where
        T: 'static,
    {
        let dev = self.dev;
        let ptr = Arc::into_raw(self);
        // SAFETY: `T: 'static`, and the devres registration below cancels the work before the
        // device-bound lifetime ends.
        let inner = unsafe { Arc::from_raw(ptr.cast::<ScopedDelayedWork<'static, T, ID>>()) };

        ManagedDelayedWorkRegistration::register(dev, inner.clone())?;

        Ok(ManagedDelayedWork { inner })
    }
}

struct ManagedWorkRegistration<T: ScopedWorkItem + 'static, const ID: u64 = 0> {
    inner: Arc<ScopedWork<'static, T, ID>>,
}

impl<T: ScopedWorkItem + 'static, const ID: u64> ManagedWorkRegistration<T, ID> {
    #[inline]
    fn register(dev: &Device<Bound>, work: Arc<ScopedWork<'static, T, ID>>) -> Result {
        devres::register::<Self, Infallible>(dev, Self { inner: work }, GFP_KERNEL)
    }
}

impl<T: ScopedWorkItem + 'static, const ID: u64> Drop for ManagedWorkRegistration<T, ID> {
    fn drop(&mut self) {
        // SAFETY: `self.inner.work` is a valid embedded `work_struct`.
        unsafe {
            bindings::cancel_work_sync(Work::raw_get(core::ptr::addr_of!(self.inner.work)));
        }
    }
}

/// Device-managed work item that is cancelled during driver detach.
pub struct ManagedWork<T: ScopedWorkItem + 'static, const ID: u64 = 0> {
    inner: Arc<ScopedWork<'static, T, ID>>,
}

impl<T: ScopedWorkItem + 'static, const ID: u64> ManagedWork<T, ID> {
    /// Creates a new device-managed work item.
    pub fn new<E>(
        name: &'static CStr,
        key: Pin<&'static LockClassKey>,
        dev: &Device<Bound>,
        handler: impl PinInit<T, E>,
    ) -> Result<Self>
    where
        E: From<Infallible>,
        Error: From<E>,
    {
        // SAFETY: The managed registration cancels the work item during detach before the
        // device-bound lifetime ends.
        let dev_static =
            unsafe { core::mem::transmute::<&Device<Bound>, &'static Device<Bound>>(dev) };
        let scoped = Arc::pin_init(ScopedWork::new(name, key, dev_static, handler), GFP_KERNEL)?;

        scoped.into_managed()
    }

    /// Enqueues the work item on the given queue.
    ///
    /// Returns `false` if the work item is already pending.
    #[inline]
    pub fn enqueue(&self, queue: &Queue) -> bool {
        self.inner.enqueue(queue)
    }
}

impl<T: ScopedWorkItem + 'static, const ID: u64> Clone for ManagedWork<T, ID> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct ManagedDelayedWorkRegistration<T: ScopedWorkItem + 'static, const ID: u64 = 0> {
    inner: Arc<ScopedDelayedWork<'static, T, ID>>,
}

impl<T: ScopedWorkItem + 'static, const ID: u64> ManagedDelayedWorkRegistration<T, ID> {
    #[inline]
    fn register(dev: &Device<Bound>, work: Arc<ScopedDelayedWork<'static, T, ID>>) -> Result {
        devres::register::<Self, Infallible>(dev, Self { inner: work }, GFP_KERNEL)
    }
}

impl<T: ScopedWorkItem + 'static, const ID: u64> Drop for ManagedDelayedWorkRegistration<T, ID> {
    fn drop(&mut self) {
        // SAFETY: `self.inner.work` is a valid embedded `delayed_work`.
        unsafe {
            let work = Work::raw_get(DelayedWork::raw_as_work(core::ptr::addr_of!(
                self.inner.work
            )));
            bindings::cancel_delayed_work_sync(crate::container_of!(
                work,
                bindings::delayed_work,
                work
            ));
        }
    }
}

/// Device-managed delayed work item that is cancelled during driver detach.
pub struct ManagedDelayedWork<T: ScopedWorkItem + 'static, const ID: u64 = 0> {
    inner: Arc<ScopedDelayedWork<'static, T, ID>>,
}

impl<T: ScopedWorkItem + 'static, const ID: u64> ManagedDelayedWork<T, ID> {
    /// Creates a new device-managed delayed work item.
    pub fn new<E>(
        work_name: &'static CStr,
        work_key: Pin<&'static LockClassKey>,
        timer_name: &'static CStr,
        timer_key: Pin<&'static LockClassKey>,
        dev: &Device<Bound>,
        handler: impl PinInit<T, E>,
    ) -> Result<Self>
    where
        E: From<Infallible>,
        Error: From<E>,
    {
        // SAFETY: The managed registration cancels the work item during detach before the
        // device-bound lifetime ends.
        let dev_static =
            unsafe { core::mem::transmute::<&Device<Bound>, &'static Device<Bound>>(dev) };
        let scoped = Arc::pin_init(
            ScopedDelayedWork::new(
                work_name, work_key, timer_name, timer_key, dev_static, handler,
            ),
            GFP_KERNEL,
        )?;

        scoped.into_managed()
    }

    /// Enqueues the delayed work item immediately on the given queue.
    ///
    /// Returns `false` if the work item is already pending.
    #[inline]
    pub fn enqueue(&self, queue: &Queue) -> bool {
        self.inner.enqueue(queue)
    }

    /// Enqueues the delayed work item on the given queue after `delay`.
    ///
    /// Returns `false` if the work item is already pending.
    #[inline]
    pub fn enqueue_delayed(&self, queue: &Queue, delay: Jiffies) -> bool {
        self.inner.enqueue_delayed(queue, delay)
    }
}

impl<T: ScopedWorkItem + 'static, const ID: u64> Clone for ManagedDelayedWork<T, ID> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
