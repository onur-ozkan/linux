// SPDX-License-Identifier: GPL-2.0 or MIT

//! Hardware-access gate for the GPU reset cycle.
//!
//! [`HwGate`] uses a mutex and [`Srcu`] to coordinate reset-sensitive hardware
//! access with reset. Readers hold the mutex while entering SRCU, then release
//! it before accessing hardware. The reset worker holds the mutex while waiting
//! for admitted readers and resetting hardware.

use kernel::{
    prelude::*,
    sync::{
        new_mutex,
        srcu,
        Mutex,
        MutexGuard,
        Srcu, //
    },
};

/// Synchronizes GPU hardware access with reset.
#[pin_data]
pub(super) struct HwGate {
    /// Admits readers and is held exclusively while the reset worker owns the
    /// hardware.
    #[pin]
    gate_lock: Mutex<()>,
    /// Drains readers that entered before the reset worker acquired `gate_lock`.
    #[pin]
    srcu: Srcu,
}

impl HwGate {
    /// Creates an open hardware-access gate.
    pub(super) fn new() -> impl PinInit<Self, Error> {
        try_pin_init!(Self {
            gate_lock <- new_mutex!(()),
            srcu <- kernel::new_srcu!(),
        })
    }

    /// Enters a reset-sensitive hardware-access section.
    #[expect(dead_code)]
    fn access(&self) -> HwAccessGuard<'_> {
        let gate_lock = self.gate_lock.lock();
        let srcu = self.srcu.read_lock();
        drop(gate_lock);

        HwAccessGuard { _srcu: srcu }
    }

    /// Stops new readers and drains admitted readers for the reset worker.
    ///
    /// Callers must serialize write-side access. The reset controller's state
    /// machine provides that serialization.
    pub(super) fn close(&self) -> HwClosedGuard<'_> {
        let gate_lock = self.gate_lock.lock();

        // Holding `gate_lock` prevents new readers from entering SRCU. Readers
        // admitted before us are enrolled, so wait for their read-side work.
        self.srcu.synchronize();

        HwClosedGuard {
            _gate_lock: gate_lock,
        }
    }
}

/// Shared hardware access that blocks reset until dropped.
#[must_use = "the gate is released when the guard is dropped"]
struct HwAccessGuard<'a> {
    _srcu: srcu::Guard<'a>,
}

/// Exclusive hardware access for the reset worker that blocks new hardware
/// accesses until dropped.
#[must_use = "the gate stays closed until the guard is dropped"]
pub(super) struct HwClosedGuard<'a> {
    _gate_lock: MutexGuard<'a, ()>,
}
