// SPDX-License-Identifier: GPL-2.0 or MIT

use kernel::{
    clk::{
        Clk,
        OptionalClk, //
    },
    device::{
        Bound,
        Core,
        DeviceContext, //
    },
    dma::{
        Device as DmaDevice,
        DmaMask, //
    },
    drm,
    drm::ioctl,
    new_mutex,
    of,
    platform, //
    prelude::*,
    regulator,
    regulator::Regulator,
    sizes::SZ_2M,
    sync::{
        aref::ARef,
        Arc,
        Mutex, //
    },
    types::ForLt, //
};

use crate::{
    file::TyrDrmFileData,
    fw::Firmware,
    gem::BoData,
    gpu::GpuInfo,
    mmu::Mmu,
    regs::gpu_control::*,
    reset, //
};

pub(crate) type IoMem<'a> = kernel::io::mem::IoMem<'a, SZ_2M>;

pub(crate) struct TyrDrmDriver;

/// Convenience type alias for the DRM device type for this driver.
pub(crate) type TyrDrmDevice<Ctx = drm::Registered> = drm::Device<TyrDrmDriver, Ctx>;
pub(crate) struct TyrPlatformDriver;

#[pin_data(PinnedDrop)]
pub(crate) struct TyrPlatformDriverData<'bound> {
    _device: ARef<TyrDrmDevice>,
    _reg: drm::Registration<'bound, TyrDrmDriver>,
}

/// Resources kept alive by the DRM registration.
#[pin_data]
pub(crate) struct TyrDrmRegistrationData<'bound> {
    /// Parent platform device.
    pub(crate) pdev: &'bound platform::Device<Bound>,

    // `ResetHandle::drop()` drains queued/running works and this must happen
    // before clocks/regulators are dropped. So keep this field before them to
    // ensure the correct drop order.
    #[pin]
    pub(crate) reset: reset::ResetHandle<'bound>,

    /// Firmware sections.
    pub(crate) fw: Arc<Firmware<'bound>>,

    #[pin]
    clks: Mutex<Clocks>,

    #[pin]
    regulators: Mutex<Regulators>,

    /// Some information on the GPU.
    ///
    /// This is mainly queried by userspace, i.e.: Mesa.
    pub(crate) gpu_info: GpuInfo,
}

kernel::of_device_table!(
    OF_TABLE,
    MODULE_OF_TABLE,
    <TyrPlatformDriver as platform::Driver>::IdInfo,
    [
        (of::DeviceId::new(c"rockchip,rk3588-mali"), ()),
        (of::DeviceId::new(c"arm,mali-valhall-csf"), ())
    ]
);

impl platform::Driver for TyrPlatformDriver {
    type IdInfo = ();
    type Data<'bound> = TyrPlatformDriverData<'bound>;
    const OF_ID_TABLE: Option<of::IdTable<Self::IdInfo>> = Some(&OF_TABLE);

