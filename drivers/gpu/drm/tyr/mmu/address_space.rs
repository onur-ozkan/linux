// SPDX-License-Identifier: GPL-2.0 or MIT

//! GPU address space management and hardware operations.
//!
//! This module manages GPU hardware address spaces, including configuration,
//! command submission, and page table update regions. It handles the hardware
//! interaction for MMU operations through MMIO register access.
//!
//! The [`AddressSpaceManager`] implements [`SlotOperations`] to integrate with
//! the slot management system, enabling and configuring address spaces in the
//! hardware slots as needed.
//!
//! [`SlotOperations`]: crate::slot::SlotOperations

use core::ops::Range;

use kernel::{
    bits::*,
    device::{
        Bound,
        Device, //
    },
    devres::Devres,
    error::Result,
    io,
    iommu::pgtable::{
        IoPageTable,
        ARM64LPAES1, //
    },
    platform,
    prelude::*,
    sync::{
        aref::ARef,
        Arc,
        ArcBorrow,
        LockedBy, //
    },
    time::Delta, //
};

use crate::{
    driver::IoMem,
    mmu::{
        AsSlotManager,
        Mmu, //
    },
    regs::*,
    slot::{
        Seat,
        SlotOperations, //
    }, //
};

/// Hardware address space configuration registers.
///
/// Contains the values to be written to the GPU's AS registers when
/// activating this address space.
#[derive(Clone, Copy)]
pub(crate) struct AddressSpaceConfig {
    pub(crate) transcfg: u64,
    pub(crate) transtab: u64,
    pub(crate) memattr: u64,
}

/// Any resource/information that will be used by the AddressSpaceManager
/// to make a VM active is present in VmAsData.
///
/// On activation, we will pass an Arc<VmAsData> that will be stored in
/// the slot to make sure the page table and the underlying resources
/// (pages) used by the AS slot won't go away while the MMU points to
/// those.
pub(crate) struct VmAsData {
    /// Tracks this VM's binding to a hardware address space slot.
    as_seat: LockedBy<Seat, AsSlotManager>,
    /// Hardware configuration for this address space.
    as_config: AddressSpaceConfig,
    /// Page table (managed by devres).
    pub(crate) page_table: Pin<KBox<Devres<IoPageTable<ARM64LPAES1>>>>,
}

impl VmAsData {
    pub(crate) fn new(
        mmu: &Mmu,
        as_config: AddressSpaceConfig,
        page_table: Pin<KBox<Devres<IoPageTable<ARM64LPAES1>>>>,
    ) -> VmAsData {
        Self {
            as_seat: LockedBy::new(&mmu.as_manager, Seat::NoSeat),
            as_config,
            page_table,
        }
    }
}

/// Manages GPU hardware address spaces via MMIO register operations.
pub(crate) struct AddressSpaceManager {
    pdev: ARef<platform::Device>,
    iomem: Arc<Devres<IoMem>>,
    /// Bitmask of available address space slots from GPU_AS_PRESENT register
    as_present: u32,
}

impl SlotOperations for AddressSpaceManager {
    type SlotData = Arc<VmAsData>;

    fn activate(&mut self, slot_idx: usize, slot_data: &Self::SlotData) -> Result {
        self.as_enable(slot_idx, &slot_data.as_config)
    }

    fn evict(&mut self, slot_idx: usize, _slot_data: &Self::SlotData) -> Result {
        if self.iomem.try_access().is_some() {
            let _ = self.as_flush(slot_idx);
            let _ = self.as_disable(slot_idx);
        }
        Ok(())
    }
}

impl AddressSpaceManager {
    pub(super) fn new(
        pdev: &platform::Device,
        iomem: ArcBorrow<'_, Devres<IoMem>>,
        as_present: u32,
    ) -> Result<AddressSpaceManager> {
        Ok(Self {
            pdev: pdev.into(),
            iomem: iomem.into(),
            as_present,
        })
    }

    fn dev(&self) -> &Device<Bound> {
        // SAFETY: pdev is a bound device.
        unsafe { self.pdev.as_ref().as_bound() }
    }

    fn validate_as_slot(&self, as_nr: usize) -> Result {
        if as_nr >= MAX_AS_REGISTERS {
            pr_err!(
                "AS slot {} out of valid range (max {})\n",
                as_nr,
                MAX_AS_REGISTERS
            );
            return Err(EINVAL);
        }

        if (self.as_present & (1 << as_nr)) == 0 {
            pr_err!(
                "AS slot {} not present in hardware (AS_PRESENT={:#x})\n",
                as_nr,
                self.as_present
            );
            return Err(EINVAL);
        }

        Ok(())
    }

    fn as_wait_ready(&self, as_nr: usize) -> Result {
        let op = || as_status(as_nr)?.read(self.dev(), &self.iomem);
        let cond = |status: &u32| -> bool { *status & AS_STATUS_ACTIVE == 0 };
        let _ =
            io::poll::read_poll_timeout(op, cond, Delta::from_millis(0), Delta::from_millis(10))?;

        Ok(())
    }

    fn as_send_cmd(&mut self, as_nr: usize, cmd: u32) -> Result {
        self.as_wait_ready(as_nr)?;
        as_command(as_nr)?.write(self.dev(), &self.iomem, cmd)?;
        Ok(())
    }

    fn as_send_cmd_and_wait(&mut self, as_nr: usize, cmd: u32) -> Result {
        self.as_send_cmd(as_nr, cmd)?;
        self.as_wait_ready(as_nr)?;
        Ok(())
    }

