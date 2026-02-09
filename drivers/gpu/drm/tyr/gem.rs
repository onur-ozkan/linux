// SPDX-License-Identifier: GPL-2.0 or MIT
//! GEM buffer object management for the Tyr driver.
//!
//! This module provides buffer object (BO) management functionality using
//! DRM's GEM subsystem with shmem backing.

use core::ops::Range;

use kernel::{
    drm::{
        gem,
        gem::shmem,
        DeviceContext, //
    },
    prelude::*,
    sync::{
        aref::ARef,
        Arc,
        ArcBorrow, //
    },
};

use crate::{
    driver::{
        TyrDrmDevice,
        TyrDrmDriver, //
    },
    vm::{
        Vm,
        VmMapFlags, //
    },
};

/// Tyr's DriverObject type for GEM objects.
#[pin_data]
pub(crate) struct BoData {
    flags: u32,
}

/// Provides a way to pass arguments when creating BoData
/// as required by the gem::DriverObject trait.
pub(crate) struct BoCreateArgs {
    flags: u32,
}

impl gem::DriverObject for BoData {
    type Driver = TyrDrmDriver;
    type Args = BoCreateArgs;

    fn new<Ctx: DeviceContext>(
        _dev: &TyrDrmDevice<Ctx>,
        _size: usize,
        args: BoCreateArgs,
    ) -> impl PinInit<Self, Error> {
        try_pin_init!(Self { flags: args.flags })
    }
}

/// Type alias for Tyr GEM buffer objects.
pub(crate) type Bo = gem::shmem::Object<BoData>;

/// Creates a dummy GEM object to serve as the root of a GPUVM.
pub(crate) fn new_dummy_object<Ctx: DeviceContext>(ddev: &TyrDrmDevice<Ctx>) -> Result<ARef<Bo>> {
    let bo = gem::shmem::Object::<BoData>::new(
        ddev,
        4096,
        shmem::ObjectConfig {
            map_wc: true,
            parent_resv_obj: None,
        },
        BoCreateArgs { flags: 0 },
    )?;

    Ok(bo)
}

/// A buffer object that is owned and managed by Tyr rather than userspace.
pub(crate) struct KernelBo {
    #[expect(dead_code)]
    pub(crate) bo: ARef<Bo>,
    vm: Arc<Vm>,
    va_range: Range<u64>,
}

impl KernelBo {
    /// Creates a new kernel-owned buffer object and maps it into GPU VA space.
    #[expect(dead_code)]
    pub(crate) fn new<Ctx: DeviceContext>(
        ddev: &TyrDrmDevice<Ctx>,
        vm: ArcBorrow<'_, Vm>,
        size: u64,
        va: u64,
        flags: VmMapFlags,
    ) -> Result<Self> {
        let bo = gem::shmem::Object::<BoData>::new(
            ddev,
            size as usize,
            shmem::ObjectConfig {
                map_wc: true,
                parent_resv_obj: None,
            },
            BoCreateArgs { flags: 0 },
        )?;

        vm.map_bo_range(&bo, 0, size, va, flags)?;

        Ok(KernelBo {
            bo,
            vm: vm.into(),
            va_range: va..(va + size),
        })
    }
}

impl Drop for KernelBo {
    fn drop(&mut self) {
        let va = self.va_range.start;
        let size = self.va_range.end - self.va_range.start;

        if let Err(e) = self.vm.unmap_range(va, size) {
            pr_err!(
                "Failed to unmap KernelBo range {:#x}..{:#x}: {:?}\n",
                self.va_range.start,
                self.va_range.end,
                e
            );
        }
    }
}
