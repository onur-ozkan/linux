// SPDX-License-Identifier: GPL-2.0

//! Provides [`AcquireCtx`] for managing multiple wound/wait
//! mutexes from the same [`Class`].

use super::{
    lock,
    lock_interruptible,
    try_lock,
    Class,
    Mutex,
    MutexGuard, //
};
use crate::{
    bindings,
    prelude::*,
    types::Opaque, //
};

/// Groups multiple [`Mutex`]es for deadlock avoidance when acquired
/// with the same [`Class`].
///
/// Dropping the context panics if any mutex acquired with it is still locked.
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
/// // Create mutexes.
/// let mutex1 = Arc::pin_init(Mutex::new(1, &SOME_WW_CLASS), GFP_KERNEL)?;
/// let mutex2 = Arc::pin_init(Mutex::new(2, &SOME_WW_CLASS), GFP_KERNEL)?;
///
/// // Create acquire context for deadlock avoidance.
/// let ctx = KBox::pin_init(AcquireCtx::new(&SOME_WW_CLASS), GFP_KERNEL)?;
///
/// let guard1 = ctx.lock(&mutex1)?;
/// let guard2 = ctx.lock(&mutex2)?;
///
/// // Mark acquisition phase as complete.
/// ctx.done();
///
/// # Ok::<(), Error>(())
/// ```
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct AcquireCtx {
    #[pin]
    pub(super) inner: Opaque<bindings::ww_acquire_ctx>,
}

impl AcquireCtx {
    /// Initializes a new [`AcquireCtx`] with the given [`Class`].
    #[inline]
    pub fn new(class: &'static Class) -> impl PinInit<Self> {
        let class_ptr = class.inner.get();
        pin_init!(AcquireCtx {
            inner <- Opaque::ffi_init(|slot: *mut bindings::ww_acquire_ctx| {
                // SAFETY: `class` is valid indefinitely.
                unsafe { bindings::ww_acquire_init(slot, class_ptr) }
            }),
        })
    }

    /// Creates a [`AcquireCtx`] from a raw pointer.
    ///
    /// This function is intended for interoperability with C code.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` points to the `inner` field of an initialized
    /// [`AcquireCtx`] belonging to the current task. The context must remain pinned
    /// and valid for the lifetime `'a` and until all mutexes acquired with it are
    /// unlocked, even if their guards are forgotten. It must not be finalized or
    /// reinitialized during that time.
    /// The associated `ww_class` must be valid indefinitely.
    #[inline]
    pub unsafe fn from_raw<'a>(ptr: *mut bindings::ww_acquire_ctx) -> &'a Self {
        // SAFETY: By the safety contract, `ptr` is valid to construct `AcquireCtx`.
        unsafe { &*ptr.cast() }
    }

    /// Marks the end of the acquire phase.
    ///
    /// Calling this function is optional. It is just useful to document
    /// the code and clearly designated the acquire phase from actually
    /// using the locked data structures.
    ///
    /// After calling this function, no more mutexes can be acquired with
    /// this context.
    #[inline]
    pub fn done(&self) {
        // SAFETY: `self.inner` contains a valid, initialized acquire context.
        unsafe { bindings::ww_acquire_done(self.inner.get()) };
    }

    /// Locks the given [`Mutex`] on this [`AcquireCtx`].
    #[inline]
    pub fn lock<'a, T>(&'a self, mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>> {
        lock(mutex, Some(self))
    }

    /// Similar to [`Self::lock`], but can be interrupted by signals.
    #[inline]
    pub fn lock_interruptible<'a, T>(&'a self, mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>> {
        lock_interruptible(mutex, Some(self))
    }

    /// Tries to lock the [`Mutex`] on this [`AcquireCtx`] without blocking.
    ///
    /// Unlike [`Self::lock`], no deadlock handling is performed.
    ///
    /// Returns `None` if the mutex belongs to a different [`Class`] or could not
    /// be acquired.
    #[must_use = "if unused, the lock will be immediately unlocked"]
    #[inline]
    pub fn try_lock<'a, T>(&'a self, mutex: &'a Mutex<T>) -> Option<MutexGuard<'a, T>> {
        try_lock(mutex, Some(self))
    }

    /// Checks the mutex's class and the acquired-lock count.
    ///
    /// Returns [`EINVAL`] if the [`Mutex`] has a different [`Class`] or
    /// [`EOVERFLOW`] if the acquired-lock count cannot be incremented.
    #[inline]
    pub(super) fn validate_mutex<T: ?Sized>(&self, mutex: &Mutex<T>) -> Result {
        let ctx_ptr = self.inner.get();
        let mutex_ptr = mutex.inner.get();

        // SAFETY: `ctx_ptr` is a valid pointer for the entire lifetime of `self`.
        let ctx_class = unsafe { (*ctx_ptr).ww_class };

        // SAFETY: `mutex_ptr` is a valid pointer for the entire lifetime of `mutex`.
        let mutex_class = unsafe { (*mutex_ptr).ww_class };

        // `self` and `mutex` must use the same class.
        if ctx_class != mutex_class {
            return Err(EINVAL);
        }

        // Check for overflow of the count checked by `AcquireCtx`'s destructor.
        // SAFETY: `ctx_ptr` points to a valid context owned by the current task.
        if unsafe { (*ctx_ptr).acquired } == u32::MAX {
            return Err(EOVERFLOW);
        }

        Ok(())
    }
}

#[pinned_drop]
impl PinnedDrop for AcquireCtx {
    #[inline]
    fn drop(self: Pin<&mut Self>) {
        let ptr = self.inner.get();
        // SAFETY: `ptr` points to this initialized context, owned by the current task.
        let acquired = unsafe { (*ptr).acquired };
        assert_eq!(
            acquired, 0,
            "ww acquire context dropped with locked mutexes"
        );

        // SAFETY: The assertion above ensures that all acquired locks were released.
        unsafe { bindings::ww_acquire_fini(ptr) };
    }
}
