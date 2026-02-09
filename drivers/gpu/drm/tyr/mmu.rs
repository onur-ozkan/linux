// SPDX-License-Identifier: GPL-2.0 or MIT

//! Memory Management Unit (MMU) driver for the Tyr GPU.
//!
//! This module manages GPU address spaces and virtual memory operations through
//! hardware MMU slots. It provides functionality for flushing page tables and
//! managing VM updates for active address spaces.
//!
//! The MMU coordinates with the [`AddressSpaceManager`] to handle hardware
//! address space allocation and page table operations, using [`SlotManager`]
//! to track which address spaces are currently active in hardware slots.
//!
//! [`AddressSpaceManager`]: address_space::AddressSpaceManager
//! [`SlotManager`]: crate::slot::SlotManager
#![allow(dead_code)]

use core::ops::Range;

use kernel::{
    devres::Devres,
    new_mutex,
    platform,
    prelude::*,
    sync::{
        Arc,
        ArcBorrow,
        Mutex, //
    }, //
};

use crate::{
    driver::IoMem,
    gpu::GpuInfo,
    mmu::address_space::{
        AddressSpaceManager,
        VmAsData, //
    },
    regs::MAX_AS_REGISTERS,
    slot::{
        SlotManager, //
    }, //
};

pub(crate) mod address_space;

pub(crate) type AsSlotManager = SlotManager<AddressSpaceManager, MAX_AS_REGISTERS>;

#[pin_data]
pub(crate) struct Mmu {
    /// Manages the allocation of hardware MMU slots to GPU address spaces.
    ///
    /// Tracks which address spaces are currently active in hardware slots and
    /// coordinates address space operations like flushing and VM updates.
    #[pin]
    pub(crate) as_manager: Mutex<AsSlotManager>,
}

impl Mmu {
    pub(crate) fn new(
        pdev: &platform::Device,
        iomem: ArcBorrow<'_, Devres<IoMem>>,
        gpu_info: &GpuInfo,
    ) -> Result<Arc<Mmu>> {
        let slot_count = gpu_info.as_present.count_ones().try_into()?;
        let as_manager = AddressSpaceManager::new(pdev, iomem, gpu_info.as_present)?;
        let mmu_init = try_pin_init!(Self{
            as_manager <- new_mutex!(SlotManager::new(as_manager, slot_count)?),
        });
        Arc::pin_init(mmu_init, GFP_KERNEL)
    }

    pub(crate) fn activate_vm(&self, vm: ArcBorrow<'_, VmAsData>) -> Result {
        self.as_manager.lock().activate_vm(vm)
    }

    pub(crate) fn deactivate_vm(&self, vm: &VmAsData) -> Result {
        self.as_manager.lock().deactivate_vm(vm)
    }

    pub(crate) fn flush_vm(&self, vm: &VmAsData) -> Result {
        self.as_manager.lock().flush_vm(vm)
    }

    pub(crate) fn start_vm_update(&self, vm: &VmAsData, region: &Range<u64>) -> Result {
        self.as_manager.lock().start_vm_update(vm, region)
    }

    pub(crate) fn end_vm_update(&self, vm: &VmAsData) -> Result {
        self.as_manager.lock().end_vm_update(vm)
    }
}
