// SPDX-License-Identifier: GPL-2.0 or MIT

//! Provides asynchronous reset handling for the Tyr DRM driver via [`ResetHandle`].
//!
//! [`ResetHandle::schedule`] runs reset work on a dedicated ordered
//! [`ScopedQueue`] and avoids duplicate pending reset requests.
//!
//! # High-level Execution Flow
//!
//! ```text
//! +------+  schedule()  +---------+  reset_work()  +------------+
//! | Idle |------------->| Pending |--------------->| InProgress |
//! +------+              +---------+                +------------+
//!    ^                                             |
//!    |               work complete                 |
//!    +---------------------------------------------+
//!
//! Teardown transitions any state to ShuttingDown, then drains pending and
//! running work.
//! ```

mod hw_gate;

pub(crate) use hw_gate::HwGate;

use kernel::{
    device::{
        Bound,
        Device, //
    },
    io::{
        poll,
        Io, //
    },
    platform,
    prelude::*,
    sync::{
        atomic::{
            Atomic,
            AtomicType,
            Full,
            Release, //
        },
        Arc, //
    },
    time,
    workqueue::{
        ScopedQueue,
        ScopedWork,
        ScopedWorkItem,
        ScopedWorkRef, //
    },
};

use crate::{
    driver::IoMem,
    gpu,
    regs::gpu_control::*, //
};

/// Lifecycle state of the reset worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
enum ResetState {
    /// Hardware is available and no reset request exists.
    Idle = 0,
    /// Reset work item is queued and waiting to be claimed by the worker.
    Pending = 1,
    /// Worker has claimed the request and is resetting hardware.
    InProgress = 2,
    /// Teardown has started and no new reset request may start.
    ShuttingDown = 3,
}

// SAFETY: `ResetState` and `i32` have the same size and alignment, and are
// round-trip transmutable.
unsafe impl AtomicType for ResetState {
    type Repr = i32;
}

/// Internal reset orchestrator that owns the state, [`HwGate`], and work item.
#[pin_data]
struct Controller<'ctrl> {
    /// Parent platform device.
    pdev: &'ctrl platform::Device<Bound>,
    /// State shared by reset schedulers and the worker.
    state: Atomic<ResetState>,
    /// Shared gate that coordinates hardware access with GPU reset.
    hw: Arc<HwGate<'ctrl>>,
}

impl<'ctrl> ScopedWorkItem for Controller<'ctrl> {
    fn run(work: &ScopedWorkRef<Self>) {
        work.reset_work();
    }
}

impl<'ctrl> Controller<'ctrl> {
    /// Creates a reset controller.
    fn new(
        pdev: &'ctrl platform::Device<Bound>,
        hw: Arc<HwGate<'ctrl>>,
    ) -> impl PinInit<Self, Error> {
        try_pin_init!(Self {
            pdev,
            state: Atomic::new(ResetState::Idle),
            hw,
        })
    }

    /// Attempts to transition the reset state from `from` to `to`.
    #[inline]
    fn try_transition(&self, from: ResetState, to: ResetState) -> bool {
        self.state.cmpxchg(from, to, Full).is_ok()
    }

    /// Processes one scheduled reset request.
    ///
    /// If the pending reset cannot be claimed, the worker returns immediately.
    ///
    /// It first claims [`ResetState::Pending`], then waits for earlier hardware
    /// accesses to complete before issuing the reset and returning the worker
    /// state to [`ResetState::Idle`].
    ///
    /// Panthor reference:
    /// - drivers/gpu/drm/panthor/panthor_device.c::panthor_device_reset_work()
    fn reset_work(&self) {
        if !self.try_transition(ResetState::Pending, ResetState::InProgress) {
            return;
        }

        dev_dbg!(self.pdev, "Starting GPU reset.\n");

        let reset_result = run_reset(self.pdev.as_ref(), &self.hw);

        if let Err(e) = reset_result {
            dev_err!(self.pdev, "GPU reset failed: {:?}\n", e);

            // TODO: Unplug the GPU.
            // There is no API for unplugging the GPU and this is unreachable
            // for now since there are no hardware users for reset API.
        } else {
            dev_dbg!(self.pdev, "GPU reset completed.\n");
        }

        let _ = self.try_transition(ResetState::InProgress, ResetState::Idle);
    }
}

