// SPDX-License-Identifier: GPL-2.0 or MIT

//! Provides asynchronous reset handling for the Tyr DRM driver via
//! [`ResetHandle`], which runs reset work on a dedicated ordered
//! workqueue and avoids duplicate pending resets.

use kernel::{
    device::{Bound, Device},
    devres::Devres,
    io::poll,
    platform,
    prelude::*,
    sync::{
        aref::ARef,
        atomic::{Acquire, Atomic, Relaxed, Release},
        Arc,
    },
    time, workqueue,
    workqueue::Work,
};

use crate::{driver::IoMem, gpu, regs};

#[pin_data]
struct Controller {
    /// Platform device reference needed for reset operations and logging.
    pdev: ARef<platform::Device>,
    /// Mapped register space needed for reset operations.
    iomem: Arc<Devres<IoMem>>,
    /// Atomic flag for controlling the scheduling pending state.
    pending: Atomic<bool>,
    /// Dedicated ordered workqueue for reset operations.
    wq: workqueue::OrderedQueue,
    /// Work item backing async reset processing.
    #[pin]
    work: Work<Controller>,
}

kernel::impl_has_work! {
    impl HasWork<Controller> for Controller { self.work }
}

impl workqueue::WorkItem for Controller {
    type Pointer = Arc<Self>;

    fn run(this: Arc<Self>) {
        this.reset_work();
    }
}

impl Controller {
    /// Creates a [`Controller`] instance.
    fn new(pdev: ARef<platform::Device>, iomem: Arc<Devres<IoMem>>) -> Result<Arc<Self>> {
        let wq = workqueue::OrderedQueue::new(c"tyr-reset-wq", 0)?;

        Arc::pin_init(
            try_pin_init!(Self {
                pdev,
                iomem,
                pending: Atomic::new(false),
                wq,
                work <- kernel::new_work!("tyr::reset"),
            }),
            GFP_KERNEL,
        )
    }

    /// Processes one scheduled reset request.
    ///
    /// Panthor reference:
    /// - drivers/gpu/drm/panthor/panthor_device.c::panthor_device_reset_work()
    fn reset_work(self: &Arc<Self>) {
        dev_info!(self.pdev.as_ref(), "GPU reset work is started.\n");

        // SAFETY: `Controller` is part of driver-private data and only exists
        // while the platform device is bound.
        let pdev = unsafe { self.pdev.as_ref().as_bound() };
        if let Err(e) = run_reset(pdev, &self.iomem) {
            dev_err!(self.pdev.as_ref(), "GPU reset failed: {:?}\n", e);
        } else {
            dev_info!(self.pdev.as_ref(), "GPU reset work is done.\n");
        }

        self.pending.store(false, Release);
    }
}

/// Reset handle that shuts down pending work gracefully on drop.
pub(crate) struct ResetHandle(Arc<Controller>);

impl ResetHandle {
    /// Creates a [`ResetHandle`] instance.
    pub(crate) fn new(pdev: ARef<platform::Device>, iomem: Arc<Devres<IoMem>>) -> Result<Self> {
        Ok(Self(Controller::new(pdev, iomem)?))
    }

    /// Schedules reset work.
    #[expect(dead_code)]
    pub(crate) fn schedule(&self) {
        // TODO: Similar to `panthor_device_schedule_reset()` in Panthor, add a
        // power management check once Tyr supports it.

        // Keep only one reset request running or queued. If one is already pending,
        // we ignore new schedule requests.
        if self.0.pending.cmpxchg(false, true, Relaxed).is_ok() {
            if self.0.wq.enqueue(self.0.clone()).is_err() {
                self.0.pending.store(false, Release);
            }
        }
    }

    /// Returns true if a reset is queued or in progress.
    ///
    /// Note that the state can change immediately after the return.
    #[inline]
    #[expect(dead_code)]
    pub(crate) fn is_pending(&self) -> bool {
        self.0.pending.load(Acquire)
    }
}

impl Drop for ResetHandle {
    fn drop(&mut self) {
        // Drain queued/running work and block future queueing attempts for this
        // work item before clocks/regulators are torn down.
        self.0.work.disable_sync();
    }
}

/// Issues a soft reset command and waits for reset-complete IRQ status.
fn issue_soft_reset(dev: &Device<Bound>, iomem: &Devres<IoMem>) -> Result {
    // Clear any stale reset-complete IRQ state before issuing a new soft reset.
    regs::GPU_IRQ_CLEAR.write(dev, iomem, regs::GPU_IRQ_RAWSTAT_RESET_COMPLETED)?;
    regs::GPU_CMD.write(dev, iomem, regs::GPU_CMD_SOFT_RESET)?;

    poll::read_poll_timeout(
        || regs::GPU_IRQ_RAWSTAT.read(dev, iomem),
        |status| *status & regs::GPU_IRQ_RAWSTAT_RESET_COMPLETED != 0,
        time::Delta::from_millis(1),
        time::Delta::from_millis(100),
    )
    .inspect_err(|_| dev_err!(dev, "GPU reset failed."))?;

    Ok(())
}

/// Runs one synchronous GPU reset pass.
///
/// Its visibility is `pub(super)` only so the probe path can run an
/// initial reset; it is not part of this module's public API.
///
/// On success, the GPU is left in a state suitable for reinitialization.
///
/// The reset sequence is as follows:
/// 1. Trigger a GPU soft reset.
/// 2. Wait for the reset-complete IRQ status.
/// 3. Power L2 back on.
///
pub(super) fn run_reset(dev: &Device<Bound>, iomem: &Devres<IoMem>) -> Result {
    issue_soft_reset(dev, iomem)?;
    gpu::l2_power_on(dev, iomem)?;
    Ok(())
}
