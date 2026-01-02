// SPDX-License-Identifier: GPL-2.0

//! Contended-lock backoff and transaction helpers for wound/wait mutexes.

use super::{
    AcquireCtx,
    Class,
    Mutex,
    MutexGuard, //
};
use crate::{
    bindings,
    prelude::*, //
};
use core::cell::Cell;

/// An [`AcquireCtx`] that remembers the contended mutex.
///
/// After [`Self::lock`] returns [`EDEADLK`], release all guards and call
/// [`Self::backoff`] before retrying. [`ww_transaction`] and
/// [`super::DynLockSet::ww_transaction`] handle this retry loop.
///
/// Mutexes must outlive this wrapper so they remain valid for retry.
///
/// # Invariants
///
/// `backoff_guard`, when present, is the sole guard for a mutex locked with `ctx`.
/// It is released before the context borrow ends. If this wrapper is forgotten,
/// `AcquireCtx`'s destructor checks for remaining locks.
pub struct AcquireCtxBackoff<'ctx, 'a> {
    ctx: Pin<&'ctx mut AcquireCtx>,
    // The cells also make `'a` invariant: recorded mutexes must outlive retries.
    contended: Cell<Option<&'a Mutex<()>>>,
    backoff_guard: Cell<Option<MutexGuard<'a, ()>>>,
}

impl<'ctx, 'a> AcquireCtxBackoff<'ctx, 'a> {
    /// Creates a wrapper for an existing [`AcquireCtx`].
    ///
    /// Preserves the context's state and ticket.
    ///
    /// # Panics
    ///
    /// Panics if the context still holds any locks, including forgotten guards.
    #[inline]
    pub fn new(ctx: Pin<&'ctx mut AcquireCtx>) -> Self {
        // INVARIANT: There is no acquired backoff lock yet.
        let this = Self {
            ctx,
            contended: Cell::new(None),
            backoff_guard: Cell::new(None),
        };
        this.assert_no_locks_held();
        this
    }

    /// Locks `mutex`, remembering it if the acquisition returns [`EDEADLK`].
    ///
    /// Further acquisitions return [`EDEADLK`] until [`Self::backoff`] succeeds.
    /// A mutex already acquired by the caller returns [`EALREADY`].
    pub fn lock<'b, T>(&'b self, mutex: &'a Mutex<T>) -> Result<MutexGuard<'b, T>> {
        if self.contended.get().is_some() {
            return Err(EDEADLK);
        }

        if let Some(guard) = self.backoff_guard.take() {
            if guard.mutex.inner.get() == mutex.inner.get() {
                // Transfer ownership of the backoff lock.
                core::mem::forget(guard);
                return Ok(MutexGuard::new(mutex));
            }
            self.backoff_guard.set(Some(guard));
        }

        match self.ctx.lock(mutex) {
            Err(e) if e == EDEADLK => {
                // SAFETY: `mutex` is initialized, remains valid for `'a`, and
                // its class is static. Only the C mutex is accessed through this view.
                let mutex = unsafe { Mutex::from_raw(mutex.inner.get()) };
                self.contended.set(Some(mutex));
                Err(e)
            }
            result => result,
        }
    }

    /// Acquires the contended mutex after the caller's guards are released.
    ///
    /// The next [`Self::lock`] for this mutex returns its guard without locking
    /// it again. The context and its ticket are preserved.
    /// Returns [`EDEADLK`] if no failed acquisition was recorded.
    ///
    /// # Panics
    ///
    /// Panics if any caller-owned locks are still held, including forgotten guards.
    pub fn backoff(&self) -> Result {
        drop(self.backoff_guard.take());
        self.assert_no_locks_held();
        let mutex = self.contended.take().ok_or(EDEADLK)?;

        // With no locks held, regular locking waits for this mutex to become available.
        // INVARIANT: The guard stays in this wrapper until transferred or dropped,
        // and `Drop` releases it before the borrow of the context ends.
        let guard = super::lock(mutex, Some(&self.ctx))?;
        self.backoff_guard.set(Some(guard));
        Ok(())
    }

    /// Marks acquisition complete; no more locks may be acquired with this context.
    #[inline]
    pub fn done(&self) {
        self.ctx.done();
    }

    #[inline]
    pub(super) fn take_contended_guard(&self) -> Option<MutexGuard<'_, ()>> {
        self.backoff_guard.take()
    }

    #[inline]
    pub(super) fn assert_no_locks_held(&self) {
        // SAFETY: This initialized context belongs to the current task.
        let acquired = unsafe { (*self.ctx.inner.get()).acquired };
        assert_eq!(acquired, 0, "ww transaction still holds caller-owned locks");
    }

    #[inline]
    pub(super) fn clear_backoff(&self) {
        drop(self.backoff_guard.take());
        self.contended.set(None);
    }

    pub(super) fn restart(&self) {
        self.clear_backoff();
        self.assert_no_locks_held();
        let ptr = self.ctx.inner.get();
        // SAFETY: This wrapper exclusively borrows the initialized, pinned context.
        // No acquired locks remain, so it can be finalized and reinitialized with
        // its static class.
        unsafe {
            let class = (*ptr).ww_class;
            bindings::ww_acquire_fini(ptr);
            bindings::ww_acquire_init(ptr, class);
        }
    }
}

