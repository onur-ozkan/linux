// SPDX-License-Identifier: GPL-2.0

//! Wound/wait locking for a set of objects discovered at runtime.
//!
//! [`DynLockSet`] keeps locks held while the caller inspects objects and finds
//! more locks to acquire. It handles repeated mutexes and provides access to
//! their protected data. [`DynLockSet::ww_transaction`] handles deadlock retries
//! and runs the work callback once all locks are held.

use super::{
    acquire_ctx_backoff::retry,
    AcquireCtxBackoff,
    Mutex,
    MutexGuard, //
};
use crate::{
    bindings,
    prelude::*,
    types::NotThreadSafe, //
};
use core::ptr::NonNull;

/// Used by [`DynLockSet`] to track acquired locks.
///
/// This type is strictly crate-private and must never be exposed
/// outside this crate.
struct RawGuard {
    mutex_ptr: NonNull<bindings::ww_mutex>,
    _not_send: NotThreadSafe,
}

impl Drop for RawGuard {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `mutex_ptr` originates from a locked `Mutex` and remains
        // valid for the lifetime of this guard, so unlocking here is sound.
        unsafe { bindings::ww_mutex_unlock(self.mutex_ptr.as_ptr()) };
    }
}

/// A tracked set of [`Mutex`] locks acquired with the same [`AcquireCtxBackoff`].
///
/// Keeps locks held across loops and helper calls, handles repeated mutexes and
/// provides access to their data through [`Self::with_locked`].
pub struct DynLockSet<'ctx, 'm> {
    ctx: &'ctx AcquireCtxBackoff<'ctx, 'm>,
    taken: KVec<RawGuard>,
}

impl<'ctx, 'm> DynLockSet<'ctx, 'm> {
    /// Creates an empty set using the given acquire context.
    #[inline]
    pub fn new(ctx: &'ctx AcquireCtxBackoff<'ctx, 'm>) -> Self {
        Self {
            ctx,
            taken: KVec::new(),
        }
    }

    /// Attempts to lock the given [`Mutex`] and stores a guard for it.
    ///
    /// Returns success if the mutex is already held by this set, unless deadlock
    /// recovery is pending.
    pub fn lock<T>(&mut self, mutex: &'m Mutex<T>) -> Result {
        match self.ctx.lock(mutex) {
            Ok(guard) => self.add_guard(guard),
            Err(e) if e == EALREADY => {
                // The context may hold a guard owned outside this set.
                if self
                    .taken
                    .iter()
                    .any(|guard| guard.mutex_ptr.as_ptr() == mutex.inner.get())
                {
                    Ok(())
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Acquires a dynamic set, retrying on deadlock then runs `work` once.
    ///
    /// Each call releases existing locks and starts a new transaction. Retries keep
    /// the same ticket. The set retains its allocation across retries and calls.
    ///
    /// `work` runs once after acquisition and must not acquire more locks. Its errors
    /// are returned without retrying. All locks and pending backoff state are released
    /// on return. The callbacks must not change the set's context or nest transactions
    /// on that context.
    ///
    /// # Panics
    ///
    /// Panics if the context holds caller-owned locks outside this set at a transaction
    /// boundary, including forgotten guards.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::{
    ///     define_ww_class,
    ///     prelude::*,
    ///     sync::lock::ww_mutex::{
    ///         AcquireCtx,
    ///         AcquireCtxBackoff,
    ///         DynLockSet,
    ///         Mutex, //
    ///     }, //
    /// };
    /// use pin_init::stack_pin_init;
    ///
    /// define_ww_class!(CLASS);
    ///
    /// stack_pin_init!(let first = Mutex::new(1, &CLASS));
    /// stack_pin_init!(let second = Mutex::new(2, &CLASS));
    ///
    /// stack_pin_init!(let acquire_ctx = AcquireCtx::new(&CLASS));
    /// let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
    /// let mut set = DynLockSet::new(&ctx);
    ///
    /// set.ww_transaction(
    ///     |set| {
    ///         set.lock(&first)?;
    ///
    ///         if set.with_locked(&first, |value| *value > 0)? {
    ///             set.lock(&second)?;
    ///         }
    ///
    ///         Ok(())
    ///     },
    ///     |set| set.with_locked(&first, |value| *value += 1),
    /// )?;
    /// # Ok::<(), Error>(())
    /// ```
    pub fn ww_transaction<R>(
        &mut self,
        mut acquire: impl FnMut(&mut Self) -> Result,
        work: impl FnOnce(&mut Self) -> Result<R>,
    ) -> Result<R> {
        let ctx = self.ctx;
        self.clear();
        ctx.restart();

        let result = retry(ctx, || {
            if let Some(guard) = self.ctx.take_contended_guard() {
                self.add_guard(guard)?;
            }

            let result = acquire(self);
            if result.is_err() {
                self.clear();
            }

            result
        });

        let result = result.and_then(|()| work(self));

        self.clear();
        ctx.clear_backoff();
        ctx.assert_no_locks_held();

        result
    }

    fn add_guard<T: ?Sized>(&mut self, guard: MutexGuard<'ctx, T>) -> Result {
        let raw_guard = RawGuard {
            // SAFETY: The guard holds this mutex which remains valid for `'ctx`.
            mutex_ptr: unsafe { NonNull::new_unchecked(guard.mutex.inner.get()) },
            _not_send: NotThreadSafe,
        };

        // Transfer unlocking to `raw_guard` before the fallible push so failure
        // drops only one guard and unlocks the mutex exactly once.
        core::mem::forget(guard);
        self.taken.push(raw_guard, GFP_KERNEL)?;

        Ok(())
    }

    /// Executes `access` with a mutable reference to the data behind [`Mutex`].
    ///
    /// Fails with [`EINVAL`] if the [`Mutex`] was not locked in this set.
    pub fn with_locked<T: Unpin, Y>(
        &mut self,
        mutex: &'m Mutex<T>,
        access: impl for<'b> FnOnce(&'b mut T) -> Y,
    ) -> Result<Y> {
        let mutex_ptr = mutex.inner.get();

        if self
            .taken
            .iter()
            .any(|guard| guard.mutex_ptr.as_ptr() == mutex_ptr)
        {
            // SAFETY: We hold the lock corresponding to `mutex` so we have
            // exclusive access to its protected data.
            let value = unsafe { &mut *mutex.data.get() };

            Ok(access(value))
        } else {
            // `mutex` isn't locked in this set.
            Err(EINVAL)
        }
    }

    /// Releases all locks in the set.
    #[inline]
    pub fn clear(&mut self) {
        // `Drop` implementation of the `RawGuard` takes care of the unlocking.
        self.taken.clear();
    }
}

impl Drop for DynLockSet<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        self.clear();
    }
}

#[kunit_tests(rust_kernel_dyn_lock_set)]
mod tests {
    use super::super::AcquireCtx;
    use super::*;
    use crate::{
        define_wd_class,
        define_ww_class, //
    };
    use pin_init::stack_pin_init;