    fn probe<'bound>(
        pdev: &'bound platform::Device<Core<'_>>,
        _info: Option<&'bound Self::IdInfo>,
    ) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        let core_clk = Clk::get(pdev.as_ref(), Some(c"core"))?;
        let stacks_clk = OptionalClk::get(pdev.as_ref(), Some(c"stacks"))?;
        let coregroup_clk = OptionalClk::get(pdev.as_ref(), Some(c"coregroup"))?;

        core_clk.prepare_enable()?;
        stacks_clk.prepare_enable()?;
        coregroup_clk.prepare_enable()?;

        let mali_regulator = Regulator::<regulator::Enabled>::get(pdev.as_ref(), c"mali")?;
        let sram_regulator = Regulator::<regulator::Enabled>::get(pdev.as_ref(), c"sram")?;

        let request = pdev.io_request_by_index(0).ok_or(ENODEV)?;

        let hw = Arc::pin_init(
            reset::HwGate::new(request.iomap_sized::<SZ_2M>()?),
            GFP_KERNEL,
        )?;

        reset::run_reset(pdev.as_ref(), &hw)?;

        let gpu_info = {
            let hw_guard = hw.access();
            let gpu_info = GpuInfo::new(hw_guard.iomem());
            gpu_info.log(pdev.as_ref());
            gpu_info
        };

        let pa_bits = MMU_FEATURES::from_raw(gpu_info.mmu_features)
            .pa_bits()
            .get();
        // SAFETY: No concurrent DMA allocations or mappings can be made because
        // the device is still being probed and therefore isn't being used by
        // other threads of execution.
        unsafe { pdev.dma_set_mask_and_coherent(DmaMask::try_new(pa_bits)?)? };

        let unreg_dev = drm::UnregisteredDevice::<TyrDrmDriver>::new(pdev, Ok(()))?;

        let mmu = Mmu::new(hw.clone(), &gpu_info)?;

        let firmware = Firmware::new(pdev, hw.clone(), &unreg_dev, mmu.as_arc_borrow(), &gpu_info)?;

        firmware.boot()?;
        firmware.enable_global_interface(&gpu_info, &core_clk)?;

        let reg_data = try_pin_init!(TyrDrmRegistrationData {
                pdev,
                // SAFETY: `ResetHandle` is stored in registration data created with `new_with_lt`
                // and is dropped before the borrowed device and MMIO references expire.
                reset <- unsafe { reset::ResetHandle::new(pdev, hw.clone())? },
                fw: firmware,
                clks <- new_mutex!(Clocks {
                    core: core_clk,
                    stacks: stacks_clk,
                    coregroup: coregroup_clk,
                }),
                regulators <- new_mutex!(Regulators {
                    _mali: mali_regulator,
                    _sram: sram_regulator,
                }),
                gpu_info,
        });

        // SAFETY: `reg` is stored in the platform driver data and is not leaked or
        // forgotten, so it is dropped before the `'bound` registration data can become
        // invalid.
        let reg = unsafe { drm::Registration::new_with_lt(pdev.as_ref(), unreg_dev, reg_data, 0)? };

        let driver = TyrPlatformDriverData {
            _device: reg.device().into(),
            _reg: reg,
        };

        // We need this to be dev_info!() because dev_dbg!() does not work at
        // all in Rust for now, and we need to see whether probe succeeded.
        dev_info!(pdev, "Tyr initialized correctly.\n");
        Ok(driver)
    }
}

#[pinned_drop]
impl PinnedDrop for TyrPlatformDriverData<'_> {
    fn drop(self: Pin<&mut Self>) {}
}

// We need to retain the name "panthor" to achieve drop-in compatibility with
// the C driver in the userspace stack.
const INFO: drm::DriverInfo = drm::DriverInfo {
    major: 1,
    minor: 5,
    patchlevel: 0,
    name: c"panthor",
    desc: c"ARM Mali Tyr DRM driver",
};

#[vtable]
impl drm::Driver for TyrDrmDriver {
    type Data = ();
    type RegistrationData = ForLt!(TyrDrmRegistrationData<'_>);
    type File = TyrDrmFileData;
    type Object<R: drm::DeviceContext> = drm::gem::shmem::Object<BoData, R>;
    type ParentDevice<Ctx: DeviceContext> = platform::Device<Ctx>;

    const INFO: drm::DriverInfo = INFO;
    const FEAT_RENDER: bool = true;

    kernel::declare_drm_ioctls! {
        (PANTHOR_DEV_QUERY, drm_panthor_dev_query, ioctl::RENDER_ALLOW, TyrDrmFileData::dev_query),
    }
}

struct Clocks {
    core: Clk,
    stacks: OptionalClk,
    coregroup: OptionalClk,
}

impl Drop for Clocks {
    fn drop(&mut self) {
        self.core.disable_unprepare();
        self.stacks.disable_unprepare();
        self.coregroup.disable_unprepare();
    }
}

struct Regulators {
    _mali: Regulator<regulator::Enabled>,
    _sram: Regulator<regulator::Enabled>,
}