impl Drop for AcquireCtxBackoff<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        // Release the backoff lock before ending the context borrow.
        drop(self.backoff_guard.take());
    }
}

/// Runs a locking algorithm, backing off and retrying on [`EDEADLK`].
///
/// The callback must immediately propagate acquisition errors and drop its guards
/// on return. It may use the protected data once all needed locks are acquired.
/// The context is stack allocated, and retries keep the same ticket. Mutexes must
/// outlive the transaction. Do not nest transactions on the same context.
///
/// # Examples
///
/// ```
/// use kernel::{
///     define_ww_class,
///     prelude::*,
///     sync::lock::ww_mutex::{
///         ww_transaction,
///         Mutex, //
///     }, //
/// };
/// use pin_init::stack_pin_init;
///
/// define_ww_class!(CLASS);
/// stack_pin_init!(let first = Mutex::new(1, &CLASS));
/// stack_pin_init!(let second = Mutex::new(2, &CLASS));
/// ww_transaction(&CLASS, |ctx| {
///     let mut first = ctx.lock(&first)?;
///     let mut second = ctx.lock(&second)?;
///     core::mem::swap(&mut *first, &mut *second);
///     Ok(())
/// })?;
/// # Ok::<(), Error>(())
/// ```
pub fn ww_transaction<'a, R>(
    class: &'static Class,
    mut transaction: impl FnMut(&AcquireCtxBackoff<'_, 'a>) -> Result<R>,
) -> Result<R> {
    pin_init::stack_pin_init!(let acquire_ctx = AcquireCtx::new(class));
    let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
    retry(&ctx, || transaction(&ctx))
}

pub(super) fn retry<R>(
    ctx: &AcquireCtxBackoff<'_, '_>,
    mut transaction: impl FnMut() -> Result<R>,
) -> Result<R> {
    loop {
        match transaction() {
            Err(e) if e == EDEADLK => ctx.backoff()?,
            result => return result,
        }
    }
}

#[kunit_tests(rust_kernel_ww_transaction)]
mod tests {
    use super::super::DynLockSet;
    use super::*;
    use crate::define_ww_class;
    use pin_init::stack_pin_init;

    define_ww_class!(TEST_TRANSACTION_CLASS);

    #[test]
    fn test_transaction_retry() -> Result {
        stack_pin_init!(let first = Mutex::new(10, &TEST_TRANSACTION_CLASS));
        stack_pin_init!(let second = Mutex::new(20, &TEST_TRANSACTION_CLASS));
        let mut attempts = 0;
        let mut identity = None;

        let values = ww_transaction(&TEST_TRANSACTION_CLASS, |ctx| {
            attempts += 1;
            let ptr = ctx.ctx.inner.get();
            // SAFETY: The transaction owns this initialized context throughout
            // the callback, and this task is its only user.
            let stamp = unsafe { (*ptr).stamp };

            if attempts == 1 {
                identity = Some((ptr, stamp));
                let _guard = ctx.lock(&first)?;
                // Simulate a failed acquisition with its contended mutex recorded.
                // SAFETY: `second` is initialized, pinned, and outlives the transaction.
                let mutex = unsafe { Mutex::from_raw(second.inner.get()) };
                ctx.contended.set(Some(mutex));
                return Err(EDEADLK);
            }

            assert_eq!(attempts, 2);
            assert_eq!(identity, Some((ptr, stamp)));
            assert!(!first.is_locked());
            assert!(second.is_locked());

            // Request the contended mutex after acquiring another lock.
            let mut a = ctx.lock(&first)?;
            let mut b = ctx.lock(&second)?;
            assert_eq!(ctx.lock(&second).err(), Some(EALREADY));
            core::mem::swap(&mut *a, &mut *b);
            Ok((*a, *b))
        })?;

        assert_eq!(attempts, 2);
        assert_eq!(values, (20, 10));
        assert!(!first.is_locked());
        assert!(!second.is_locked());
        Ok(())
    }