    define_ww_class!(TEST_WOUND_WAIT_CLASS);
    define_wd_class!(TEST_WAIT_DIE_CLASS);

    #[test]
    fn test_lock_and_clear() -> Result {
        stack_pin_init!(let mutex = Mutex::new(10, &TEST_WOUND_WAIT_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_WOUND_WAIT_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
        let mut set = DynLockSet::new(&ctx);

        assert_eq!(set.taken.capacity(), 0);

        set.lock(&mutex)?;
        set.lock(&mutex)?;
        assert_eq!(set.taken.len(), 1);

        set.with_locked(&mutex, |v| *v += 7)?;
        assert_eq!(set.with_locked(&mutex, |v| *v)?, 17);

        let capacity = set.taken.capacity();
        assert!(capacity > 0);

        set.clear();
        assert!(!mutex.is_locked());
        assert_eq!(set.taken.len(), 0);
        assert_eq!(set.taken.capacity(), capacity);

        Ok(())
    }

    #[test]
    fn test_drop_unlocks() -> Result {
        stack_pin_init!(let first = Mutex::new(1, &TEST_WOUND_WAIT_CLASS));
        stack_pin_init!(let second = Mutex::new(2, &TEST_WOUND_WAIT_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_WOUND_WAIT_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());

        {
            let mut set = DynLockSet::new(&ctx);

            set.lock(&first)?;
            set.lock(&second)?;
        }

        assert!(!first.is_locked());
        assert!(!second.is_locked());

        Ok(())
    }

    #[test]
    fn test_context_reuse() -> Result {
        stack_pin_init!(let mutex = Mutex::new(5, &TEST_WOUND_WAIT_CLASS));
        let mut acquire_ctx = KBox::pin_init(AcquireCtx::new(&TEST_WOUND_WAIT_CLASS), GFP_KERNEL)?;

        {
            let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
            let mut set = DynLockSet::new(&ctx);

            set.ww_transaction(
                |set| set.lock(&mutex),
                |set| set.with_locked(&mutex, |value| *value += 1),
            )?;

            assert!(!mutex.is_locked());
        }

        let guard = acquire_ctx.lock(&mutex)?;
        assert_eq!(*guard, 6);

        Ok(())
    }

    #[test]
    fn test_reject_external_guard() -> Result {
        stack_pin_init!(let mutex = Mutex::new(5, &TEST_WOUND_WAIT_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_WOUND_WAIT_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
        let guard = ctx.lock(&mutex)?;
        let mut set = DynLockSet::new(&ctx);

        assert_eq!(set.lock(&mutex), Err(EALREADY));
        assert_eq!(set.with_locked(&mutex, |v| *v), Err(EINVAL));

        set.clear();
        assert!(mutex.is_locked());
        assert_eq!(*guard, 5);

        drop(guard);
        assert!(!mutex.is_locked());

        Ok(())
    }

    #[test]
    fn test_access_without_lock() -> Result {
        stack_pin_init!(let mutex = Mutex::new(5, &TEST_WOUND_WAIT_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_WOUND_WAIT_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
        let mut set = DynLockSet::new(&ctx);

        assert_eq!(set.with_locked(&mutex, |v| *v), Err(EINVAL));

        Ok(())
    }

    #[test]
    fn test_class_mismatch() -> Result {
        stack_pin_init!(let mutex = Mutex::new(5, &TEST_WOUND_WAIT_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_WAIT_DIE_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
        let mut set = DynLockSet::new(&ctx);

        assert_eq!(set.lock(&mutex), Err(EINVAL));
        assert!(!mutex.is_locked());

        Ok(())
    }
}
