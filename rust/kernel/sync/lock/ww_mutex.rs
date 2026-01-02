// SPDX-License-Identifier: GPL-2.0

//! Rust abstractions for the kernel's wound-wait locking primitives.
//!
//! It is designed to avoid deadlocks when locking multiple [`Mutex`]es
//! that belong to the same [`Class`]. Each lock acquisition uses an
//! [`AcquireCtx`] to track ordering and ensure forward progress.
//!
//! [`ww_transaction`] provides automatic deadlock recovery for ordinary guards.
//! Use [`DynLockSet::ww_transaction`] for dynamically discovered locks and
//! separate acquisition and work callbacks.
//!
//! See srctree/Documentation/locking/ww-mutex-design.rst for more details.

use crate::{
    bindings,
    container_of,
    error::to_result,
    prelude::*,
    types::{
        NotThreadSafe,
        Opaque, //
    }, //
};
use core::cell::UnsafeCell;

pub use acquire_ctx::AcquireCtx;
pub use acquire_ctx_backoff::{
    ww_transaction,
    AcquireCtxBackoff, //
};
pub use class::Class;
pub use dyn_lock_set::DynLockSet;

mod acquire_ctx;
mod acquire_ctx_backoff;
mod class;
mod dyn_lock_set;

/// A wound-wait (ww) mutex that is powered with deadlock avoidance
/// when acquiring multiple locks of the same [`Class`].
///
/// Each mutex belongs to a [`Class`], which the wound-wait algorithm
/// uses to figure out the order of acquisition and prevent deadlocks.
///
/// # Examples
///
/// ```
/// use kernel::{
///     define_ww_class,
///     sync::{
///         lock::ww_mutex::{
///             AcquireCtx,
///             Class,
///             Mutex, //
///         },
///         Arc,
///     },
/// };
/// use pin_init::stack_pin_init;
///
/// define_ww_class!(SOME_WW_CLASS);
///
/// let mutex = Arc::pin_init(Mutex::new(42, &SOME_WW_CLASS), GFP_KERNEL)?;
/// let ctx = KBox::pin_init(AcquireCtx::new(&SOME_WW_CLASS), GFP_KERNEL)?;
///
/// let guard = ctx.lock(&mutex)?;
/// assert_eq!(*guard, 42);
///
/// # Ok::<(), Error>(())
/// ```
#[pin_data]
#[repr(C)]
pub struct Mutex<T: ?Sized> {
    #[pin]
    inner: Opaque<bindings::ww_mutex>,
    data: UnsafeCell<T>,
}

impl<T> Mutex<T> {
    /// Initializes [`Mutex`] with the given `data` and [`Class`].
    #[inline]
    pub fn new(data: T, class: &'static Class) -> impl PinInit<Self> {
        let class_ptr = class.inner.get();
        pin_init!(Mutex {
            inner <- Opaque::ffi_init(|slot: *mut bindings::ww_mutex| {
                // SAFETY: `class` is valid indefinitely.
                unsafe { bindings::ww_mutex_init(slot, class_ptr) }
            }),
            data: UnsafeCell::new(data),
        })
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Checks if this [`Mutex`] is currently locked.
    ///
    /// The returned value is racy as another thread can acquire
    /// or release the lock immediately after this call returns.
    #[inline]
    pub fn is_locked(&self) -> bool {
        // SAFETY: It's safe to call `ww_mutex_is_locked` on
        // a valid mutex.
        unsafe { bindings::ww_mutex_is_locked(self.inner.get()) }
    }

    /// Locks this [`Mutex`] without [`AcquireCtx`].
    #[inline]
    pub fn lock(&self) -> Result<MutexGuard<'_, T>> {
        lock(self, None)
    }

    /// Similar to [`Self::lock`], but can be interrupted by signals.
    #[inline]
    pub fn lock_interruptible(&self) -> Result<MutexGuard<'_, T>> {
        lock_interruptible(self, None)
    }

    /// Tries to lock this [`Mutex`] with no [`AcquireCtx`] and without blocking.
    ///
    /// Returns `None` if the mutex could not be acquired.
    #[must_use = "if unused, the lock will be immediately unlocked"]
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        try_lock(self, None)
    }
}

impl Mutex<()> {
    /// Creates a [`Mutex`] from a raw pointer.
    ///
    /// This function is intended for interoperability with C code.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// - `ptr` is a valid pointer to a `ww_mutex`.
    /// - `ptr` must remain valid for the lifetime `'a`.
    /// - The `ww_class` associated with this mutex must be valid indefinitely.
    #[inline]
    pub unsafe fn from_raw<'a>(ptr: *mut bindings::ww_mutex) -> &'a Self {
        // SAFETY: By the safety contract, the caller guarantees that `ptr`
        // points to a valid `ww_mutex` which is the `inner` field of `Mutex`,
        // that it remains valid for the lifetime `'a` and the associated
        // `ww_class` remains valid indefinitely.
        //
        // Because [`Mutex`] is `#[repr(C)]`, the `inner` field sits at a
        // stable offset that `container_of!` can safely rely on.
        unsafe { &*container_of!(Opaque::cast_from(ptr), Self, inner) }
    }
}

