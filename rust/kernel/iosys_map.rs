// SPDX-License-Identifier: GPL-2.0

//! IO-agnostic memory mapping interfaces.
//!
//! This crate provides bindings for the `struct iosys_map` type, which provides a common interface
//! for memory mappings which can reside within coherent memory, or within IO memory.
//!
//! C header: [`include/linux/iosys-map.h`](srctree/include/linux/pci.h)

use crate::{
    error::code::*,
    io::{
        Io,
        IoCapable,
        IoKnownSize, //
    },
    prelude::*, //
};
use bindings;
use core::marker::PhantomData;

/// Raw unsized representation of a `struct iosys_map`.
///
/// This struct is a transparent wrapper around `struct iosys_map`. The C API does not provide the
/// size of the mapping by default, and thus this type also does not include the size of the
/// mapping. As such, it cannot be used for actually accessing the underlying data pointed to by the
/// mapping.
///
/// With the exception of kernel crates which may provide their own wrappers around `RawIoSysMap`,
/// users will typically not interact with this type directly.
#[repr(transparent)]
pub struct RawIoSysMap<const SIZE: usize = 0>(bindings::iosys_map);

impl<const SIZE: usize> RawIoSysMap<SIZE> {
    /// Convert from a raw `bindings::iosys_map`.
    #[allow(unused)]
    #[inline]
    pub(crate) fn from_raw(val: bindings::iosys_map) -> Self {
        Self(val)
    }

    /// Returns whether the mapping is within IO memory space or not.
    #[inline]
    pub fn is_iomem(&self) -> bool {
        self.0.is_iomem
    }

    /// Convert from a `RawIoSysMap<SIZE>` to a raw `bindings::iosys_map` ref.
    #[expect(unused)]
    #[inline]
    pub(crate) fn as_raw(&self) -> &bindings::iosys_map {
        &self.0
    }

    /// Convert from a `RawIoSysMap<SIZE>` to a raw mutable `bindings::iosys_map` ref.
    #[allow(unused)]
    #[inline]
    pub(crate) fn as_raw_mut(&mut self) -> &mut bindings::iosys_map {
        &mut self.0
    }

    /// Returns the address pointed to by this iosys map.
    ///
    /// Note that this address is not guaranteed to be valid, and may or may not reside in I/O
    /// memory.
    #[inline]
    pub fn addr(&self) -> usize {
        (if self.is_iomem() {
            // SAFETY: We confirmed above that this iosys map is contained within iomem, so it's
            // safe to read vaddr_iomem
            unsafe { self.0.__bindgen_anon_1.vaddr_iomem }
        } else {
            // SAFETY: We confirmed above that this iosys map is not contaned within iomem, so it's
            // safe to read vaddr.
            unsafe { self.0.__bindgen_anon_1.vaddr }
        }) as usize
    }
}

// SAFETY: As we make no guarantees about the validity of the mapping, there's no issue with sending
// this type between threads.
unsafe impl<const SIZE: usize> Send for RawIoSysMap<SIZE> {}

impl<const SIZE: usize> Clone for RawIoSysMap<SIZE> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

macro_rules! impl_raw_iosys_map_io_capable {
    ($ty:ty, $read_fn:ident, $write_fn:ident) => {
        impl<const SIZE: usize> IoCapable<$ty> for RawIoSysMap<SIZE> {
            #[inline(always)]
            unsafe fn io_read(&self, address: usize) -> $ty {
                let offset = address - self.addr();
                // SAFETY: By the trait invariant `address` is a valid address for iosys map
                // operations.
                unsafe { bindings::$read_fn(&self.0, offset) }
            }

            #[inline(always)]
            unsafe fn io_write(&self, value: $ty, address: usize) {
                let offset = address - self.addr();
                // SAFETY: By the trait invariant `address` is a valid address for iosys map
                // operations.
                unsafe { bindings::$write_fn(&self.0, offset, value) };
            }
        }
    };
}

impl_raw_iosys_map_io_capable!(u8, iosys_map_rd_u8, iosys_map_wr_u8);
impl_raw_iosys_map_io_capable!(u16, iosys_map_rd_u16, iosys_map_wr_u16);
impl_raw_iosys_map_io_capable!(u32, iosys_map_rd_u32, iosys_map_wr_u32);
#[cfg(CONFIG_64BIT)]
impl_raw_iosys_map_io_capable!(u64, iosys_map_rd_u64, iosys_map_wr_u64);

/// A sized version of a [`RawIoSysMap`].
///
/// This type includes the runtime size of the [`RawIoSysMap`] and can be used for checked I/O
/// operations.
///
/// # Invariants
///
/// - The iosys mapping referenced by this type is guaranteed to be of at least `size` bytes in
///   size
/// - The iosys mapping referenced by this type is valid for the lifetime `'a`.
#[derive(Clone)]
pub struct IoSysMapRef<'a, const SIZE: usize = 0> {
    pub(crate) raw_map: RawIoSysMap<SIZE>,
    size: usize,
    _p: PhantomData<&'a ()>,
}

impl<'a, const SIZE: usize> IoSysMapRef<'a, SIZE> {
    /// Create a new [`IoSysMapRef`] from a [`RawIoSysMap`].
    ///
    /// Returns an error if the specified size is invalid.
    ///
    /// # Safety
    ///
    /// - The caller guarantees that the mapping is of at least `size` bytes.
    /// - The caller guarantees that the mapping remains valid for the lifetime of `'a`.
    pub(crate) unsafe fn new(map: RawIoSysMap<SIZE>, size: usize) -> Result<Self> {
        if size < SIZE {
            return Err(EINVAL);
        }

        Ok(Self {
            raw_map: map,
            size,
            _p: PhantomData,
        })
    }

    /// Returns whether the mapping is within IO memory space or not.
    #[inline]
    pub fn is_iomem(&self) -> bool {
        self.raw_map.is_iomem()
    }

    /// Returns the address pointed to by this iosys map.
    ///
    /// Note that this address is not guaranteed to be valid, and may or may not reside in I/O
    /// memory.
    #[inline]
    pub fn addr(&self) -> usize {
        self.raw_map.addr()
    }
}

macro_rules! impl_iosys_map_io_capable {
    ($ty:ty) => {
        impl<'a, const SIZE: usize> IoCapable<$ty> for IoSysMapRef<'a, SIZE> {
            #[inline(always)]
            unsafe fn io_read(&self, address: usize) -> $ty {
                // SAFETY: By the trait invariant `address` is a valid address for iosys map
                // operations.
                unsafe { self.raw_map.io_read(address) }
            }

            #[inline(always)]
            unsafe fn io_write(&self, value: $ty, address: usize) {
                // SAFETY: By the trait invariant `address` is a valid address for iosys map
                // operations.
                unsafe { self.raw_map.io_write(value, address) };
            }
        }
    };
}

impl_iosys_map_io_capable!(u8);
impl_iosys_map_io_capable!(u16);
impl_iosys_map_io_capable!(u32);
#[cfg(CONFIG_64BIT)]
impl_iosys_map_io_capable!(u64);

impl<'a, const SIZE: usize> Io for IoSysMapRef<'a, SIZE> {
    #[inline]
    fn addr(&self) -> usize {
        self.raw_map.addr()
    }

    #[inline]
    fn maxsize(&self) -> usize {
        self.size
    }
}

impl<'a, const SIZE: usize> IoKnownSize for IoSysMapRef<'a, SIZE> {
    const MIN_SIZE: usize = SIZE;
}