    fn as_enable(&mut self, as_nr: usize, as_config: &AddressSpaceConfig) -> Result {
        self.validate_as_slot(as_nr)?;

        let transtab = as_config.transtab;
        let transcfg = as_config.transcfg;
        let memattr = as_config.memattr;

        let transtab_lo = (transtab & 0xffffffff) as u32;
        let transtab_hi = (transtab >> 32) as u32;

        let transcfg_lo = (transcfg & 0xffffffff) as u32;
        let transcfg_hi = (transcfg >> 32) as u32;

        let memattr_lo = (memattr & 0xffffffff) as u32;
        let memattr_hi = (memattr >> 32) as u32;

        let dev = self.dev();
        as_transtab_lo(as_nr)?.write(dev, &self.iomem, transtab_lo)?;
        as_transtab_hi(as_nr)?.write(dev, &self.iomem, transtab_hi)?;

        as_transcfg_lo(as_nr)?.write(dev, &self.iomem, transcfg_lo)?;
        as_transcfg_hi(as_nr)?.write(dev, &self.iomem, transcfg_hi)?;

        as_memattr_lo(as_nr)?.write(dev, &self.iomem, memattr_lo)?;
        as_memattr_hi(as_nr)?.write(dev, &self.iomem, memattr_hi)?;

        self.as_send_cmd_and_wait(as_nr, AS_COMMAND_UPDATE)?;

        Ok(())
    }

    fn as_disable(&mut self, as_nr: usize) -> Result {
        self.validate_as_slot(as_nr)?;

        // Flush AS before disabling
        self.as_send_cmd_and_wait(as_nr, AS_COMMAND_FLUSH_MEM)?;

        let dev = self.dev();
        as_transtab_lo(as_nr)?.write(dev, &self.iomem, 0)?;
        as_transtab_hi(as_nr)?.write(dev, &self.iomem, 0)?;

        as_memattr_lo(as_nr)?.write(dev, &self.iomem, 0)?;
        as_memattr_hi(as_nr)?.write(dev, &self.iomem, 0)?;

        as_transcfg_lo(as_nr)?.write(dev, &self.iomem, AS_TRANSCFG_ADRMODE_UNMAPPED as u32)?;
        as_transcfg_hi(as_nr)?.write(dev, &self.iomem, 0)?;

        self.as_send_cmd_and_wait(as_nr, AS_COMMAND_UPDATE)?;

        Ok(())
    }

    fn as_start_update(&mut self, as_nr: usize, region: &Range<u64>) -> Result {
        self.validate_as_slot(as_nr)?;

        // The locked region is a naturally aligned power of 2 block encoded as
        // log2 minus(1).
        //
        // Calculate the desired start/end and look for the highest bit which
        // differs. The smallest naturally aligned block must include this bit
        // change, the desired region starts with this bit (and subsequent bits)
        // zeroed and ends with the bit (and subsequent bits) set to one.
        let region_width = core::cmp::max(
            64 - (region.start ^ (region.end - 1)).leading_zeros() as u8,
            AS_LOCK_REGION_MIN_SIZE.trailing_zeros() as u8,
        ) - 1;

        // Mask off the low bits of region.start, which would be ignored by the
        // hardware anyways.
        let region_start =
            region.start & genmask_checked_u64(u32::from(region_width)..=63).ok_or(EINVAL)?;

        let region = (u64::from(region_width)) | region_start;

        let region_lo = (region & 0xffffffff) as u32;
        let region_hi = (region >> 32) as u32;

        // Lock the region that needs to be updated.
        let dev = self.dev();
        as_lockaddr_lo(as_nr)?.write(dev, &self.iomem, region_lo)?;
        as_lockaddr_hi(as_nr)?.write(dev, &self.iomem, region_hi)?;

        self.as_send_cmd(as_nr, AS_COMMAND_LOCK)
    }

    fn as_end_update(&mut self, as_nr: usize) -> Result {
        self.validate_as_slot(as_nr)?;
        self.as_send_cmd_and_wait(as_nr, AS_COMMAND_FLUSH_PT)
    }

    fn as_flush(&mut self, as_nr: usize) -> Result {
        self.validate_as_slot(as_nr)?;
        self.as_send_cmd(as_nr, AS_COMMAND_FLUSH_PT)
    }
}

impl AsSlotManager {
    /// Locks a region for page table updates if the VM has an active slot.
    pub(super) fn start_vm_update(&mut self, vm: &VmAsData, region: &Range<u64>) -> Result {
        let seat = vm.as_seat.access(self);
        match seat.slot() {
            Some(slot) => {
                let as_nr = slot as usize;
                self.as_start_update(as_nr, region)
            }
            _ => Ok(()),
        }
    }

    /// Flushes page table updates for a VM if it has an active slot.
    pub(super) fn end_vm_update(&mut self, vm: &VmAsData) -> Result {
        let seat = vm.as_seat.access(self);
        match seat.slot() {
            Some(slot) => {
                let as_nr = slot as usize;
                self.as_end_update(as_nr)
            }
            _ => Ok(()),
        }
    }

    /// Flushes page tables for a VM if it has an active slot.
    pub(super) fn flush_vm(&mut self, vm: &VmAsData) -> Result {
        let seat = vm.as_seat.access(self);
        match seat.slot() {
            Some(slot) => {
                let as_nr = slot as usize;
                self.as_flush(as_nr)
            }
            _ => Ok(()),
        }
    }

    /// Flushes page tables for a VM if it has an active slot.
    pub(super) fn activate_vm(&mut self, vm: ArcBorrow<'_, VmAsData>) -> Result {
        self.activate(&vm.as_seat, vm.into())
    }

    /// Flushes page tables for a VM if it has an active slot.
    pub(super) fn deactivate_vm(&mut self, vm: &VmAsData) -> Result {
        self.evict(&vm.as_seat)
    }
}
