// SPDX-License-Identifier: GPL-2.0

//! Provides [`Class`] to group wound/wait mutexes to be acquired together
//! and specifies which deadlock avoidance algorithm to use (e.g., wound-wait
//! or wait-die).
//!
//! The [`define_ww_class!`] and [`define_wd_class!`] macros provide safe
//! ways to create classes.

use crate::{
    bindings,
    prelude::*,
    types::Opaque, //
};

/// Defines a static wound-wait [`Class`].
///
/// # Examples
///
/// ```
/// use kernel::define_ww_class;
///
/// define_ww_class!(SOME_WW_CLASS);
/// ```
#[macro_export]
macro_rules! define_ww_class {
    ($name:ident) => {
        static $name: $crate::sync::lock::ww_mutex::Class =
            // SAFETY: This is `static`, so address is fixed and won't move.
            unsafe {
                $crate::sync::lock::ww_mutex::Class::new_unpinned(
                    $crate::c_str!(::core::concat!(::core::stringify!($name), "_acquire")),
                    $crate::c_str!(::core::concat!(::core::stringify!($name), "_mutex")),
                    false,
                )
            };
    };
}

/// Defines a static wait-die [`Class`].
///
/// # Examples
///
/// ```
/// use kernel::define_wd_class;
///
/// define_wd_class!(SOME_WD_CLASS);
/// ```
#[macro_export]
macro_rules! define_wd_class {
    ($name:ident) => {
        static $name: $crate::sync::lock::ww_mutex::Class =
            // SAFETY: This is `static`, so address is fixed and won't move.
            unsafe {
                $crate::sync::lock::ww_mutex::Class::new_unpinned(
                    $crate::c_str!(::core::concat!(::core::stringify!($name), "_acquire")),
                    $crate::c_str!(::core::concat!(::core::stringify!($name), "_mutex")),
                    true,
                )
            };
    };
}

/// Used to group mutexes together for deadlock avoidance.
///
/// All mutexes that might be acquired together should use the same class.
///
/// # Examples
///
/// ```
/// use kernel::{
///     define_wd_class,
///     define_ww_class, //
/// };
///
/// define_ww_class!(SOME_WW_CLASS);
/// define_wd_class!(SOME_WD_CLASS);
///
/// # Ok::<(), Error>(())
/// ```
#[pin_data]
#[repr(transparent)]
pub struct Class {
    #[pin]
    pub(super) inner: Opaque<bindings::ww_class>,
}

impl Class {
    /// Creates an unpinned [`Class`].
    ///
    /// # Safety
    ///
    /// Caller must guarantee that the returned value must be pinned before
    /// its first use.
    #[doc(hidden)]
    pub const unsafe fn new_unpinned(
        acquire_name: &'static CStr,
        mutex_name: &'static CStr,
        is_wait_die: bool,
    ) -> Self {
        Class {
            inner: Opaque::new(bindings::ww_class {
                stamp: bindings::atomic_long_t { counter: 0 },
                acquire_name: crate::str::as_char_ptr_in_const_context(acquire_name),
                mutex_name: crate::str::as_char_ptr_in_const_context(mutex_name),
                is_wait_die: is_wait_die as u32,
                ..pin_init::zeroed()
            }),
        }
    }

    /// Creates a [`Class`] from a raw pointer.
    ///
    /// This function is intended for interoperability with C code.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` points to the `inner` field of
    /// [`Class`] and that it remains valid for the lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw<'a>(ptr: *mut bindings::ww_class) -> &'a Self {
        // SAFETY: By the safety contract, `ptr` is valid to construct `Class`.
        unsafe { &*ptr.cast() }
    }
}

// SAFETY: [`Class`] is immutable except for its atomic stamp. It's fine to share it across threads.
unsafe impl Sync for Class {}
// SAFETY: Doesn't hold anything thread-specific. It's safe to send to other threads.
unsafe impl Send for Class {}