/// User-facing handle for scheduling resets.
///
/// Dropping the handle drains any queued or in-flight reset work before the
/// [`ScopedQueue`] and the clock and regulator resources are released.
#[pin_data(PinnedDrop)]
pub(crate) struct ResetHandle<'reset> {
    #[pin]
    controller: ScopedWork<Controller<'reset>>,
    wq: ScopedQueue<'reset>,
}

impl<'reset> ResetHandle<'reset> {
    /// Creates [`ResetHandle`].
    ///
    /// # Safety
    ///
    /// The returned handle must not be leaked or otherwise prevented from
    /// running [`Drop`], since it owns work that may borrow from `'reset`.
    pub(crate) unsafe fn new(
        pdev: &'reset platform::Device<Bound>,
        hw: Arc<HwGate<'reset>>,
    ) -> Result<impl PinInit<Self, Error>> {
        Ok(try_pin_init!(Self {
            controller <- kernel::new_scoped_work!("tyr::reset", Controller::new(pdev, hw)),
            // SAFETY: The caller guarantees the handle is dropped.
            wq: unsafe { ScopedQueue::new(c"tyr-reset-wq")? },
        }))
    }

    /// Schedules a GPU reset on the dedicated workqueue.
    ///
    /// If a reset is already pending or in progress the call is a no-op.
    #[expect(dead_code)]
    pub(crate) fn schedule(&'reset self) {
        // TODO: Similar to `panthor_device_schedule_reset()` in Panthor, add a
        // power management check once Tyr supports it.

        if self
            .controller
            .try_transition(ResetState::Idle, ResetState::Pending)
        {
            let _ = self.wq.enqueue(&self.controller);
        }
    }
}

#[pinned_drop]
impl<'reset> PinnedDrop for ResetHandle<'reset> {
    fn drop(self: Pin<&mut Self>) {
        // Stop new reset requests before draining queued/running work.
        self.controller
            .state
            .store(ResetState::ShuttingDown, Release);
    }
}

/// Issues a soft reset command and waits for reset-complete IRQ status.
fn issue_soft_reset(dev: &Device<Bound>, io: &IoMem<'_>) -> Result {
    // Clear any stale reset-complete IRQ state before issuing a new soft reset.
    io.write_reg(GPU_IRQ_CLEAR::zeroed().with_reset_completed(true));

    io.write_reg(GPU_COMMAND::reset(ResetMode::SoftReset));

    poll::read_poll_timeout(
        || Ok(io.read(GPU_IRQ_RAWSTAT)),
        |status| status.reset_completed(),
        time::Delta::from_millis(1),
        time::Delta::from_millis(100),
    )
    .inspect_err(|_| dev_err!(dev, "GPU reset timed out."))?;

    Ok(())
}

/// Runs one synchronous GPU reset pass.
///
/// Its visibility is `pub(super)` only so the probe path can run an
/// initial reset; it is not part of this module's public API.
///
/// On success, the GPU is left in a state suitable for reinitialization.
///
/// The sequence is as follows:
///   - Trigger a GPU soft reset.
///   - Wait for the reset-complete IRQ status.
///   - Power L2 back on.
pub(super) fn run_reset(dev: &Device<Bound>, hw: &HwGate<'_>) -> Result {
    let hw_guard = hw.close();
    let iomem = hw_guard.iomem();

    issue_soft_reset(dev, iomem)?;
    gpu::l2_power_on(dev, iomem)?;
    Ok(())
}
