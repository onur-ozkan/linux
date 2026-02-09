// SPDX-License-Identifier: GPL-2.0 or MIT

// We don't expect that all the registers and fields will be used, even in the
// future.
//
// Nevertheless, it is useful to have most of them defined, like the C driver
// does.
#![allow(dead_code)]

use kernel::{
    bits::{
        bit_u32,
        bit_u64, //
    },
    device::{
        Bound,
        Device, //
    },
    devres::Devres,
    io::Io,
    prelude::*, //
};

use crate::driver::IoMem;

/// Represents a register in the Register Set
///
/// TODO: Replace this with the Nova `register!()` macro when it is available.
/// In particular, this will automatically give us 64bit register reads and
/// writes.
pub(crate) struct Register<const OFFSET: usize>;

impl<const OFFSET: usize> Register<OFFSET> {
    #[inline]
    pub(crate) fn read(&self, dev: &Device<Bound>, iomem: &Devres<IoMem>) -> Result<u32> {
        let value = (*iomem).access(dev)?.read32(OFFSET);
        Ok(value)
    }

    #[inline]
    pub(crate) fn write(&self, dev: &Device<Bound>, iomem: &Devres<IoMem>, value: u32) -> Result {
        (*iomem).access(dev)?.write32(value, OFFSET);
        Ok(())
    }
}

pub(crate) const GPU_ID: Register<0x0> = Register;
pub(crate) const GPU_L2_FEATURES: Register<0x4> = Register;
pub(crate) const GPU_CORE_FEATURES: Register<0x8> = Register;
pub(crate) const GPU_CSF_ID: Register<0x1c> = Register;
pub(crate) const GPU_REVID: Register<0x280> = Register;
pub(crate) const GPU_TILER_FEATURES: Register<0xc> = Register;
pub(crate) const GPU_MEM_FEATURES: Register<0x10> = Register;
pub(crate) const GPU_MMU_FEATURES: Register<0x14> = Register;
pub(crate) const GPU_AS_PRESENT: Register<0x18> = Register;
pub(crate) const GPU_IRQ_RAWSTAT: Register<0x20> = Register;

pub(crate) const GPU_IRQ_RAWSTAT_FAULT: u32 = bit_u32(0);
pub(crate) const GPU_IRQ_RAWSTAT_PROTECTED_FAULT: u32 = bit_u32(1);
pub(crate) const GPU_IRQ_RAWSTAT_RESET_COMPLETED: u32 = bit_u32(8);
pub(crate) const GPU_IRQ_RAWSTAT_POWER_CHANGED_SINGLE: u32 = bit_u32(9);
pub(crate) const GPU_IRQ_RAWSTAT_POWER_CHANGED_ALL: u32 = bit_u32(10);
pub(crate) const GPU_IRQ_RAWSTAT_CLEAN_CACHES_COMPLETED: u32 = bit_u32(17);
pub(crate) const GPU_IRQ_RAWSTAT_DOORBELL_STATUS: u32 = bit_u32(18);
pub(crate) const GPU_IRQ_RAWSTAT_MCU_STATUS: u32 = bit_u32(19);

pub(crate) const GPU_IRQ_CLEAR: Register<0x24> = Register;
pub(crate) const GPU_IRQ_MASK: Register<0x28> = Register;
pub(crate) const GPU_IRQ_STAT: Register<0x2c> = Register;
pub(crate) const GPU_CMD: Register<0x30> = Register;
pub(crate) const GPU_CMD_SOFT_RESET: u32 = 1 | (1 << 8);
pub(crate) const GPU_CMD_HARD_RESET: u32 = 1 | (2 << 8);
pub(crate) const GPU_THREAD_FEATURES: Register<0xac> = Register;
pub(crate) const GPU_THREAD_MAX_THREADS: Register<0xa0> = Register;
pub(crate) const GPU_THREAD_MAX_WORKGROUP_SIZE: Register<0xa4> = Register;
pub(crate) const GPU_THREAD_MAX_BARRIER_SIZE: Register<0xa8> = Register;
pub(crate) const GPU_TEXTURE_FEATURES0: Register<0xb0> = Register;
pub(crate) const GPU_SHADER_PRESENT_LO: Register<0x100> = Register;
pub(crate) const GPU_SHADER_PRESENT_HI: Register<0x104> = Register;
pub(crate) const GPU_TILER_PRESENT_LO: Register<0x110> = Register;
pub(crate) const GPU_TILER_PRESENT_HI: Register<0x114> = Register;
pub(crate) const GPU_L2_PRESENT_LO: Register<0x120> = Register;
pub(crate) const GPU_L2_PRESENT_HI: Register<0x124> = Register;
pub(crate) const L2_READY_LO: Register<0x160> = Register;
pub(crate) const L2_READY_HI: Register<0x164> = Register;
pub(crate) const L2_PWRON_LO: Register<0x1a0> = Register;
pub(crate) const L2_PWRON_HI: Register<0x1a4> = Register;
pub(crate) const L2_PWRTRANS_LO: Register<0x220> = Register;
pub(crate) const L2_PWRTRANS_HI: Register<0x204> = Register;
pub(crate) const L2_PWRACTIVE_LO: Register<0x260> = Register;
pub(crate) const L2_PWRACTIVE_HI: Register<0x264> = Register;