// SAFETY: `Mutex` can be sent to another thread if the protected
// data `T` can be.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

// SAFETY: `Mutex` serialises access to the protected data, so it is
// `Sync` as long as the data is `Send`.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

/// A guard that provides exclusive access to the data protected
/// by a [`Mutex`].
///
/// # Invariants
///
/// The guard holds an exclusive lock on the associated [`Mutex`]. The lock is held
/// for the entire lifetime of this guard and is automatically released when the
/// guard is dropped.
#[must_use = "the lock unlocks immediately when the guard is unused"]
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    _not_send: NotThreadSafe,
}

impl<'a, T: ?Sized> MutexGuard<'a, T> {
    /// Creates a new guard for the given [`Mutex`].
    #[inline]
    fn new(mutex: &'a Mutex<T>) -> Self {
        debug_assert!(mutex.is_locked());

        // INVARIANT: Locking and `from_raw` ensure that the current task holds the mutex
        // and transfer sole responsibility for unlocking it to this guard.
        Self {
            mutex,
            _not_send: NotThreadSafe,
        }
    }
}

impl<'a> MutexGuard<'a, ()> {
    /// Creates a [`MutexGuard`] from a raw pointer.
    ///
    /// This function is intended for interoperability with C code.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// - `ptr` is a valid pointer to a `ww_mutex`.
    /// - `ptr` must remain valid for the lifetime `'b`.
    /// - The `ww_class` associated with the `ww_mutex` must be valid indefinitely.
    /// - The current task holds the mutex.
    /// - The caller transfers responsibility for unlocking the mutex to the returned guard
    ///   and must not unlock it separately (including through another guard).
    /// - Any associated acquire context remains valid until the mutex is unlocked,
    ///   even if the returned guard is forgotten. The context must not be finalized or
    ///   reinitialized before then.
    #[inline]
    pub unsafe fn from_raw<'b>(ptr: *mut bindings::ww_mutex) -> MutexGuard<'b, ()> {
        // SAFETY: By this function's safety contract, the caller guarantees that `ptr` points to a
        // valid `ww_mutex` which is the `inner` field of a `Mutex`. The caller also guarantees
        // that `ptr` is valid for the lifetime `'b` and its `ww_class` is valid indefinitely.
        let mutex = unsafe { Mutex::from_raw(ptr) };

        MutexGuard::new(mutex)
    }
}

impl<T: ?Sized> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: self.mutex is locked, so we have exclusive access.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized + Unpin> core::ops::DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: self.mutex is locked, so we have exclusive access.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: self.mutex is locked and are about to release it.
        unsafe { bindings::ww_mutex_unlock(self.mutex.inner.get()) };
    }
}

// SAFETY: `MutexGuard` can be shared between threads if the data can.
unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}

/// Locks a mutex with an optional acquire context.
#[inline]
fn lock<'a, T: ?Sized>(mutex: &'a Mutex<T>, ctx: Option<&AcquireCtx>) -> Result<MutexGuard<'a, T>> {
    let ctx_ptr = match ctx {
        Some(ctx) => {
            ctx.validate_mutex(mutex)?;
            ctx.inner.get()
        }
        None => core::ptr::null_mut(),
    };

    // SAFETY: `mutex` is pinned and valid. `ctx_ptr` is null or refers to a pinned context of
    // the same class. Callers keep the context valid until the mutex is unlocked.
    let ret = unsafe { bindings::ww_mutex_lock(mutex.inner.get(), ctx_ptr) };
    to_result(ret)?;

    Ok(MutexGuard::new(mutex))
}

/// Locks a mutex with an optional acquire context, allowing interruption by signals.
#[inline]
fn lock_interruptible<'a, T: ?Sized>(
    mutex: &'a Mutex<T>,
    ctx: Option<&AcquireCtx>,
) -> Result<MutexGuard<'a, T>> {
    let ctx_ptr = match ctx {
        Some(ctx) => {
            ctx.validate_mutex(mutex)?;
            ctx.inner.get()
        }
        None => core::ptr::null_mut(),
    };

    // SAFETY: `mutex` is pinned and valid. `ctx_ptr` is null or refers to a pinned context of
    // the same class. Callers keep the context valid until the mutex is unlocked.
    let ret = unsafe { bindings::ww_mutex_lock_interruptible(mutex.inner.get(), ctx_ptr) };
    to_result(ret)?;

    Ok(MutexGuard::new(mutex))
}

/// Tries to lock a mutex with an optional acquire context without blocking.
#[inline]
fn try_lock<'a, T: ?Sized>(
    mutex: &'a Mutex<T>,
    ctx: Option<&AcquireCtx>,
) -> Option<MutexGuard<'a, T>> {
    let ctx_ptr = match ctx {
        Some(ctx) => {
            ctx.validate_mutex(mutex).ok()?;
            ctx.inner.get()
        }
        None => core::ptr::null_mut(),
    };

    // SAFETY: `mutex` is pinned and valid. `ctx_ptr` is null or refers to a pinned context of
    // the same class. Callers keep the context valid until the mutex is unlocked.
    let ret = unsafe { bindings::ww_mutex_trylock(mutex.inner.get(), ctx_ptr) };

    if ret != 0 {
        Some(MutexGuard::new(mutex))
    } else {
        None
    }
}