    #[test]
    fn test_backoff_lock_cleanup() -> Result {
        stack_pin_init!(let first = Mutex::new(1, &TEST_TRANSACTION_CLASS));
        stack_pin_init!(let contended = Mutex::new(2, &TEST_TRANSACTION_CLASS));

        // Release the unclaimed backoff lock on both success and error.
        for fail in [false, true] {
            let mut attempts = 0;
            let result = ww_transaction(&TEST_TRANSACTION_CLASS, |ctx| {
                attempts += 1;
                let _guard = ctx.lock(&first)?;
                if attempts == 1 {
                    // SAFETY: `contended` is initialized, pinned, and outlives the transaction.
                    let mutex = unsafe { Mutex::from_raw(contended.inner.get()) };
                    ctx.contended.set(Some(mutex));
                    return Err(EDEADLK);
                }

                assert_eq!(attempts, 2);
                assert!(contended.is_locked());
                if fail {
                    Err(EINVAL)
                } else {
                    Ok(())
                }
            });

            assert_eq!(result, if fail { Err(EINVAL) } else { Ok(()) });
            assert_eq!(attempts, 2);
            assert!(!first.is_locked());
            assert!(!contended.is_locked());
        }
        Ok(())
    }

    #[test]
    fn test_deadlock_without_contention() -> Result {
        stack_pin_init!(let mutex = Mutex::new(5, &TEST_TRANSACTION_CLASS));
        let mut attempts = 0;

        let result: Result = ww_transaction(&TEST_TRANSACTION_CLASS, |ctx| {
            attempts += 1;
            let _guard = ctx.lock(&mutex)?;
            // A callback-generated EDEADLK cannot identify a lock to wait for.
            Err(if attempts == 1 { EDEADLK } else { EINVAL })
        });

        assert_eq!(result, Err(EDEADLK));
        assert_eq!(attempts, 1);
        assert!(!mutex.is_locked());
        Ok(())
    }

    #[test]
    fn test_dynamic_retry() -> Result {
        stack_pin_init!(let first = Mutex::new(10, &TEST_TRANSACTION_CLASS));
        stack_pin_init!(let second = Mutex::new(20, &TEST_TRANSACTION_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_TRANSACTION_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
        let mut set = DynLockSet::new(&ctx);
        let mut attempts = 0;
        let mut identity = None;

        let result = set.ww_transaction(
            |set| {
                attempts += 1;
                let ptr = ctx.ctx.inner.get();
                // SAFETY: The pinned context is initialized and used only by
                // this task throughout the transaction.
                let stamp = unsafe { (*ptr).stamp };
                if attempts == 1 {
                    identity = Some((ptr, stamp));
                    set.lock(&first)?;
                    // SAFETY: `second` is initialized, pinned, and outlives the transaction.
                    let mutex = unsafe { Mutex::from_raw(second.inner.get()) };
                    ctx.contended.set(Some(mutex));
                    return Err(EDEADLK);
                }

                assert_eq!(attempts, 2);
                assert_eq!(identity, Some((ptr, stamp)));
                assert!(!first.is_locked());
                // Backoff makes the contended mutex available before lock().
                assert_eq!(set.with_locked(&second, |v| *v)?, 20);
                set.lock(&second)?;
                set.lock(&first)
            },
            |set| {
                set.with_locked(&first, |v| *v += 1)?;
                set.with_locked(&second, |v| {
                    *v += 2;
                    *v
                })
            },
        )?;

        assert_eq!(result, 22);
        assert_eq!(attempts, 2);
        assert!(!first.is_locked());
        assert!(!second.is_locked());
        Ok(())
    }

    #[test]
    fn test_dynamic_error_cleanup() -> Result {
        stack_pin_init!(let first = Mutex::new(1, &TEST_TRANSACTION_CLASS));
        stack_pin_init!(let contended = Mutex::new(2, &TEST_TRANSACTION_CLASS));
        stack_pin_init!(let acquire_ctx = AcquireCtx::new(&TEST_TRANSACTION_CLASS));
        let ctx = AcquireCtxBackoff::new(acquire_ctx.as_mut());
        let mut set = DynLockSet::new(&ctx);

        for swallow in [false, true] {
            let mut work_ran = false;
            let result = set.ww_transaction(
                |set| {
                    set.lock(&first)?;
                    // A swallowed or mapped EDEADLK must not poison the next call.
                    // SAFETY: `contended` is initialized, pinned, and outlives the transaction.
                    let mutex = unsafe { Mutex::from_raw(contended.inner.get()) };
                    ctx.contended.set(Some(mutex));
                    assert_eq!(set.lock(&first), Err(EDEADLK));
                    if swallow {
                        Ok(())
                    } else {
                        Err(EINVAL)
                    }
                },
                |set| {
                    work_ran = true;
                    ctx.done();
                    set.with_locked(&first, |v| *v += 1)
                },
            );

            assert_eq!(result, if swallow { Ok(()) } else { Err(EINVAL) });
            assert_eq!(work_ran, swallow);
            assert!(ctx.contended.get().is_none());
            assert!(!first.is_locked());
            assert!(!contended.is_locked());

            set.ww_transaction(
                |set| {
                    set.lock(&contended)?;
                    set.lock(&first)
                },
                |set| set.with_locked(&contended, |v| *v += 1),
            )?;
            assert!(!first.is_locked());
            assert!(!contended.is_locked());
        }
        Ok(())
    }
}