pub(crate) const MCU_CONTROL: Register<0x700> = Register;
pub(crate) const MCU_CONTROL_ENABLE: u32 = 1;
pub(crate) const MCU_CONTROL_AUTO: u32 = 2;
pub(crate) const MCU_CONTROL_DISABLE: u32 = 0;

pub(crate) const MCU_STATUS: Register<0x704> = Register;
pub(crate) const MCU_STATUS_DISABLED: u32 = 0;
pub(crate) const MCU_STATUS_ENABLED: u32 = 1;
pub(crate) const MCU_STATUS_HALT: u32 = 2;
pub(crate) const MCU_STATUS_FATAL: u32 = 3;

pub(crate) const GPU_COHERENCY_FEATURES: Register<0x300> = Register;

pub(crate) const JOB_IRQ_RAWSTAT: Register<0x1000> = Register;
pub(crate) const JOB_IRQ_CLEAR: Register<0x1004> = Register;
pub(crate) const JOB_IRQ_MASK: Register<0x1008> = Register;
pub(crate) const JOB_IRQ_STAT: Register<0x100c> = Register;

pub(crate) const JOB_IRQ_GLOBAL_IF: u32 = bit_u32(31);

pub(crate) const MMU_IRQ_RAWSTAT: Register<0x2000> = Register;
pub(crate) const MMU_IRQ_CLEAR: Register<0x2004> = Register;
pub(crate) const MMU_IRQ_MASK: Register<0x2008> = Register;
pub(crate) const MMU_IRQ_STAT: Register<0x200c> = Register;

pub(crate) const AS_TRANSCFG_ADRMODE_UNMAPPED: u64 = bit_u64(0);
pub(crate) const AS_TRANSCFG_ADRMODE_AARCH64_4K: u64 = bit_u64(2) | bit_u64(1);
pub(crate) const AS_TRANSCFG_PTW_MEMATTR_WB: u64 = bit_u64(25);
pub(crate) const AS_TRANSCFG_PTW_RA: u64 = bit_u64(30);

pub(crate) const fn as_transcfg_ina_bits(x: u64) -> u64 {
    x << 6
}

pub(crate) const AS_MEMATTR_AARCH64_SH_MIDGARD_INNER: u32 = 0 << 4;
pub(crate) const AS_MEMATTR_AARCH64_INNER_OUTER_NC: u32 = 1 << 6;
pub(crate) const AS_MEMATTR_AARCH64_INNER_OUTER_WB: u32 = 2 << 6;

pub(crate) fn as_memattr_aarch64_inner_alloc_expl(w: bool, r: bool) -> u32 {
    (3 << 2) | (u32::from(w)) | ((u32::from(r)) << 1)
}

pub(crate) const AS_COMMAND_UPDATE: u32 = 1;
pub(crate) const AS_COMMAND_LOCK: u32 = 2;
pub(crate) const AS_COMMAND_FLUSH_PT: u32 = 4;
pub(crate) const AS_COMMAND_FLUSH_MEM: u32 = 5;

pub(crate) const AS_STATUS_ACTIVE: u32 = bit_u32(0);

pub(crate) const AS_LOCK_REGION_MIN_SIZE: u32 = bit_u32(15);

/// Maximum number of hardware address space slots.
/// The actual number of slots available is usually much lower.
pub(crate) const MAX_AS_REGISTERS: usize = 32;

const MMU_BASE: usize = 0x2400;
const MMU_AS_SHIFT: usize = 6;

const fn mmu_as(as_nr: usize) -> usize {
    MMU_BASE + (as_nr << MMU_AS_SHIFT)
}

pub(crate) struct AsRegister(usize);

impl AsRegister {
    fn new(as_nr: usize, offset: usize) -> Result<Self> {
        Ok(AsRegister(mmu_as(as_nr) + offset))
    }

    #[inline]
    pub(crate) fn read(&self, dev: &Device<Bound>, iomem: &Devres<IoMem>) -> Result<u32> {
        let value = (*iomem).access(dev)?.try_read32(self.0)?;
        Ok(value)
    }

    #[inline]
    pub(crate) fn write(&self, dev: &Device<Bound>, iomem: &Devres<IoMem>, value: u32) -> Result {
        (*iomem).access(dev)?.try_write32(value, self.0)?;
        Ok(())
    }
}

pub(crate) fn as_transtab_lo(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x0)
}

pub(crate) fn as_transtab_hi(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x4)
}

pub(crate) fn as_memattr_lo(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x8)
}

pub(crate) fn as_memattr_hi(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0xc)
}

pub(crate) fn as_lockaddr_lo(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x10)
}

pub(crate) fn as_lockaddr_hi(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x14)
}

pub(crate) fn as_command(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x18)
}

pub(crate) fn as_status(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x28)
}

pub(crate) fn as_transcfg_lo(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x30)
}
pub(crate) fn as_transcfg_hi(as_nr: usize) -> Result<AsRegister> {
    AsRegister::new(as_nr, 0x34)
}