#[kunit_tests(rust_kernel_ww_mutex)]
mod tests {
    use super::*;
    use crate::{
        define_wd_class,
        define_ww_class,
        sync::Arc, //
    };

    define_ww_class!(TEST_WOUND_WAIT_CLASS);
    define_wd_class!(TEST_WAIT_DIE_CLASS);

    #[test]
    fn test_ww_mutex_basic_lock_unlock() -> Result {
        let mutex = Arc::pin_init(Mutex::new(42, &TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;
        let ctx = KBox::pin_init(AcquireCtx::new(&TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;

        let guard = ctx.lock(&mutex)?;
        assert_eq!(*guard, 42);

        // Drop the lock.
        drop(guard);

        let mut guard = ctx.lock(&mutex)?;
        *guard = 100;
        assert_eq!(*guard, 100);

        Ok(())
    }

    #[test]
    fn test_ww_mutex_trylock() -> Result {
        let mutex = Arc::pin_init(Mutex::new(123, &TEST_WAIT_DIE_CLASS), GFP_KERNEL)?;
        let ctx = KBox::pin_init(AcquireCtx::new(&TEST_WAIT_DIE_CLASS), GFP_KERNEL)?;

        // `try_lock` on unlocked mutex should succeed.
        let guard = ctx.try_lock(&mutex).ok_or(EBUSY)?;
        assert_eq!(*guard, 123);

        // Now it should fail immediately as it's already locked.
        assert!(ctx.try_lock(&mutex).is_none());

        drop(guard);
        assert!(ctx.try_lock(&mutex).is_some());

        Ok(())
    }

    #[test]
    fn test_ww_mutex_class_mismatch() -> Result {
        let mutex = Arc::pin_init(Mutex::new(123, &TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;
        let ctx = KBox::pin_init(AcquireCtx::new(&TEST_WAIT_DIE_CLASS), GFP_KERNEL)?;

        assert_eq!(ctx.lock(&mutex).err(), Some(EINVAL));
        assert_eq!(ctx.lock_interruptible(&mutex).err(), Some(EINVAL));
        assert!(ctx.try_lock(&mutex).is_none());
        assert!(!mutex.is_locked());

        Ok(())
    }

    #[test]
    fn test_ww_mutex_is_locked() -> Result {
        let mutex = Arc::pin_init(Mutex::new("hello", &TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;
        let ctx = KBox::pin_init(AcquireCtx::new(&TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;

        // Should not be locked initially.
        assert!(!mutex.is_locked());

        let guard = ctx.lock(&mutex)?;
        assert!(mutex.is_locked());

        drop(guard);
        assert!(!mutex.is_locked());

        Ok(())
    }

    #[test]
    fn test_ww_acquire_context_done() -> Result {
        let mutex1 = Arc::pin_init(Mutex::new(1, &TEST_WAIT_DIE_CLASS), GFP_KERNEL)?;
        let mutex2 = Arc::pin_init(Mutex::new(2, &TEST_WAIT_DIE_CLASS), GFP_KERNEL)?;
        let ctx = KBox::pin_init(AcquireCtx::new(&TEST_WAIT_DIE_CLASS), GFP_KERNEL)?;

        // Acquire multiple mutexes with the same context.
        let guard1 = ctx.lock(&mutex1)?;
        let guard2 = ctx.lock(&mutex2)?;

        assert_eq!(*guard1, 1);
        assert_eq!(*guard2, 2);

        ctx.done();

        Ok(())
    }

    #[test]
    fn test_mutex_without_ctx() -> Result {
        let mutex = Arc::pin_init(Mutex::new(100, &TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;
        let guard = mutex.lock()?;

        assert_eq!(*guard, 100);
        assert!(mutex.is_locked());
        assert!(mutex.try_lock().is_none());

        drop(guard);
        assert!(!mutex.is_locked());

        let guard = mutex.try_lock().ok_or(EBUSY)?;
        assert_eq!(*guard, 100);
        drop(guard);
        assert!(!mutex.is_locked());

        Ok(())
    }

    #[test]
    fn test_guard_from_raw_unlocks_mutex() -> Result {
        let mutex = Arc::pin_init(Mutex::new((), &TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;
        let guard = mutex.lock()?;

        // Keep the mutex locked without leaving another guard responsible for unlocking it.
        core::mem::forget(guard);

        // SAFETY: `mutex` and its class remain valid for this test. The current task holds
        // the mutex, and forgetting the previous guard lets this guard take over unlocking it.
        let guard = unsafe { MutexGuard::from_raw(mutex.inner.get()) };
        assert!(mutex.is_locked());

        drop(guard);
        assert!(!mutex.is_locked());

        Ok(())
    }
}
